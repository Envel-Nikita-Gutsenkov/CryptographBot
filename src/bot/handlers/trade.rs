//! Telegram mirrors of the trading CLI: `/trade` (commander mode) and
//! `/tradetask` (standing advisor instructions). Admin-gated; operate on the
//! same sim/live DBs the CLI uses (sim by default, `--live` for the live DB).

use crate::config::Config;
use crate::trading::advisor::{decide, BuySignal};
use crate::trading::backtest::fetch_history_blocking;
use crate::trading::store::{
    LotStatus, NewAllocation, NewConstraint, NewTradeTask, RecommendationStatus, RuleType, Side,
    TaskExecution, TaskStatus, TradingMode, TradingStore,
};
use std::sync::Arc;
use teloxide::{prelude::*, types::ParseMode};

/// Open the trading DB off the tokio worker: `TradingStore::open` runs schema
/// init (a write lock) and can block on a file lock held by another process
/// (CLI training, `trade seed`) — on the async runtime that would stall the
/// whole bot, so it goes to the blocking pool.
async fn store_for(mode: TradingMode) -> anyhow::Result<TradingStore> {
    tokio::task::spawn_blocking(move || TradingStore::open(mode))
        .await
        .map_err(|e| anyhow::anyhow!("trading store task panicked: {e}"))?
}

fn mode_from(parts: &[&str]) -> TradingMode {
    if parts.iter().any(|p| *p == "--live") {
        TradingMode::Live
    } else {
        TradingMode::Simulation
    }
}

fn flag_str<'a>(parts: &'a [&str], name: &str) -> Option<&'a str> {
    parts
        .iter()
        .position(|p| *p == name)
        .and_then(|i| parts.get(i + 1))
        .copied()
}

fn flag_f64(parts: &[&str], name: &str) -> Option<f64> {
    flag_str(parts, name).and_then(|s| s.parse().ok())
}

fn flag_i64(parts: &[&str], name: &str) -> Option<i64> {
    flag_str(parts, name).and_then(|s| s.parse().ok())
}

fn flag_u32(parts: &[&str], name: &str) -> Option<u32> {
    flag_str(parts, name).and_then(|s| s.parse().ok())
}

// ---------------------------------------------------------------------------
// /trade — commander mode
// ---------------------------------------------------------------------------

#[allow(deprecated)]
pub async fn handle_trade_cmd(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }
    let parts: Vec<&str> = args.split_whitespace().collect();
    let mode = mode_from(&parts);
    let out = match parts.first().copied() {
        Some("list") => trade_list(&store_for(mode).await?, mode),
        Some("pnl") => trade_pnl(&store_for(mode).await?, mode),
        Some("fee") => trade_fee(&store_for(mode).await?, &parts, mode),
        Some("allocate") => trade_allocate(&store_for(mode).await?, &parts, mode),
        Some("deallocate") => trade_deallocate(&store_for(mode).await?, &parts, mode),
        Some("constraints") => trade_constraints(&store_for(mode).await?, mode),
        Some("rule") => trade_rule(&store_for(mode).await?, &parts, mode),
        _ => Ok(TRADE_USAGE.to_string()),
    };
    match out {
        Ok(text) => {
            bot.send_message(msg.chat.id, crate::format::esc(&text))
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("❌ {e}")).await?;
        }
    }
    Ok(())
}

const TRADE_USAGE: &str = "Usage:\n\
    /trade list — pending recommendations\n\
    /trade pnl — realized + unrealized PnL\n\
    /trade fee [--buy P] [--sell P] — show/set exchange fees\n\
    /trade allocate SYMBOL USD — create an allocation\n\
    /trade deallocate ID — delete an allocation\n\
    /trade constraints — active guardrails\n\
    /trade rule SYMBOL ceiling|floor|drawdown VALUE
    /trade rule del ID — delete a constraint\n\
    Append --live to operate on the live DB (default: sim).";

fn trade_list(store: &TradingStore, mode: TradingMode) -> anyhow::Result<String> {
    let recs = store.list_recommendations(20, Some(RecommendationStatus::Pending))?;
    if recs.is_empty() {
        return Ok(format!("No pending recommendations [{}].", mode.as_str()));
    }
    let mut s = format!("Pending recommendations [{}]:\n", mode.as_str());
    for r in recs {
        s.push_str(&format!(
            "• {} {} {} @ {:.4} (${:.2}) — {}\n",
            r.side.as_str(),
            r.symbol,
            r.amount,
            r.recommended_price,
            r.value_usd,
            r.rationale
        ));
    }
    Ok(s)
}

