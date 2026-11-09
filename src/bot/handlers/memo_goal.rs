//! `/memo`, `/goal` and `/plan` commands — AI memory notes, proactive
//! investment goals, and the plan dashboard.
//!
//! Everything is stored in the `Portfolio` (so it rides along in `/backup`
//! exports) AND mirrored into dedicated SQLite tables (`user_memos` /
//! `user_goals`) so it survives even if the generic `users` table is rebuilt.
//!
//! Goals carry **temporal memory**: dated progress notes (user-reported via
//! `/goal progress` or appended by the AI on scans) and the last AI reasoning
//! (`ai_summary`), both fed back into every proactive check so the model knows
//! how much of the plan is already done.

use std::sync::Arc;

use chrono::{Datelike, Duration, TimeZone, Timelike, Utc};
use teloxide::{prelude::*, types::ParseMode};

use crate::{
    config::Config,
    crypto::CryptoClient,
    db::{SqliteStore, Store},
    models::{Goal, Memo, ProgressNote},
};

pub async fn handle_memo_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    sqlite: Arc<SqliteStore>,
    user_id: u64,
    args: String,
) -> anyhow::Result<()> {
    let arg = args.trim();

    match arg {
        "" | "list" => list_memos(&bot, msg.chat.id, &store, user_id).await,
        "clear" => {
            store.write(user_id, |p| p.memos.clear());
            persist_memos(&store, &sqlite, user_id).await;
            bot.send_message(msg.chat.id, "🗑 All memos deleted.")
                .await?;
            Ok(())
        }
        _ if arg.starts_with("del ") || arg.starts_with("delete ") => {
            let id: u64 = arg
                .split_whitespace()
                .nth(1)
                .unwrap_or("0")
                .trim_start_matches('#')
                .parse()
                .unwrap_or(0);
            let removed = store.write(user_id, |p| {
                let before = p.memos.len();
                p.memos.retain(|m| m.id != id);
                before != p.memos.len()
            });
            if removed {
                persist_memos(&store, &sqlite, user_id).await;
                bot.send_message(msg.chat.id, format!("🗑 Memo #{} deleted.", id))
                    .await?;
            } else {
                bot.send_message(msg.chat.id, format!("❌ Memo #{} not found.", id))
                    .await?;
            }
            Ok(())
        }
        text => {
            let memo_id = store.write(user_id, |p| {
                let id = p.alloc_memo_id();
                p.memos.push(Memo {
                    id,
                    text: text.to_string(),
                    created_at: Utc::now(),
                });
                id
            });
            persist_memos(&store, &sqlite, user_id).await;
            bot.send_message(
                msg.chat.id,
                format!(
                    "🧠 <b>Memo #{} saved</b>.\nThe AI will always take it into account when analysing the market and your portfolio.",
                    memo_id
                ),
            )
            .parse_mode(ParseMode::Html)
            .await?;
            Ok(())
        }
    }
}

