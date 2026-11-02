use chrono::Utc;
use std::sync::Arc;
use teloxide::{prelude::*, types::ChatId};
use tokio::time::Duration;
use tracing::{info, warn};

use crate::{
    config::Config,
    crypto::{
        ai::{
            prompts::{WatcherResult, WatcherSeverity},
            GeminiClient,
        },
        CryptoClient,
    },
    db::{SqliteStore, Store},
};

const CRITICAL_COOLDOWN_HOURS: i64 = 12;
const IMPORTANT_COOLDOWN_HOURS: i64 = 48;

/// One alert per incident, not per day: once the user has been told about
/// `binance` / `ledger` / ... we stay quiet about that entity for a week unless
/// a different one is hit.
const SECURITY_TOPIC_COOLDOWN_DAYS: i64 = 7;

/// The AI watcher ticks every 2–4 h, and its boot tick races the news worker
/// (summaries are written ~10 s later), so a breach could go unnoticed for
/// hours. This deterministic scan runs on its own cadence; news articles are
/// refreshed every ~10 min, so a major incident is still surfaced within ~25 min.
/// 15 min keeps that guarantee while cutting the number of DB/news passes 3x.
const SECURITY_SCAN_INTERVAL_SECS: u64 = 900;

/// Cap alerts per scan: a noisy feed must not turn into a wall of messages.
/// One incident per message; the next scan picks up anything that is still fresh.
const SECURITY_MAX_ALERTS_PER_SCAN: usize = 1;

/// Never send more than one security digest per user inside this window,
/// regardless of how many distinct topics the feed surfaced. Without this a
/// burst of unrelated headlines (a hack, a wallet bug, a Linux CVE) produced a
/// message every few minutes.
const SECURITY_MIN_GAP_HOURS: i64 = 6;

/// Losses below this are background noise for a portfolio holder: small
/// protocol exploits, hot-wallet thefts, individual scam drains. Only an
/// incident on a named custody provider or a headline-sized loss is surfaced.
const MAJOR_SECURITY_USD: f64 = 100_000_000.0;

/// Max characters of an article body fed to the detector — enough to catch the
/// incident sentence without scanning whole feeds every 5 minutes.
const SECURITY_ARTICLE_CHARS: usize = 600;

/// (needle, incident class). Matched case-insensitively against the news
/// summaries. Both English and Russian phrasing are covered because the news
/// worker may summarise in either language.
const INCIDENT_WORDS: &[(&str, &str)] = &[
    ("hack", "breach"),
    ("breach", "breach"),
    ("exploit", "exploit"),
    ("stolen", "theft"),
    ("stole", "theft"),
    ("theft", "theft"),
    ("drained", "theft"),
    ("compromised", "breach"),
    ("phishing", "phishing"),
    ("depeg", "depeg"),
    ("insolven", "insolvency"),
    ("frozen", "freeze"),
    ("vulnerabilit", "vulnerability"),
    ("backdoor", "backdoor"),
    ("malware", "malware"),
    ("взлом", "breach"),
    ("взлома", "breach"),
    ("украд", "theft"),
    ("похит", "theft"),
    ("эксплойт", "exploit"),
    ("депег", "depeg"),
    ("замороз", "freeze"),
    ("фишинг", "phishing"),
];

/// Entities we track as a security topic. Order matters: the first match wins,
/// so specific brands come before generic infrastructure words.
/// Entities whose compromise can directly cost the user money (exchanges,
/// bridges, stablecoin issuers). These always warrant an alert.
const FUND_CUSTODY_ENTITIES: &[&str] = &[
    "binance",
    "coinbase",
    "bybit",
    "kraken",
    "kucoin",
    "bitfinex",
    "okx",
    "htx",
    "huobi",
    "gate.io",
    "tether",
    "usdt",
    "usdc",
    "circle",
    "ronin",
    "wormhole",
    "nomad",
    "poly network",
    "bridge",
];

/// Hardware/software wallet brands. A bug here matters, but only when the text
/// describes an actual compromise — product news like "Trezor lifts password
/// ceiling after Coldcard's thefts" must not page the user.
const WALLET_BRANDS: &[&str] = &["ledger", "trezor", "metamask", "phantom", "safepal"];

