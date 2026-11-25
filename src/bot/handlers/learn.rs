//! Interactive tutorial (`/learn`).
//!
//! A guided, button-driven tour of the whole bot. Each step explains one
//! functional area with a concrete example command, and the inline keyboard
//! lets the user walk forward/back without re-typing anything. It is kept
//! deliberately independent of the main menu (nothing is added there) — the
//! tour is its own little submenu reached only through `/learn` (or the
//! "🎓 Tutorial" button on `/help`).

use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode};

/// One page of the tour: a headline, a short "what it does" body, and a couple
/// of example commands the user can copy straight into the chat.
struct Step {
    title: &'static str,
    body: &'static str,
    examples: &'static [&'static str],
}

const STEPS: &[Step] = &[
    Step {
        title: "1/8 · Market & prices",
        body: "The fast layer: live quotes, long-range context and the biggest movers.\n\
               Everything here is cached and refreshed by background workers, so it is \
               near-real-time (WebSocket for the majors).",
        examples: &[
            "/price BTC ETH SOL",
            "/long BTC — 90/180/360-day change + where price sits in its range",
            "/top 10 — top coins by 24h volume",
            "/fng — Fear & Greed index",
        ],
    },
    Step {
        title: "2/8 · Portfolio & P&L",
        body: "Your holdings, cost basis and profit/loss. Record purchases (or move coins \
               between wallets) and the bot recomputes average entry, unrealized P&L and \
               allocation. Charts are generated on demand.",
        examples: &[
            "/add BTC 0.01 60000 exchange:binance — record a buy",
            "/portfolio — text overview",
            "/pnl — P&L chart",
            "/distribution — allocation pie",
            "/history BTC — purchase history",
        ],
    },
    Step {
        title: "3/8 · AI assistant",
        body: "The reasoning layer. `/ask` is a free-form chat grounded in YOUR portfolio, \
               the live market, news and the shared database. `/ai` gives a deep single-coin \
               analysis. `/memo` gives the AI long-term memory; plain-text replies continue \
               the conversation.",
        examples: &[
            "/ask should I top up ETH now?",
            "/ai BTC — deep analysis",
            "/memo invest $100 at a time — teach the AI your style",
            "/memo list",
        ],
    },
    Step {
        title: "4/8 · Forecast & quant levels",
        body: "The deterministic math layer (no AI, reproducible): a volatility cone per \
               horizon, dip zones with P(touch), the Sniper's left-tail limit buckets, the \
               Bottom radar and the Ladder planner. `/analyze` merges all of them into one \
               screen with a single Action line so the sub-commands never contradict.",
        examples: &[
            "/forecast BTC ETH — cone for 1/3/7/14/30/90/180d",
            "/dip BTC — dip zones + buy-the-dip verdict",
            "/bottom BTC — is this the bottom, should I buy?",
            "/ladder BTC ETH 500 — staged limit traps",
            "/analyze BTC ETH — one-screen plan",
            "/decide — decision board across all coins",
        ],
    },
    Step {
        title: "5/8 · Goals & proactive alerts",
        body: "Tell the bot WHAT you want in plain language and it watches the market for \
               you. When conditions fit it sends a smart alert (deduplicated, rate-limited) \
               and tracks progress. Price alerts are the sharp, rule-based sibling: \
               two consecutive confirming ticks, no single-wick false positives.",
        examples: &[
            "/goal want to spend $400 on crypto this month",
            "/goal list — see goals + last AI reasoning",
            "/plan check — ask the AI about your plan right now",
            "/alert add BTC < 60000 cooldown:6h",
        ],
    },
    Step {
        title: "6/8 · Trading modes",
        body: "Commander mode: monthly budget, dip%/ceiling rules, standing advisor tasks and \
               realized + mark-to-market PnL. The advisor respects your constraints before it \
               ever suggests a trade.",
        examples: &[
            "/trade list — budgets, constraints, rules, PnL",
            "/trade allocate BTC 500 — monthly budget",
            "/trade rule add dip 8 — buy on 8% dips",
            "/tradetask add --name dca --assets BTC ETH",
            "/tradetask advise — run the advisor now",
        ],
    },
    Step {
        title: "7/8 · News",
        body: "A watcher-style digest across many trusted feeds, ranked by relevance to your \
               holdings and watchlist. Per-symbol news is strict: it only shows headlines that \
               actually mention the coin.",
        examples: &[
            "/news — full digest",
            "/news BTC — BTC-only headlines",
        ],
    },
    Step {
        title: "8/8 · Settings & power features",
        body: "Tune the bot: AI model, notification style, quiet hours, threshold presets and \
               more — all through buttons, no syntax to memorise. Backup/restore keeps your \
               data portable.",
        examples: &[
            "/settings — interactive settings menu",
            "/model list → /model set N",
            "/backup — export JSON",
            "/help — full command reference",
        ],
    },
];

