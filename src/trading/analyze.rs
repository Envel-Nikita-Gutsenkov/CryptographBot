//! Market-state analytics: a compact, deterministic read of where a symbol
//! stands right now — trend (SMA20/50/200), momentum (RSI), distance from
//! ATH, support/resistance, volatility, halving phase, monthly seasonality —
//! plus what the deterministic engine would do at this price and what the
//! top-3 trained profiles (ensemble) agree on.
//!
//! This is the "does the bot understand the local?" layer: it turns the same
//! OHLCV history the backtests replay into a human-readable verdict, and it
//! grounds the AI debate in the same numbers.

use crate::crypto::ai::indicators::{rsi, sma, volatility};
use crate::trading::engine::{Decision, TradingEngine};
use crate::trading::market_calendar::{days_to_next_halving, halving_phase, monthly_seasonality};
use crate::trading::store::TradingStore;
use crate::trading::strategies::{plan_buy_ensemble, resolve_profile_for_stats, StrategyProfile};

/// Deterministic snapshot of one symbol's market state.
#[derive(Debug, Clone)]
pub struct MarketState {
    pub symbol: String,
    pub price: f64,
    pub sma20: Option<f64>,
    pub sma50: Option<f64>,
    pub sma200: Option<f64>,
    pub rsi14: Option<f64>,
    pub ath: Option<f64>,
    /// Negative = below ATH (how far from the top).
    pub ath_distance_pct: Option<f64>,
    pub support20: Option<f64>,
    pub resistance20: Option<f64>,
    pub volatility20: Option<f64>,
    pub halving_phase: String,
    pub days_since_halving: i64,
    pub days_to_next_halving: Option<i64>,
    pub seasonality: Vec<(u32, f64)>,
}

/// Compute the market state from `history` (ts_ms, close) — the same series
/// the backtests replay. `price` is the reference price (latest close or an
/// explicit override).
pub fn market_state(symbol: &str, price: f64, history: &[(i64, f64)]) -> MarketState {
    let closes: Vec<f64> = history.iter().map(|&(_, c)| c).collect();
    let ath = closes.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let ath = if ath.is_finite() { Some(ath) } else { None };
    let ath_distance_pct = ath.map(|a| (price - a) / a * 100.0);
    let window = closes.len().min(20);
    let support20 = if window > 0 {
        closes[closes.len() - window..]
            .iter()
            .copied()
            .fold(f64::INFINITY, f64::min)
    } else {
        f64::INFINITY
    };
    let resistance20 = if window > 0 {
        closes[closes.len() - window..]
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    } else {
        f64::NEG_INFINITY
    };
    let today = chrono::Utc::now().date_naive();
    let (phase, days_since) = halving_phase(today);
    MarketState {
        symbol: symbol.to_uppercase(),
        price,
        sma20: sma(&closes, 20),
        sma50: sma(&closes, 50),
        sma200: sma(&closes, 200),
        rsi14: rsi(&closes, 14),
        ath,
        ath_distance_pct,
        support20: if support20.is_finite() {
            Some(support20)
        } else {
            None
        },
        resistance20: if resistance20.is_finite() {
            Some(resistance20)
        } else {
            None
        },
        volatility20: volatility(&closes, 20),
        halving_phase: phase.as_str().to_string(),
        days_since_halving: days_since,
        days_to_next_halving: days_to_next_halving(today),
        seasonality: monthly_seasonality(history),
    }
}

