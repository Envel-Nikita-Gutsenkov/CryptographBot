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

        if let Some(sym) = &symbol {
            if !is_plausible_symbol(sym) {
                bot.send_message(
                    msg.chat.id,
                    format!(
                        "❌ `{}` is not a ticker. Send a symbol like BTC/ETH, or just the name to skip:",
                        crate::format::esc(sym)
                    ),
                )
                .parse_mode(ParseMode::MarkdownV2)
                .reply_markup(keyboards::cancel_only())
                .await?;
                return Ok(());
            }
        }
        if let (Some(sym), Some(addr)) = (&symbol, &address) {
            if let Some(reason) = address_error(sym, addr) {
                bot.send_message(
                    msg.chat.id,
                    format!("❌ {reason}. Send again or just the name to skip:"),
                )
                .reply_markup(keyboards::cancel_only())
                .await?;
                return Ok(());
            }
        }

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
    if name.trim().is_empty() {
        bot.send_message(msg.chat.id, "❌ Wallet name cannot be empty")
            .await?;
        return Ok(());
    }
    let symbol = parts[1].to_uppercase();
    if !is_plausible_symbol(&symbol) {
        bot.send_message(
            msg.chat.id,
            format!(
                "❌ `{}` is not a ticker. Symbol must be 2–10 letters/digits (e.g. BTC, ETH, SOL).",
                crate::format::esc(&symbol)
            ),
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        return Ok(());
    }
    let address = parts.get(2).map(|s| s.to_string());
    if let Some(addr) = &address {
        if let Some(reason) = address_error(&symbol, addr) {
            bot.send_message(msg.chat.id, format!("❌ {reason}"))
                .await?;
            return Ok(());
        }
    }
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

/// A ticker must look like a symbol: 2–10 ASCII alphanumerics containing at
/// least one letter. This rejects an accidental number such as `60000` (a price
/// typed where a ticker belonged) while still allowing tickers like `1INCH`.
fn is_plausible_symbol(s: &str) -> bool {
    (2..=10).contains(&s.len())
        && s.chars().all(|c| c.is_ascii_alphanumeric())
        && s.chars().any(|c| c.is_ascii_alphabetic())
}

/// Offline shape check for addresses whose format we can verify without a
/// network call. Returns `None` when the address is acceptable (or of a type we
/// cannot verify), and `Some(reason)` when it is definitely malformed.
fn address_error(symbol: &str, address: &str) -> Option<&'static str> {
    let addr = address.trim();
    if addr.is_empty() {
        return Some("address is empty");
    }
    if symbol.eq_ignore_ascii_case("BTC") {
        let bech32 = (27..=62).contains(&addr.len())
            && (addr.starts_with("bc1") || addr.starts_with("BC1"))
            && addr[3..]
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let base58 = (26..=35).contains(&addr.len())
            && (addr.starts_with('1') || addr.starts_with('3'))
            && addr
                .chars()
                .all(|c| c.is_ascii_alphanumeric() && !matches!(c, '0' | 'O' | 'I' | 'l'));
        if !(bech32 || base58) {
            return Some("not a valid BTC address (expected bc1… or 1…/3…)");
        }
    } else if addr.starts_with("0x") || addr.starts_with("0X") {
        let hex = &addr[2..];
        if hex.len() != 40 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some("not a valid EVM address (expected 0x + 40 hex chars)");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{address_error, is_plausible_symbol, parse_wallet_spec};
    use crate::models::WalletKind;

    #[test]
    fn plausible_symbols_accept_tickers_and_reject_prices() {
        assert!(is_plausible_symbol("BTC"));
        assert!(is_plausible_symbol("ETH"));
        assert!(is_plausible_symbol("1INCH"));
        assert!(!is_plausible_symbol("60000"));
        assert!(!is_plausible_symbol("B"));
        assert!(!is_plausible_symbol("BTC-USDT"));
    }

    #[test]
    fn btc_addresses_are_shape_checked() {
        assert!(address_error("BTC", "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq").is_none());
        assert!(address_error("BTC", "1BoatSLRHtKNngkdXEeobR76b53LETtpyT").is_none());
        assert!(address_error("BTC", "60000").is_some());
        assert!(address_error("BTC", "not-an-address").is_some());
        assert!(address_error("BTC", "").is_some());
    }

    #[test]
    fn evm_addresses_are_shape_checked() {
        assert!(address_error("ETH", "0x52908400098527886E0F7030069857D2E4169EE7").is_none());
        assert!(address_error("ETH", "0x52908400098527886E0F7030069857D2E4169EE").is_some());
        assert!(address_error("ETH", "0xZZ908400098527886E0F7030069857D2E4169EE7").is_some());
    }

    #[test]
    fn unverifiable_networks_accept_any_nonempty_address() {
        assert!(address_error("SOL", "anything-non-empty").is_none());
    }

    #[test]
    fn wallet_spec_prefixes_are_parsed() {
        assert_eq!(parse_wallet_spec("cold:Ledger").0, WalletKind::Cold);
        assert_eq!(parse_wallet_spec("exchange:Binance").1, "Binance");
        assert_eq!(
            parse_wallet_spec("Plain"),
            (WalletKind::Hot, "Plain".into())
        );
    }
}
