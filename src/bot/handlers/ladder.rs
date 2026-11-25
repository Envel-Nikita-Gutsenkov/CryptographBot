//! `/ladder [SYMS] [BUDGET]` — interactive multi-coin limit-order ("traps")
//! planner.
//!
//! The user picks a budget and a set of coins; the bot turns that into a table
//! of concrete limit buys *below* spot, each with its touch probability, the
//! spend, the quantity and the resulting blended average entry — the same
//! question the user used to ask an external model, answered from the local
//! math.
//!
//! Everything numeric comes from [`build_sniper_report`] (cone / first-passage
//! probabilities / average-entry maths). This module only allocates the budget,
//! orders the tranches and renders the table; it never invents a level.
//!
//! Interactive state is carried entirely in the callback payload
//! (`lad:<op>:<budget>:<csv>:<extra>`), so no dialogue FSM is needed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use teloxide::{
    prelude::*,
    types::{CallbackQuery, ChatId, InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode},
};
use tracing::warn;

use crate::{
    bot::handlers::{build_sniper_report, forecast::callback_target},
    config::Config,
    crypto::CryptoClient,
    db::{SqliteStore, Store},
    format::{fmt_price, format_thousands},
    trading::sniper::DipSniperReport,
};

/// Hard cap on how many coins one ladder spans (keeps the callback payload under
/// Telegram's 64-byte limit and the table readable).
const MAX_SYMBOLS: usize = 6;
/// Fallback budget when the user did not pass one and no goal suggests a value.
const DEFAULT_BUDGET: f64 = 500.0;
/// Budget quick-pick buttons.
const BUDGET_PRESETS: [f64; 4] = [100.0, 250.0, 500.0, 1000.0];
/// Per-symbol fetch ceiling so one slow provider cannot stall the whole ladder.
const PER_SYMBOL_TIMEOUT: Duration = Duration::from_secs(25);

// ── callback payload ────────────────────────────────────────────────────────

/// `lad:<op>:<budget>:<csv>:<extra>`
///
/// `csv` is the symbol list; empty means "default to holdings" and `-` means
/// "explicitly none". `extra` carries the symbol to toggle for `op=sel`.
struct LadCb {
    op: String,
    budget: f64,
    csv: String,
    extra: String,
}

fn parse_cb(data: &str) -> Option<LadCb> {
    let parts: Vec<&str> = data.splitn(5, ':').collect();
    if parts.len() < 5 || parts[0] != "lad" {
        return None;
    }
    Some(LadCb {
        op: parts[1].to_string(),
        budget: parts[2].parse::<f64>().unwrap_or(0.0),
        csv: parts[3].to_string(),
        extra: parts[4].to_string(),
    })
}

fn cb_key(op: &str, budget: f64, symbols: &[String], extra: &str) -> String {
    format!(
        "lad:{}:{}:{}:{}",
        op,
        budget.round().max(0.0) as i64,
        encode_selection(symbols),
        extra
    )
}

/// Encode a selection for the callback payload. Empty → `-` (explicit none),
/// capped to [`MAX_SYMBOLS`] to stay under the 64-byte limit.
fn encode_selection(symbols: &[String]) -> String {
    if symbols.is_empty() {
        return "-".to_string();
    }
    symbols
        .iter()
        .take(MAX_SYMBOLS)
        .map(|s| s.to_uppercase())
        .collect::<Vec<_>>()
        .join(",")
}

/// Resolve the `csv` field into a concrete selection. `""` → default holdings,
/// `"-"` → empty, otherwise the listed symbols.
fn resolve_selection(csv: &str, holdings: &[String]) -> Vec<String> {
    if csv.is_empty() {
        return holdings.iter().take(MAX_SYMBOLS).cloned().collect();
    }
    if csv == "-" {
        return Vec::new();
    }
    csv.split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_uppercase())
        .take(MAX_SYMBOLS)
        .collect()
}

