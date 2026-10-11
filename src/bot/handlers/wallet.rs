use crate::{
    bot::{keyboards, state::State},
    config::Config,
    crypto::BlockchainClient,
    db::Store,
    models::{Network, Wallet, WalletKind},
};
use chrono::Utc;
use std::sync::Arc;
use teloxide::{
    dispatching::dialogue::InMemStorage, payloads::SendMessageSetters, prelude::*, types::ParseMode,
};

type MyDialogue = Dialogue<State, InMemStorage<State>>;

pub async fn handle_wallet_step1(
    bot: Bot,
    msg: Message,
    _dialogue: MyDialogue,
) -> anyhow::Result<()> {
    bot.send_message(
        msg.chat.id,
        "Please choose the wallet type from the inline keyboard buttons.",
    )
    .reply_markup(keyboards::cancel_only())
    .await?;
    Ok(())
}

pub async fn handle_wallet_step2(
    bot: Bot,
    msg: Message,
    dialogue: MyDialogue,
    store: Arc<Store>,
) -> anyhow::Result<()> {
    let text = msg.text().unwrap_or("").trim();
    if text.is_empty() {
        bot.send_message(
            msg.chat.id,
            "❌ Name cannot be empty. Please enter a wallet name:",
        )
        .reply_markup(keyboards::cancel_only())
        .await?;
        return Ok(());
    }

    let state = dialogue.get().await?;
    if let Some(State::WalletStep2 { kind, .. }) = state {
        let parts: Vec<&str> = text.split_whitespace().collect();
        let name = parts[0].to_string();
        let symbol = parts.get(1).map(|s| s.to_uppercase());
        let address = parts.get(2).map(|s| s.to_string());

        let network = symbol.as_ref().and_then(|sym| sym.parse::<Network>().ok());
        let user_id = msg.chat.id.0 as u64;

        let id = store.write(user_id, |p| {
            if let Some(w) = p
                .wallets
                .iter()
                .find(|w| w.name.eq_ignore_ascii_case(&name))
            {
                return w.id;
            }
            let id = p.alloc_wallet_id();
            p.wallets.push(Wallet {
                id,
                name: name.clone(),
                kind: kind.clone(),
                address: address.clone(),
                network: network.clone(),
                exchange: if kind == WalletKind::Exchange {
                    Some(name.clone())
                } else {
                    None
                },
                monitored: address.is_some(),
                last_balance: std::collections::HashMap::new(),
                last_checked: None,
            });
            id
        });

        dialogue.exit().await?;

        let mon = if address.is_some() {
            " (auto-monitoring ON)"
        } else {
            ""
        };
        bot.send_message(
            msg.chat.id,
            format!(
                "✅ Wallet `{}` \\({}\\) added \\#{}{}",
                crate::format::esc(&name),
                kind.label(),
                id,
                mon
            ),
        )
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(keyboards::wallets_footer())
        .await?;

        crate::bot::handlers::portfolio::auto_backup(&bot, &store, user_id).await;
    } else {
        dialogue.exit().await?;
        bot.send_message(msg.chat.id, "Dialogue state lost. Starting over.")
            .await?;
    }

    Ok(())
}