/// Words that describe an active compromise (as opposed to a mere mention).
const ACTIVE_COMPROMISE_WORDS: &[&str] = &[
    "hack",
    "breach",
    "exploit",
    "stolen",
    "stole",
    "drained",
    "compromised",
    "vulnerabilit",
    "backdoor",
    "malware",
    "взлом",
    "украд",
    "похит",
    "эксплойт",
];

const SECURITY_ENTITIES: &[&str] = &[
    "binance",
    "coinbase",
    "bybit",
    "kraken",
    "kucoin",
    "bitfinex",
    "okx",
    "htx",
    "huobi",
    "gate.io",
    "ledger",
    "trezor",
    "metamask",
    "phantom",
    "safepal",
    "tether",
    "usdt",
    "usdc",
    "circle",
    "ronin",
    "wormhole",
    "nomad",
    "poly network",
    "bridge",
];

/// Deterministic security-incident detection over cached news summaries.
///
/// The watcher severity is decided by a free-tier LLM, and on 2026-10-30 that
/// model returned `none` for a record-breaking $6.3B Binance breach: the news
/// summary existed in the DB but the user was never notified. This scan does
/// not depend on the model, so an incident is always surfaced.
///
/// Returns a stable topic key (`binance`, `ledger`, `breach`, ...) used to
/// deduplicate alerts for the same incident.
pub fn detect_security_topic(text: &str) -> Option<String> {
    // "ETH hackathon" is not an incident.
    let lower = text.to_lowercase().replace("hackathon", " ");
    let class = INCIDENT_WORDS
        .iter()
        .find(|(needle, _)| lower.contains(needle))
        .map(|(_, class)| *class)?;
    match SECURITY_ENTITIES.iter().find(|e| lower.contains(**e)) {
        Some(entity) => Some((*entity).to_string()),
        None => Some(class.to_string()),
    }
}

/// True when `topic` is worth a Telegram message.
///
/// Detecting an incident word is cheap and produces far too many hits: over two
/// days a real Binance breach was followed by alerts for a $138k DeFi drain, a
/// BTCPay hot-wallet theft, Coldcard wallet news and a Linux CVE affecting
/// Solana validators. The rule now is:
/// - custody entities (exchange/bridge/stablecoin) → always;
/// - wallet brands → only on an active compromise word;
/// - generic classes (`theft`, `vulnerability`, ...) → only for losses of
///   `MAJOR_SECURITY_USD` or more.
fn incident_is_significant(topic: &str, text: &str) -> bool {
    if FUND_CUSTODY_ENTITIES.contains(&topic) {
        return true;
    }
    let lower = text.to_lowercase();
    if WALLET_BRANDS.contains(&topic) {
        return ACTIVE_COMPROMISE_WORDS
            .iter()
            .any(|needle| lower.contains(needle));
    }
    incident_amount_usd(text)
        .map(|amount| amount >= MAJOR_SECURITY_USD)
        .unwrap_or(false)
}

/// Largest USD figure mentioned in `text` (`$6.3B`, `$100M`, `$5,800,000`,
/// `$2.65 million`). Suffixes may be attached or separated by a space.
fn incident_amount_usd(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut best: Option<f64> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            let mut j = i + 1;
            let mut digits = String::new();
            while j < bytes.len()
                && (bytes[j].is_ascii_digit() || bytes[j] == b',' || bytes[j] == b'.')
            {
                if bytes[j] != b',' {
                    digits.push(bytes[j] as char);
                }
                j += 1;
            }
            if let Ok(value) = digits.parse::<f64>() {
                let attached = bytes.get(j).map(|b| b.to_ascii_lowercase());
                let mut k = j;
                while k < bytes.len() && bytes[k] == b' ' {
                    k += 1;
                }
                let word: String = text[k..]
                    .chars()
                    .take_while(|c| c.is_alphabetic())
                    .collect::<String>()
                    .to_lowercase();
                let mult = match attached {
                    Some(b'b') => 1e9,
                    Some(b'm') => 1e6,
                    Some(b'k') => 1e3,
                    _ if word.starts_with("trillion") => 1e12,
                    _ if word.starts_with("billion") => 1e9,
                    _ if word.starts_with("million") => 1e6,
                    _ if word.starts_with("thousand") => 1e3,
                    _ => 1.0,
                };
                let total = value * mult;
                best = Some(best.map_or(total, |current: f64| current.max(total)));
            }
        }
        i += 1;
    }
    best
}

