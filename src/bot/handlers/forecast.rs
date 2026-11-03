//! `/forecast [SYM ...]` — deterministic multi-horizon price forecast.
//!
//! Horizons: 1, 3, 5, 7, 14, 30, 90, 180 days. The numbers come from
//! [`crate::trading::forecast`] (drift + volatility over the local daily
//! closes, merged with the intraday rows the price worker stores); no AI call
//! is involved, so the command is cheap and cannot fail on a provider quota.

use std::sync::Arc;
use std::time::Duration;

use teloxide::{
    prelude::*,
    types::{CallbackQuery, ChatId, MessageId, ParseMode},
};
use tracing::warn;

use crate::{
    bot::keyboards,
    config::Config,
    crypto::{ai::GeminiClient, CryptoClient},
    db::{SqliteStore, Store},
    trading::{
        forecast::{self as fx, Forecast},
        history_file,
    },
};

/// Load the daily close series for `symbol` from every available source:
/// 1. the on-disk `data/history/{SYM}.csv` cache (years of daily closes),
/// 2. intraday rows from SQLite resampled to one close per day (last wins),
/// 3. provider history as a last resort when the cache is empty.
pub async fn load_series(
    symbol: &str,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
) -> Vec<(i64, f64)> {
    let sym = symbol.to_uppercase();

    let mut series = history_file::load_history(&sym).unwrap_or_default();
    if let Ok(rows) = sqlite.get_price_history(&sym, 220).await {
        if !rows.is_empty() {
            series = fx::merge_daily_history(&series, &rows);
        }
    }
    // The container's /app/data is a volume, so the shipped CSV cache may not be
    // present there; also top the series up from the providers when it is short
    // or does not reach back a full year.
    let covers_year = series
        .first()
        .map(|&(ts, _)| chrono::Utc::now().timestamp_millis() - ts >= 300 * 86_400_000)
        .unwrap_or(false);
    if series.len() < 200 || !covers_year {
        if let Ok(h) = cmc.get_history(&sym, "365d").await {
            if !h.is_empty() {
                series = fx::merge_daily_history(&series, &h);
            }
        }
    }
    series
}

/// Assemble the daily series for `symbol` and build the deterministic forecast.
pub async fn build_forecast(
    symbol: &str,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    live_price: Option<f64>,
) -> anyhow::Result<Forecast> {
    let sym = symbol.to_uppercase();
    let series = load_series(&sym, cmc, sqlite).await;

    let price = live_price
        .filter(|p| *p > 0.0)
        .or_else(|| series.last().map(|&(_, p)| p))
        .ok_or_else(|| anyhow::anyhow!("no price or history available for {sym}"))?;

    Ok(fx::forecast(&sym, price, &series))
}

pub async fn handle_forecast_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    sqlite: Arc<SqliteStore>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);
    if !config.is_admin(user_id) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    // `/forecast` → the user's watchlist; `/forecast BTC ETH` → those symbols.
    // `/forecast verify [SYM ...]` → walk-forward accuracy of the shipped model
    // on realized history (this is how "была ли модель точна" is answered).
    let words: Vec<&str> = args.split_whitespace().collect();
    let verify = words
        .first()
        .map(|w| {
            matches!(
                w.to_lowercase().as_str(),
                "verify" | "accuracy" | "check" | "backtest"
            )
        })
        .unwrap_or(false);
    let symbol_words: &[&str] = if verify { &words[1..] } else { &words };
    let mut symbols: Vec<String> = symbol_words
        .iter()
        .filter(|w| w.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|w| w.to_uppercase())
        .collect();
    if symbols.is_empty() {
        symbols = store.read(user_id, |p| {
            let mut v = p.settings.default_watchlist.clone();
            if v.is_empty() {
                v = p.holdings_by_symbol().into_keys().collect();
                v.sort();
            }
            v
        });
        symbols.truncate(4);
    }
    if symbols.is_empty() {
        symbols = vec!["BTC".into(), "ETH".into()];
    }

    if verify {
        for sym in &symbols {
            let history = load_series(sym, &cmc, &sqlite).await;
            let text = if history.len() < 200 {
                format!(
                    "⚠️ <b>{sym}</b>: мало истории для бэктеста ({} дн.)",
                    history.len()
                )
            } else {
                format!(
                    "<pre>{}</pre>",
                    html_escape::encode_text(&fx::backtest_report(sym, &history))
                )
            };
            if let Err(e) = bot
                .send_message(ChatId(msg.chat.id.0), &text)
                .parse_mode(ParseMode::Html)
                .await
            {
                warn!("Forecast verify send failed for {sym}: {e}");
            }
        }
        return Ok(());
    }

    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = cmc.get_quotes(&sym_refs).await.unwrap_or_default();
    let price_for = |sym: &str| {
        quotes
            .iter()
            .find(|q| q.symbol.eq_ignore_ascii_case(sym))
            .map(|q| q.price_usd)
    };

    for sym in &symbols {
        let avg_entry = store.read(user_id, |p| p.avg_buy_price(sym));
        // Forecast reports are English-only by product decision; do not localise
        // them from `preferred_language` (chat language is a separate setting).
        let russian = false;
        let (markdown, html) = match build_forecast(sym, &cmc, &sqlite, price_for(sym)).await {
            Ok(f) => (
                f.render_markdown(avg_entry, russian),
                f.render_html_for(avg_entry, russian),
            ),
            Err(e) => {
                warn!("Forecast for {sym} failed: {e}");
                let text = format!("⚠️ **{sym}**: нет данных для прогноза ({e})");
                (text.clone(), html_escape::encode_text(&text).to_string())
            }
        };
        send_forecast_report(&bot, &config, ChatId(msg.chat.id.0), sym, &markdown, &html).await;
    }

    Ok(())
}

