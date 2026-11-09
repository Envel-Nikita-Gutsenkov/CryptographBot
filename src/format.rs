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

/// Keep the head and the tail of `s`, dropping the middle. Used when a prompt
/// block has instructions at the start and machine-readable ids/rules at the
/// end — plain truncation would silently drop the ids.
pub fn truncate_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    const MARKER: &str = "\n… (middle trimmed to fit the provider window) …\n";
    let marker_len = MARKER.chars().count();
    if max <= marker_len + 20 {
        return truncate_chars(s, max);
    }
    let keep = max - marker_len;
    let head = keep * 3 / 5;
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push_str(MARKER);
    out.extend(chars[chars.len() - tail..].iter());
    out
}

// ── Cache normalization ──

/// Compact, formatting-free version of an AI-generated summary for cache storage.
/// Cached summaries (`news_summaries`, `news_summary_history`, `macro_cache`)
/// are only ever re-read as AI prompt context — never rendered — so markdown
/// scaffolding (`###`, `**`, backticks) and emoji are pure token and storage
/// overhead. Bullet markers and list whitespaces are preserved.
pub fn plain_for_cache(s: &str) -> String {
    // Strip inline markers first, then per-line scaffolding.
    let de_markdown = s
        .replace("**", "")
        .replace(&format!("{}", '`'), "")
        .replace('*', "");

    let mut out = String::with_capacity(s.len());
    for raw_line in de_markdown.lines() {
        let line = raw_line.trim().trim_start_matches('#').trim();
        if line.is_empty() {
            out.push('\n');
            continue;
        }
        for c in line.chars() {
            let v = c as u32;
            let pictograph = (0x1F000..=0x1FAFF).contains(&v) // emoji main
                || (0x2600..=0x27BF).contains(&v) // misc symbols & dingbats
                || (0x2B00..=0x2BFF).contains(&v) // arrows/stars blocks
                || (0x1F1E6..=0x1F1FF).contains(&v) // flags
                || v == 0xFE0F // variation selector-16
                || v == 0x200D // zero-width joiner
                || v == 0x20E3 // combining enclosing keycap
                || v == 0xFFFD; // replacement char
            if pictograph {
                continue;
            }
            out.push(c);
        }
        out.push('\n');
    }

    // Collapse runs of blank lines to a single one.
    let mut collapsed = String::with_capacity(out.len());
    let mut last_blank = false;
    for line in out.lines() {
        let blank = line.is_empty();
        if blank && last_blank {
            continue;
        }
        collapsed.push_str(line);
        collapsed.push('\n');
        last_blank = blank;
    }
    collapsed.trim().to_string()
}

// ── Price formatting ─────────────────────────────────────────────────────────

/// Decimal places that keep a price readable across nine orders of magnitude.
///
/// Cents are enough for BTC/ETH, but a sub-cent token (PEPE, SHIB, BONK) that
/// shares the `$0.00000` cell with every other row is useless — the forecast,
/// dip ladder and sniper zones all showed an unreadable zero. Scale the
/// precision with the magnitude below $0.01 so the significant digits survive.
pub fn price_decimals(p: f64) -> usize {
    let a = p.abs();
    if !a.is_finite() || a <= 0.0 {
        return 2;
    }
    if a >= 1000.0 {
        0
    } else if a >= 10.0 {
        2
    } else if a >= 1.0 {
        3
    } else if a >= 0.01 {
        5
    } else if a >= 0.001 {
        6
    } else if a >= 0.0001 {
        7
    } else if a >= 0.00001 {
        8
    } else {
        9
    }
}