/// /wallet add cold:Ledger BTC bc1q...
pub async fn handle_wallet_add(
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
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.len() < 2 {
        bot.send_message(
            msg.chat.id,
            "Usage: `/wallet add [cold:|hot:|exchange:]NAME SYM [ADDRESS]`\n\
             Example: `/wallet add cold:Ledger BTC bc1q...`",
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        return Ok(());
    }
    let (kind, name) = parse_wallet_spec(parts[0]);
    let symbol = parts[1].to_uppercase();
    let address = parts.get(2).map(|s| s.to_string());
    let network = symbol.parse::<Network>().ok();
    let user_id = msg.chat.id.0 as u64;

    let id = store.write(user_id, |p| {
        // Don't create duplicate
        if let Some(w) = p
            .wallets
            .iter()
            .find(|w| w.name.eq_ignore_ascii_case(&name))
        {
            return w.id;
        }
        let id = p.alloc_wallet_id();
        p.wallets.push(Wallet {
            id,
            name: name.clone(),
            kind: kind.clone(),
            address: address.clone(),
            network: network.clone(),
            exchange: if kind == WalletKind::Exchange {
                Some(name.clone())
            } else {
                None
            },
            monitored: address.is_some(),
            last_balance: std::collections::HashMap::new(),
            last_checked: None,
        });
        id
    });
    let mon = if address.is_some() {
        " (auto-monitoring ON)"
    } else {
        ""
    };
    bot.send_message(
        msg.chat.id,
        format!(
            "✅ Wallet `{}` \\({}\\) added \\#{}{}",
            name,
            kind.label(),
            id,
            mon
        ),
    )
    .parse_mode(ParseMode::MarkdownV2)
    .reply_markup(keyboards::wallets_footer())
    .await?;
    crate::bot::handlers::portfolio::auto_backup(&bot, &store, user_id).await;
    Ok(())
}

pub async fn handle_wallet_list(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }
    let user_id = msg.chat.id.0 as u64;
    let wallets = store.read(user_id, |p| p.wallets.clone());
    if wallets.is_empty() {
        bot.send_message(msg.chat.id, "No wallets\\. Add with `/wallet add`")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(keyboards::wallets_footer())
            .await?;
        return Ok(());
    }
    let mut text = "*Wallets*\n\n".to_string();
    for w in &wallets {
        let addr = w.address.as_deref().unwrap_or("—");
        let mon = if w.monitored { "👁" } else { "○" };
        text.push_str(&format!(
            "{} {} *{}* \\#{}  {}\n`{}`\n\n",
            w.kind.emoji(),
            mon,
            crate::format::esc(&w.name),
            w.id,
            w.kind.label(),
            crate::format::esc(&addr[..addr.len().min(20)])
        ));
    }
    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(keyboards::wallets_footer())
        .await?;
    Ok(())
}

pub async fn handle_wallet_refresh(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
    blockchain: Arc<BlockchainClient>,
    wallet_id: Option<u64>,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }
    let user_id = msg.chat.id.0 as u64;

    let wallets = store.read(user_id, |p| {
        p.wallets
            .iter()
            .filter(|w| wallet_id.map(|id| w.id == id).unwrap_or(true))
            .filter(|w| w.monitored && w.address.is_some())
            .cloned()
            .collect::<Vec<_>>()
    });

    if wallets.is_empty() {
        bot.send_message(msg.chat.id, "No monitorable wallets found")
            .await?;
        return Ok(());
    }

    bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing)
        .await?;
    let mut lines = Vec::new();

    for w in wallets {
        let addr = w.address.as_deref().unwrap_or("");
        let net = w.network.as_ref().cloned().unwrap_or(Network::Ethereum);
        match blockchain.get_balance(addr, &net).await {
            Ok(bal) => {
                store.write(user_id, |p| {
                    if let Some(wallet) = p.wallets.iter_mut().find(|x| x.id == w.id) {
                        wallet.last_balance.insert(w.name.clone(), bal);
                        wallet.last_checked = Some(Utc::now());
                    }
                });
                lines.push(format!(
                    "{} *{}*: `{}`",
                    w.kind.emoji(),
                    crate::format::esc(&w.name),
                    crate::format::esc(&format!("{:.8}", bal))
                ));
            }
            Err(e) => lines.push(format!(
                "{} *{}*: ❌ {}",
                w.kind.emoji(),
                crate::format::esc(&w.name),
                crate::format::esc(&e.to_string())
            )),
        }
    }

    bot.send_message(
        msg.chat.id,
        format!("🔄 *Balances*\n\n{}", lines.join("\n")),
    )
    .parse_mode(ParseMode::MarkdownV2)
    .await?;
    Ok(())
}

pub async fn handle_wallet_delete(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
    id: u64,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }
    let removed = store.write(msg.chat.id.0 as u64, |p| {
        let before = p.wallets.len();
        p.wallets.retain(|w| w.id != id);
        p.wallets.len() < before
    });
    bot.send_message(
        msg.chat.id,
        if removed {
            format!("🗑 Wallet #{id} removed")
        } else {
            format!("❌ Wallet #{id} not found")
        },
    )
    .await?;
    if removed {
        crate::bot::handlers::portfolio::auto_backup(&bot, &store, msg.chat.id.0 as u64).await;
    }
    Ok(())
}

fn parse_wallet_spec(s: &str) -> (WalletKind, String) {
    if let Some(n) = s.strip_prefix("cold:") {
        (WalletKind::Cold, n.into())
    } else if let Some(n) = s.strip_prefix("hot:") {
        (WalletKind::Hot, n.into())
    } else if let Some(n) = s.strip_prefix("exchange:") {
        (WalletKind::Exchange, n.into())
    } else {
        (WalletKind::Hot, s.into())
    }
}