// ── small helpers ───────────────────────────────────────────────────────────

fn is_russian(store: &Store, user_id: u64) -> bool {
    store
        .read(user_id, |p| p.settings.preferred_language.clone())
        .to_lowercase()
        .starts_with("ru")
}

/// Holdings (sorted by value desc) plus the watchlist, de-duplicated, capped at
/// [`MAX_SYMBOLS`]. Non-held watchlist coins are appended after holdings.
fn candidates(store: &Store, user_id: u64) -> (Vec<String>, Vec<String>) {
    store.read(user_id, |p| {
        let by_sym = p.holdings_by_symbol();
        let mut held: Vec<(String, f64)> = by_sym.into_iter().collect();
        held.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let mut held_syms: Vec<String> = held.into_iter().map(|(s, _)| s).collect();
        held_syms.truncate(MAX_SYMBOLS);

        let watchlist: Vec<String> = p
            .settings
            .default_watchlist
            .iter()
            .map(|s| s.to_uppercase())
            .collect();
        (held_syms, watchlist)
    })
}

/// Look through the user's active goals for a spend budget and a note.
/// Returns `(budget, goal_id, goal_text)` for the first goal that names a
/// plausible budget ($50+) alongside a buying keyword.
fn goal_budget_hint(store: &Store, user_id: u64) -> Option<(f64, u64, String)> {
    let goals = store.read(user_id, |p| {
        p.goals
            .iter()
            .filter(|g| g.active)
            .map(|g| (g.id, g.text.clone()))
            .collect::<Vec<_>>()
    });
    const KEYS: [&str; 8] = [
        "докуп", "потрат", "купить", "бюджет", "buy", "spend", "budget", "invest",
    ];
    for (id, text) in goals {
        let lower = text.to_lowercase();
        if !KEYS.iter().any(|k| lower.contains(k)) {
            continue;
        }
        // Extract the first integer >= 50 as a candidate budget.
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_digit() {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b',' || bytes[i] == b' ') {
                    i += 1;
                }
                let raw: String = text[start..i].chars().filter(|c| c.is_ascii_digit()).collect();
                if let Ok(v) = raw.parse::<f64>() {
                    if (50.0..=100_000.0).contains(&v) {
                        return Some((v, id, text));
                    }
                }
            } else {
                i += 1;
            }
        }
    }
    None
}

/// Right-pad / left-pad helpers for the monospace table.
fn right(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w {
        s.to_string()
    } else {
        format!("{}{}", " ".repeat(w - n), s)
    }
}

/// Quote decimals that keep small-cap prices readable.
fn price_str(p: f64) -> String {
    if p >= 1000.0 {
        format!("{:.1}", p)
    } else if p >= 1.0 {
        format!("{:.3}", p)
    } else {
        format!("{:.4}", p)
    }
}

fn qty_str(q: f64) -> String {
    if q >= 1000.0 {
        format!("{:.2}", q)
    } else if q >= 1.0 {
        format!("{:.4}", q)
    } else {
        format!("{:.6}", q)
    }
}

// ── command entry point ─────────────────────────────────────────────────────