pub fn format_thousands(n: f64, decimals: usize) -> String {
    // Round BEFORE formatting so a value like `-0.001` does not become the ugly
    // `-0.00`, then normalise `-0.0` to `0.0` so we never print `$-0.00`.
    let factor = 10f64.powi(decimals as i32);
    let scaled = n * factor;
    let rounded = if scaled.is_finite() {
        scaled.round() / factor
    } else {
        n
    };
    let n = if rounded == 0.0 { 0.0 } else { rounded };
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

/// Signed percentage with `decimals` places, never emitting a negative zero
/// such as `-0.0%` for a value that is, to the displayed precision, exactly
/// zero. Tiny negatives collapse to `0.0%`/`0.00%` without a sign.
pub fn fmt_pct_signed(pct: f64, decimals: usize) -> String {
    let scale = 10f64.powi(decimals as i32);
    let rounded = if pct.is_finite() {
        (pct * scale).round() / scale
    } else {
        0.0
    };
    if rounded == 0.0 {
        format!("{:.*}%", decimals, 0.0)
    } else {
        format!("{:+.*}%", decimals, rounded)
    }
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
    if !pct.is_finite() {
        return "N/A".to_string();
    }
    let rounded = (pct * 100.0).round() / 100.0;
    // `-0.0` is `>= 0.0` yet formats as `-0.00`, which would yield `+-0.00%`.
    if rounded == 0.0 {
        "📈 +0.00%".to_string()
    } else if rounded > 0.0 {
        format!("📈 +{:.2}%", rounded)
    } else {
        format!("📉 {:.2}%", rounded)
    }
}

pub fn fmt_change_plain(pct_opt: Option<f64>) -> String {
    match pct_opt {
        Some(pct) if pct.is_finite() => {
            let rounded = (pct * 100.0).round() / 100.0;
            // Normalise `-0.0` (which is `>= 0.0` yet formats as `-0.00`) so a
            // tiny negative never renders as the nonsensical `+-0.00%`.
            if rounded == 0.0 {
                "+0.00%".to_string()
            } else if rounded > 0.0 {
                format!("+{:.2}%", rounded)
            } else {
                format!("{:.2}%", rounded)
            }
        }
        _ => "N/A".to_string(),
    }
}

/// Format a signed USD amount, placing the minus sign *before* the dollar
/// sign (`-$42.30`) instead of the ugly `$-42.30` that `format!("${:.2}")`
/// produces for negative values. Rounds to 2 decimals and never emits `-0.00`.
pub fn fmt_usd_signed(n: f64) -> String {
    let factor = 100.0;
    let rounded = if n.is_finite() {
        (n * factor).round() / factor
    } else {
        0.0
    };
    if rounded < 0.0 {
        format!("-${}", format_thousands(rounded.abs(), 2))
    } else {
        format!("${}", format_thousands(rounded, 2))
    }
}

/// Format large USD amounts: $1.23B, $456.7M, $12.3K, -$1.2K (sign before `$`).
pub fn fmt_large_usd(n: f64) -> String {
    let (sign, a) = if n < 0.0 { ("-", n.abs()) } else { ("", n) };
    if a >= 1_000_000_000.0 {
        format!("{sign}${:.2}B", a / 1_000_000_000.0)
    } else if a >= 1_000_000.0 {
        format!("{sign}${:.2}M", a / 1_000_000.0)
    } else if a >= 1_000.0 {
        format!("{sign}${:.1}K", a / 1_000.0)
    } else {
        format!("{sign}${:.2}", a)
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

/// Escape the three characters that matter to Telegram's HTML parser.
fn push_html_escaped(input: &str, out: &mut String) {
    for c in input.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
}

/// Guard against Telegram turning a percentage unit into a clickable command.
///
/// `%/day` and `%/8h` were rendered as the commands `/day` and `/8h` in the
/// audit logs. Inserting a zero-width space after the slash keeps the visual
/// text identical while making the slash unable to start a command.
pub fn guard_slash_units(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + 8);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        out.push(c);
        if c == '%'
            && i + 2 < chars.len()
            && chars[i + 1] == '/'
            && chars[i + 2].is_ascii_alphanumeric()
        {
            out.push('/');
            out.push('\u{200B}');
            i += 2;
            continue;
        }
        i += 1;
    }
    out
}

/// True when the neighbouring char makes an underscore part of a word
/// (so `price_7d_ago` is never mistaken for italic emphasis).
fn is_word_char(c: Option<char>) -> bool {
    matches!(c, Some(ch) if ch.is_alphanumeric())
}

/// Convert a single line of (LLM-produced) GFM markdown into Telegram HTML.
///
/// Emphasis is handled run-based: a `***`/`**` open is matched to a
/// `***`/`**` close and the whole closing run is consumed, so `**x***` never
/// leaves a stray `*`. An unmatched bold run is dropped rather than shown as
/// literal asterisks. Underscore emphasis also requires a word boundary, which
/// keeps identifiers like `price_7d_ago` intact.
fn inline_markdown_to_html(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::with_capacity(line.len() + 16);
    let mut i = 0;
    let n = chars.len();
    while i < n {
        let c = chars[i];
        if c == '`' {
            // Inline code span.
            if let Some(rel) = chars[i + 1..].iter().position(|&x| x == '`') {
                let inner: String = chars[i + 1..i + 1 + rel].iter().collect();
                out.push_str("<code>");
                push_html_escaped(&inner, &mut out);
                out.push_str("</code>");
                i += rel + 2;
                continue;
            }
            out.push('`');
            i += 1;
            continue;
        }
        if c == '*' || c == '_' {
            // Measure the opening run of the delimiter.
            let mut open_end = i;
            while open_end < n && chars[open_end] == c {
                open_end += 1;
            }
            let open_run = open_end - i;
            let prev = if i > 0 { Some(chars[i - 1]) } else { None };
            // Underscores only open at a word boundary.
            let opens = c == '*' || !is_word_char(prev);
            if !opens {
                for _ in 0..open_run {
                    out.push(c);
                }
                i = open_end;
                continue;
            }

            // Find a matching closing run.
            let mut close: Option<(usize, usize)> = None; // (start, end)
            let mut j = open_end;
            while j < n {
                if chars[j] == c {
                    let mut run_end = j;
                    while run_end < n && chars[run_end] == c {
                        run_end += 1;
                    }
                    let run = run_end - j;
                    let next = if run_end < n { Some(chars[run_end]) } else { None };
                    let closes = c == '*' || !is_word_char(next);
                    // A lone `*` must close on another lone `*`; the first `*`
                    // of a `**` run belongs to bold.
                    let run_matches = if open_run == 1 { run == 1 } else { run >= 2 };
                    if closes && run_matches {
                        close = Some((j, run_end));
                        break;
                    }
                    j = run_end;
                    continue;
                }
                j += 1;
            }

            match close {
                Some((close_start, close_end)) if close_start > open_end => {
                    let inner: String = chars[open_end..close_start].iter().collect();
                    let inner_html = inline_markdown_to_html(&inner);
                    if open_run >= 3 {
                        out.push_str("<b><i>");
                        out.push_str(&inner_html);
                        out.push_str("</i></b>");
                    } else if open_run == 2 {
                        out.push_str("<b>");
                        out.push_str(&inner_html);
                        out.push_str("</b>");
                    } else {
                        out.push_str("<i>");
                        out.push_str(&inner_html);
                        out.push_str("</i>");
                    }
                    i = close_end;
                    continue;
                }
                _ => {
                    // No closing run: drop a stray bold run (`**`/`***`) so it
                    // never renders literally; keep single `*`/`_` as text.
                    if open_run >= 2 && c == '*' {
                        i = open_end;
                        continue;
                    }
                    for _ in 0..open_run {
                        out.push(c);
                    }
                    i = open_end;
                    continue;
                }
            }
        }
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
        i += 1;
    }
    out
}

/// Convert LLM GFM markdown to a single Telegram-HTML message.
///
/// The bot used to send model answers raw (or fall back to plain text), so
/// `**WAIT.**` and `_Gen: ..._` leaked as literal asterisks/underscores, and
/// tables lost their alignment. This converter escapes HTML entities, turns
/// code fences into `<pre>`, headings/bold into `<b>`, italics into `<i>`, and
/// renders pipe tables as aligned `<pre>` blocks. Units are passed through
/// [`guard_slash_units`] so `%/day` is never linked as a command.
pub fn markdown_to_html(md: &str) -> String {
    let mut out = String::with_capacity(md.len() + 32);
    let mut in_code = false;
    let mut table: Vec<String> = Vec::new();

    let flush_table = |table: &mut Vec<String>, out: &mut String| {
        if table.is_empty() {
            return;
        }
        out.push_str("<pre>");
        for line in table.iter() {
            push_html_escaped(line, out);
            out.push('\n');
        }
        out.push_str("</pre>\n");
        table.clear();
    };

    for line in md.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            flush_table(&mut table, &mut out);
            if in_code {
                out.push_str("</pre>\n");
                in_code = false;
            } else {
                out.push_str("<pre>");
                in_code = true;
            }
            continue;
        }
        if in_code {
            push_html_escaped(line, &mut out);
            out.push('\n');
            continue;
        }

        let is_table_row = trimmed.contains('|');
        let is_separator =
            trimmed.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) && trimmed.contains('-');
        if is_table_row {
            if !is_separator {
                table.push(line.to_string());
            }
            continue;
        }
        flush_table(&mut table, &mut out);

        if let Some(rest) = trimmed.strip_prefix('#') {
            let title = rest.trim_start_matches('#').trim();
            if !title.is_empty() {
                out.push_str("<b>");
                push_html_escaped(title, &mut out);
                out.push_str("</b>\n");
                continue;
            }
        }

        if let Some(rest) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
        {
            out.push_str("• ");
            out.push_str(&inline_markdown_to_html(rest));
            out.push('\n');
            continue;
        }

        out.push_str(&inline_markdown_to_html(line));
        out.push('\n');
    }
    flush_table(&mut table, &mut out);
    if in_code {
        out.push_str("</pre>");
    }
    guard_slash_units(out.trim_end())
}

