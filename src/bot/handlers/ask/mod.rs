//! `/ask` — the deep market-analysis pipeline (Telegram + CLI entry points).
//!
//! Split into logical modules (each < 600 lines):
//! * `pipeline.rs` — `execute_ask_pipeline` orchestration
//! * `market.rs` — live market data + local memory context blocks
//! * `news.rs` — news context (cached summaries + on-demand MoE pipeline)
//! * `search.rs` — web-search planner / dispatch / summarize helpers
//! * `reflect.rs` — agentic reflection (one targeted refinement pass)

mod market;
mod news;
mod pipeline;
mod reflect;
mod search;

pub use pipeline::execute_ask_pipeline;
pub use search::run_search_cli;

use std::sync::Arc;
use teloxide::{prelude::*, types::ParseMode};

use crate::{
    config::Config,
    crypto::CryptoClient,
    db::{SqliteStore, Store},
};

pub async fn handle_ask_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    sqlite: Arc<crate::db::SqliteStore>,
    args: String,
) -> anyhow::Result<()> {
    let question = args.trim().to_string();

    if question.is_empty() {
        bot.send_message(
            msg.chat.id,
            "💬 <b>AI Chat</b>\n\n\
             Ask anything about your portfolio or the market. Examples:\n\
             • <code>/ask why is ETH dropping?</code>\n\
             • <code>/ask should I buy more SOL now?</code>\n\
             • <code>/ask what is my average BTC entry price?</code>",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);

    let loading = bot
        .send_message(msg.chat.id, "🧠 Thinking…")
        .parse_mode(ParseMode::Html)
        .await?;

    // Run the whole pipeline in a background task so a long /ask (up to 180s)
    // never blocks the dispatcher: other commands (/b, /price, …) keep working
    // while the answer is being produced. Progress + the final answer are sent
    // from the spawned task via the bot handle.
    let bot_task = bot.clone();
    let chat_id = msg.chat.id;
    let loading_id = loading.id;
    let reply_context = reply_context_from(&msg);

    tokio::spawn(async move {
        // Mark this as a user-facing request for its whole lifetime: background
        // workers pause before spending provider quotas while it runs, so a
        // volatility sweep cannot drain the free tiers and leave the answer
        // empty ("лимиты исчерпаны" during a price drop).
        let _interactive = crate::crypto::ai::pools::InteractiveGuard::acquire();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let progress_bot = bot_task.clone();
        let progress_chat = chat_id;
        let progress_msg = loading_id;
        // Update the loading message with progress as the pipeline advances.
        let _ui_task = tokio::spawn(async move {
            while let Some(status) = rx.recv().await {
                let _ = progress_bot
                    .edit_message_text(progress_chat, progress_msg, status)
                    .await;
            }
        });

        let result = match tokio::time::timeout(
            std::time::Duration::from_secs(180),
            execute_ask_pipeline(
                &question,
                user_id,
                store,
                cmc,
                config.clone(),
                sqlite,
                Some(tx),
                reply_context,
            ),
        )
        .await
        {
            Ok(res) => res,
            Err(_) => Err(anyhow::anyhow!(
                "Ask pipeline timed out after 180s — please try again."
            )),
        };

        let _ = bot_task.delete_message(chat_id, loading_id).await;

        match result {
            Ok(answer) => {
                if let Err(e) = crate::bot::handlers::ai::send_rich_message_via_api(
                    &bot_task,
                    config.clone(),
                    chat_id,
                    answer,
                )
                .await
                {
                    let err_str = crate::format::redact_secrets(&e.to_string());
                    let safe_err = if err_str.len() > 1000 {
                        format!("{}...", &err_str.chars().take(1000).collect::<String>())
                    } else {
                        err_str
                    };
                    bot_task
                        .send_message(chat_id, format!("❌ AI failed to respond: {safe_err}"))
                        .await
                        .ok();
                }
            }
            Err(e) => {
                let err_str = crate::format::redact_secrets(&e.to_string());
                let safe_err = if err_str.len() > 1000 {
                    format!("{}...", &err_str.chars().take(1000).collect::<String>())
                } else {
                    err_str
                };
                // Show what actually happened to the pool instead of claiming
                // "the rest of the providers did not answer": models are skipped
                // by benches, day-deaths and missing keys long before any request
                // is made, so the old wording was simply false and made /aihealth
                // look broken. pool_status_report() prints the real state.
                let mut msg = format!(
                    "❌ AI не дал ответа: {safe_err}\n\nСостояние пула на момент ошибки:\n"
                );
                for line in crate::crypto::ai::pools::pool_status_report() {
                    msg.push_str(&line);
                    msg.push('\n');
                }
                msg.push_str(
                    "\nПолная картина и перепроверка: /aihealth → 🔄 Recheck now.\n\
                     /forecast, /dip, /sniper и /portfolio работают без AI.",
                );
                bot_task.send_message(chat_id, msg).await.ok();
            }
        }
    });

    Ok(())
}

/// Build the dialogue context from the message the user replied to.
///
/// Replying to the bot's card, to your own earlier message, or to a photo /
/// document / voice note must all continue the thread — a reply that carries
/// only media used to be dropped silently (`text().or_else(caption())?`),
/// so "а это что?" replied to a chart produced a context-free answer. Media
/// replies now degrade to a short marker (`[photo]`, `[document: name]`, …)
/// plus any caption.
pub fn reply_context_from(msg: &Message) -> Option<String> {
    let replied = msg.reply_to_message()?;
    let body = replied
        .text()
        .map(|t| t.to_string())
        .or_else(|| replied.caption().map(|c| c.to_string()))
        .unwrap_or_else(|| media_marker(replied));
    if body.trim().is_empty() {
        return None;
    }
    // Replying to your own message is just as valid a follow-up as replying to
    // the bot's card — mark the author so the model knows whose words it is
    // continuing (needed because the bot's cards are long and look like data).
    let from_bot = replied.from().map(|u| u.is_bot).unwrap_or(false);
    compose_reply_context(from_bot, &body)
}

/// Pure part of [`reply_context_from`] — testable without constructing a
/// `teloxide::types::Message`.
fn compose_reply_context(from_bot: bool, body: &str) -> Option<String> {
    if body.trim().is_empty() {
        return None;
    }
    let who = if from_bot { "assistant (bot)" } else { "user" };
    Some(truncate_chars(
        &format!("[{who}]: {body}"),
        2000,
        "... (truncated)",
    ))
}

/// Short textual stand-in for a reply that has no text and no caption.
fn media_marker(m: &Message) -> String {
    if m.photo().is_some() {
        return "[photo]".to_string();
    }
    if let Some(doc) = m.document() {
        return match &doc.file_name {
            Some(name) => format!("[document: {name}]"),
            None => "[document]".to_string(),
        };
    }
    if m.voice().is_some() {
        return "[voice message]".to_string();
    }
    if m.video_note().is_some() {
        return "[video note]".to_string();
    }
    if m.video().is_some() {
        return "[video]".to_string();
    }
    if m.audio().is_some() {
        return "[audio]".to_string();
    }
    if m.animation().is_some() {
        return "[animation]".to_string();
    }
    if m.sticker().is_some() {
        return "[sticker]".to_string();
    }
    if m.contact().is_some() {
        return "[contact]".to_string();
    }
    if m.location().is_some() {
        return "[location]".to_string();
    }
    "[message without text]".to_string()
}

fn truncate_chars(s: &str, max: usize, suffix: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let mut out: String = chars.into_iter().take(max).collect();
    out.push_str(suffix);
    out
}

/// CLI reproduction of the full `/ask` pipeline (`cargo run -- ask <user|demo> "q"`).
///
/// Enables `CRYPTOGRAPH_DEBUG_PROMPTS` so every stage is printed: the system
/// prompt, every web search the models request (and its results), both
/// generator drafts, the critic, and finally the answer. This is how cases like
/// "AI mixed old discontinued Trezor models with new ones" get reproduced and
/// diagnosed — you can see exactly what data the model was given and where the
/// mix-up entered.
pub async fn run_cli_ask(
    user_id: u64,
    question: &str,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    sqlite: Arc<SqliteStore>,
    reply_context: Option<String>,
) -> anyhow::Result<()> {
    std::env::set_var("CRYPTOGRAPH_DEBUG_PROMPTS", "1");

    println!("=== /ask PIPELINE REPLAY ===");
    println!("User: {}", user_id);
    println!("Question: {}", question);
    if let Some(r) = reply_context.as_deref() {
        println!("Replying to: {}", r);
    }
    println!();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let progress_task = tokio::spawn(async move {
        while let Some(status) = rx.recv().await {
            println!("⏳ {}", status);
        }
    });

    let result = match tokio::time::timeout(
        std::time::Duration::from_secs(180),
        execute_ask_pipeline(
            question,
            user_id,
            store,
            cmc,
            config,
            sqlite,
            Some(tx),
            reply_context,
        ),
    )
    .await
    {
        Ok(res) => res,
        Err(_) => Err(anyhow::anyhow!(
            "Ask pipeline timed out after 180s — please try again."
        )),
    };

    drop(progress_task); // tx is dropped with the pipeline; task ends on its own

    match result {
        Ok(answer) => {
            println!("\n=== FINAL ANSWER ===\n{}", answer);
        }
        Err(e) => {
            println!("\n=== PIPELINE FAILED ===\n{:#}", e);
        }
    }
    Ok(())
}

/// Render the last N /ask exchanges as a compact dialogue block for the prompt.
/// Oldest exchanges are dropped first; each message is truncated to 500 chars.
pub fn format_ask_history(history: &[(String, String)]) -> String {
    const MAX_EXCHANGES: usize = 6;
    const MAX_CHARS: usize = 500;
    let start = history.len().saturating_sub(MAX_EXCHANGES);
    let trunc = |s: &str| -> String {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() > MAX_CHARS {
            let mut t: String = chars.into_iter().take(MAX_CHARS).collect();
            t.push_str("... (truncated)");
            t
        } else {
            s.to_string()
        }
    };
    let mut out = String::new();
    for (q, a) in &history[start..] {
        out.push_str(&format!("User: {}\nAssistant: {}\n", trunc(q), trunc(a)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_history_format_bounds_and_order() {
        let h: Vec<(String, String)> = (0..10)
            .map(|i| (format!("q{i}"), format!("a{i}")))
            .collect();
        let s = format_ask_history(&h);
        // Only the last 6 exchanges survive, oldest dropped first.
        assert!(!s.contains("q0"), "oldest exchange must be dropped");
        assert!(s.contains("q4"));
        assert!(s.contains("q9"));
        assert!(s.find("q4").unwrap() < s.find("q9").unwrap());
        // Long messages are truncated.
        let long = "x".repeat(700);
        let s2 = format_ask_history(&[("q".to_string(), long)]);
        assert!(s2.contains("... (truncated)"));
        assert!(s2.len() < 700 + 200);
        // Empty history -> empty block.
        assert!(format_ask_history(&[]).is_empty());
    }

    #[test]
    fn reply_context_tags_author_and_keeps_body() {
        let bot = compose_reply_context(true, "📈 BTC forecast · 77712 USD").unwrap();
        assert!(bot.starts_with("[assistant (bot)]: "));
        assert!(bot.contains("BTC forecast"));

        let user = compose_reply_context(false, "я купил на 60к").unwrap();
        assert!(user.starts_with("[user]: "));
        assert!(user.contains("60к"));
    }

    #[test]
    fn reply_context_truncates_long_bodies() {
        let long = "x".repeat(5000);
        let ctx = compose_reply_context(true, &long).unwrap();
        assert!(ctx.ends_with("... (truncated)"));
        // 2000 chars of body + "[assistant (bot)]: " prefix + suffix.
        assert!(ctx.chars().count() < 2000 + 64);
    }

    #[test]
    fn reply_context_rejects_blank_body() {
        assert!(compose_reply_context(true, "").is_none());
        assert!(compose_reply_context(false, "   \n\t").is_none());
    }
}
