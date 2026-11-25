//! `/analyze [SYM ...]` — the merged, one-screen plan.
//!
//! `/forecast`, `/dip`, `/sniper` and `/bottom` used to print four overlapping
//! walls of indicators and sometimes disagreed in the same minute. `/analyze`
//! collapses them into one structured answer per symbol:
//!
//! 1. spot vs the user's average entry price,
//! 2. realistic top-up levels (real dips with a >=10% chance at the longest
//!    horizon — the same filter the dip ladder uses),
//! 3. a concrete verdict (`Buy now / Scale in / Wait / Hold`) plus the
//!    deterministic `Action:` line,
//! 4. the 0..100 bottom score as supporting context.
//!
//! It is deterministic: the numbers come from [`build_dip_report`] and
//! [`build_bottom_signal`]. The AI can only narrate the finished screen
//! (`ana:ai:SYM`); it cannot change a single number.

use std::sync::Arc;
use std::time::Duration;

use teloxide::{
    prelude::*,
    types::{CallbackQuery, ChatId, ParseMode},
};
use tracing::warn;

use crate::{
    bot::{
        handlers::{build_bottom_signal, build_dip_report},
        keyboards,
    },
    config::Config,
    crypto::{ai::GeminiClient, CryptoClient},
    db::{SqliteStore, Store},
    trading::{BottomSignal, DipReport, DipZone},
};

// ── Formatting ──────────────────────────────────────────────────────────────

fn decimals(price: f64) -> usize {
    crate::format::price_decimals(price)
}

fn money(price: f64) -> String {
    format!("${:.*}", decimals(price), price)
}

fn pct(p: f64) -> String {
    format!("{:.0}%", (p * 100.0).clamp(0.0, 100.0))
}

/// Pick the nearest few real dips: ascending order means the nearest zone is the
/// last element, so reverse for an actionable "closest first" list.
fn nearest_levels(report: &DipReport, limit: usize) -> Vec<&DipZone> {
    let mut zones: Vec<&DipZone> = report.display_zones();
    zones.sort_by(|a, b| b.price.total_cmp(&a.price));
    zones.truncate(limit);
    zones
}

fn levels_table(report: &DipReport, russian: bool) -> String {
    let zones = nearest_levels(report, 3);
    let longest = report.horizons.last().copied().unwrap_or(30);
    let dist_h = if russian { "ДИСТ" } else { "DIST" };
    // Size the LEVEL column to the widest cell: a sub-cent token renders a long
    // `$0.000002558`, which used to spill into the DIST column.
    let level_w = zones
        .iter()
        .map(|z| money(z.price).chars().count())
        .max()
        .unwrap_or(0)
        .max("LEVEL".len());
    let mut lines = vec![format!(
        "{:<level_w$} {:>7}{:>7}",
        "LEVEL",
        dist_h,
        format!("P({longest}d)")
    )];
    if zones.is_empty() {
        lines.push(if russian {
            "(цены на поддержке — реальных дипов ниже нет)".to_string()
        } else {
            "(price is on support — no real dip below)".to_string()
        });
    } else {
        for z in zones {
            lines.push(format!(
                "{:<level_w$} {:>7}{:>7}",
                money(z.price),
                crate::format::fmt_pct_signed(z.distance_pct, 1),
                pct(z.prob_at(longest).unwrap_or(0.0)),
            ));
        }
    }
    lines.join("\n")
}

/// Deterministic one-screen plan for a symbol. Pure: takes already-built
/// reports, so it can be unit-tested without network calls.
pub fn render_analyze(report: &DipReport, bottom: Option<&BottomSignal>, russian: bool) -> String {
    let esc = html_escape::encode_text;
    let avg = report.avg_entry.filter(|a| a.is_finite() && *a > 0.0);
    let avg_line = match avg {
        Some(a) => format!(
            "{} <code>{}</code> · {} <b>{}</b>",
            if russian {
                "средняя"
            } else {
                "avg entry"
            },
            money(a),
            if russian { "спот" } else { "spot" },
            crate::format::fmt_pct_signed((report.price / a - 1.0) * 100.0, 1),
        ),
        None => {
            if russian {
                "средняя неизвестна — <code>/add</code> для входа от средней".to_string()
            } else {
                "average entry unknown — <code>/add</code> to enable entry-relative advice"
                    .to_string()
            }
        }
    };
    let rsi = report
        .rsi14
        .map(|r| format!("{r:.0}"))
        .unwrap_or_else(|| "n/a".into());
    let sma = report.sma200.map(money).unwrap_or_else(|| "n/a".into());

    let mut out = format!(
        "🔎 <b>{}</b> · <code>{}</code>\n{}\nvol <code>{:.2}%/day</code> · RSI <code>{}</code> · SMA200 <code>{}</code>\n",
        esc(&report.symbol),
        money(report.price),
        avg_line,
        report.vol_daily_pct,
        rsi,
        sma,
    );

    out.push_str(&format!(
        "\n<b>{}</b>\n<pre>{}</pre>\n",
        if russian {
            "Уровни докупки (реальные дипы)"
        } else {
            "Top-up levels (real dips)"
        },
        esc(&levels_table(report, russian)),
    ));

    out.push_str(&format!(
        "\n{} <b>{}</b>\n{}\n",
        report.verdict.icon(),
        report.verdict.label(),
        esc(&report.reason),
    ));
    out.push_str(&format!("\n<b>{}</b>\n", esc(&report.action_line())));

    if let Some(b) = bottom {
        out.push_str(&format!(
            "\n🧭 {} <b>{}/100</b> · {} · {} <code>{}</code>\n",
            if russian {
                "Оценка дна"
            } else {
                "Bottom score"
            },
            b.bottom_score,
            b.score_band(russian),
            if russian { "бид" } else { "bid" },
            money(b.buy_price),
        ));
    }

    out.push_str(
        "<i>Deterministic: /analyze merges forecast, dip, sniper and bottom. \
         Touch probabilities are model-implied chances of each level being reached.</i>",
    );
    crate::format::guard_slash_units(&out)
}

