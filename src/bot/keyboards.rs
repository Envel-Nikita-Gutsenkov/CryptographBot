#![allow(dead_code)]
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

// ── Callback data constants ───────────────────────────────────────────────────

pub mod cb {
    pub const PRICE: &str = "price";
    pub const TOP10: &str = "top:10";
    pub const TOP20: &str = "top:20";
    pub const TOP50: &str = "top:50";
    pub const PORTFOLIO: &str = "portfolio";
    pub const PORTFOLIO_CHART: &str = "portfolio:chart";
    pub const PURCHASE_HISTORY: &str = "portfolio:history";
    pub const AI_PORTFOLIO: &str = "ai:portfolio";
    pub const PNL: &str = "pnl";
    pub const PNL_CHART: &str = "pnl:chart";
    pub const DISTRIBUTION: &str = "distribution:chart";
    pub const ALERTS: &str = "alerts";
    pub const ALERT_ADD: &str = "alert:add";
    pub const BACKUP: &str = "backup";
    pub const WALLETS: &str = "wallets";
    pub const WALLET_ADD: &str = "wallet:add";
    pub const SETTINGS: &str = "settings";
    pub const WATCHLIST: &str = "watchlist";
    pub const CONVERT: &str = "convert";
    pub const TRANSFERS: &str = "transfers";
    pub const REFRESH: &str = "refresh";
    pub const CANCEL: &str = "cancel";
    pub const PLAN: &str = "plan";
    pub const PLAN_GOALS: &str = "plan:goals";
    pub const PLAN_MEMOS: &str = "plan:memos";
    pub const PLAN_STATUS: &str = "plan:status";
    pub const PLAN_CHECK: &str = "plan:check";
    pub const PLAN_HELP: &str = "plan:help";

    pub fn alert_del(id: u64) -> String {
        format!("alert:del:{id}")
    }
    pub fn alert_toggle(id: u64) -> String {
        format!("alert:toggle:{id}")
    }
    pub fn wallet_del(id: u64) -> String {
        format!("wallet:del:{id}")
    }
    pub fn wallet_refresh(id: u64) -> String {
        format!("wallet:refresh:{id}")
    }
    pub fn purchase_correct(id: u64) -> String {
        format!("purchase:correct:{id}")
    }
    pub fn purchase_del(id: u64) -> String {
        format!("purchase:del:{id}")
    }
    pub fn holding_correct(wallet_id: u64, sym: &str) -> String {
        format!("holding:correct:{wallet_id}:{sym}")
    }
    pub fn transfer_from(wallet_id: u64) -> String {
        format!("transfer:from:{wallet_id}")
    }
    pub fn transfer_to(wallet_id: u64) -> String {
        format!("transfer:to:{wallet_id}")
    }
    pub fn confirm(token: &str) -> String {
        format!("confirm:{token}")
    }

    pub fn goal_confirm(id: u64) -> String {
        format!("goal:confirm:{id}")
    }
    pub fn goal_keep(id: u64) -> String {
        format!("goal:keep:{id}")
    }

    pub fn patch_confirm(token: &str) -> String {
        format!("patch:confirm:{token}")
    }
    pub fn patch_cancel(token: &str) -> String {
        format!("patch:cancel:{token}")
    }
}

// ── Main menu ─────────────────────────────────────────────────────────────────

pub fn main_menu() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        row2("💵 Prices", cb::PRICE, "📊 Top Movers", cb::TOP10),
        row2("💼 Portfolio", cb::PORTFOLIO, "📈 P&L", cb::PNL),
        row2(
            "🥧 Allocation",
            cb::DISTRIBUTION,
            "📋 Transfers",
            cb::TRANSFERS,
        ),
        row2("🔔 Alerts", cb::ALERTS, "👛 Wallets", cb::WALLETS),
        row2("💱 Convert", cb::CONVERT, "⚙️ Settings", cb::SETTINGS),
        row2("🧠 AI Analysis", "menu:ai", "🎯 Plan", cb::PLAN),
        row2("📰 News", "menu:news", "📊 Stats", "setting:stats"),
        row1("💾 Backup Now", cb::BACKUP),
    ])
}

// ── Plan dashboard sub-menu ───────────────────────────────────────────────────

