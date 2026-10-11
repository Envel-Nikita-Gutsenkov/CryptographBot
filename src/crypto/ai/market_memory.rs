//! Local market memory: price-trajectory summaries computed from the bot's
//! own SQLite `price_history` table.
//!
//! Live APIs only return the current snapshot, which is why the model used to
//! sound amnesic ("he doesn't remember history"). This module is the missing
//! temporal layer: it lets the model see *where prices came from* over the
//! last N days instead of only the current tick.

use crate::db::SqliteStore;

use super::indicators::chart_context;

/// Per-symbol trajectory summary from local history samples `(ts_ms, price)`.
/// Returns `None` when there isn't enough data to say anything meaningful.
pub fn summarize_history(symbol: &str, history: &[(i64, f64)]) -> Option<String> {
    if history.len() < 2 {
        return None;
    }
    let first = history.first()?.1;
    let last = history.last()?.1;
    if first <= 0.0 || last <= 0.0 {
        return None;
    }
    let span_pct = (last - first) / first * 100.0;
    let min = history
        .iter()
        .map(|(_, p)| *p)
        .fold(f64::INFINITY, f64::min);
    let max = history
        .iter()
        .map(|(_, p)| *p)
        .fold(f64::NEG_INFINITY, f64::max);
    let range_pct = (max - min) / first * 100.0;

    let now_ms = history.last().map(|(ts, _)| *ts).unwrap_or(0);
    let day_ms = 86_400_000_i64;
    let week_ms = 7 * day_ms;
    let window_change = |window_ms: i64| -> Option<f64> {
        let idx = history
            .iter()
            .position(|(ts, _)| *ts >= now_ms - window_ms)?;
        let base = history[idx].1;
        if base <= 0.0 {
            return None;
        }
        Some((last - base) / base * 100.0)
    };
    let chg_24h = window_change(day_ms);
    let chg_7d = window_change(week_ms);

    let trend = if span_pct >= 10.0 {
        "uptrend"
    } else if span_pct <= -10.0 {
        "downtrend"
    } else if range_pct >= 15.0 {
        "high volatility"
    } else {
        "range-bound"
    };

    let mut out = format!(
        "- {}: {:.0} → {:.0} ({:+.1}% over window) | range: {:.0}–{:.0} | trend: {}",
        symbol, first, last, span_pct, min, max, trend
    );
    if let Some(c) = chg_24h {
        out.push_str(&format!(" | 24h: {:+.1}%", c));
    }
    if let Some(c) = chg_7d {
        out.push_str(&format!(" | 7d: {:+.1}%", c));
    }
    Some(out)
}