pub async fn handle_goal_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    sqlite: Arc<SqliteStore>,
    user_id: u64,
    args: String,
) -> anyhow::Result<()> {
    let arg = args.trim();

    match arg {
        "" | "list" => list_goals(&bot, msg.chat.id, &store, user_id).await,
        _ if arg.starts_with("del ") || arg.starts_with("delete ") => {
            let id: u64 = arg
                .split_whitespace()
                .nth(1)
                .unwrap_or("0")
                .trim_start_matches('#')
                .parse()
                .unwrap_or(0);
            let removed = store.write(user_id, |p| {
                let before = p.goals.len();
                p.goals.retain(|g| g.id != id);
                before != p.goals.len()
            });
            if removed {
                persist_goals(&store, &sqlite, user_id).await;
                bot.send_message(msg.chat.id, format!("🗑 Goal #{} deleted.", id))
                    .await?;
            } else {
                bot.send_message(msg.chat.id, format!("❌ Goal #{} not found.", id))
                    .await?;
            }
            Ok(())
        }
        _ if arg.starts_with("done ") => {
            let id: u64 = arg
                .split_whitespace()
                .nth(1)
                .unwrap_or("0")
                .trim_start_matches('#')
                .parse()
                .unwrap_or(0);
            let found = store.write(user_id, |p| {
                if let Some(g) = p.goals.iter_mut().find(|g| g.id == id) {
                    g.done = true;
                    g.active = false;
                    // Clean up temporal memory of a finished goal (no junk kept).
                    g.ai_summary = None;
                    g.ai_summary_at = None;
                    g.progress_notes.clear();
                    true
                } else {
                    false
                }
            });
            if found {
                persist_goals(&store, &sqlite, user_id).await;
                bot.send_message(msg.chat.id, format!("🎯 Goal #{} marked as done.", id))
                    .await?;
            } else {
                bot.send_message(msg.chat.id, format!("❌ Goal #{} not found.", id))
                    .await?;
            }
            Ok(())
        }
        _ if arg.starts_with("resume ") => {
            let id: u64 = arg
                .split_whitespace()
                .nth(1)
                .unwrap_or("0")
                .trim_start_matches('#')
                .parse()
                .unwrap_or(0);
            let found = store.write(user_id, |p| {
                if let Some(g) = p.goals.iter_mut().find(|g| g.id == id) {
                    g.done = false;
                    g.active = true;
                    g.last_alert_at = None;
                    true
                } else {
                    false
                }
            });
            if found {
                persist_goals(&store, &sqlite, user_id).await;
                bot.send_message(msg.chat.id, format!("🔄 Goal #{} is active again.", id))
                    .await?;
            } else {
                bot.send_message(msg.chat.id, format!("❌ Goal #{} not found.", id))
                    .await?;
            }
            Ok(())
        }
        // `/goal progress <id> <text>` (alias: `/goal note`) — tell the AI what
        // you already did, so it remembers dates and plan stages.
        _ if arg.starts_with("progress ") || arg.starts_with("note ") => {
            let mut parts = arg.splitn(3, ' ');
            parts.next();
            let id: u64 = parts
                .next()
                .unwrap_or("0")
                .trim_start_matches('#')
                .parse()
                .unwrap_or(0);
            let text = parts.next().unwrap_or("").trim().to_string();
            if text.is_empty() {
                bot.send_message(
                    msg.chat.id,
                    "📌 Usage: <code>/goal progress N what you did</code>\ne.g. <code>/goal progress 1 bought 100 USD ETH</code>",
                )
                .parse_mode(ParseMode::Html)
                .await?;
                return Ok(());
            }
            let ok = store.write(user_id, |p| {
                if let Some(g) = p.goals.iter_mut().find(|g| g.id == id) {
                    g.progress_notes.push(ProgressNote {
                        at: Utc::now(),
                        text,
                    });
                    true
                } else {
                    false
                }
            });
            if ok {
                persist_goals(&store, &sqlite, user_id).await;
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "📌 Progress added to goal #{} — the AI will remember it.",
                        id
                    ),
                )
                .await?;
            } else {
                bot.send_message(msg.chat.id, format!("❌ Goal #{} not found.", id))
                    .await?;
            }
            Ok(())
        }
        text => {
            let expires = parse_goal_expiry(text);
            let goal_id = store.write(user_id, |p| {
                let id = p.alloc_goal_id();
                p.goals.push(Goal {
                    id,
                    text: text.to_string(),
                    created_at: Utc::now(),
                    expires_at: expires,
                    active: true,
                    last_alert_at: None,
                    last_declined_at: None,
                    ignore_ai_cancel: false,
                    alert_count: 0,
                    done: false,
                    progress_notes: Vec::new(),
                    ai_summary: None,
                    ai_summary_at: None,
                    sent_alerts: Vec::new(),
                });
                id
            });
            persist_goals(&store, &sqlite, user_id).await;
            let expiry_note = expires
                .map(|e| format!("\n⏳ Relevant until: {}", e.format("%d.%m.%Y %H:%M")))
                .unwrap_or_else(|| "\n⏳ Expiry: none (the AI judges relevance itself)".into());
            bot.send_message(
                msg.chat.id,
                format!(
                    "🎯 <b>Goal #{} set</b>.\n{}\n\nThe AI scans the market regularly and will notify you when conditions fit. Manage: <code>/goal list</code>, <code>/goal progress {}</code>, <code>/goal done {}</code>, <code>/goal del {}</code>.",
                    goal_id, expiry_note, goal_id, goal_id, goal_id
                ),
            )
            .parse_mode(ParseMode::Html)
            .await?;
            Ok(())
        }
    }
}