pub fn plan_menu() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        row2("🎯 Goals", cb::PLAN_GOALS, "🧠 Memos", cb::PLAN_MEMOS),
        row2(
            "📊 Plan Status",
            cb::PLAN_STATUS,
            "🤖 AI Check",
            cb::PLAN_CHECK,
        ),
        row1("❓ Help", cb::PLAN_HELP),
        row1("🏠 Main Menu", "menu"),
    ])
}

// ── Portfolio sub-menu ────────────────────────────────────────────────────────

pub fn portfolio_menu() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        row2(
            "📊 Overview Chart",
            cb::PORTFOLIO_CHART,
            "📈 P&L Chart",
            cb::PNL_CHART,
        ),
        row2(
            "🥧 Allocation",
            cb::DISTRIBUTION,
            "📜 Purchase History",
            cb::PURCHASE_HISTORY,
        ),
        row2(
            "🧠 AI Portfolio Advice",
            cb::AI_PORTFOLIO,
            "🔄 Refresh",
            cb::REFRESH,
        ),
        row1("🏠 Main Menu", "menu"),
    ])
}

// ── Top list sub-menu ─────────────────────────────────────────────────────────

pub fn top_menu() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        row3(
            "Top 10",
            cb::TOP10,
            "Top 20",
            cb::TOP20,
            "Top 50",
            cb::TOP50,
        ),
        row1("📈 Show Top Movers Chart", "top:chart"),
        row1("🏠 Main Menu", "menu"),
    ])
}

// ── Alert list actions ────────────────────────────────────────────────────────

pub fn alert_actions(alert_id: u64, enabled: bool) -> InlineKeyboardMarkup {
    let toggle_label = if enabled {
        "⏸ Pause"
    } else {
        "▶️ Enable"
    };
    InlineKeyboardMarkup::new([vec![
        btn(toggle_label, &cb::alert_toggle(alert_id)),
        btn("🗑 Delete", &cb::alert_del(alert_id)),
    ]])
}

pub fn alerts_footer() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        row2("➕ Add Alert", cb::ALERT_ADD, "🔄 Refresh", cb::ALERTS),
        row1("🏠 Main Menu", "menu"),
    ])
}

// ── Wallet list actions ───────────────────────────────────────────────────────

pub fn wallet_actions(wallet_id: u64, monitored: bool) -> InlineKeyboardMarkup {
    let mon_label = if monitored {
        "👁 Monitoring ON"
    } else {
        "👁 Monitoring OFF"
    };
    InlineKeyboardMarkup::new([
        vec![
            btn("🔄 Refresh Balance", &cb::wallet_refresh(wallet_id)),
            btn(mon_label, &format!("wallet:toggle:{wallet_id}")),
        ],
        vec![
            btn("🔀 Transfer From", &cb::transfer_from(wallet_id)),
            btn("🗑 Remove", &cb::wallet_del(wallet_id)),
        ],
        row1("« Back", cb::WALLETS),
    ])
}

pub fn wallets_footer() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        row2(
            "➕ Add Wallet",
            cb::WALLET_ADD,
            "🔄 Refresh All",
            cb::WALLETS,
        ),
        row1("🏠 Main Menu", "menu"),
    ])
}

// ── Purchase actions ──────────────────────────────────────────────────────────

pub fn purchase_actions(purchase_id: u64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        vec![
            btn("✏️ Correct", &cb::purchase_correct(purchase_id)),
            btn("🗑 Delete", &cb::purchase_del(purchase_id)),
        ],
        row1("« Back", cb::PORTFOLIO),
    ])
}

// ── Alert condition picker ────────────────────────────────────────────────────

pub fn alert_condition_picker() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        row2(
            "📉 Price Below",
            "alert:cond:below",
            "📈 Price Above",
            "alert:cond:above",
        ),
        row2(
            "⬇️ Drop % over period",
            "alert:cond:drop",
            "⬆️ Rise % over period",
            "alert:cond:rise",
        ),
        row1("❌ Cancel", cb::CANCEL),
    ])
}

pub fn alert_period_picker() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        row3(
            "1h",
            "alert:period:1h",
            "4h",
            "alert:period:4h",
            "24h",
            "alert:period:24h",
        ),
        row3(
            "7d",
            "alert:period:7d",
            "30d",
            "alert:period:30d",
            "90d",
            "alert:period:90d",
        ),
        row1("❌ Cancel", cb::CANCEL),
    ])
}