/// Send one forecast: the rich markdown table (with an HTML fallback), then the
/// action keyboard on its own small message.
///
/// The keyboard deliberately does not ride on the rich message. Bot API 10.1
/// rich messages come back to teloxide 0.12 as a message kind its `Update`
/// deserializer does not know, so the whole `callback_query` is discarded inside
/// the polling stream — before any handler, log line or filter can see it. That
/// is why every button on the forecast table looked dead while the table itself
/// rendered fine. A regular `sendMessage` strip always parses.
pub(crate) async fn send_forecast_report(
    bot: &Bot,
    config: &Config,
    chat_id: ChatId,
    sym: &str,
    markdown: &str,
    html: &str,
) {
    if let Err(e) =
        crate::bot::rich_messages::send_rich_message_markdown(config, chat_id.0, markdown).await
    {
        warn!("Rich forecast send failed for {sym} ({e}); falling back to HTML");
        if let Err(e) = bot
            .send_message(chat_id, html)
            .parse_mode(ParseMode::Html)
            .await
        {
            warn!("Forecast send failed for {sym}: {e}");
            return;
        }
    }
    if let Err(e) = send_actions_strip(bot, chat_id, sym).await {
        warn!("Forecast action strip failed for {sym}: {e}");
    }
}

/// The one-line message that carries the forecast buttons.
async fn send_actions_strip(bot: &Bot, chat_id: ChatId, sym: &str) -> anyhow::Result<Message> {
    let b = bot
        .send_message(chat_id, format!("⚡ <b>{sym}</b> · actions"))
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboards::forecast_actions(sym));
    Ok(b.await?)
}

/// Where a callback should be answered and whether it can be edited in place.
///
/// Rich messages (`sendRichMessage`) come back to teloxide 0.12 without a
/// parseable `message`, so requiring one made every inline button on a rich
/// forecast a silent no-op. Falling back to the caller's private chat and
/// sending a fresh message keeps the button alive either way.
pub(crate) fn callback_target(q: &CallbackQuery) -> (ChatId, Option<MessageId>) {
    match q.message.as_ref() {
        Some(m) => (m.chat.id, Some(m.id)),
        None => (ChatId(q.from.id.0 as i64), None),
    }
}

