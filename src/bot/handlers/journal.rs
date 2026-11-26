//! `/journal` — admin view of the bot's own decision journal.
//!
//! The journal itself is written by the dispatcher (every inbound update) and by
//! the decision-producing handlers (`/decide`, `/dip`, `/sniper`, `/forecast`,
//! `/ladder`, `/ask`). This handler only reads it, so the operator can audit
//! "what was asked, what was answered, on what market snapshot, and why" without
//! shelling into the container.

use std::sync::Arc;

use teloxide::{prelude::*, types::ParseMode};

use crate::{
    config::Config,
    db::{JournalRow, SqliteStore},
};

/// Rows fetched for one `/journal` page. Bigger than the default so the cap can
/// be reached without a second query, small enough to stay under Telegram's
/// 4096-unit message limit after formatting.
const MAX_LIMIT: i64 = 25;
const DEFAULT_LIMIT: i64 = 10;

pub async fn handle_journal_cmd(
    bot: Bot,
    msg: Message,
    sqlite: Arc<SqliteStore>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);
    if !config.is_admin(user_id) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let words: Vec<&str> = args.split_whitespace().collect();
    let text = match words.first().copied().unwrap_or("") {
        "help" => usage(),
        "stats" => match sqlite.journal_stats() {
            Ok(stats) => render_stats(&stats, crate::db::sqlite::JOURNAL_KEEP_LAST),
            Err(e) => format!(
                "❌ journal stats failed: {}",
                html_escape::encode_text(&e.to_string())
            ),
        },
        "grade" => {
            let horizon = words.get(1).and_then(|s| s.parse().ok()).unwrap_or(14);
            match sqlite.journal_grade(horizon) {
                Ok(grades) => render_grade(&grades, horizon),
                Err(e) => format!(
                    "❌ journal grade failed: {}",
                    html_escape::encode_text(&e.to_string())
                ),
            }
        }
        "prune" => {
            let keep = words
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(crate::db::sqlite::JOURNAL_KEEP_LAST);
            match sqlite.prune_journal(keep) {
                Ok(n) => format!("🧹 Pruned <b>{n}</b> old rows (keeping newest {keep})."),
                Err(e) => format!(
                    "❌ prune failed: {}",
                    html_escape::encode_text(&e.to_string())
                ),
            }
        }
        "" | "list" => {
            let limit = words
                .get(1)
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_LIMIT);
            let kind = words.get(2).copied();
            recent(&sqlite, limit, kind)
        }
        first => {
            // `/journal 20` or `/journal decision` both work.
            match first.parse::<i64>() {
                Ok(n) => recent(&sqlite, n, words.get(1).copied()),
                Err(_) => recent(&sqlite, DEFAULT_LIMIT, Some(first)),
            }
        }
    };

    let text = clamp_telegram(&text);
    let _ = bot
        .send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .await;
    Ok(())
}

fn recent(sqlite: &SqliteStore, limit: i64, kind: Option<&str>) -> String {
    let limit = limit.clamp(1, MAX_LIMIT);
    let rows = match sqlite.recent_journal(limit) {
        Ok(rows) => rows,
        Err(e) => {
            return format!(
                "❌ journal read failed: {}",
                html_escape::encode_text(&e.to_string())
            )
        }
    };
    let rows: Vec<JournalRow> = match kind {
        Some(k) => rows.into_iter().filter(|r| r.kind == k).collect(),
        None => rows,
    };
    render_recent(&rows, limit, kind)
}

