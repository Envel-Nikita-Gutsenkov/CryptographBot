//! Telegram MarkdownV2 validation helpers.
//!
//! Telegram rejects a whole message with `can't parse entities: Character 'X'
//! is reserved` when a reserved character is not backslash-escaped. That
//! failure is only visible at send time (a user sees a red error bubble), so we
//! validate locally instead: messages with a literal + `format!` skeleton are
//! checked by a unit test that scans the sources, and runtime text can go
//! through [`prepare`], which falls back to a fully escaped send rather than
//! letting Telegram reject the whole message.
//!
//! Validation covers the rules that actually break real messages:
//! * `- . ! + = # { } >` must be escaped outside code spans;
//! * `* _ ~ | [] ()` delimiters must be balanced;
//! * inside a link destination `(...)` only `)` and `\` are special;
//! * inside `` `code` `` and ```` ```pre``` ```` only `` ` `` and `\` are special.
//!
//! It is deliberately a validator, not a renderer: entity nesting rules that
//! Telegram accepts in practice are not re-implemented here.

/// Characters Telegram reserves outside entities. Inside code spans only
/// `` ` `` and `\` are special, so these are legal there unescaped.
const RESERVED_ALWAYS: &[char] = &[
    '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
];

/// Reserved characters that must never appear unescaped outside code — these
/// have no legitimate unescaped use at all (unlike `*`, `_`, `[`, … which open
/// entities).
const RESERVED_STRICT: &[char] = &['-', '.', '!', '+', '=', '#', '{', '}', '>'];

/// Why a message would be rejected by Telegram's MarkdownV2 parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MdV2Error {
    pub char_at: char,
    pub position: usize,
    pub reason: &'static str,
}

impl std::fmt::Display for MdV2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unescaped {:?} at byte {} ({})",
            self.char_at, self.position, self.reason
        )
    }
}

/// Returns `Ok(())` when Telegram would accept `text` as MarkdownV2.
pub fn validate(text: &str) -> Result<(), MdV2Error> {
    let mut escaped = false;
    let mut in_code = false;
    let mut in_pre = false;
    let mut in_link_dest = false;
    let mut prev = '\0';
    let mut prev_escaped = false;

    // Delimiter balance. `_` and `*` double as bold/italic markers, so counts
    // are kept rather than pair-matched.
    let mut underscores = 0usize;
    let mut stars = 0usize;
    let mut tildes = 0usize;
    let mut open_brackets = 0usize;
    let mut open_parens = 0usize;
    let mut open_pipes = 0usize;

    let mut iter = text.char_indices().peekable();
    while let Some((pos, c)) = iter.next() {
        if in_pre {
            // ``` ... ``` — a literal backtick triple closes it.
            if c == '`' {
                let mut n = 1;
                while let Some((_, '`')) = iter.peek() {
                    iter.next();
                    n += 1;
                }
                if n >= 3 {
                    in_pre = false;
                }
            }
            continue;
        }
        // Inside a link destination `(...)` Telegram treats only `)` and `\`
        // specially; dots, slashes, `!` and the like are literal there.
        if in_link_dest {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == ')' {
                in_link_dest = false;
            }
            prev = c;
            prev_escaped = false;
            continue;
        }

        if escaped {
            escaped = false;
            prev = c;
            prev_escaped = true;
            continue;
        }
        match c {
            '\\' => {
                escaped = true;
                continue;
            }
            '`' => {
                // Three or more backticks open a pre block, one opens code.
                let mut n = 1;
                while let Some((_, '`')) = iter.peek() {
                    iter.next();
                    n += 1;
                }
                if n >= 3 {
                    in_pre = true;
                } else {
                    in_code = !in_code;
                }
                prev = c;
                prev_escaped = false;
                continue;
            }
            _ if in_code => {
                prev = c;
                prev_escaped = false;
                continue;
            }
            _ => {}
        }

        // `](` opens a link destination.
        if c == '(' && prev == ']' && !prev_escaped {
            in_link_dest = true;
            prev = c;
            prev_escaped = false;
            continue;
        }

        if RESERVED_STRICT.contains(&c) {
            return Err(MdV2Error {
                char_at: c,
                position: pos,
                reason: "reserved character must be escaped outside code",
            });
        }
        match c {
            '*' => stars += 1,
            '_' => underscores += 1,
            '~' => tildes += 1,
            '[' => open_brackets += 1,
            ']' => open_brackets = open_brackets.saturating_sub(1),
            '(' => open_parens += 1,
            ')' => open_parens = open_parens.saturating_sub(1),
            '|' => open_pipes += 1,
            _ => {}
        }
        prev = c;
        prev_escaped = false;
    }

    if in_link_dest {
        return Err(MdV2Error {
            char_at: '(',
            position: text.len(),
            reason: "unbalanced MarkdownV2 link destination",
        });
    }

    let unbalanced = [
        (stars % 2 != 0, '*'),
        (underscores % 2 != 0, '_'),
        (tildes % 2 != 0, '~'),
        (open_brackets != 0, '['),
        (open_parens != 0, '('),
        (open_pipes % 2 != 0, '|'),
    ];
    if let Some((_, ch)) = unbalanced.iter().find(|(bad, _)| *bad) {
        return Err(MdV2Error {
            char_at: *ch,
            position: text.len(),
            reason: "unbalanced MarkdownV2 entity delimiter",
        });
    }
    Ok(())
}

