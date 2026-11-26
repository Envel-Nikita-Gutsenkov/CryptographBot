//! `/decide` — one compact, deterministic decision board across every coin in
//! the portfolio.
//!
//! Unlike `/analyze` (a full per-symbol wall), `/decide` answers a single
//! question for the whole book at once: *what do I actually do right now?* It
//! reuses the same [`build_dip_report`] engine that powers `/dip`, `/sniper` and
//! `/bottom`, so the verdicts and levels are identical — just ranked and
//! collapsed into one message with no prose.

use std::collections::BTreeSet;
use std::sync::Arc;

use teloxide::{prelude::*, types::ParseMode};

use crate::{
    bot::handlers::{build_bottom_signal, build_dip_report_closes_forecast},
    config::Config,
    crypto::CryptoClient,
    db::{SqliteStore, Store},
    trading::{
        decision::{entry_policy, rank, render_html, Decision},
    },
};

fn is_russian(store: &Store, user_id: u64) -> bool {
    store
        .read(user_id, |p| p.settings.preferred_language.clone())
        .to_lowercase()
        .starts_with("ru")
}

/// Symbols the user actually holds, sorted, with a sensible fallback when the
/// portfolio is empty.
fn target_symbols(store: &Store, user_id: u64) -> Vec<String> {
    let mut syms: BTreeSet<String> = store.read(user_id, |p| {
        p.holdings_by_symbol()
            .keys()
            .map(|s| s.to_uppercase())
            .collect()
    });
    if syms.is_empty() {
        let watch = store.read(user_id, |p| p.settings.default_watchlist.clone());
        for s in watch {
            syms.insert(s.to_uppercase());
        }
    }
    if syms.is_empty() {
        for s in ["BTC", "ETH", "SOL", "LINK"] {
            syms.insert(s.to_string());
        }
    }
    syms.into_iter().collect()
}

pub async fn handle_decide_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    sqlite: Arc<SqliteStore>,
    _config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    let user_id = msg.chat.id.0 as u64;

    let mut symbols: Vec<String> = args
        .split_whitespace()
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect();
    if symbols.is_empty() {
        symbols = target_symbols(&store, user_id);
    }

    let now = chrono::Utc::now();
    let mut decisions: Vec<Decision> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let (cash, protect) = store.read(user_id, |p| {
        (
            p.settings.sniper_cash_usdt,
            p.settings.sniper_protect_avg_entry,
        )
    });

    // Anchor every coin on the same live quote `/dip` and `/forecast` use, so
    // the board's price, ranges and levels can never disagree with them (it
    // used to fall back to the last cached close, up to 36h stale).
    let refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = cmc.get_quotes(&refs).await.unwrap_or_default();
    let price_for = |sym: &str| {
        quotes
            .iter()
            .find(|q| q.symbol.eq_ignore_ascii_case(sym))
            .map(|q| q.price_usd)
            .filter(|p| *p > 0.0)
    };

    for sym in &symbols {
        let avg = store.read(user_id, |p| p.avg_buy_price(sym));
        let position = store.read(user_id, |p| {
            *p.holdings_by_symbol()
                .get(&sym.to_uppercase())
                .unwrap_or(&0.0)
        });
        let last_topup_days = store.read(user_id, |p| {
            p.purchases
                .iter()
                .filter(|pu| pu.symbol.eq_ignore_ascii_case(sym))
                .map(|pu| (now - pu.date).num_days())
                .min()
        });

        match build_dip_report_closes_forecast(sym, &cmc, &sqlite, price_for(sym), avg).await {
            Ok((report, closes, forecast)) => {
                let policy = entry_policy(&closes);
                let mut d = Decision::from_report_with_forecast(&report, policy, &forecast);
                d.last_topup_days = last_topup_days;
                // Bottom read: reuse the exact `/bottom` engine so the board
                // never disagrees with it. The score is shown as a current
                // state gauge, not a price target.
                if let Ok(b) = build_bottom_signal(
                    sym, &cmc, &sqlite, price_for(sym), avg, position, cash, protect,
                )
                .await
                {
                    d.attach_bottom(b.bottom_score);
                }
                decisions.push(d);
            }
            Err(e) => skipped.push((sym.clone(), e.to_string())),
        }
    }

    rank(&mut decisions);

    let ru = is_russian(&store, user_id);
    let text = render_html(&decisions, &skipped, ru);

    // Decision journal: one row per coin with the exact market snapshot the
    // verdict was derived from, plus one row for the answer the user saw. This
    // is what lets us audit later "why did it say wait here?".
    let arg_symbols = symbols.join(",");
    for d in &decisions {
        let ctx = serde_json::json!({
            "price": d.price,
            "avg_entry": d.avg_entry,
            "rsi14": d.rsi14,
            "downtrend": d.downtrend,
            "trend_gated": d.trend_gated,
            "now_pct": d.now_pct,
            "last_topup_days": d.last_topup_days,
            "level": d.level.as_ref().map(|l| serde_json::json!({
                "price": l.price,
                "distance_pct": l.distance_pct,
                "touch_prob": l.touch_prob,
                "horizon_days": l.horizon_days,
                "label": l.label,
            })),
            "p_down": d.outlook.as_ref().map(|o| o.p_down),
            "bottom_score": d.bottom.as_ref().map(|b| b.score),
        })
        .to_string();
        let reason = d.level.as_ref().map(|l| l.label.clone()).unwrap_or_default();
        sqlite.record_journal(
            user_id,
            "decision",
            "/decide",
            &arg_symbols,
            &d.symbol,
            d.action.as_str(),
            None,
            &reason,
            &ctx,
            "",
        );
    }
    if !skipped.is_empty() {
        let reason = skipped
            .iter()
            .map(|(s, e)| format!("{s}: {e}"))
            .collect::<Vec<_>>()
            .join("; ");
        sqlite.record_journal(user_id, "decision", "/decide", &arg_symbols, "", "skip", None, &reason, "{}", "");
    }
    sqlite.record_journal(user_id, "answer", "/decide", &arg_symbols, "", "", None, "", "{}", &text);

    let _ = bot
        .send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .await;

    Ok(())
}
