#![allow(dead_code)]
//! Text formatting utilities — all display logic lives here, handlers stay clean.

use crate::crypto::providers::alternative_me::FearAndGreed;
use crate::models::{CmcQuote, GlobalMetrics, Period};

/// Truncate a string to at most `max` characters, appending `...` when cut.
/// Char-safe: never slices at a byte index that could split a UTF-8 char
/// (the old `&s[..800]` pattern panicked on Cyrillic/emoji article bodies).
pub fn truncate_chars(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push_str("...");
    }
    out
}

// ── Price formatting ──────────────────────────────────────────────────────────

pub fn format_thousands(n: f64, decimals: usize) -> String {
    let parts: Vec<String> = format!("{:.1$}", n, decimals)
        .split('.')
        .map(|s| s.to_string())
        .collect();
    let int_part = &parts[0];
    let mut result = String::new();
    let mut count = 0;
    for c in int_part.chars().rev() {
        if count > 0 && count % 3 == 0 {
            result.push(',');
        }
        result.push(c);
        count += 1;
    }
    let mut formatted = result.chars().rev().collect::<String>();
    if parts.len() > 1 {
        formatted.push('.');
        formatted.push_str(&parts[1]);
    }
    formatted
}

/// Format a USD price with appropriate decimal places.
pub fn fmt_price(price: f64) -> String {
    if price >= 1_000.0 {
        format!("${}", format_thousands(price, 2))
    } else if price >= 1.0 {
        format!("${}", format_thousands(price, 4))
    } else if price >= 0.001 {
        format!("${}", format_thousands(price, 6))
    } else {
        format!("${}", format_thousands(price, 8))
    }
}

/// Format a percentage change with emoji and sign.
pub fn fmt_change(pct: f64) -> String {
    let rounded = (pct * 100.0).round() / 100.0;
    if rounded >= 0.0 {
        format!("📈 +{:.2}%", rounded)
    } else {
        format!("📉 {:.2}%", rounded)
    }
}

pub fn fmt_change_plain(pct_opt: Option<f64>) -> String {
    match pct_opt {
        Some(pct) => {
            let rounded = (pct * 100.0).round() / 100.0;
            if rounded >= 0.0 {
                format!("+{:.2}%", rounded)
            } else {
                format!("{:.2}%", rounded)
            }
        }
        None => "N/A".to_string(),
    }
}

/// Format large USD amounts: $1.23B, $456.7M, $12.3K.
pub fn fmt_large_usd(n: f64) -> String {
    if n.abs() >= 1_000_000_000.0 {
        format!("${:.2}B", n / 1_000_000_000.0)
    } else if n.abs() >= 1_000_000.0 {
        format!("${:.2}M", n / 1_000_000.0)
    } else if n.abs() >= 1_000.0 {
        format!("${:.1}K", n / 1_000.0)
    } else {
        format!("${:.2}", n)
    }
}

/// Format a crypto amount with smart decimal truncation.
pub fn fmt_crypto(amount: f64, symbol: &str) -> String {
    if amount >= 1_000.0 {
        format!("{:.2} {}", amount, symbol)
    } else if amount >= 1.0 {
        format!("{:.4} {}", amount, symbol)
    } else {
        format!("{:.8} {}", amount, symbol)
    }
}

/// Format a P&L value with sign and color emoji.
pub fn fmt_pnl_usd(pnl: f64) -> String {
    if pnl >= 0.0 {
        format!("✅ +${:.2}", pnl)
    } else {
        format!("🔴 -${:.2}", pnl.abs())
    }
}

pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        match c {
            '_' | '*' | '[' | ']' | '(' | ')' | '~' | '`' | '>' | '#' | '+' | '-' | '=' | '|'
            | '{' | '}' | '.' | '!' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Redact API keys and bearer tokens from an error string before it is shown
/// to the user. Reqwest errors embed the full request URL (including
/// `?key=...`), which used to leak the Gemini key into Telegram error messages.
pub fn redact_secrets(s: &str) -> String {
    const MARKERS: &[(&str, &str)] = &[
        // (original text to match, replacement)
        ("key=", "key=[REDACTED]"),
        ("api_key=", "api_key=[REDACTED]"),
        ("apikey=", "apikey=[REDACTED]"),
        ("token=", "token=[REDACTED]"),
        ("access_token=", "access_token=[REDACTED]"),
        ("x-api-key=", "x-api-key=[REDACTED]"),
        ("authorization: bearer ", "Authorization: Bearer [REDACTED]"),
    ];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        // Google-style API key (AIza...) anywhere in the text.
        let mut earliest: Option<(usize, usize, &'static str)> = None; // (pos, orig_len, replacement)
        let mut search_from = 0usize;
        while let Some(rel) = rest[search_from..].find("AIza") {
            let abs = search_from + rel;
            let after = &rest[abs + 4..];
            let key_len = after
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .count();
            if key_len >= 20 {
                earliest = Some((abs, 4, "AIza[REDACTED]"));
                break;
            }
            search_from = abs + 4;
        }
        // Named query-param / header markers.
        let lower = rest.to_lowercase();
        for (orig, repl) in MARKERS {
            if let Some(rel) = lower.find(orig) {
                if earliest.map(|(p, _, _)| rel < p).unwrap_or(true) {
                    earliest = Some((rel, orig.len(), repl));
                }
            }
        }
        let Some((pos, orig_len, replacement)) = earliest else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..pos]);
        out.push_str(replacement);
        if replacement.starts_with("Authorization") {
            // Redact the whole "Authorization: Bearer <token>" header value.
            let after = &rest[pos + orig_len..];
            let skip = after
                .chars()
                .take_while(|c| !matches!(c, '\n' | '\r'))
                .count();
            rest = &after[skip..];
        } else {
            // Redact the value up to the next separator.
            let after = &rest[pos + orig_len..];
            let skip = after
                .chars()
                .take_while(|c| !matches!(c, '&' | ' ' | '\"' | '\'' | ')' | ']' | '\n' | '\r'))
                .count();
            rest = &after[skip..];
        }
    }
    out
}

