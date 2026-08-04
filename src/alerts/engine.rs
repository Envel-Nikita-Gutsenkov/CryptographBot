//! Background alert engine.
//! Wakes every `Config::alert_interval_secs`, evaluates all rules,
//! enforces cooldown + 2-consecutive-trigger dedup before firing.

use std::sync::Arc;

use chrono::Utc;
use teloxide::{prelude::*, types::ChatId};
use tokio::time::{interval, Duration};
use tracing::{debug, error, warn};

use crate::{alerts::rules, config::Config, crypto::CryptoClient, db::Store, format::fmt_price};

pub async fn run(bot: Bot, store: Arc<Store>, cmc: Arc<CryptoClient>, config: Arc<Config>) {
    let tick = Duration::from_secs(config.alert_interval_secs);
    let mut ticker = interval(tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        debug!("Alert engine tick");

        let user_ids = store.all_user_ids();
        for user_id in user_ids {
            if let Err(e) = process_user(&bot, &store, &cmc, &config, user_id).await {
                warn!("Alert check for user {user_id}: {e}");
            }
        }
    }
}

async fn process_user(
    bot: &Bot,
    store: &Store,
    cmc: &CryptoClient,
    config: &Config,
    user_id: u64,
) -> anyhow::Result<()> {
    let (symbols, rules_snapshot): (Vec<String>, Vec<crate::models::AlertRule>) =
        store.read(user_id, |p| {
            let symbols: Vec<String> = p
                .alerts
                .iter()
                .filter(|r| r.enabled)
                .map(|r| r.symbol.clone())
                .collect();
            let rules = p.alerts.clone();
            (symbols, rules)
        });

    if symbols.is_empty() || is_quiet_hour(store, user_id) {
        return Ok(());
    }

    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = match cmc.get_quotes(&sym_refs).await {
        Ok(q) => q,
        Err(e) => {
            warn!("CMC error during alert check: {e}");
            return Ok(());
        }
    };

    let mut triggered = Vec::new();

    for rule in &rules_snapshot {
        if !rule.enabled {
            continue;
        }

        let quote = match quotes
            .iter()
            .find(|q| q.symbol.eq_ignore_ascii_case(&rule.symbol))
        {
            Some(q) => q,
            None => continue,
        };

        if !rules::is_basic_condition_met(rule, quote) {
            store.write(user_id, |p| {
                if let Some(r) = p.alerts.iter_mut().find(|r| r.id == rule.id) {
                    let mut modified = r.consecutive_triggers != 0;
                    r.consecutive_triggers = 0;
                    if r.last_fired_price.is_some() {
                        r.last_fired_price = None;
                        modified = true;
                    }
                    modified
                } else {
                    false
                }
            });
            continue;
        }

        if !rules::evaluate(rule, quote) {
            continue;
        }

        let (triggers, can_fire) = store.write(user_id, |p| {
            if let Some(r) = p.alerts.iter_mut().find(|r| r.id == rule.id) {
                r.consecutive_triggers = r.consecutive_triggers.saturating_add(1);
                let ok = r.consecutive_triggers >= 1 && rules::cooldown_ok(r);
                (r.consecutive_triggers, ok)
            } else {
                (0, false)
            }
        });

        if !can_fire {
            debug!(
                "Rule #{} consecutive={triggers}, waiting for dedup or cooldown",
                rule.id
            );
            continue;
        }

        triggered.push((rule.clone(), quote.clone()));
    }

    if triggered.is_empty() {
        return Ok(());
    }

    // Combine all triggered alerts into one message
    let mut combined_msg = String::new();
    let mut prompt_context = String::new();

    for (i, (rule, quote)) in triggered.iter().enumerate() {
        let detail_escaped = build_alert_message(rule, quote);

        if i > 0 {
            combined_msg.push_str("\n\n---\n\n");
        }
        combined_msg.push_str(&detail_escaped);

        prompt_context.push_str(&format!(
            "- {}: crossed {} ({})\n",
            rule.symbol, rule.threshold, rule.condition
        ));
    }

    // Request AI summary for ALL alerts combined
    let ai = crate::crypto::ai::GeminiClient::new(&config.gemini_api_key);
    if ai.is_configured() {
        let req_client = reqwest::Client::new();
        let prompt = format!(
            "The following crypto alerts just triggered simultaneously:\n{}\n\nProvide a very short, combined summary of the overall market situation. Max 2-3 sentences. Do not use asterisks or markdown, just plain text.",
            prompt_context
        );
        if let Ok(comment) = crate::crypto::ai::pipeline::call_with_critic(
            &req_client,
            &config.gemini_api_key,
            &prompt,
            None,
            false,
            true,
            cmc.sqlite().unwrap(),
        )
        .await
        {
            if !comment.is_empty() {
                let escaped_comment = html_escape(&comment.trim().to_string());
                combined_msg.push_str(&format!(
                    "\n\n🤖 <b>AI Market Summary:</b> <i>{}</i>",
                    escaped_comment
                ));
            }
        }
    }

    if let Err(e) = bot
        .send_message(ChatId(user_id as i64), &combined_msg)
        .parse_mode(teloxide::types::ParseMode::Html)
        .await
    {
        let msg = e.to_string();
        if msg.contains("Forbidden") {
            // Recipient can't be messaged (blocked the bot, or the "user" is
            // actually another bot). Disable their alerts once so we stop
            // retrying and spamming ERROR every cycle instead of forever.
            store.write(user_id, |p| {
                for r in &mut p.alerts {
                    r.enabled = false;
                }
            });
            warn!("Disabled alerts for {user_id}: bot cannot message this chat: {msg}");
        } else {
            error!("Failed to send batched alert to {user_id}: {msg}");
        }
    } else {
        // Mark all fired + reset counters
        store.write(user_id, |p| {
            for (rule, quote) in &triggered {
                if let Some(r) = p.alerts.iter_mut().find(|r| r.id == rule.id) {
                    r.last_fired = Some(Utc::now());
                    r.consecutive_triggers = 0;
                    r.last_fired_price = Some(quote.price_usd);
                }
            }
        });
    }

    Ok(())
}

