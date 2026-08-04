use std::sync::Arc;
use teloxide::{prelude::*, types::ParseMode};
use crate::{config::Config, crypto::ai::fetch_news_for_symbol};

#[allow(deprecated)]
pub async fn handle_news_cmd(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let parts: Vec<&str> = args.split_whitespace().collect();
    let (symbol, limit) = parse_news_args(&parts);

    let loading = bot.send_message(msg.chat.id, "📰 Fetching news…").await?;
    let result = fetch_news_for_symbol(&symbol, &symbol, limit).await;
    let _ = bot.delete_message(msg.chat.id, loading.id).await;

    send_news(&bot, msg.chat.id, &symbol, result).await
}

#[allow(deprecated)]
pub async fn handle_news_callback(
    bot: Bot,
    chat_id: ChatId,
    symbol: String,
    _config: Arc<Config>,
) -> anyhow::Result<()> {
    let loading = bot.send_message(chat_id, "📰 Fetching news…").await?;
    let query = if symbol.to_lowercase() == "general" { "general".to_string() } else { symbol.clone() };
    let result = fetch_news_for_symbol(&query, &symbol, 5).await;
    let _ = bot.delete_message(chat_id, loading.id).await;
    send_news(&bot, chat_id, &symbol, result).await
}

fn parse_news_args(parts: &[&str]) -> (String, usize) {
    match parts {
        [] => ("general".into(), 5),
        [s] => {
            if let Ok(n) = s.parse::<usize>() {
                ("general".into(), n)
            } else {
                (s.to_uppercase(), 5)
            }
        }
        [s, n, ..] => {
            let sym = s.to_uppercase();
            let lim = n.parse().unwrap_or(5);
            (sym, lim)
        }
    }
}

async fn send_news(bot: &Bot, chat_id: ChatId, symbol: &str, result: anyhow::Result<String>) -> anyhow::Result<()> {
    match result {
        Ok(news) => {
            let display = if symbol.to_lowercase() == "general" { "General Market" } else { symbol };
            let header = format!("📰 *Recent News for {}*:\n\n", crate::format::esc(display));
            bot.send_message(chat_id, format!("{}{}", header, crate::format::esc(&news)))
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
        }
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Failed to fetch news: {e}")).await?;
        }
    }
    Ok(())
}
