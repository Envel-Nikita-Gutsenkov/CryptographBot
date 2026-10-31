//! Proactive dip-zone watcher — the "is this the bottom, should I add?" ping.
//!
//! Runs once a day (not every few minutes: a dip verdict does not change on a
//! 5-minute candle) and only for users whose price is at or below their average
//! entry **and** whose deterministic verdict is a buy (BUY NOW / SCALE IN).
//!
//! Delivery dedup uses a `HashMap<symbol:zone, timestamp>` in
//! [`UserSettings::dip_seen`] with a per-zone cooldown. The map is essential:
//! the single-slot dedup used by the old security watcher re-sent the same
//! incident whenever two symbols/zones alternated between scans.

use std::sync::Arc;

use chrono::{DateTime, Timelike, Utc};
use teloxide::prelude::*;
use tracing::{info, warn};

use crate::{
    bot::{handlers::build_dip_report, keyboards},
    config::Config,
    crypto::CryptoClient,
    db::{SqliteStore, Store},
    trading::DipVerdict,
};

/// Daily cadence. The first scan waits a little so it does not compete with the
/// startup burst of the other workers.
const FIRST_SCAN_DELAY_SECS: u64 = 15 * 60;
const SCAN_INTERVAL_SECS: u64 = 24 * 3600;
/// Symbols per user per scan (API economy).
const MAX_SYMBOLS: usize = 4;