/// True when this topic has not been delivered (or was delivered more than
/// `SECURITY_TOPIC_COOLDOWN_DAYS` ago). Uses the per-topic history, not a single
/// "last topic" slot, so alternating incidents cannot re-trigger each other.
fn topic_is_fresh(
    seen: &std::collections::HashMap<String, chrono::DateTime<Utc>>,
    topic: &str,
    now: chrono::DateTime<Utc>,
) -> bool {
    seen.get(topic)
        .map(|ts| now.signed_duration_since(*ts).num_days() >= SECURITY_TOPIC_COOLDOWN_DAYS)
        .unwrap_or(true)
}

/// True when the per-user security digest gap has elapsed. Keeps a burst of
/// unrelated headlines to one message per `SECURITY_MIN_GAP_HOURS`.
fn security_gap_elapsed(p: &crate::models::Portfolio, now: chrono::DateTime<Utc>) -> bool {
    p.settings
        .ai_last_security_notify
        .map(|t| now.signed_duration_since(t).num_hours() >= SECURITY_MIN_GAP_HOURS)
        .unwrap_or(true)
}

/// Record delivered topics and prune entries older than four cooldown windows.
fn remember_topics(
    p: &mut crate::models::Portfolio,
    topics: &[String],
    now: chrono::DateTime<Utc>,
) {
    let seen = &mut p.settings.ai_security_seen;
    for topic in topics {
        seen.insert(topic.clone(), now);
    }
    let keep_days = SECURITY_TOPIC_COOLDOWN_DAYS * 4;
    seen.retain(|_, ts| now.signed_duration_since(*ts).num_days() < keep_days);
    // Keep the legacy single-slot fields in sync for older readers/debugging.
    p.settings.ai_last_security_topic = topics.last().cloned();
    p.settings.ai_last_security_notify = Some(now);
    p.settings.ai_last_important_notify = Some(now);
}