fn trade_pnl(store: &TradingStore, mode: TradingMode) -> anyhow::Result<String> {
    let executions = store.list_executions(10_000)?;
    let mut realized = 0.0f64;
    let mut fees = 0.0f64;
    let mut buy_usd = 0.0f64;
    for e in &executions {
        fees += e.fee_usd;
        match e.side {
            Side::Buy => buy_usd += e.net_usd,
            Side::Sell => realized += e.net_usd,
        }
    }
    let realized_pnl = realized - buy_usd;
    let lots = store.list_lots(None)?;
    let mut open_value = 0.0f64;
    let mut open_cost = 0.0f64;
    let mut open_lots = 0usize;
    let mut cold_lots = 0usize;
    let today = chrono::Local::now().date_naive();
    for lot in &lots {
        if lot.status == LotStatus::Closed {
            continue;
        }
        if lot.status == LotStatus::ColdHold {
            cold_lots += 1;
        } else {
            open_lots += 1;
        }
        let sym = lot.pair.split('/').next().unwrap_or(&lot.pair).to_string();
        let price = fetch_history_blocking(&sym, today - chrono::Duration::days(7), today)
            .ok()
            .and_then(|h| h.last().map(|(_, p)| *p))
            .unwrap_or(lot.entry_price);
        open_value += lot.amount * price;
        open_cost += lot.cost_basis_usd;
    }
    let unrealized = open_value - open_cost;
    let total = realized_pnl + unrealized;
    let deployed = buy_usd + open_cost;
    let pct = if deployed > 0.0 {
        total / deployed * 100.0
    } else {
        0.0
    };
    Ok(format!(
        "PnL report [{}]:\n  Realized: ${:.2}\n  Unrealized (mark-to-market): ${:.2}\n  Total: ${:.2} ({:.1}%)\n  Fees paid: ${:.2}\n  Open lots: {} | cold-hold: {} | deployed: ${:.2}",
        mode.as_str(),
        realized_pnl,
        unrealized,
        total,
        pct,
        fees,
        open_lots,
        cold_lots,
        deployed
    ))
}

fn trade_fee(store: &TradingStore, parts: &[&str], mode: TradingMode) -> anyhow::Result<String> {
    if flag_f64(parts, "--buy").is_some() || flag_f64(parts, "--sell").is_some() {
        let buy = flag_f64(parts, "--buy").unwrap_or(0.0);
        let sell = flag_f64(parts, "--sell").unwrap_or(0.0);
        let f = store.set_fee_config(buy, sell)?;
        Ok(format!(
            "Exchange fees set [{}]: buy {:.2}% | sell {:.2}%",
            mode.as_str(),
            f.buy_fee_pct,
            f.sell_fee_pct
        ))
    } else {
        let f = store.get_fee_config()?;
        Ok(format!(
            "Exchange fees [{}]: buy {:.2}% | sell {:.2}% (updated {})",
            mode.as_str(),
            f.buy_fee_pct,
            f.sell_fee_pct,
            f.updated_at
        ))
    }
}

fn trade_allocate(
    store: &TradingStore,
    parts: &[&str],
    mode: TradingMode,
) -> anyhow::Result<String> {
    if parts.len() < 3 {
        return Ok("Usage: /trade allocate SYMBOL USD".to_string());
    }
    let symbol = parts[1].to_uppercase();
    let usd: f64 = parts[2]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid USD amount '{}'", parts[2]))?;
    let a = store.create_allocation(NewAllocation {
        asset: symbol.clone(),
        total_budget_usd: usd,
    })?;
    Ok(format!(
        "Allocation created [{}]: {} pool of {:.2} USD (id: {})",
        mode.as_str(),
        symbol,
        usd,
        a.id
    ))
}

fn trade_deallocate(
    store: &TradingStore,
    parts: &[&str],
    mode: TradingMode,
) -> anyhow::Result<String> {
    if parts.len() < 2 {
        return Ok("Usage: /trade deallocate ID".to_string());
    }
    store.delete_allocation(parts[1])?;
    Ok(format!(
        "Allocation {} deleted [{}].",
        parts[1],
        mode.as_str()
    ))
}

