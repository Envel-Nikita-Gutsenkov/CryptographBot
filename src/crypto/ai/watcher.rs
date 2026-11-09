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
/// `binance` / `ledger` / ... we stay quiet about that entity for a month unless
/// a different one is hit. A hack that keeps being re-reported for weeks is
/// still the same incident; the alert is about action, not about the headline.
const SECURITY_TOPIC_COOLDOWN_DAYS: i64 = 30;

/// A per-article fingerprint (normalised opening text) suppresses the *same*
/// story even when it is re-served under a different URL or after the article
/// retention window re-inserted it. This is what stops a week-old hack from
/// firing again as "new news".
const SECURITY_FINGERPRINT_COOLDOWN_DAYS: i64 = 30;

/// Generic incident classes (`breach`, `theft`, ...) have no identity: the same
/// word shows up in unrelated stories, so they get a short cooldown instead of
/// the full weekly window. Named entities keep the long one.
const SECURITY_CLASS_COOLDOWN_DAYS: i64 = 2;

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

/// Language that turns a story into advice rather than a report of a loss.
/// "Binance warns iPhone users…", "founder warns about understaffing" and
/// "market is vulnerable to leverage" are not incidents: nothing has been lost
/// yet. They only count when the same text also carries realised-loss evidence.
const ADVISORY_WORDS: &[&str] = &[
    "warn",
    "warning",
    "warns",
    "warned",
    "advis",
    "caution",
    "urge",
    "urges",
    "remind",
    "предупред",
    "предупрежд",
    "совету",
    "рекоменд",
    "напомина",
    "призыв",
];

/// Wording that describes funds actually moving out of, or being frozen at, a
/// custodian *right now*. This is what separates a live exchange incident from
/// a review, a warning or a product announcement.
const CUSTODY_COMPROMISE_WORDS: &[&str] = &[
    "withdrawals halted",
    "halted withdrawals",
    "halts withdrawals",
    "halt withdrawals",
    "withdrawals suspended",
    "suspended withdrawals",
    "suspend withdrawals",
    "suspends withdrawals",
    "withdrawals frozen",
    "drained",
    "drain of",
    "stolen",
    "stole",
    "funds missing",
    "lost funds",
    "funds lost",
    "hot wallet drained",
    "keys exposed",
    "private keys exposed",
    "приостановил вывод",
    "приостановила вывод",
    "приостановили вывод",
    "остановил вывод",
    "остановила вывод",
    "заморозил средства",
    "заморозила средства",
    "украд",
    "похит",
];

/// Tokens that make a market-wide (`GENERAL`) headline crypto-related at all.
/// The GENERAL feed carries oil, macro and politics; without this gate a Brent
/// move or a jobs report could be run through the incident detector.
const CRYPTO_CONTEXT_WORDS: &[&str] = &[
    "crypto",
    "крипт",
    "bitcoin",
    "биткоин",
    "btc",
    "ethereum",
    "эфир",
    "blockchain",
    "блокчейн",
    "defi",
    "stablecoin",
    "стейблкоин",
    "altcoin",
    "web3",
    "on-chain",
    "token",
    "токен",
    "wallet",
    "кошел",
    "exchange",
    "биржа",
    "binance",
    "coinbase",
    "solana",
    "ledger",
    "trezor",
];

/// What a compromise of an entity means for the user's own funds. This is the
/// only thing that distinguishes an exchange from a wallet brand, so the
/// significance rule reads off it instead of keeping two parallel lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntityKind {
    /// Exchange, bridge or stablecoin issuer: funds can be lost directly, so
    /// any reported incident is worth a message.
    Custody,
    /// Hardware/software wallet brand: only an *active* compromise matters;
    /// product news like "Trezor lifts password ceiling" must not page anyone.
    Wallet,
}

/// One tracked organisation. `key` is the canonical dedup key written into the
/// per-user history; `aliases` are the lowercase needles that map onto it, so
/// a Russian article and an English one collapse onto the same key.
#[derive(Debug, PartialEq, Eq)]
struct SecurityEntity {
    key: &'static str,
    aliases: &'static [&'static str],
    kind: EntityKind,
}

