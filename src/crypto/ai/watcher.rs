use chrono::Utc;
use std::sync::Arc;
use teloxide::{prelude::*, types::ChatId};
use tokio::time::Duration;
use tracing::{info, warn};

use crate::{
    config::Config,
    crypto::{
        ai::{prompts::WatcherSeverity, GeminiClient},
        CryptoClient,
    },
    db::{SqliteStore, Store},
};

const CRITICAL_COOLDOWN_HOURS: i64 = 12;
const IMPORTANT_COOLDOWN_HOURS: i64 = 48;

/// Minimum local OHLCV samples (10-min cadence → ~2h of coverage) before the
/// local 7d min/max is trusted over the provider history chain.
const MIN_OHLCV_POINTS: usize = 12;

/// (min, max) over locally stored OHLCV rows. Uses per-day `low`/`high` when
/// present, else the close price. Non-positive prices are ignored.
fn min_max_from_ohlcv(rows: &[(i64, Option<f64>, Option<f64>, Option<f64>, f64)]) -> (f64, f64) {
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &(_, _, high, low, close) in rows {
        for price in [low, high, Some(close)].into_iter().flatten() {
            if price <= 0.0 {
                continue;
            }
            lo = lo.min(price);
            hi = hi.max(price);
        }
    }
    if lo.is_infinite() {
        (0.0, 0.0)
    } else {
        (lo, hi)
    }
}