fn trade_constraints(store: &TradingStore, mode: TradingMode) -> anyhow::Result<String> {
    let cs = store.list_constraints(None, false)?;
    if cs.is_empty() {
        return Ok(format!("No constraints [{}].", mode.as_str()));
    }
    let mut s = format!("Constraints [{}]:\n", mode.as_str());
    for c in cs {
        s.push_str(&format!(
            "• {} {} {} (active: {})\n",
            c.asset,
            c.rule_type.as_str(),
            c.threshold_value,
            c.is_active
        ));
    }
    Ok(s)
}

fn trade_rule(store: &TradingStore, parts: &[&str], mode: TradingMode) -> anyhow::Result<String> {
    if parts.get(1).map(|s| s.eq_ignore_ascii_case("del")) == Some(true) {
        let id = parts
            .get(2)
            .ok_or_else(|| anyhow::anyhow!("Usage: /trade rule del ID"))?;
        store.delete_constraint(id)?;
        return Ok(format!("Constraint {} deleted [{}].", id, mode.as_str()));
    }
    if parts.len() < 4 {
        return Ok("Usage: /trade rule SYMBOL ceiling|floor|drawdown VALUE".to_string());
    }
    let symbol = parts[1].to_uppercase();
    let rule_type = match parts[2].to_lowercase().as_str() {
        "ceiling" | "max" => RuleType::MaxPriceCeiling,
        "floor" | "min" => RuleType::MinPriceFloor,
        "drawdown" => RuleType::MaxDrawdownStep,
        "pause-buy" => RuleType::PauseBuy,
        "pause-sell" => RuleType::PauseSell,
        other => anyhow::bail!("unknown rule type '{other}'"),
    };
    let value: f64 = parts[3]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid threshold '{}'", parts[3]))?;
    let c = store.add_constraint(NewConstraint {
        asset: symbol.clone(),
        rule_type,
        threshold_value: value,
        raw_prompt: format!("TG: {} {} {}", symbol, parts[2], value),
        expires_at: None,
    })?;
    Ok(format!(
        "Constraint added [{}]: {} {} {} (id: {})",
        mode.as_str(),
        symbol,
        rule_type.as_str(),
        value,
        c.id
    ))
}

// ---------------------------------------------------------------------------
// /tradetask — standing advisor instructions
// ---------------------------------------------------------------------------

#[allow(deprecated)]
pub async fn handle_tradetask_cmd(
    bot: Bot,
    msg: Message,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }
    let parts: Vec<&str> = args.split_whitespace().collect();
    let mode = mode_from(&parts);
    let out = match parts.first().copied() {
        Some("list") => task_list(&store_for(mode).await?, mode),
        Some("add") => task_add(&store_for(mode).await?, &parts, mode),
        Some("pause") => task_set_status(&store_for(mode).await?, &parts, TaskStatus::Paused, mode),
        Some("resume") => {
            task_set_status(&store_for(mode).await?, &parts, TaskStatus::Active, mode)
        }
        Some("del") | Some("delete") => task_del(&store_for(mode).await?, &parts, mode),
        Some("advise") => task_advise(&store_for(mode).await?, &parts, mode).await,
        _ => Ok(TASKTASK_USAGE.to_string()),
    };
    match out {
        Ok(text) => {
            bot.send_message(msg.chat.id, crate::format::esc(&text))
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("❌ {e}")).await?;
        }
    }
    Ok(())
}

const TASKTASK_USAGE: &str = "Usage:\n\
    /tradetask list — standing tasks\n\
    /tradetask add --name NAME [--assets BTC,ETH] [--budget 200] [--tranches 4] [--margin 0.03] [--alerts 1]\n\
    /tradetask pause NAME | resume NAME
    /tradetask del NAME|ID — delete a task
    /tradetask advise — run the advisor for active tasks\n\
    Append --live to operate on the live DB (default: sim).";