/// Human-readable block. Compact enough for the AI context and the CLI.
pub fn render(state: &MarketState) -> String {
    let mut s = format!(
        "{} @ ${:.2} | RSI14: {} | SMA20: {} | SMA50: {} | SMA200: {}",
        state.symbol,
        state.price,
        fmt_opt(state.rsi14, 0),
        fmt_opt(state.sma20, 0),
        fmt_opt(state.sma50, 0),
        fmt_opt(state.sma200, 0)
    );
    if let Some(d) = state.ath_distance_pct {
        s.push_str(&format!(
            " | vs ATH ${:.0} ({:+.1}%)",
            state.ath.unwrap_or(0.0),
            d
        ));
    }
    if let (Some(lo), Some(hi)) = (state.support20, state.resistance20) {
        s.push_str(&format!(" | S/R(20): {lo:.0}–{hi:.0}"));
    }
    if let Some(v) = state.volatility20 {
        s.push_str(&format!(" | vol20: {v:.1}%"));
    }
    s.push_str(&format!(
        " | cycle: {} ({}d since halving",
        state.halving_phase, state.days_since_halving
    ));
    if let Some(n) = state.days_to_next_halving {
        s.push_str(&format!(", ~{n}d to next"));
    }
    s.push_str(")");
    let best: Vec<String> = state
        .seasonality
        .iter()
        .filter(|(_, r)| *r > 0.0)
        .map(|(m, r)| format!("{m}: {:+.1}%", r))
        .collect();
    let worst: Vec<String> = state
        .seasonality
        .iter()
        .filter(|(_, r)| *r < 0.0)
        .map(|(m, r)| format!("{m}: {:+.1}%", r))
        .collect();
    if !best.is_empty() || !worst.is_empty() {
        s.push_str(&format!(
            "; strongest months: {}; weakest: {}",
            if best.is_empty() {
                "none".into()
            } else {
                best.join(", ")
            },
            if worst.is_empty() {
                "none".into()
            } else {
                worst.join(", ")
            }
        ));
    }
    s
}

fn fmt_opt(v: Option<f64>, prec: usize) -> String {
    match v {
        Some(x) => format!("{x:.prec$}"),
        None => "N/A".into(),
    }
}

/// What the deterministic engine (latest persisted config, else runner) would
/// do at this price, plus the top-3 trained profiles' ensemble verdict.
/// `history` (ts_ms, close) is fed into the engines so trend filters and ATH
/// guards see the same market the caller sees.
pub fn engine_verdict(
    store: &TradingStore,
    symbol: &str,
    price: f64,
    history: &[(i64, f64)],
) -> String {
    let config = crate::trading::strategies::auto_engine_config(store, symbol, history)
        .unwrap_or_else(|_| crate::trading::engine::EngineConfig::runner());
    let mut out = String::new();
    let regime = crate::trading::regime::classify_history(history);
    out.push_str(&format!(
        "  regime: {} (auto config: margin {:.3}, tranches {}, tp {:.2}, trail {:.2}, {})\n",
        regime.as_str(),
        config.min_profit_margin,
        config.tranche_count,
        config.take_profit_pct,
        config.trail_pct,
        config.profit_mode.as_str()
    ));
    let engine = match TradingEngine::new(config) {
        Ok(mut e) => {
            for &(ts, p) in history {
                e.observe(&crate::trading::engine::MarketTick::new(
                    symbol,
                    p,
                    p,
                    p,
                    ts / 1000,
                ));
            }
            e
        }
        Err(e) => {
            out.push_str(&format!("  engine: config invalid — {e}"));
            out.push('\n');
            out.push_str("  open lots: none\n");
            out.push_str("  ensemble (top-3 trained):\n    error — no valid engine config\n");
            return out;
        }
    };
    match engine.plan_buy(store, symbol, price, price, price) {
        Ok(Decision::Proceed(p)) => {
            out.push_str(&format!(
                "  engine: BUY ${:.2} tranche (exit target ${:.2}) — {}",
                p.tranche_usd, p.target_exit_price, p.rationale
            ));
        }
        Ok(Decision::Skip(r)) => out.push_str(&format!("  engine: SKIP — {r}")),
        Err(e) => out.push_str(&format!("  engine: error — {e}")),
    }
    out.push('\n');
    // Open lots: would the engine sell at this price?
    let lots = store.list_lots(None).unwrap_or_default();
    let open: Vec<_> = lots
        .iter()
        .filter(|l| {
            (l.pair == symbol.to_uppercase() || l.pair == format!("{}/USDT", symbol.to_uppercase()))
                && l.status.as_str() != "CLOSED"
        })
        .collect();
    if open.is_empty() {
        out.push_str("  open lots: none\n");
    } else {
        out.push_str("  open lots:\n");
        for l in open {
            let pnl = if l.entry_price > 0.0 {
                (price - l.entry_price) / l.entry_price * 100.0
            } else {
                0.0
            };
            let verdict = match engine.plan_sell(store, &l.id, price) {
                Ok(Decision::Proceed(_)) => "SELL",
                Ok(Decision::Skip(_)) => "hold",
                Err(_) => "hold",
            };
            out.push_str(&format!(
                "    lot {}: entry ${:.2}, unrealized {:+.1}% → {verdict}\n",
                l.id, l.entry_price, pnl
            ));
        }
    }
    // Ensemble: top-3 trained profiles majority vote.
    out.push_str("  ensemble (top-3 trained):\n");
    match plan_buy_ensemble(store, symbol, price, history) {
        Ok(Decision::Proceed(p)) => out.push_str(&format!(
            "    BUY ${:.2} tranche — {}",
            p.tranche_usd, p.rationale
        )),
        Ok(Decision::Skip(r)) => out.push_str(&format!("    SKIP — {r}")),
        Err(e) => out.push_str(&format!("    error — {e}")),
    }
    out
}