pub async fn handle_ladder_cmd(
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
    let chat = ChatId(msg.chat.id.0);
    let russian = is_russian(&store, user_id);

    // Parse args: numeric token → budget, alphabetic token → symbol.
    let mut symbols: Vec<String> = Vec::new();
    let mut budget = 0.0_f64;
    for word in args.split_whitespace() {
        let cleaned: String = word
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',')
            .collect();
        if !cleaned.is_empty() {
            if let Ok(v) = cleaned.replace(',', "").parse::<f64>() {
                budget = v;
                continue;
            }
        }
        if word.chars().all(|c| c.is_alphabetic()) {
            let s = word.to_uppercase();
            if !symbols.contains(&s) {
                symbols.push(s);
            }
        }
    }
    symbols.truncate(MAX_SYMBOLS);

    // Budget explicitly given → build straight away (default coins = holdings).
    if budget > 0.0 {
        let (holdings, _) = candidates(&store, user_id);
        if symbols.is_empty() {
            symbols = holdings;
        }
        if symbols.is_empty() {
            bot.send_message(
                chat,
                if russian {
                    "У тебя нет монет в портфеле. Укажи монеты: <code>/ladder BTC ETH 500</code>"
                } else {
                    "You hold no coins. Name some: <code>/ladder BTC ETH 500</code>"
                },
            )
            .parse_mode(ParseMode::Html)
            .await?;
            return Ok(());
        }
        bot.send_chat_action(chat, teloxide::types::ChatAction::Typing)
            .await?;
        let text =
            build_ladder_text(&store, &cmc, &sqlite, user_id, &symbols, budget, russian).await;
        let kb = result_kb(budget, &symbols);
        bot.send_message(chat, text)
            .parse_mode(ParseMode::Html)
            .reply_markup(kb)
            .await?;
        return Ok(());
    }

    // Otherwise show the interactive menu (prefilled with any named symbols).
    let (text, kb) = render_menu(&store, user_id, budget, &symbols, russian);
    bot.send_message(chat, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    Ok(())
}

// ── menu ────────────────────────────────────────────────────────────────────

fn render_menu(
    store: &Store,
    user_id: u64,
    budget: f64,
    selection: &[String],
    russian: bool,
) -> (String, InlineKeyboardMarkup) {
    let (holdings, watchlist) = candidates(store, user_id);
    // Default selection is the holdings; explicit list stays as given.
    let sel: Vec<String> = if selection.is_empty() {
        holdings.clone()
    } else {
        selection.to_vec()
    };

    // Budget: explicit → keep; otherwise a goal hint; otherwise the default.
    let (eff_budget, goal_line) = if budget > 0.0 {
        (budget, None)
    } else if let Some((b, id, text)) = goal_budget_hint(store, user_id) {
        let snippet: String = text.chars().take(48).collect();
        (b, Some((id, snippet)))
    } else {
        (DEFAULT_BUDGET, None)
    };

    // Candidate coins: holdings then any watchlist coin not already held.
    let mut coins: Vec<String> = holdings.clone();
    for w in &watchlist {
        if !coins.contains(w) && coins.len() < MAX_SYMBOLS {
            coins.push(w.clone());
        }
    }

    let title = if russian {
        "🎯 <b>Buy Ladder</b> — ловушки на покупку ниже рынка"
    } else {
        "🎯 <b>Buy Ladder</b> — limit traps below spot"
    };
    let mut text = format!("{title}\n\n");
    text.push_str(&format!(
        "{} <b>${}</b>  ·  {} <b>{}</b>\n",
        if russian { "Бюджет:" } else { "Budget:" },
        format_thousands(eff_budget, 0),
        if russian { "Монет:" } else { "Coins:" },
        sel.len()
    ));
    if let Some((id, snippet)) = goal_line {
        text.push_str(&format!(
            "💡 {} #{}: <i>{}</i>\n",
            if russian { "из цели" } else { "from goal" },
            id,
            html_escape::encode_text(&snippet)
        ));
    }
    text.push_str(&format!(
        "\n{}\n",
        if russian {
            "Выбери монеты (✓ = да), затем построй."
        } else {
            "Pick coins (✓ = on), then build."
        }
    ));
    if coins.is_empty() {
        text.push_str(&format!(
            "\n{}\n",
            if russian {
                "Нет монет в портфеле — просто укажи их командой: /ladder SOL 500"
            } else {
                "No coins in the portfolio — name them instead: /ladder SOL 500"
            }
        ));
    }

    // Coin toggles, two per row.
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut row: Vec<InlineKeyboardButton> = Vec::new();
    for c in &coins {
        let on = sel.contains(c);
        let label = if on {
            format!("✅ {c}")
        } else {
            format!("▫️ {c}")
        };
        row.push(InlineKeyboardButton::callback(
            label,
            cb_key("sel", eff_budget, &sel, c),
        ));
        if row.len() == 2 {
            rows.push(std::mem::take(&mut row));
        }
    }
    if !row.is_empty() {
        rows.push(row);
    }

    // Quick selection rows.
    rows.push(vec![
        InlineKeyboardButton::callback(
            if russian { "📊 Все мои" } else { "📊 All holdings" },
            cb_key("all", eff_budget, &sel, ""),
        ),
        InlineKeyboardButton::callback(
            if russian { "⭐ Watchlist" } else { "⭐ Watchlist" },
            cb_key("wl", eff_budget, &sel, ""),
        ),
    ]);

    // Budget presets.
    let mut brow: Vec<InlineKeyboardButton> = Vec::new();
    for p in BUDGET_PRESETS {
        let label = if (eff_budget - p).abs() < 0.5 {
            format!("✅ ${}", format_thousands(p, 0))
        } else {
            format!("${}", format_thousands(p, 0))
        };
        brow.push(InlineKeyboardButton::callback(
            label,
            cb_key("bud", p, &sel, ""),
        ));
    }
    rows.push(brow);

    // Build + hint.
    rows.push(vec![InlineKeyboardButton::callback(
        if russian {
            "🎯 Построить ловушки"
        } else {
            "🎯 Build ladder"
        },
        cb_key("go", eff_budget, &sel, ""),
    )]);
    rows.push(vec![InlineKeyboardButton::callback(
        if russian {
            "💵 Бюджет вручную"
        } else {
            "💵 Custom budget"
        },
        cb_key("custom", eff_budget, &sel, ""),
    )]);

    (text, InlineKeyboardMarkup::new(rows))
}