/// Callbacks attached to a forecast message:
/// * `fc:ai:SYM` — ask the AI to narrate the deterministic table.
/// * `fc:fx:SYM` — rebuild the forecast from the freshest data, in place.
pub async fn handle_forecast_callback(
    bot: Bot,
    q: CallbackQuery,
    cmc: Arc<CryptoClient>,
    sqlite: Arc<SqliteStore>,
    config: Arc<Config>,
    store: Arc<Store>,
) -> anyhow::Result<()> {
    let _ = bot.answer_callback_query(&q.id).await;

    let data = q.data.clone().unwrap_or_default();
    let (chat_id, message_id) = callback_target(&q);
    let user_id = q.from.id.0;
    tracing::info!(
        "forecast callback: data={data} chat={} in_place={}",
        chat_id.0,
        message_id.is_some()
    );
    if !config.is_admin(user_id) {
        bot.send_message(chat_id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let mut parts = data.splitn(3, ':');
    let _ = parts.next(); // "fc"
    let action = parts.next().unwrap_or_default();
    let sym = parts.next().unwrap_or_default().to_uppercase();
    if sym.is_empty() || !sym.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(());
    }

    match action {
        "ai" => {
            let loading = bot
                .send_message(chat_id, format!("🧠 Analysing <b>{sym}</b>…"))
                .parse_mode(ParseMode::Html)
                .await?;
            let loading_id = loading.id;
            let bot_task = bot.clone();
            let store_task = store.clone();
            // The AI call can take tens of seconds; never block the dispatcher.
            tokio::spawn(async move {
                let quotes = cmc.get_quotes(&[sym.as_str()]).await.unwrap_or_default();
                let live = quotes.first().map(|q| q.price_usd);
                let forecast = build_forecast(&sym, &cmc, &sqlite, live).await;
                let _ = bot_task.delete_message(chat_id, loading_id).await;

                let f = match forecast {
                    Ok(f) => f,
                    Err(e) => {
                        warn!("Forecast for {sym} failed in AI callback: {e}");
                        let _ = bot_task
                            .send_message(chat_id, format!("⚠️ {sym}: нет данных ({e})"))
                            .await;
                        return;
                    }
                };

                let ai = GeminiClient::new(&config.gemini_api_key);
                // Portfolio-aware verdict lines are always shown, even when the
                // AI read succeeds: the user asked for a clear bottom line.
                let avg_entry = store_task.read(user_id, |p| p.avg_buy_price(&sym));
                // Forecast is English-only.
                let russian = false;
                let verdict = f
                    .action_lines(avg_entry, russian)
                    .into_iter()
                    .map(|l| format!("• {}", html_escape::encode_text(&l)))
                    .collect::<Vec<_>>()
                    .join("\n");
                // Commentary is a fast single-pass call: it must not depend on a
                // single provider's key (Gemini 429s constantly), so the guard is
                // gone and the multi-provider chain decides.
                let commentary = match tokio::time::timeout(
                    Duration::from_secs(50),
                    ai.forecast_commentary(&f.symbol, &f.render(), sqlite.clone()),
                )
                .await
                {
                    Ok(Ok(t)) => {
                        if t.trim().is_empty() {
                            None
                        } else {
                            Some(t)
                        }
                    }
                    Ok(Err(e)) => {
                        warn!("Forecast AI commentary failed for {sym}: {e}");
                        None
                    }
                    Err(_) => {
                        warn!("Forecast AI commentary timed out for {sym}");
                        None
                    }
                };

                let text = match commentary {
                    Some(t) => format!(
                        "🧠 <b>{sym} — AI read</b>\n\n{t}\n\n🎯 <b>{}</b>\n{verdict}",
                        if russian { "Итог" } else { "Bottom line" }
                    ),
                    None => format!(
                        "{}\n\n🎯 <b>{}</b>\n{verdict}",
                        f.summary_html(),
                        if russian { "Итог" } else { "Bottom line" }
                    ),
                };

                // The model is asked for `<b>` only, but never trust that: fall
                // back to plain text if Telegram rejects the HTML.
                let sent = bot_task
                    .send_message(chat_id, &text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::forecast_actions(&sym))
                    .await;
                if sent.is_err() {
                    let _ = bot_task
                        .send_message(chat_id, &text)
                        .reply_markup(keyboards::forecast_actions(&sym))
                        .await;
                }
            });
        }
        "fx" => {
            let quotes = cmc.get_quotes(&[sym.as_str()]).await.unwrap_or_default();
            let live = quotes.first().map(|q| q.price_usd);
            let avg_entry = store.read(user_id, |p| p.avg_buy_price(&sym));
            // Forecast is English-only.
            let russian = false;
            let (markdown, html) = match build_forecast(&sym, &cmc, &sqlite, live).await {
                Ok(f) => (
                    f.render_markdown(avg_entry, russian),
                    f.render_html_for(avg_entry, russian),
                ),
                Err(e) => {
                    warn!("Forecast refresh for {sym} failed: {e}");
                    let text = format!("⚠️ <b>{sym}</b>: нет данных для прогноза ({e})");
                    (text.clone(), text)
                }
            };
            // Drop the message the button sat on (the strip, the old table or the
            // AI read) so a refresh never stacks three copies of the same report.
            if let Some(mid) = message_id {
                let _ = bot.delete_message(chat_id, mid).await;
            }
            send_forecast_report(&bot, &config, chat_id, &sym, &markdown, &html).await;
        }
        _ => {}
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::callback_target;
    use teloxide::types::CallbackQuery;

    fn query(json: &str) -> CallbackQuery {
        serde_json::from_str(json).expect("valid CallbackQuery")
    }

    #[test]
    fn callback_without_a_message_still_has_a_target() {
        // The prod bug: a button on a rich message arrives with no `message`,
        // and requiring it turned every tap into a silent no-op.
        let q = query(
            r#"{"id":"1","from":{"id":123456789,"is_bot":false,"first_name":"N"},
                "chat_instance":"x","data":"fc:fx:BTC"}"#,
        );
        assert!(q.message.is_none());
        let (chat_id, message_id) = callback_target(&q);
        assert_eq!(chat_id.0, 123456789, "must answer in the caller's chat");
        assert!(
            message_id.is_none(),
            "nothing to edit — send a fresh message"
        );
    }

    #[test]
    fn callback_with_a_message_edits_in_place() {
        let q = query(
            r#"{"id":"2","from":{"id":1,"is_bot":false,"first_name":"A"},
                "message":{"message_id":42,"date":1700000000,
                  "chat":{"id":777,"type":"private","first_name":"A"},
                  "text":"forecast"},
                "chat_instance":"y","data":"fc:ai:ETH"}"#,
        );
        let (chat_id, message_id) = callback_target(&q);
        assert_eq!(chat_id.0, 777);
        assert_eq!(message_id.expect("message id").0, 42);
    }
}