/// Fetch local history for each symbol and join into a prompt block.
/// Returns an empty string when the DB has no samples yet (fresh install).
pub async fn trajectory_context(sqlite: &SqliteStore, symbols: &[String], days: u32) -> String {
    let mut lines = Vec::new();
    for sym in symbols {
        if let Ok(history) = sqlite.get_price_history(sym, days).await {
            if let Some(line) = summarize_history(sym, &history) {
                lines.push(line);
            }
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!(
            "PRICE TRAJECTORY (local history, last {} days):\n{}",
            days,
            lines.join("\n")
        )
    }
}

/// Fetch local OHLCV history for each symbol and build a chart-view block
/// (RSI, SMA, support/resistance, volatility). Returns an empty string when
/// there isn't enough data yet.
pub async fn chart_context_block(sqlite: &SqliteStore, symbols: &[String], days: u32) -> String {
    let mut lines = Vec::new();
    for sym in symbols {
        if let Ok(ohlcv) = sqlite.get_ohlcv_history(sym, days).await {
            if let Some(line) = chart_context(sym, &ohlcv) {
                lines.push(line);
            }
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!(
            "CHART VIEW (local OHLCV, last {} days):\n{}",
            days,
            lines.join("\n")
        )
    }
}

/// Run a sync closure off the tokio worker with a hard timeout. The trading
/// DB open + schema init can block on a file lock held by another process
/// (CLI training, `trade seed`, the advisor worker); on the async runtime that
/// would stall the whole /ask pipeline and even its watchdog timer. Moving it
/// to the blocking pool keeps the runtime responsive and bounds the wait.
/// Returns `None` on timeout/panic — callers degrade gracefully.
async fn bounded_sync<T, F>(label: &'static str, secs: u64, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::time::timeout(
        std::time::Duration::from_secs(secs),
        tokio::task::spawn_blocking(f),
    )
    .await
    {
        Ok(Ok(v)) => Some(v),
        Ok(Err(_)) => {
            tracing::warn!("{label}: blocking task panicked — skipping");
            None
        }
        Err(_) => {
            tracing::warn!("{label}: timed out after {secs}s — skipping");
            None
        }
    }
}

/// Trained-strategy block: top-scoring engine configs from the trading
/// subsystem's historical sweeps (2019–2026), so the model can ground its
/// advice in what actually worked for the user's symbols instead of guessing.
/// Returns an empty string when no training data exists yet. Bounded: the sync
/// DB open runs on the blocking pool with a timeout.
pub async fn trained_strategy_block(symbols: &[String], per_symbol: usize) -> String {
    let symbols = symbols.to_vec();
    bounded_sync("trained_strategy_block", 8, move || {
        trained_strategy_block_sync(&symbols, per_symbol)
    })
    .await
    .unwrap_or_default()
}

fn trained_strategy_block_sync(symbols: &[String], per_symbol: usize) -> String {
    use crate::trading::store::{TradingMode, TradingStore};
    let Ok(store) = TradingStore::open(TradingMode::Simulation) else {
        return String::new();
    };
    let mut lines = Vec::new();
    for sym in symbols {
        let Ok(rows) = store.list_training_stats(sym, per_symbol as i64) else {
            continue;
        };
        for r in rows {
            lines.push(format!(
                "- {}: config {} | margin {:.1}% | tranches {} | profit {} | runs {} | avg PnL {:+.2}% | win {:.0}% | max DD {:.1}% | score {:.1}",
                r.symbol,
                r.config_name,
                r.min_profit_margin * 100.0,
                r.tranche_count,
                r.profit_mode,
                r.runs,
                r.avg_pnl_pct,
                r.win_rate * 100.0,
                r.avg_max_dd_pct,
                r.score
            ));
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!(
            "TRAINED STRATEGY (top configs from historical sweeps):\n{}",
            lines.join("\n")
        )
    }
}

/// Market-calendar block: BTC halving phase (as of today) + per-symbol monthly
/// seasonality, computed from the full training history and shipped in the seed
/// DB. Gives the model explicit cyclical knowledge ("we are N days after the
/// halving, October historically +X%") instead of only implicit regime memory.
/// Returns an empty string when no seasonality data exists yet. Bounded: the
/// sync DB open runs on the blocking pool with a timeout.
pub async fn seasonality_context_block(symbols: &[String]) -> String {
    let symbols = symbols.to_vec();
    bounded_sync("seasonality_context_block", 8, move || {
        seasonality_context_block_sync(&symbols)
    })
    .await
    .unwrap_or_default()
}

fn seasonality_context_block_sync(symbols: &[String]) -> String {
    use crate::trading::market_calendar::seasonality_block_from_stats;
    use crate::trading::store::{TradingMode, TradingStore};
    let Ok(store) = TradingStore::open(TradingMode::Simulation) else {
        return String::new();
    };
    let today = chrono::Utc::now().date_naive();
    let mut lines = Vec::new();
    for sym in symbols {
        let Ok(season) = store.list_seasonality(sym) else {
            continue;
        };
        if season.is_empty() {
            continue;
        }
        lines.push(seasonality_block_from_stats(sym, &season, today));
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!(
            "MARKET CALENDAR (halving phase + trained monthly seasonality):\n{}",
            lines.join("\n")
        )
    }
}

/// Regime block: current regime per symbol (classified from the local OHLCV
/// cache) + the best trained config for that regime and what it historically
/// earned. This is the "market is EUPHORIA, bot historically earned +127% on
/// ETH with d15-tp3.0" grounding — regime-aware instead of generic platitudes.
/// Cheap: local CSV + persisted regime profiles, no network. Bounded: the sync
/// DB open + history file read run on the blocking pool with a timeout.
pub async fn regime_context_block(symbols: &[String]) -> String {
    let symbols = symbols.to_vec();
    bounded_sync("regime_context_block", 8, move || {
        regime_context_block_sync(&symbols)
    })
    .await
    .unwrap_or_default()
}

fn regime_context_block_sync(symbols: &[String]) -> String {
    use crate::trading::history_file::load_history;
    use crate::trading::regime::classify_history;
    use crate::trading::store::{TradingMode, TradingStore};
    let Ok(store) = TradingStore::open(TradingMode::Simulation) else {
        return String::new();
    };
    let mut lines = Vec::new();
    for sym in symbols {
        let Some(history) = load_history(sym) else {
            continue;
        };
        if history.len() < 20 {
            continue;
        }
        let regime = classify_history(&history);
        let price = history.last().map(|(_, p)| *p).unwrap_or(0.0);
        let mut line = format!("- {}: regime {} @ ${:.2}", sym, regime.as_str(), price);
        if let Ok(Some(profile)) = store.get_regime_profile(sym, regime.as_str()) {
            line.push_str(&format!(
                " | best {} config: {} (score {:.1}, avg PnL {:+.1}%, {} runs)",
                regime.as_str(),
                profile.config_name,
                profile.score,
                profile.avg_pnl_pct,
                profile.runs
            ));
        }
        lines.push(line);
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!(
            "REGIME & TRAINED PROFILES (current regime + best config per regime):\n{}",
            lines.join("\n")
        )
    }
}

/// Locality block: where each symbol stands RIGHT NOW (trend, RSI, ATH
/// distance, halving phase, seasonality) + what the deterministic engine and
/// the top-3 trained ensemble would do at the latest close. This is the
/// "is 81k BTC a buy?" answer — grounded in the same OHLCV the backtests
/// replay, no extra data source. Returns an empty string when history is
/// unavailable.
pub async fn locality_context_block(symbols: &[String]) -> String {
    use crate::trading::analyze::{engine_verdict, market_state, render};
    use crate::trading::backtest::fetch_history;
    use crate::trading::store::{TradingMode, TradingStore};
    // The store open + schema init is sync and can block on a file lock held
    // by another process — run it on the blocking pool with a timeout instead
    // of stalling the tokio worker (which would also defeat the ask watchdog).
    let store = match bounded_sync("locality store open", 5, || {
        TradingStore::open(TradingMode::Simulation).ok()
    })
    .await
    {
        Some(Some(store)) => store,
        _ => return String::new(),
    };
    let today = chrono::Utc::now().date_naive();
    // Hard total budget: locality is a nice-to-have context block, never a
    // reason for /ask to hang. Each symbol gets at most 10s (cache hits are
    // instant; network fallbacks are bounded).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut lines = Vec::new();
    for sym in symbols {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let per = remaining.min(std::time::Duration::from_secs(10));
        let from = today - chrono::Duration::days(400);
        let Ok(history) = tokio::time::timeout(per, fetch_history(sym, from, today)).await else {
            tracing::warn!("locality history fetch timed out for {sym} — skipping");
            continue;
        };
        let Ok(history) = history else {
            continue;
        };
        if history.is_empty() {
            continue;
        }
        let price = history.last().map(|(_, p)| *p).unwrap_or(0.0);
        let state = market_state(sym, price, &history);
        let mut block = render(&state);
        block.push('\n');
        block.push_str(&engine_verdict(&store, sym, price, &history));
        lines.push(block);
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!(
            "MARKET LOCALITY (deterministic snapshot + engine/ensemble verdict at latest close):\n{}",
            lines.join("\n---\n")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(prices: &[(i64, f64)]) -> Vec<(i64, f64)> {
        prices.to_vec()
    }

    #[test]
    fn empty_or_single_sample_returns_none() {
        assert!(summarize_history("BTC", &[]).is_none());
        assert!(summarize_history("BTC", &[(1, 60_000.0)]).is_none());
    }

    #[test]
    fn uptrend_is_detected() {
        let now = 1_700_000_000_000_i64;
        let day = 86_400_000_i64;
        let h = hist(&[
            (now - 30 * day, 60_000.0),
            (now - 7 * day, 65_000.0),
            (now - day, 70_000.0),
            (now, 78_000.0),
        ]);
        let s = summarize_history("BTC", &h).unwrap();
        assert!(s.contains("BTC: 60000 → 78000 (+30.0% over window)"));
        assert!(s.contains("trend: uptrend"));
        assert!(s.contains("24h: +11.4%"));
        assert!(s.contains("7d: +20.0%"));
    }

    #[test]
    fn downtrend_is_detected() {
        let now = 1_700_000_000_000_i64;
        let day = 86_400_000_i64;
        let h = hist(&[
            (now - 30 * day, 78_000.0),
            (now - 7 * day, 70_000.0),
            (now - day, 65_000.0),
            (now, 60_000.0),
        ]);
        let s = summarize_history("ETH", &h).unwrap();
        assert!(s.contains("trend: downtrend"));
        assert!(s.contains("-23.1% over window"));
    }

    #[test]
    fn high_volatility_when_range_wide_but_flat() {
        let now = 1_700_000_000_000_i64;
        let day = 86_400_000_i64;
        let h = hist(&[
            (now - 30 * day, 60_000.0),
            (now - 20 * day, 72_000.0),
            (now - 10 * day, 58_000.0),
            (now, 62_000.0),
        ]);
        let s = summarize_history("SOL", &h).unwrap();
        assert!(s.contains("trend: high volatility"));
    }
}
