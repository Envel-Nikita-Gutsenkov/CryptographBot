//! Compact "community & exchange signals" context, assembled from the
//! short-TTL `market_signals` table.
//!
//! The raw rows are written by the signals worker (CMC headlines, CMC trending
//! searches, Binance announcements, CoinGecko community votes). This module
//! turns them into a small, price-free block that can be pasted into a prompt:
//! the news summarizer folds it into its stored summary, and `/ask` uses it to
//! answer "what does the crowd think" without a second web search.

use crate::db::SqliteStore;
use crate::models::MarketSignal;

/// Hard cap on the formatted block so it can never crowd out the question.
const MAX_CHARS: usize = 1_800;

/// Does this market-wide item mention `symbol`? Matches the same aliases and
/// whole-word rules as the RSS feed, so "sol" never matches "solution".
pub fn mentions(signal: &MarketSignal, symbol: &str) -> bool {
    let symbol = symbol.trim().to_uppercase();
    let Some(aliases) = crate::crypto::client::aliases_for(&symbol) else {
        return false;
    };
    let haystack = format!("{} {}", signal.title, signal.body).to_lowercase();
    aliases
        .iter()
        .any(|needle| crate::crypto::client::contains_word_lower(&haystack, needle))
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        MarketSignal::KIND_SENTIMENT => "Community votes",
        MarketSignal::KIND_TRENDING => "Trending searches",
        MarketSignal::KIND_ANNOUNCEMENT => "Exchange announcements",
        _ => "Headlines",
    }
}

fn one_line(signal: &MarketSignal) -> String {
    let title = signal
        .title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if signal.body.trim().is_empty() {
        title
    } else {
        let body = signal.body.split_whitespace().collect::<Vec<_>>().join(" ");
        let body: String = body.chars().take(160).collect();
        format!("{title} — {body}")
    }
}

/// True when a published timestamp falls inside the window. Unknown or
/// unparseable timestamps are kept (better a possibly-old item than none).
fn within_hours(published_at: &Option<String>, hours: i64) -> bool {
    let Some(raw) = published_at else {
        return true;
    };
    match chrono::DateTime::parse_from_rfc3339(raw) {
        Ok(dt) => {
            chrono::Utc::now()
                .signed_duration_since(dt.with_timezone(&chrono::Utc))
                .num_hours()
                <= hours
        }
        Err(_) => true,
    }
}

/// Build the signals block for `symbols`. Empty when the table is cold, so
/// callers can concatenate unconditionally.
pub async fn signals_context_block(sqlite: &SqliteStore, symbols: &[String], hours: i64) -> String {
    let hours = hours.max(1);
    let mut sentiments: Vec<String> = Vec::new();
    let mut trending: Vec<String> = Vec::new();
    let mut relevant: Vec<String> = Vec::new();

    // Per-symbol community votes.
    for sym in symbols.iter().take(5) {
        if let Ok(rows) = sqlite.get_market_signals(sym, hours, 6).await {
            for row in rows {
                if row.kind == MarketSignal::KIND_SENTIMENT {
                    sentiments.push(format!("{}: {}", row.symbol, row.title));
                }
            }
        }
    }

    // What the whole market is searching for — a source-level signal, not
    // tied to a holding.
    if let Ok(rows) = sqlite
        .get_signals_by_kind(MarketSignal::KIND_TRENDING, hours, 6)
        .await
    {
        let names: Vec<String> = rows
            .iter()
            .map(|r| {
                if r.title.is_empty() {
                    r.symbol.clone()
                } else {
                    format!("{} ({})", r.title, r.symbol)
                }
            })
            .collect();
        if !names.is_empty() {
            trending.push(names.join(", "));
        }
    }

    // Market-wide headlines and announcements that mention a watched coin.
    let mut fallback: Vec<String> = Vec::new();
    if let Ok(rows) = sqlite.get_market_signals("GENERAL", hours, 200).await {
        for row in &rows {
            if !within_hours(&row.published_at, hours) {
                continue;
            }
            if symbols.iter().any(|s| mentions(row, s)) {
                relevant.push(format!("[{}] {}", kind_label(&row.kind), one_line(row)));
            } else if row.kind == MarketSignal::KIND_NEWS {
                fallback.push(format!("[{}] {}", kind_label(&row.kind), one_line(row)));
            }
        }
        // A story that mentions none of the watched coins is still useful
        // market context; keep a couple when the coin-specific list is thin.
        if relevant.len() < 3 {
            relevant.extend(fallback.into_iter().take(3 - relevant.len()));
        }
        relevant.truncate(8);
    }

    if sentiments.is_empty() && trending.is_empty() && relevant.is_empty() {
        return String::new();
    }

    let mut block = format!("COMMUNITY & EXCHANGE SIGNALS (last {hours}h):\n");
    if !sentiments.is_empty() {
        block.push_str(&format!("- {}: {}\n", "Sentiment", sentiments.join("; ")));
    }
    if !trending.is_empty() {
        block.push_str(&format!("- Trending: {}\n", trending.join("; ")));
    }
    for line in &relevant {
        block.push_str(&format!("- {line}\n"));
    }
    crate::format::truncate_chars(block.trim_end(), MAX_CHARS)
}

/// Convenience wrapper for a single symbol (news worker).
pub async fn signals_block_for(sqlite: &SqliteStore, symbol: &str, hours: i64) -> String {
    signals_context_block(sqlite, &[symbol.to_string()], hours).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(kind: &str, symbol: &str, title: &str, body: &str) -> MarketSignal {
        MarketSignal::new(kind, "test", symbol, title).with_body(body)
    }

    #[test]
    fn mentions_uses_whole_word_aliases() {
        let s = sig(MarketSignal::KIND_NEWS, "GENERAL", "Solana outage", "");
        assert!(mentions(&s, "SOL"));
        let noise = sig(MarketSignal::KIND_NEWS, "GENERAL", "Solution shipped", "");
        assert!(!mentions(&noise, "SOL"));
    }

    #[test]
    fn one_line_collapses_whitespace() {
        let s = sig(
            MarketSignal::KIND_NEWS,
            "GENERAL",
            "A\n  title",
            "body   text",
        );
        assert_eq!(one_line(&s), "A title — body text");
    }

    #[test]
    fn published_window_filters_old_items_and_keeps_unknown() {
        let now = chrono::Utc::now();
        let recent = Some(now.to_rfc3339());
        let old = Some((now - chrono::Duration::days(10)).to_rfc3339());
        assert!(within_hours(&recent, 72));
        assert!(!within_hours(&old, 72));
        assert!(within_hours(&None, 72));
        assert!(within_hours(&Some("not-a-date".into()), 72));
    }
}
