use crate::{access::AccessControl, db::Store};
use std::sync::Arc;
use teloxide::{
    net::Download,
    prelude::*,
    types::{InputFile, ParseMode},
};

pub async fn handle_backup(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    acl: Arc<AccessControl>,
) -> anyhow::Result<()> {
    if !acl.is_allowed(msg.chat.id.0 as u64) {
        return Ok(());
    }
    let user_id = msg.chat.id.0 as u64;
    match store.export_backup(user_id) {
        Ok(data) => {
            bot.send_document(
                msg.chat.id,
                InputFile::memory(data).file_name("cryptograph_backup.json"),
            )
            .caption("📋 Portfolio backup.\nSend this file back to /restore.")
            .await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("Backup error: {e}"))
                .await?;
        }
    }
    Ok(())
}

pub async fn handle_restore_document(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    acl: Arc<AccessControl>,
) -> anyhow::Result<()> {
    if !acl.is_allowed(msg.chat.id.0 as u64) {
        return Ok(());
    }
    let doc = match msg.document() {
        Some(d) => d,
        None => return Ok(()),
    };

    let file = bot.get_file(&doc.file.id).await?;
    let mut buf = Vec::new();
    bot.download_file(&file.path, &mut buf).await?;

    let user_id = msg.chat.id.0 as u64;
    match store.decode_backup(&buf) {
        Err(e) => {
            bot.send_message(msg.chat.id, format!("❌ Invalid backup: {e}"))
                .await?;
        }
        Ok(portfolio) => {
            let info = format!(
                "📦 *Backup loaded:*\n\
                 {} purchases  \\|  {} wallets\n\
                 {} alerts  \\|  {} transfers\n\n\
                 ✅ Portfolio restored\\.",
                portfolio.purchases.len(),
                portfolio.wallets.len(),
                portfolio.alerts.len(),
                portfolio.transfers.len(),
            );
            store.restore(user_id, portfolio);
            bot.send_message(msg.chat.id, info)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            auto_backup_plain(&bot, &store, user_id).await;
        }
    }
    Ok(())
}

pub async fn auto_backup_plain(bot: &Bot, store: &Store, user_id: u64) {
    let should = store.read(user_id, |p| p.settings.backup_on_change);
    if !should {
        return;
    }
    if let Ok(data) = store.export_backup(user_id) {
        let _ = bot
            .send_document(
                ChatId(user_id as i64),
                InputFile::memory(data).file_name("cryptograph_backup.json"),
            )
            .caption("🔄 Auto-backup")
            .await;
    }
}
