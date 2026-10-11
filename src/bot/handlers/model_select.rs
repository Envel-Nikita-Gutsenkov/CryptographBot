use crate::{config::Config, crypto::ai::providers::available_models, db::Store};
use std::sync::Arc;
use teloxide::{prelude::*, types::ParseMode};

pub async fn handle_model_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let parts: Vec<&str> = args.trim().splitn(2, ' ').collect();
    match parts.as_slice() {
        ["list"] | [] => show_model_list(&bot, msg.chat.id).await,
        ["set", rest] => set_model(&bot, msg.chat.id, store, rest.trim()).await,
        _ => show_model_list(&bot, msg.chat.id).await,
    }
}

async fn show_model_list(bot: &Bot, chat_id: ChatId) -> anyhow::Result<()> {
    let models = available_models();
    let mut lines = vec!["🤖 *Available AI Models*\n".to_string()];
    for (i, m) in models.iter().enumerate() {
        lines.push(format!("{}\\. `{}` — {}", i + 1, m.id, m.display));
    }
    lines.push("\n_Use_ `/model set <id_or_number>` _to select\\._\n_Use_ `/model set default` _to reset\\._".to_string());
    bot.send_message(chat_id, lines.join("\n"))
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
    Ok(())
}

async fn set_model(
    bot: &Bot,
    chat_id: ChatId,
    store: Arc<Store>,
    input: &str,
) -> anyhow::Result<()> {
    let user_id = chat_id.0 as u64;
    let models = available_models();

    if input == "default" || input == "reset" {
        store.write(user_id, |p| p.settings.ai_model_override = None);
        crate::bot::handlers::portfolio::auto_backup(bot, &store, user_id).await;
        bot.send_message(
            chat_id,
            "✅ AI model reset to default chain \\(Gemini Pro → Flash → Mistral\\)\\.",
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        return Ok(());
    }

    // Try numeric index first
    let model = if let Ok(n) = input.parse::<usize>() {
        models.get(n.saturating_sub(1))
    } else {
        models.iter().find(|m| m.id == input)
    };

    match model {
        Some(m) => {
            store.write(user_id, |p| {
                p.settings.ai_model_override = Some(m.id.to_string())
            });
            crate::bot::handlers::portfolio::auto_backup(bot, &store, user_id).await;
            bot.send_message(
                chat_id,
                format!("✅ AI model set to *{}* \\(`{}`\\)\\.", m.display, m.id),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        }
        None => {
            bot.send_message(
                chat_id,
                format!("❌ Unknown model `{input}`\\. Use `/model list` to see options\\."),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        }
    }
    Ok(())
}