// ── callback entry point ────────────────────────────────────────────────────

pub async fn handle_ladder_callback(
    bot: Bot,
    q: CallbackQuery,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    sqlite: Arc<SqliteStore>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    let _ = bot.answer_callback_query(&q.id).await;
    let data = q.data.clone().unwrap_or_default();
    let Some(cb) = parse_cb(&data) else {
        return Ok(());
    };
    let (chat_id, message_id) = callback_target(&q);
    let user_id = q.from.id.0;
    if !config.is_admin(user_id) {
        return Ok(());
    }
    let russian = is_russian(&store, user_id);
    let (holdings, watchlist) = candidates(&store, user_id);

    let budget = cb.budget;
    let mut symbols = resolve_selection(&cb.csv, &holdings);

    match cb.op.as_str() {
        "sel" => {
            let sym = cb.extra.to_uppercase();
            if sym.is_empty() {
                return Ok(());
            }
            if let Some(pos) = symbols.iter().position(|s| *s == sym) {
                symbols.remove(pos);
            } else if symbols.len() < MAX_SYMBOLS {
                symbols.push(sym);
            }
        }
        "all" => {
            symbols = holdings.clone();
        }
        "wl" => {
            symbols = watchlist.iter().take(MAX_SYMBOLS).cloned().collect();
        }
        "bud" => {
            // budget already parsed from the payload (finite, > 0).
        }
        "custom" => {
            // No text-input dialogue; show a copy-ready command with the current
            // selection so the user can type any amount they like.
            let syms = if symbols.is_empty() {
                "-".to_string()
            } else {
                symbols.join(" ")
            };
            let hint = if russian {
                format!(
                    "✏️ Чтобы задать свой бюджет — отправь команду:\n<code>/ladder {} СУММА</code>\n\nНапример: <code>/ladder {} 750</code>",
                    syms, syms
                )
            } else {
                format!(
                    "✏️ To set a custom budget, send:\n<code>/ladder {} AMOUNT</code>\n\nExample: <code>/ladder {} 750</code>",
                    syms, syms
                )
            };
            let _ = bot
                .send_message(chat_id, &hint)
                .parse_mode(ParseMode::Html)
                .await;
            let (text, kb) = render_menu(&store, user_id, budget, &symbols, russian);
            return send_or_edit(&bot, chat_id, message_id, text, kb).await;
        }
        "menu" => {
            let (text, kb) = render_menu(&store, user_id, budget, &symbols, russian);
            return send_or_edit(&bot, chat_id, message_id, text, kb).await;
        }
        "go" => {
            if symbols.is_empty() {
                let msg = if russian {
                    "Выбери хотя бы одну монету."
                } else {
                    "Pick at least one coin."
                };
                let (text, kb) = render_menu(&store, user_id, budget, &symbols, russian);
                let _ = bot.send_message(chat_id, msg).await;
                return send_or_edit(&bot, chat_id, message_id, text, kb).await;
            }
            let _ = bot
                .send_chat_action(chat_id, teloxide::types::ChatAction::Typing)
                .await;
            let text =
                build_ladder_text(&store, &cmc, &sqlite, user_id, &symbols, budget, russian).await;
            let kb = result_kb(budget, &symbols);
            return send_or_edit(&bot, chat_id, message_id, text, kb).await;
        }
        _ => return Ok(()),
    }

    // Toggling / selection change → re-render the menu in place.
    let (text, kb) = render_menu(&store, user_id, budget, &symbols, russian);
    send_or_edit(&bot, chat_id, message_id, text, kb).await
}

