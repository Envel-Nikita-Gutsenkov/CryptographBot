//! `/sniper [SYM ...]` — "Hardcore Dip Sniper" accumulation report.
//!
//! Shows the drawdown Z-score, the three left-tail limit buckets, their touch
//! probabilities and the resulting average entry. The maths lives in
//! [`crate::trading::sniper`]; the AI can only narrate it (`snp:ai:SYM`).

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
    trading::{
        forecast::merge_daily_history,
        history_file,
        sniper::{aggregate_last_day, sniper_report, DipSniperReport},
    },
};

/// Build the deterministic sniper report for `symbol`.
pub async fn build_sniper_report(
    symbol: &str,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    live_price: Option<f64>,
    avg_entry: Option<f64>,
    position_size: f64,
    cash: f64,
    protect_avg_entry: bool,
) -> anyhow::Result<DipSniperReport> {
    let sym = symbol.to_uppercase();
    let forecast = crate::bot::handlers::build_forecast(&sym, cmc, sqlite, live_price).await?;

    // Same daily series the forecast used: CSV cache → SQLite → provider top-up.
    let mut series = history_file::load_history(&sym).unwrap_or_default();
    if let Ok(rows) = sqlite.get_price_history(&sym, 220).await {
        if !rows.is_empty() {
            series = merge_daily_history(&series, &rows);
        }
    }
    if series.len() < 200 {
        if let Ok(h) = cmc.get_history(&sym, "365d").await {
            if !h.is_empty() {
                series = merge_daily_history(&series, &h);
            }
        }
    }

    // Today's candle (open/high/low) from the intraday rows, when present.
    let candle = sqlite
        .get_ohlcv_history(&sym, 2)
        .await
        .ok()
        .and_then(|rows| aggregate_last_day(&rows));

    Ok(sniper_report(
        &forecast,
        &series,
        candle,
        None,
        avg_entry,
        position_size,
        cash,
        protect_avg_entry,
    ))
}

fn is_russian(store: &Store, user_id: u64) -> bool {
    store
        .read(user_id, |p| p.settings.preferred_language.clone())
        .to_lowercase()
        .starts_with("ru")
}

fn parse_amount(raw: &str) -> Option<f64> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    cleaned.parse::<f64>().ok().filter(|v| *v >= 0.0)
}

fn parse_on_off(raw: &str) -> Option<bool> {
    match raw.to_lowercase().as_str() {
        "on" | "1" | "yes" | "true" | "вкл" => Some(true),
        "off" | "0" | "no" | "false" | "выкл" => Some(false),
        _ => None,
    }
}

fn cash_line(store: &Store, user_id: u64, russian: bool) -> String {
    let (cash, protect, alerts) = store.read(user_id, |p| {
        (
            p.settings.sniper_cash_usdt,
            p.settings.sniper_protect_avg_entry,
            p.settings.sniper_alerts_enabled,
        )
    });
    if russian {
        format!(
            "🩸 <b>Dip Sniper</b>\n\nКэш: <b>${:.0}</b>\nЗащита средней: <b>{}</b>\nУведомления: <b>{}</b>\n\n\
             /sniper cash 1000 — задать кэш\n/sniper protect on|off — защита средней\n\
             /sniper alerts on|off — уведомления\n/sniper BTC — отчёт по монете",
            cash,
            if protect { "вкл" } else { "выкл" },
            if alerts { "вкл" } else { "выкл" }
        )
    } else {
        format!(
            "🩸 <b>Dip Sniper</b>\n\nCash: <b>${:.0}</b>\nAvg-entry guard: <b>{}</b>\nAlerts: <b>{}</b>\n\n\
             /sniper cash 1000 — set the cash pool\n/sniper protect on|off — avg-entry guard\n\
             /sniper alerts on|off — notifications\n/sniper BTC — report for one coin",
            cash,
            if protect { "on" } else { "off" },
            if alerts { "on" } else { "off" }
        )
    }
}

