use std::sync::Arc;
use teloxide::{
    dispatching::dialogue::InMemStorage, payloads::SendMessageSetters, prelude::*, types::ParseMode,
};

use crate::{
    bot::{keyboards, state::State},
    config::Config,
    crypto::CryptoClient,
    db::Store,
    models::{AlertCondition, AlertRule, Period},
};

type MyDialogue = Dialogue<State, InMemStorage<State>>;

pub async fn handle_alert_step1(
    bot: Bot,
    msg: Message,
    dialogue: MyDialogue,
) -> anyhow::Result<()> {
    let symbol = msg.text().unwrap_or("").trim().to_uppercase();
    if symbol.is_empty() {
        bot.send_message(
            msg.chat.id,
            "❌ Symbol cannot be empty. Please enter a symbol (e.g. BTC):",
        )
        .reply_markup(keyboards::cancel_only())
        .await?;
        return Ok(());
    }

    dialogue
        .update(State::AlertStep2 {
            symbol: symbol.clone(),
            condition: AlertCondition::PriceBelow, // Default placeholder, will be chosen next
        })
        .await?;

    bot.send_message(
        msg.chat.id,
        format!(
            "🔔 *Alert Wizard* \\(Step 2 of 3\\)\n\n\
                 Symbol: *{}*\n\n\
                 Select the alert condition type:",
            symbol
        ),
    )
    .parse_mode(ParseMode::MarkdownV2)
    .reply_markup(keyboards::alert_condition_picker())
    .await?;

    Ok(())
}

pub async fn handle_alert_step2(
    bot: Bot,
    msg: Message,
    _dialogue: MyDialogue,
) -> anyhow::Result<()> {
    bot.send_message(
        msg.chat.id,
        "Please select the condition using the inline keyboard buttons.",
    )
    .reply_markup(keyboards::cancel_only())
    .await?;
    Ok(())
}

pub async fn handle_alert_step3(
    bot: Bot,
    msg: Message,
    dialogue: MyDialogue,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
) -> anyhow::Result<()> {
    let text = msg.text().unwrap_or("").trim();
    let threshold: f64 = match text.parse() {
        Ok(v) => v,
        Err(_) => {
            bot.send_message(
                msg.chat.id,
                "❌ Invalid number. Please enter a valid decimal number:",
            )
            .reply_markup(keyboards::cancel_only())
            .await?;
            return Ok(());
        }
    };

    let state = dialogue.get().await?;
    if let Some(State::AlertStep3 {
        symbol,
        condition,
        period,
        ..
    }) = state
    {
        let user_id = msg.chat.id.0 as u64;
        let id = store.write(user_id, |p| {
            let id = p.alloc_alert_id();
            p.alerts.push(AlertRule {
                id,
                symbol: symbol.clone(),
                condition: condition.clone(),
                threshold,
                period: period.clone(),
                cooldown_hours: 12,
                last_fired: None,
                consecutive_triggers: 0,
                enabled: true,
                label: None,
                last_fired_price: None,
            });
            id
        });

        let desc = crate::alerts::fmt_rule(&AlertRule {
            id,
            symbol: symbol.clone(),
            condition: condition.clone(),
            threshold,
            period: period.clone(),
            cooldown_hours: 12,
            last_fired: None,
            consecutive_triggers: 0,
            enabled: true,
            label: None,
            last_fired_price: None,
        });

        let warning =
            check_already_triggered_warning(&cmc, &symbol, &condition, threshold, &period).await;

        dialogue.exit().await?;

        bot.send_message(
            msg.chat.id,
            format!(
                "✅ *Alert \\#{id} created successfully\\!*\n{}{}",
                crate::format::esc(&desc),
                warning
            ),
        )
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(keyboards::alerts_footer())
        .await?;

        crate::bot::handlers::portfolio::auto_backup(&bot, &store, user_id).await;
    } else {
        dialogue.exit().await?;
        bot.send_message(msg.chat.id, "Dialogue state lost. Starting over.")
            .await?;
    }

    Ok(())
}