/// Normalise model output for a `ParseMode::Html` message.
///
/// Some prompts ask for bare `<b>` HTML, others for plain text, and the model
/// mixes both — `**WAIT.**` next to real `<b>` tags. If the text already
/// contains HTML tags we keep those tags, but still run the markdown→HTML
/// converter on everything around them, so stray `**`/`_` never leaks. If there
/// are no tags we convert the whole string with [`markdown_to_html`].
pub fn model_output_to_html(s: &str) -> String {
    const HTML_TAGS: [&str; 14] = [
        "<b>", "<i>", "<u>", "<s>", "<code>", "<pre>", "<a ", "<br", "<strong>", "<em>",
        "<del>", "<blockquote>", "<span ", "<tg-spoiler>",
    ];
    if HTML_TAGS.iter().any(|t| s.contains(t)) {
        guard_slash_units(&mix_markdown_with_tags(s))
    } else {
        markdown_to_html(s)
    }
}

/// Recognise a Telegram-safe HTML tag starting at `chars[i]`, returning its
/// length in chars. Anything else is treated as plain text (and escaped).
fn match_html_tag(chars: &[char], i: usize) -> Option<usize> {
    let rest: String = chars[i..].iter().take(256).collect();
    let lower = rest.to_ascii_lowercase();
    const SIMPLE: [&str; 27] = [
        "</b>", "<b>", "</i>", "<i>", "</code>", "<code>", "</pre>", "<pre>", "</u>", "<u>",
        "</a>", "</s>", "<s>", "</strong>", "<strong>", "</em>", "<em>", "</del>", "<del>",
        "</ins>", "<ins>", "</blockquote>", "<blockquote>", "</tg-spoiler>", "<tg-spoiler>",
        "<br>", "</span>",
    ];
    for t in SIMPLE {
        if lower.starts_with(t) {
            return Some(t.chars().count());
        }
    }
    for t in ["<br/>", "<br />"] {
        if lower.starts_with(t) {
            return Some(t.chars().count());
        }
    }
    if lower.starts_with("<a ") || lower.starts_with("<span ") {
        if let Some(pos) = lower.find('>') {
            return Some(rest[..=pos].chars().count());
        }
    }
    None
}

