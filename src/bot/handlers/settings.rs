use crate::{config::Config, crypto::CryptoClient, db::Store};
use std::sync::{Arc, OnceLock, RwLock};
use teloxide::{
    payloads::{EditMessageTextSetters, SendMessageSetters},
    prelude::*,
    types::ParseMode,
};

pub static STATS_CACHE: OnceLock<RwLock<String>> = OnceLock::new();

pub async fn update_stats_cache(sqlite: &crate::db::SqliteStore) {
    match format_stats_menu(sqlite).await {
        Ok(html) => {
            if let Ok(mut cache) = STATS_CACHE
                .get_or_init(|| RwLock::new(String::new()))
                .write()
            {
                *cache = html;
            }
        }
        Err(e) => {
            if let Ok(mut cache) = STATS_CACHE
                .get_or_init(|| RwLock::new(String::new()))
                .write()
            {
                *cache = format!("❌ <b>Failed to load statistics</b>\n\nError: {}", e);
            }
        }
    }
}

pub async fn handle_settings(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    _cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }
    let user_id = msg.chat.id.0 as u64;
    let (watchlist, backup_on, display, quiet_str, lang) = store.read(user_id, |p| {
        let q = match (
            p.settings.alert_quiet_hours_start,
            p.settings.alert_quiet_hours_end,
        ) {
            (Some(s), Some(e)) => format!("{:02}:00 - {:02}:00 UTC", s, e),
            _ => "Disabled".to_string(),
        };
        (
            p.settings.default_watchlist.join(", "),
            p.settings.backup_on_change,
            format!("{:?}", p.settings.currency_display),
            q,
            lang_label(&p.settings.preferred_language).to_string(),
        )
    });
    let text = format!(
        "⚙️ *Settings*\n\n\
         Watchlist: `{}`\n\
         Auto\\-backup on change: *{}*\n\
         Currency display: *{}*\n\
         Quiet Hours: *{}*\n\
         AI Language: *{}*\n",
        crate::format::esc(&watchlist),
        if backup_on { "ON" } else { "OFF" },
        crate::format::esc(&display),
        crate::format::esc(&quiet_str),
        crate::format::esc(&lang),
    );

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(settings_menu(backup_on, &display, &quiet_str, &lang))
        .await?;
    Ok(())
}

