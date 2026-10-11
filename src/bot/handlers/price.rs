use std::sync::Arc;

use teloxide::{payloads::SendMessageSetters, prelude::*, types::ParseMode};

use crate::{
    bot::keyboards,
    config::Config,
    crypto::CryptoClient,
    db::Store,
    error::BotError,
    format::{fmt_global_metrics, fmt_quote_card, fmt_top_row},
};

pub async fn handle_price(
    bot: Bot,
    msg: Message,
    user_id: u64,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    tracing::info!("handle_price called: user_id={}, args={:?}", user_id, args);
    guard_admin(&bot, &msg, user_id, &config).await?;

    let symbols = if args.trim().is_empty() {
        store.read(user_id, |p| p.settings.default_watchlist.clone())
    } else {
        args.split_whitespace().map(|s| s.to_uppercase()).collect()
    };

    if symbols.is_empty() {
        bot.send_message(
            msg.chat.id,
            "No symbols configured\\. Use /watchlist add BTC",
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        return Ok(());
    }

    let typing = bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing);
    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();

    let (_, quotes) = tokio::join!(typing, cmc.get_quotes(&sym_refs));

    match quotes {
        Err(BotError::RateLimited) => {
            bot.send_message(msg.chat.id, "⚠️ Rate limit hit — please wait a moment")
                .await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("Error: {e}")).await?;
        }
        Ok(quotes) => {
            for q in &quotes {
                let keyboard = teloxide::types::InlineKeyboardMarkup::new(vec![vec![
                    teloxide::types::InlineKeyboardButton::callback(
                        "📈 View Chart",
                        &format!("chart:{}:1d", q.symbol),
                    ),
                ]]);
                bot.send_message(msg.chat.id, fmt_quote_card(q))
                    .parse_mode(ParseMode::MarkdownV2)
                    .reply_markup(keyboard)
                    .await?;
            }
        }
    }
    Ok(())
}

pub async fn handle_top(
    bot: Bot,
    msg: Message,
    user_id: u64,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    guard_admin(&bot, &msg, user_id, &config).await?;

    let limit: u32 = args.trim().parse().unwrap_or(10).clamp(5, 50);

    bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing)
        .await?;

    let (global, top) = tokio::join!(cmc.get_global_metrics(), cmc.get_top(limit));

    let mut text = String::new();

    if let Ok(g) = global {
        text.push_str(&fmt_global_metrics(&g));
        text.push_str("\n\n");
    }

    match top {
        Ok(quotes) => {
            text.push_str(&format!("📋 *Top {} by Volume 24h*\n\n", quotes.len()));
            for (i, q) in quotes.iter().enumerate() {
                text.push_str(&fmt_top_row(i + 1, q));
                text.push('\n');
            }
            bot.send_message(msg.chat.id, text)
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(keyboards::top_menu())
                .await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("Error fetching top: {e}"))
                .await?;
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub async fn handle_top_chart(
    bot: Bot,
    msg: Message,
    user_id: u64,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    limit: u32,
) -> anyhow::Result<()> {
    guard_admin(&bot, &msg, user_id, &config).await?;

    bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::UploadPhoto)
        .await?;

    let quotes = cmc.get_top(limit).await?;
    let chart = crate::crypto::charts::ChartRenderer::top_movers(&quotes).await?;

    bot.send_photo(msg.chat.id, teloxide::types::InputFile::memory(chart))
        .caption(format!("Top {} by Volume 24h", quotes.len()))
        .await?;

    Ok(())
}

async fn guard_admin(
    bot: &Bot,
    msg: &Message,
    user_id: u64,
    config: &Config,
) -> anyhow::Result<()> {
    if !config.is_admin(user_id) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        anyhow::bail!(BotError::Unauthorized);
    }
    Ok(())
}