/// `/plan` — plan dashboard: menu with buttons, text status, or an on-demand AI check.
pub async fn handle_plan_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    sqlite: Arc<SqliteStore>,
    user_id: u64,
    args: String,
) -> anyhow::Result<()> {
    let arg = args.trim();

    match arg {
        "status" => plan_status(&bot, msg.chat.id, &store, user_id).await,
        "check" => plan_check(&bot, msg.chat.id, &store, &cmc, &sqlite, user_id).await,
        "goals" => list_goals(&bot, msg.chat.id, &store, user_id).await,
        "memos" => list_memos(&bot, msg.chat.id, &store, user_id).await,
        _ if arg.starts_with("ask ") || arg.starts_with("ask:") => {
            let q = arg
                .strip_prefix("ask ")
                .or_else(|| arg.strip_prefix("ask:"))
                .unwrap_or("")
                .trim()
                .to_string();
            if q.is_empty() {
                bot.send_message(
                    msg.chat.id,
                    "🤖 Usage: <code>/plan ask your question about your plan</code>",
                )
                .parse_mode(ParseMode::Html)
                .await?;
                return Ok(());
            }
            crate::bot::handlers::ask::handle_ask_cmd(bot, msg, store, cmc, config, sqlite, q).await
        }
        _ => {
            bot.send_message(
                msg.chat.id,
                "🎯 <b>Plan dashboard</b>\n\n\
                 • Goals — your proactive goals with progress & AI reasoning\n\
                 • Memos — AI memory notes\n\
                 • Plan Status — full timeline of every goal\n\
                 • AI Check — ask the AI right now: what's the state of my plan?\n\
                 • Help — full command reference",
            )
            .parse_mode(ParseMode::Html)
            .reply_markup(crate::bot::keyboards::plan_menu())
            .await?;
            Ok(())
        }
    }
}