pub async fn handle_setting_toggle(
    bot: Bot,
    msg: Message,
    user_id: u64,
    store: Arc<Store>,
    _cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    _sqlite: Arc<crate::db::SqliteStore>,
    data: &str,
) -> anyhow::Result<()> {
    if !config.is_admin(user_id) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    store.write(user_id, |p| {
        match data {
            "setting:toggle_backup" => {
                p.settings.backup_on_change = !p.settings.backup_on_change;
            }
            "setting:toggle_currency" => {
                use crate::models::CurrencyDisplay;
                p.settings.currency_display = match p.settings.currency_display {
                    CurrencyDisplay::Usd => CurrencyDisplay::Byn,
                    CurrencyDisplay::Byn => CurrencyDisplay::Both,
                    CurrencyDisplay::Both => CurrencyDisplay::Usd,
                };
            }
            "setting:toggle_quiet" => {
                let current = (
                    p.settings.alert_quiet_hours_start,
                    p.settings.alert_quiet_hours_end,
                );
                let next = match current {
                    (None, None) => (Some(22), Some(8)),
                    (Some(22), Some(8)) => (Some(23), Some(7)),
                    (Some(23), Some(7)) => (Some(0), Some(6)),
                    _ => (None, None),
                };
                p.settings.alert_quiet_hours_start = next.0;
                p.settings.alert_quiet_hours_end = next.1;
            }
            "setting:stats" => {
                // Handled separately below, but keep here to prevent unknown callback error
            }
            "setting:toggle_language" => {
                p.settings.preferred_language = match p.settings.preferred_language.as_str() {
                    "" => "en".to_string(),
                    "en" => "ru".to_string(),
                    _ => String::new(), // back to auto
                };
            }
            _ => {}
        }
    });

    if data == "setting:stats" {
        let text = {
            let cache = STATS_CACHE
                .get_or_init(|| RwLock::new(String::new()))
                .read()
                .unwrap();
            if cache.is_empty() {
                "⏳ <b>Statistics is loading...</b> Please wait a couple of minutes (or check permissions for data.sqlite).".to_string()
            } else {
                cache.clone()
            }
        };
        let _ = bot.delete_message(msg.chat.id, msg.id).await;
        let _ =
            crate::bot::rich_messages::send_rich_message_markdown(&config, msg.chat.id.0, &text)
                .await;
        if !text.starts_with("⏳") {
            let doc_bytes = text.as_bytes().to_vec();
            let _ = bot
                .send_document(
                    msg.chat.id,
                    teloxide::types::InputFile::memory(doc_bytes).file_name("ai_stats.md"),
                )
                .caption("📊 AI & Provider Performance Statistics")
                .await;
        }
        return Ok(());
    }

    if data == "settings:back" {
        // Just let it fall through and re-render the settings menu
    }

    let (watchlist, backup_on, display, quiet_str, lang) = store.read(user_id, |p| {
        let q = match (
            p.settings.alert_quiet_hours_start,
            p.settings.alert_quiet_hours_end,
        ) {
            (Some(s), Some(e)) => format!("{:02}:00 - {:02}:00 UTC", s, e),
            _ => "Disabled".to_string(),
        };
        (
            p.settings.default_watchlist.join(", "),
            p.settings.backup_on_change,
            format!("{:?}", p.settings.currency_display),
            q,
            lang_label(&p.settings.preferred_language).to_string(),
        )
    });
    let text = format!(
        "⚙️ *Settings*\n\n\
         Watchlist: `{}`\n\
         Auto\\-backup on change: *{}*\n\
         Currency display: *{}*\n\
         Quiet Hours: *{}*\n\
         AI Language: *{}*\n",
        crate::format::esc(&watchlist),
        if backup_on { "ON" } else { "OFF" },
        crate::format::esc(&display),
        crate::format::esc(&quiet_str),
        crate::format::esc(&lang),
    );

    let _ = bot
        .edit_message_text(msg.chat.id, msg.id, text)
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(settings_menu(backup_on, &display, &quiet_str, &lang))
        .await;

    Ok(())
}

pub async fn handle_watchlist(
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
    let user_id = msg.chat.id.0 as u64;

    match parts.as_slice() {
        ["add", sym] => {
            let sym = sym.to_uppercase();
            store.write(user_id, |p| {
                if !p.settings.default_watchlist.contains(&sym) {
                    p.settings.default_watchlist.push(sym.clone());
                }
            });
            bot.send_message(msg.chat.id, format!("✅ {sym} added to watchlist"))
                .await?;
            crate::bot::handlers::portfolio::auto_backup(&bot, &store, user_id).await;
        }
        ["remove" | "del", sym] => {
            let sym = sym.to_uppercase();
            store.write(user_id, |p| {
                p.settings.default_watchlist.retain(|s| s != &sym)
            });
            bot.send_message(msg.chat.id, format!("🗑 {sym} removed from watchlist"))
                .await?;
            crate::bot::handlers::portfolio::auto_backup(&bot, &store, user_id).await;
        }
        ["list"] | [] => {
            let wl = store.read(user_id, |p| p.settings.default_watchlist.join(", "));
            bot.send_message(msg.chat.id, format!("📋 Watchlist: {wl}"))
                .await?;
        }
        _ => {
            bot.send_message(msg.chat.id, "Usage: /watchlist add BTC | remove BTC | list")
                .await?;
        }
    }
    Ok(())
}