/// Run the inline markdown converter across a string that already contains
/// HTML tags, leaving those tags untouched.
///
/// Tags are swapped for private-use placeholders before conversion and restored
/// afterwards, so emphasis can span a tag boundary without leaking asterisks.
fn mix_markdown_with_tags(s: &str) -> String {
    const OPEN: char = '\u{0001}';
    const CLOSE: char = '\u{0002}';
    let chars: Vec<char> = s.chars().collect();
    let mut text = String::with_capacity(s.len() + 16);
    let mut tags: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some(len) = match_html_tag(&chars, i) {
                tags.push(chars[i..i + len].iter().collect());
                text.push(OPEN);
                text.push_str(&(tags.len() - 1).to_string());
                text.push(CLOSE);
                i += len;
                continue;
            }
        }
        text.push(chars[i]);
        i += 1;
    }
    let converted = inline_markdown_to_html(&text);
    let mut out = String::with_capacity(converted.len());
    let cc: Vec<char> = converted.chars().collect();
    let mut k = 0;
    while k < cc.len() {
        if cc[k] == OPEN {
            let mut num = String::new();
            let mut m = k + 1;
            while m < cc.len() && cc[m].is_ascii_digit() {
                num.push(cc[m]);
                m += 1;
            }
            if m < cc.len() && cc[m] == CLOSE {
                if let Ok(idx) = num.parse::<usize>() {
                    if let Some(t) = tags.get(idx) {
                        out.push_str(t);
                        k = m + 1;
                        continue;
                    }
                }
            }
        }
        out.push(cc[k]);
        k += 1;
    }
    out
}

