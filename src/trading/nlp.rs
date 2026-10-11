//! Free-text intent parser for the commander/advice mode.
//!
//! The operator talks in plain language ("ок я купил ETH за 1800") instead of
//! structured subcommands. This module extracts the trade intent (side,
//! symbol, price, optional amount) plus a verbatim note, so the CLI and the
//! Telegram layer can route the report through the engine's guardrails exactly
//! like a structured command. No external dependencies — pure string logic.

use super::store::{RuleType, Side};

/// Result of parsing a free-text trade report.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedTradeReport {
    pub side: Side,
    pub symbol: String,
    pub price: f64,
    /// Optional amount in coin units ("купил 0.5 ETH за 1800").
    pub amount: Option<f64>,
    /// The operator's original words — kept verbatim as the audit note.
    pub note: String,
}

/// A natural-language operator constraint ("пока биток выше 65к не покупай").
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedConstraint {
    pub symbol: String,
    pub rule_type: RuleType,
    pub threshold_value: f64,
    pub note: String,
}

/// (alias, canonical ticker) — matched as whole words, case-insensitive.
const SYMBOL_ALIASES: &[(&str, &str)] = &[
    ("btc", "BTC"),
    ("bitcoin", "BTC"),
    ("биткоин", "BTC"),
    ("биток", "BTC"),
    ("битка", "BTC"),
    ("eth", "ETH"),
    ("ethereum", "ETH"),
    ("эфир", "ETH"),
    ("эфириум", "ETH"),
    ("sol", "SOL"),
    ("solana", "SOL"),
    ("link", "LINK"),
    ("chainlink", "LINK"),
];

/// Words that introduce the fill price ("купил ETH за 1800", "sold at 1900").
/// Longest-first ordering matters: "по цене" must win over "по".
const PRICE_MARKERS: &[&str] = &[
    "по цене",
    "получилось",
    "получил",
    "получила",
    "вышло",
    "за",
    "по",
    "at",
    "price",
    "@",
];

/// Parse a natural-language constraint. Understands the common Russian/English
/// phrasings:
///
/// * "пока биток выше 65к не покупай" → BTC ceiling 65 000 (buy only below)
/// * "не продавай SOL дешевле 50" → SOL floor 50 (sell only above)
/// * "buy ETH only if below 2000" → ETH ceiling 2 000
/// * "покупай если биток выше 40к" → BTC floor 40 000 (buy only above)
///
/// Returns `None` when the intent is unclear (no symbol, no number, no
/// buy/sell verb). Numbers support k/к/тыс and m/млн suffixes ("65к" → 65000).
pub fn parse_constraint(text: &str) -> Option<ParsedConstraint> {
    let raw = text.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_lowercase();

    let (_alias, symbol) = SYMBOL_ALIASES
        .iter()
        .find(|(alias, _)| contains_word(&lower, alias))?;

    let value = extract_threshold(&lower)?;
    let buy = contains_stem(&lower, &["купи", "покупай", "покупать", "купить", "bought", "buy"]);
    let sell = contains_stem(&lower, &["прода", "продавай", "продавать", "продать", "sold", "sell"]);
    if !buy && !sell {
        return None;
    }
    let above = contains_stem(&lower, &["выше", "больше", "дороже", "above", "over", "higher"]);
    let below = contains_stem(&lower, &["ниже", "меньше", "дешевле", "below", "under", "lower"]);
    if above == below {
        return None; // no clear direction
    }

    // "пока X выше P не покупай" / "buy only if below P" -> MaxPriceCeiling
    // "не продавай ниже P" / "buy only if above P" -> MinPriceFloor
    // We detect if user phrased as "don't buy above" or "buy only below" (both Ceiling)
    // vs "don't sell below" or "sell only above" or "buy only above" (Floor).
    let not_prefix = lower.contains("не ") || lower.contains("dont") || lower.contains("don't");
    let rule_type = if buy {
        if (above && not_prefix) || (below && !not_prefix) {
            RuleType::MaxPriceCeiling
        } else {
            RuleType::MinPriceFloor
        }
    } else {
        // sell
        if (below && not_prefix) || (above && !not_prefix) {
            RuleType::MinPriceFloor
        } else {
            RuleType::MaxPriceCeiling
        }
    };

    Some(ParsedConstraint {
        symbol: symbol.to_string(),
        rule_type,
        threshold_value: value,
        note: raw.to_string(),
    })
}