async fn plan_status(
    bot: &Bot,
    chat_id: teloxide::types::ChatId,
    store: &Store,
    user_id: u64,
) -> anyhow::Result<()> {
    let goals = store.read(user_id, |p| p.goals.clone());
    if goals.is_empty() {
        bot.send_message(
            chat_id,
            "🎯 <b>No goals yet.</b>\nExample: <code>/goal want to spend 400 USD on crypto this month</code>",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    let mut lines = vec![
        "🎯 <b>Plan status</b>".to_string(),
        format!("Today: {} (UTC)", Utc::now().format("%Y-%m-%d")),
        String::new(),
    ];
    for g in &goals {
        let status = if g.done {
            "✅ done"
        } else if !g.active {
            "⏸ inactive"
        } else {
            "🟢 active"
        };
        let expires = fmt_expiry(g);
        lines.push(format!(
            "#{} {} — <b>{}</b>{}",
            g.id,
            status,
            escape_html(&g.text),
            expires
        ));
        if !g.progress_notes.is_empty() {
            lines.push("  📌 <b>Progress:</b>".into());
            for n in g.progress_notes.iter().take(8) {
                lines.push(format!(
                    "    • {}: {}",
                    n.at.format("%d.%m %H:%M"),
                    escape_html(&n.text)
                ));
            }
            if g.progress_notes.len() > 8 {
                lines.push(format!("    … and {} more", g.progress_notes.len() - 8));
            }
        } else {
            lines.push("  📌 <b>Progress:</b> nothing recorded yet".into());
        }
        if let Some(note) = ai_note(g) {
            let short = crate::format::truncate_chars(&note, 220);
            lines.push(format!("  🧠 <b>AI:</b> {}", escape_html(&short)));
        }
        lines.push(String::new());
    }
    lines.push("Add progress: <code>/goal progress N what you did</code>".into());
    lines.push("Ask the AI: <code>/plan check</code>".into());

    bot.send_message(chat_id, lines.join("\n"))
        .parse_mode(ParseMode::Html)
        .reply_markup(crate::bot::keyboards::plan_menu())
        .await?;
    Ok(())
}

/// On-demand AI plan check: runs the same double-checked scan as the background
/// worker but in report mode — nothing is auto-marked done/stale, no alerts are
/// consumed. The user sees what the AI thinks about every goal right now.
async fn plan_check(
    bot: &Bot,
    chat_id: teloxide::types::ChatId,
    store: &Store,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    user_id: u64,
) -> anyhow::Result<()> {
    let has_goals = store.read(user_id, |p| p.goals.iter().any(|g| g.active && !g.done));
    if !has_goals {
        bot.send_message(
            chat_id,
            "🎯 <b>No active goals</b> — nothing to check.\nAdd one: <code>/goal want to spend 400 USD on crypto this month</code>",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    bot.send_message(
        chat_id,
        "🤖 Checking your plan against the market… (30-120s)",
    )
    .await?;

    let outcome = match crate::crypto::ai::proactive_worker::run_scan(
        store, cmc, sqlite, user_id, false, false,
    )
    .await
    {
        Ok(o) => o,
        Err(e) => {
            let err_str = crate::format::redact_secrets(&e.to_string());
            bot.send_message(
                chat_id,
                format!("❌ AI plan check failed: {err_str}\nThe model pool may be temporarily unavailable — try again later."),
            )
            .await?;
            return Ok(());
        }
    };

    let mut lines = vec![format!(
        "🤖 <b>AI Plan Check</b> ({})",
        outcome.current_date
    )];

    // Market read (from the critic's JSON, if parseable).
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&outcome.trace.critic_output) {
        if let Some(read) = v.get("market_read").and_then(|m| m.as_str()) {
            let short = crate::format::truncate_chars(read, 300);
            lines.push(format!("\n📊 <b>Market:</b> {}", escape_html(&short)));
        }
    }

    lines.push(String::new());
    if outcome.actions.is_empty() {
        lines.push("(the AI returned no per-goal verdicts)".to_string());
    }
    for a in &outcome.actions {
        let verdict = match a.action.as_str() {
            "alert" => "🔔 alert",
            "done" => "✅ done",
            "propose" => "💡 propose",
            _ => "⏳ wait",
        };
        let goal_text = store.read(user_id, |p| {
            p.goals
                .iter()
                .find(|g| g.id == a.goal_id)
                .map(|g| g.text.clone())
                .unwrap_or_default()
        });
        lines.push(format!(
            "#{} {} — <b>{}</b>",
            a.goal_id,
            verdict,
            escape_html(&goal_text)
        ));
        let why = if !a.reason.trim().is_empty() {
            a.reason.trim()
        } else {
            a.message.trim()
        };
        if !why.is_empty() {
            let short = crate::format::truncate_chars(why, 300);
            lines.push(format!("   {}", escape_html(&short)));
        }
    }

    if !outcome.messages.is_empty() {
        lines.push(String::new());
        lines.push("📨 <b>Would notify you with:</b>".to_string());
        for m in outcome.messages.iter().take(5) {
            let short = crate::format::truncate_chars(m, 300);
            lines.push(format!("   • {}", escape_html(&short)));
        }
    }

    lines.push(String::new());
    lines.push("Nothing was auto-changed. Manage goals: <code>/goal list</code>".into());

    bot.send_message(chat_id, lines.join("\n"))
        .parse_mode(ParseMode::Html)
        .reply_markup(crate::bot::keyboards::plan_menu())
        .await?;
    Ok(())
}

async fn list_memos(
    bot: &Bot,
    chat_id: teloxide::types::ChatId,
    store: &Store,
    user_id: u64,
) -> anyhow::Result<()> {
    let memos = store.read(user_id, |p| p.memos.clone());
    if memos.is_empty() {
        bot.send_message(
            chat_id,
            "🧠 <b>No memos yet.</b>\nAdd one: <code>/memo always invest 100 USD at a time</code>",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }
    let mut lines = vec!["🧠 <b>Your memos (AI memory):</b>".to_string()];
    for m in &memos {
        lines.push(format!(
            "#{} · {} · <i>{}</i>",
            m.id,
            m.created_at.format("%d.%m"),
            escape_html(&m.text)
        ));
    }
    lines.push(String::new());
    lines.push("<code>/memo del N</code> — delete · <code>/memo clear</code> — clear all".into());
    bot.send_message(chat_id, lines.join("\n"))
        .parse_mode(ParseMode::Html)
        .reply_markup(crate::bot::keyboards::plan_menu())
        .await?;
    Ok(())
}

async fn list_goals(
    bot: &Bot,
    chat_id: teloxide::types::ChatId,
    store: &Store,
    user_id: u64,
) -> anyhow::Result<()> {
    let goals = store.read(user_id, |p| p.goals.clone());
    if goals.is_empty() {
        bot.send_message(
            chat_id,
            "🎯 <b>No goals yet.</b>\nExample: <code>/goal want to spend 400 USD on crypto this month</code>",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }
    let mut lines = vec!["🎯 <b>Your goals:</b>".to_string()];
    for g in &goals {
        let status = if g.done {
            "✅ done"
        } else if !g.active {
            "⏸ inactive"
        } else {
            "🟢 active"
        };
        let expires = fmt_expiry(g);
        let alerts = if g.alert_count > 0 {
            format!(" · 🔔 {} alerts", g.alert_count)
        } else {
            String::new()
        };
        lines.push(format!(
            "#{} {} — <i>{}</i>{}{}",
            g.id,
            status,
            escape_html(&g.text),
            expires,
            alerts
        ));
        if let Some(last) = g.progress_notes.last() {
            lines.push(format!(
                "   📌 {}: <i>{}</i>",
                last.at.format("%d.%m"),
                escape_html(&last.text)
            ));
        }
        if let Some(note) = ai_note(g) {
            let short = crate::format::truncate_chars(&note, 140);
            lines.push(format!("   🧠 <i>{}</i>", escape_html(&short)));
        }
    }
    lines.push(String::new());
    lines.push(
        "<code>/goal progress N text</code> — add progress · <code>/goal done N</code> — done · <code>/goal del N</code> — delete"
            .into(),
    );
    bot.send_message(chat_id, lines.join("\n"))
        .parse_mode(ParseMode::Html)
        .reply_markup(crate::bot::keyboards::plan_menu())
        .await?;
    Ok(())
}

/// Persist the user's memos to their dedicated SQLite table.
pub async fn persist_memos(store: &Store, sqlite: &SqliteStore, user_id: u64) {
    let memos = store.read(user_id, |p| p.memos.clone());
    sqlite.save_user_memos(user_id, &memos).await;
}

/// Persist the user's goals to their dedicated SQLite table.
pub async fn persist_goals(store: &Store, sqlite: &SqliteStore, user_id: u64) {
    let goals = store.read(user_id, |p| p.goals.clone());
    sqlite.save_user_goals(user_id, &goals).await;
}

/// Heuristic expiry for a goal written in free text:
/// - "месяц/month" → end of the current calendar month
/// - "к N / до N" (day of month) → that day this month (next month if passed)
/// - "20XX" year → end of that year
/// - "N дней/days / N недель/weeks / N месяцев/months" → relative duration
/// - otherwise → None (open-ended, the AI judges relevance itself)
/// Human-friendly goal-expiry label with an explicit year so a bare `31.08`
/// can never be mistaken for the current month, and goals whose deadline has
/// already passed are labelled as `expired` rather than a misleading `until`.
fn fmt_expiry(g: &Goal) -> String {
    match g.expires_at {
        Some(e) if e < Utc::now() && !g.done => format!(" · expired {}", e.format("%d.%m.%Y")),
        Some(e) => format!(" · until {}", e.format("%d.%m.%Y")),
        None => String::new(),
    }
}

/// Persisted AI reasoning, shown only while the goal is active (a finished or
/// paused goal keeps an old note that would otherwise read as if it were
/// current) and stamped with the day it was produced so relative wording like
/// "expires in 8 days" is anchored to a date.
fn ai_note(g: &Goal) -> Option<String> {
    if !g.active || g.done {
        return None;
    }
    let sum = g.ai_summary.as_ref()?;
    Some(match g.ai_summary_at {
        Some(at) => format!("({}) {}", at.format("%d.%m"), sum),
        None => sum.clone(),
    })
}

fn parse_goal_expiry(text: &str) -> Option<chrono::DateTime<Utc>> {
    let lower = text.to_lowercase();
    let now = Utc::now();

    // End of current month
    if lower.contains("месяц") || lower.contains("month") {
        let (y, m) = (now.year(), now.month());
        let last_day = days_in_month(y, m);
        return now
            .with_day(last_day)
            .and_then(|d| d.with_hour(23))
            .and_then(|d| d.with_minute(59))
            .and_then(|d| d.with_second(59));
    }

    // Explicit calendar dates: "31.08", "31.08.2026", "31/08", "31-08".
    // Must run before the bare day-of-month loop below, which would otherwise
    // read "до 31.08" as day 31 of the *current* month.
    if let Some(dt) = parse_dotted_date(&lower, now) {
        return Some(dt);
    }

    // "к 28-29" / "до 28" — a day of the current month (or the next month that
    // actually has that day: "до 31" stated in a 30-day month must not abort).
    for word in lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        if let Ok(day) = word.parse::<u32>() {
            if (1..=31).contains(&day)
                && (lower.contains(&format!("к {}", day)) || lower.contains(&format!("до {}", day)))
            {
                if let Some(target) = resolve_day_deadline(now, day) {
                    return Some(target);
                }
            }
        }
    }

    // "в 2027" / "к 2027" — end of that year
    for word in lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() == 4)
    {
        if let Ok(year) = word.parse::<i32>() {
            if (2024..=2040).contains(&year) {
                return now
                    .with_year(year)
                    .and_then(|d| d.with_month(12))
                    .and_then(|d| d.with_day(31))
                    .and_then(|d| d.with_hour(23))
                    .and_then(|d| d.with_minute(59))
                    .and_then(|d| d.with_second(59));
            }
        }
    }

    // Relative durations: "2 дня", "3 недели", "1 месяц", "30d", "2w"
    let num_before = |needle: &str| {
        lower
            .split(needle)
            .next()
            .and_then(|before| {
                before
                    .chars()
                    .rev()
                    .take_while(|c| c.is_ascii_digit() || c.is_whitespace())
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect::<String>()
                    .trim()
                    .parse::<i64>()
                    .ok()
            })
            .filter(|n| *n >= 1 && *n <= 365)
    };
    if lower.contains("недел") || lower.contains("week") {
        if let Some(n) = num_before("недел").or_else(|| num_before("week")) {
            return Some(now + Duration::weeks(n));
        }
    }
    if lower.contains("дней")
        || lower.contains("дня")
        || lower.contains("день")
        || lower.contains("day")
    {
        if let Some(n) = num_before("дней")
            .or_else(|| num_before("дня"))
            .or_else(|| num_before("день"))
            .or_else(|| num_before("day"))
        {
            return Some(now + Duration::days(n));
        }
    }
    for suffix in ["d", "w"] {
        if lower.ends_with(suffix) {
            let n = lower[..lower.len() - 1].trim().parse::<i64>().ok();
            if let Some(n) = n.filter(|n| *n >= 1 && *n <= 365) {
                return Some(
                    now + if suffix == "d" {
                        Duration::days(n)
                    } else {
                        Duration::weeks(n)
                    },
                );
            }
        }
    }

    None
}

/// Resolve "day N of this month, else the next month that actually has day N"
/// at 23:59:59. Never aborts the whole expiry parse when the current month is
/// too short for the requested day (e.g. "до 31" written in November).
fn resolve_day_deadline(now: chrono::DateTime<Utc>, day: u32) -> Option<chrono::DateTime<Utc>> {
    let mut year = now.year();
    let mut month = now.month();
    for _ in 0..12 {
        if day <= days_in_month(year, month) {
            let date = chrono::NaiveDate::from_ymd_opt(year, month, day)?;
            let utc = Utc.from_utc_datetime(&date.and_hms_opt(23, 59, 59)?);
            if utc > now {
                return Some(utc);
            }
        }
        let (ny, nm) = if month == 12 {
            (year + 1, 1)
        } else {
            (year, month + 1)
        };
        year = ny;
        month = nm;
    }
    None
}

/// Parse an explicit `dd.mm` / `dd.mm.yyyy` (also `dd/mm`, `dd-mm`) date.
/// A two-field date is only accepted when the day is unambiguous (`>= 13`) so
/// that decimal amounts like `1.5` or price levels are not mistaken for dates.
/// A bare `dd.mm` resolves to this year if still ahead, otherwise next year.
fn parse_dotted_date(text: &str, now: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
    let bytes = text.as_bytes();
    let mut best: Option<chrono::DateTime<Utc>> = None;
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len()
            && (bytes[i].is_ascii_digit() || matches!(bytes[i], b'.' | b'/' | b'-'))
        {
            i += 1;
        }
        let run = text[start..i].trim_matches(|c| c == '.' || c == '/' || c == '-');
        let parts: Vec<&str> = run
            .split(|c| c == '.' || c == '/' || c == '-')
            .filter(|p| !p.is_empty())
            .collect();
        if parts.len() != 2 && parts.len() != 3 {
            continue;
        }
        let (Ok(day), Ok(month)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>()) else {
            continue;
        };
        if !(1..=31).contains(&day) || !(1..=12).contains(&month) {
            continue;
        }
        let explicit_year = if parts.len() == 3 {
            match parts[2].parse::<i32>() {
                Ok(y) if (2020..=2100).contains(&y) => Some(y),
                _ => continue,
            }
        } else {
            // Two-field date must be unambiguous as a day-of-month.
            if day < 13 {
                continue;
            }
            None
        };
        let year = match explicit_year {
            Some(y) => y,
            None => {
                let date = chrono::NaiveDate::from_ymd_opt(now.year(), month, day)?;
                let utc = Utc.from_utc_datetime(&date.and_hms_opt(23, 59, 59)?);
                if utc > now {
                    now.year()
                } else {
                    now.year() + 1
                }
            }
        };
        let date = chrono::NaiveDate::from_ymd_opt(year, month, day)
            .or_else(|| chrono::NaiveDate::from_ymd_opt(year, month, days_in_month(year, month)))?;
        let utc = Utc.from_utc_datetime(&date.and_hms_opt(23, 59, 59)?);
        best = Some(match best {
            Some(prev) if prev <= utc => prev,
            _ => utc,
        });
    }
    best
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (y, m) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_next = chrono::NaiveDate::from_ymd_opt(y, m, 1)
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(year, 1, 1).unwrap());
    first_next.pred_opt().map(|d| d.day()).unwrap_or(28)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn expiry_end_of_month() {
        let e = parse_goal_expiry("хочу потратить 400 баксов в этом месяце");
        assert!(e.is_some(), "should expire at end of month");
        let now = Utc::now();
        assert!(e.unwrap() > now);
    }

    #[test]
    fn expiry_relative_days() {
        let e = parse_goal_expiry("вывести плюс через 3 дня").unwrap();
        let now = Utc::now();
        let days = (e - now).num_days();
        assert!(days >= 2 && days <= 4, "expected ~3 days, got {}", days);
    }

    #[test]
    fn expiry_open_ended() {
        assert!(parse_goal_expiry("хочу x2 по BTC").is_none());
    }

    #[test]
    fn expiry_year() {
        let e = parse_goal_expiry("снять на машину в 2027").unwrap();
        assert_eq!(e.year(), 2027);
        assert_eq!(e.month(), 12);
    }

    #[test]
    fn expiry_dotted_date_does_not_abort_or_pick_wrong_month() {
        // Regression: "до 31.08" used to be read as day 31 of the *current*
        // month and, when that day did not exist (30-day month), the whole
        // parse returned None -> goal silently became open-ended.
        let now = Utc::now();
        let dt = parse_dotted_date("накопить до 31.08", now).expect("date parsed");
        assert_eq!(dt.month(), 8);
        assert_eq!(dt.day(), 31);
        // Full ISO-ish date with explicit year.
        let dt = parse_dotted_date("до 31.08.2027", now).unwrap();
        assert_eq!(dt.year(), 2027);
        assert_eq!(dt.month(), 8);
        assert_eq!(dt.day(), 31);
    }

    #[test]
    fn expiry_decimal_is_not_a_date() {
        // "купить 1.5 ETH" must not be parsed as 1 May.
        assert!(parse_dotted_date("купить 1.5 eth", Utc::now()).is_none());
        assert!(parse_goal_expiry("купить 1.5 eth").is_none());
    }

    #[test]
    fn expiry_day_of_month_when_current_month_too_short() {
        // "до 31" in a 30-day month must roll to the next month, not vanish.
        let now = chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 11, 22, 12, 0, 0)
            .single()
            .unwrap();
        let target = resolve_day_deadline(now, 31).expect("resolved");
        assert_eq!(target.month(), 12);
        assert_eq!(target.day(), 31);
    }

    fn sample_goal(expires_at: Option<chrono::DateTime<Utc>>, done: bool) -> Goal {
        Goal {
            id: 1,
            text: "test".into(),
            created_at: Utc::now(),
            expires_at,
            active: true,
            last_alert_at: None,
            last_declined_at: None,
            ignore_ai_cancel: false,
            alert_count: 0,
            done,
            progress_notes: Vec::new(),
            ai_summary: None,
            ai_summary_at: None,
            sent_alerts: Vec::new(),
        }
    }

    #[test]
    fn expiry_label_is_unambiguous_and_marks_past() {
        // Open-ended goal: no suffix at all.
        assert_eq!(fmt_expiry(&sample_goal(None, false)), "");

        // Future deadline: full date incl. year, prefixed with `until`.
        let future = Utc::now() + Duration::days(30);
        let s = fmt_expiry(&sample_goal(Some(future), false));
        assert!(s.starts_with(" · until "), "got {s:?}");
        assert!(s.ends_with(&future.format("%d.%m.%Y").to_string()), "got {s:?}");

        // Past but not done: must read `expired`, not a misleading `until`.
        let past = Utc::now() - Duration::days(5);
        let s = fmt_expiry(&sample_goal(Some(past), false));
        assert!(s.starts_with(" · expired "), "got {s:?}");

        // Past but done: keep the neutral `until` wording (goal achieved).
        let s = fmt_expiry(&sample_goal(Some(past), true));
        assert!(s.starts_with(" · until "), "got {s:?}");
    }
}