pub async fn run(
    bot: Bot,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    sqlite: Arc<SqliteStore>,
    config: Arc<Config>,
) {
    loop {
        info!("AI watcher tick");
        for user_id in store.all_user_ids() {
            let enabled = store.read(user_id, |p| p.settings.ai_watcher_enabled);
            if enabled {
                if let Err(e) = check_user(&bot, &store, &cmc, &sqlite, &config, user_id).await {
                    tracing::error!("AI Watcher error for user {}: {:?}", user_id, e);
                }
            }
        }

        // Adaptive sleep interval based on market volatility of user-watched assets
        let mut sleep_secs = 4 * 3600; // default: 4 hours
        let mut all_symbols = std::collections::HashSet::new();
        for user_id in store.all_user_ids() {
            let user_symbols = store.read(user_id, |p| {
                let mut syms: std::collections::HashSet<String> =
                    p.holdings_by_symbol().into_keys().collect();
                syms.extend(p.settings.default_watchlist.clone());
                syms
            });
            all_symbols.extend(user_symbols);
        }
        let sym_refs: Vec<&str> = all_symbols.iter().map(|s| s.as_str()).collect();
        if !sym_refs.is_empty() {
            if let Ok(quotes) = cmc.get_quotes(&sym_refs).await {
                let is_volatile = quotes.iter().any(|q| {
                    q.change_1h.unwrap_or(0.0).abs() >= 2.0
                        || q.change_24h.unwrap_or(0.0).abs() >= 5.0
                });
                if is_volatile {
                    sleep_secs = 2 * 3600; // 2 hours in volatile markets
                    info!("AI Watcher: Volatile market detected! Sleep interval shortened to 2 hours.");
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
    }
}

async fn check_user(
    bot: &Bot,
    store: &Store,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    config: &Config,
    user_id: u64,
) -> anyhow::Result<()> {
    let ai = GeminiClient::new(&config.gemini_api_key);
    if !ai.is_configured() {
        return Ok(());
    }

    let symbols = collect_symbols(store, user_id);
    if symbols.is_empty() {
        return Ok(());
    }

    // Fetch Global Metrics for "entire market" context
    let mut global_ctx = String::new();
    if let Ok(g) = cmc.get_global_metrics().await {
        global_ctx = format!(
            "GLOBAL MARKET METRICS:\nTotal Market Cap: ${:.2}B | 24h Vol: ${:.2}B | BTC Dominance: {:.1}% | ETH Dominance: {:.1}%\n\n",
            g.total_market_cap / 1_000_000_000.0,
            g.total_volume_24h / 1_000_000_000.0,
            g.btc_dominance,
            g.eth_dominance
        );
    }

    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = match cmc.get_quotes(&sym_refs).await {
        Ok(q) => q,
        Err(e) => {
            warn!("Watcher: CMC error: {e}");
            return Ok(());
        }
    };

    if quotes.is_empty() {
        return Ok(());
    }

    let mut snapshots = Vec::new();
    for q in &quotes {
        // 7d min/max: prefer the local OHLCV table (populated by the
        // price-prefetch worker every 10 min for all active symbols) and only
        // hit the provider history chain as a fallback for sparse data.
        let (min_7d, max_7d) = match sqlite.get_ohlcv_history(&q.symbol, 7).await {
            Ok(rows) if rows.len() >= MIN_OHLCV_POINTS => min_max_from_ohlcv(&rows),
            _ => {
                if let Ok(h) = cmc.get_history(&q.symbol, "7d").await {
                    if !h.is_empty() {
                        (
                            h.iter().map(|(_, p)| *p).fold(f64::INFINITY, f64::min),
                            h.iter().map(|(_, p)| *p).fold(f64::NEG_INFINITY, f64::max),
                        )
                    } else {
                        (q.price_usd, q.price_usd)
                    }
                } else {
                    (q.price_usd, q.price_usd)
                }
            }
        };

        let mut hist_str = format!(" | 7d_min:${:.2} | 7d_max:${:.2}", min_7d, max_7d);
        if let Ok(Some(sum)) = sqlite.get_news_summary(&q.symbol).await {
            hist_str.push_str(&format!("\nNews Summary:\n{}", sum));
        }

        snapshots.push(format!(
            "{}: ${:.2} | 1h:{:.1}% | 24h:{:.2}% | 7d:{:.2}%{}",
            q.symbol,
            q.price_usd,
            q.change_1h.unwrap_or(0.0),
            q.change_24h.unwrap_or(0.0),
            q.change_7d.unwrap_or(0.0),
            hist_str
        ));
    }
    let snapshot = format!("{}{}", global_ctx, snapshots.join("\n\n"));

    let previous_analysis = store.read(user_id, |p| p.settings.ai_last_watcher_analysis.clone());

    // User context: memos (e.g. "I use a Ledger Nano S Plus bought from X") and
    // active goals, so the watcher can personalize alerts and security threats.
    let (_, memory_context) = crate::alerts::engine::build_user_context(store, user_id);
    let user_context: Option<&str> = if memory_context.trim().is_empty() {
        None
    } else {
        Some(memory_context.as_str())
    };

    let result = ai
        .watcher_analysis(
            &snapshot,
            previous_analysis.as_deref(),
            user_context,
            std::sync::Arc::new(sqlite.clone()),
        )
        .await?;
    if result.severity == WatcherSeverity::None {
        return Ok(());
    }

    let now = Utc::now();
    let can_notify = store.write(user_id, |p| match result.severity {
        WatcherSeverity::Critical => {
            let ok = p
                .settings
                .ai_last_critical_notify
                .map(|t| now.signed_duration_since(t).num_hours() >= CRITICAL_COOLDOWN_HOURS)
                .unwrap_or(true);
            if ok {
                p.settings.ai_last_critical_notify = Some(now);
            }
            ok
        }
        WatcherSeverity::Important => {
            let ok = p
                .settings
                .ai_last_important_notify
                .map(|t| now.signed_duration_since(t).num_hours() >= IMPORTANT_COOLDOWN_HOURS)
                .unwrap_or(true);
            if ok {
                p.settings.ai_last_important_notify = Some(now);
            }
            ok
        }
        WatcherSeverity::None => false,
    });

    // Only record the analysis as "advised" when the user actually receives
    // the message. Storing it on a cooldown-suppressed tick poisons the next
    // prompt with "what you advised earlier" and the model concludes the user
    // was already notified — alerts then arrive days late ("still active").
    if !can_notify {
        return Ok(());
    }
    store.write(user_id, |p| {
        p.settings.ai_last_watcher_analysis = Some(result.detail.clone());
    });

    let (emoji, header) = match result.severity {
        WatcherSeverity::Critical => ("🚨", "AI Market Alert"),
        _ => ("📢", "AI Market Update"),
    };
    let msg = format!(
        "{emoji} <b>{header}</b>\n\n{}\n\n{}",
        result.headline, result.detail
    );

    if let Err(e) = bot
        .send_message(ChatId(user_id as i64), &msg)
        .parse_mode(teloxide::types::ParseMode::Html)
        .await
    {
        warn!("Watcher: failed to notify user {user_id}: {e}");
    }

    Ok(())
}

fn collect_symbols(store: &Store, user_id: u64) -> Vec<String> {
    store.read(user_id, |p| {
        let mut syms: std::collections::HashSet<String> =
            p.holdings_by_symbol().into_keys().collect();
        syms.extend(p.settings.default_watchlist.clone());
        syms.insert("BTC".into());
        syms.insert("ETH".into());
        syms.into_iter().collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        low: Option<f64>,
        high: Option<f64>,
        close: f64,
    ) -> (i64, Option<f64>, Option<f64>, Option<f64>, f64) {
        (0, Some(close), high, low, close)
    }

    #[test]
    fn min_max_uses_low_and_high() {
        let rows = vec![
            row(Some(80.0), Some(95.0), 90.0),
            row(Some(70.0), Some(120.0), 100.0),
        ];
        assert_eq!(min_max_from_ohlcv(&rows), (70.0, 120.0));
    }

    #[test]
    fn min_max_falls_back_to_close_when_low_high_none() {
        // Legacy row shape: open/high/low all None, only close (price) present
        let rows = vec![(0, None, None, None, 60.0)];
        assert_eq!(min_max_from_ohlcv(&rows), (60.0, 60.0));
    }

    #[test]
    fn min_max_ignores_nonpositive_prices() {
        // Glitched zero low is skipped; negative low is skipped (close used)
        let rows = vec![
            row(Some(0.0), Some(50.0), 40.0),
            row(Some(-5.0), None, 45.0),
        ];
        assert_eq!(min_max_from_ohlcv(&rows), (40.0, 50.0));
    }

    #[test]
    fn min_max_empty_rows_return_zeros() {
        assert_eq!(min_max_from_ohlcv(&[]), (0.0, 0.0));
        let rows = vec![row(Some(0.0), None, -1.0)];
        assert_eq!(min_max_from_ohlcv(&rows), (0.0, 0.0));
    }

    #[test]
    fn min_max_single_row() {
        let rows = vec![row(Some(10.0), Some(20.0), 15.0)];
        assert_eq!(min_max_from_ohlcv(&rows), (10.0, 20.0));
    }
}