pub async fn handle_convert(
    bot: Bot,
    msg: Message,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.len() != 3 {
        bot.send_message(
            msg.chat.id,
            "Usage: /convert AMOUNT FROM TO\nExample: /convert 100 BYN BTC",
        )
        .await?;
        return Ok(());
    }
    let amount: f64 = match parts[0].parse() {
        Ok(v) => v,
        Err(_) => {
            bot.send_message(msg.chat.id, "Invalid amount").await?;
            return Ok(());
        }
    };
    let from = parts[1].to_uppercase();
    let to = parts[2].to_uppercase();

    bot.send_chat_action(msg.chat.id, teloxide::types::ChatAction::Typing)
        .await?;

    let usd = cmc.to_usd(amount, &from).await?;
    let result = if to == "USD" {
        usd
    } else {
        let quotes = cmc.get_quotes(&[to.as_str()]).await?;
        let price = quotes.first().map(|q| q.price_usd).unwrap_or(0.0);
        if price == 0.0 {
            bot.send_message(msg.chat.id, format!("Unknown symbol: {to}"))
                .await?;
            return Ok(());
        }
        usd / price
    };

    bot.send_message(
        msg.chat.id,
        format!("💱 *{} {}* = *{:.8} {}*", amount, from, result, to),
    )
    .parse_mode(ParseMode::MarkdownV2)
    .await?;
    Ok(())
}

fn settings_menu(
    backup_on: bool,
    display: &str,
    quiet_hours: &str,
    lang: &str,
) -> teloxide::types::InlineKeyboardMarkup {
    use teloxide::types::InlineKeyboardButton as btn;
    let backup_label = if backup_on {
        "💾 Auto-backup: ON"
    } else {
        "💾 Auto-backup: OFF"
    };
    let currency_label = format!("💵 Currency: {}", display);
    let quiet_label = format!("⏰ Quiet Hours: {}", quiet_hours);
    let lang_label = format!("🌐 AI Language: {}", lang);
    teloxide::types::InlineKeyboardMarkup::new([
        vec![btn::callback(backup_label, "setting:toggle_backup")],
        vec![btn::callback(currency_label, "setting:toggle_currency")],
        vec![btn::callback(quiet_label, "setting:toggle_quiet")],
        vec![btn::callback(lang_label, "setting:toggle_language")],
        vec![
            btn::callback("📋 Watchlist", "settings:watchlist"),
            btn::callback("📊 Stats", "setting:stats"),
        ],
        vec![btn::callback("🏠 Main Menu", "menu")],
    ])
}

/// Human-readable label for the preferred-language setting.
fn lang_label(code: &str) -> &str {
    match code {
        "en" => "English",
        "ru" => "Русский",
        _ => "Auto",
    }
}

