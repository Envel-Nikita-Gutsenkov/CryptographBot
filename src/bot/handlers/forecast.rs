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
    types::{CallbackQuery, ChatId, ParseMode},
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
        let russian = store
            .read(user_id, |p| p.settings.preferred_language.clone())
            .to_lowercase()
            .starts_with("ru");
        let text = match build_forecast(sym, &cmc, &sqlite, price_for(sym)).await {
            Ok(f) => f.render_markdown(avg_entry, russian),
            Err(e) => {
                warn!("Forecast for {sym} failed: {e}");
                format!("⚠️ **{sym}**: нет данных для прогноза ({e})")
            }
        };
        let markup = serde_json::to_value(keyboards::forecast_actions(sym)).ok();
        // Rich messages render the GFM table as a real table; fall back to the
        // monospace HTML table if the API rejects the rich payload.
        if let Err(e) = crate::bot::rich_messages::send_rich_message_markdown_with_markup(
            &config,
            msg.chat.id.0,
            &text,
            markup.clone(),
        )
        .await
        {
            warn!("Rich forecast send failed for {sym} ({e}); falling back to HTML");
            let html = match build_forecast(sym, &cmc, &sqlite, price_for(sym)).await {
                Ok(f) => f.render_html_for(avg_entry, russian),
                Err(_) => text.clone(),
            };
            if let Err(e) = bot
                .send_message(ChatId(msg.chat.id.0), &html)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboards::forecast_actions(sym))
                .await
            {
                warn!("Forecast send failed for {sym}: {e}");
            }
        }
    }

    Ok(())
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
    // Rich messages (`sendRichMessage`) come back to teloxide 0.12 without a
    // parseable `message`, so requiring it made every button on a rich
    // forecast message a silent no-op. Fall back to the caller's private chat
    // and send a fresh message instead of editing in place.
    let chat_id = q
        .message
        .as_ref()
        .map(|m| m.chat.id)
        .unwrap_or(ChatId(q.from.id.0 as i64));
    let message_id = q.message.as_ref().map(|m| m.id);
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
                let russian = store_task
                    .read(user_id, |p| p.settings.preferred_language.clone())
                    .to_lowercase()
                    .starts_with("ru");
                let verdict = f
                    .action_lines(avg_entry, russian)
                    .into_iter()
                    .map(|l| format!("• {}", html_escape::encode_text(&l)))
                    .collect::<Vec<_>>()
                    .join("\n");
                let commentary = if ai.is_configured() {
                    match tokio::time::timeout(
                        Duration::from_secs(60),
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
                    }
                } else {
                    None
                };

                let text = match commentary {
                    Some(t) => format!(
                        "🧠 <b>{sym} — AI read</b>\n\n{t}\n\n🎯 <b>{}</b>\n{verdict}\n\
                         <i>Model output, not financial advice.</i>",
                        if russian { "Итог" } else { "Bottom line" }
                    ),
                    None => format!(
                        "{}\n\n🎯 <b>{}</b>\n{verdict}\n<i>AI commentary unavailable — deterministic summary only.</i>",
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
            let russian = store
                .read(user_id, |p| p.settings.preferred_language.clone())
                .to_lowercase()
                .starts_with("ru");
            let html = match build_forecast(&sym, &cmc, &sqlite, live).await {
                Ok(f) => f.render_html_for(avg_entry, russian),
                Err(e) => {
                    warn!("Forecast refresh for {sym} failed: {e}");
                    format!("⚠️ <b>{sym}</b>: нет данных для прогноза ({e})")
                }
            };
            let markdown = match build_forecast(&sym, &cmc, &sqlite, live).await {
                Ok(f) => f.render_markdown(avg_entry, russian),
                Err(_) => html.clone(),
            };
            let markup = serde_json::to_value(keyboards::forecast_actions(&sym)).ok();
            match message_id {
                Some(mid) => {
                    let edited = crate::bot::rich_messages::edit_rich_message_markdown(
                        &config,
                        chat_id.0,
                        mid.0,
                        &markdown,
                        markup.clone(),
                    )
                    .await;
                    if edited.is_err() {
                        let _ = bot
                            .edit_message_text(chat_id, mid, &html)
                            .parse_mode(ParseMode::Html)
                            .reply_markup(keyboards::forecast_actions(&sym))
                            .await;
                    }
                }
                // No message to edit (rich-message callback): send a fresh one.
                None => {
                    if crate::bot::rich_messages::send_rich_message_markdown_with_markup(
                        &config, chat_id.0, &markdown, markup,
                    )
                    .await
                    .is_err()
                    {
                        let _ = bot
                            .send_message(chat_id, &html)
                            .parse_mode(ParseMode::Html)
                            .reply_markup(keyboards::forecast_actions(&sym))
                            .await;
                    }
                }
            }
        }
        _ => {}
    }

    Ok(())
}