/// Extract a threshold number with k/к/тыс/m/млн suffixes ("65к" → 65000).
fn extract_threshold(lower: &str) -> Option<f64> {
    let mut i = 0;
    while i < lower.len() {
        let ch = lower[i..].chars().next().unwrap();
        if ch.is_ascii_digit() {
            let run: String = lower[i..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
                .collect();
            let base = parse_number_run(&run)?;
            let after = &lower[i + run.len()..];
            let suffix = after
                .chars()
                .take_while(|c| c.is_alphabetic())
                .collect::<String>();
            let mult = match suffix.as_str() {
                "к" | "k" | "тыс" | "тыс." | "k." => 1_000.0,
                "м" | "m" | "млн" | "млн." => 1_000_000.0,
                _ => 1.0,
            };
            return Some(base * mult);
        }
        i += ch.len_utf8();
    }
    None
}

/// Parse a free-text trade report. Returns `None` when the intent is unclear
/// (no buy/sell verb, no known symbol, no price).
pub fn parse_trade_report(text: &str) -> Option<ParsedTradeReport> {
    let raw = text.trim();
    if raw.is_empty() {
        return None;
    }
    let lower = raw.to_lowercase();

    let side = if contains_any(&lower, &["продал", "продала", "продать", "sold", "sell"])
    {
        Side::Sell
    } else if contains_any(&lower, &["купил", "купила", "купить", "bought", "buy"])
    {
        Side::Buy
    } else {
        return None;
    };

    let (alias, symbol) = SYMBOL_ALIASES
        .iter()
        .find(|(alias, _)| contains_word(&lower, alias))?;

    let price = extract_price(&lower)?;
    // If the symbol-adjacent number is the same as the price, it was the price
    // all along ("купил ETH 1800") — don't treat it as an amount.
    let amount = extract_amount(&lower, alias).filter(|a| (a - price).abs() > 1e-9);

    Some(ParsedTradeReport {
        side,
        symbol: symbol.to_string(),
        price,
        amount,
        note: raw.to_string(),
    })
}

fn contains_stem(haystack: &str, stems: &[&str]) -> bool {
    stems.iter().any(|s| haystack.contains(s))
}

fn contains_any(haystack: &str, words: &[&str]) -> bool {
    words.iter().any(|w| contains_word(haystack, w))
}

/// True when `word` appears in `haystack` as a standalone token (not as a
/// substring of a longer word — "sol" must not match "solution").
fn contains_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !c.is_alphanumeric())
        .any(|tok| tok == word)
}

/// First standalone occurrence of `word` in `text`, searching from `from`.
fn find_word(text: &str, word: &str, from: usize) -> Option<usize> {
    let mut search_from = from;
    while let Some(rel) = text[search_from..].find(word) {
        let abs = search_from + rel;
        let before_ok = abs == 0
            || !text[..abs]
                .chars()
                .next_back()
                .map(|c| c.is_alphanumeric())
                .unwrap_or(false);
        let after_ok = text[abs + word.len()..]
            .chars()
            .next()
            .map(|c| !c.is_alphanumeric())
            .unwrap_or(true);
        if before_ok && after_ok {
            return Some(abs);
        }
        search_from = abs + word.len();
    }
    None
}

/// Extract the fill price: prefer a number right after a price marker
/// ("за 1800", "at 1900", "получилось 1850"), else the last number in text.
fn extract_price(lower: &str) -> Option<f64> {
    let mut last: Option<f64> = None;
    for marker in PRICE_MARKERS {
        let mut from = 0;
        while let Some(idx) = find_word(lower, marker, from) {
            let after = idx + marker.len();
            if let Some(n) = number_after(lower, after) {
                last = Some(n);
            }
            from = after;
        }
    }
    if last.is_some() {
        return last;
    }
    last_number(lower)
}