async fn send_or_edit(
    bot: &Bot,
    chat_id: ChatId,
    message_id: Option<MessageId>,
    text: String,
    kb: InlineKeyboardMarkup,
) -> anyhow::Result<()> {
    if let Some(mid) = message_id {
        if bot
            .edit_message_text(chat_id, mid, &text)
            .parse_mode(ParseMode::Html)
            .reply_markup(kb.clone())
            .await
            .is_ok()
        {
            return Ok(());
        }
        // Edit failed (e.g. identical content) — ignore; keep the existing view.
    } else {
        let _ = bot
            .send_message(chat_id, &text)
            .parse_mode(ParseMode::Html)
            .reply_markup(kb)
            .await;
    }
    Ok(())
}

fn result_kb(budget: f64, symbols: &[String]) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        vec![
            InlineKeyboardButton::callback("🔄 Rebuild", cb_key("go", budget, symbols, "")),
            InlineKeyboardButton::callback("✏️ Adjust", cb_key("menu", budget, symbols, "")),
        ],
        vec![InlineKeyboardButton::callback(
            "🎯 New budget",
            cb_key("custom", budget, symbols, ""),
        )],
    ])
}

// ── ladder computation + rendering ──────────────────────────────────────────

/// Build the ladder text for `symbols` with `budget` split equally across them.
async fn build_ladder_text(
    store: &Store,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    user_id: u64,
    symbols: &[String],
    budget: f64,
    russian: bool,
) -> String {
    let symbols: Vec<String> = symbols.iter().take(MAX_SYMBOLS).cloned().collect();
    if symbols.is_empty() {
        return if russian {
            "Выбери хотя бы одну монету.".to_string()
        } else {
            "Pick at least one coin.".to_string()
        };
    }

    // One read for all holdings / averages / protection flag.
    let (avg_map, pos_map, protect) = store.read(user_id, |p| {
        let held = p.holdings_by_symbol();
        let mut avg = HashMap::new();
        let mut pos = HashMap::new();
        for s in &symbols {
            avg.insert(s.clone(), p.avg_buy_price(s));
            pos.insert(s.clone(), held.get(s).copied().unwrap_or(0.0));
        }
        (avg, pos, p.settings.sniper_protect_avg_entry)
    });

    let per_coin = budget / symbols.len() as f64;

    // Fetch the sniper report per coin (bounded), sequentially to stay gentle.
    let mut reports: Vec<(String, Option<f64>, f64, Option<DipSniperReport>)> = Vec::new();
    for sym in &symbols {
        let avg = avg_map.get(sym).copied().flatten();
        let position = *pos_map.get(sym).unwrap_or(&0.0);
        let res = tokio::time::timeout(
            PER_SYMBOL_TIMEOUT,
            build_sniper_report(sym, cmc, sqlite, None, avg, position, per_coin, protect),
        )
        .await;
        let report = match res {
            Ok(Ok(r)) => Some(r),
            Ok(Err(e)) => {
                warn!("ladder: sniper failed for {}: {e}", sym);
                None
            }
            Err(_) => {
                warn!("ladder: sniper timed out for {}", sym);
                None
            }
        };
        reports.push((sym.clone(), avg, position, report));
    }

    render_ladder(&reports, budget, per_coin, russian)
}

