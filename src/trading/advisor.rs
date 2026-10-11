//! Trade-task advisor: turns market data into bounded buy/sell advice.
//!
//! A "trade task" is a standing instruction with a monthly budget, a daily
//! alert cap and an execution style (manual today — the operator executes the
//! advice on their own wallet and reports the fill). The advisor is
//! deterministic: given the same task, fees, prices and history it produces
//! the same advice, so it can be backtested over real history and tuned by
//! grid search. The AI debate room (planned) sits between the advisor and the
//! operator, never inside it.
//!
//! Fees are asymmetric (buy-side / sell-side percentages) so recommendations
//! account for the real cost of the operator's exchange.

use anyhow::{bail, Result};
use chrono::NaiveDate;
use std::collections::{BTreeMap, HashMap};

use super::backtest::fetch_history_blocking;
use super::store::{
    ExchangeFeeConfig, LotStatus, RuleType, Side, TaskStatus, TradeTask, TradingStore,
};
use crate::crypto::ai::indicators;

/// 7-day drop (percent) that alone triggers a buy alert.
pub const DEFAULT_DIP_PCT: f64 = 5.0;
/// RSI at or below which a buy alert triggers.
pub const DEFAULT_RSI_OVERSOLD: f64 = 35.0;
/// Default tranche count for a task.
pub const DEFAULT_TASK_TRANCHES: u32 = 4;
/// Default minimum profit margin for a task.
pub const DEFAULT_TASK_MARGIN: f64 = 0.03;

/// Which buy signal the advisor listens for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuySignal {
    /// Any of the three signals below.
    Any,
    /// 7-day drop >= [`DEFAULT_DIP_PCT`].
    DipOnly,
    /// RSI <= [`DEFAULT_RSI_OVERSOLD`].
    RsiOnly,
    /// Price below the 20-day simple moving average.
    SmaOnly,
}

impl BuySignal {
    pub fn as_str(&self) -> &'static str {
        match self {
            BuySignal::Any => "any",
            BuySignal::DipOnly => "dip",
            BuySignal::RsiOnly => "rsi",
            BuySignal::SmaOnly => "sma",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "any" => Some(BuySignal::Any),
            "dip" | "drop" => Some(BuySignal::DipOnly),
            "rsi" => Some(BuySignal::RsiOnly),
            "sma" => Some(BuySignal::SmaOnly),
            _ => None,
        }
    }
}

/// One piece of advice produced by the advisor.
#[derive(Debug, Clone)]
pub struct TaskAdvice {
    pub symbol: String,
    pub side: Side,
    pub price: f64,
    pub amount: f64,
    /// Cash deployed (buy, fees included) or net proceeds (sell).
    pub value_usd: f64,
    pub target_exit_price: Option<f64>,
    pub rationale: String,
}

/// Buy-signal check over the recent close series.
pub fn buy_signal(price: f64, closes: &[f64], signal: BuySignal) -> bool {
    let dip = closes.len() >= 8 && {
        let week_ago = closes[closes.len() - 8];
        week_ago > 0.0 && (price - week_ago) / week_ago * 100.0 <= -DEFAULT_DIP_PCT
    };
    let rsi_ok = indicators::rsi(closes, 14)
        .map(|r| r <= DEFAULT_RSI_OVERSOLD)
        .unwrap_or(false);
    let sma_ok = indicators::sma(closes, 20)
        .map(|s| price < s)
        .unwrap_or(false);
    match signal {
        BuySignal::Any => dip || rsi_ok || sma_ok,
        BuySignal::DipOnly => dip,
        BuySignal::RsiOnly => rsi_ok,
        BuySignal::SmaOnly => sma_ok,
    }
}