fn change_emoji_val(pct_opt: Option<f64>, has_data: bool) -> String {
    if !has_data || pct_opt.is_none() {
        return "*N/A*".to_string();
    }
    let raw = pct_opt.unwrap();
    if !raw.is_finite() {
        return "*N/A*".to_string();
    }
    // Round to 2 decimals up front so a tiny negative like -0.001% becomes
    // exactly 0.0 and renders as `+0.00%`, never the ugly `-0.00%`.
    let rounded = (raw * 100.0).round() / 100.0;
    let pct = if rounded == 0.0 { 0.0 } else { rounded };
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
            "{} {}",
            esc(&fmt_large_usd(q.circulating_supply).replace('$', "")),
            esc_sym
        )
    } else {
        "N/A".to_string()
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
         `├ Mkt Cap  ` : {}\n\
         `└ Supply   ` : {}",
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
    use super::{
        change_emoji_val, fmt_change, fmt_change_plain, fmt_large_usd, fmt_pct_signed,
        fmt_usd_signed, format_thousands, guard_slash_units, markdown_to_html,
        model_output_to_html, plain_for_cache, price_decimals, redact_secrets, truncate_chars,
        truncate_middle,
    };

    #[test]
    fn price_decimals_keeps_sub_cent_tokens_readable() {
        assert_eq!(price_decimals(60_571.0), 0);
        assert_eq!(price_decimals(8.5591), 3);
        assert_eq!(price_decimals(0.123456), 5);
        assert_eq!(price_decimals(0.000004), 9);
        // A PEPE-like price must never collapse to `$0.00000`.
        let pepe = 0.0000042_f64;
        let rendered = format!("${}", format_thousands(pepe, price_decimals(pepe)));
        assert!(rendered != "$0.00000", "sub-cent price rendered as {rendered}");
        assert!(
            rendered.trim_start_matches('$').trim_matches('0').contains('4'),
            "significant digit lost: {rendered}"
        );
        // Degenerate inputs must not panic.
        assert_eq!(price_decimals(0.0), 2);
        assert_eq!(price_decimals(f64::NAN), 2);
        assert_eq!(price_decimals(-5.0), 3);
    }

    #[test]
    fn markdown_to_html_converts_alert_advice_without_leaks() {
        // Real shape from the Nov-2026 logs: bold emphasis plus a `_Gen:_`
        // footer that used to reach Telegram as literal asterisks/underscores.
        let md = "ETH dropped **29.05% in 30 days** to **$1,764.89**, now **0.8% below** \
                  your average.\n\n**WAIT** — no action needed.\n\n_Gen: gpt-oss-120b + ministral-14b_";
        let html = markdown_to_html(md);
        assert!(!html.contains("**"), "raw bold leaked: {html}");
        assert!(!html.contains("_Gen"), "raw italic leaked: {html}");
        assert!(html.contains("<b>29.05% in 30 days</b>"));
        assert!(html.contains("<i>Gen: gpt-oss-120b + ministral-14b</i>"));
    }

    #[test]
    fn fmt_change_and_plain_never_emit_double_sign() {
        // A tiny negative rounds to zero: must read `+0.00%`, never `+-0.00%`.
        for v in [-0.0001, -0.0, -1e-9, 0.0, 1e-9] {
            let a = fmt_change_plain(Some(v));
            let b = fmt_change(v);
            assert!(!a.contains("+-"), "plain double sign for {v}: {a}");
            assert!(!b.contains("+-"), "emoji double sign for {v}: {b}");
            assert!(!a.contains("-0.00"), "plain negative zero for {v}: {a}");
            assert!(!b.contains("-0.00"), "emoji negative zero for {v}: {b}");
        }
        assert_eq!(fmt_change_plain(Some(-0.0001)), "+0.00%");
        assert_eq!(fmt_change(-0.0001), "📈 +0.00%");
        assert_eq!(fmt_change_plain(Some(-1.5)), "-1.50%");
        assert_eq!(fmt_change(-1.5), "📉 -1.50%");
        assert_eq!(fmt_change_plain(Some(f64::NAN)), "N/A");
        assert_eq!(fmt_change(f64::INFINITY), "N/A");
    }

    #[test]
    fn fmt_pct_signed_collapses_negative_zero() {
        assert_eq!(fmt_pct_signed(-0.001, 1), "0.0%");
        assert_eq!(fmt_pct_signed(-0.0, 1), "0.0%");
        assert_eq!(fmt_pct_signed(0.0, 2), "0.00%");
        assert_eq!(fmt_pct_signed(-0.0001, 2), "0.00%");
        assert_eq!(fmt_pct_signed(-4.26, 1), "-4.3%");
        assert_eq!(fmt_pct_signed(4.26, 2), "+4.26%");
        assert_eq!(fmt_pct_signed(f64::NAN, 1), "0.0%");
    }

    #[test]
    fn format_thousands_never_prints_negative_zero() {
        assert_eq!(format_thousands(-0.0, 2), "0.00");
        assert_eq!(format_thousands(-0.001, 2), "0.00");
        assert_eq!(format_thousands(0.0, 2), "0.00");
        assert_eq!(format_thousands(-1234.5, 2), "-1,234.50");
    }

    #[test]
    fn fmt_usd_signed_puts_minus_before_dollar() {
        assert_eq!(fmt_usd_signed(-42.30), "-$42.30");
        assert_eq!(fmt_usd_signed(42.30), "$42.30");
        assert_eq!(fmt_usd_signed(-1234.5), "-$1,234.50");
        // Never emit the ugly `$-0.00`.
        assert_eq!(fmt_usd_signed(-0.001), "$0.00");
        assert_eq!(fmt_usd_signed(0.0), "$0.00");
        // NaN must not panic or print garbage.
        assert_eq!(fmt_usd_signed(f64::NAN), "$0.00");
    }

    #[test]
    fn change_emoji_val_never_prints_negative_zero() {
        // -0.001% used to render as `-0.00%`; it must become `+0.00%`.
        assert!(change_emoji_val(Some(-0.001), true).contains("+0.00%"));
        assert!(!change_emoji_val(Some(-0.001), true).contains("-0.00%"));
        assert!(change_emoji_val(Some(-1.5), true).contains("-1.50%"));
        assert!(change_emoji_val(Some(2.0), true).contains("+2.00%"));
        assert_eq!(change_emoji_val(None, true), "*N/A*");
    }

    #[test]
    fn fmt_large_usd_sign_before_dollar() {
        assert_eq!(fmt_large_usd(-1_500.0), "-$1.5K");
        assert_eq!(fmt_large_usd(1_500.0), "$1.5K");
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let s: String = (0..200).map(|i| format!("{i}.")).collect();
        let body = format!("HEAD{}{}TAIL", s, s);
        let t = truncate_middle(&body, 200);
        assert!(t.starts_with("HEAD"));
        assert!(t.ends_with("TAIL"));
        assert!(t.contains("middle trimmed"));
        assert!(t.chars().count() <= 200 + 10);
        // Short input is returned unchanged.
        assert_eq!(truncate_middle("abc", 10), "abc");
    }

    #[test]
    fn plain_for_cache_strips_markdown_and_emoji() {
        let s = "### 📊 ETH Market Update\n\n**Bearish bias:** traders are leveraged short.\n- Funding: +0.0040%\n";
        let p = plain_for_cache(s);
        assert!(!p.contains('#'));
        assert!(!p.contains("**"));
        assert!(
            p.chars().all(|c| (c as u32) < 0x2600),
            "no pictographs: {p}"
        );
        assert!(p.contains("Bearish bias: traders are leveraged short"));
        assert!(p.contains("Funding: +0.0040%"));
    }

    #[test]
    fn plain_for_cache_keeps_text_and_numbers() {
        let s = "### Summary\nBTC $83,60 (-0.9%)\n\n\n\t\nprice_7d_ago: 85766.87 USD";
        let p = plain_for_cache(s);
        assert!(p.contains("BTC $83,60 (-0.9%)"));
        assert!(p.contains("price_7d_ago: 85766.87 USD"));
        // runs of blank lines collapse to a single blank line
        assert!(!p.contains("\n\n\n"));
    }

    #[test]
    fn plain_for_cache_collapses_blank_lines_and_trims() {
        let p = plain_for_cache("\n\n### heading\n\n\n\nbody line\n\n\n");
        assert_eq!(p, "heading\n\nbody line");
    }

    #[test]
    fn plain_for_cache_keeps_cyrillic() {
        let p = plain_for_cache("### 🚀 Проверка\nСигнал: докупать не надо");
        assert!(p.contains("Сигнал: докупать не надо"));
    }

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

    #[test]
    fn markdown_bold_and_italic_become_html() {
        let html = markdown_to_html("**WAIT.** the trend is _flat_ and `aS` is weak");
        assert!(html.contains("<b>WAIT.</b>"), "html = {html}");
        assert!(html.contains("<i>flat</i>"), "html = {html}");
        assert!(html.contains("<code>aS</code>"), "html = {html}");
        assert!(!html.contains("**"), "no raw asterisks: {html}");
    }

    #[test]
    fn markdown_escapes_raw_html() {
        let html = markdown_to_html("a < b & c > d");
        assert!(html.contains("&lt;") && html.contains("&amp;") && html.contains("&gt;"));
    }

    #[test]
    fn triple_emphasis_consumes_full_closing_run() {
        let html = markdown_to_html("cell **~+41.7%*** done");
        assert!(html.contains("<b>~+41.7%</b>"), "html = {html}");
        assert!(!html.contains("*"), "no stray asterisks: {html}");
    }

    #[test]
    fn unmatched_bold_run_is_dropped() {
        let html = markdown_to_html("average purchase price**");
        assert!(!html.contains('*'), "stray marker must be dropped: {html}");
        assert!(html.contains("average purchase price"), "html = {html}");
    }

    #[test]
    fn underscore_inside_identifier_is_untouched() {
        let html = markdown_to_html("price_7d_ago and change_24h");
        assert!(html.contains("price_7d_ago"), "html = {html}");
        assert!(html.contains("change_24h"), "html = {html}");
        assert!(!html.contains("<i>"), "identifiers must not become italic: {html}");
    }

    #[test]
    fn underscore_emphasis_still_works_at_boundaries() {
        let html = markdown_to_html("_Gen: test snapshot_");
        assert!(html.contains("<i>Gen: test snapshot</i>"), "html = {html}");
    }

    #[test]
    fn html_tags_keep_markdown_converted() {
        // Model mixes real HTML with unstyled markdown; both must normalise.
        let html = model_output_to_html("<b>ETH</b> is **3.4% below** now");
        assert!(html.contains("<b>ETH</b>"), "html = {html}");
        assert!(html.contains("<b>3.4% below</b>"), "html = {html}");
        assert!(!html.contains("**"), "no raw asterisks: {html}");
    }

    #[test]
    fn emphasis_can_span_html_tag_boundary() {
        let html = model_output_to_html("**before <i>mid</i> after**");
        assert!(html.contains("<b>before <i>mid</i> after</b>"), "html = {html}");
        assert!(!html.contains("**"), "no raw asterisks: {html}");
    }

    #[test]
    fn html_model_output_escapes_unknown_angles() {
        let html = model_output_to_html("<b>ok</b> if x < y & z > w");
        assert!(html.contains("<b>ok</b>"), "html = {html}");
        // Unknown angle brackets must not survive as raw tags.
        assert!(!html.contains("< y"), "html = {html}");
        assert!(html.contains("&lt; y"), "html = {html}");
    }

    #[test]
    fn telegram_strong_and_em_tags_survive() {
        let html = model_output_to_html("<strong>bold</strong> and <em>it</em>");
        assert!(html.contains("<strong>bold</strong>"), "html = {html}");
        assert!(html.contains("<em>it</em>"), "html = {html}");
        assert!(!html.contains("&lt;strong"), "html = {html}");
    }

    #[test]
    fn code_fence_becomes_pre_and_is_escaped() {
        let html = markdown_to_html("```rust\nlet x = 1 < 2;\n```");
        assert!(html.contains("<pre>"), "html = {html}");
        assert!(html.contains("1 &lt; 2"), "html = {html}");
    }

    #[test]
    fn table_rows_are_rendered_in_pre() {
        let md = "| A | B |\n|---|---|\n| 1 | 2 |";
        let html = markdown_to_html(md);
        assert!(
            html.contains("<pre>") && html.contains("| A | B |"),
            "html = {html}"
        );
        // The separator row must be dropped.
        assert!(!html.contains("|---|"), "html = {html}");
    }

    #[test]
    fn slash_units_never_form_commands() {
        let guarded = guard_slash_units("vol 2.3%/day and funding 0.01%/8h");
        // The slash is still visible but a zero-width space follows it.
        assert!(guarded.contains("/\u{200B}day"), "guarded = {guarded}");
        assert!(guarded.contains("/\u{200B}8h"), "guarded = {guarded}");
    }

    #[test]
    fn markdown_converter_guards_units() {
        let html = markdown_to_html("vol **2.3%/day**");
        assert!(html.contains("/\u{200B}day"), "html = {html}");
    }
}