fn task_list(store: &TradingStore, mode: TradingMode) -> anyhow::Result<String> {
    let tasks = store.list_tasks()?;
    if tasks.is_empty() {
        return Ok(format!("No trade tasks [{}].", mode.as_str()));
    }
    let mut s = format!("Trade tasks [{}]:\n", mode.as_str());
    for t in tasks {
        s.push_str(&format!(
            "• {} [{}] — {} | budget ${:.0}/mo | {} tranches | {:.1}% margin | {} alert/day | spent ${:.2} this month\n",
            t.name,
            t.status.as_str(),
            t.assets,
            t.monthly_budget_usd,
            t.tranche_count,
            t.min_profit_margin * 100.0,
            t.max_alerts_per_day,
            t.month_spend_usd
        ));
    }
    Ok(s)
}

fn task_add(store: &TradingStore, parts: &[&str], mode: TradingMode) -> anyhow::Result<String> {
    let name = flag_str(parts, "--name")
        .ok_or_else(|| anyhow::anyhow!("--name is required"))?
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string();
    if name.is_empty() {
        anyhow::bail!("task name must not be empty");
    }
    let assets = flag_str(parts, "--assets")
        .unwrap_or("BTC,ETH,SOL,LINK")
        .to_uppercase();
    let t = store.create_task(NewTradeTask {
        name: name.to_string(),
        assets,
        execution: TaskExecution::Manual,
        monthly_budget_usd: flag_f64(parts, "--budget").unwrap_or(200.0),
        max_alerts_per_day: flag_i64(parts, "--alerts").unwrap_or(1),
        tranche_count: flag_u32(parts, "--tranches")
            .unwrap_or(crate::trading::advisor::DEFAULT_TASK_TRANCHES),
        min_profit_margin: flag_f64(parts, "--margin")
            .unwrap_or(crate::trading::advisor::DEFAULT_TASK_MARGIN),
    })?;
    Ok(format!(
        "Task created [{}]: {} (id: {})\n  assets: {} | budget ${:.0}/mo | {} tranches | {:.1}% margin",
        mode.as_str(),
        t.name,
        t.id,
        t.assets,
        t.monthly_budget_usd,
        t.tranche_count,
        t.min_profit_margin * 100.0
    ))
}

fn task_set_status(
    store: &TradingStore,
    parts: &[&str],
    status: TaskStatus,
    mode: TradingMode,
) -> anyhow::Result<String> {
    let name = parts
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("task name required"))?;
    let tasks = store.list_tasks()?;
    let task = tasks
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| anyhow::anyhow!("task '{name}' not found"))?;
    let t = store.set_task_status(&task.id, status)?;
    Ok(format!(
        "Task [{}] '{}' is now {}",
        mode.as_str(),
        t.name,
        t.status.as_str()
    ))
}

fn task_del(store: &TradingStore, parts: &[&str], mode: TradingMode) -> anyhow::Result<String> {
    let name = parts
        .get(1)
        .ok_or_else(|| anyhow::anyhow!("task name or id required"))?;
    store.delete_task(name)?;
    Ok(format!("Task '{}' deleted [{}].", name, mode.as_str()))
}

async fn task_advise(
    store: &TradingStore,
    _parts: &[&str],
    mode: TradingMode,
) -> anyhow::Result<String> {
    let tasks = store.list_tasks()?;
    let active: Vec<_> = tasks
        .into_iter()
        .filter(|t| t.status == TaskStatus::Active)
        .collect();
    if active.is_empty() {
        return Ok(format!("No active tasks [{}].", mode.as_str()));
    }
    let fees = store.get_fee_config()?;
    let today = chrono::Local::now().date_naive();
    let mut s = format!("Advisor run [{}]:\n", mode.as_str());
    for task in &active {
        for sym in task
            .assets
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            let history = fetch_history_blocking(sym, today - chrono::Duration::days(30), today)?;
            let price = history.last().map(|(_, p)| *p).unwrap_or(0.0);
            let closes: Vec<f64> = history.iter().map(|(_, p)| *p).collect();
            match decide(task, &fees, store, sym, price, &closes, BuySignal::Any)? {
                Some(adv) => {
                    s.push_str(&format!(
                        "• {} {} {} @ {:.4} (${:.2}) — {}\n",
                        adv.side.as_str(),
                        adv.symbol,
                        adv.amount,
                        adv.price,
                        adv.value_usd,
                        adv.rationale
                    ));
                }
                None => {
                    s.push_str(&format!("• {} — no action (price {:.4})\n", sym, price));
                }
            }
        }
    }
    Ok(s)
}