/// Render the final ladder table.
fn render_ladder(
    reports: &[(String, Option<f64>, f64, Option<DipSniperReport>)],
    budget: f64,
    per_coin: f64,
    russian: bool,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "🎯 <b>{}</b>\n",
        if russian {
            "Лестница лимитных покупок"
        } else {
            "Buy Ladder"
        }
    ));
    out.push_str(&format!(
        "{} <b>${}</b> · {} <b>${}</b>\n",
        if russian { "Бюджет:" } else { "Budget:" },
        format_thousands(budget, 0),
        if russian { "на монету:" } else { "per coin:" },
        format_thousands(per_coin, 0)
    ));

    let mut total_planned = 0.0_f64;
    let mut traps = 0usize;
    // (name, pct_vs_avg, rsi) for the "best to catch" pick.
    let mut picks: Vec<(String, f64, Option<f64>)> = Vec::new();
    let mut any_rendered = false;

    for (sym, avg, position, report) in reports {
        let Some(report) = report else {
            out.push_str(&format!(
                "\n▫️ <b>{sym}</b> — {} \n",
                if russian {
                    "не удалось получить данные, пропускаю"
                } else {
                    "data unavailable, skipped"
                }
            ));
            continue;
        };
        any_rendered = true;

        let spot = report.price;
        let avg = avg.filter(|a| a.is_finite() && *a > 0.0);
        let mut head = format!("<b>{sym}</b> · spot {}", fmt_price(spot));
        if let Some(a) = avg {
            let pct = (spot / a - 1.0) * 100.0;
            head.push_str(&format!(
                " · {} {} <b>{:+.1}%</b>",
                if russian { "средняя" } else { "avg" },
                fmt_price(a),
                pct
            ));
            picks.push((sym.clone(), pct, report.rsi14));
        } else {
            picks.push((sym.clone(), 0.0, report.rsi14));
        }
        if let Some(rsi) = report.rsi14 {
            head.push_str(&format!(" · RSI {rsi:.0}"));
        }
        out.push_str(&format!("\n{head}\n"));

        // Sort shallow → deep so fills accumulate top-down.
        let mut buckets: Vec<_> = report.buckets.iter().collect();
        buckets.sort_by(|a, b| {
            b.limit_price
                .partial_cmp(&a.limit_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Running position/cost so the "new avg" column is cumulative.
        let mut run_pos = *position;
        let mut run_cost = position * avg.unwrap_or(spot);

        let mut lines: Vec<String> = vec![format!(
            "{}",
            format!(
                "{} {} {} {} {} {} {}",
                right("#", 2),
                right(if russian { "лимит" } else { "limit" }, 9),
                right("disc", 7),
                right("P30", 4),
                right(if russian { "сумма" } else { "spend" }, 7),
                right(if russian { "кол-во" } else { "qty" }, 11),
                right(if russian { "средн" } else { "newavg" }, 9),
            )
        )];
        lines.push("-".repeat(54));

        for (i, b) in buckets.iter().enumerate() {
            let qty = if b.limit_price > 0.0 {
                b.spend_usd / b.limit_price
            } else {
                0.0
            };
            run_pos += qty;
            run_cost += b.spend_usd;
            let new_avg = if run_pos > 0.0 { run_cost / run_pos } else { 0.0 };
            total_planned += b.spend_usd;
            traps += 1;

            let marker = if b.triggered { "⚡" } else { " " };
            lines.push(format!(
                "{}{} {} {} {} {} {} {}",
                marker,
                right(&(i + 1).to_string(), 1),
                right(&price_str(b.limit_price), 9),
                right(&format!("{:+.1}%", b.discount_pct), 7),
                right(&format!("{:.0}%", b.prob_touch_30d * 100.0), 4),
                right(&format!("${}", format_thousands(b.spend_usd, 0)), 7),
                right(&qty_str(qty), 11),
                right(&price_str(new_avg), 9),
            ));
        }

        out.push_str("<pre>");
        out.push_str(&lines.join("\n"));
        out.push_str("</pre>");

        // Coin summary: final blended average if every tranche fills.
        let total_spend: f64 = buckets.iter().map(|b| b.spend_usd).sum();
        let total_qty: f64 = buckets
            .iter()
            .map(|b| if b.limit_price > 0.0 { b.spend_usd / b.limit_price } else { 0.0 })
            .sum();
        let blended = if (position + total_qty) > 0.0 {
            (position * avg.unwrap_or(spot) + total_spend) / (position + total_qty)
        } else {
            0.0
        };
        out.push_str(&format!(
            "\n{} ${} → {} → {} {} {}\n",
            if russian { "Σ" } else { "Σ" },
            format_thousands(total_spend, 0),
            qty_str(total_qty),
            if russian { "средняя →" } else { "blended avg" },
            fmt_price(blended),
            if let Some(a) = avg {
                format!("({:+.1}% {})", (blended / a - 1.0) * 100.0, if russian { "к твоей" } else { "vs yours" })
            } else {
                String::new()
            }
        ));
    }

    if !any_rendered {
        return format!(
            "🎯 <b>Buy Ladder</b>\n\n{}",
            if russian {
                "Не удалось получить данные ни по одной монете. Попробуй позже."
            } else {
                "Could not fetch data for any coin. Try again shortly."
            }
        );
    }

    // Footer: totals + the best coin to catch right now.
    out.push_str(&format!(
        "\n━━━━━━━━━━\n{} ${} · {} {}\n",
        if russian { "Итого ловушек:" } else { "Total planned:" },
        format_thousands(total_planned, 0),
        traps,
        if russian { "ордеров" } else { "traps" }
    ));

    if let Some((name, pct, rsi)) = best_pick(&picks) {
        out.push_str(&format!(
            "🎯 {} <b>{}</b>",
            if russian { "Лучше ловить сейчас:" } else { "Best to catch now:" },
            name
        ));
        if pct.abs() > 0.01 {
            out.push_str(&format!(
                " (spot {:+.1}% {}, RSI {})",
                pct,
                if russian { "к средней" } else { "vs avg" },
                rsi.map(|r| format!("{r:.0}")).unwrap_or_else(|| "—".into())
            ));
        }
        out.push('\n');
    }

    out.push_str(&format!(
        "\n<i>{}</i>",
        if russian {
            "Уровни из конуса волатильности /forecast; P30 — вероятность коснуться за 30 дней. Не финансовый совет."
        } else {
            "Levels from the /forecast volatility cone; P30 = chance to touch within 30d. Not financial advice."
        }
    ));
    out
}

/// Pick the coin most worth catching now: lowest price vs its average, with a
/// low-RSI tie-break bonus.
fn best_pick(picks: &[(String, f64, Option<f64>)]) -> Option<(String, f64, Option<f64>)> {
    picks
        .iter()
        .min_by(|a, b| {
            let sa = a.1 + a.2.map(|r| (r - 30.0).max(0.0) * 0.05).unwrap_or(0.0);
            let sb = b.1 + b.2.map(|r| (r - 30.0).max(0.0) * 0.05).unwrap_or(0.0);
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
}
