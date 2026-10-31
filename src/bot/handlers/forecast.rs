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

/// Assemble the daily series for `symbol`:
/// 1. the on-disk `data/history/{SYM}.csv` cache (years of daily closes),
/// 2. intraday rows from SQLite resampled to one close per day (last wins),
/// 3. provider history as a last resort when the cache is empty.
pub async fn build_forecast(
    symbol: &str,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    live_price: Option<f64>,
) -> anyhow::Result<Forecast> {
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
    let mut symbols: Vec<String> = args
        .split_whitespace()
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

    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = cmc.get_quotes(&sym_refs).await.unwrap_or_default();
    let price_for = |sym: &str| {
        quotes
            .iter()
            .find(|q| q.symbol.eq_ignore_ascii_case(sym))
            .map(|q| q.price_usd)
    };

    for sym in &symbols {
        let text = match build_forecast(sym, &cmc, &sqlite, price_for(sym)).await {
            Ok(f) => f.render_html(),
            Err(e) => {
                warn!("Forecast for {sym} failed: {e}");
                format!("⚠️ <b>{sym}</b>: нет данных для прогноза ({e})")
            }
        };
        if let Err(e) = bot
            .send_message(ChatId(msg.chat.id.0), &text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboards::forecast_actions(sym))
            .await
        {
            warn!("Forecast send failed for {sym}: {e}");
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
) -> anyhow::Result<()> {
    let _ = bot.answer_callback_query(&q.id).await;

    let data = q.data.clone().unwrap_or_default();
    let msg = match q.message {
        Some(m) => m,
        None => return Ok(()),
    };
    let chat_id = msg.chat.id;
    let user_id = q.from.id.0;
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
                        "🧠 <b>{sym} — AI read</b>\n\n{t}\n\n\
                         <i>Model output, not financial advice.</i>"
                    ),
                    None => format!(
                        "{}\n\n<i>AI commentary unavailable — deterministic summary only.</i>",
                        f.summary_html()
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
            let text = match build_forecast(&sym, &cmc, &sqlite, live).await {
                Ok(f) => f.render_html(),
                Err(e) => {
                    warn!("Forecast refresh for {sym} failed: {e}");
                    format!("⚠️ <b>{sym}</b>: нет данных для прогноза ({e})")
                }
            };
            let edited = bot
                .edit_message_text(chat_id, msg.id, &text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboards::forecast_actions(&sym))
                .await;
            if edited.is_err() {
                let _ = bot
                    .send_message(chat_id, &text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::forecast_actions(&sym))
                    .await;
            }
        }
        _ => {}
    }

    Ok(())
}