/// Pull the sentence that mentions the incident, so a security alert can be
/// generated even when the LLM call fails or returns `none`.
fn extract_security_snippet(text: &str) -> Option<String> {
    // Split on `.`/`!`/newline only when followed by whitespace or the end, so a
    // decimal like `$6.3B` does not truncate the sentence.
    let mut sentences: Vec<&str> = Vec::new();
    let mut start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' || b == b'!' || b == b'.' {
            let next_is_space = text
                .get(i + 1..)
                .map(|s| s.chars().next().map(|c| c.is_whitespace()).unwrap_or(true))
                .unwrap_or(true);
            if next_is_space {
                sentences.push(&text[start..i + 1]);
                start = i + 1;
            }
        }
    }
    if start < text.len() {
        sentences.push(&text[start..]);
    }

    let sentence = sentences.into_iter().find(|s| {
        let lower = s.to_lowercase();
        INCIDENT_WORDS
            .iter()
            .any(|(needle, _)| lower.contains(needle))
    })?;
    let snippet = sentence.trim();
    if snippet.is_empty() {
        None
    } else {
        Some(snippet.chars().take(500).collect())
    }
}

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
    // Deterministic security scanner on its own short cadence. It does not use
    // the AI provider, price quotes, or the 2–4 h watcher interval, so an
    // exchange breach is reported within one news-prefetch cycle.
    {
        let sec_bot = bot.clone();
        let sec_store = store.clone();
        let sec_sqlite = sqlite.clone();
        let sec_config = config.clone();
        tokio::spawn(async move {
            loop {
                run_security_scan(&sec_bot, &sec_store, &sec_sqlite, &sec_config).await;
                tokio::time::sleep(Duration::from_secs(SECURITY_SCAN_INTERVAL_SECS)).await;
            }
        });
    }

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

    // Deterministic safety net, evaluated BEFORE any quote provider call so a
    // provider outage can never hide an incident. Never depend on the LLM to
    // notice security news: on 2026-10-30 the model returned `none` for a $6.3B
    // Binance breach and the user was never told.
    //
    // Only articles fetched in the last 24h count, and detection is per article
    // (never over a concatenated corpus). The old corpus-wide scan matched the
    // word "hack" anywhere in a week of cached summaries and re-fired the same
    // "Security incident detected" card over and over.
    let incidents = fresh_security_incidents(sqlite, store, user_id).await;
    let security_topic = incidents.first().map(|(t, _)| t.clone());
    let security_text = incidents.first().map(|(_, t)| t.clone());
    let now = Utc::now();
    let fresh_security = security_topic
        .as_deref()
        .map(|topic| {
            store.read(user_id, |p| {
                topic_is_fresh(&p.settings.ai_security_seen, topic, now)
            })
        })
        .unwrap_or(false);

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
            if fresh_security {
                send_security_alert(
                    bot,
                    store,
                    user_id,
                    now,
                    security_topic,
                    security_text.as_deref().unwrap_or(""),
                )
                .await;
            }
            return Ok(());
        }
    };

    if quotes.is_empty() {
        if fresh_security {
            send_security_alert(
                bot,
                store,
                user_id,
                now,
                security_topic,
                security_text.as_deref().unwrap_or(""),
            )
            .await;
        }
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
        .await
        .unwrap_or_else(|e| {
            warn!("Watcher: AI analysis failed: {e}");
            WatcherResult {
                severity: WatcherSeverity::None,
                headline: String::new(),
                detail: String::new(),
            }
        });

    if result.severity == WatcherSeverity::None && !fresh_security {
        return Ok(());
    }

    // A fresh security incident is at least `important` and ignores the global
    // 12h/48h market cooldowns; deduplication is per topic (7 days) instead.
    let severity = if fresh_security {
        match result.severity {
            WatcherSeverity::Critical => WatcherSeverity::Critical,
            _ => WatcherSeverity::Important,
        }
    } else {
        result.severity
    };

    let can_notify = if fresh_security {
        let topic = security_topic.clone();
        store.write(user_id, |p| {
            if !security_gap_elapsed(p, now) {
                return false;
            }
            // Re-check atomically: the fast security scanner may have already
            // delivered this incident while the AI analysis was running.
            let fresh = topic
                .as_deref()
                .map(|t| topic_is_fresh(&p.settings.ai_security_seen, t, now))
                .unwrap_or(false);
            if !fresh {
                return false;
            }
            if let Some(t) = topic.as_deref() {
                remember_topics(p, &[t.to_string()], now);
            }
            if severity == WatcherSeverity::Critical {
                p.settings.ai_last_critical_notify = Some(now);
            }
            true
        })
    } else {
        store.write(user_id, |p| match severity {
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
        })
    };

    // Only record the analysis as "advised" when the user actually receives
    // the message. Storing it on a cooldown-suppressed tick poisons the next
    // prompt with "what you advised earlier" and the model concludes the user
    // was already notified — alerts then arrive days late ("still active").
    if !can_notify {
        return Ok(());
    }
    if !result.detail.is_empty() {
        store.write(user_id, |p| {
            p.settings.ai_last_watcher_analysis = Some(result.detail.clone());
        });
    }

    let (emoji, header) = match severity {
        WatcherSeverity::Critical => ("🚨", "AI Market Alert"),
        _ => ("📢", "AI Market Update"),
    };
    // The AI may have returned nothing (quota error, `none`, empty JSON). For a
    // fresh incident fall back to the news summary itself so the alert still
    // reaches the user.
    let (headline, detail) = if result.severity == WatcherSeverity::None {
        (
            "⚠️ Security incident detected".to_string(),
            security_text
                .as_deref()
                .and_then(extract_security_snippet)
                .unwrap_or_else(|| "A security incident was reported in the news feed.".into()),
        )
    } else {
        (result.headline.clone(), result.detail.clone())
    };
    let msg = format!("{emoji} <b>{header}</b>\n\n{headline}\n\n{detail}");

    if let Err(e) = bot
        .send_message(ChatId(user_id as i64), &msg)
        .parse_mode(teloxide::types::ParseMode::Html)
        .await
    {
        warn!("Watcher: failed to notify user {user_id}: {e}");
    }

    Ok(())
}