/// Pure renderer, kept separate so the formatting rules are testable without a
/// database.
fn render_recent(rows: &[JournalRow], limit: i64, kind: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str("📒 <b>Journal</b>");
    if let Some(k) = kind {
        out.push_str(&format!(" · kind: <code>{}</code>", escape(k)));
    }
    out.push_str(&format!(
        " · newest {limit} (cap {})\n\n",
        crate::db::sqlite::JOURNAL_KEEP_LAST
    ));
    if rows.is_empty() {
        out.push_str("Empty — no commands, answers or decisions recorded yet.");
        return out;
    }

    for (i, r) in rows.iter().enumerate() {
        out.push_str(&format!(
            "<b>#{}</b> <code>{}</code> · {} · <code>{}</code>\n",
            i + 1,
            escape(&short_time(&r.created_at)),
            escape(&r.kind),
            escape(&r.command),
        ));
        match r.kind.as_str() {
            "decision" => {
                let mut line = String::new();
                if !r.symbol.is_empty() {
                    line.push_str(&format!("<b>{}</b> ", escape(&r.symbol)));
                }
                if !r.action.is_empty() {
                    line.push_str(&format!("{} ", escape(&r.action.to_uppercase())));
                }
                if let Some(c) = r.confidence {
                    line.push_str(&format!("· {:.0}% ", (c * 100.0).clamp(0.0, 100.0)));
                }
                for part in context_parts(&r.context_json) {
                    line.push_str(&format!("· {}", part));
                }
                if !line.trim().is_empty() {
                    out.push_str(&format!("   {}\n", line.trim_end()));
                }
                if !r.reason.is_empty() {
                    out.push_str(&format!("   <i>{}</i>\n", escape(&clip(&r.reason, 200))));
                }
            }
            "answer" => {
                if !r.args.is_empty() {
                    out.push_str(&format!("   question: {}\n", escape(&clip(&r.args, 160))));
                }
                if !r.result.is_empty() {
                    out.push_str(&format!("   <i>{}</i>\n", escape(&clip(&r.result, 240))));
                }
            }
            _ => {
                // Inbound command / text / callback: show what the user sent.
                if !r.args.is_empty() {
                    out.push_str(&format!("   {}\n", escape(&clip(&r.args, 200))));
                } else if r.command == "media" {
                    out.push_str("   (media)\n");
                }
            }
        }
    }
    out.trim_end().to_string()
        + "\n\n<i>/journal stats · /journal grade [days] · /journal prune [keep]</i>"
}

/// Pull the market snapshot out of a decision's `context_json`, keeping only the
/// keys the decision actually used. Unknown shapes are ignored, never fatal.
fn context_parts(ctx: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(ctx) else {
        return Vec::new();
    };
    let mut parts = Vec::new();
    let num = |k: &str| v.get(k).and_then(|x| x.as_f64());
    let text = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string());

    if let Some(p) = num("price").or_else(|| num("spot")) {
        parts.push(format!("spot {}", crate::format::fmt_price(p)));
    }
    if let Some(r) = num("rsi14").or_else(|| num("rsi")) {
        parts.push(format!("RSI {r:.0}"));
    }
    if let Some(regime) = text("regime") {
        parts.push(escape(&clip(&regime, 40)));
    }
    if let Some(vol) = num("vol_daily_pct").or_else(|| num("sigma_pct")) {
        parts.push(format!("σ {vol:.1}%/d"));
    }
    if let Some(p14) = num("p14").or_else(|| num("touch_prob")) {
        parts.push(format!("P14 {:.0}%", (p14 * 100.0).clamp(0.0, 100.0)));
    }
    if let Some(n) = v.get("levels").and_then(|l| l.as_array()).map(|a| a.len()) {
        parts.push(format!("{n} levels"));
    }
    parts
}

fn render_stats(stats: &[(String, String, i64)], cap: i64) -> String {
    if stats.is_empty() {
        return "📊 Journal is empty — nothing recorded yet.".to_string();
    }
    let mut out = format!("📊 <b>Journal stats</b> (cap {cap} rows)\n\n<pre>");
    for (kind, action, n) in stats {
        out.push_str(&format!(
            "{:<10} {:<16} {}\n",
            clip(kind, 10),
            clip(action, 16),
            n
        ));
    }
    out.push_str("</pre>");
    out
}

fn render_grade(grades: &[crate::db::JournalGrade], horizon: i64) -> String {
    if grades.is_empty() {
        return format!(
            "📉 No decisions old enough to grade yet (horizon {horizon}d). \
             The journal needs price history at least that far ahead of a decision."
        );
    }
    // Mean move per action: a systematically wrong "wait" shows up here.
    use std::collections::BTreeMap;
    let mut agg: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for g in grades {
        if let Some(ch) = g.change_pct {
            let e = agg.entry(g.action.clone()).or_insert((0, 0.0));
            e.0 += 1;
            e.1 += ch;
        }
    }
    let mut out = format!("📉 <b>Decision accuracy</b> · horizon {horizon}d\n\n<pre>");
    for (action, (n, sum)) in agg {
        out.push_str(&format!(
            "{:<8} n={:<4} mean {:+.2}%\n",
            clip(&action, 8),
            n,
            sum / n as f64
        ));
    }
    out.push_str(
        "</pre>\n<i>Forward change after each call: buy should be positive, wait negative.</i>",
    );
    out
}

