use crate::{crypto::CryptoClient, format};
use std::sync::Arc;
use teloxide::prelude::*;

pub async fn handle_fng_cmd(
    bot: Bot,
    msg: Message,
    crypto: Arc<CryptoClient>,
) -> anyhow::Result<()> {
    bot.send_message(msg.chat.id, "📊 Вычисляю Индекс Страха и Жадности...")
        .reply_to_message_id(msg.id)
        .await?;

    match crypto.get_fng().await {
        Ok(fng) => {
            let msg_text = format::format_fng(&fng);
            bot.send_message(msg.chat.id, msg_text)
                .parse_mode(teloxide::types::ParseMode::Html)
                .await?;
        }
        Err(e) => {
            bot.send_message(
                msg.chat.id,
                format!("❌ Не удалось получить данные FNG: {}", e),
            )
            .await?;
        }
    }

    Ok(())
}