pub fn esc_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for c in s.chars() {
        match c {
            '\\' | '`' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

fn change_emoji_val(pct_opt: Option<f64>, has_data: bool) -> String {
    if !has_data || pct_opt.is_none() {
        return "*N/A*".to_string();
    }
    let pct = pct_opt.unwrap();
    let emoji = if pct >= 0.0 { "🟢" } else { "🔴" };
    let sign = if pct >= 0.0 { "+" } else { "" };
    format!("{} `{}{:.2}%`", emoji, sign, pct)
}

// ── CMC quote card ────────────────────────────────────────────────────────────

pub fn fmt_quote_card(q: &CmcQuote) -> String {
    let esc_sym = esc(&q.symbol);
    let esc_name = esc(&q.name);
    let esc_rank = if q.rank > 0 {
        format!("`Rank \\#{}`", esc_code(&q.rank.to_string()))
    } else {
        "`Rank: N/A`".to_string()
    };
    let esc_price = esc(&fmt_price(q.price_usd));

    let c1h = change_emoji_val(q.change_1h, true);
    let c24h = change_emoji_val(q.change_24h, true);
    let c7d = change_emoji_val(q.change_7d, true);
    let c30d = change_emoji_val(q.change_30d, true);

    let vol = if q.volume_24h > 0.0 {
        esc(&fmt_large_usd(q.volume_24h))
    } else {
        "*N/A*".to_string()
    };
    let cap = if q.market_cap > 0.0 {
        esc(&fmt_large_usd(q.market_cap))
    } else {
        "*N/A*".to_string()
    };

    let supply_str = if q.circulating_supply > 0.0 {
        format!(
            "\n         └  Supply    :  {} {}",
            esc(&fmt_large_usd(q.circulating_supply).replace('$', "")),
            esc_sym
        )
    } else {
        String::new()
    };

    format!(
        "🪙 *{} \\({}\\)*   {}\n\
         💵 Price:  *{}*\n\n\
         📊 *Price Performance:*\n\
         `├ 1h ` : {}\n\
         `├ 24h` : {}\n\
         `├ 7d ` : {}\n\
         `└ 30d` : {}\n\n\
         💰 *Market Capitalization:*\n\
         `├ Vol (24h)` : {}\n\
         `└ Mkt Cap  ` : {}{}",
        esc_name, esc_sym, esc_rank, esc_price, c1h, c24h, c7d, c30d, vol, cap, supply_str
    )
}

// ── Global metrics card ───────────────────────────────────────────────────────

pub fn fmt_global_metrics(g: &GlobalMetrics) -> String {
    let mut out = format!(
        "🌐 *Global Market*\n\
         Total Cap: *{}*\n\
         Volume 24h: *{}*\n\
         BTC Dom: *{}%*   ETH Dom: *{}%*\n\
         Active coins: *{}*",
        esc(&fmt_large_usd(g.total_market_cap)),
        esc(&fmt_large_usd(g.total_volume_24h)),
        esc(&format!("{:.1}", g.btc_dominance)),
        esc(&format!("{:.1}", g.eth_dominance)),
        esc(&g.active_cryptocurrencies.to_string()),
    );

    if let Some(s_cap) = g.stablecoin_mcap {
        out.push_str(&format!(
            "\nStablecoin Cap: *{}*",
            esc(&fmt_large_usd(s_cap))
        ));
    }
    if let Some(d_vol) = g.dex_volume_24h {
        out.push_str(&format!("\nDEX Vol 24h: *{}*", esc(&fmt_large_usd(d_vol))));
    }

    out
}

// ── Top list row ──────────────────────────────────────────────────────────────

pub fn fmt_top_row(rank: usize, q: &CmcQuote) -> String {
    format!(
        "*\\#{} {}* — {}\n  24h: {} │ Vol: {}",
        rank,
        esc(&q.symbol),
        esc(&fmt_price(q.price_usd)),
        esc(&fmt_change_plain(q.change_24h)),
        esc(&fmt_large_usd(q.volume_24h)),
    )
}

// ── Symbol emoji ──────────────────────────────────────────────────────────────

pub fn symbol_emoji(sym: &str) -> &'static str {
    match sym.to_uppercase().as_str() {
        "BTC" => "₿",
        "ETH" => "Ξ",
        "SOL" => "◎",
        "BNB" => "🔶",
        "XRP" => "✕",
        "USDT" | "USDC" | "DAI" | "BUSD" => "💲",
        "LINK" => "🔗",
        "DOT" => "●",
        "ADA" => "🔵",
        "AVAX" => "🔺",
        "MATIC" | "POL" => "💜",
        "DOGE" => "🐶",
        "SHIB" => "🐕",
        _ => "🪙",
    }
}

// ── Period label ──────────────────────────────────────────────────────────────

pub fn period_label(p: &Period) -> &'static str {
    match p {
        Period::OneHour => "1h",
        Period::FourHours => "4h",
        Period::OneDay => "24h",
        Period::SevenDays => "7d",
        Period::ThirtyDays => "30d",
        Period::NinetyDays => "90d",
    }
}

