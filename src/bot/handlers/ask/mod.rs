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
    let reply_context = msg.reply_to_message().and_then(|m| m.text()).map(|s| {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() > 2000 {
            let mut t: String = chars.into_iter().take(2000).collect();
            t.push_str("... (truncated)");
            t
        } else {
            s.to_string()
        }
    });

    tokio::spawn(async move {
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
                bot_task
                    .send_message(chat_id, format!("❌ AI pipeline failed: {safe_err}"))
                    .await
                    .ok();
            }
        }
    });

    Ok(())
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
) -> anyhow::Result<()> {
    std::env::set_var("CRYPTOGRAPH_DEBUG_PROMPTS", "1");

    println!("\n=== /ask PIPELINE REPLAY ===");
    println!("User: {}", user_id);
    println!("Question: {}\n", question);

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
            None,
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
}