/// Decide one action for `symbol` at `price`, given the recent close series.
/// Returns `Ok(None)` when the task is paused, the budget is exhausted, the
/// daily alert cap is hit, or no signal fires.
pub fn decide(
    task: &TradeTask,
    fees: &ExchangeFeeConfig,
    store: &TradingStore,
    symbol: &str,
    price: f64,
    closes: &[f64],
    signal: BuySignal,
) -> Result<Option<TaskAdvice>> {
    if task.status != TaskStatus::Active {
        return Ok(None);
    }
    if task.alerts_today >= task.max_alerts_per_day {
        return Ok(None);
    }
    let remaining = task.monthly_budget_usd - task.month_spend_usd;
    if remaining <= 0.0 {
        return Ok(None);
    }
    let buy_fee = fees.buy_fee_pct / 100.0;
    let sell_fee = fees.sell_fee_pct / 100.0;

    // Sell first: any open lot at or above its exit target?
    let lots = store.list_lots(None)?;
    for lot in lots
        .iter()
        .filter(|l| l.pair == symbol && l.status == LotStatus::Open)
    {
        if price >= lot.target_exit_price {
            let proceeds = lot.amount * price * (1.0 - sell_fee);
            return Ok(Some(TaskAdvice {
                symbol: symbol.to_string(),
                side: Side::Sell,
                price,
                amount: lot.amount,
                value_usd: proceeds,
                target_exit_price: Some(lot.target_exit_price),
                rationale: format!(
                    "price {price:.2} >= exit target {:.2} (net of {:.2}% sell fee)",
                    lot.target_exit_price, fees.sell_fee_pct
                ),
            }));
        }
    }

    // Ceiling constraint gates buys.
    let ceiling = store
        .list_constraints(Some(symbol), true)?
        .iter()
        .find(|c| c.rule_type == RuleType::MaxPriceCeiling)
        .map(|c| c.threshold_value);
    if let Some(cap) = ceiling {
        if price >= cap {
            return Ok(None);
        }
    }
    // Never average up: skip when an open lot was entered at or below price.
    if lots
        .iter()
        .any(|l| l.pair == symbol && l.status == LotStatus::Open && l.entry_price <= price)
    {
        return Ok(None);
    }
    if !buy_signal(price, closes, signal) {
        return Ok(None);
    }

    let tranche = (task.monthly_budget_usd / task.tranche_count as f64).min(remaining);
    let amount = tranche / price;
    let cost_basis = tranche * (1.0 + buy_fee);
    let target_exit = price * (1.0 + buy_fee) * (1.0 + task.min_profit_margin) / (1.0 - sell_fee);
    Ok(Some(TaskAdvice {
        symbol: symbol.to_string(),
        side: Side::Buy,
        price,
        amount,
        value_usd: cost_basis,
        target_exit_price: Some(target_exit),
        rationale: format!(
            "buy-the-dip signal — tranche ${tranche:.2} of ${:.2}/mo budget, exit target {target_exit:.2} incl. {:.2}% buy + {:.2}% sell fees",
            task.monthly_budget_usd, fees.buy_fee_pct, fees.sell_fee_pct
        ),
    }))
}

// ---------------------------------------------------------------------------
// Simulation over real history
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TaskSimConfig {
    pub symbols: Vec<String>,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub monthly_budget_usd: f64,
    pub max_alerts_per_day: i64,
    pub tranche_count: u32,
    pub min_profit_margin: f64,
    pub buy_fee_pct: f64,
    pub sell_fee_pct: f64,
    pub signal: BuySignal,
    /// Optional global price ceiling applied to every symbol.
    pub ceiling: Option<f64>,
}

