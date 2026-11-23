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

        match build_dip_report_closes_forecast(sym, &cmc, &sqlite, None, avg).await {
            Ok((report, closes, forecast)) => {
                let policy = entry_policy(&closes);
                let mut d = Decision::from_report_with_forecast(&report, policy, &forecast);
                d.last_topup_days = last_topup_days;
                // Bottom read: reuse the exact `/bottom` engine so the board
                // never disagrees with it. The score is shown as a current
                // state gauge, not a price target.
                if let Ok(b) = build_bottom_signal(
                    sym, &cmc, &sqlite, None, avg, position, cash, protect,
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

    let _ = bot
        .send_message(msg.chat.id, text)
        .parse_mode(ParseMode::Html)
        .await;

    Ok(())
}