async fn format_stats_menu(sqlite: &crate::db::SqliteStore) -> anyhow::Result<String> {
    let mut out = String::from("## 📊 Technical Statistics\n\n");

    for (days_label, days) in &[("24 Hours", 1), ("7 Days", 7), ("30 Days", 30)] {
        let mut metrics = sqlite.get_aggregated_metrics(*days).await?;
        let all_models = crate::crypto::ai::providers::available_models();
        for m in all_models {
            let item_name = format!("{:?}/{}", m.provider, m.id);
            if !metrics
                .iter()
                .any(|x| x.item == item_name && x.category == "AI Model")
            {
                metrics.push(crate::models::MetricAgg {
                    category: "AI Model".to_string(),
                    item: item_name,
                    ok_count: 0,
                    err_count: 0,
                });
            }
        }
        // Zero-fill web-search providers so unused ones (Exa/Local only run when
        // the previous provider fails) still appear in the stats instead of
        // being invisible until their first real call.
        for p in ["Tavily", "Exa", "Local"] {
            if !metrics
                .iter()
                .any(|x| x.item == p && x.category == "Search")
            {
                metrics.push(crate::models::MetricAgg {
                    category: "Search".to_string(),
                    item: p.to_string(),
                    ok_count: 0,
                    err_count: 0,
                });
            }
        }
        if metrics.is_empty() {
            continue;
        }

        out.push_str(&format!("### 📅 Last {}\n", days_label));

        // Group by category
        let mut by_cat: std::collections::HashMap<String, Vec<crate::models::MetricAgg>> =
            std::collections::HashMap::new();
        for m in metrics {
            by_cat.entry(m.category.clone()).or_default().push(m);
        }

        let mut categories: Vec<_> = by_cat.keys().cloned().collect();
        categories.sort();

        for cat in categories {
            out.push_str(&format!("#### {}\n", cat));
            out.push_str("| Status | Model/Source | OK | ERR | Win Rate |\n");
            out.push_str("|---|---|---|---|---|\n");
            let mut items = by_cat.remove(&cat).unwrap();
            items.sort_by(|a, b| a.item.cmp(&b.item));

            for item in items {
                let total = item.ok_count + item.err_count;
                let win_rate = if total == 0 {
                    0.0
                } else {
                    (item.ok_count as f64 / total as f64) * 100.0
                };
                let icon = if total == 0 {
                    "⚪"
                } else if win_rate >= 95.0 {
                    "🟢"
                } else if win_rate >= 80.0 {
                    "🟡"
                } else {
                    "🔴"
                };
                out.push_str(&format!(
                    "| {} | `{}` | {} | {} | **{:.1}%** |\n",
                    icon, item.item, item.ok_count, item.err_count, win_rate
                ));
            }
            out.push('\n');
        }
    }

    if out == "## 📊 Technical Statistics\n\n" {
        out.push_str("*No data available yet. Please wait 5 minutes.*");
    }

    // ── Paid usage (DeepSeek) ──────────────────────────────────────────────
    if let Ok((used_tokens, used_reqs)) = sqlite.get_monthly_usage("deepseek") {
        out.push_str("## 💰 Paid Usage (DeepSeek)\n\n");
        let token_budget: i64 = std::env::var("DEEPSEEK_MONTHLY_TOKEN_BUDGET")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3_000_000);
        let req_budget: i64 = std::env::var("DEEPSEEK_MONTHLY_REQ_BUDGET")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(150);
        let token_pct = if token_budget > 0 {
            used_tokens * 100 / token_budget
        } else {
            0
        };
        let req_pct = if req_budget > 0 {
            used_reqs * 100 / req_budget
        } else {
            0
        };
        out.push_str(&format!(
            "| Metric | Used | Budget | Used % | Remaining |\n|---|---|---|---|---|\n"
        ));
        out.push_str(&format!(
            "| Tokens | {} | {} | {}% | {} |\n",
            used_tokens,
            token_budget,
            token_pct,
            (token_budget - used_tokens).max(0)
        ));
        out.push_str(&format!(
            "| Requests | {} | {} | {}% | {} |\n\n",
            used_reqs,
            req_budget,
            req_pct,
            (req_budget - used_reqs).max(0)
        ));

        for (label, days) in [
            ("Today", 1),
            ("7 Days", 7),
            ("30 Days", 30),
            ("60 Days", 60),
            ("90 Days", 90),
        ] {
            match sqlite.get_paid_usage_summary(days).await {
                Ok(rows) if !rows.is_empty() => {
                    out.push_str(&format!("### {}\n", label));
                    out.push_str("| Model | Tokens | Requests |\n|---|---|---|\n");
                    let mut total_t = 0i64;
                    let mut total_r = 0i64;
                    for (model, tokens, reqs) in rows {
                        total_t += tokens;
                        total_r += reqs;
                        out.push_str(&format!("| `{}` | {} | {} |\n", model, tokens, reqs));
                    }
                    out.push_str(&format!(
                        "| **Total** | **{}** | **{}** |\n\n",
                        total_t, total_r
                    ));
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("Failed to load paid usage: {}", e),
            }
        }
    }

    Ok(out)
}