impl Default for TaskSimConfig {
    fn default() -> Self {
        Self {
            symbols: vec!["BTC".into(), "ETH".into(), "SOL".into(), "LINK".into()],
            from: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
            to: NaiveDate::from_ymd_opt(2026, 9, 5).unwrap(),
            monthly_budget_usd: 200.0,
            max_alerts_per_day: 1,
            tranche_count: DEFAULT_TASK_TRANCHES,
            min_profit_margin: DEFAULT_TASK_MARGIN,
            buy_fee_pct: 0.0,
            sell_fee_pct: 0.0,
            signal: BuySignal::Any,
            ceiling: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskSimResult {
    pub label: String,
    pub symbols: String,
    pub start_date: String,
    pub end_date: String,
    pub monthly_budget_usd: f64,
    pub buy_fee_pct: f64,
    pub sell_fee_pct: f64,
    pub tranche_count: u32,
    pub min_profit_margin: f64,
    pub max_alerts_per_day: i64,
    pub signal: String,
    pub total_buys: usize,
    pub total_sells: usize,
    pub realized_profit_usd: f64,
    pub unrealized_profit_usd: f64,
    pub fees_paid_usd: f64,
    pub open_lots: usize,
    pub total_deployed_usd: f64,
    pub win_rate: f64,
    pub total_pnl_pct: f64,
    pub score: f64,
}

struct Lot {
    entry: f64,
    amount: f64,
    cost: f64,
    target: f64,
    closed: bool,
}

fn ms_to_date_key(ms: i64) -> String {
    let secs = ms.div_euclid(86_400_000) * 86_400;
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .format("%Y-%m-%d")
        .to_string()
}

fn month_day_keys(date: &str) -> (String, String) {
    let month = date.chars().take(7).collect::<String>();
    (month, date.to_string())
}

/// Replay the task over real daily history with a SHARED monthly budget across
/// all symbols, one alert per day max. Executes fills in-memory (no store
/// writes) so a full grid sweep stays cheap.
pub fn simulate_task(
    cfg: &TaskSimConfig,
    histories: &HashMap<String, Vec<(i64, f64)>>,
) -> Result<TaskSimResult> {
    let mut days: BTreeMap<i64, HashMap<String, f64>> = BTreeMap::new();
    for (sym, h) in histories {
        for (ts, price) in h {
            days.entry(*ts).or_default().insert(sym.clone(), *price);
        }
    }
    if days.is_empty() {
        bail!("no history for task simulation");
    }

    let mut lots: HashMap<String, Vec<Lot>> = HashMap::new();
    let mut closes: HashMap<String, Vec<f64>> = HashMap::new();
    let mut month_spend = 0.0;
    let mut month_key = String::new();
    let mut alerts_today = 0i64;
    let mut day_key = String::new();
    let mut buys = 0usize;
    let mut sells = 0usize;
    let mut realized = 0.0;
    let mut unrealized = 0.0;
    let mut fees_paid = 0.0;
    let mut wins = 0usize;
    let mut losses = 0usize;
    let mut deployed = 0.0;

    let buy_fee = cfg.buy_fee_pct / 100.0;
    let sell_fee = cfg.sell_fee_pct / 100.0;

    for (ts, prices) in &days {
        let day = ms_to_date_key(*ts);
        let (mkey, dkey) = month_day_keys(&day);
        if mkey != month_key {
            month_key = mkey;
            month_spend = 0.0;
        }
        if dkey != day_key {
            day_key = dkey;
            alerts_today = 0;
        }
        if alerts_today >= cfg.max_alerts_per_day {
            continue;
        }
        for sym in &cfg.symbols {
            if let Some(price) = prices.get(sym) {
                closes.entry(sym.clone()).or_default().push(*price);
            }
        }

        // 1) Sell check across all symbols (one alert day).
        let mut acted = false;
        for sym in &cfg.symbols {
            let Some(price) = prices.get(sym) else {
                continue;
            };
            let sym_lots = lots.entry(sym.clone()).or_default();
            for lot in sym_lots.iter_mut().filter(|l| !l.closed) {
                if *price >= lot.target {
                    let proceeds = lot.amount * *price * (1.0 - sell_fee);
                    let profit = proceeds - lot.cost;
                    realized += profit;
                    fees_paid += lot.amount * *price * sell_fee;
                    if profit >= 0.0 {
                        wins += 1;
                    } else {
                        losses += 1;
                    }
                    lot.closed = true;
                    sells += 1;
                    acted = true;
                }
            }
            if acted {
                break;
            }
        }
        if acted {
            alerts_today += 1;
            continue;
        }

        // 2) Buy check: pick the strongest signal across symbols.
        let remaining = cfg.monthly_budget_usd - month_spend;
        if remaining <= 0.0 {
            continue;
        }
        let mut best: Option<(String, f64, f64)> = None; // (symbol, drop_pct, price)
        for sym in &cfg.symbols {
            let Some(price) = prices.get(sym) else {
                continue;
            };
            if let Some(cap) = cfg.ceiling {
                if *price >= cap {
                    continue;
                }
            }
            let sym_closes = closes.get(sym).map(|v| v.as_slice()).unwrap_or(&[]);
            if sym_closes.len() < 8 {
                continue;
            }
            let has_open = lots
                .get(sym)
                .map(|ls| ls.iter().any(|l| !l.closed && l.entry <= *price))
                .unwrap_or(false);
            if has_open {
                continue;
            }
            if !buy_signal(*price, sym_closes, cfg.signal) {
                continue;
            }
            let week_ago = sym_closes[sym_closes.len() - 8];
            let drop = if week_ago > 0.0 {
                (*price - week_ago) / week_ago * 100.0
            } else {
                0.0
            };
            if best.as_ref().map(|(_, d, _)| drop < *d).unwrap_or(true) {
                best = Some((sym.clone(), drop, *price));
            }
        }
        if let Some((sym, _drop, price)) = best {
            let tranche = (cfg.monthly_budget_usd / cfg.tranche_count as f64).min(remaining);
            let amount = tranche / price;
            let cost = tranche * (1.0 + buy_fee);
            let target = price * (1.0 + buy_fee) * (1.0 + cfg.min_profit_margin) / (1.0 - sell_fee);
            lots.entry(sym).or_default().push(Lot {
                entry: price,
                amount,
                cost,
                target,
                closed: false,
            });
            month_spend += cost;
            deployed += cost;
            fees_paid += cost - tranche;
            buys += 1;
            alerts_today += 1;
        }
    }

    // Mark-to-market at the last day.
    let last_day = days.keys().next_back().copied().unwrap_or(0);
    let last_prices = days.get(&last_day).cloned().unwrap_or_default();
    let mut open_lots = 0usize;
    for (sym, ls) in &lots {
        for lot in ls.iter().filter(|l| !l.closed) {
            open_lots += 1;
            if let Some(price) = last_prices.get(sym) {
                let value = lot.amount * *price * (1.0 - sell_fee);
                unrealized += value - lot.cost;
            }
        }
    }

    let realized_pnl_pct = if deployed > 0.0 {
        realized / deployed * 100.0
    } else {
        0.0
    };
    let unrealized_pnl_pct = if deployed > 0.0 {
        unrealized / deployed * 100.0
    } else {
        0.0
    };
    let total_pnl_pct = realized_pnl_pct + unrealized_pnl_pct;
    let open_ratio = if buys > 0 {
        open_lots as f64 / buys as f64 * 100.0
    } else {
        0.0
    };
    // Reward realized profit hardest, count unrealized half, penalize capital
    // stuck in open lots.
    let score = realized_pnl_pct + 0.5 * unrealized_pnl_pct - 5.0 * open_ratio;
    let win_rate = if wins + losses > 0 {
        wins as f64 / (wins + losses) as f64 * 100.0
    } else {
        0.0
    };

    Ok(TaskSimResult {
        label: format!(
            "t{} m{:.1}% b{:.1}% s{:.1}% sig={}",
            cfg.tranche_count,
            cfg.min_profit_margin * 100.0,
            cfg.buy_fee_pct,
            cfg.sell_fee_pct,
            cfg.signal.as_str()
        ),
        symbols: cfg.symbols.join(","),
        start_date: cfg.from.to_string(),
        end_date: cfg.to.to_string(),
        monthly_budget_usd: cfg.monthly_budget_usd,
        buy_fee_pct: cfg.buy_fee_pct,
        sell_fee_pct: cfg.sell_fee_pct,
        tranche_count: cfg.tranche_count,
        min_profit_margin: cfg.min_profit_margin,
        max_alerts_per_day: cfg.max_alerts_per_day,
        signal: cfg.signal.as_str().to_string(),
        total_buys: buys,
        total_sells: sells,
        realized_profit_usd: realized,
        unrealized_profit_usd: unrealized,
        fees_paid_usd: fees_paid,
        open_lots,
        total_deployed_usd: deployed,
        win_rate,
        total_pnl_pct,
        score,
    })
}

// ---------------------------------------------------------------------------
// Grid sweep
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TaskGridConfig {
    pub symbols: Vec<String>,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub monthly_budget_usd: f64,
    pub max_alerts_per_day: i64,
    pub tranche_counts: Vec<u32>,
    pub margins: Vec<f64>,
    pub buy_fees: Vec<f64>,
    pub sell_fees: Vec<f64>,
    pub signals: Vec<BuySignal>,
    pub ceiling: Option<f64>,
}

/// Sweep the full parameter grid over real history (one history fetch per
/// symbol, replayed for every combo). Pure in-memory math — fast enough to run
/// hundreds of combos locally.
pub fn simulate_grid(cfg: &TaskGridConfig) -> Result<Vec<TaskSimResult>> {
    let mut histories = HashMap::new();
    for sym in &cfg.symbols {
        let h = fetch_history_blocking(sym, cfg.from, cfg.to)?;
        if h.is_empty() {
            bail!("no history for {sym}");
        }
        histories.insert(sym.clone(), h);
    }
    let mut results = Vec::new();
    for &tranches in &cfg.tranche_counts {
        for &margin in &cfg.margins {
            for &bf in &cfg.buy_fees {
                for &sf in &cfg.sell_fees {
                    for &sig in &cfg.signals {
                        let sim_cfg = TaskSimConfig {
                            symbols: cfg.symbols.clone(),
                            from: cfg.from,
                            to: cfg.to,
                            monthly_budget_usd: cfg.monthly_budget_usd,
                            max_alerts_per_day: cfg.max_alerts_per_day,
                            tranche_count: tranches,
                            min_profit_margin: margin,
                            buy_fee_pct: bf,
                            sell_fee_pct: sf,
                            signal: sig,
                            ceiling: cfg.ceiling,
                        };
                        results.push(simulate_task(&sim_cfg, &histories)?);
                    }
                }
            }
        }
    }
    Ok(results)
}

pub fn report(r: &TaskSimResult) -> String {
    let deployed = r.total_deployed_usd.max(1e-9);
    format!(
        "=== Task sim: {} {} → {} (budget ${:.0}/mo, {} alert/day) ===\n\
         Config: {} tranches, {:.1}% margin, {:.1}% buy / {:.1}% sell fees, signal={}\n\
         Buys: {} | Sells: {} | Open lots: {} | Win rate: {:.0}%\n\
         Deployed: ${:.2} | Fees paid: ${:.2}\n\
         Realized PnL: ${:.2} ({:.1}%) | Unrealized: ${:.2} ({:.1}%) | Total: {:.1}%\n\
         Score: {:.1}\n",
        r.symbols,
        r.start_date,
        r.end_date,
        r.monthly_budget_usd,
        r.max_alerts_per_day,
        r.tranche_count,
        r.min_profit_margin * 100.0,
        r.buy_fee_pct,
        r.sell_fee_pct,
        r.signal,
        r.total_buys,
        r.total_sells,
        r.open_lots,
        r.win_rate,
        r.total_deployed_usd,
        r.fees_paid_usd,
        r.realized_profit_usd,
        r.realized_profit_usd / deployed * 100.0,
        r.unrealized_profit_usd,
        r.unrealized_profit_usd / deployed * 100.0,
        r.total_pnl_pct,
        r.score
    )
}

pub fn grid_summary(results: &[TaskSimResult]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "=== Task grid: {} runs (sorted by score) ===\n",
        results.len()
    ));
    s.push_str(&format!(
        "{:<30} {:>5} {:>5} {:>5} {:>8} {:>8} {:>8} {:>7} {:>7}\n",
        "CONFIG", "BUYS", "SELLS", "OPEN", "REAL$", "UNREAL$", "FEES$", "PNL%", "SCORE"
    ));
    let mut sorted = results.to_vec();
    sorted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for r in sorted.iter().take(25) {
        s.push_str(&format!(
            "{:<30} {:>5} {:>5} {:>5} {:>8.2} {:>8.2} {:>8.2} {:>7.1} {:>7.1}\n",
            r.label,
            r.total_buys,
            r.total_sells,
            r.open_lots,
            r.realized_profit_usd,
            r.unrealized_profit_usd,
            r.fees_paid_usd,
            r.total_pnl_pct,
            r.score
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::store::{
        NewConstraint, NewLot, NewTradeTask, RuleType, TaskExecution, TradingMode,
    };

    fn declining_closes(n: usize, from: f64, to: f64) -> Vec<f64> {
        (0..n)
            .map(|i| from + (to - from) * i as f64 / n as f64)
            .collect()
    }

    fn active_task(store: &TradingStore) -> TradeTask {
        store
            .create_task(NewTradeTask {
                name: "test".into(),
                assets: "BTC".into(),
                execution: TaskExecution::Manual,
                monthly_budget_usd: 200.0,
                max_alerts_per_day: 1,
                tranche_count: 4,
                min_profit_margin: 0.03,
            })
            .unwrap()
    }

    #[test]
    fn buy_signal_dip_fires() {
        // 25 flat days at 100, then a sharp 5-day slide to 90: the 7-day
        // drop is ~10%.
        let mut closes = vec![100.0; 25];
        closes.extend(declining_closes(5, 100.0, 90.0));
        assert!(buy_signal(90.0, &closes, BuySignal::DipOnly));
        assert!(buy_signal(90.0, &closes, BuySignal::Any));
    }

    #[test]
    fn buy_signal_short_series_is_safe() {
        let closes = vec![100.0, 99.0, 98.0];
        assert!(!buy_signal(97.0, &closes, BuySignal::Any));
        assert!(!buy_signal(97.0, &closes, BuySignal::DipOnly));
    }

    #[test]
    fn buy_signal_rsi_and_sma_fire() {
        // Monotonic decline: RSI(14) is pinned low and price sits below SMA(20).
        let closes = declining_closes(30, 100.0, 90.0);
        assert!(buy_signal(90.0, &closes, BuySignal::RsiOnly));
        assert!(buy_signal(90.0, &closes, BuySignal::SmaOnly));
    }

    #[test]
    fn decide_paused_task_returns_none() {
        let store = TradingStore::open_in_memory(TradingMode::Simulation).unwrap();
        let mut task = active_task(&store);
        task.status = TaskStatus::Paused;
        let fees = store.get_fee_config().unwrap();
        let closes = declining_closes(30, 100.0, 90.0);
        let out = decide(&task, &fees, &store, "BTC", 90.0, &closes, BuySignal::Any).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn decide_alert_cap_blocks() {
        let store = TradingStore::open_in_memory(TradingMode::Simulation).unwrap();
        let task = active_task(&store);
        store.bump_task_alert(&task.id, 0.0).unwrap(); // alerts_today = 1 = cap
        let task = store.get_task(&task.id).unwrap(); // re-read the bumped row
        let fees = store.get_fee_config().unwrap();
        let closes = declining_closes(30, 100.0, 90.0);
        let out = decide(&task, &fees, &store, "BTC", 90.0, &closes, BuySignal::Any).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn decide_budget_exhausted_returns_none() {
        let store = TradingStore::open_in_memory(TradingMode::Simulation).unwrap();
        let task = active_task(&store);
        store.bump_task_alert(&task.id, 200.0).unwrap(); // full month budget spent
        let task = store.get_task(&task.id).unwrap(); // re-read the bumped row
        let fees = store.get_fee_config().unwrap();
        let closes = declining_closes(30, 100.0, 90.0);
        let out = decide(&task, &fees, &store, "BTC", 90.0, &closes, BuySignal::Any).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn decide_ceiling_blocks_buy() {
        let store = TradingStore::open_in_memory(TradingMode::Simulation).unwrap();
        let task = active_task(&store);
        store
            .add_constraint(NewConstraint {
                asset: "BTC".into(),
                rule_type: RuleType::MaxPriceCeiling,
                threshold_value: 85.0,
                raw_prompt: "test".into(),
                expires_at: None,
            })
            .unwrap();
        let fees = store.get_fee_config().unwrap();
        let closes = declining_closes(30, 100.0, 90.0);
        let out = decide(&task, &fees, &store, "BTC", 90.0, &closes, BuySignal::Any).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn decide_buy_sizes_tranche_with_fees() {
        let store = TradingStore::open_in_memory(TradingMode::Simulation).unwrap();
        let task = active_task(&store);
        store.set_fee_config(5.0, 2.5).unwrap();
        let fees = store.get_fee_config().unwrap();
        let closes = declining_closes(30, 100.0, 90.0);
        let out = decide(&task, &fees, &store, "BTC", 90.0, &closes, BuySignal::Any)
            .unwrap()
            .expect("buy advice");
        assert_eq!(out.side, Side::Buy);
        // 200/4 = 50 tranche, cost basis includes 5% buy fee.
        assert!((out.value_usd - 52.5).abs() < 1e-9);
        // Exit target must cover buy fee + margin + sell fee.
        let expected = 90.0 * 1.05 * 1.03 / 0.975;
        assert!((out.target_exit_price.unwrap() - expected).abs() < 1e-9);
    }

    #[test]
    fn decide_no_averaging_up() {
        let store = TradingStore::open_in_memory(TradingMode::Simulation).unwrap();
        let task = active_task(&store);
        store
            .open_lot(NewLot {
                allocation_id: None,
                pair: "BTC".into(),
                entry_price: 85.0,
                amount: 0.5,
                fee_paid_usd: 0.0,
                target_exit_price: 95.0,
                cost_basis_usd: Some(42.5),
                peak_price: Some(85.0),
            })
            .unwrap();
        let fees = store.get_fee_config().unwrap();
        let closes = declining_closes(30, 100.0, 90.0);
        // Price 90 >= entry 85 -> averaging up would be refused.
        let out = decide(&task, &fees, &store, "BTC", 90.0, &closes, BuySignal::Any).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn decide_sells_at_target() {
        let store = TradingStore::open_in_memory(TradingMode::Simulation).unwrap();
        let task = active_task(&store);
        store
            .open_lot(NewLot {
                allocation_id: None,
                pair: "BTC".into(),
                entry_price: 80.0,
                amount: 0.5,
                fee_paid_usd: 0.0,
                target_exit_price: 90.0,
                cost_basis_usd: Some(40.0),
                peak_price: Some(80.0),
            })
            .unwrap();
        let fees = store.get_fee_config().unwrap();
        let closes = declining_closes(30, 100.0, 90.0);
        let out = decide(&task, &fees, &store, "BTC", 95.0, &closes, BuySignal::Any)
            .unwrap()
            .expect("sell advice");
        assert_eq!(out.side, Side::Sell);
        assert_eq!(out.amount, 0.5);
    }

    #[test]
    fn simulate_task_dip_then_recovery_realizes_profit() {
        // 60 daily candles: flat 100 (30d), dip to 80 (5d), recovery to 120.
        let mut histories = HashMap::new();
        let mut series = Vec::new();
        let base = 1_700_000_000_000i64; // arbitrary ms epoch
        for i in 0..30 {
            series.push((base + i * 86_400_000, 100.0));
        }
        for i in 0..5 {
            series.push((base + (30 + i) * 86_400_000, 80.0));
        }
        for i in 0..25 {
            series.push((base + (35 + i) * 86_400_000, 120.0));
        }
        histories.insert("BTC".to_string(), series);
        let cfg = TaskSimConfig {
            symbols: vec!["BTC".into()],
            monthly_budget_usd: 200.0,
            max_alerts_per_day: 1,
            tranche_count: 4,
            min_profit_margin: 0.03,
            buy_fee_pct: 0.0,
            sell_fee_pct: 0.0,
            signal: BuySignal::Any,
            ceiling: None,
            ..TaskSimConfig::default()
        };
        let r = simulate_task(&cfg, &histories).unwrap();
        assert!(
            r.total_buys >= 1,
            "expected at least one buy, got {}",
            r.total_buys
        );
        assert!(
            r.total_sells >= 1,
            "expected at least one sell, got {}",
            r.total_sells
        );
        assert!(r.realized_profit_usd > 0.0, "expected realized profit");
        assert!(r.total_pnl_pct > 0.0);
    }

    #[test]
    fn grid_summary_sorts_by_score() {
        let mk = |score: f64, label: &str| TaskSimResult {
            label: label.into(),
            symbols: "BTC".into(),
            start_date: "2020-01-01".into(),
            end_date: "2026-09-05".into(),
            monthly_budget_usd: 200.0,
            buy_fee_pct: 0.0,
            sell_fee_pct: 0.0,
            tranche_count: 4,
            min_profit_margin: 0.03,
            max_alerts_per_day: 1,
            signal: "any".into(),
            total_buys: 10,
            total_sells: 8,
            realized_profit_usd: 10.0,
            unrealized_profit_usd: 2.0,
            fees_paid_usd: 0.0,
            open_lots: 2,
            total_deployed_usd: 100.0,
            win_rate: 100.0,
            total_pnl_pct: 12.0,
            score,
        };
        let results = vec![mk(1.0, "low"), mk(9.0, "high"), mk(5.0, "mid")];
        let out = grid_summary(&results);
        let high = out.find("high").unwrap();
        let mid = out.find("mid").unwrap();
        let low = out.find("low").unwrap();
        assert!(
            high < mid && mid < low,
            "top-25 must be score-sorted: {out}"
        );
    }
}