/// Returns a message body safe to send with `ParseMode::MarkdownV2`.
///
/// Valid input is returned untouched (so intentional `*bold*`, `_italic_`,
/// links and code spans keep their formatting). Invalid input is escaped into
/// literal text and a warning is logged, so a single stray `-` can never turn
/// into a red "can't parse entities" bubble for the user.
pub fn prepare(text: &str) -> std::borrow::Cow<'_, str> {
    match validate(text) {
        Ok(()) => std::borrow::Cow::Borrowed(text),
        Err(err) => {
            tracing::warn!(target: "mdv2", %err, "MarkdownV2 validation failed; sending escaped text");
            std::borrow::Cow::Owned(escape(text))
        }
    }
}


/// Escape every reserved character so `text` renders literally under
/// MarkdownV2. Existing backslash escapes are preserved.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // Keep an existing escape pair intact.
            out.push('\\');
            if let Some(next) = chars.next() {
                out.push(next);
            }
            continue;
        }
        if RESERVED_ALWAYS.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_hyphen_is_rejected() {
        // The exact production failure: `whole-book` in the Trading menu.
        let err = validate("open the whole-book views:").unwrap_err();
        assert_eq!(err.char_at, '-');
    }

    #[test]
    fn escaped_reserved_characters_pass() {
        validate("open the whole\\-book views:").unwrap();
        validate("RSI 41 \\> 30 \\(ok\\)\\.").unwrap();
        validate("P\\&L is fine \\!").unwrap();
    }

    #[test]
    fn code_spans_allow_reserved_characters() {
        validate("use `--name foo` or `a-b.c!`").unwrap();
        validate("```\n--flag -2.5%\n```").unwrap();
    }

    #[test]
    fn balanced_entities_pass_and_unbalanced_fail() {
        validate("*bold* and _italic_ and ~strike~").unwrap();
        validate("[label](https://example.com/a)").unwrap();
        assert!(validate("*bold").is_err());
        assert!(validate("[label](url").is_err());
    }

    #[test]
    fn escape_round_trips_through_validate() {
        let raw = "Trading Toolkit — whole-book views: P&L 100% (a.b!) #1 +2 -3 =x |y| {z} ~w~ *q* _e_ [l](u)";
        let escaped = escape(raw);
        validate(&escaped).expect("escaped text must validate");
    }

    #[test]
    fn escape_keeps_existing_escapes() {
        assert_eq!(escape("a\\.b"), "a\\.b");
        assert_eq!(escape("a.b"), "a\\.b");
    }

    // ------------------------------------------------------------------
    // Source scan: catch unescaped reserved characters in *static*
    // MarkdownV2 messages before they reach Telegram (the `whole-book` bug).
    // ------------------------------------------------------------------

    use std::path::{Path, PathBuf};

    fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                collect_rust_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }

    /// Parses a Rust double-quoted literal starting at `open` (index of the
    /// opening quote) and returns its raw body (escapes left intact).
    fn literal_body(s: &str, open: usize) -> Option<String> {
        let mut body = String::new();
        let mut prev_backslash = false;
        for c in s[open + 1..].chars() {
            if prev_backslash {
                body.push('\\');
                body.push(c);
                prev_backslash = false;
                continue;
            }
            match c {
                '\\' => prev_backslash = true,
                '"' => return Some(body),
                _ => body.push(c),
            }
        }
        None
    }

    /// Resolves Rust string escapes (`\\`, `\n`, line continuations, …).
    fn unescape_rust(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('\'') => out.push('\''),
                Some('\n') => {
                    // Backslash-newline: drop the newline and leading indentation.
                    while matches!(chars.peek(), Some(' ') | Some('\t') | Some('\n') | Some('\r')) {
                        chars.next();
                    }
                }
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        }
        out
    }

    /// Replaces `format!` placeholders (`{id}`, `{:.8}`, `{name:?}`) with a
    /// harmless letter so the surrounding literal can be validated. `{{`/`}}`
    /// become literal braces (which remain in the output and are validated).
    fn neutralize_format_placeholders(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '{' {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    out.push('{');
                    continue;
                }
                for d in chars.by_ref() {
                    if d == '}' {
                        break;
                    }
                }
                out.push('X');
            } else {
                out.push(c);
            }
        }
        out
    }

    /// Scans `src`, returning `file:line` failures for static MarkdownV2 texts.
    fn scan_source(label: &str, src: &str, problems: &mut Vec<String>) {
        // Split so this scanner's own needle is not found in this file.
        let needle = concat!("ParseMode::", "MarkdownV2");
        let mut from = 0;
        while let Some(rel) = src[from..].find(needle) {
            let pos = from + rel;
            from = pos + needle.len();

            let line_no = src[..pos].matches('\n').count() + 1;
            let line_start = src[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let line_end = src[pos..].find('\n').map(|i| pos + i).unwrap_or(src.len());
            if src[line_start..line_end].trim_start().starts_with("//") {
                continue; // doc/comment mention, not a send
            }

            let head = &src[..pos];
            let owner = match head
                .rfind("send_message")
                .or_else(|| head.rfind("edit_message_text"))
            {
                Some(i) => i,
                None => continue,
            };
            let slice = &src[owner..pos];
            if slice.contains(';') {
                continue; // crossed a statement boundary
            }

            let is_format = slice.contains("format!");
            let open = match slice.find('"') {
                Some(i) => i,
                None => continue, // text comes from a variable
            };
            let body = match literal_body(slice, open) {
                Some(b) => b,
                None => continue,
            };
            let mut text = unescape_rust(&body);
            if is_format {
                text = neutralize_format_placeholders(&text);
            }
            if let Err(err) = validate(&text) {
                problems.push(format!("{label}:{line_no}: {err} -- {text:?}"));
            }
        }
    }

    #[test]
    fn scanner_flags_a_bare_hyphen_literal() {
        // Mirrors the pre-fix `menu:trading` arm.
        let src = "\
            bot.send_message(msg.chat.id,
                \"Trading Toolkit -- whole-book views:\")
                .parse_mode(ParseMode::MarkdownV2)
                .await;
";
        let mut problems = Vec::new();
        scan_source("synthetic.rs", src, &mut problems);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("'-'"), "{}", problems[0]);
    }

    #[test]
    fn scanner_accepts_escaped_and_dynamic_messages() {
        let src = "\
            bot.send_message(msg.chat.id, \"open the whole\\\\-book views\")
                .parse_mode(ParseMode::MarkdownV2)
                .await;
            let n = 1;
            bot.send_message(msg.chat.id, format!(\"row {} ok\", n))
                .parse_mode(ParseMode::MarkdownV2)
                .await;
            bot.send_message(msg.chat.id, rendered)
                .parse_mode(ParseMode::MarkdownV2)
                .await;
";
        let mut problems = Vec::new();
        scan_source("synthetic.rs", src, &mut problems);
        assert!(problems.is_empty(), "unexpected: {problems:?}");
    }

    #[test]
    fn every_static_markdownv2_message_is_valid() {
        let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        collect_rust_files(&src_root, &mut files);
        assert!(!files.is_empty(), "no Rust sources under {src_root:?}");

        let mut problems = Vec::new();
        for file in &files {
            // This module holds the scanner and the word in its docs.
            if file.ends_with("mdv2.rs") {
                continue;
            }
            let src = std::fs::read_to_string(file).expect("read source");
            scan_source(&file.display().to_string(), &src, &mut problems);
        }

        assert!(
            problems.is_empty(),
            "static MarkdownV2 messages Telegram would reject:\n{}",
            problems.join("\n")
        );
    }
}
