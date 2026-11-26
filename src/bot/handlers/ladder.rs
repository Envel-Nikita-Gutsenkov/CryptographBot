//! `/ladder [SYMS] [BUDGET]` — interactive multi-coin limit-order ("traps")
//! planner.
//!
//! The user picks a budget and a set of coins; the bot turns that into a table
//! of concrete limit buys *below* spot, each with its touch probability, the
//! spend, the quantity and the resulting blended average entry — the same
//! question the user used to ask an external model, answered from the local
//! math.
//!
//! Defaults are smart: the coin set seeds from the user's holdings, and an
//! active buy goal ("докупить ETH и SOL на 600$") pre-selects the named coins
//! and supplies its budget, so `/ladder` with no arguments usually already
//! describes the trade the user meant to plan.
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
    trading::{
        dipzones::touch_probability,
        sniper::{DipSniperReport, TRAP_HORIZON_DAYS},
    },
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

/// A hint derived from the user's active goals: an optional spend budget, the
/// source goal, and any coin symbols the goal names (e.g. "докупить ETH и SOL
/// на 500$" ⇒ budget 500, assets [ETH, SOL]).
struct GoalHint {
    budget: Option<f64>,
    id: u64,
    text: String,
    assets: Vec<String>,
}

/// Coins we recognise in free-text goals. Kept broad on purpose: a goal like
/// "докупить ETH и SOL" should pre-select those rows without the user retyping.
const TRACKED_SYMBOLS: &[&str] = &[
    "BTC", "ETH", "SOL", "LINK", "BNB", "XRP", "ADA", "DOGE", "AVAX", "DOT", "MATIC",
    "POL", "TON", "TRX", "LTC", "ATOM", "NEAR", "ARB", "OP", "SUI", "APT", "INJ", "TIA",
    "SEI", "PEPE", "SHIB", "UNI", "AAVE", "MKR", "CRV", "FIL", "ICP", "ETC", "XLM", "ALGO",
    "VET", "HBAR", "RNDR", "FET", "TAO", "WIF", "BONK", "JUP", "PYTH", "STX", "GRT", "LDO",
];

/// Symbols named in a goal's free-text, in the order [`TRACKED_SYMBOLS`] lists
/// them. Whole-word matching keeps "sol" out of "solution" and "link" out of
/// "linking".
/// Russian/transliterated coin names, so a goal written as "докупить эфир
/// и соланы" pre-selects ETH and SOL. Kept local to goal parsing: the shared
/// [`aliases_for`](crate::crypto::client::aliases_for) table stays English, as
/// it also drives news matching where the feeds are English.
fn russian_aliases(sym: &str) -> &'static [&'static str] {
    match sym {
        "BTC" => &["биткоин", "биткойн", "битк"],
        "ETH" => &["эфир", "эфириум", "эфира"],
        "SOL" => &["солана", "соланы", "солане", "солян"],
        "LINK" => &["чейнлинк", "линк"],
        "BNB" => &["бинанс", "бинанса"],
        "XRP" => &["рипл", "риппл"],
        "ADA" => &["кардано"],
        "DOGE" => &["догикоин", "доги"],
        "AVAX" => &["аваланч", "авалакс"],
        "DOT" => &["полкадот", "полкадоты"],
        "MATIC" | "POL" => &["полигон", "матик"],
        "TON" => &["тоникойн"],
        "TRX" => &["трон"],
        "LTC" => &["лайткоин", "лайткойн"],
        "SHIB" => &["шиба", "шибаину"],
        "UNI" => &["юнисвоп"],
        "ATOM" => &["космос"],
        "ARB" => &["арбитрум"],
        "INJ" => &["инжектив"],
        "APT" => &["аптос"],
        "TIA" => &["селестия"],
        "OP" => &["оптимизм"],
        "PEPE" => &["пепе"],
        _ => &[],
    }
}