fn plain_analyze(report: &DipReport, bottom: Option<&BottomSignal>, russian: bool) -> String {
    let mut s = report.plain();
    if let Some(b) = bottom {
        s.push('\n');
        s.push_str(&b.plain(russian));
    }
    s
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

fn fallback_symbols(store: &Store, user_id: u64) -> Vec<String> {
    store.read(user_id, |p| {
        let mut watch = p.settings.default_watchlist.clone();
        if watch.is_empty() {
            watch = p.holdings_by_symbol().into_keys().collect();
            watch.sort();
        }
        watch
    })
}

/// Build the merged screen for one symbol from the freshest data.
async fn build_screen(
    sym: &str,
    store: &Store,
    user_id: u64,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    live_price: Option<f64>,
) -> anyhow::Result<(DipReport, Option<BottomSignal>, bool)> {
    let russian = is_russian(store, user_id);
    let (avg, position, cash, protect) = store.read(user_id, |p| {
        (
            p.avg_buy_price(sym),
            p.holdings_by_symbol().get(sym).copied().unwrap_or(0.0),
            p.settings.sniper_cash_usdt,
            p.settings.sniper_protect_avg_entry,
        )
    });
    let report = build_dip_report(sym, cmc, sqlite, live_price, avg).await?;
    // The bottom score is supporting context; a failure there must not hide the
    // dip plan, so degrade to `None` instead of returning an error.
    let bottom = build_bottom_signal(sym, cmc, sqlite, live_price, avg, position, cash, protect)
        .await
        .ok();
    Ok((report, bottom, russian))
}

pub async fn handle_analyze_cmd(
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

    let fallback = fallback_symbols(&store, user_id);
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
        let text = match build_screen(sym, &store, user_id, &cmc, &sqlite, price_for(sym)).await {
            Ok((report, bottom, russian)) => render_analyze(&report, bottom.as_ref(), russian),
            Err(e) => {
                warn!("Analyze for {sym} failed: {e}");
                format!("⚠️ <b>{sym}</b>: нет данных для анализа ({e})")
            }
        };
        let sent = bot
            .send_message(ChatId(msg.chat.id.0), &text)
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboards::analyze_actions(sym))
            .await;
        if sent.is_err() {
            let _ = bot
                .send_message(ChatId(msg.chat.id.0), &text)
                .reply_markup(keyboards::analyze_actions(sym))
                .await;
        }
    }
    Ok(())
}