/// Deterministic security scan over the freshest news, independent of the AI
/// watcher. Runs every `SECURITY_SCAN_INTERVAL_SECS`.
///
/// Detection is per article (not over a concatenated feed), so an incident word
/// in one story and an exchange name in another cannot produce a false alert.
/// Duplicate suppression is atomic inside a single `Store::write`, so the AI
/// watcher and this scanner can never both notify about the same incident.
pub async fn run_security_scan(bot: &Bot, store: &Store, sqlite: &SqliteStore, config: &Config) {
    let ai = GeminiClient::new(&config.gemini_api_key);
    for user_id in store.all_user_ids() {
        let enabled = store.read(user_id, |p| p.settings.ai_watcher_enabled);
        if !enabled {
            continue;
        }

        let symbols = collect_symbols(store, user_id);
        let mut candidates: Vec<(String, String)> = Vec::new();

        fn push_unique(candidates: &mut Vec<(String, String)>, topic: String, text: String) {
            if !candidates.iter().any(|(t, _)| *t == topic) {
                candidates.push((topic, text));
            }
        }

        // Market-wide incidents (hacks, exchange halts) often only appear in the
        // general summary, never in a single symbol's feed.
        if let Ok(Some(general)) = sqlite.get_news_summary("GENERAL").await {
            if !general.contains("Failed to analyze") {
                if let Some(topic) = detect_security_topic(&general) {
                    if incident_is_significant(&topic, &general) {
                        push_unique(&mut candidates, topic, general);
                    }
                }
            }
        }

        // Scan per article: an incident word in one story and an exchange name
        // in another must not be combined into a false alert.
        'symbols: for sym in &symbols {
            let Ok(articles) = sqlite.get_news_articles(sym, 1).await else {
                continue;
            };
            for (_source, title, body, _url, _fetched_at) in articles {
                let body: String = body.chars().take(SECURITY_ARTICLE_CHARS).collect();
                let text = format!("{title}. {body}");
                if let Some(topic) = detect_security_topic(&text) {
                    if incident_is_significant(&topic, &text) {
                        push_unique(&mut candidates, topic, text);
                    }
                }
                if candidates.len() >= SECURITY_MAX_ALERTS_PER_SCAN {
                    break 'symbols;
                }
            }
        }

        // Named entities (binance, ledger, ...) outrank generic classes
        // (`breach`, `theft`), so a minor unaffiliated exploit cannot hide the
        // exchange breach the user actually cares about.
        let prioritized: Vec<(String, String)> = prioritize_candidates(candidates)
            .into_iter()
            .take(SECURITY_MAX_ALERTS_PER_SCAN)
            .collect();

        let now = Utc::now();
        // Only incidents the user has not heard about inside the cooldown.
        let fresh: Vec<(String, String)> = prioritized
            .into_iter()
            .filter(|(topic, _)| {
                store.read(user_id, |p| {
                    topic_is_fresh(&p.settings.ai_security_seen, topic, now)
                })
            })
            .collect();
        if fresh.is_empty() {
            continue;
        }

        let lang = store.read(user_id, |p| p.settings.preferred_language.clone());
        let digest = compose_security_digest(&ai, &fresh, &lang, sqlite).await;

        // Mark exactly the topics we are about to report, atomically, so the AI
        // watcher path cannot deliver the same incident concurrently. The global
        // gap check happens in the same critical section, so a burst of fresh
        // topics still yields a single message per `SECURITY_MIN_GAP_HOURS`.
        let topics: Vec<String> = fresh.iter().map(|(t, _)| t.clone()).collect();
        let delivered: Vec<String> = store.write(user_id, |p| {
            if !security_gap_elapsed(p, now) {
                return Vec::new();
            }
            let still_fresh: Vec<String> = topics
                .iter()
                .filter(|t| topic_is_fresh(&p.settings.ai_security_seen, t, now))
                .cloned()
                .collect();
            if !still_fresh.is_empty() {
                remember_topics(p, &still_fresh, now);
            }
            still_fresh
        });
        if delivered.is_empty() {
            continue;
        }

        info!(
            "Security scan: alerting user {user_id} about {} incident(s): {}",
            delivered.len(),
            delivered.join(", ")
        );
        // One pretty rich message (Telegram renders headings/lists nicely); the
        // old HTML send stays as a fallback.
        let rich = crate::bot::rich_messages::send_rich_message_markdown(
            config,
            user_id as i64,
            &richify(&digest),
        )
        .await;
        if let Err(e) = rich {
            warn!("Security scan: rich send failed for {user_id} ({e}); using HTML");
            if let Err(e) = bot
                .send_message(ChatId(user_id as i64), &digest)
                .parse_mode(teloxide::types::ParseMode::Html)
                .await
            {
                warn!("Security scan: failed to notify user {user_id}: {e}");
            }
        }
    }
}

