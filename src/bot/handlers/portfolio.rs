use std::sync::Arc;

use chrono::Utc;
use teloxide::{payloads::SendMessageSetters, prelude::*, types::ParseMode};

use crate::{
    bot::keyboards,
    config::Config,
    crypto::CryptoClient,
    db::Store,
    format::{fmt_crypto, fmt_pnl_usd, fmt_price, symbol_emoji},
    models::{CorrectionEntry, PaidCurrency, Portfolio, Purchase, Transfer, WalletKind},
};

// ── Add purchase ──────────────────────────────────────────────────────────────

/// Parse: /add BTC 0.01 600 [hot:name | cold:name | exchange:name]
pub async fn handle_add(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    tracing::info!("handle_add: user={}, args={:?}", msg.chat.id, args);
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.len() < 3 {
        bot.send_message(
            msg.chat.id,
            "Usage: <code>/add SYM AMOUNT COST_USD [hot:name|cold:name|exchange:name]</code>\n\
             Example: <code>/add BTC 0.001 60 exchange:binance</code>",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    let symbol = parts[0].to_uppercase();
    let amount: f64 = match parts[1].parse() {
        Ok(v) => v,
        Err(_) => {
            bot.send_message(msg.chat.id, "Invalid amount").await?;
            return Ok(());
        }
    };
    let cost_usd: f64 = match parts[2].parse() {
        Ok(v) => v,
        Err(_) => {
            bot.send_message(msg.chat.id, "Invalid cost").await?;
            return Ok(());
        }
    };

    // Optional wallet designation
    let wallet_spec = parts.get(3).copied().unwrap_or("hot:default");
    let (kind, wallet_name) = parse_wallet_spec(wallet_spec);

    let user_id = msg.chat.id.0 as u64;

    let purchase_id = store.write(user_id, |p| {
        // Ensure wallet exists
        let wallet_id = ensure_wallet(p, &wallet_name, kind.clone(), &symbol);

        // Record purchase (immutable cost basis)
        let id = p.alloc_purchase_id();
        p.purchases.push(Purchase {
            id,
            symbol: symbol.clone(),
            amount,
            cost_usd,
            paid_amount: cost_usd,
            paid_currency: PaidCurrency::Usd,
            date: Utc::now(),
            source: wallet_name.clone(),
            notes: None,
            created_at: Utc::now(),
            corrections: Vec::new(),
        });

        // Credit holding
        let h = p.holding_mut(wallet_id, &symbol);
        h.amount += amount;

        id
    });

    // Try to get live price for context
    let price_info = match cmc.get_quotes(&[symbol.as_str()]).await {
        Ok(quotes) if !quotes.is_empty() => {
            let q = &quotes[0];
            let current_val = amount * q.price_usd;
            let pnl = current_val - cost_usd;
            format!(
                "\nCurrent value: <b>{}</b> ({})\nPrice: <b>{}</b>",
                fmt_price(current_val),
                fmt_pnl_usd(pnl),
                fmt_price(q.price_usd),
            )
        }
        _ => String::new(),
    };

    let reply = format!(
        "✅ <b>Purchase #{} recorded</b>\n\
         {} {} {} @ {}/coin\n\
         Cost basis: <b>{}</b>{}\n\
         Wallet: <b>{}</b> ({})",
        purchase_id,
        symbol_emoji(&symbol),
        symbol,
        fmt_crypto(amount, &symbol),
        fmt_price(cost_usd / amount),
        fmt_price(cost_usd),
        price_info,
        wallet_name,
        kind.label(),
    );

    bot.send_message(msg.chat.id, reply)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboards::portfolio_menu())
        .await?;

    // Auto-backup
    auto_backup(&bot, &store, user_id).await;
    Ok(())
}

// ── Move (transfer) ───────────────────────────────────────────────────────────

/// Parse: /move BTC 0.5 from:ledger to:binance fee:0.0001 [fee_usd:3.50]
pub async fn handle_move(
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
    if parts.len() < 4 {
        bot.send_message(
            msg.chat.id,
            "Usage: `/move SYM AMOUNT from:WALLET to:WALLET [fee:AMOUNT] [fee_usd:USD]`\n\
             Example: `/move BTC 0.5 from:binance to:ledger fee:0.0001 fee_usd:6.5`",
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        return Ok(());
    }

    let symbol = parts[0].to_uppercase();
    let amount_sent: f64 = match parts[1].parse() {
        Ok(v) => v,
        Err(_) => {
            bot.send_message(msg.chat.id, "Invalid amount").await?;
            return Ok(());
        }
    };

    let mut from_name: Option<String> = None;
    let mut to_name: Option<String> = None;
    let mut fee: f64 = 0.0;
    let mut fee_usd: Option<f64> = None;

    for part in &parts[2..] {
        if let Some(v) = part.strip_prefix("from:") {
            from_name = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("to:") {
            to_name = Some(v.to_string());
        } else if let Some(v) = part.strip_prefix("fee:") {
            fee = v.parse().unwrap_or(0.0);
        } else if let Some(v) = part.strip_prefix("fee_usd:") {
            fee_usd = v.parse().ok();
        }
    }

    let amount_received = amount_sent - fee;
    if amount_received < 0.0 {
        bot.send_message(msg.chat.id, "Fee cannot exceed amount sent")
            .await?;
        return Ok(());
    }

    let user_id = msg.chat.id.0 as u64;

    let result: Result<(u64, String), String> = store.write(user_id, |p| {
        let from_id = from_name.as_deref().and_then(|n| {
            p.wallets
                .iter()
                .find(|w| w.name.eq_ignore_ascii_case(n))
                .map(|w| w.id)
        });
        let to_id = to_name.as_deref().and_then(|n| {
            p.wallets
                .iter()
                .find(|w| w.name.eq_ignore_ascii_case(n))
                .map(|w| w.id)
        });

        if from_name.is_some() && from_id.is_none() {
            return Err(format!(
                "Wallet '{}' not found",
                from_name.as_deref().unwrap()
            ));
        }
        if to_name.is_some() && to_id.is_none() {
            return Err(format!(
                "Wallet '{}' not found",
                to_name.as_deref().unwrap()
            ));
        }

        let tid = p.alloc_transfer_id();
        let transfer = Transfer::new(
            tid,
            from_id,
            to_id,
            symbol.clone(),
            amount_sent,
            amount_received,
            fee_usd,
            None,
            Utc::now(),
            None,
            None,
        );
        p.apply_transfer(&transfer);

        let summary = format!(
            "#{tid}: {} {} → {} (fee: {})",
            amount_sent, symbol, amount_received, fee,
        );
        Ok((tid, summary))
    });

    match result {
        Err(e) => {
            bot.send_message(msg.chat.id, format!("❌ {e}")).await?;
        }
        Ok((tid, _summary)) => {
            let fee_info = fee_usd
                .map(|u| format!("\nFee \\(USD\\): *{}*", fmt_price(u)))
                .unwrap_or_default();

            let reply = format!(
                "🔀 *Transfer \\#{} recorded*\n\
                 {} {}\n\
                 Sent: *{}*   Received: *{}*\n\
                 Network fee: *{}* {}{}\n\n\
                 💡 Cost basis unchanged — only holdings updated\\.",
                tid,
                symbol_emoji(&symbol),
                symbol,
                fmt_crypto(amount_sent, &symbol),
                fmt_crypto(amount_received, &symbol),
                fmt_crypto(fee, &symbol),
                symbol,
                fee_info,
            );

            bot.send_message(msg.chat.id, reply)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;

            auto_backup(&bot, &store, user_id).await;
        }
    }
    Ok(())
}

// ── Portfolio overview ────────────────────────────────────────────────────────

pub async fn handle_portfolio(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let user_id = msg.chat.id.0 as u64;
    let (symbols, invested): (Vec<String>, f64) = store.read(user_id, |p| {
        let syms = p.holdings_by_symbol().into_keys().collect();
        (syms, p.total_invested_usd())
    });

    if symbols.is_empty() {
        bot.send_message(
            msg.chat.id,
            "📭 Portfolio is empty.\nUse /add to record a purchase.",
        )
        .await?;
        return Ok(());
    }

    bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing)
        .await?;

    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = cmc.get_quotes(&sym_refs).await.unwrap_or_default();
    let prices: std::collections::HashMap<String, f64> = quotes
        .iter()
        .map(|q| (q.symbol.clone(), q.price_usd))
        .collect();

    let (html_msg, _total_val) = store.read(user_id, |p| {
        let mut lines = Vec::new();
        let mut total_val = 0.0f64;

        lines.push(String::from("<b>Asset | Amount | Avg Entry | Price | Value | PnL</b>\n---------------------------------------------------------"));

        let by_symbol = p.holdings_by_symbol();
        for (sym, amt) in &by_symbol {
            let price = prices.get(sym.as_str()).copied().unwrap_or(0.0);
            let val = amt * price;
            total_val += val;
            let (cost, _is_manual) = if let Some(&m) = p.manual_cost_basis.get(sym) {
                (m, true)
            } else {
                (p.purchases.iter().filter(|x| &x.symbol == sym).map(|x| x.cost_usd).sum(), false)
            };
            
            let avg_entry = if *amt > 0.0 { cost / amt } else { 0.0 };
            let pnl = val - cost;
            let pnl_pct = if cost > 0.0 { pnl / cost * 100.0 } else { 0.0 };
            
            let emoji = if pnl >= 0.0 { "🟢" } else { "🔴" };
            let pnl_str = format!("{emoji} ${:.2} ({:+.1}%)", pnl, pnl_pct);
            
            lines.push(format!(
                "{} <b>{}</b>: {}\n└ Entry: ${:.2} | Price: ${:.2}\n└ Value: ${:.2} | PnL: {}",
                symbol_emoji(sym), sym, fmt_crypto(*amt, sym), avg_entry, price, val, pnl_str
            ));
            lines.push(String::new());
        }

        let pnl = total_val - p.total_invested_usd();
        let pnl_pct = if invested > 0.0 { pnl / invested * 100.0 } else { 0.0 };
        let emoji = if pnl >= 0.0 { "📈" } else { "📉" };

        let header = format!(
            "💼 <b>Portfolio Overview</b>\n\n\
            <b>Total Value:</b> ${:.2}\n\
            <b>Total Invested:</b> ${:.2}\n\
            <b>Net Profit:</b> {} ${:.2} ({:+.2}%)\n\n",
            total_val, invested, emoji, pnl, pnl_pct
        );

        let body = lines.join("\n");
        (format!("{}{}", header, body), total_val)
    });

    if let Err(e) = bot
        .send_message(msg.chat.id, &html_msg)
        .parse_mode(teloxide::types::ParseMode::Html)
        .await
    {
        tracing::error!("Failed to send HTML message: {}", e);
        let _ = bot
            .send_message(msg.chat.id, "Failed to render portfolio.")
            .await;
    }

    bot.send_message(msg.chat.id, "Options:")
        .reply_markup(keyboards::portfolio_menu())
        .await?;

    Ok(())
}

/// Send portfolio overview chart as image.
pub async fn handle_portfolio_chart(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let user_id = msg.chat.id.0 as u64;
    let symbols: Vec<String> =
        store.read(user_id, |p| p.holdings_by_symbol().into_keys().collect());

    if symbols.is_empty() {
        bot.send_message(msg.chat.id, "Portfolio is empty").await?;
        return Ok(());
    }

    bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::UploadPhoto)
        .await?;

    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = cmc.get_quotes(&sym_refs).await.unwrap_or_default();
    let prices: std::collections::HashMap<String, f64> = quotes
        .iter()
        .map(|q| (q.symbol.clone(), q.price_usd))
        .collect();

    let (_portfolio_clone, chart) = store.read(user_id, |p| {
        let port = p.clone();
        let chart = futures::executor::block_on(
            crate::crypto::charts::ChartRenderer::portfolio_overview(&port, &prices),
        );
        (port, chart)
    });

    match chart {
        Ok(png) => {
            bot.send_photo(msg.chat.id, teloxide::types::InputFile::memory(png))
                .caption("Portfolio Overview")
                .await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("Chart error: {e}"))
                .await?;
        }
    }
    Ok(())
}

