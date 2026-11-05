use crate::{
    bot::{handlers::forecast::callback_target, keyboards},
    config::Config,
    crypto::ai::providers::{available_models, probe_all_models, ProbeResult},
    db::{SqliteStore, Store},
};
use std::sync::Arc;
use teloxide::{
    prelude::*,
    types::{CallbackQuery, ChatId, ParseMode},
};

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
        .reply_markup(keyboards::aihealth_actions())
        .await?;
    Ok(())
}

/// `aih:probe` — re-probe every configured model against the live provider.
/// Admin-only, since it deliberately ignores the circuit breakers.
pub async fn handle_aihealth_callback(
    bot: Bot,
    q: CallbackQuery,
    config: Arc<Config>,
    sqlite: Arc<SqliteStore>,
) -> anyhow::Result<()> {
    let _ = bot.answer_callback_query(&q.id).await;
    let data = q.data.clone().unwrap_or_default();
    if data != "aih:probe" {
        return Ok(());
    }
    let (chat, _) = callback_target(&q);
    if !config.is_admin(q.from.id.0) {
        bot.send_message(chat, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let total = available_models().len();
    let loading = bot
        .send_message(
            chat,
            format!("🔄 Probing {total} models against their live APIs…\nThis takes up to ~60s."),
        )
        .await?;

    // Dedicated client: the shared one has an 8s budget, too tight for a
    // cold provider plus a reasoning model. Probes run in the background so the
    // callback is answered immediately instead of leaving a dead spinner.
    tokio::spawn(async move {
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(25))
            .use_rustls_tls()
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = bot
                    .edit_message_text(chat, loading.id, format!("❌ HTTP client: {e}"))
                    .await;
                return;
            }
        };

        let results = probe_all_models(&client, &config.gemini_api_key, Some(sqlite)).await;
        let text = build_probe_text(&results);

        // The loading message carries no markup; if editing fails (e.g. the
        // message was deleted) fall back to a new message so results are never lost.
        let edited = bot
            .edit_message_text(chat, loading.id, text.clone())
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboards::aihealth_actions())
            .await
            .is_ok();
        if !edited {
            let _ = bot
                .send_message(chat, text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboards::aihealth_actions())
                .await;
        }
    });

    Ok(())
}

/// One-line reason for a probe failure, derived from the HTTP status.
fn probe_note(r: &ProbeResult) -> &'static str {
    if r.ok {
        return "ok";
    }
    match r.status {
        0 => "no answer",
        401 | 403 => "auth/key",
        402 => "no credits",
        404 | 410 => "no model",
        408 => "timeout",
        413 => "too large",
        429 => "quota/429",
        500..=599 => "server",
        _ => "error",
    }
}

/// Render the live-probe result. Pure so the layout and the 4096-char Telegram
/// limit are testable without a network call.
pub(crate) fn build_probe_text(results: &[ProbeResult]) -> String {
    let ok_count = results.iter().filter(|r| r.ok).count();
    let total = results.len();
    let slowest = results.iter().map(|r| r.latency_ms).max().unwrap_or(0);

    let mut body = String::from("st   provider   model                    ms   note\n");
    for r in results {
        body.push_str(&format!(
            "{:<4} {:<10} {:<23} {:>5} {}\n",
            if r.ok { "200" } else { "---" },
            truncate(&r.provider, 10),
            truncate(&r.model, 23),
            r.latency_ms,
            probe_note(r),
        ));
    }
    if results.is_empty() {
        body.push_str("no models configured\n");
    }

    let mut failures = String::new();
    for r in results.iter().filter(|r| !r.ok) {
        let code = if r.status == 0 {
            "-".to_string()
        } else {
            r.status.to_string()
        };
        failures.push_str(&format!(
            "\n{} {} [{}] {}",
            r.provider,
            r.model,
            code,
            truncate(r.detail.trim(), 120),
        ));
    }

    let header = format!("🔍 <b>Live probe</b> · {ok_count}/{total} ok · slowest {slowest} ms");
    let tail = if failures.is_empty() {
        "\nAll providers answered.".to_string()
    } else {
        format!("\n<b>Failures</b>{failures}")
    };

    let text = format!(
        "{header}\n<pre>{}</pre>{}",
        escape_html(&body),
        escape_html(&tail)
    );
    if text.chars().count() > 4000 {
        text.chars().take(4000).collect::<String>()
    } else {
        text
    }
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

    fn probe(provider: &str, model: &str, status: u16, ok: bool, detail: &str) -> ProbeResult {
        ProbeResult {
            provider: provider.to_string(),
            model: model.to_string(),
            status,
            ok,
            latency_ms: 1234,
            detail: detail.to_string(),
        }
    }

    #[test]
    fn probe_text_counts_and_labels_failures() {
        let results = vec![
            probe("Mistral", "mistral-large-4", 200, true, ""),
            probe(
                "Gemini",
                "gemini-3.5-flash",
                429,
                false,
                "Gemini gemini-3.5-flash HTTP 429: quota exceeded",
            ),
            probe(
                "Cerebras",
                "gpt-oss-120b",
                402,
                false,
                "HTTP 402: payment required",
            ),
        ];
        let text = build_probe_text(&results);
        assert!(text.contains("1/3 ok"), "got: {text}");
        assert!(text.contains("quota/429"));
        assert!(text.contains("no credits"));
        assert!(text.contains("Failures"));
        // Raw provider error stays visible for diagnosis.
        assert!(text.contains("quota exceeded"));
    }

    #[test]
    fn probe_text_all_green_has_no_failure_section() {
        let results = vec![probe("Groq", "gpt-oss-120b", 200, true, "")];
        let text = build_probe_text(&results);
        assert!(text.contains("1/1 ok"));
        assert!(text.contains("All providers answered"));
        assert!(!text.contains("Failures"));
    }

    #[test]
    fn probe_text_escapes_html_and_stays_small() {
        let results: Vec<_> = (0..40)
            .map(|i| {
                probe(
                    "Provider<&",
                    &format!("model-{i}<tag>"),
                    500,
                    false,
                    "HTTP 500: <html>error</html>",
                )
            })
            .collect();
        let text = build_probe_text(&results);
        assert!(!text.contains("<tag>"));
        assert!(!text.contains("<html>"));
        assert!(text.chars().count() <= 4000);
    }
}