fn is_quiet_hour(store: &Store, user_id: u64) -> bool {
    store.read(user_id, |p| {
        let (start, end) = match (
            p.settings.alert_quiet_hours_start,
            p.settings.alert_quiet_hours_end,
        ) {
            (Some(s), Some(e)) => (s, e),
            _ => return false,
        };
        let hour = Utc::now()
            .format("%H")
            .to_string()
            .parse::<u8>()
            .unwrap_or(0);
        if start <= end {
            hour >= start && hour < end
        } else {
            hour >= start || hour < end
        }
    })
}

fn build_alert_message(rule: &crate::models::AlertRule, quote: &crate::models::CmcQuote) -> String {
    use crate::models::AlertCondition;

    let esc_threshold = html_escape(&rule.threshold.to_string());
    let esc_price = html_escape(&fmt_price(quote.price_usd));
    let esc_period = html_escape(
        &rule
            .period
            .as_ref()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "24h".to_string()),
    );

    let detail = match rule.condition {
        AlertCondition::PriceBelow => format!(
            "Price dropped below <b>{}</b>\nCurrent: <b>{}</b>",
            esc_threshold, esc_price
        ),
        AlertCondition::PriceAbove => format!(
            "Price rose above <b>{}</b>\nCurrent: <b>{}</b>",
            esc_threshold, esc_price
        ),
        AlertCondition::DropPercent => {
            let actual = rules::period_change(quote, rule.period.as_ref()).abs();
            let esc_actual = html_escape(&format!("{:.2}", actual));
            format!(
                "Dropped <b>{}%</b> over {} (threshold: {}%)\nCurrent price: <b>{}</b>",
                esc_actual, esc_period, esc_threshold, esc_price
            )
        }
        AlertCondition::RisePercent => {
            let actual = rules::period_change(quote, rule.period.as_ref()).abs();
            let esc_actual = html_escape(&format!("{:.2}", actual));
            format!(
                "Rose <b>{}%</b> over {} (threshold: {}%)\nCurrent price: <b>{}</b>",
                esc_actual, esc_period, esc_threshold, esc_price
            )
        }
    };

    let label = rule
        .label
        .as_deref()
        .map(|l| format!(" — {}", html_escape(l)))
        .unwrap_or_default();

    format!(
        "🔔 <b>Alert #{} — {}</b>{}\n\n{}",
        rule.id,
        html_escape(&rule.symbol),
        label,
        detail
    )
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