/// Correction: /correct #ID field new_value [reason]
pub async fn handle_correct(
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

    let parts: Vec<&str> = args.splitn(4, ' ').collect();
    if parts.len() < 3 {
        bot.send_message(
            msg.chat.id,
            "Usage: `/correct \\#ID field new_value [reason]`\n\
             Fields: `amount`, `cost_usd`, `date`, `source`, `notes`",
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        return Ok(());
    }

    let id: u64 = match parts[0].trim_start_matches('#').parse() {
        Ok(v) => v,
        Err(_) => {
            bot.send_message(msg.chat.id, "Invalid ID").await?;
            return Ok(());
        }
    };
    let field = parts[1];
    let new_value = parts[2];
    let reason = parts.get(3).copied().unwrap_or("manual correction");

    let user_id = msg.chat.id.0 as u64;
    let result: Result<String, String> = store.write(user_id, |p| {
        let purchase = p
            .purchases
            .iter_mut()
            .find(|x| x.id == id)
            .ok_or_else(|| format!("Purchase #{id} not found"))?;

        let old_value = match field {
            "amount" => {
                let old = purchase.amount.to_string();
                purchase.amount = new_value
                    .parse()
                    .map_err(|_| "Invalid number".to_string())?;
                old
            }
            "cost_usd" => {
                let old = purchase.cost_usd.to_string();
                purchase.cost_usd = new_value
                    .parse()
                    .map_err(|_| "Invalid number".to_string())?;
                purchase.paid_amount = purchase.cost_usd;
                old
            }
            "source" => {
                let old = purchase.source.clone();
                purchase.source = new_value.to_string();
                old
            }
            "notes" => {
                let old = purchase.notes.clone().unwrap_or_default();
                purchase.notes = Some(new_value.to_string());
                old
            }
            _ => return Err(format!("Unknown field '{field}'")),
        };

        purchase.corrections.push(CorrectionEntry {
            at: Utc::now(),
            field: field.to_string(),
            old_value: old_value.clone(),
            new_value: new_value.to_string(),
            reason: reason.to_string(),
        });

        Ok(old_value)
    });

    match result {
        Err(e) => {
            bot.send_message(msg.chat.id, format!("❌ {e}")).await?;
        }
        Ok(old) => {
            bot.send_message(
                msg.chat.id,
                format!("✅ Purchase #{id} corrected\n`{field}`: `{old}` → `{new_value}`"),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            auto_backup(&bot, &store, user_id).await;
        }
    }
    Ok(())
}

/// Manually adjust a holding: /adjust wallet:name SYM new_amount [reason]
pub async fn handle_adjust(
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
    if parts.len() < 3 {
        bot.send_message(
            msg.chat.id,
            "Usage: `/adjust wallet:NAME SYM AMOUNT [reason]`",
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        return Ok(());
    }

    let wallet_spec = parts[0].strip_prefix("wallet:").unwrap_or(parts[0]);
    let symbol = parts[1].to_uppercase();
    let new_amount: f64 = match parts[2].parse() {
        Ok(v) => v,
        Err(_) => {
            bot.send_message(msg.chat.id, "Invalid amount").await?;
            return Ok(());
        }
    };
    let reason = parts.get(3).copied().unwrap_or("manual adjustment");

    let user_id = msg.chat.id.0 as u64;
    let result: Result<f64, String> = store.write(user_id, |p| {
        let wallet_id = p
            .wallets
            .iter()
            .find(|w| w.name.eq_ignore_ascii_case(wallet_spec))
            .map(|w| w.id)
            .ok_or_else(|| format!("Wallet '{wallet_spec}' not found"))?;

        let h = p.holding_mut(wallet_id, &symbol);
        let old = h.amount;
        h.corrections.push(CorrectionEntry {
            at: Utc::now(),
            field: "amount".into(),
            old_value: old.to_string(),
            new_value: new_amount.to_string(),
            reason: reason.to_string(),
        });
        h.amount = new_amount;
        Ok(old)
    });

    match result {
        Err(e) => {
            bot.send_message(msg.chat.id, format!("❌ {e}")).await?;
        }
        Ok(old) => {
            bot.send_message(
                msg.chat.id,
                format!("✅ {symbol} in `{wallet_spec}` adjusted\n`{old:.8}` → `{new_amount:.8}`"),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            auto_backup(&bot, &store, user_id).await;
        }
    }
    Ok(())
}

/// Manually override cost basis: /invested SYM [AMOUNT|clear]
pub async fn handle_invested_cmd(
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
            "Usage: `/invested SYM AMOUNT`\n\
             Example: `/invested BTC 50000`\n\
             To clear override: `/invested BTC clear`",
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        return Ok(());
    }

    let symbol = parts[0].to_uppercase();
    let val_str = parts[1].to_lowercase();
    let user_id = msg.chat.id.0 as u64;

    if val_str == "clear" {
        store.write(user_id, |p| {
            p.manual_cost_basis.remove(&symbol);
        });
        bot.send_message(
            msg.chat.id,
            format!(
                "✅ Manual cost basis cleared for {}. Now using purchase history.",
                symbol
            ),
        )
        .await?;
        auto_backup(&bot, &store, user_id).await;
        return Ok(());
    }

    let amount: f64 = match val_str.parse() {
        Ok(v) => v,
        Err(_) => {
            bot.send_message(msg.chat.id, "Invalid amount").await?;
            return Ok(());
        }
    };

    store.write(user_id, |p| {
        p.manual_cost_basis.insert(symbol.clone(), amount);
    });

    bot.send_message(
        msg.chat.id,
        format!(
            "✅ Total invested amount for {} manually set to ${}",
            symbol, amount
        ),
    )
    .await?;
    auto_backup(&bot, &store, user_id).await;
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_wallet_spec(spec: &str) -> (WalletKind, String) {
    if let Some(name) = spec.strip_prefix("cold:") {
        (WalletKind::Cold, name.to_string())
    } else if let Some(name) = spec.strip_prefix("hot:") {
        (WalletKind::Hot, name.to_string())
    } else if let Some(name) = spec.strip_prefix("exchange:") {
        (WalletKind::Exchange, name.to_string())
    } else {
        (WalletKind::Hot, spec.to_string())
    }
}

fn ensure_wallet(p: &mut Portfolio, name: &str, kind: WalletKind, _symbol: &str) -> u64 {
    if let Some(w) = p.wallets.iter().find(|w| w.name.eq_ignore_ascii_case(name)) {
        return w.id;
    }
    let id = p.alloc_wallet_id();
    p.wallets.push(crate::models::Wallet {
        id,
        name: name.to_string(),
        kind,
        address: None,
        network: None,
        exchange: None,
        monitored: false,
        last_balance: std::collections::HashMap::new(),
        last_checked: None,
    });
    id
}

pub async fn auto_backup(bot: &Bot, store: &Store, user_id: u64) {
    let should_backup = store.read(user_id, |p| p.settings.backup_on_change);
    if !should_backup {
        return;
    }

    match store.export_backup(user_id) {
        Ok(data) => {
            let _ = bot
                .send_document(
                    ChatId(user_id as i64),
                    teloxide::types::InputFile::memory(data).file_name("cryptograph_backup.json"),
                )
                .caption("🔒 Auto-backup — keep this file safe")
                .await;
        }
        Err(e) => tracing::warn!("Auto-backup failed: {e}"),
    }
}

/// Same as auto_backup but does NOT send the file to Telegram — used after silent/AI-triggered patches.
pub async fn auto_backup_silent(bot: &Bot, store: &Store, user_id: u64) {
    let _ = bot; // reserved for future encrypted send
    let should_backup = store.read(user_id, |p| p.settings.backup_on_change);
    if !should_backup {
        return;
    }
    match store.export_backup(user_id) {
        Ok(_) => tracing::info!("Silent auto-backup completed for user {user_id}"),
        Err(e) => tracing::warn!("Silent auto-backup failed: {e}"),
    }
}

pub async fn handle_advice(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let user_id = msg.chat.id.0 as u64;
    bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing)
        .await?;

    let (symbols, watchlist, avg_prices) = store.read(user_id, |p| {
        let syms: Vec<String> = p.holdings_by_symbol().into_keys().collect();
        let wl = p.settings.default_watchlist.clone();
        let mut avgs = std::collections::HashMap::new();
        for s in &syms {
            if let Some(avg) = p.avg_buy_price(s) {
                if avg > 0.0 {
                    avgs.insert(s.clone(), avg);
                }
            }
        }
        (syms, wl, avgs)
    });

    let mut all_syms = std::collections::HashSet::new();
    for s in &symbols {
        all_syms.insert(s.clone());
    }
    for s in &watchlist {
        all_syms.insert(s.clone());
    }
    all_syms.insert("BTC".to_string());
    all_syms.insert("ETH".to_string());

    let sym_refs: Vec<&str> = all_syms.iter().map(|s| s.as_str()).collect();
    let quotes = cmc.get_quotes(&sym_refs).await.unwrap_or_default();
    let prices: std::collections::HashMap<String, &crate::models::CmcQuote> =
        quotes.iter().map(|q| (q.symbol.clone(), q)).collect();

    // Algorithmic Sentiment Index Calculation
    let mut avg_24h_change: f64 = 0.0;
    if !quotes.is_empty() {
        let sum_change: f64 = quotes.iter().map(|q| q.change_24h.unwrap_or(0.0)).sum();
        avg_24h_change = sum_change / quotes.len() as f64;
    }
    let sentiment_score = (50.0f64 + avg_24h_change * 2.0f64).clamp(0.0f64, 100.0f64) as u32;
    let sentiment_label = match sentiment_score {
        0..=25 => "Extreme Fear 😨 \\(Strong Buy Opportunity\\)",
        26..=45 => "Fear 😰 \\(Accumulation Phase\\)",
        46..=55 => "Neutral 😐",
        56..=75 => "Greed 🤑 \\(Hold / Caution\\)",
        _ => "Extreme Greed 🚨 \\(Take Profit / Sell Target\\)",
    };

    let mut lines = Vec::new();

    lines.push(format!(
        "📊 *Algorithmic Market Sentiment:*\n\
         ├  Index Score: *{} / 100*\n\
         └  Current Status: *{}*",
        sentiment_score, sentiment_label
    ));

    lines.push("📅 *Market Cycle & Halving:*\n\
               The next Bitcoin halving is expected around *April 2028*\\. Historically, halving events reduce block rewards and trigger a supply shock, starting a major bull cycle that peaks 12\\-18 months later \\(late 2028 / early 2029\\)\\. \n\
               💡 *Strategy*: 2026\\-2027 is a prime *accumulation phase*\\. Focus on buying during major market drawdowns \\(\\-30% or more\\) and hold through to the 2028\\-2029 target exit window\\.".to_string());

    let mut holding_advice = Vec::new();
    for (sym, avg) in &avg_prices {
        if let Some(quote) = prices.get(sym) {
            let current = quote.price_usd;
            let ratio = current / avg;
            let esc_sym = crate::format::esc(sym);
            if ratio <= 0.80 {
                let diff = (1.0 - ratio) * 100.0;
                holding_advice.push(format!("🟢 *{}*: Down *{:.1}%* below your average buy price \\(avg: *{}*, current: *{}*\\)\\. Excellent opportunity to *DCA \\(average down\\)* to lower your entry cost\\.", 
                    esc_sym, diff, crate::format::esc(&crate::format::fmt_price(*avg)), crate::format::esc(&crate::format::fmt_price(current))));
            } else if ratio >= 1.50 {
                let diff = (ratio - 1.0) * 100.0;
                holding_advice.push(format!("🔴 *{}*: Up *{:.1}%* profit \\(avg: *{}*, current: *{}*\\)\\. Consider scaling out/taking partial profits \\(e.g\\. 10\\-20%\\) if it is nearing your target\\.", 
                    esc_sym, diff, crate::format::esc(&crate::format::fmt_price(*avg)), crate::format::esc(&crate::format::fmt_price(current))));
            } else {
                holding_advice.push(format!("⚪ *{}*: trading near cost basis \\(avg: *{}*, current: *{}*\\)\\. Hold position\\.", 
                    esc_sym, crate::format::esc(&crate::format::fmt_price(*avg)), crate::format::esc(&crate::format::fmt_price(current))));
            }
        }
    }
    if !holding_advice.is_empty() {
        lines.push(format!(
            "💼 *Portfolio Position Advice:*\n{}",
            holding_advice.join("\n")
        ));
    }

    let mut dips = Vec::new();
    for quote in &quotes {
        if watchlist.contains(&quote.symbol) || quote.symbol == "BTC" || quote.symbol == "ETH" {
            if quote.change_7d.unwrap_or(0.0) < -10.0 {
                dips.push(format!("🔥 *{}* has dropped *{:.1}%* in the last 7 days! Price is *{}*\\. Strong candidate to buy the dip\\.", 
                    crate::format::esc(&quote.symbol), quote.change_7d.unwrap_or(0.0).abs(), crate::format::esc(&crate::format::fmt_price(quote.price_usd))));
            }
        }
    }
    if !dips.is_empty() {
        lines.push(format!(
            "📉 *Dip Buying Opportunities:*\n{}",
            dips.join("\n")
        ));
    } else {
        lines.push("📉 *Dip Buying Opportunities:*\nNo major watchlist dips \\(>10% drop in 7d\\) detected right now\\. Stay patient\\.".to_string());
    }

    let reply = format!(
        "💡 *Long\\-Term Investment Intelligence*\n\n{}",
        lines.join("\n\n")
    );

    bot.send_message(msg.chat.id, reply)
        .parse_mode(ParseMode::MarkdownV2)
        .await?;

    Ok(())
}

pub async fn handle_pnl_chart(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let user_id = msg.chat.id.0 as u64;
    let symbols: Vec<String> =
        store.read(user_id, |p| p.holdings_by_symbol().into_keys().collect());

    if symbols.is_empty() {
        bot.send_message(msg.chat.id, "Portfolio is empty").await?;
        return Ok(());
    }

    bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::UploadPhoto)
        .await?;

    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = cmc.get_quotes(&sym_refs).await.unwrap_or_default();
    let prices: std::collections::HashMap<String, f64> = quotes
        .iter()
        .map(|q| (q.symbol.clone(), q.price_usd))
        .collect();

    let chart = store.read(user_id, |p| {
        let port = p.clone();
        futures::executor::block_on(crate::crypto::charts::ChartRenderer::pnl_breakdown(
            &port, &prices,
        ))
    });

    match chart {
        Ok(png) => {
            bot.send_photo(msg.chat.id, teloxide::types::InputFile::memory(png))
                .caption("P&L Breakdown")
                .await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("Chart error: {e}"))
                .await?;
        }
    }
    Ok(())
}

// ── Purchase History ─────────────────────────────────────────────────────────

pub async fn handle_purchase_history(
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

    let html_msg = store.read(user_id, |p| {
        if p.purchases.is_empty() {
            return String::from("📭 <b>No purchase history found.</b>");
        }

        let mut lines = Vec::new();
        lines.push(String::from("📜 <b>Purchase History</b>\n"));

        let mut sorted_purchases = p.purchases.clone();
        sorted_purchases.sort_by(|a, b| b.date.cmp(&a.date));

        for pur in &sorted_purchases {
            let date_str = pur.date.format("%Y-%m-%d").to_string();
            let cost_str = format!("${:.2}", pur.cost_usd);
            
            lines.push(format!(
                "<b>#{} | {} | {}</b>\n└ Amount: {} | Cost: {}\n└ Source: {}",
                pur.id, date_str, pur.symbol, fmt_crypto(pur.amount, &pur.symbol), cost_str, pur.source
            ));
            lines.push(String::new());
        }

        lines.push(String::from("<i>To fix any mistakes, use the AI patcher! (e.g. <code>/patch fix date of purchase #5 to 2024-05-12</code>)</i>"));
        lines.join("\n")
    });

    if let Err(e) = bot
        .send_message(msg.chat.id, &html_msg)
        .parse_mode(teloxide::types::ParseMode::Html)
        .await
    {
        tracing::error!("HTML message failed: {}", e);
        let _ = bot
            .send_message(
                msg.chat.id,
                "Failed to render purchase history. Check logs.",
            )
            .await;
    }

    Ok(())
}