pub async fn handle_sniper_cmd(
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
    let chat = ChatId(msg.chat.id.0);
    let words: Vec<&str> = args.split_whitespace().collect();
    let first = words.first().map(|w| w.to_lowercase()).unwrap_or_default();

    // Settings sub-commands.
    match first.as_str() {
        "cash" => {
            match words.get(1).and_then(|w| parse_amount(w)) {
                Some(amount) => {
                    store.write(user_id, |p| p.settings.sniper_cash_usdt = amount);
                    let text = if russian {
                        format!("✅ Кэш для Dip Sniper: <b>${amount:.0}</b>")
                    } else {
                        format!("✅ Dip Sniper cash: <b>${amount:.0}</b>")
                    };
                    bot.send_message(chat, text)
                        .parse_mode(ParseMode::Html)
                        .await?;
                }
                None => {
                    bot.send_message(chat, cash_line(&store, user_id, russian))
                        .parse_mode(ParseMode::Html)
                        .await?;
                }
            }
            return Ok(());
        }
        "protect" => {
            if let Some(on) = words.get(1).and_then(|w| parse_on_off(w)) {
                store.write(user_id, |p| p.settings.sniper_protect_avg_entry = on);
                let text = if russian {
                    format!(
                        "✅ Защита средней: <b>{}</b>",
                        if on {
                            "включена"
                        } else {
                            "выключена"
                        }
                    )
                } else {
                    format!(
                        "✅ Avg-entry guard: <b>{}</b>",
                        if on { "on" } else { "off" }
                    )
                };
                bot.send_message(chat, text)
                    .parse_mode(ParseMode::Html)
                    .await?;
            } else {
                bot.send_message(chat, cash_line(&store, user_id, russian))
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
            return Ok(());
        }
        "alerts" => {
            if let Some(on) = words.get(1).and_then(|w| parse_on_off(w)) {
                store.write(user_id, |p| p.settings.sniper_alerts_enabled = on);
                let text = if russian {
                    format!(
                        "✅ Уведомления: <b>{}</b>",
                        if on {
                            "включены"
                        } else {
                            "выключены"
                        }
                    )
                } else {
                    format!("✅ Alerts: <b>{}</b>", if on { "on" } else { "off" })
                };
                bot.send_message(chat, text)
                    .parse_mode(ParseMode::Html)
                    .await?;
            } else {
                bot.send_message(chat, cash_line(&store, user_id, russian))
                    .parse_mode(ParseMode::Html)
                    .await?;
            }
            return Ok(());
        }
        "help" => {
            bot.send_message(chat, cash_line(&store, user_id, russian))
                .parse_mode(ParseMode::Html)
                .await?;
            return Ok(());
        }
        _ => {}
    }

    let mut symbols: Vec<String> = words
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
        let text = match build_sniper_report(sym, &cmc, &sqlite, live, avg, position, cash, protect)
            .await
        {
            Ok(r) => r.render_html(russian),
            Err(e) => {
                warn!("Sniper report for {sym} failed: {e}");
                if russian {
                    format!("⚠️ <b>{sym}</b>: нет данных для Sniper ({e})")
                } else {
                    format!("⚠️ <b>{sym}</b>: no data for Sniper ({e})")
                }
            }
        };
        let sent = bot
            .send_message(chat, &text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboards::sniper_actions(sym))
            .await;
        if sent.is_err() {
            // A stray entity must not swallow the report.
            let _ = bot
                .send_message(chat, &text)
                .reply_markup(keyboards::sniper_actions(sym))
                .await;
        }
    }
    Ok(())
}

/// Callbacks: `snp:cmd:SYM`, `snp:ai:SYM`, `snp:fx:SYM`.
pub async fn handle_sniper_callback(
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
    let _ = parts.next(); // "snp"
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

    let report =
        match build_sniper_report(&sym, &cmc, &sqlite, live, avg, position, cash, protect).await {
            Ok(r) => r,
            Err(e) => {
                warn!("Sniper report for {sym} failed: {e}");
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
                let commentary = if ai.is_configured() {
                    match tokio::time::timeout(
                        Duration::from_secs(60),
                        ai.forecast_commentary(&sym, &report.plain(russian), sqlite_task),
                    )
                    .await
                    {
                        Ok(Ok(t)) if !t.trim().is_empty() => Some(t),
                        Ok(Err(e)) => {
                            warn!("Sniper AI commentary failed for {sym}: {e}");
                            None
                        }
                        _ => None,
                    }
                } else {
                    None
                };
                let _ = bot_task.delete_message(chat, loading.id).await;
                let verdict = html_escape::encode_text(&report.verdict(russian)).to_string();
                let table = html_escape::encode_text(&report.table_lines().join("\n")).to_string();
                let text = match commentary {
                    Some(t) => format!(
                        "🧠 <b>{sym} — AI read</b>\n\n{t}\n\n🎯 <b>{}</b>\n{}\n\n<pre>{table}</pre>\n\
                         <i>{}</i>",
                        if russian { "Итог" } else { "Bottom line" },
                        verdict,
                        html_escape::encode_text(if russian {
                            "Модель, не финансовый совет."
                        } else {
                            "Model output, not financial advice."
                        })
                    ),
                    None => format!(
                        "{}\n\n🎯 <b>{}</b>\n{}",
                        report.render_html(russian),
                        if russian { "Итог" } else { "Bottom line" },
                        verdict
                    ),
                };
                let sent = bot_task
                    .send_message(chat, &text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::sniper_actions(&sym))
                    .await;
                if sent.is_err() {
                    let _ = bot_task
                        .send_message(chat, &text)
                        .reply_markup(keyboards::sniper_actions(&sym))
                        .await;
                }
            });
        }
        "fx" | "cmd" => {
            let html = report.render_html(russian);
            let edited = match message_id {
                Some(mid) => bot
                    .edit_message_text(chat, mid, &html)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::sniper_actions(&sym))
                    .await
                    .is_ok(),
                None => false,
            };
            if !edited {
                let _ = bot
                    .send_message(chat, &html)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::sniper_actions(&sym))
                    .await;
            }
        }
        _ => {}
    }
    Ok(())
}