/// Callbacks: `ana:ai:SYM` (AI narration) and `ana:fx:SYM` (rebuild in place).
pub async fn handle_analyze_callback(
    bot: Bot,
    q: CallbackQuery,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    sqlite: Arc<SqliteStore>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    let _ = bot.answer_callback_query(&q.id).await;
    let data = q.data.clone().unwrap_or_default();
    let (chat_id, message_id) = super::forecast::callback_target(&q);
    let user_id = q.from.id.0;
    if !config.is_admin(user_id) {
        bot.send_message(chat_id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let mut parts = data.splitn(3, ':');
    let _ = parts.next(); // "ana"
    let action = parts.next().unwrap_or_default();
    let sym = parts.next().unwrap_or_default().to_uppercase();
    if sym.is_empty() || !sym.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Ok(());
    }
    let live = cmc
        .get_quotes(&[sym.as_str()])
        .await
        .unwrap_or_default()
        .first()
        .map(|q| q.price_usd);

    match action {
        "ai" => {
            let loading = bot
                .send_message(chat_id, format!("🧠 Analysing <b>{sym}</b>…"))
                .parse_mode(ParseMode::Html)
                .await?;
            let loading_id = loading.id;
            let bot_task = bot.clone();
            let sqlite_task = sqlite.clone();
            let cmc_task = cmc.clone();
            let store_task = store.clone();
            let config_task = config.clone();
            tokio::spawn(async move {
                let (report, bottom, russian) =
                    match build_screen(&sym, &store_task, user_id, &cmc_task, &sqlite_task, live)
                        .await
                    {
                        Ok(v) => v,
                        Err(e) => {
                            warn!("Analyze AI for {sym} failed: {e}");
                            let _ = bot_task.delete_message(chat_id, loading_id).await;
                            let _ = bot_task
                                .send_message(chat_id, format!("⚠️ {sym}: нет данных ({e})"))
                                .await;
                            return;
                        }
                    };
                let plain = plain_analyze(&report, bottom.as_ref(), russian);
                let ai = GeminiClient::new(&config_task.gemini_api_key);
                let commentary = match tokio::time::timeout(
                    Duration::from_secs(50),
                    ai.dip_commentary(&sym, &plain, sqlite_task),
                )
                .await
                {
                    Ok(Ok(t)) if !t.trim().is_empty() => Some(t),
                    Ok(Ok(_)) => None,
                    Ok(Err(e)) => {
                        warn!("Analyze AI commentary failed for {sym}: {e}");
                        None
                    }
                    Err(_) => {
                        warn!("Analyze AI commentary timed out for {sym}");
                        None
                    }
                };
                let _ = bot_task.delete_message(chat_id, loading_id).await;

                let text = match commentary {
                    Some(t) => format!(
                        "🧠 <b>{sym} — AI read</b>\n\n{}\n\n{}\n\n\
                         <i>The numbers above are deterministic; the AI only narrates them.</i>",
                        crate::format::model_output_to_html(&t),
                        render_analyze(&report, bottom.as_ref(), russian),
                    ),
                    None => render_analyze(&report, bottom.as_ref(), russian),
                };
                let sent = bot_task
                    .send_message(chat_id, &text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::analyze_actions(&sym))
                    .await;
                if sent.is_err() {
                    let _ = bot_task
                        .send_message(chat_id, &text)
                        .reply_markup(keyboards::analyze_actions(&sym))
                        .await;
                }
            });
        }
        "fx" => {
            let text = match build_screen(&sym, &store, user_id, &cmc, &sqlite, live).await {
                Ok((report, bottom, russian)) => render_analyze(&report, bottom.as_ref(), russian),
                Err(e) => {
                    warn!("Analyze refresh for {sym} failed: {e}");
                    format!("⚠️ <b>{sym}</b>: нет данных ({e})")
                }
            };
            let edited = match message_id {
                Some(mid) => bot
                    .edit_message_text(chat_id, mid, &text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::analyze_actions(&sym))
                    .await
                    .is_ok(),
                None => false,
            };
            if !edited {
                let _ = bot
                    .send_message(chat_id, &text)
                    .parse_mode(ParseMode::Html)
                    .reply_markup(keyboards::analyze_actions(&sym))
                    .await;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::{dip_report, forecast, DipVerdict};

    fn line_series(prices: &[f64]) -> Vec<(i64, f64)> {
        let start = 1_700_000_000_000i64;
        let day = 86_400_000i64;
        prices
            .iter()
            .enumerate()
            .map(|(i, &p)| (start + i as i64 * day, p))
            .collect()
    }

    fn report_for(prices: &[f64], avg: Option<f64>) -> DipReport {
        let series = line_series(prices);
        let price = series.last().unwrap().1;
        let fc = forecast::forecast("BTC", price, &series);
        dip_report(&fc, &series, avg)
    }

    #[test]
    fn analyze_screen_has_price_vs_avg_level_and_action() {
        // A long rise, then a partial pullback that stops well above the
        // historical base — so there are real dip levels *below* spot.
        let mut prices: Vec<f64> = (0..200)
            .map(|i| 80.0 + i as f64 * 0.2 + (i as f64 * 0.5).sin() * 6.0)
            .collect();
        let peak = prices[199];
        prices.extend((0..40).map(|i| peak - i as f64 * 0.5));
        let report = report_for(&prices, Some(peak * 1.05));

        assert!(
            !matches!(report.verdict, DipVerdict::NoData),
            "need a usable verdict"
        );
        assert!(!report.display_zones().is_empty(), "need a real dip level");

        let html = render_analyze(&report, None, false);
        // (1) spot vs average entry
        assert!(html.contains("avg entry"), "must state avg entry: {html}");
        assert!(html.contains("vs average") || html.contains('%'));
        // (2) a filtered top-up level renders in the <pre> table
        assert!(html.contains("<pre>"));
        let zone = *report.display_zones().last().unwrap();
        let zone_price = zone.price;
        assert!(
            html.contains(&money(zone_price)),
            "filtered level {} must appear: {html}",
            money(zone_price)
        );
        // (3) a concrete verdict with an imperative Action line
        assert!(html.contains("Action:"), "must end with an Action line");
    }

    #[test]
    fn analyze_without_avg_is_honest_and_still_renders() {
        // A flat-ish series with no usable dip should still render, and must not
        // fabricate an average entry.
        let prices: Vec<f64> = (0..120).map(|i| 100.0 + (i as f64 * 0.1)).collect();
        let report = report_for(&prices, None);
        let html = render_analyze(&report, None, false);
        assert!(html.contains("unknown"));
        assert!(html.contains("Action:"));
    }
}
