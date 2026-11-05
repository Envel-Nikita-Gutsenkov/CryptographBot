//! `/long [SYM ...]` — long-range price windows (90 / 180 / 360 days).
//!
//! Unlike `/price`, which only shows the spot quote, this answers "where are we
//! relative to the last quarter / half-year / year": the change over each
//! window, the window low/high and where the current price sits inside that
//! range. Everything is deterministic — no AI call — and the daily series is
//! served from the on-disk cache first, so the command is cheap on provider
//! quotas.
//!
//! `/long` without arguments uses the user's watchlist, exactly like `/price`.

use std::sync::Arc;

use teloxide::{payloads::SendMessageSetters, prelude::*, types::ParseMode};
use tracing::warn;

use crate::{
    bot::keyboards,
    config::Config,
    crypto::CryptoClient,
    db::{SqliteStore, Store},
    format::fmt_price,
};

/// Windows shown for every symbol, in days.
pub const WINDOWS: [i64; 3] = [90, 180, 360];

/// Cap the number of symbols so one call cannot fan out into dozens of history
/// requests against rate-limited providers.
const MAX_SYMBOLS: usize = 5;

const DAY_MS: i64 = 86_400_000;

/// One long-range window reading for a symbol.
#[derive(Debug, Clone, PartialEq)]
pub struct LongWindow {
    pub days: i64,
    /// Close closest to `days` ago (the earliest close we have if the series is
    /// shorter than the window).
    pub start_price: f64,
    /// Percent change from `start_price` to the current price.
    pub change_pct: f64,
    pub low: f64,
    pub high: f64,
    /// Where the current price sits inside `[low, high]`, 0..100.
    pub in_range_pct: f64,
    /// How many days of data actually back this window (<= `days`).
    pub covered_days: i64,
}

impl LongWindow {
    pub fn is_partial(&self) -> bool {
        self.covered_days < self.days
    }
}

/// Compute the long-range windows for `series` (daily closes, oldest first)
/// against the current price. Windows the series cannot cover are still
/// returned, flagged as partial, so the user sees how much history exists.
pub fn compute_windows(series: &[(i64, f64)], current: f64, now_ms: i64) -> Vec<LongWindow> {
    let mut points: Vec<(i64, f64)> = series
        .iter()
        .copied()
        .filter(|(_, p)| p.is_finite() && *p > 0.0)
        .collect();
    points.sort_by_key(|(ts, _)| *ts);

    if points.is_empty() || current <= 0.0 {
        return Vec::new();
    }

    let first_ts = points[0].0;
    let mut out = Vec::with_capacity(WINDOWS.len());

    for days in WINDOWS {
        let cutoff = now_ms - days * DAY_MS;
        // Points inside the window, plus the live price as the newest sample.
        let mut in_window: Vec<f64> = points
            .iter()
            .filter(|(ts, _)| *ts >= cutoff)
            .map(|(_, p)| *p)
            .collect();
        in_window.push(current);

        // Baseline: the last close at or before the cutoff. If the series does
        // not reach that far back, use its oldest close and mark it partial.
        let (start_price, start_ts) = points
            .iter()
            .rev()
            .find(|(ts, _)| *ts <= cutoff)
            .map(|&(ts, p)| (p, ts))
            .unwrap_or((points[0].1, first_ts));

        let low = in_window.iter().copied().fold(f64::INFINITY, f64::min);
        let high = in_window.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let span = high - low;
        let in_range_pct = if span > 0.0 {
            ((current - low) / span * 100.0).clamp(0.0, 100.0)
        } else {
            100.0
        };
        let covered_days = ((now_ms - start_ts) / DAY_MS).max(0);

        out.push(LongWindow {
            days,
            start_price,
            change_pct: (current / start_price - 1.0) * 100.0,
            low,
            high,
            in_range_pct,
            covered_days,
        });
    }

    out
}

fn window_label(w: &LongWindow) -> String {
    if w.is_partial() {
        format!("{}d*", w.days)
    } else {
        format!("{}d", w.days)
    }
}

/// Render one symbol as an HTML message body (monospace table + footer).
pub fn render_symbol(
    symbol: &str,
    current: f64,
    series_len: usize,
    windows: &[LongWindow],
) -> String {
    let mut text = format!(
        "📊 <b>{symbol}</b> · {}\n<i>{series_len} daily closes</i>\n",
        fmt_price(current)
    );

    if windows.is_empty() {
        text.push_str("\nNo history available for this symbol yet.");
        return text;
    }

    text.push_str("\n<pre>");
    text.push_str(&format!(
        "{:<7}{:>9}{:>13}{:>13}{:>9}\n",
        "Window", "Change", "Low", "High", "In range"
    ));
    for w in windows {
        text.push_str(&format!(
            "{:<7}{:>9}{:>13}{:>13}{:>8.0}%\n",
            window_label(w),
            format!("{:+.1}%", w.change_pct),
            fmt_price(w.low),
            fmt_price(w.high),
            w.in_range_pct
        ));
    }
    text.push_str("</pre>");

    let longest = windows.last().unwrap();
    text.push_str(&format!(
        "\nRange {}d: {} – {} · now {:.0}% of range",
        if longest.is_partial() {
            longest.covered_days
        } else {
            longest.days
        },
        fmt_price(longest.low),
        fmt_price(longest.high),
        longest.in_range_pct,
    ));

    if windows.iter().any(|w| w.is_partial()) {
        text.push_str("\n<i>* partial: history is shorter than the window</i>");
    }

    text
}