/// Fresh (last 24h) per-article security incidents for one user, deduplicated
/// by topic and ordered so named entities (exchanges, wallets) come first.
///
/// This is the only input for the AI watcher's security path: scanning a week
/// of cached summaries made every old "hack" headline look new.
async fn fresh_security_incidents(
    sqlite: &SqliteStore,
    store: &Store,
    user_id: u64,
) -> Vec<(String, String)> {
    let symbols = collect_symbols(store, user_id);
    let mut out: Vec<(String, String)> = Vec::new();

    // Market-wide incidents (exchange hacks, stablecoin depegs) often only
    // appear in the general summary, not in a single symbol's feed.
    if let Ok(Some(general)) = sqlite.get_news_summary("GENERAL").await {
        if !general.contains("Failed to analyze") {
            if let Some(topic) = detect_security_topic(&general) {
                if incident_is_significant(&topic, &general) {
                    out.push((topic, general));
                }
            }
        }
    }

    for sym in &symbols {
        let Ok(articles) = sqlite.get_news_articles(sym, 1).await else {
            continue;
        };
        for (_source, title, body, _url, _fetched_at) in articles {
            let body: String = body.chars().take(SECURITY_ARTICLE_CHARS).collect();
            let text = format!("{title}. {body}");
            if let Some(topic) = detect_security_topic(&text) {
                if incident_is_significant(&topic, &text) && !out.iter().any(|(t, _)| *t == topic) {
                    out.push((topic, text));
                }
            }
        }
    }
    prioritize_candidates(out)
}