fn goal_assets_from_text(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut out: Vec<String> = Vec::new();
    for &sym in TRACKED_SYMBOLS {
        let mut needles: Vec<String> = crate::crypto::client::aliases_for(sym)
            .map(|a| a.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        needles.extend(russian_aliases(sym).iter().map(|s| s.to_string()));
        needles.push(sym.to_lowercase());
        let hit = needles
            .iter()
            .any(|n| crate::crypto::client::contains_word_lower(&lower, n));
        if hit && !out.iter().any(|s| s == sym) {
            out.push(sym.to_string());
        }
    }
    out
}

/// First integer in `50..=100_000` found in the text, used as a spend budget.
fn first_budget(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b',') {
                i += 1;
            }
            let raw: String = text[start..i].chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(v) = raw.parse::<f64>() {
                if (50.0..=100_000.0).contains(&v) {
                    return Some(v);
                }
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Look through the user's active goals for a budget and/or coins to buy.
/// A goal that names both a budget and coins wins over a weaker one; otherwise
/// the first goal with either signal is returned. Budgets are only trusted when
/// a buying keyword is present, so an "alert at 70000" goal is not mistaken for
/// a spend budget.
fn goal_hint(store: &Store, user_id: u64) -> Option<GoalHint> {
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
    let mut fallback: Option<GoalHint> = None;
    for (id, text) in goals {
        let lower = text.to_lowercase();
        let has_buy_word = KEYS.iter().any(|k| lower.contains(k));
        let assets = goal_assets_from_text(&text);
        if !has_buy_word && assets.is_empty() {
            continue;
        }
        let budget = if has_buy_word { first_budget(&text) } else { None };
        if budget.is_some() && !assets.is_empty() {
            return Some(GoalHint { budget, id, text, assets });
        }
        if fallback.is_none() {
            fallback = Some(GoalHint { budget, id, text, assets });
        }
    }
    fallback
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

/// Quote decimals that keep small-cap prices readable, with thousands
/// separators so large prices match the header formatting (e.g. `57,000.0`).
fn price_str(p: f64) -> String {
    if p >= 1000.0 {
        crate::format::format_thousands(p, 1)
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

    // Goal hint drives both the budget and the default coin selection.
    let goal = goal_hint(store, user_id);
    let goal_assets = goal.as_ref().map(|g| g.assets.clone()).unwrap_or_default();

    // Default selection: coins named in the goal, else the holdings; an explicit
    // list stays as given.
    let sel: Vec<String> = if selection.is_empty() {
        if goal_assets.is_empty() {
            holdings.clone()
        } else {
            goal_assets.clone()
        }
    } else {
        selection.to_vec()
    };

    // Budget: explicit → keep; otherwise the goal budget; otherwise the default.
    let (eff_budget, goal_line) = if budget > 0.0 {
        (budget, None)
    } else if let Some(g) = &goal {
        let snippet: String = g.text.chars().take(48).collect();
        (
            g.budget.unwrap_or(DEFAULT_BUDGET),
            Some((g.id, snippet, g.assets.clone())),
        )
    } else {
        (DEFAULT_BUDGET, None)
    };

    // Candidate coins: holdings, then watchlist, then any goal coin not yet
    // listed — so a "buy ETH" goal surfaces ETH even if it is not held.
    let mut coins: Vec<String> = holdings.clone();
    for w in &watchlist {
        if !coins.contains(w) && coins.len() < MAX_SYMBOLS {
            coins.push(w.clone());
        }
    }
    for a in &goal_assets {
        if !coins.contains(a) && coins.len() < MAX_SYMBOLS {
            coins.push(a.clone());
        }
    }

    let title = if russian {
        "🎯 <b>Лестница покупок</b> — ловушки ниже рынка"
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
    if let Some((id, snippet, assets)) = goal_line {
        let asset_note = if assets.is_empty() {
            String::new()
        } else {
            format!(
                " → <b>{}</b>",
                assets
                    .iter()
                    .map(|a| html_escape::encode_text(a))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        text.push_str(&format!(
            "💡 {} #{}: <i>{}</i>{}\n",
            if russian { "из цели" } else { "from goal" },
            id,
            html_escape::encode_text(&snippet),
            asset_note
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

/// Smallest per-order size we try not to go below when splitting a budget.
const LADDER_MIN_ORDER_USD: f64 = 20.0;

/// Rounding granularity for order sizes, scaled to the budget.
fn nice_step(v: f64) -> f64 {
    if v >= 1000.0 {
        50.0
    } else if v >= 300.0 {
        25.0
    } else if v >= 100.0 {
        10.0
    } else if v >= 30.0 {
        5.0
    } else {
        1.0
    }
}

fn round_to(v: f64, step: f64) -> f64 {
    if step <= 0.0 {
        v.max(0.0)
    } else {
        (v / step).round() * step
    }
}

/// Pyramid of tranche weights for `k` levels (shallow → deep).
fn ladder_weights(k: usize) -> Vec<f64> {
    match k {
        0 => vec![],
        1 => vec![1.0],
        2 => vec![0.4, 0.6],
        _ => vec![0.20, 0.35, 0.45],
    }
}

/// Split one coin's budget into a sensible number of round-sized tranches.
/// Small budgets collapse to a single order instead of three tiny ones, and
/// amounts are rounded to human-friendly steps so the plan reads cleanly.
fn allocate_ladder(per_coin: f64, max_levels: usize) -> Vec<f64> {
    let max_levels = max_levels.clamp(1, 3);
    if !(per_coin > 0.0) || !per_coin.is_finite() {
        return vec![per_coin.max(0.0)];
    }
    let affordable = (per_coin / LADDER_MIN_ORDER_USD).floor() as usize;
    let k = affordable.clamp(1, max_levels);
    let step = nice_step(per_coin);
    let mut spends: Vec<f64> = ladder_weights(k)
        .iter()
        .map(|w| round_to(per_coin * w, step))
        .collect();
    // Make the tranches add up to the exact budget; absorb the remainder in
    // the deepest (largest) tranche so rounding never loses or invents money.
    let target = round_to(per_coin, step);
    let sum: f64 = spends.iter().sum();
    if let Some(last) = spends.last_mut() {
        *last = (*last + (target - sum)).max(step);
    }
    spends.retain(|s| *s > 0.0);
    if spends.is_empty() {
        spends.push(per_coin);
    }
    spends
}

/// Standard market-drop levels (percent below spot) shown as scenarios.
const LADDER_SCENARIOS_PCT: [f64; 3] = [10.0, 20.0, 30.0];

/// Human label for a market regime (`EUPHORIA` → `эйфория` …).
fn regime_label(regime: &str, russian: bool) -> String {
    if !russian {
        return regime.to_string();
    }
    let label = match regime.to_ascii_uppercase().as_str() {
        "EUPHORIA" => "эйфория",
        "DIP" => "просадка",
        "BEAR" => "медвежий",
        "BULL" => "бычий",
        "SIDEWAYS" => "боковик",
        _ => return regime.to_string(),
    };
    label.to_string()
}

/// One market-drop scenario: what the ladder does if price falls `drop_pct`
/// percent within the trap horizon.
struct Scenario {
    drop_pct: f64,
    level: f64,
    prob: f64,
    deployed: f64,
    blended: Option<f64>,
}

/// Build the −10/−20/−30% scenarios from the same diffusion the ladder uses, so
/// the odds line up with the P14 column instead of being invented.
///
/// `levels` are the planned limit prices (shallow → deep) aligned with `spends`.
fn ladder_scenarios(
    spot: f64,
    mu: f64,
    sigma: f64,
    levels: &[f64],
    spends: &[f64],
    avg: Option<f64>,
    position: f64,
) -> Vec<Scenario> {
    let base_cost = position * avg.unwrap_or(spot);
    LADDER_SCENARIOS_PCT
        .iter()
        .map(|&d| {
            let level = spot * (1.0 - d / 100.0);
            let prob = touch_probability(spot, level, mu, sigma.max(0.003), TRAP_HORIZON_DAYS);
            let mut deployed = 0.0;
            let mut qty = 0.0;
            for (&lvl, &s) in levels.iter().zip(spends.iter()) {
                if lvl > 0.0 && lvl + 1e-9 >= level {
                    deployed += s;
                    qty += s / lvl;
                }
            }
            let blended = if position + qty > 0.0 {
                Some((base_cost + deployed) / (position + qty))
            } else {
                None
            };
            Scenario {
                drop_pct: d,
                level,
                prob,
                deployed,
                blended,
            }
        })
        .collect()
}

/// Format one scenario as a single compact line.
fn scenario_line(sc: &Scenario, russian: bool) -> String {
    let prob = format!("{:.0}%", (sc.prob * 100.0).round());
    let odds = if russian {
        format!("шанс {prob}")
    } else {
        format!("{prob} odds")
    };
    let head = format!("📉 −{:.0}% ({}) · {odds}", sc.drop_pct, fmt_price(sc.level));
    if sc.deployed <= 0.0 {
        return format!(
            "{head} · {}",
            if russian {
                "ни один ордер не зацепится"
            } else {
                "no fill"
            }
        );
    }
    let avg_part = sc
        .blended
        .map(|b| format!(" → {} {}", if russian { "ср" } else { "avg" }, fmt_price(b)))
        .unwrap_or_default();
    format!(
        "{head} · {} ${}{avg_part}",
        if russian { "вложишь" } else { "deploy" },
        format_thousands(sc.deployed, 0),
    )
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
        let mut head = format!(
            "<b>{sym}</b> · {} {}",
            if russian { "спот" } else { "spot" },
            fmt_price(spot)
        );
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
        // Market read: current regime + how much this coin actually moves per day.
        head.push_str(&format!(
            " · {} · σ {:.1}%{}",
            regime_label(&report.regime, russian),
            report.vol_daily_pct,
            if russian { "/д" } else { "/d" }
        ));
        out.push_str(&format!("\n{head}\n"));

        // Sort shallow → deep so fills accumulate top-down.
        let mut buckets: Vec<_> = report.buckets.iter().collect();
        buckets.sort_by(|a, b| {
            b.limit_price
                .partial_cmp(&a.limit_price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Right-size the tranches to this coin's budget: small budgets get one
        // clean order, larger ones the full three-level pyramid.
        let spends = allocate_ladder(per_coin, buckets.len());
        // Limit prices aligned with `spends` (only the tranches actually planned).
        let levels: Vec<f64> = buckets
            .iter()
            .zip(spends.iter())
            .map(|(b, _)| b.limit_price)
            .collect();

        // Running position/cost so the "new avg" column is cumulative.
        let mut run_pos = *position;
        let mut run_cost = position * avg.unwrap_or(spot);

        let mut lines: Vec<String> = vec![format!(
            "{}",
            format!(
                "{} {} {} {} {} {} {}",
                right("#", 2),
                right(if russian { "лимит" } else { "limit" }, 9),
                right(if russian { "скидка" } else { "disc" }, 7),
                right("P14", 4),
                right(if russian { "сумма" } else { "spend" }, 7),
                right(if russian { "кол-во" } else { "qty" }, 11),
                right(if russian { "средняя" } else { "newavg" }, 11),
            )
        )];
        lines.push("-".repeat(54));

        for (i, (b, &spend)) in buckets.iter().zip(spends.iter()).enumerate() {
            let qty = if b.limit_price > 0.0 {
                spend / b.limit_price
            } else {
                0.0
            };
            run_pos += qty;
            run_cost += spend;
            let new_avg = if run_pos > 0.0 { run_cost / run_pos } else { 0.0 };
            total_planned += spend;
            traps += 1;

            let marker = if b.triggered { "⚡" } else { " " };
            lines.push(format!(
                "{}{} {} {} {} {} {} {}",
                marker,
                right(&(i + 1).to_string(), 1),
                right(&price_str(b.limit_price), 9),
                right(&format!("{:+.1}%", b.discount_pct), 7),
                right(&format!("{:.0}%", b.prob_touch * 100.0), 4),
                right(&format!("${}", format_thousands(spend, 0)), 7),
                right(&qty_str(qty), 11),
                right(&price_str(new_avg), 11),
            ));
        }

        out.push_str("<pre>");
        out.push_str(&lines.join("\n"));
        out.push_str("</pre>");

        // Coin summary: final blended average if every tranche fills.
        let total_spend: f64 = spends.iter().sum();
        let total_qty: f64 = buckets
            .iter()
            .zip(spends.iter())
            .map(|(b, &s)| if b.limit_price > 0.0 { s / b.limit_price } else { 0.0 })
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

        // Scenario band: what this ladder does on a −10/−20/−30% market move,
        // scored with the same diffusion as the P14 column.
        let scenarios = ladder_scenarios(
            spot,
            report.drift_daily_pct / 100.0,
            report.vol_daily_pct / 100.0,
            &levels,
            &spends,
            avg,
            *position,
        );
        let sc: Vec<String> = scenarios
            .iter()
            .map(|s| scenario_line(s, russian))
            .collect();
        out.push_str(&sc.join("\n"));
        out.push('\n');
    }

    if !any_rendered {
        return format!(
            "🎯 <b>{}</b>\n\n{}",
            if russian {
                "Лестница лимитных покупок"
            } else {
                "Buy Ladder"
            },
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
        if russian { "Запланировано:" } else { "Total planned:" },
        format_thousands(total_planned, 0),
        traps,
        if russian {
            orders_word_ru(traps)
        } else if traps == 1 {
            "trap"
        } else {
            "traps"
        }
    ));

    if let Some((name, pct, rsi)) = best_pick(&picks) {
        // Only claim something is worth catching when it actually is: below the
        // average, or genuinely oversold. A coin above its average with a normal
        // RSI is not a dip, so say so plainly instead of overselling it.
        let rsi_n = rsi.map(|r| format!("{r:.0}")).unwrap_or_else(|| "—".into());
        let is_dip = pct < -1.0 || rsi.is_some_and(|r| r < 35.0);
        let lead = if is_dip {
            if russian { "🎯 Лучше ловить сейчас:" } else { "🎯 Best to catch now:" }
        } else if russian {
            "💤 Ловить пока нечего, ближе всех:"
        } else {
            "💤 Nothing dipping right now; closest:"
        };
        out.push_str(&format!("{lead} <b>{name}</b>"));
        if pct.abs() > 0.01 || rsi.is_some() {
            out.push_str(&format!(
                " ({} {:+.1}% {}, RSI {rsi_n})",
                if russian { "спот" } else { "spot" },
                pct,
                if russian { "к средней" } else { "vs avg" },
            ));
        }
        out.push('\n');
    }

    out.push_str(&format!(
        "\n<i>{}</i>",
        if russian {
            "Уровни подобраны под реальную вероятность касания за 14 дней (диффузия /dip): P14 — шанс, что лимит зацепит в этот срок."
        } else {
            "Levels are solved for a real 14-day touch probability (the /dip diffusion): P14 = chance the limit fills within that window."
        }
    ));
    if per_coin < LADDER_MIN_ORDER_USD * 3.0 {
        out.push_str(&format!(
            "\n<i>{}</i>",
            if russian {
                "Малый бюджет на монету → меньше, но более крупных ордеров вместо горстки мелких."
            } else {
                "Small per-coin budget → fewer, larger tranches instead of a handful of tiny ones."
            }
        ));
    }
    out.push_str(&format!(
        "\n<i>{}</i>",
        if russian {
            "Сценарии −10/−20/−30% — та же диффузия: шанс такого падения за 14 дней и что из ладдера исполнится (сумма и средняя после закупа)."
        } else {
            "−10/−20/−30% scenarios use the same diffusion: the odds of such a drop within 14 days and what of the ladder would fill (spend and post-buy avg)."
        }
    ));
    out
}

/// Pick the coin most worth catching now: deepest discount to its average,
/// nudged by RSI so an overbought name (poor entry) is demoted and an oversold
/// one (currency in a capitulation) is promoted.
fn best_pick(picks: &[(String, f64, Option<f64>)]) -> Option<(String, f64, Option<f64>)> {
    let score = |r: &(String, f64, Option<f64>)| -> f64 {
        let rsi_adj = match r.2 {
            Some(rsi) if rsi > 60.0 => (rsi - 60.0) * 0.1,  // overbought → worse entry
            Some(rsi) if rsi < 35.0 => -(35.0 - rsi) * 0.1, // oversold → better entry
            _ => 0.0,
        };
        r.1 + rsi_adj
    };
    picks
        .iter()
        .min_by(|a, b| {
            score(a)
                .partial_cmp(&score(b))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
}

/// Russian plural for "ордер" (1 → ордер, 2–4 → ордера, 5–20 → ордеров, …).
fn orders_word_ru(n: usize) -> &'static str {
    let m100 = n % 100;
    let m10 = n % 10;
    if (11..=14).contains(&m100) {
        "ордеров"
    } else if m10 == 1 {
        "ордер"
    } else if (2..=4).contains(&m10) {
        "ордера"
    } else {
        "ордеров"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_assets_picks_named_coins() {
        let a = goal_assets_from_text("докупить ETH и SOL на 500$");
        assert!(a.contains(&"ETH".to_string()));
        assert!(a.contains(&"SOL".to_string()));
    }

    #[test]
    fn goal_assets_uses_names_not_just_tickers() {
        let a = goal_assets_from_text("подкупать биткоин на просадках");
        assert_eq!(a, vec!["BTC".to_string()]);
    }

    #[test]
    fn goal_assets_ignores_whole_word_false_positives() {
        // "solution" must not read as SOL, "linkedin" must not read as LINK.
        let a = goal_assets_from_text("find a solution and post it on linkedin");
        assert!(a.is_empty(), "unexpected matches: {a:?}");
    }

    #[test]
    fn first_budget_reads_a_plausible_number() {
        assert_eq!(first_budget("купить на 500$"), Some(500.0));
        assert_eq!(first_budget("spend 1,000 usd"), Some(1000.0));
        // Too small to be a budget, and no number at all → none.
        assert_eq!(first_budget("2 coils"), None);
        assert_eq!(first_budget("докупить"), None);
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.5
    }

    #[test]
    fn allocate_small_budget_is_a_single_order() {
        assert_eq!(allocate_ladder(25.0, 3), vec![25.0]);
        // Below the min order size it still makes exactly one order.
        assert_eq!(allocate_ladder(10.0, 3), vec![10.0]);
    }

    #[test]
    fn allocate_two_tranches_for_modest_budget() {
        let s = allocate_ladder(45.0, 3);
        assert_eq!(s.len(), 2, "got {s:?}");
        assert!(approx(s.iter().sum::<f64>(), 45.0), "sum {s:?}");
    }

    #[test]
    fn allocate_full_pyramid_for_large_budget() {
        for budget in [60.0, 250.0, 1000.0] {
            let s = allocate_ladder(budget, 3);
            assert_eq!(s.len(), 3, "budget {budget} -> {s:?}");
            assert!(approx(s.iter().sum::<f64>(), budget), "sum {s:?}");
            // Deepest tranche is the largest (pyramid shape preserved).
            assert!(s[2] >= s[0], "not a pyramid: {s:?}");
            // No tiny crumbs.
            assert!(s.iter().all(|v| *v >= 5.0), "tiny tranche in {s:?}");
        }
    }

    #[test]
    fn allocate_respects_one_level_max() {
        assert_eq!(allocate_ladder(500.0, 1), vec![500.0]);
    }

    #[test]
    fn regime_label_localizes_when_russian() {
        assert_eq!(regime_label("SIDEWAYS", true), "боковик");
        assert_eq!(regime_label("SIDEWAYS", false), "SIDEWAYS");
        // Unknown regimes pass through untouched.
        assert_eq!(regime_label("WEIRD", true), "WEIRD");
    }

    #[test]
    fn scenarios_deploy_more_and_are_less_likely_on_deeper_drops() {
        // Spot 100, flat drift, ~3%/day vol; tranches planned at −5/−10/−20%.
        let levels = [95.0, 90.0, 80.0];
        let spends = [10.0, 20.0, 30.0];
        let sc = ladder_scenarios(100.0, 0.0, 0.03, &levels, &spends, None, 0.0);

        let d10 = sc.iter().find(|s| (s.drop_pct - 10.0).abs() < 1e-9).unwrap();
        // −10% (level 90) reaches the first two tranches (95, 90).
        assert!((d10.deployed - 30.0).abs() < 1e-6, "got {}", d10.deployed);
        assert!(d10.blended.unwrap() < 100.0);

        let d30 = sc.iter().find(|s| (s.drop_pct - 30.0).abs() < 1e-9).unwrap();
        // −30% (level 70) reaches all three planned tranches.
        assert!((d30.deployed - 60.0).abs() < 1e-6, "got {}", d30.deployed);
        // A deeper drop is never more likely than a shallower one.
        assert!(d10.prob >= d30.prob);
    }

    #[test]
    fn scenarios_with_no_planned_orders_deploy_nothing() {
        let sc = ladder_scenarios(100.0, 0.0, 0.03, &[], &[], None, 0.0);
        assert!(sc.iter().all(|s| s.deployed == 0.0));
        assert!(sc.iter().all(|s| s.blended.is_none()));
    }

    fn has_cyrillic(s: &str) -> bool {
        s.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c))
    }

    #[test]
    fn scenario_line_is_fully_english_or_fully_russian() {
        let sc = Scenario {
            drop_pct: 20.0,
            level: 80.0,
            prob: 0.15,
            deployed: 120.0,
            blended: Some(92.0),
        };
        let en = scenario_line(&sc, false);
        assert!(!has_cyrillic(&en), "english line leaked cyrillic: {en}");
        assert!(en.contains("deploy") && en.contains("odds"), "{en}");

        let ru = scenario_line(&sc, true);
        assert!(!ru.contains("deploy"), "russian line leaked english: {ru}");
        assert!(has_cyrillic(&ru), "russian line lost cyrillic: {ru}");
    }

    #[test]
    fn best_pick_prefers_oversold_over_overbought_at_equal_discount() {
        // Same -5% vs avg for both; the oversold (RSI 25) coin must win over the
        // overbought (RSI 78) one.
        let picks = vec![
            ("OVER".to_string(), -5.0, Some(78.0)),
            ("OVER2".to_string(), -5.0, Some(25.0)),
        ];
        let (name, _, _) = best_pick(&picks).unwrap();
        assert_eq!(name, "OVER2");
    }

    /// A representative report so the full table (header + Σ + scenario band)
    /// can be rendered and inspected in both languages without any network.
    fn sample_report() -> DipSniperReport {
        use crate::trading::sniper::{CapBand, Capitulation};
        use crate::trading::sniper::SniperBucket;
        let bucket = |index, limit_price: f64, discount_pct: f64, prob_touch: f64| SniperBucket {
            index,
            label: "bucket",
            quantile: prob_touch,
            sigma_mult: -1.0,
            limit_price,
            discount_pct,
            weight: 0.3,
            spend_usd: 100.0,
            prob_touch,
            new_avg_entry: None,
            dilutes_avg: false,
            triggered: index == 1,
        };
        DipSniperReport {
            symbol: "BTC".to_string(),
            price: 60_000.0,
            regime: "SIDEWAYS".to_string(),
            rsi14: Some(41.0),
            vol_daily_pct: 3.2,
            drift_daily_pct: 0.0,
            anchor_price: 63_000.0,
            z14: -1.1,
            avg_entry: Some(58_000.0),
            position_size: 0.05,
            cash: 1_000.0,
            protect_avg_entry: false,
            guard_active: false,
            capitulation: Capitulation {
                score: 2,
                max_score: 6,
                band: CapBand::Watch,
                z_hit: false,
                rsi_hit: true,
                volume_spike: false,
                long_wick: false,
                oi_flush: false,
            },
            derivatives: None,
            buckets: vec![
                bucket(1, 57_000.0, 5.0, 0.55),
                bucket(2, 54_000.0, 10.0, 0.32),
                bucket(3, 51_000.0, 15.0, 0.18),
            ],
            history_days: 200,
        }
    }

    #[test]
    fn full_render_is_clean_in_each_language() {
        let reports = vec![(
            "BTC".to_string(),
            Some(58_000.0),
            0.05,
            Some(sample_report()),
        )];

        let en = render_ladder(&reports, 300.0, 300.0, false);
        println!("\n===== ENGLISH =====\n{en}\n");
        assert!(!has_cyrillic(&en), "english render leaked cyrillic:\n{en}");
        assert!(en.contains("Buy Ladder"), "{en}");
        assert!(en.contains("odds"), "{en}");
        assert!(en.contains("deploy") || en.contains("no fill"), "{en}");
        assert!(en.contains("P14") && en.contains("limit") && en.contains("disc"), "{en}");

        let ru = render_ladder(&reports, 300.0, 300.0, true);
        println!("\n===== RUSSIAN =====\n{ru}\n");
        assert!(has_cyrillic(&ru), "russian render lost cyrillic:\n{ru}");
        assert!(ru.contains("Лестница") || ru.contains("лестница"), "{ru}");
        for leak in ["odds", "deploy", "limit", "disc", "spot", "Buy Ladder"] {
            assert!(!ru.contains(leak), "russian render leaked `{leak}`:\n{ru}");
        }
        // Correct Russian plural, never "3 ордеров".
        assert!(ru.contains("ордер") && !ru.contains("ордеров"), "{ru}");
    }

    #[test]
    fn orders_word_ru_uses_correct_plural() {
        assert_eq!(orders_word_ru(1), "ордер");
        assert_eq!(orders_word_ru(3), "ордера");
        assert_eq!(orders_word_ru(4), "ордера");
        assert_eq!(orders_word_ru(5), "ордеров");
        assert_eq!(orders_word_ru(11), "ордеров");
        assert_eq!(orders_word_ru(21), "ордер");
        assert_eq!(orders_word_ru(22), "ордера");
    }
}
