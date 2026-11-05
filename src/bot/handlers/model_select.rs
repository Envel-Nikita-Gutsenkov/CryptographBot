use crate::{
    config::Config,
    crypto::ai::providers::available_models,
    db::{SqliteStore, Store},
};
use std::sync::Arc;
use teloxide::{prelude::*, types::ParseMode};

pub async fn handle_model_cmd(
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

    let parts: Vec<&str> = args.trim().splitn(2, ' ').collect();
    match parts.as_slice() {
        ["list"] | [] => show_model_list(&bot, msg.chat.id).await,
        ["set", rest] => set_model(&bot, msg.chat.id, store, rest.trim()).await,
        _ => show_model_list(&bot, msg.chat.id).await,
    }
}

async fn show_model_list(bot: &Bot, chat_id: ChatId) -> anyhow::Result<()> {
    let models = available_models();
    let mut lines = vec!["🤖 *Available AI Models*\n".to_string()];
    for (i, m) in models.iter().enumerate() {
        lines.push(format!("{}\\. `{}` — {}", i + 1, m.id, m.display));
    }
    lines.push("\n_Use_ `/model set <id_or_number>` _to select\\._\n_Use_ `/model set default` _to reset\\._".to_string());
    bot.send_message(chat_id, lines.join("\n"))
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
    Ok(())
}

async fn set_model(
    bot: &Bot,
    chat_id: ChatId,
    store: Arc<Store>,
    input: &str,
) -> anyhow::Result<()> {
    let user_id = chat_id.0 as u64;
    let models = available_models();

    if input == "default" || input == "reset" {
        store.write(user_id, |p| p.settings.ai_model_override = None);
        crate::bot::handlers::portfolio::auto_backup(bot, &store, user_id).await;
        bot.send_message(
            chat_id,
            "✅ AI model reset to default chain \\(Gemini Pro → Flash → Mistral\\)\\.",
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
        return Ok(());
    }

    // Try numeric index first
    let model = if let Ok(n) = input.parse::<usize>() {
        models.get(n.saturating_sub(1))
    } else {
        models.iter().find(|m| m.id == input)
    };

    match model {
        Some(m) => {
            store.write(user_id, |p| {
                p.settings.ai_model_override = Some(m.id.to_string())
            });
            crate::bot::handlers::portfolio::auto_backup(bot, &store, user_id).await;
            bot.send_message(
                chat_id,
                format!("✅ AI model set to *{}* \\(`{}`\\)\\.", m.display, m.id),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        }
        None => {
            bot.send_message(
                chat_id,
                format!("❌ Unknown model `{input}`\\. Use `/model list` to see options\\."),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        }
    }
    Ok(())
}

/// `/aihealth` — provider/model reliability plus the current bench state.
/// Everything here is read from real traffic (`model_stats`) and the in-memory
/// benches, so it shows exactly what the pool will try next.
pub async fn handle_aihealth_cmd(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    sqlite: Arc<SqliteStore>,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let stats = sqlite.get_model_stats(7).unwrap_or_default();
    let health = sqlite.get_all_model_health().unwrap_or_default();
    let benched = crate::crypto::ai::pools::quota_report();
    let dead = crate::crypto::ai::pools::dead_models_today();

    let text = build_aihealth_text(&stats, &health, benched.as_deref(), &dead);

    bot.send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

/// Render the `/aihealth` body. Split out from the handler so the layout and
/// the 4096-char Telegram limit can be tested without a network call.
pub(crate) fn build_aihealth_text(
    stats: &[(String, String, i64, i64, i64, f64)],
    health: &[(String, String, String)],
    benched: Option<&str>,
    dead: &[String],
) -> String {
    let mut body = String::new();
    body.push_str("provider   model                      calls  err%    ms\n");
    for (provider, model, calls, errors, latency, _q) in stats.iter().take(28) {
        let err_pct = if *calls > 0 {
            *errors as f64 / *calls as f64 * 100.0
        } else {
            0.0
        };
        body.push_str(&format!(
            "{:<10} {:<26} {:>5} {:>5.1} {:>5}\n",
            truncate(provider, 10),
            truncate(model, 26),
            calls,
            err_pct,
            latency
        ));
    }
    if stats.is_empty() {
        body.push_str("no traffic recorded yet\n");
    }

    let down: Vec<String> = health
        .iter()
        .filter(|(_, status, _)| status == "COOLDOWN" || status == "DEAD")
        .map(|(id, status, _)| format!("{} {}", status, id))
        .collect();

    let mut tail = String::new();
    tail.push_str(&format!(
        "\nBenched providers: {}\n",
        benched.unwrap_or("none")
    ));
    tail.push_str(&format!(
        "Benched models: {}\n",
        if down.is_empty() {
            "none".to_string()
        } else {
            down.join(", ")
        }
    ));
    tail.push_str(&format!(
        "Skipped as dead today (0% success): {}",
        if dead.is_empty() {
            "none".to_string()
        } else {
            dead.join(", ")
        }
    ));

    let text = format!(
        "🤖 <b>AI health · 7 days</b>\n<pre>{}</pre>{}",
        escape_html(&body),
        escape_html(&tail)
    );

    // Telegram caps messages at 4096 chars; the table is bounded by take(28).
    if text.chars().count() > 4000 {
        text.chars().take(4000).collect::<String>()
    } else {
        text
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// HTML parse mode needs `<`, `>` and `&` escaped only; `crate::format::esc`
/// produces MarkdownV2 backslashes, which would litter the table.
fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        provider: &str,
        model: &str,
        calls: i64,
        errors: i64,
    ) -> (String, String, i64, i64, i64, f64) {
        (
            provider.to_string(),
            model.to_string(),
            calls,
            errors,
            1_200,
            0.0,
        )
    }

    #[test]
    fn aihealth_renders_rates_and_state() {
        let stats = vec![row("Cohere", "command-a-plus", 10, 5)];
        let health = vec![(
            "Gemini/gemini-3.5-flash".to_string(),
            "COOLDOWN".to_string(),
            String::new(),
        )];
        let text = build_aihealth_text(
            &stats,
            &health,
            Some("Cohere (до 00:00 UTC)"),
            &["gpt-oss-20b".to_string()],
        );
        assert!(text.contains("command-a-plus"));
        assert!(text.contains("50.0"), "error rate must be rendered");
        assert!(text.contains("COOLDOWN Gemini/gemini-3.5-flash"));
        assert!(text.contains("Benched providers: Cohere"));
        assert!(text.contains("gpt-oss-20b"));
    }

    #[test]
    fn aihealth_handles_empty_state() {
        let text = build_aihealth_text(&[], &[], None, &[]);
        assert!(text.contains("no traffic recorded yet"));
        assert!(text.contains("Benched providers: none"));
        assert!(text.contains("Skipped as dead today (0% success): none"));
    }

    #[test]
    fn aihealth_stays_within_telegram_limit() {
        let stats: Vec<_> = (0..100)
            .map(|i| row(&format!("P{i}"), &format!("m{i}"), 10, i))
            .collect();
        let text = build_aihealth_text(&stats, &[], None, &[]);
        assert!(text.chars().count() <= 4000);
    }
}