/// List the top-3 trained profiles for a symbol (name + score + key params).
pub fn top_profiles(store: &TradingStore, symbol: &str) -> Vec<String> {
    store
        .list_training_stats(symbol, 3)
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let params = store
                .get_strategy_profile(&r.config_name)
                .ok()
                .flatten()
                .or_else(|| {
                    resolve_profile_for_stats(store, &r.config_name)
                        .ok()
                        .flatten()
                })
                .and_then(|rec| StrategyProfile::from_record(&rec).ok())
                .map(|p| {
                    format!(
                        "m{:.3}-t{}-{}",
                        p.engine_config.min_profit_margin,
                        p.engine_config.tranche_count,
                        p.engine_config.profit_mode.as_str()
                    )
                })
                .unwrap_or_default();
            format!(
                "{} (score {:.1}, avg PnL {:+.1}%, win {:.0}%, maxDD {:.1}%, {params})",
                r.config_name,
                r.score,
                r.avg_pnl_pct,
                r.win_rate * 100.0,
                r.avg_max_dd_pct
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(prices: &[f64]) -> Vec<(i64, f64)> {
        let mut out = Vec::new();
        let mut ts = 1_600_000_000_000i64;
        for &p in prices {
            out.push((ts, p));
            ts += 86_400_000;
        }
        out
    }

    #[test]
    fn market_state_computes_indicators_and_ath() {
        // 300 days: flat 100, then a dip to 80, then recovery to 120.
        let mut prices = vec![100.0; 250];
        prices.extend(vec![80.0; 20]);
        prices.extend((0..30).map(|i| 80.0 + i as f64 * 40.0 / 29.0));
        let h = hist(&prices);
        let s = market_state("BTC", 120.0, &h);
        assert_eq!(s.symbol, "BTC");
        assert!(s.sma20.is_some());
        assert!(s.sma50.is_some());
        assert!(s.sma200.is_some());
        assert!(s.rsi14.is_some());
        // ATH is 120 (the last close) — distance 0%.
        assert_eq!(s.ath_distance_pct.unwrap().round(), 0.0);
        assert!(s.support20.is_some());
        assert!(s.resistance20.is_some());
        assert!(s.volatility20.is_some());
        assert!(!s.halving_phase.is_empty());
        // Seasonality: 12 months, all finite.
        assert_eq!(s.seasonality.len(), 12);
        for (_, r) in &s.seasonality {
            assert!(r.is_finite());
        }
    }

    #[test]
    fn market_state_below_ath_reports_negative_distance() {
        let h = hist(&[100.0; 100]);
        let s = market_state("ETH", 80.0, &h);
        assert!((s.ath_distance_pct.unwrap() + 20.0).abs() < 1e-9);
    }

    #[test]
    fn render_is_compact_and_readable() {
        let h = hist(
            &(0..300)
                .map(|i| 100.0 + (i as f64 % 50.0) * 0.5)
                .collect::<Vec<_>>(),
        );
        let s = market_state("SOL", 110.0, &h);
        let text = render(&s);
        assert!(text.contains("SOL"));
        assert!(text.contains("RSI14"));
        assert!(text.contains("cycle"));
        assert!(text.len() < 600);
    }

    #[test]
    fn empty_history_never_panics() {
        let s = market_state("LINK", 10.0, &[]);
        assert!(s.sma20.is_none());
        assert!(s.ath.is_none());
        assert!(s.ath_distance_pct.is_none());
        assert!(s.support20.is_none());
        assert!(s.resistance20.is_none());
        assert!(!render(&s).is_empty());
    }
}
