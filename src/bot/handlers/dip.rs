//! `/dip [SYM ...]` — "is this the bottom, should I buy the dip?" report.
//!
//! Deterministic: the zone ladder and the touch probabilities come from
//! [`crate::trading::dipzones`], the average entry price from the portfolio.
//! The AI only narrates a report it cannot change (`dip:ai:SYM`).

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
    trading::{dip_report, DipReport},
};

/// Build the deterministic dip report for `symbol`, reusing the forecast
/// loader (CSV cache → SQLite intraday → provider top-up).
pub async fn build_dip_report(
    symbol: &str,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    live_price: Option<f64>,
    avg_entry: Option<f64>,
) -> anyhow::Result<DipReport> {
    let forecast = crate::bot::handlers::build_forecast(symbol, cmc, sqlite, live_price).await?;
    // The dip report needs the same daily series the forecast was built from;
    // load it once more (cheap: it is the same cached series).
    let sym = symbol.to_uppercase();
    let mut series = crate::trading::history_file::load_history(&sym).unwrap_or_default();
    if let Ok(rows) = sqlite.get_price_history(&sym, 220).await {
        if !rows.is_empty() {
            series = crate::trading::forecast::merge_daily_history(&series, &rows);
        }
    }
    if series.len() < 200 {
        if let Ok(h) = cmc.get_history(&sym, "365d").await {
            if !h.is_empty() {
                series = crate::trading::forecast::merge_daily_history(&series, &h);
            }
        }
    }
    Ok(dip_report(&forecast, &series, avg_entry))
}

fn parse_symbols(args: &str, fallback: Vec<String>) -> Vec<String> {
    let mut symbols: Vec<String> = args
        .split_whitespace()
        .filter(|w| w.chars().all(|c| c.is_ascii_alphanumeric()))
        .map(|w| w.to_uppercase())
        .collect();
    if symbols.is_empty() {
        symbols = fallback;
        symbols.truncate(4);
    }
    symbols
}

pub async fn handle_dip_cmd(
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

    let fallback: Vec<String> = store.read(user_id, |p| {
        let mut watch = p.settings.default_watchlist.clone();
        if watch.is_empty() {
            watch = p.holdings_by_symbol().into_keys().collect();
            watch.sort();
        }
        watch
    });
    let symbols = parse_symbols(&args, fallback);

    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = cmc.get_quotes(&sym_refs).await.unwrap_or_default();
    let price_for = |sym: &str| {
        quotes
            .iter()
            .find(|q| q.symbol.eq_ignore_ascii_case(sym))
            .map(|q| q.price_usd)
    };

    for sym in &symbols {
        let avg = store.read(user_id, |p| p.avg_buy_price(sym));
        let text = match build_dip_report(sym, &cmc, &sqlite, price_for(sym), avg).await {
            Ok(r) => r.render_html(),
            Err(e) => {
                warn!("Dip report for {sym} failed: {e}");
                format!("⚠️ <b>{sym}</b>: нет данных для анализа ({e})")
            }
        };
        let sent = bot
            .send_message(ChatId(msg.chat.id.0), &text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboards::dip_actions(sym))
            .await;
        if sent.is_err() {
            // Never let a stray HTML character swallow the report.
            let _ = bot
                .send_message(ChatId(msg.chat.id.0), &text)
                .reply_markup(keyboards::dip_actions(sym))
                .await;
        }
    }

    Ok(())
}

/// Callbacks attached to a dip message:
/// * `dip:ai:SYM` — AI narration of the deterministic report.
/// * `dip:fx:SYM` — rebuild the report from the freshest data, in place.
pub async fn handle_dip_callback(
    bot: Bot,
    q: CallbackQuery,
    store: Arc<Store>,
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
    let _ = parts.next(); // "dip"
    let action = parts.next().unwrap_or_default();
    let sym = parts.next().unwrap_or_default().to_uppercase();
    if sym.is_empty() || !sym.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(());
    }
    let avg = store.read(user_id, |p| p.avg_buy_price(&sym));

    match action {
        "ai" => {
            let loading = bot
                .send_message(chat_id, format!("🧠 Analysing <b>{sym}</b> dip zones…"))
                .parse_mode(ParseMode::Html)
                .await?;
            let loading_id = loading.id;
            let bot_task = bot.clone();
            tokio::spawn(async move {
                let quotes = cmc.get_quotes(&[sym.as_str()]).await.unwrap_or_default();
                let live = quotes.first().map(|q| q.price_usd);
                let report = build_dip_report(&sym, &cmc, &sqlite, live, avg).await;
                let _ = bot_task.delete_message(chat_id, loading_id).await;

                let r = match report {
                    Ok(r) => r,
                    Err(e) => {
                        warn!("Dip report for {sym} failed in AI callback: {e}");
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
                        ai.dip_commentary(&r.symbol, &r.plain(), sqlite.clone()),
                    )
                    .await
                    {
                        Ok(Ok(t)) if !t.trim().is_empty() => Some(t),
                        Ok(Ok(_)) => None,
                        Ok(Err(e)) => {
                            warn!("Dip AI commentary failed for {sym}: {e}");
                            None
                        }
                        Err(_) => {
                            warn!("Dip AI commentary timed out for {sym}");
                            None
                        }
                    }
                } else {
                    None
                };

                let text = match commentary {
                    Some(t) => format!(
                        "🧠 <b>{sym} — AI read</b>\n\n{t}\n\n\
                         <i>Model output, not financial advice. The probabilities above are \
                         deterministic.</i>"
                    ),
                    None => format!(
                        "{}\n\n<i>AI commentary unavailable — deterministic report only.</i>",
                        r.summary_html()
                    ),
                };

                let sent = bot_task
                    .send_message(chat_id, &text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::dip_actions(&sym))
                    .await;
                if sent.is_err() {
                    let _ = bot_task
                        .send_message(chat_id, &text)
                        .reply_markup(keyboards::dip_actions(&sym))
                        .await;
                }
            });
        }
        "fx" => {
            let quotes = cmc.get_quotes(&[sym.as_str()]).await.unwrap_or_default();
            let live = quotes.first().map(|q| q.price_usd);
            let text = match build_dip_report(&sym, &cmc, &sqlite, live, avg).await {
                Ok(r) => r.render_html(),
                Err(e) => {
                    warn!("Dip refresh for {sym} failed: {e}");
                    format!("⚠️ <b>{sym}</b>: нет данных ({e})")
                }
            };
            let edited = bot
                .edit_message_text(chat_id, msg.id, &text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboards::dip_actions(&sym))
                .await;
            if edited.is_err() {
                let _ = bot
                    .send_message(chat_id, &text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::dip_actions(&sym))
                    .await;
            }
        }
        _ => {}
    }

    Ok(())
}