pub async fn run(
    bot: Bot,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    sqlite: Arc<SqliteStore>,
    config: Arc<Config>,
) {
    info!("Starting proactive dip-zone worker");
    tokio::time::sleep(std::time::Duration::from_secs(FIRST_SCAN_DELAY_SECS)).await;
    loop {
        if let Err(e) = scan(&bot, &store, &cmc, &sqlite, &config).await {
            warn!("Dip worker scan failed: {e:?}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(SCAN_INTERVAL_SECS)).await;
    }
}

fn in_quiet_hours(now_hour: u8, start: Option<u8>, end: Option<u8>) -> bool {
    let (Some(s), Some(e)) = (start, end) else {
        return false;
    };
    if s == e {
        return false;
    }
    if s < e {
        now_hour >= s && now_hour < e
    } else {
        now_hour >= s || now_hour < e
    }
}

/// Record `key` for `now` and return whether the caller should notify.
/// Pruning uses four cooldown windows (28 days by default), so `dip_seen`
/// cannot grow forever, while distinct zones/symbols stay independently
/// deduplicated — the map is the fix for the security-alert spam regression.
fn allow_send(
    seen: &mut std::collections::HashMap<String, DateTime<Utc>>,
    key: &str,
    now: DateTime<Utc>,
    cooldown_hours: i64,
) -> bool {
    let cooldown = chrono::Duration::hours(cooldown_hours.max(1));
    let keep = cooldown * 4;
    seen.retain(|_, at| now.signed_duration_since(*at) < keep);
    if let Some(last) = seen.get(key) {
        if now.signed_duration_since(*last) < cooldown {
            return false;
        }
    }
    seen.insert(key.to_string(), now);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn quiet_hours_wrap_and_disable() {
        assert!(!in_quiet_hours(12, None, None));
        assert!(!in_quiet_hours(12, Some(9), Some(9)));
        assert!(in_quiet_hours(10, Some(9), Some(18)));
        assert!(!in_quiet_hours(18, Some(9), Some(18)));
        // Wrapping window: 22 → 08.
        assert!(in_quiet_hours(23, Some(22), Some(8)));
        assert!(in_quiet_hours(2, Some(22), Some(8)));
        assert!(!in_quiet_hours(12, Some(22), Some(8)));
    }

    #[test]
    fn dedup_is_per_zone_not_single_slot() {
        let mut seen = std::collections::HashMap::new();
        let t0 = Utc.with_ymd_and_hms(2026, 8, 20, 12, 0, 0).unwrap();
        assert!(allow_send(&mut seen, "BTC:63000", t0, 168));
        // Same zone inside the cooldown → suppressed.
        assert!(!allow_send(
            &mut seen,
            "BTC:63000",
            t0 + chrono::Duration::hours(24),
            168
        ));
        // A *different* zone while the first is still cooling down must fire.
        assert!(allow_send(
            &mut seen,
            "BTC:58000",
            t0 + chrono::Duration::hours(25),
            168
        ));
        // Alternating back must still be suppressed (the old single-slot bug).
        assert!(!allow_send(
            &mut seen,
            "BTC:63000",
            t0 + chrono::Duration::hours(26),
            168
        ));
        // After the cooldown the same zone fires again.
        assert!(allow_send(
            &mut seen,
            "BTC:63000",
            t0 + chrono::Duration::hours(169),
            168
        ));
    }

    #[test]
    fn dedup_prunes_old_entries() {
        let mut seen = std::collections::HashMap::new();
        let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        assert!(allow_send(&mut seen, "ETH:1", t0, 24));
        let later = t0 + chrono::Duration::days(30);
        assert!(allow_send(&mut seen, "ETH:1", later, 24));
        // The stale key was pruned, so the map holds only the fresh entry.
        assert_eq!(seen.len(), 1);
    }
}

async fn scan(
    bot: &Bot,
    store: &Store,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    _config: &Config,
) -> anyhow::Result<()> {
    let now = Utc::now();
    for user_id in store.all_user_ids() {
        let (enabled, quiet_start, quiet_end, symbols) = store.read(user_id, |p| {
            let mut watch = p.settings.default_watchlist.clone();
            watch.extend(p.holdings_by_symbol().into_keys());
            watch.sort();
            watch.dedup();
            watch.truncate(MAX_SYMBOLS);
            (
                p.settings.dip_alert_enabled,
                p.settings.alert_quiet_hours_start,
                p.settings.alert_quiet_hours_end,
                watch,
            )
        });
        if !enabled
            || symbols.is_empty()
            || in_quiet_hours(now.hour() as u8, quiet_start, quiet_end)
        {
            continue;
        }

        let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
        let quotes = match cmc.get_quotes(&sym_refs).await {
            Ok(q) if !q.is_empty() => q,
            Ok(_) => continue,
            Err(e) => {
                warn!("Dip worker: quotes failed for user {user_id}: {e}");
                continue;
            }
        };

        for sym in &symbols {
            let avg = store.read(user_id, |p| p.avg_buy_price(sym));
            // Entry-relative alerts only. Without a cost basis we cannot say
            // "below your average", and a plain zone ping would be noise.
            let Some(avg) = avg.filter(|a| *a > 0.0) else {
                continue;
            };
            let Some(price) = quotes
                .iter()
                .find(|q| q.symbol.eq_ignore_ascii_case(sym))
                .map(|q| q.price_usd)
            else {
                continue;
            };
            if price > avg * 1.0001 {
                continue;
            }

            let report = match build_dip_report(sym, cmc, sqlite, Some(price), Some(avg)).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("Dip worker: report for {sym} failed: {e}");
                    continue;
                }
            };
            if !report.verdict.is_buy() {
                continue;
            }

            let zone_key = match &report.verdict {
                DipVerdict::BuyNow => format!("{sym}:now"),
                DipVerdict::ScaleIn { zone_price, .. } => format!("{sym}:{zone_price:.0}"),
                _ => continue,
            };

            let should_send = store.write(user_id, |p| {
                let cooldown_hours = p.settings.dip_cooldown_hours.max(1) as i64;
                allow_send(&mut p.settings.dip_seen, &zone_key, now, cooldown_hours)
            });
            if !should_send {
                continue;
            }

            let text = format!("🔔 <b>Proactive dip alert</b>\n\n{}", report.render_html());
            let sent = bot
                .send_message(ChatId(user_id as i64), &text)
                .parse_mode(teloxide::types::ParseMode::Html)
                .reply_markup(keyboards::dip_actions(sym))
                .await;
            if let Err(e) = sent {
                warn!("Dip worker: send to {user_id} failed: {e}");
            } else {
                info!("Dip worker: alert for {sym} (zone {zone_key}) sent to {user_id}");
            }
        }
    }
    Ok(())
}