/// Order matters: `detect_security_keys` preserves it, and named entities must
/// come before generic ones. Aliases live next to their key instead of in a
/// separate Cyrillic→Latin match, so adding a brand is one line here.
const SECURITY_ENTITIES: &[SecurityEntity] = &[
    SecurityEntity {
        key: "binance",
        aliases: &["binance", "бинанс"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "coinbase",
        aliases: &["coinbase"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "bybit",
        aliases: &["bybit"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "kraken",
        aliases: &["kraken"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "kucoin",
        aliases: &["kucoin"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "bitfinex",
        aliases: &["bitfinex"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "okx",
        aliases: &["okx"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "htx",
        aliases: &["htx"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "huobi",
        aliases: &["huobi"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "gate.io",
        aliases: &["gate.io"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "bitmart",
        aliases: &["bitmart", "битмарт"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "mercado bitcoin",
        aliases: &["mercado bitcoin"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "tether",
        aliases: &["tether"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "usdt",
        aliases: &["usdt"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "usdc",
        aliases: &["usdc"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "circle",
        aliases: &["circle"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "ronin",
        aliases: &["ronin"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "wormhole",
        aliases: &["wormhole"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "nomad",
        aliases: &["nomad"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "poly network",
        aliases: &["poly network"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "bridge",
        aliases: &["bridge"],
        kind: EntityKind::Custody,
    },
    SecurityEntity {
        key: "ledger",
        aliases: &["ledger"],
        kind: EntityKind::Wallet,
    },
    SecurityEntity {
        key: "trezor",
        aliases: &["trezor"],
        kind: EntityKind::Wallet,
    },
    SecurityEntity {
        key: "metamask",
        aliases: &["metamask"],
        kind: EntityKind::Wallet,
    },
    SecurityEntity {
        key: "phantom",
        aliases: &["phantom"],
        kind: EntityKind::Wallet,
    },
    SecurityEntity {
        key: "safepal",
        aliases: &["safepal"],
        kind: EntityKind::Wallet,
    },
];

/// One incident word. Every row is self-contained: the needle, the dedup
/// class it belongs to, and whether the wording describes a *live* compromise
/// (`active`) rather than a bare mention. That flag is what makes a
/// wallet-brand story signal, so it must be per word: "$100M thefts" is a
/// post-hoc noun, while "users drained" is an active drain.
struct IncidentWord {
    needle: &'static str,
    class: &'static str,
    active: bool,
}

/// Matched case-insensitively against news summaries. Both English and Russian
/// phrasing are covered because the news worker may summarise in either
/// language. The first matching row wins, so specific words come first.
const INCIDENT_WORDS: &[IncidentWord] = &[
    IncidentWord {
        needle: "hack",
        class: "breach",
        active: true,
    },
    IncidentWord {
        needle: "breach",
        class: "breach",
        active: true,
    },
    IncidentWord {
        needle: "exploit",
        class: "exploit",
        active: true,
    },
    IncidentWord {
        needle: "stolen",
        class: "theft",
        active: true,
    },
    IncidentWord {
        needle: "stole",
        class: "theft",
        active: true,
    },
    IncidentWord {
        needle: "theft",
        class: "theft",
        active: false,
    },
    IncidentWord {
        needle: "drained",
        class: "theft",
        active: true,
    },
    IncidentWord {
        needle: "compromised",
        class: "breach",
        active: true,
    },
    IncidentWord {
        needle: "phishing",
        class: "phishing",
        active: false,
    },
    IncidentWord {
        needle: "depeg",
        class: "depeg",
        active: false,
    },
    IncidentWord {
        needle: "insolven",
        class: "insolvency",
        active: false,
    },
    IncidentWord {
        needle: "frozen",
        class: "freeze",
        active: false,
    },
    IncidentWord {
        needle: "vulnerabilit",
        class: "vulnerability",
        active: true,
    },
    IncidentWord {
        needle: "backdoor",
        class: "backdoor",
        active: true,
    },
    IncidentWord {
        needle: "malware",
        class: "malware",
        active: true,
    },
    IncidentWord {
        needle: "взлом",
        class: "breach",
        active: true,
    },
    IncidentWord {
        needle: "украд",
        class: "theft",
        active: true,
    },
    IncidentWord {
        needle: "похит",
        class: "theft",
        active: true,
    },
    IncidentWord {
        needle: "эксплойт",
        class: "exploit",
        active: true,
    },
    IncidentWord {
        needle: "депег",
        class: "depeg",
        active: false,
    },
    IncidentWord {
        needle: "замороз",
        class: "freeze",
        active: false,
    },
    IncidentWord {
        needle: "фишинг",
        class: "phishing",
        active: false,
    },
];

/// First incident word present in the (lowercased) text.
fn find_incident(text: &str) -> Option<&'static IncidentWord> {
    INCIDENT_WORDS.iter().find(|w| text.contains(w.needle))
}

/// Lookup by canonical dedup key. Every topic produced by
/// `detect_security_keys` is an entity key, so this is also the entity test.
fn entity_by_key(key: &str) -> Option<&'static SecurityEntity> {
    SECURITY_ENTITIES.iter().find(|e| e.key == key)
}

fn is_entity(topic: &str) -> bool {
    entity_by_key(topic).is_some()
}

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
    detect_security_keys(text).into_iter().next()
}

/// Every stable dedup key this text carries: all named entities it mentions,
/// or — when none is named — the incident class.
///
/// One article can mention several entities (e.g. "Binance" and "Tether").
/// Deduping only on the first match let two code paths derive two different
/// keys for the same incident and deliver it twice; recording the whole set
/// keeps `binance` suppressed no matter which article or path fires next.
pub fn detect_security_keys(text: &str) -> Vec<String> {
    // "ETH hackathon" is not an incident.
    let lower = text.to_lowercase().replace("hackathon", " ");
    let Some(word) = find_incident(&lower) else {
        return Vec::new();
    };
    let entities: Vec<String> = SECURITY_ENTITIES
        .iter()
        .filter(|e| e.aliases.iter().any(|alias| lower.contains(alias)))
        .map(|e| e.key.to_string())
        .collect();
    if entities.is_empty() {
        vec![word.class.to_string()]
    } else {
        entities
    }
}

/// True when `topic` is worth a Telegram message.
///
/// Detecting an incident word is cheap and produces far too many hits: over two
/// days a real Binance breach was followed by alerts for a $138k DeFi drain, a
/// BTCPay hot-wallet theft, Coldcard wallet news and a Linux CVE affecting
/// Solana validators. The rule now is:
/// - advisory text ("X warns users about a possible issue") with no realised
///   loss is never an incident — this is what produced "BitMart founder warns",
///   "Binance warns iPhone users" and "market vulnerable to leverage";
/// - custody entities (exchange/bridge/stablecoin) → only with a headline-sized
///   loss or an active halt/freeze/drain of customer funds, not for merely being
///   named in a story;
/// - wallet brands → only on an active compromise word;
/// - generic classes (`theft`, `vulnerability`, ...) → only for losses of
///   `MAJOR_SECURITY_USD` or more.
fn incident_is_significant(topic: &str, text: &str) -> bool {
    let lower = text.to_lowercase();
    let realized = has_realized_loss(&lower, text);
    // A warning about a *possible* future problem is not news the user can act
    // on. Only a confirmed loss promotes it to an incident.
    if is_advisory(&lower) && !realized {
        return false;
    }
    match entity_by_key(topic).map(|e| e.kind) {
        Some(EntityKind::Custody) => realized,
        Some(EntityKind::Wallet) => INCIDENT_WORDS
            .iter()
            .any(|w| w.active && lower.contains(w.needle)),
        None => amount_is_headline(text),
    }
}

/// True when the text reads as advice/warning rather than a report.
fn is_advisory(lower: &str) -> bool {
    ADVISORY_WORDS.iter().any(|w| lower.contains(w))
}

/// A loss large enough to matter regardless of who was hit.
fn amount_is_headline(text: &str) -> bool {
    incident_amount_usd(text)
        .map(|amount| amount >= MAJOR_SECURITY_USD)
        .unwrap_or(false)
}

/// Evidence that funds were actually lost or frozen, as opposed to a warning
/// about a possible problem. Either a headline-sized dollar loss or concrete
/// custody wording (halted withdrawals, drained hot wallet, stolen funds).
fn has_realized_loss(lower: &str, text: &str) -> bool {
    amount_is_headline(text) || CUSTODY_COMPROMISE_WORDS.iter().any(|w| lower.contains(w))
}

/// True when a market-wide (`GENERAL`) headline is about crypto at all. Coin
/// feeds are crypto by construction; the GENERAL feed is not, so oil, macro and
/// politics must be filtered out before the incident detector sees them.
fn crypto_relevant(lower: &str) -> bool {
    CRYPTO_CONTEXT_WORDS.iter().any(|w| lower.contains(w))
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

/// True when this topic has not been delivered (or was delivered more than its
/// cooldown ago). Uses the per-topic history, not a single "last topic" slot, so
/// alternating incidents cannot re-trigger each other.
fn topic_is_fresh(
    seen: &std::collections::HashMap<String, chrono::DateTime<Utc>>,
    topic: &str,
    now: chrono::DateTime<Utc>,
) -> bool {
    let cooldown = if topic.starts_with("fp:") {
        SECURITY_FINGERPRINT_COOLDOWN_DAYS
    } else if is_entity(topic) {
        SECURITY_TOPIC_COOLDOWN_DAYS
    } else {
        SECURITY_CLASS_COOLDOWN_DAYS
    };
    seen.get(topic)
        .map(|ts| now.signed_duration_since(*ts).num_days() >= cooldown)
        .unwrap_or(true)
}

/// Normalised fingerprint of the opening of an article. Same story, same key —
/// no matter how many times a feed re-serves it.
fn incident_fingerprint(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut kept = 0usize;
    for c in text.chars() {
        if !c.is_alphanumeric() {
            continue;
        }
        let lower = c.to_lowercase().to_string();
        for b in lower.as_bytes() {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        kept += 1;
        if kept >= 120 {
            break;
        }
    }
    format!("fp:{hash:016x}")
}

/// Dedup keys for one article: every named entity (or the incident class) plus
/// the story fingerprint.
fn security_keys_for_text(text: &str) -> Vec<String> {
    let mut keys = detect_security_keys(text);
    if !keys.is_empty() {
        keys.push(incident_fingerprint(text));
    }
    keys
}

/// Every key must be unseen: a repeat that mentions one already-delivered
/// entity is the same incident, even if another entity in the sentence is new.
fn keys_are_fresh(
    seen: &std::collections::HashMap<String, chrono::DateTime<Utc>>,
    keys: &[String],
    now: chrono::DateTime<Utc>,
) -> bool {
    !keys.is_empty() && keys.iter().all(|k| topic_is_fresh(seen, k, now))
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

    let sentence = sentences
        .into_iter()
        .find(|s| find_incident(&s.to_lowercase()).is_some())?;
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
    // Let a user's /ask own the provider quotas while it runs.
    crate::crypto::ai::pools::background_gate(Duration::from_secs(20)).await;

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
    let security_text = incidents.first().map(|(_, t)| t.clone());
    // Dedup on every entity named in the incident, not just the first match:
    // different articles (and different code paths) can otherwise derive two
    // keys for one event and deliver it twice.
    let security_keys: Option<Vec<String>> = security_text
        .as_deref()
        .map(security_keys_for_text)
        .filter(|keys| !keys.is_empty());
    let now = Utc::now();
    let fresh_security = security_keys
        .as_deref()
        .map(|keys| {
            store.read(user_id, |p| {
                keys_are_fresh(&p.settings.ai_security_seen, keys, now)
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
                    security_keys.clone(),
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
                security_keys.clone(),
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
        let keys = security_keys.clone().unwrap_or_default();
        store.write(user_id, |p| {
            if !security_gap_elapsed(p, now) {
                return false;
            }
            // Re-check atomically: the fast security scanner may have already
            // delivered this incident while the AI analysis was running.
            if !keys_are_fresh(&p.settings.ai_security_seen, &keys, now) {
                return false;
            }
            remember_topics(p, &keys, now);
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

        let symbols = security_symbols(store, user_id);
        let mut candidates: Vec<(String, String)> = Vec::new();

        fn push_unique(candidates: &mut Vec<(String, String)>, topic: String, text: String) {
            if !candidates.iter().any(|(t, _)| *t == topic) {
                candidates.push((topic, text));
            }
        }

        // Scan per article: an incident word in one story and an exchange name
        // in another must not be combined into a false alert. GENERAL is the
        // market-wide feed, so exchange hacks that no single coin feed carries
        // are still covered — without re-reading a stale AI summary.
        'symbols: for sym in &symbols {
            let Ok(articles) = sqlite.get_news_articles(sym, 1).await else {
                continue;
            };
            for (_source, title, body, _url, _fetched_at) in articles {
                let body: String = body.chars().take(SECURITY_ARTICLE_CHARS).collect();
                let text = format!("{title}. {body}");
                // The GENERAL feed also carries oil, macro and politics; only
                // crypto-related headlines may reach the incident detector.
                if sym == "GENERAL" && !crypto_relevant(&text.to_lowercase()) {
                    continue;
                }
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
            .filter(|(_, text)| {
                let keys = security_keys_for_text(text);
                store.read(user_id, |p| {
                    keys_are_fresh(&p.settings.ai_security_seen, &keys, now)
                })
            })
            .collect();
        if fresh.is_empty() {
            continue;
        }

        let lang = store.read(user_id, |p| p.settings.preferred_language.clone());
        let digest = compose_security_digest(&ai, &fresh, &lang, sqlite).await;

        // Mark every key the delivered text carries — including the named
        // entities the composed digest mentions. The digest is what the user
        // sees ("Binance ..."), so those names must be suppressed next time even
        // if the raw article only said "an exchange".
        let mut topics: Vec<String> = fresh
            .iter()
            .flat_map(|(_, text)| security_keys_for_text(text))
            .collect();
        topics.extend(detect_security_keys(&digest));
        topics.sort();
        topics.dedup();
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
    let symbols = security_symbols(store, user_id);
    let mut out: Vec<(String, String)> = Vec::new();

    for sym in &symbols {
        let Ok(articles) = sqlite.get_news_articles(sym, 1).await else {
            continue;
        };
        for (_source, title, body, _url, _fetched_at) in articles {
            let body: String = body.chars().take(SECURITY_ARTICLE_CHARS).collect();
            let text = format!("{title}. {body}");
            if sym == "GENERAL" && !crypto_relevant(&text.to_lowercase()) {
                continue;
            }
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
    keys: Option<Vec<String>>,
    corpus: &str,
) {
    let Some(mut keys) = keys else {
        return;
    };
    // Also cover entity names that only appear in the snippet we are about to
    // send, so the next alert about the same exchange is suppressed.
    keys.extend(security_keys_for_text(corpus));
    keys.sort();
    keys.dedup();
    if keys.is_empty() {
        return;
    }
    // Re-check atomically: the fast security scanner may have already delivered
    // this incident while the AI analysis was running.
    let deliver = store.write(user_id, |p| {
        if !security_gap_elapsed(p, now) {
            return false;
        }
        if !keys_are_fresh(&p.settings.ai_security_seen, &keys, now) {
            return false;
        }
        remember_topics(p, &keys, now);
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
    candidates.sort_by_key(|(topic, _)| !is_entity(topic));
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

/// Symbols whose raw article feed is scanned for security incidents. GENERAL
/// carries the market-wide stories that a single coin feed may never mention.
fn security_symbols(store: &Store, user_id: u64) -> Vec<String> {
    let mut syms = collect_symbols(store, user_id);
    if !syms.iter().any(|s| s == "GENERAL") {
        syms.push("GENERAL".into());
    }
    syms
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
    fn entity_kinds_drive_the_significance_rule() {
        // Custody: any reported incident is signal. Wallet: only an active
        // compromise. Classes are not entities, so they fall back to the loss
        // threshold.
        assert_eq!(
            entity_by_key("binance").map(|e| e.kind),
            Some(EntityKind::Custody)
        );
        assert_eq!(
            entity_by_key("ledger").map(|e| e.kind),
            Some(EntityKind::Wallet)
        );
        assert_eq!(entity_by_key("breach"), None);
        assert!(is_entity("bitmart"));
        assert!(!is_entity("theft"));
    }

    #[test]
    fn cyrillic_alias_maps_to_the_latin_entity() {
        let ru = "Binance: бинанс взломан на $3,1 млрд";
        let keys = detect_security_keys(ru);
        assert_eq!(keys, vec!["binance".to_string()], "keys = {keys:?}");
    }

    #[test]
    fn incident_keys_include_every_named_entity() {
        // One article, two custody entities: both must be remembered, otherwise
        // the AI path and the scanner can derive different keys for one event.
        let text = "Tether froze funds after a Binance hack drained $3.1 billion";
        let keys = detect_security_keys(text);
        assert!(keys.contains(&"binance".to_string()), "keys = {keys:?}");
        assert!(keys.contains(&"tether".to_string()), "keys = {keys:?}");
        // Named entities replace the generic class, so unrelated breaches are
        // not suppressed for a week by this delivery.
        assert!(!keys.contains(&"breach".to_string()), "keys = {keys:?}");
    }

    #[test]
    fn generic_incident_class_expires_before_a_named_entity() {
        use std::collections::HashMap;
        let now = Utc::now();
        let mut seen: HashMap<String, chrono::DateTime<Utc>> = HashMap::new();
        seen.insert("breach".into(), now - chrono::Duration::days(3));
        // Class cooldown is 2 days → fresh again.
        assert!(topic_is_fresh(&seen, "breach", now));
        seen.insert("binance".into(), now - chrono::Duration::days(3));
        // Entity cooldown is a month → still suppressed.
        assert!(!topic_is_fresh(&seen, "binance", now));
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
            now + chrono::Duration::days(31)
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
            .insert("ancient".into(), now - chrono::Duration::days(130));
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
    fn headline_custody_breach_is_significant() {
        // The 2026-10-30 Binance breach is exactly what the alert exists for.
        let text = "Binance investigating hack as suspicious outflows reach $5.8 billion";
        let topic = detect_security_topic(text).unwrap();
        assert_eq!(topic, "binance");
        assert!(incident_is_significant(&topic, text));
    }

    #[test]
    fn custody_mention_without_a_loss_is_not_an_incident() {
        // Merely naming an exchange is not an incident. This is the exact class
        // of false alert the user received: warnings, reviews and staffing news.
        for text in [
            "BitMart: после взлома основатель предупредил о нехватке специалистов по безопасности",
            "Apple iOS Vulnerability Puts Crypto Wallets at Risk as Binance Issues Warning",
            "Binance says it is reviewing its security roadmap after the incident",
            "Mercado Bitcoin will resume withdrawals after an earlier outage",
        ] {
            assert!(
                detect_security_topic(text).is_none()
                    || !incident_is_significant(&detect_security_topic(text).unwrap(), text),
                "should be filtered: {text}"
            );
        }
    }

    #[test]
    fn advisory_without_realised_loss_is_suppressed() {
        // A wallet brand can be "active" in the wording yet still be a warning.
        let text = "Ledger warns users about a vulnerability that could expose private keys";
        let topic = detect_security_topic(text).unwrap();
        assert!(!incident_is_significant(&topic, text));
    }

    #[test]
    fn advisory_with_a_real_loss_stays_significant() {
        // "warns" must not hide a confirmed, headline-sized loss.
        let text = "Binance warns users after a $500M hack drained a hot wallet";
        let topic = detect_security_topic(text).unwrap();
        assert!(incident_is_significant(&topic, text));
    }

    #[test]
    fn active_custody_halt_is_significant_without_a_dollar_amount() {
        let text = "Kraken halts withdrawals after an exploit drained a hot wallet";
        let topic = detect_security_topic(text).unwrap();
        assert_eq!(topic, "kraken");
        assert!(incident_is_significant(&topic, text));
    }

    #[test]
    fn general_feed_rejects_non_crypto_headlines() {
        // Oil, macro and politics ride the GENERAL feed and must never reach the
        // incident detector, even if they mention losses or money.
        for text in [
            "Brent oil falls below $90 per barrel after Iran news",
            "Цены на нефть Brent упали на 4,6% после новости об Иране",
            "Fed minutes show officials split on rate cuts",
            "US adds 86,000 jobs in October",
        ] {
            assert!(!crypto_relevant(&text.to_lowercase()), "not crypto: {text}");
        }
        for text in [
            "Bitcoin ETF inflows hit a record",
            "Крипторынок вырос за неделю",
            "Binance listing update",
        ] {
            assert!(crypto_relevant(&text.to_lowercase()), "crypto: {text}");
        }
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