// ── Transfer summary ──────────────────────────────────────────────────────────

pub fn fmt_fee(fee: f64, symbol: &str, fee_usd: Option<f64>) -> String {
    let usd_part = fee_usd
        .map(|u| format!(" (≈ {})", fmt_price(u)))
        .unwrap_or_default();
    format!("{} {}{}", fmt_crypto(fee, symbol), symbol, usd_part)
}

pub fn format_fng(fng: &FearAndGreed) -> String {
    let val_num = fng.value.parse::<u32>().unwrap_or(0);
    let emoji = match val_num {
        0..=24 => "🔴",
        25..=45 => "🟠",
        46..=54 => "🟡",
        55..=74 => "🟢",
        _ => "🤑",
    };

    format!(
        "<b>Fear & Greed Index</b>\n\n\
        {} <b>Index:</b> {} / 100\n\
        🏷 <b>Sentiment:</b> {}\n",
        emoji, fng.value, fng.value_classification
    )
}

#[cfg(test)]
mod tests {
    use super::{redact_secrets, truncate_chars};

    #[test]
    fn truncate_chars_is_char_safe() {
        // Cyrillic + emoji: byte 800 would split a char with the old `&s[..800]`.
        let s = "привет ".repeat(200); // 1400 chars, multi-byte UTF-8
        let t = truncate_chars(&s, 800);
        assert!(t.ends_with("..."));
        assert!(t.chars().count() <= 803);
        // Short strings pass through untouched.
        assert_eq!(truncate_chars("коротко", 800), "коротко");
        // Exact-length strings are not marked truncated.
        assert_eq!(truncate_chars("abcdef", 6), "abcdef");
    }

    #[test]
    fn redacts_google_api_key_in_url() {
        // Exact leak observed in the chat export (2026-06-28): the reqwest
        // timeout error embedded the full URL with the Gemini key.
        let err = "error sending request for url (https://generativelanguage.googleapis.com/v1beta/models/gemini-3.5-flash:generateContent?key=AIzaSy012345678901234567890123456789012): operation timed out";
        let redacted = redact_secrets(err);
        assert!(!redacted.contains("AIzaSy012345678901234567890123456789012"));
        assert!(redacted.contains("key=[REDACTED]"));
        assert!(redacted.contains("operation timed out"));
        // The URL structure is preserved so the error stays diagnosable.
        assert!(redacted.contains("generateContent"));
    }

    #[test]
    fn redacts_query_param_secrets() {
        let err = "HTTP 401 for https://api.example.com/v1?api_key=sk-1234567890abcdef&x=1";
        let redacted = redact_secrets(err);
        assert!(!redacted.contains("sk-1234567890abcdef"));
        assert!(redacted.contains("api_key=[REDACTED]"));
        assert!(redacted.contains("&x=1"));
    }

    #[test]
    fn redacts_bearer_header() {
        let err = "request failed: Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.abcdef.xyz";
        let redacted = redact_secrets(err);
        assert!(!redacted.contains("eyJhbGciOiJIUzI1NiJ9"));
        assert!(redacted.contains("Bearer [REDACTED]"));
    }

    #[test]
    fn redacts_standalone_google_key() {
        // A bare AIza... key with no query-param marker must also be redacted.
        let err = "Gemini call failed with key AIzaSy012345678901234567890123456789012";
        let redacted = redact_secrets(err);
        assert!(!redacted.contains("AIzaSy012345678901234567890123456789012"));
        assert!(redacted.contains("AIza[REDACTED]"));
    }

    #[test]
    fn leaves_plain_errors_untouched() {
        let err = "Failed to generate any successful drafts";
        assert_eq!(redact_secrets(err), err);
    }
}