/// Extract an optional amount: a number directly adjacent to the symbol word
/// ("0.5 ETH" or "ETH 0.5").
fn extract_amount(lower: &str, alias: &str) -> Option<f64> {
    let alias_idx = find_word(lower, alias, 0)?;
    let before = lower[..alias_idx].trim_end();
    if let Some(n) = trailing_number(before) {
        return Some(n);
    }
    let after = lower[alias_idx + alias.len()..].trim_start();
    leading_number(after)
}

/// Parse the first number found in `text` starting at `start` (skipping
/// non-digit characters).
fn number_after(text: &str, start: usize) -> Option<f64> {
    let rest = &text[start..];
    let digits_start = rest.find(|c: char| c.is_ascii_digit())?;
    let run: String = rest[digits_start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.' || c.is_whitespace())
        .collect();
    parse_number_run(&run)
}

/// The last number in `text`.
fn last_number(text: &str) -> Option<f64> {
    let mut last = None;
    let mut i = 0;
    while i < text.len() {
        let ch = text[i..].chars().next().unwrap();
        if ch.is_ascii_digit() {
            let run: String = text[i..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.' || c.is_whitespace())
                .collect();
            if let Some(n) = parse_number_run(&run) {
                last = Some(n);
            }
            i += run.len();
        } else {
            i += ch.len_utf8();
        }
    }
    last
}

/// A number at the very end of `text` ("купил 0.5 ETH" → "0.5").
fn trailing_number(text: &str) -> Option<f64> {
    let trimmed = text.trim_end();
    let last_digit = trimmed.rfind(|c: char| c.is_ascii_digit())?;
    // Expand backwards over digits, separators and spaces to the number start.
    let mut start = last_digit;
    while start > 0 {
        let prev = trimmed[..start].chars().next_back().unwrap();
        if prev.is_ascii_digit() || prev == ',' || prev == '.' || prev.is_whitespace() {
            start -= prev.len_utf8();
        } else {
            break;
        }
    }
    let run: String = trimmed[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.' || c.is_whitespace())
        .collect();
    parse_number_run(&run)
}

/// A number at the very start of `text` ("ETH 0.5 за 1800" → "0.5").
fn leading_number(text: &str) -> Option<f64> {
    let trimmed = text.trim_start();
    if !trimmed
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        return None;
    }
    let run: String = trimmed
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == ',' || *c == '.' || c.is_whitespace())
        .collect();
    parse_number_run(&run)
}

/// Normalize a raw digit run ("1 800", "1,800", "1800,5") into an f64.
///
/// Separator heuristic: when both ',' and '.' are present the last one is the
/// decimal separator; when only one is present, three digits after it mean it
/// is a thousands separator ("1,800" → 1800), otherwise it is a decimal
/// separator ("1800,5" → 1800.5).
fn parse_number_run(run: &str) -> Option<f64> {
    let cleaned: String = run
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == ',' || *c == '.')
        .collect();
    let cleaned = cleaned.trim_end_matches(|c| c == ',' || c == '.');
    if cleaned.is_empty() {
        return None;
    }
    let mut cleaned = cleaned.to_string();
    let has_comma = cleaned.contains(',');
    let has_dot = cleaned.contains('.');
    if has_comma && has_dot {
        // The later separator is the decimal one.
        if cleaned.rfind(',').unwrap() > cleaned.rfind('.').unwrap() {
            cleaned = cleaned.replace('.', "").replace(',', ".");
        } else {
            cleaned = cleaned.replace(',', "");
        }
    } else if has_comma {
        let after = cleaned.split(',').nth(1).unwrap_or("");
        if after.len() == 3 {
            cleaned = cleaned.replace(',', "");
        } else {
            cleaned = cleaned.replace(',', ".");
        }
    } else if has_dot {
        let after = cleaned.split('.').nth(1).unwrap_or("");
        if after.len() == 3 {
            cleaned = cleaned.replace('.', "");
        }
    }
    cleaned.parse::<f64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_russian_buy_report() {
        let p = parse_trade_report("ок я купил ETH за 1800").unwrap();
        assert_eq!(p.side, Side::Buy);
        assert_eq!(p.symbol, "ETH");
        assert_eq!(p.price, 1800.0);
        assert_eq!(p.amount, None);
    }

    #[test]
    fn parses_buy_with_amount_before_symbol() {
        let p = parse_trade_report("купил 0.5 ETH за 1800").unwrap();
        assert_eq!(p.side, Side::Buy);
        assert_eq!(p.symbol, "ETH");
        assert_eq!(p.price, 1800.0);
        assert_eq!(p.amount, Some(0.5));
    }

    #[test]
    fn parses_buy_with_amount_after_symbol() {
        let p = parse_trade_report("купил ETH 0.5 за 1800").unwrap();
        assert_eq!(p.symbol, "ETH");
        assert_eq!(p.price, 1800.0);
        assert_eq!(p.amount, Some(0.5));
    }

    #[test]
    fn parses_sell_report_english() {
        let p = parse_trade_report("sold 0.1 LINK at 10.5").unwrap();
        assert_eq!(p.side, Side::Sell);
        assert_eq!(p.symbol, "LINK");
        assert_eq!(p.price, 10.5);
        assert_eq!(p.amount, Some(0.1));
    }

    #[test]
    fn parses_ru_aliases_and_thousands_separator() {
        let p = parse_trade_report("продал биток за 65 000").unwrap();
        assert_eq!(p.side, Side::Sell);
        assert_eq!(p.symbol, "BTC");
        assert_eq!(p.price, 65000.0);

        let p = parse_trade_report("купил эфир за 1,800").unwrap();
        assert_eq!(p.symbol, "ETH");
        assert_eq!(p.price, 1800.0);
    }

    #[test]
    fn parses_got_result_phrasing() {
        let p = parse_trade_report("продал SOL, получилось 85.5").unwrap();
        assert_eq!(p.side, Side::Sell);
        assert_eq!(p.symbol, "SOL");
        assert_eq!(p.price, 85.5);
    }

    #[test]
    fn symbol_only_price_is_not_amount() {
        // "купил ETH 1800" — the only number is the price, not an amount.
        let p = parse_trade_report("купил ETH 1800").unwrap();
        assert_eq!(p.price, 1800.0);
        assert_eq!(p.amount, None);
    }

    #[test]
    fn rejects_unclear_intent() {
        assert!(parse_trade_report("").is_none());
        assert!(parse_trade_report("как дела?").is_none());
        assert!(parse_trade_report("купил что-то за 100").is_none());
        assert!(parse_trade_report("купил ETH").is_none());
    }

    #[test]
    fn sol_does_not_match_solana_or_solution() {
        let p = parse_trade_report("купил solana за 75").unwrap();
        assert_eq!(p.symbol, "SOL");
        assert!(parse_trade_report("купил solution за 75").is_none());
    }

    #[test]
    fn parses_ceiling_constraint_ru() {
        let c = parse_constraint("пока биток выше 65к не покупай").unwrap();
        assert_eq!(c.symbol, "BTC");
        assert_eq!(c.rule_type, RuleType::MaxPriceCeiling);
        assert_eq!(c.threshold_value, 65000.0);
    }

    #[test]
    fn parses_floor_constraint_ru() {
        let c = parse_constraint("не продавай SOL дешевле 50").unwrap();
        assert_eq!(c.symbol, "SOL");
        assert_eq!(c.rule_type, RuleType::MinPriceFloor);
        assert_eq!(c.threshold_value, 50.0);
    }

    #[test]
    fn parses_english_constraint_with_million() {
        let c = parse_constraint("buy ETH only if below 2000").unwrap();
        assert_eq!(c.symbol, "ETH");
        assert_eq!(c.rule_type, RuleType::MaxPriceCeiling);
        assert_eq!(c.threshold_value, 2000.0);
        let c = parse_constraint("buy BTC only if above 1.5m").unwrap();
        assert_eq!(c.rule_type, RuleType::MinPriceFloor);
        assert_eq!(c.threshold_value, 1_500_000.0);
    }

    #[test]
    fn rejects_unclear_constraints() {
        assert!(parse_constraint("").is_none());
        assert!(parse_constraint("как дела?").is_none());
        assert!(parse_constraint("биток выше 65к").is_none()); // no verb
        assert!(parse_constraint("не покупай биток").is_none()); // no number
    }
}
