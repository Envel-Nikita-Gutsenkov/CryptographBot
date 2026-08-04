use std::sync::Arc;
use teloxide::{payloads::SendMessageSetters, prelude::*, types::ParseMode};
use crate::access::AccessControl;

/// /allow USER_ID [optional_note]
pub async fn handle_allow(
    bot: Bot, msg: Message, acl: Arc<AccessControl>, args: String,
) -> anyhow::Result<()> {
    let caller = msg.chat.id.0 as u64;
    if !acl.is_admin(caller) {
        bot.send_message(msg.chat.id, "⛔ Admin only").await?;
        return Ok(());
    }
    let parts: Vec<&str> = args.split_whitespace().collect();
    let Some(&target_str) = parts.first() else {
        bot.send_message(msg.chat.id, "Usage: /allow USER_ID or /allow @username").await?;
        return Ok(());
    };

    let (user_id, username) = if target_str.starts_with('@') {
        // Try to find by username in already-approved list
        match acl.find_by_username(target_str) {
            Some(id) => (id, Some(target_str.trim_start_matches('@').to_string())),
            None => {
                bot.send_message(msg.chat.id,
                    format!("Unknown username {target_str}\\. Use numeric ID if not yet approved\\.")
                ).parse_mode(ParseMode::MarkdownV2).await?;
                return Ok(());
            }
        }
    } else {
        match target_str.parse::<u64>() {
            Ok(id) => (id, None),
            Err(_) => {
                bot.send_message(msg.chat.id, "Invalid user ID — must be numeric").await?;
                return Ok(());
            }
        }
    };

    acl.approve(user_id, username.clone(), caller);
    bot.send_message(msg.chat.id,
        format!("✅ User `{user_id}`{} approved", username.map(|u| format!(" (@{u})")).unwrap_or_default())
    ).parse_mode(ParseMode::MarkdownV2).await?;
    Ok(())
}

/// /revoke USER_ID
pub async fn handle_revoke(
    bot: Bot, msg: Message, acl: Arc<AccessControl>, args: String,
) -> anyhow::Result<()> {
    let caller = msg.chat.id.0 as u64;
    if !acl.is_admin(caller) {
        bot.send_message(msg.chat.id, "⛔ Admin only").await?;
        return Ok(());
    }
    let target_str = args.trim();
    let user_id = if target_str.starts_with('@') {
        match acl.find_by_username(target_str) {
            Some(id) => id,
            None => {
                bot.send_message(msg.chat.id, format!("Unknown user: {target_str}")).await?;
                return Ok(());
            }
        }
    } else {
        match target_str.parse::<u64>() {
            Ok(id) => id,
            Err(_) => {
                bot.send_message(msg.chat.id, "Invalid user ID").await?;
                return Ok(());
            }
        }
    };

    if acl.is_admin(user_id) {
        bot.send_message(msg.chat.id, "❌ Cannot revoke admin access").await?;
        return Ok(());
    }

    let removed = acl.revoke(user_id);
    bot.send_message(msg.chat.id,
        if removed { format!("🗑 Access revoked for `{user_id}`") } else { format!("User `{user_id}` was not approved") }
    ).parse_mode(ParseMode::MarkdownV2).await?;
    Ok(())
}

/// /acl — list approved users
pub async fn handle_acl_list(
    bot: Bot, msg: Message, acl: Arc<AccessControl>,
) -> anyhow::Result<()> {
    if !acl.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Admin only").await?;
        return Ok(());
    }
    let list = acl.approved_list();
    if list.is_empty() {
        bot.send_message(msg.chat.id, "No approved users\\.").parse_mode(ParseMode::MarkdownV2).await?;
        return Ok(());
    }
    let mut text = "*Approved users:*\n\n".to_string();
    for u in &list {
        let uname = u.username.as_deref().map(|n| format!(" @{n}")).unwrap_or_default();
        text.push_str(&format!(
            "`{}`{}  — approved by `{}` at {}\n",
            u.id, uname, u.approved_by,
            u.approved_at.format("%Y\\-%-m\\-%-d")
        ));
    }
    bot.send_message(msg.chat.id, text).parse_mode(ParseMode::MarkdownV2).await?;
    Ok(())
}