#[allow(clippy::too_many_arguments)]
pub async fn handle_long_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    sqlite: Arc<SqliteStore>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);
    if !config.is_admin(user_id) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let mut symbols: Vec<String> = args
        .split_whitespace()
        .filter(|w| w.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|w| w.to_uppercase())
        .collect();
    if symbols.is_empty() {
        symbols = store.read(user_id, |p| p.settings.default_watchlist.clone());
    }
    if symbols.is_empty() {
        bot.send_message(
            msg.chat.id,
            "No symbols configured. Use /watchlist add BTC or /long BTC ETH",
        )
        .await?;
        return Ok(());
    }
    symbols.dedup();
    symbols.truncate(MAX_SYMBOLS);

    let typing = bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing);
    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let (_, quotes) = tokio::join!(typing, cmc.get_quotes(&sym_refs));
    let quotes = quotes.unwrap_or_default();

    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut sent = 0usize;

    for sym in &symbols {
        // Daily closes come from the disk cache / SQLite first; the provider is
        // only consulted when the cache cannot cover a year.
        let series = crate::bot::handlers::forecast::load_series(sym, &cmc, &sqlite).await;
        if series.is_empty() {
            warn!("/long: no series for {sym}");
            bot.send_message(msg.chat.id, format!("📊 {sym}: no history available"))
                .await?;
            continue;
        }

        let current = quotes
            .iter()
            .find(|q| q.symbol.eq_ignore_ascii_case(sym))
            .map(|q| q.price_usd)
            .filter(|p| *p > 0.0)
            .or_else(|| series.last().map(|&(_, p)| p))
            .unwrap_or_default();

        let windows = compute_windows(&series, current, now_ms);
        let text = render_symbol(sym, current, series.len(), &windows);

        bot.send_message(msg.chat.id, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboards::long_range_menu(sym))
            .await?;
        sent += 1;
    }

    if sent == 0 {
        bot.send_message(msg.chat.id, "No data returned for the requested symbols.")
            .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daily(n: i64, f: impl Fn(i64) -> f64) -> Vec<(i64, f64)> {
        let now = 1_700_000_000_000i64;
        (0..n).map(|i| (now - (n - 1 - i) * DAY_MS, f(i))).collect()
    }

    #[test]
    fn computes_change_and_range_for_full_windows() {
        // Price rises linearly from 100 to 199 over 361 days.
        let series = daily(362, |i| 100.0 + i as f64);
        let now = series.last().unwrap().0 + DAY_MS;
        let current = series.last().unwrap().1;
        let w = compute_windows(&series, current, now);
        assert_eq!(w.len(), 3);

        let d90 = &w[0];
        assert_eq!(d90.days, 90);
        assert!((d90.change_pct - (current / (current - 90.0) - 1.0) * 100.0).abs() < 0.5);
        assert!(d90.low >= current - 91.0 && d90.low <= current - 89.0);
        assert!((d90.high - current).abs() < 1e-9);
        assert!(d90.in_range_pct > 95.0);
        assert!(!d90.is_partial());

        let d360 = &w[2];
        assert!(!d360.is_partial());
    }

    #[test]
    fn flags_partial_window_when_history_is_short() {
        let series = daily(30, |i| 10.0 + i as f64 * 0.1);
        let now = series.last().unwrap().0 + DAY_MS;
        let current = series.last().unwrap().1;
        let w = compute_windows(&series, current, now);
        assert_eq!(w.len(), 3);
        for x in &w {
            assert!(x.is_partial());
            assert!(x.covered_days <= 30);
            assert_eq!(x.start_price, series[0].1);
        }
    }

    #[test]
    fn empty_series_yields_no_windows() {
        assert!(compute_windows(&[], 100.0, 0).is_empty());
        assert!(compute_windows(&[(0, 100.0)], 0.0, 0).is_empty());
    }

    #[test]
    fn flat_series_reports_full_range_position() {
        let series = daily(200, |_| 50.0);
        let now = series.last().unwrap().0 + DAY_MS;
        let w = compute_windows(&series, 50.0, now);
        for x in &w {
            assert_eq!(x.in_range_pct, 100.0);
            assert!((x.change_pct).abs() < 1e-9);
        }
    }

    #[test]
    fn render_contains_windows_and_partial_note() {
        let series = daily(30, |i| 10.0 + i as f64);
        let now = series.last().unwrap().0 + DAY_MS;
        let w = compute_windows(&series, 39.0, now);
        let text = render_symbol("TEST", 39.0, series.len(), &w);
        assert!(text.contains("90d*"));
        assert!(text.contains("180d*"));
        assert!(text.contains("360d*"));
        assert!(text.contains("partial"));
        assert!(text.contains("<pre>"));
    }
}