fn usage() -> String {
    "📒 <b>/journal</b> — audit the bot's own log\n\n\
     • <code>/journal</code> — newest 10 entries\n\
     • <code>/journal 20</code> — newest 20 (max 25 per page)\n\
     • <code>/journal decision</code> — filter by kind (inbound, answer, decision)\n\
     • <code>/journal stats</code> — counts per kind and command\n\
     • <code>/journal grade 14</code> — mean forward move per action (accuracy)\n\
     • <code>/journal prune 1000</code> — keep only the newest N rows"
        .to_string()
}

fn short_time(ts: &str) -> String {
    // Stored as `YYYY-MM-DD HH:MM:SS` (UTC); show `MM-DD HH:MM`.
    if ts.len() >= 16 {
        ts[5..16].to_string()
    } else {
        ts.to_string()
    }
}

fn clip(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn escape(s: &str) -> String {
    html_escape::encode_text(s).to_string()
}

/// Telegram rejects messages over 4096 UTF-16 units. Cut on a char boundary so
/// a giant page can never break the send.
fn clamp_telegram(text: &str) -> String {
    const MAX_UNITS: usize = 4000;
    if text.encode_utf16().count() <= MAX_UNITS {
        return text.to_string();
    }
    let mut out = String::new();
    let mut units = 0;
    for ch in text.chars() {
        let u = ch.len_utf16();
        if units + u > MAX_UNITS - 1 {
            break;
        }
        out.push(ch);
        units += u;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: &str, command: &str, args: &str, action: &str) -> JournalRow {
        JournalRow {
            created_at: "2026-11-26 19:31:05".to_string(),
            user_id: 7,
            kind: kind.to_string(),
            command: command.to_string(),
            args: args.to_string(),
            symbol: "BTC".to_string(),
            action: action.to_string(),
            confidence: Some(0.62),
            reason: "levels solved | RSI 41".to_string(),
            context_json:
                r#"{"price":60000.0,"rsi14":41.0,"regime":"sideways","vol_daily_pct":3.2}"#
                    .to_string(),
            result: String::new(),
            outcome: None,
        }
    }

    #[test]
    fn empty_journal_says_so() {
        let text = render_recent(&[], 10, None);
        assert!(text.contains("Empty"));
    }

    #[test]
    fn decision_block_carries_the_snapshot_and_reason() {
        let text = render_recent(&[row("decision", "/decide", "BTC", "buy")], 10, None);
        assert!(text.contains("BUY"));
        assert!(text.contains("62%"));
        assert!(text.contains("RSI 41"));
        assert!(text.contains("sideways"));
        assert!(text.contains("P14") == false); // no p14 key in this context
        assert!(text.contains("levels solved"));
    }

    #[test]
    fn inbound_text_is_escaped_not_rendered() {
        let mut r = row("inbound", "text", "<b>hi</b> & bye", "");
        r.kind = "inbound".into();
        let text = render_recent(&[r], 10, None);
        assert!(text.contains("&lt;b&gt;hi&lt;/b&gt; &amp; bye"));
        assert!(!text.contains("<b>hi</b>"));
    }

    #[test]
    fn long_fields_are_clipped() {
        let mut r = row("answer", "/ask", &"q".repeat(500), "");
        r.result = "a".repeat(1000);
        r.kind = "answer".into();
        let text = render_recent(&[r], 10, None);
        assert!(text.chars().count() < 1200);
        assert!(text.contains('…'));
    }

    #[test]
    fn clamps_to_telegram_limit() {
        let text = "я".repeat(5000);
        let out = clamp_telegram(&text);
        assert!(out.encode_utf16().count() <= 4000);
    }

    #[test]
    fn stats_and_grade_render_without_panicking_on_odd_data() {
        assert!(render_stats(&[], 1000).contains("empty"));
        let stats = vec![("inbound".to_string(), "/decide".to_string(), 12)];
        assert!(render_stats(&stats, 1000).contains("/decide"));
        assert!(render_grade(&[], 14).contains("No decisions"));
    }
}
