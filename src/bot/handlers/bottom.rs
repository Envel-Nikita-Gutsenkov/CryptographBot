//! `/bottom [SYM ...]` — "is this the bottom, should I buy?" radar.
//!
//! Unlike `/dip`, this works for coins that are *not* in the portfolio: it
//! prints a concrete buy level, a 0..100 bottom score and a verdict, all derived
//! from the deterministic [`crate::trading::bottom`] reading of the sniper
//! report. The AI may only narrate it (`btm:ai:SYM`).

use std::sync::Arc;
use std::time::Duration;

use teloxide::{
    prelude::*,
    types::{CallbackQuery, ChatId, ParseMode},
};
use tracing::warn;

use crate::{
    bot::{handlers::forecast::callback_target, keyboards},
    config::Config,
    crypto::{ai::GeminiClient, CryptoClient},
    db::{SqliteStore, Store},
    trading::{bottom::bottom_signal, BottomSignal},
};

/// Build the bottom reading for `symbol` from the deterministic sniper report.
#[allow(clippy::too_many_arguments)]
pub async fn build_bottom_signal(
    symbol: &str,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    live_price: Option<f64>,
    avg_entry: Option<f64>,
    position_size: f64,
    cash: f64,
    protect_avg_entry: bool,
) -> anyhow::Result<BottomSignal> {
    let report = crate::bot::handlers::build_sniper_report(
        symbol,
        cmc,
        sqlite,
        live_price,
        avg_entry,
        position_size,
        cash,
        protect_avg_entry,
    )
    .await?;
    Ok(bottom_signal(&report))
}

fn is_russian(store: &Store, user_id: u64) -> bool {
    store
        .read(user_id, |p| p.settings.preferred_language.clone())
        .to_lowercase()
        .starts_with("ru")
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

pub async fn handle_bottom_cmd(
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
    let russian = is_russian(&store, user_id);

    let fallback: Vec<String> = store.read(user_id, |p| {
        let mut watch = p.settings.default_watchlist.clone();
        if watch.is_empty() {
            watch = p.holdings_by_symbol().into_keys().collect();
            watch.sort();
        }
        watch
    });
    let mut symbols = parse_symbols(&args, fallback);
    if symbols.is_empty() {
        symbols = vec!["BTC".into(), "ETH".into(), "SOL".into(), "LINK".into()];
    }

    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = cmc.get_quotes(&sym_refs).await.unwrap_or_default();
    let (cash, protect) = store.read(user_id, |p| {
        (
            p.settings.sniper_cash_usdt,
            p.settings.sniper_protect_avg_entry,
        )
    });

    for sym in &symbols {
        let (avg, position) = store.read(user_id, |p| {
            (
                p.avg_buy_price(sym),
                p.holdings_by_symbol().get(sym).copied().unwrap_or(0.0),
            )
        });
        let live = quotes
            .iter()
            .find(|q| q.symbol.eq_ignore_ascii_case(sym))
            .map(|q| q.price_usd);
        let text = match build_bottom_signal(sym, &cmc, &sqlite, live, avg, position, cash, protect)
            .await
        {
            Ok(s) => s.render_html(russian),
            Err(e) => {
                warn!("Bottom radar for {sym} failed: {e}");
                if russian {
                    format!("⚠️ <b>{sym}</b>: нет данных для Bottom Radar ({e})")
                } else {
                    format!("⚠️ <b>{sym}</b>: no data for Bottom Radar ({e})")
                }
            }
        };
        let sent = bot
            .send_message(ChatId(msg.chat.id.0), &text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboards::bottom_actions(sym))
            .await;
        if sent.is_err() {
            let _ = bot
                .send_message(ChatId(msg.chat.id.0), &text)
                .reply_markup(keyboards::bottom_actions(sym))
                .await;
        }
    }
    Ok(())
}

/// Callbacks: `btm:ai:SYM`, `btm:fx:SYM`.
pub async fn handle_bottom_callback(
    bot: Bot,
    q: CallbackQuery,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    sqlite: Arc<SqliteStore>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    let _ = bot.answer_callback_query(&q.id).await;
    let data = q.data.clone().unwrap_or_default();
    let (chat, message_id) = callback_target(&q);
    let user_id = q.from.id.0;
    if !config.is_admin(user_id) {
        bot.send_message(chat, "⛔ Unauthorized").await?;
        return Ok(());
    }
    let mut parts = data.splitn(3, ':');
    let _ = parts.next(); // "btm"
    let action = parts.next().unwrap_or_default();
    let sym = parts.next().unwrap_or_default().to_uppercase();
    if sym.is_empty() || !sym.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(());
    }
    let russian = is_russian(&store, user_id);
    let (avg, position, cash, protect) = store.read(user_id, |p| {
        (
            p.avg_buy_price(&sym),
            p.holdings_by_symbol().get(&sym).copied().unwrap_or(0.0),
            p.settings.sniper_cash_usdt,
            p.settings.sniper_protect_avg_entry,
        )
    });
    let live = cmc
        .get_quotes(&[sym.as_str()])
        .await
        .unwrap_or_default()
        .first()
        .map(|q| q.price_usd);

    let signal =
        match build_bottom_signal(&sym, &cmc, &sqlite, live, avg, position, cash, protect).await {
            Ok(s) => s,
            Err(e) => {
                warn!("Bottom radar for {sym} failed: {e}");
                let _ = bot
                    .send_message(
                        chat,
                        format!(
                            "⚠️ {sym}: {}",
                            if russian {
                                "нет данных"
                            } else {
                                "no data available"
                            }
                        ),
                    )
                    .await;
                return Ok(());
            }
        };

    match action {
        "ai" => {
            let loading = bot
                .send_message(chat, format!("🧠 Analysing <b>{sym}</b>…"))
                .parse_mode(ParseMode::Html)
                .await?;
            let bot_task = bot.clone();
            let sqlite_task = sqlite.clone();
            let config_task = config.clone();
            tokio::spawn(async move {
                let ai = GeminiClient::new(&config_task.gemini_api_key);
                let commentary = match tokio::time::timeout(
                    Duration::from_secs(50),
                    ai.bottom_commentary(&sym, &signal.plain(russian), sqlite_task),
                )
                .await
                {
                    Ok(Ok(t)) if !t.trim().is_empty() => Some(t),
                    Ok(Err(e)) => {
                        warn!("Bottom AI commentary failed for {sym}: {e}");
                        None
                    }
                    _ => None,
                };
                let _ = bot_task.delete_message(chat, loading.id).await;
                let text = match commentary {
                    Some(t) => format!(
                        "🧠 <b>{sym} — AI read</b>\n\n{t}\n\n{}",
                        signal.render_html(russian)
                    ),
                    None => signal.render_html(russian),
                };
                let sent = bot_task
                    .send_message(chat, &text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::bottom_actions(&sym))
                    .await;
                if sent.is_err() {
                    let _ = bot_task
                        .send_message(chat, &text)
                        .reply_markup(keyboards::bottom_actions(&sym))
                        .await;
                }
            });
        }
        "fx" => {
            let html = signal.render_html(russian);
            let edited = match message_id {
                Some(mid) => bot
                    .edit_message_text(chat, mid, &html)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::bottom_actions(&sym))
                    .await
                    .is_ok(),
                None => false,
            };
            if !edited {
                let _ = bot
                    .send_message(chat, &html)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::bottom_actions(&sym))
                    .await;
            }
        }
        _ => {}
    }
    Ok(())
}