/// Convert the `• ` bullets the security prompts produce into rich-markdown
/// list items, so the digest renders as a proper list instead of hanging
/// bullets. Everything else (inline `<b>`) is valid rich markdown already.
fn richify(digest: &str) -> String {
    digest
        .lines()
        .map(|l| {
            let t = l.trim_start();
            match t.strip_prefix("• ") {
                Some(rest) => format!("- {rest}"),
                None => l.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One nicely written alert for every fresh incident, instead of one message per
/// match. Uses the AI when configured and falls back to deterministic snippets
/// when the providers are down.
async fn compose_security_digest(
    ai: &GeminiClient,
    incidents: &[(String, String)],
    preferred_language: &str,
    sqlite: &SqliteStore,
) -> String {
    let mut source = String::new();
    for (i, (topic, text)) in incidents.iter().enumerate() {
        let snippet = extract_security_snippet(text).unwrap_or_else(|| text.clone());
        source.push_str(&format!("[{}] {}\n\n", topic, snippet));
        if i + 1 >= incidents.len() {
            break;
        }
    }

    let lang = match preferred_language.trim() {
        "" => "Russian",
        "ru" => "Russian",
        "en" => "English",
        other => other,
    };

    if ai.is_configured() {
        if let Ok(text) = ai
            .security_digest(&source, lang, std::sync::Arc::new(sqlite.clone()))
            .await
        {
            let text = text.trim();
            if !text.is_empty() {
                return format!("🚨 <b>Security alert</b>\n\n{text}");
            }
        }
    }

    let mut msg = "🚨 <b>Security alert</b>\n\n".to_string();
    for (topic, text) in incidents {
        let snippet = extract_security_snippet(text).unwrap_or_else(|| text.clone());
        msg.push_str(&format!(
            "• <b>{}</b>: {}\n",
            html_escape::encode_text(topic),
            html_escape::encode_text(&snippet)
        ));
    }
    msg
}

/// Deliver a security incident when the AI path is unavailable (provider
/// outage, empty quotes) — and mark it as delivered so the same incident is not
/// repeated for a week.
async fn send_security_alert(
    bot: &Bot,
    store: &Store,
    user_id: u64,
    now: chrono::DateTime<Utc>,
    topic: Option<String>,
    corpus: &str,
) {
    let Some(topic) = topic else {
        return;
    };
    // Re-check atomically: the fast security scanner may have already delivered
    // this incident while the AI analysis was running.
    let deliver = store.write(user_id, |p| {
        if !security_gap_elapsed(p, now) {
            return false;
        }
        if !topic_is_fresh(&p.settings.ai_security_seen, &topic, now) {
            return false;
        }
        remember_topics(p, &[topic], now);
        true
    });
    if !deliver {
        return;
    }
    let detail = extract_security_snippet(corpus)
        .unwrap_or_else(|| "A security incident was reported in the news feed.".into());
    let msg =
        format!("🚨 <b>AI Market Alert</b>\n\n⚠️ <b>Security incident detected</b>\n\n{detail}");
    if let Err(e) = bot
        .send_message(ChatId(user_id as i64), &msg)
        .parse_mode(teloxide::types::ParseMode::Html)
        .await
    {
        warn!("Watcher: failed to send security alert to {user_id}: {e}");
    }
}

/// Entities the user can actually hold (exchanges, wallets, stablecoins) come
/// before generic incident classes, so an unrelated small exploit cannot crowd
/// out an exchange breach under the per-scan alert cap.
fn prioritize_candidates(mut candidates: Vec<(String, String)>) -> Vec<(String, String)> {
    candidates.sort_by_key(|(topic, _)| !SECURITY_ENTITIES.contains(&topic.as_str()));
    candidates
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
    fn richify_turns_bullets_into_list_items() {
        let d = "🚨 <b>Security alert</b>\n\n• Binance: $40M drained\n• Ledger: firmware implant";
        let r = richify(d);
        assert!(r.contains("- Binance: $40M drained"));
        assert!(r.contains("- Ledger: firmware implant"));
        assert!(!r.contains("• "));
        // Header untouched.
        assert!(r.starts_with("🚨 <b>Security alert</b>"));
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

    #[test]
    fn detects_binance_breach() {
        let summary = "A record-breaking $6.3 billion Binance hack saw 2.1M BTC and \
                      1.1M ETH stolen by the TraderTraitor group.";
        assert_eq!(detect_security_topic(summary).as_deref(), Some("binance"));
    }

    #[test]
    fn ignores_hackathon_and_benign_news() {
        assert_eq!(
            detect_security_topic("ETH hackathon announced for next month"),
            None
        );
        assert_eq!(
            detect_security_topic("BTC is up 3% on strong ETF inflows"),
            None
        );
    }

    #[test]
    fn detects_russian_wording() {
        let summary = "Крупнейший взлом биржи затронул Ledger-кошельки пользователей.";
        assert_eq!(detect_security_topic(summary).as_deref(), Some("ledger"));
    }

    #[test]
    fn security_topic_dedup_window() {
        use std::collections::HashMap;
        let now = Utc::now();
        let mut seen: HashMap<String, chrono::DateTime<Utc>> = HashMap::new();
        // First sighting → alert.
        assert!(topic_is_fresh(&seen, "binance", now));
        seen.insert("binance".into(), now - chrono::Duration::days(1));
        // Same topic one day later → suppressed.
        assert!(!topic_is_fresh(&seen, "binance", now));
        // Different entity → alert immediately.
        assert!(topic_is_fresh(&seen, "ledger", now));
        // Same topic after the window → alert again.
        assert!(topic_is_fresh(
            &seen,
            "binance",
            now + chrono::Duration::days(8)
        ));
    }

    #[test]
    fn alternating_incidents_do_not_repeat() {
        // Regression: a single "last topic" slot re-alerted binance every scan
        // whenever a second incident (e.g. "theft") appeared in between.
        use std::collections::HashMap;
        let now = Utc::now();
        let mut seen: HashMap<String, chrono::DateTime<Utc>> = HashMap::new();
        seen.insert("binance".into(), now - chrono::Duration::minutes(10));
        seen.insert("theft".into(), now - chrono::Duration::minutes(5));
        assert!(!topic_is_fresh(&seen, "binance", now));
        assert!(!topic_is_fresh(&seen, "theft", now));
    }

    #[test]
    fn remember_topics_prunes_stale_entries() {
        use std::collections::HashMap;
        let now = Utc::now();
        let mut p = crate::models::Portfolio::default();
        p.settings.ai_security_seen = HashMap::new();
        p.settings
            .ai_security_seen
            .insert("ancient".into(), now - chrono::Duration::days(60));
        remember_topics(&mut p, &["binance".to_string()], now);
        assert!(!p.settings.ai_security_seen.contains_key("ancient"));
        assert!(p.settings.ai_security_seen.contains_key("binance"));
        assert_eq!(
            p.settings.ai_last_security_topic.as_deref(),
            Some("binance")
        );
    }

    #[test]
    fn entity_topics_outrank_generic_classes() {
        let cands = vec![
            ("exploit".to_string(), "x".to_string()),
            ("binance".to_string(), "y".to_string()),
            ("breach".to_string(), "z".to_string()),
        ];
        let ordered: Vec<String> = prioritize_candidates(cands)
            .into_iter()
            .map(|(t, _)| t)
            .collect();
        assert_eq!(ordered, vec!["binance", "exploit", "breach"]);
    }

    #[test]
    fn snippet_covers_the_incident_sentence() {
        let text = "BTC flat. Binance hack drains $6.3B. ETH follows.";
        let snippet = extract_security_snippet(&text).unwrap();
        assert!(snippet.contains("Binance hack"));
        assert!(snippet.contains("$6.3B"));
    }

    #[test]
    fn parses_usd_amounts() {
        assert_eq!(incident_amount_usd("Binance drained $6.3B"), Some(6.3e9));
        assert_eq!(incident_amount_usd("a $100M wallet theft"), Some(1.0e8));
        assert_eq!(incident_amount_usd("lost $5,800,000 total"), Some(5.8e6));
        assert_eq!(
            incident_amount_usd("Polymarket $2.65 million exploit"),
            Some(2.65e6)
        );
        assert_eq!(incident_amount_usd("no numbers here"), None);
    }

    #[test]
    fn custody_entity_always_significant() {
        // The 2026-10-30 Binance breach is exactly what the alert exists for.
        let text = "Binance investigating hack as suspicious outflows reach $5.8 billion";
        let topic = detect_security_topic(text).unwrap();
        assert_eq!(topic, "binance");
        assert!(incident_is_significant(&topic, text));
    }

    #[test]
    fn small_generic_incidents_are_ignored() {
        // Real 2026-11-02 noise that paged the user: BTCPay hot wallets, a Linux
        // CVE affecting Solana validators and a $138k DeFi drain.
        for text in [
            "BTCPay Server warns of hot wallet thefts running on outdated software",
            "Solana validators told to act immediately as critical Linux vulnerability emerges",
            "Hackers hijack a dozen DeFi websites and steal about $138,000",
        ] {
            let topic = detect_security_topic(text).unwrap();
            assert!(
                !incident_is_significant(&topic, text),
                "should be filtered: {text}"
            );
        }
    }

    #[test]
    fn headline_sized_generic_loss_is_significant() {
        let text = "Unknown DeFi protocol hacked for $6.3B in the largest heist this year";
        let topic = detect_security_topic(text).unwrap();
        assert!(incident_is_significant(&topic, text));
    }

    #[test]
    fn wallet_brand_requires_active_compromise() {
        let product_news = "Trezor lifts password ceiling after Coldcard's $100M wallet thefts";
        let topic = detect_security_topic(product_news).unwrap();
        assert_eq!(topic, "trezor");
        assert!(!incident_is_significant(&topic, product_news));

        let active = "Ledger users drained after malicious firmware vulnerability";
        assert_eq!(detect_security_topic(active).as_deref(), Some("ledger"));
        assert!(incident_is_significant("ledger", active));
    }

    #[test]
    fn security_gap_suppresses_bursts() {
        let now = Utc::now();
        let mut p = crate::models::Portfolio::default();
        assert!(security_gap_elapsed(&p, now));
        p.settings.ai_last_security_notify = Some(now - chrono::Duration::hours(1));
        assert!(!security_gap_elapsed(&p, now));
        p.settings.ai_last_security_notify = Some(now - chrono::Duration::hours(7));
        assert!(security_gap_elapsed(&p, now));
    }
}
