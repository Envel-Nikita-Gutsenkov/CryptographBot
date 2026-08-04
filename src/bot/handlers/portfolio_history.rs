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
        lines.push(String::from("📊 <b>Portfolio Statistics</b>\n"));

        let mut total_cost = 0.0;
        let mut asset_summary: std::collections::HashMap<String, (f64, f64)> = std::collections::HashMap::new();

        for pur in &p.purchases {
            total_cost += pur.cost_usd;
            let entry = asset_summary.entry(pur.symbol.clone()).or_insert((0.0, 0.0));
            entry.0 += pur.amount;
            entry.1 += pur.cost_usd;
        }

        lines.push(format!("💰 <b>Total Invested:</b> ${:.2}\n", total_cost));

        lines.push(String::from("💼 <b>Assets Summary:</b>"));
        lines.push(String::from("<b>Asset | Amount | Avg Entry | Total Cost</b>\n------------------------------------------------"));

        let mut sorted_assets: Vec<_> = asset_summary.into_iter().collect();
        sorted_assets.sort_by(|a, b| b.1.1.partial_cmp(&a.1.1).unwrap_or(std::cmp::Ordering::Equal));

        for (symbol, (amount, cost)) in sorted_assets {
            let avg_entry = if amount > 0.0 { cost / amount } else { 0.0 };
            lines.push(format!(
                "<b>{}</b>: {}\n└ Entry: ${:.2} | Cost: ${:.2}",
                symbol, crate::bot::utils::fmt_crypto(amount, &symbol), avg_entry, cost
            ));
            lines.push(String::new());
        }

        lines.push(String::from("\n📜 <b>Detailed Purchase History</b>\n------------------------------------------------"));

        let mut sorted_purchases = p.purchases.clone();
        sorted_purchases.sort_by(|a, b| b.date.cmp(&a.date));

        for pur in &sorted_purchases {
            let date_str = pur.date.format("%Y-%m-%d").to_string();
            lines.push(format!(
                "<b>#{} | {} | {}</b>\n└ Amount: {} | Cost: ${:.2}\n└ Wallet: {}",
                pur.id, date_str, pur.symbol, crate::bot::utils::fmt_crypto(pur.amount, &pur.symbol), pur.cost_usd, pur.source
            ));
            lines.push(String::new());
        }
        lines.push(String::from("<i>To fix mistakes, use <code>/patch fix amount of purchase #5 to 0.1</code></i>"));
        lines.join("\n")
    });

    if let Err(e) = bot.send_message(msg.chat.id, &html_msg).parse_mode(teloxide::types::ParseMode::Html).await {
        tracing::error!("HTML message failed: {}", e);
        let _ = bot.send_message(msg.chat.id, "Failed to render purchase history. Check logs.").await;
    }

    Ok(())
}