// ── Wallet kind picker ────────────────────────────────────────────────────────

pub fn wallet_kind_picker() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        row3(
            "🧊 Cold",
            "wallet:kind:cold",
            "🔥 Hot",
            "wallet:kind:hot",
            "🏦 Exchange",
            "wallet:kind:exchange",
        ),
        row1("❌ Cancel", cb::CANCEL),
    ])
}

// ── Confirm / Cancel ──────────────────────────────────────────────────────────

pub fn confirm_cancel(confirm_token: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([vec![
        btn("✅ Confirm", &cb::confirm(confirm_token)),
        btn("❌ Cancel", cb::CANCEL),
    ]])
}

/// Keyboard for an AI proposal to reconsider a goal: the user either confirms
/// deactivation or keeps the goal active. The AI can never deactivate a goal
/// without the user pressing Confirm.
pub fn goal_propose_keyboard(goal_id: u64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([vec![
        btn("✅ Confirm (deactivate)", &cb::goal_confirm(goal_id)),
        btn("🔒 Keep active", &cb::goal_keep(goal_id)),
    ]])
}

pub fn cancel_only() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([row1("❌ Cancel", cb::CANCEL)])
}

pub fn chart_period_menu(symbol: &str, active_period: &str) -> InlineKeyboardMarkup {
    let p1d = if active_period == "1d" {
        "• 1D •"
    } else {
        "1D"
    };
    let p7d = if active_period == "7d" {
        "• 7D •"
    } else {
        "7D"
    };
    let p30d = if active_period == "30d" {
        "• 30D •"
    } else {
        "30D"
    };
    let p90d = if active_period == "90d" {
        "• 90D •"
    } else {
        "90D"
    };
    let p180d = if active_period == "180d" {
        "• 180D •"
    } else {
        "180D"
    };
    let p365d = if active_period == "365d" {
        "• 1Y •"
    } else {
        "1Y"
    };

    InlineKeyboardMarkup::new([
        vec![
            btn(p1d, &format!("chart:{symbol}:1d")),
            btn(p7d, &format!("chart:{symbol}:7d")),
            btn(p30d, &format!("chart:{symbol}:30d")),
            btn(p90d, &format!("chart:{symbol}:90d")),
            btn(p180d, &format!("chart:{symbol}:180d")),
            btn(p365d, &format!("chart:{symbol}:365d")),
        ],
        vec![btn(
            "🧠 AI Analysis",
            &format!("ai_analysis:{symbol}:{active_period}"),
        )],
        row1("🏠 Main Menu", "menu"),
    ])
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn btn(text: &str, data: &str) -> InlineKeyboardButton {
    InlineKeyboardButton::callback(text, data)
}

fn row1(t1: &str, d1: &str) -> Vec<InlineKeyboardButton> {
    vec![btn(t1, d1)]
}

fn row2(t1: &str, d1: &str, t2: &str, d2: &str) -> Vec<InlineKeyboardButton> {
    vec![btn(t1, d1), btn(t2, d2)]
}

fn row3(t1: &str, d1: &str, t2: &str, d2: &str, t3: &str, d3: &str) -> Vec<InlineKeyboardButton> {
    vec![btn(t1, d1), btn(t2, d2), btn(t3, d3)]
}

pub fn ai_coin_picker() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        row3(
            "BTC",
            "ai_select:BTC",
            "ETH",
            "ai_select:ETH",
            "SOL",
            "ai_select:SOL",
        ),
        row3(
            "BNB",
            "ai_select:BNB",
            "XRP",
            "ai_select:XRP",
            "ADA",
            "ai_select:ADA",
        ),
        row1("❌ Cancel", cb::CANCEL),
    ])
}

pub fn news_coin_picker() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([
        row3(
            "BTC",
            "news_select:BTC",
            "ETH",
            "news_select:ETH",
            "SOL",
            "news_select:SOL",
        ),
        row3(
            "BNB",
            "news_select:BNB",
            "XRP",
            "news_select:XRP",
            "General",
            "news_select:general",
        ),
        row1("🏠 Main Menu", "menu"),
    ])
}

pub fn patch_confirm_menu(token: &str) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([vec![
        btn("✅ Apply", &cb::patch_confirm(token)),
        btn("❌ Cancel", &cb::patch_cancel(token)),
    ]])
}