fn step_text(i: usize) -> String {
    let s = &STEPS[i];
    let mut out = format!("🎓 <b>{}</b>\n\n{}\n\n", s.title, s.body);
    out.push_str("<b>Try:</b>\n");
    for ex in s.examples {
        out.push_str(&format!("• <code>{}</code>\n", html(ex)));
    }
    out
}

fn html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn step_keyboard(i: usize) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    let mut nav: Vec<InlineKeyboardButton> = Vec::new();
    if i > 0 {
        nav.push(InlineKeyboardButton::callback(
            "◀ Back",
            format!("learn:step:{}", i - 1),
        ));
    }
    if i + 1 < STEPS.len() {
        nav.push(InlineKeyboardButton::callback(
            "Next ▶",
            format!("learn:step:{}", i + 1),
        ));
    }
    if !nav.is_empty() {
        rows.push(nav);
    }
    if i + 1 == STEPS.len() {
        rows.push(vec![InlineKeyboardButton::callback("🎉 Finish", "learn:done")]);
    }
    rows.push(vec![
        InlineKeyboardButton::callback("🎓 Restart", "learn:step:0"),
        InlineKeyboardButton::callback("🏠 Menu", "menu"),
    ]);
    InlineKeyboardMarkup::new(rows)
}

/// Intro screen shown by `/learn` (and by `learn:start`).
fn intro_text() -> String {
    "🎓 <b>Cryptograph — interactive tutorial</b>\n\n\
     A short guided tour, 8 screens, buttons only — no typing required. \
     Each screen explains one area and gives copy-ready example commands.\n\n\
     Tap <b>Next ▶</b> to begin."
        .to_string()
}

fn intro_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![
        vec![InlineKeyboardButton::callback("▶ Start tour", "learn:step:0")],
        vec![
            InlineKeyboardButton::callback("📚 Command reference", "learn:help"),
            InlineKeyboardButton::callback("🏠 Menu", "menu"),
        ],
    ])
}

pub async fn handle_learn_cmd(bot: Bot, msg: Message) -> anyhow::Result<()> {
    bot.send_message(msg.chat.id, intro_text())
        .parse_mode(ParseMode::Html)
        .reply_markup(intro_keyboard())
        .await?;
    Ok(())
}

/// Handle `learn:*` callbacks. Runs outside the dialogue state machine so it
/// also works from rich messages.
pub async fn handle_learn_callback(
    bot: Bot,
    q: CallbackQuery,
    _store: std::sync::Arc<crate::db::Store>,
) -> anyhow::Result<()> {
    let _ = bot.answer_callback_query(&q.id).await;
    let data = q.data.as_deref().unwrap_or("");
    let chat = match q.message.as_ref() {
        Some(m) => m.chat.id,
        None => return Ok(()),
    };

    let (text, kb) = match data {
        "learn:step:0" => (format!("{}\n\n{}", intro_text(), step_text(0)), step_keyboard(0)),
        "learn:help" => (
            "📚 Type <code>/help</code> for the full command reference — it is split into \
             market, portfolio, AI, trading and system sections."
                .to_string(),
            InlineKeyboardMarkup::new(vec![
                vec![InlineKeyboardButton::callback("▶ Resume tour", "learn:step:0")],
                vec![InlineKeyboardButton::callback("🏠 Menu", "menu")],
            ]),
        ),
        "learn:done" => (
            "🎉 <b>Tour complete!</b>\n\nYou now know the whole map. A good first step: \
             <code>/goal want to spend $N on crypto this month</code> so the bot starts \
             watching for you.\n\nType <code>/help</code> anytime."
                .to_string(),
            InlineKeyboardMarkup::new(vec![
                vec![
                    InlineKeyboardButton::callback("🎓 Restart", "learn:step:0"),
                    InlineKeyboardButton::callback("🏠 Menu", "menu"),
                ],
            ]),
        ),
        other => {
            let idx = other
                .strip_prefix("learn:step:")
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|i| *i < STEPS.len())
                .unwrap_or(0);
            (step_text(idx), step_keyboard(idx))
        }
    };

    // Edit in place when possible, otherwise send fresh.
    if let Some(m) = q.message.as_ref() {
        if bot
            .edit_message_text(chat, m.id, text.clone())
            .parse_mode(ParseMode::Html)
            .reply_markup(kb.clone())
            .await
            .is_ok()
        {
            return Ok(());
        }
    }
    bot.send_message(chat, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(kb)
        .await?;
    Ok(())
}