pub async fn handle_alert_list(
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
    let rules = store.read(user_id, |p| p.alerts.clone());

    if rules.is_empty() {
        bot.send_message(msg.chat.id, "🔕 No alerts configured\\.")
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(keyboards::alerts_footer())
            .await?;
        return Ok(());
    }

    bot.send_message(
        msg.chat.id,
        format!("*🔔 Alerts \\({} total\\)*", rules.len()),
    )
    .parse_mode(ParseMode::MarkdownV2)
    .await?;

    for r in &rules {
        let desc = crate::alerts::fmt_rule(r);
        let status = if r.enabled { "🟢" } else { "⏸" };
        let last_fired = r
            .last_fired
            .map(|t| t.format("%m/%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "never".to_string());
        let text = format!(
            "{} {}\n🕒 Last fired: {}\n⏱ Cooldown: {}h",
            status,
            crate::format::esc(&desc),
            crate::format::esc(&last_fired),
            r.cooldown_hours,
        );
        bot.send_message(msg.chat.id, text)
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(keyboards::alert_actions(r.id, r.enabled))
            .await?;
    }

    bot.send_message(msg.chat.id, "Use the buttons above to manage each alert\\.")
        .parse_mode(ParseMode::MarkdownV2)
        .reply_markup(keyboards::alerts_footer())
        .await?;

    Ok(())
}

pub async fn handle_alert_delete(
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

    let user_id = msg.chat.id.0 as u64;
    let removed = store.write(user_id, |p| {
        let before = p.alerts.len();
        p.alerts.retain(|r| r.id != id);
        p.alerts.len() < before
    });

    if removed {
        // Delete the alert message, fall back to edit on failure
        if bot.delete_message(msg.chat.id, msg.id).await.is_err() {
            let _ = bot
                .edit_message_text(msg.chat.id, msg.id, format!("🗑 Alert #{id} deleted"))
                .await;
        }
        crate::bot::handlers::portfolio::auto_backup(&bot, &store, user_id).await;
    } else {
        bot.send_message(msg.chat.id, format!("❌ Alert #{id} not found"))
            .await?;
    }
    Ok(())
}

pub async fn handle_alert_toggle(
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

    let user_id = msg.chat.id.0 as u64;
    let new_state = store.write(user_id, |p| {
        p.alerts.iter_mut().find(|r| r.id == id).map(|r| {
            r.enabled = !r.enabled;
            r.enabled
        })
    });

    match new_state {
        Some(enabled) => {
            // Update the inline keyboard so Pause <-> Enable reflects new state
            let _ = bot
                .edit_message_reply_markup(msg.chat.id, msg.id)
                .reply_markup(keyboards::alert_actions(id, enabled))
                .await;
            let label = if enabled {
                "▶️ Alert #"
            } else {
                "⏸ Alert #"
            };
            bot.send_message(
                msg.chat.id,
                format!("{label}{id} {}", if enabled { "enabled" } else { "paused" }),
            )
            .await?;
            crate::bot::handlers::portfolio::auto_backup(&bot, &store, user_id).await;
        }
        None => {
            bot.send_message(msg.chat.id, format!("❌ Alert #{id} not found"))
                .await?;
        }
    }
    Ok(())
}

/// Parse and create alert from command: /alert add BTC < 60000 [cooldown:12h]
/// Also handles: /alert add BTC 24h < -5%
pub async fn handle_alert_add_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
    cmc: Arc<CryptoClient>,
    args: String,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    // Parse: SYM [PERIOD] < | > THRESHOLD [cooldown:Nh]
    match parse_alert_expr(&args) {
        None => {
            bot.send_message(
                msg.chat.id,
                "Usage:\n\
                 `BTC < 60000` — price below 60k\n\
                 `ETH > 4000` — price above 4k\n\
                 `BTC 24h < -5%` — 24h drop over 5%\n\
                 `BTC 7d > 20%` — 7d rise over 20%\n\
                 Add `cooldown:12h` to set cooldown\\.",
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        }
        Some((symbol, cond, threshold, period, cooldown)) => {
            let user_id = msg.chat.id.0 as u64;
            let id = store.write(user_id, |p| {
                let id = p.alloc_alert_id();
                p.alerts.push(AlertRule {
                    id,
                    symbol: symbol.clone(),
                    condition: cond.clone(),
                    threshold,
                    period: period.clone(),
                    cooldown_hours: cooldown,
                    last_fired: None,
                    consecutive_triggers: 0,
                    enabled: true,
                    label: None,
                    last_fired_price: None,
                });
                id
            });

            let desc = crate::alerts::fmt_rule(&AlertRule {
                id,
                symbol: symbol.clone(),
                condition: cond.clone(),
                threshold,
                period: period.clone(),
                cooldown_hours: cooldown,
                last_fired: None,
                consecutive_triggers: 0,
                enabled: true,
                label: None,
                last_fired_price: None,
            });

            let warning =
                check_already_triggered_warning(&cmc, &symbol, &cond, threshold, &period).await;

            bot.send_message(
                msg.chat.id,
                format!(
                    "✅ *Alert \\#{id} created*\n{}{}",
                    crate::format::esc(&desc),
                    warning
                ),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .reply_markup(keyboards::alerts_footer())
            .await?;

            crate::bot::handlers::portfolio::auto_backup(&bot, &store, user_id).await;
        }
    }
    Ok(())
}

/// Flexible parser for alert expressions.
/// Formats: `SYM op VALUE`, `SYM PERIOD op VALUE%`, `SYM PERIOD op VALUE`
fn parse_alert_expr(s: &str) -> Option<(String, AlertCondition, f64, Option<Period>, u64)> {
    let parts: Vec<&str> = s.split_whitespace().collect();

    let mut symbol = String::new();
    let mut period: Option<Period> = None;
    let mut op: Option<&str> = None;
    let mut threshold: Option<f64> = None;
    let mut cooldown = 12u64;

    let mut i = 0;
    while i < parts.len() {
        let p = parts[i];
        if i == 0 {
            symbol = p.to_uppercase();
        } else if let Some(c) = p.strip_prefix("cooldown:") {
            cooldown = c.trim_end_matches('h').parse().unwrap_or(12);
        } else if matches!(p, "<" | ">") {
            op = Some(p);
        } else if period.is_none() && op.is_none() {
            period = parse_period(p);
        } else if op.is_some() && threshold.is_none() {
            let raw = p.trim_end_matches('%');
            threshold = raw.parse().ok();
        }
        i += 1;
    }

    let op = op?;
    let threshold = threshold?.abs(); // use abs; condition encodes direction
    let is_pct = period.is_some();

    let cond = match (op, is_pct) {
        ("<", false) => AlertCondition::PriceBelow,
        (">", false) => AlertCondition::PriceAbove,
        ("<", true) => AlertCondition::DropPercent,
        (">", true) => AlertCondition::RisePercent,
        _ => return None,
    };

    Some((symbol, cond, threshold, period, cooldown))
}

fn parse_period(s: &str) -> Option<Period> {
    match s.to_lowercase().as_str() {
        "1h" => Some(Period::OneHour),
        "4h" => Some(Period::FourHours),
        "24h" | "1d" => Some(Period::OneDay),
        "7d" | "1w" => Some(Period::SevenDays),
        "30d" | "1m" => Some(Period::ThirtyDays),
        "90d" | "3m" => Some(Period::NinetyDays),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_price_below() {
        let r = parse_alert_expr("BTC < 60000").unwrap();
        assert_eq!(r.0, "BTC");
        assert_eq!(r.1, AlertCondition::PriceBelow);
        assert_eq!(r.2, 60000.0);
        assert!(r.3.is_none());
    }

    #[test]
    fn parse_drop_percent() {
        let r = parse_alert_expr("ETH 24h < 5%").unwrap();
        assert_eq!(r.1, AlertCondition::DropPercent);
        assert_eq!(r.2, 5.0);
        assert_eq!(r.3, Some(Period::OneDay));
    }

    #[test]
    fn parse_with_cooldown() {
        let r = parse_alert_expr("BTC > 100000 cooldown:6h").unwrap();
        assert_eq!(r.4, 6);
    }
}

async fn check_already_triggered_warning(
    cmc: &CryptoClient,
    symbol: &str,
    condition: &AlertCondition,
    threshold: f64,
    period: &Option<Period>,
) -> String {
    if let Ok(quotes) = cmc.get_quotes(&[symbol]).await {
        if let Some(quote) = quotes
            .iter()
            .find(|q| q.symbol.eq_ignore_ascii_case(symbol))
        {
            let temp_rule = AlertRule {
                id: 0,
                symbol: symbol.to_string(),
                condition: condition.clone(),
                threshold,
                period: period.clone(),
                cooldown_hours: 12,
                last_fired: None,
                consecutive_triggers: 0,
                enabled: true,
                label: None,
                last_fired_price: None,
            };
            if crate::alerts::rules::evaluate(&temp_rule, quote) {
                let current_val = match condition {
                    AlertCondition::PriceBelow | AlertCondition::PriceAbove => {
                        format!("${:.2}", quote.price_usd)
                    }
                    _ => {
                        let pct = match period {
                            Some(Period::OneHour) => quote.change_1h.unwrap_or(0.0),
                            Some(Period::FourHours) => quote.change_24h.unwrap_or(0.0),
                            Some(Period::OneDay) => quote.change_24h.unwrap_or(0.0),
                            Some(Period::SevenDays) => quote.change_7d.unwrap_or(0.0),
                            Some(Period::ThirtyDays) => quote.change_30d.unwrap_or(0.0),
                            Some(Period::NinetyDays) => quote.change_90d.unwrap_or(0.0),
                            None => quote.change_24h.unwrap_or(0.0),
                        };
                        format!("{:.2}%", pct)
                    }
                };
                return format!(
                    "\n\n⚠️ *Warning*: This condition is *already met*\\!\nCurrent value: *{}*",
                    crate::format::esc(&current_val)
                );
            }
        }
    }
    String::new()
}

pub async fn handle_alert_reset(
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

    store.write(user_id, |p| {
        let def = crate::models::Portfolio::default();
        p.alerts = def.alerts;
        p.next_alert_id = def.next_alert_id;
    });

    bot.send_message(msg.chat.id, "✅ Default alerts restored successfully!")
        .await?;
    crate::bot::handlers::portfolio::auto_backup(&bot, &store, user_id).await;
    Ok(())
}
