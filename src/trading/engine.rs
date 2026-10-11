//! Deterministic trading engine (Phase 2 of the trading module plan).
//!
//! Pure decision logic + guarded execution. Everything here is deterministic:
//! given the same store state and market tick, the same decision is produced.
//! AI (the Bull/Bear/Arbiter debate room, Phase 4) sits BETWEEN `plan_*` and
//! `execute_*`, never inside the engine — so a model can never bypass the
//! algorithmic guardrails.
//!
//! Invariants enforced here (defense in depth on top of the store):
//!
//! * **No-loss exit** — a lot may only be sold when
//!   `amount * exit * (1 - fee_rate) >= cost_basis * (1 + min_profit_margin)`.
//!   Underwater lots transition to `COLD_HOLD` and are never liquidated at a
//!   loss; they become long-term DCA holdings until price recovers.
//! * **Never average up** — a buy is rejected while the price is at or above
//!   the last entry price of an existing lot for the same symbol.
//! * **Tranche sizing** — each buy deploys `total_budget / tranche_count` USD
//!   of the pool (default 3 × 33%), so a pool is exhausted over N dips.
//! * **Circuit breakers** — buys halt on a wide bid/ask spread or a crash drop
//!   inside the observation window (cascading-liquidation protection).
//! * **Operator constraints** — ceilings / floors / drawdown limits / pause
//!   rules (from `operator_constraints`) gate every buy and sell.

use anyhow::{bail, Result};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

use super::store::{
    LotStatus, NewExecution, NewLot, OperatorConstraint, RuleType, Side, StrategyAllocation,
    TradeExecution, TradeLot, TradingStore, EPSILON,
};

// -- defaults ---------------------------------------------------------------

/// Minimum net profit required to close a lot (1%).
pub const DEFAULT_MIN_PROFIT_MARGIN: f64 = 0.01;
/// Taker fee rate applied to every fill (0.3%, typical DEX swap fee).
pub const DEFAULT_TAKER_FEE_RATE: f64 = 0.003;
/// Number of tranches a pool is split into (3 × 33%).
pub const DEFAULT_TRANCHE_COUNT: u32 = 3;
/// Minimum buy size as a fraction of the pool (0.5%). Prevents dust-lot
/// churn from rounding remainders after `deploy_funds`.
pub const DEFAULT_MIN_TRANCHE_FRACTION: f64 = 0.005;
/// Maximum allowed bid/ask spread in percent (0.75%).
pub const DEFAULT_MAX_SPREAD_PCT: f64 = 0.75;
/// Crash threshold: buys halt if price drops this much inside the window (15%).
pub const DEFAULT_CRASH_DROP_PCT: f64 = 15.0;
/// Observation window for crash detection in seconds (15 minutes).
pub const DEFAULT_CRASH_WINDOW_SECS: i64 = 900;
/// How many days of history the ATH guard looks back to find the recent high.
/// 0 disables the guard entirely.
pub const DEFAULT_ATH_LOOKBACK_DAYS: i64 = 30;
/// Skip buys when the price is within this percent of the recent high
/// (i.e. don't buy tops — wait for a pullback).
pub const DEFAULT_MAX_BUY_FROM_HIGH_PCT: f64 = 5.0;
/// Run-up guard: skip buys when the price has risen more than this percent
/// above the recent low over the same lookback window. Stops the accumulator
/// from buying after a big run-up even when the price has pulled back from
/// the high (e.g. BTC 59k in July -> 81k in September). 0 disables the guard.
pub const DEFAULT_MAX_RUNUP_PCT: f64 = 30.0;

/// Take-profit target: lots exit at `min_exit * (1 + take_profit_pct)` instead
/// of the no-loss floor, so winners run in bull markets. Trend exit then locks
/// gains at the floor when the 50-day trend turns before the target is hit.
pub const DEFAULT_TAKE_PROFIT_PCT: f64 = 0.15;

// -- config -----------------------------------------------------------------

/// How realized profit is routed after a lot closes — the "Win-Win
/// Accumulator" strategy knob (plan §5.4).
///
/// * [`ProfitMode::Reinvest`] — every dollar of profit returns to the working
///   pool (compounding).
/// * [`ProfitMode::Separate`] — profit is parked in a separate reserve; the
///   working pool stays at its original budget and the reserve is never
///   deployed into new buys (capital preservation).
/// * [`ProfitMode::Partial`] — `f` fraction of profit is reinvested, the rest
///   goes to the reserve.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ProfitMode {
    Reinvest,
    Separate,
    Partial(f64),
}

impl ProfitMode {
    /// Canonical storage/CLI encoding: `REINVEST`, `SEPARATE`, `PARTIAL:0.50`.
    pub fn as_str(&self) -> String {
        match self {
            ProfitMode::Reinvest => "REINVEST".to_string(),
            ProfitMode::Separate => "SEPARATE".to_string(),
            ProfitMode::Partial(f) => format!("PARTIAL:{f:.4}"),
        }
    }

    pub fn parse(s: &str) -> Option<ProfitMode> {
        match s {
            "REINVEST" => Some(ProfitMode::Reinvest),
            "SEPARATE" => Some(ProfitMode::Separate),
            _ => s
                .strip_prefix("PARTIAL:")
                .and_then(|rest| rest.parse::<f64>().ok())
                .filter(|f| f.is_finite() && (0.0..=1.0).contains(f))
                .map(ProfitMode::Partial),
        }
    }
}

/// Trend-regime filter that gates buys (and optionally exits) on moving
/// averages computed from the engine's long history.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum TrendFilter {
    /// No trend filter — buy on dips regardless of regime.
    Off,
    /// Buy only when price is above the 50-day SMA.
    Sma50,
    /// Buy only when the 20-day SMA is above the 50-day SMA (golden cross).
    GoldenCross,
}

impl TrendFilter {
    /// Canonical storage/CLI encoding.
    pub fn as_str(&self) -> &'static str {
        match self {
            TrendFilter::Off => "OFF",
            TrendFilter::Sma50 => "SMA50",
            TrendFilter::GoldenCross => "GOLDEN_CROSS",
        }
    }

    pub fn parse(s: &str) -> Option<TrendFilter> {
        match s.to_uppercase().as_str() {
            "OFF" => Some(TrendFilter::Off),
            "SMA50" | "SMA" => Some(TrendFilter::Sma50),
            "GOLDEN_CROSS" | "GC" => Some(TrendFilter::GoldenCross),
            _ => None,
        }
    }
}

/// Tunable engine parameters. All values are validated at construction.
///
/// `Serialize`/`Deserialize` support JSON snapshots inside `strategy_profiles`
/// (plan §8) — the catalog stores whole configs, not field-by-field rows.
/// `#[serde(default)]` keeps profiles written by older builds (which lacked
/// the ATH/trend/take-profit fields) loadable — missing fields fall back to
/// the defaults instead of failing deserialization.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    pub min_profit_margin: f64,
    pub taker_fee_rate: f64,
    pub tranche_count: u32,
    pub min_tranche_fraction: f64,
    pub max_spread_pct: f64,
    pub crash_drop_pct: f64,
    pub crash_window_secs: i64,
    /// ATH guard: look back this many days for the recent high. 0 = disabled.
    pub ath_lookback_days: i64,
    /// ATH guard: skip buys within this percent of the recent high.
    pub max_buy_from_high_pct: f64,
    /// Conviction filter: when > 0, only buy when price is at least this
    /// percent below the recent high ("rare but accurate" mode). 0 = off.
    pub min_dip_from_high_pct: f64,
    /// Run-up guard: skip buys when price is more than this percent above the
    /// recent low over `ath_lookback_days`. 0 = disabled.
    pub max_runup_pct: f64,
    /// Trend filter: gate buys on the 50-day SMA regime. OFF = disabled.
    pub trend_filter: TrendFilter,
    /// Trend exit: sell in-profit lots when price crosses below the 50-day SMA
    /// (locks in gains before a reversal). Never violates the no-loss invariant.
    pub trend_exit: bool,
    /// Take-profit target: exit at `min_exit * (1 + take_profit_pct)`.
    pub take_profit_pct: f64,
    /// Trailing stop: when > 0, a lot in profit sells if price drops this
    /// percent below its peak since entry (never below the no-loss floor).
    /// Lets winners run in bull markets instead of capping at the fixed
    /// take-profit target. 0 = disabled.
    pub trail_pct: f64,
    pub profit_mode: ProfitMode,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            min_profit_margin: DEFAULT_MIN_PROFIT_MARGIN,
            taker_fee_rate: DEFAULT_TAKER_FEE_RATE,
            tranche_count: DEFAULT_TRANCHE_COUNT,
            min_tranche_fraction: DEFAULT_MIN_TRANCHE_FRACTION,
            max_spread_pct: DEFAULT_MAX_SPREAD_PCT,
            crash_drop_pct: DEFAULT_CRASH_DROP_PCT,
            crash_window_secs: DEFAULT_CRASH_WINDOW_SECS,
            ath_lookback_days: DEFAULT_ATH_LOOKBACK_DAYS,
            max_buy_from_high_pct: DEFAULT_MAX_BUY_FROM_HIGH_PCT,
            min_dip_from_high_pct: 0.0,
            max_runup_pct: DEFAULT_MAX_RUNUP_PCT,
            trend_filter: TrendFilter::Off,
            trend_exit: false,
            take_profit_pct: DEFAULT_TAKE_PROFIT_PCT,
            trail_pct: 0.0,
            profit_mode: ProfitMode::Reinvest,
        }
    }
}

impl EngineConfig {
    /// The "runner" exit profile (take-profit 100% + 30% trailing stop +
    /// GoldenCross trend filter/exit). This is the profile that beat B&H in
    /// 62.3% and DCA in 58.7% of the 167-window 2026-09-06 comparison, and it
    /// is what the sim sweep uses on top of the trained entry configs. Kept
    /// separate from `Default` so tests keep the conservative baseline.
    pub fn runner() -> Self {
        Self {
            trend_filter: TrendFilter::GoldenCross,
            trend_exit: true,
            take_profit_pct: 1.0,
            trail_pct: 0.30,
            ..Self::default()
        }
    }
}

impl EngineConfig {
    pub fn validate(&self) -> Result<()> {
        if !self.min_profit_margin.is_finite() || self.min_profit_margin < 0.0 {
            bail!("min_profit_margin must be finite and >= 0");
        }
        if !self.taker_fee_rate.is_finite() || !(0.0..1.0).contains(&self.taker_fee_rate) {
            bail!("taker_fee_rate must be finite and in [0, 1)");
        }
        if self.tranche_count == 0 {
            bail!("tranche_count must be >= 1");
        }
        if !self.min_tranche_fraction.is_finite()
            || !(0.0..1.0).contains(&self.min_tranche_fraction)
        {
            bail!("min_tranche_fraction must be finite and in [0, 1)");
        }
        if !self.max_spread_pct.is_finite() || self.max_spread_pct < 0.0 {
            bail!("max_spread_pct must be finite and >= 0");
        }
        if !self.crash_drop_pct.is_finite() || self.crash_drop_pct <= 0.0 {
            bail!("crash_drop_pct must be finite and > 0");
        }
        if self.crash_window_secs <= 0 {
            bail!("crash_window_secs must be > 0");
        }
        if self.ath_lookback_days < 0 || self.ath_lookback_days > 3650 {
            bail!("ath_lookback_days must be in [0, 3650]");
        }
        if !self.max_buy_from_high_pct.is_finite() || self.max_buy_from_high_pct < 0.0 {
            bail!("max_buy_from_high_pct must be finite and >= 0");
        }
        if !self.min_dip_from_high_pct.is_finite() || self.min_dip_from_high_pct < 0.0 {
            bail!("min_dip_from_high_pct must be finite and >= 0");
        }
        if !self.max_runup_pct.is_finite() || self.max_runup_pct < 0.0 {
            bail!("max_runup_pct must be finite and >= 0");
        }
        if !self.take_profit_pct.is_finite() || self.take_profit_pct < 0.0 {
            bail!("take_profit_pct must be finite and >= 0");
        }
        if !self.trail_pct.is_finite() || self.trail_pct < 0.0 {
            bail!("trail_pct must be finite and >= 0");
        }
        if let ProfitMode::Partial(f) = self.profit_mode {
            if !f.is_finite() || !(0.0..=1.0).contains(&f) {
                bail!("profit_mode PARTIAL fraction must be finite and in [0, 1]");
            }
        }
        Ok(())
    }
}

// -- market tick ------------------------------------------------------------

/// One price observation. `bid`/`ask` may equal `price` when no order book is
/// available (spread then reads as 0 and never trips the breaker).
#[derive(Debug, Clone)]
pub struct MarketTick {
    pub symbol: String,
    pub price: f64,
    pub bid: f64,
    pub ask: f64,
    pub timestamp: i64,
}

impl MarketTick {
    pub fn new(symbol: &str, price: f64, bid: f64, ask: f64, timestamp: i64) -> Self {
        Self {
            symbol: symbol.to_uppercase(),
            price,
            bid,
            ask,
            timestamp,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if self.symbol.trim().is_empty() {
            bail!("symbol must not be empty");
        }
        if !self.price.is_finite() || self.price <= 0.0 {
            bail!("price must be finite and > 0");
        }
        if !self.bid.is_finite() || self.bid <= 0.0 {
            bail!("bid must be finite and > 0");
        }
        if !self.ask.is_finite() || self.ask <= 0.0 {
            bail!("ask must be finite and > 0");
        }
        if self.ask + EPSILON < self.bid {
            bail!("ask must be >= bid");
        }
        Ok(())
    }
}

// -- decisions --------------------------------------------------------------

/// A soft decision: either a concrete plan or a skip with a human-readable
/// reason. Hard errors (invalid input, DB failures) remain `Err`.
#[derive(Debug)]
pub enum Decision<T> {
    Proceed(T),
    Skip(String),
}

impl<T> Decision<T> {
    pub fn skip(reason: impl Into<String>) -> Self {
        Self::Skip(reason.into())
    }
}

/// A fully specified buy: what, how much, at what price, and the exit target
/// that satisfies the no-loss invariant.
#[derive(Debug, Clone, PartialEq)]
pub struct BuyPlan {
    pub symbol: String,
    pub allocation_id: String,
    pub entry_price: f64,
    pub amount: f64,
    pub fee_usd: f64,
    pub cost_basis_usd: f64,
    pub target_exit_price: f64,
    pub tranche_usd: f64,
    pub rationale: String,
}

/// A fully specified sell: the lot, exit price, fees and realized profit.
#[derive(Debug, Clone, PartialEq)]
pub struct SellPlan {
    pub lot_id: String,
    pub exit_price: f64,
    pub amount: f64,
    pub fee_usd: f64,
    pub net_usd: f64,
    pub profit_usd: f64,
    pub rationale: String,
}

/// Result of one full engine cycle over a tick (plans only — nothing executed).
#[derive(Debug, Clone, Default)]
pub struct TickOutcome {
    pub symbol: String,
    pub price: f64,
    pub buys: Vec<BuyPlan>,
    pub sells: Vec<SellPlan>,
    /// Lot ids that were candidates but held (below exit target).
    pub holds: Vec<String>,
    /// Status transitions and (in `apply_tick`) executed fills.
    pub transitions: Vec<String>,
    /// Skip reasons collected during the cycle.
    pub notes: Vec<String>,
}

// -- engine -----------------------------------------------------------------

pub struct TradingEngine {
    pub config: EngineConfig,
    price_history: HashMap<String, VecDeque<(i64, f64)>>,
    /// Longer lookback for the ATH guard (trimmed to `ath_lookback_days`).
    long_history: HashMap<String, VecDeque<(i64, f64)>>,
    /// Day-bucketed closes (last close per UTC day), maintained incrementally
    /// in [`Self::observe`]. `sma`/ATH/run-up read this instead of re-bucketing
    /// `long_history` on every call — the training hot path calls them per tick.
    daily_closes: HashMap<String, VecDeque<(i64, f64)>>,
    /// Replay-scoped cache of store reads. Some only for the backtest/training
    /// path ([`Self::new_replay`]), where the engine is the sole writer, so the
    /// cache is always valid. Live mode keeps `None` and reads the store fresh
    /// every tick so external writes (Telegram commands) are always visible.
    replay: Option<RefCell<ReplayCache>>,
}

/// Per-symbol store reads cached for the duration of one replay.
#[derive(Default)]
struct ReplayCache {
    constraints: HashMap<String, Vec<OperatorConstraint>>,
    last_entry: HashMap<String, Option<f64>>,
    allocation: HashMap<String, Option<StrategyAllocation>>,
    lots: HashMap<String, Vec<TradeLot>>,
    /// Trailing-stop peaks (lot_id -> peak). In replay mode the engine is the
    /// sole writer, so peaks live here instead of a SQLite UPDATE per tick per
    /// lot — the training hot path would otherwise pay ~180 writes per lot.
    peaks: HashMap<String, f64>,
}

impl TradingEngine {
    pub fn new(config: EngineConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            price_history: HashMap::new(),
            long_history: HashMap::new(),
            daily_closes: HashMap::new(),
            replay: None,
        })
    }

    /// Engine for the replay path (backtests/training sweeps): caches store
    /// reads so per-tick SQLite traffic drops from 4 queries to ~0. Only valid
    /// while this engine is the sole writer to the store.
    pub fn new_replay(config: EngineConfig) -> Result<Self> {
        let mut e = Self::new(config)?;
        e.replay = Some(RefCell::new(ReplayCache::default()));
        Ok(e)
    }

    /// Record a price observation and trim the history to the crash window.
    pub fn observe(&mut self, tick: &MarketTick) {
        let symbol = tick.symbol.to_uppercase();
        let window = self.config.crash_window_secs;
        let entry = self.price_history.entry(symbol.clone()).or_default();
        entry.push_back((tick.timestamp, tick.price));
        if let Some(&(latest_ts, _)) = entry.back() {
            let cutoff = latest_ts - window;
            while let Some(&(ts, _)) = entry.front() {
                if ts < cutoff {
                    entry.pop_front();
                } else {
                    break;
                }
            }
        }
        // ATH-guard lookback buffer (separate, longer retention).
        // Keep at least 50 days so the SMA50 trend filter/exit always has
        // enough history even when the ATH-guard lookback is shorter (or 0).
        let lookback = (self.config.ath_lookback_days * 86_400).max(50 * 86_400);
        let long = self.long_history.entry(symbol.clone()).or_default();
        long.push_back((tick.timestamp, tick.price));
        if let Some(&(latest_ts, _)) = long.back() {
            let cutoff = latest_ts - lookback;
            while let Some(&(ts, _)) = long.front() {
                if ts < cutoff {
                    long.pop_front();
                } else {
                    break;
                }
            }
        }
        // Day-bucketed closes (last close per UTC day) — the SMA/ATH/run-up
        // inputs. Updated here so `sma()` never re-buckets the whole history.
        let day = tick.timestamp.div_euclid(86_400);
        let days = self.daily_closes.entry(symbol).or_default();
        match days.back_mut() {
            Some((d, lp)) if *d == day => *lp = tick.price,
            _ => days.push_back((day, tick.price)),
        }
        if let Some(&(latest_day, _)) = days.back() {
            let cutoff_day = latest_day - lookback / 86_400;
            while let Some(&(d, _)) = days.front() {
                if d < cutoff_day {
                    days.pop_front();
                } else {
                    break;
                }
            }
        }
    }

    /// Percent drop from the recent high over `ath_lookback_days`. `None` when
    /// the guard is disabled or there is not enough history yet.
    pub fn drop_from_recent_high(&self, symbol: &str) -> Option<f64> {
        if self.config.ath_lookback_days <= 0 {
            return None;
        }
        let hist = self.long_history.get(&symbol.to_uppercase())?;
        let latest = hist.back()?.1;
        let high = hist
            .iter()
            .map(|&(_, p)| p)
            .fold(f64::NEG_INFINITY, f64::max);
        if high <= 0.0 {
            return None;
        }
        Some((high - latest) / high * 100.0)
    }

    /// Percent run-up from the recent low over `ath_lookback_days`. `None` when
    /// the guard is disabled or there is not enough history yet.
    pub fn runup_from_recent_low(&self, symbol: &str) -> Option<f64> {
        if self.config.ath_lookback_days <= 0 || self.config.max_runup_pct <= 0.0 {
            return None;
        }
        let hist = self.long_history.get(&symbol.to_uppercase())?;
        let latest = hist.back()?.1;
        let low = hist.iter().map(|&(_, p)| p).fold(f64::INFINITY, f64::min);
        if low <= 0.0 {
            return None;
        }
        Some((latest - low) / low * 100.0)
    }

    /// Simple moving average of the last `period` daily closes. `None` when
    /// fewer than `period` days are known. Reads the incrementally maintained
    /// day-bucketed closes (see [`Self::observe`]) — O(period), not O(history).
    pub fn sma(&self, symbol: &str, period: usize) -> Option<f64> {
        if period == 0 {
            return None;
        }
        let days = self.daily_closes.get(&symbol.to_uppercase())?;
        if days.len() < period {
            return None;
        }
        let tail = days.iter().rev().take(period);
        let mut sum = 0.0;
        let mut n = 0usize;
        for &(_, p) in tail {
            sum += p;
            n += 1;
        }
        Some(sum / n as f64)
    }
    pub fn max_drop_pct_in_window(&self, symbol: &str) -> Option<f64> {
        let hist = self.price_history.get(&symbol.to_uppercase())?;
        let latest = hist.back()?.1;
        let high = hist
            .iter()
            .map(|&(_, p)| p)
            .fold(f64::NEG_INFINITY, f64::max);
        if high <= 0.0 {
            return None;
        }
        Some((high - latest) / high * 100.0)
    }

    /// Minimum exit price that satisfies the no-loss invariant for a lot.
    pub fn min_exit_price(&self, cost_basis: f64, amount: f64) -> f64 {
        cost_basis * (1.0 + self.config.min_profit_margin)
            / (amount * (1.0 - self.config.taker_fee_rate))
    }

    /// The no-loss invariant itself: net proceeds after sell fees must cover
    /// the cost basis plus the minimum profit margin. Compared against
    /// [`Self::min_exit_price`] (not a recomputed net) so a sell at exactly the
    /// stored `target_exit_price` can never be rejected by float rounding.
    pub fn invariant_ok(&self, cost_basis: f64, amount: f64, exit_price: f64) -> bool {
        exit_price + EPSILON >= self.min_exit_price(cost_basis, amount)
    }

    fn is_expired(c: &OperatorConstraint) -> bool {
        match &c.expires_at {
            None => false,
            Some(iso) => match chrono::DateTime::parse_from_rfc3339(iso) {
                Ok(dt) => dt < chrono::Utc::now(),
                // Unparseable expiry is treated as "not expired" (fail-safe:
                // never silently drop a constraint because of a bad date).
                Err(_) => false,
            },
        }
    }

    /// Active, non-expired constraints for `symbol`, including global `ALL`.
    /// One query instead of two: `asset IN ('ALL', ?)`.
    fn active_constraints_for(
        &self,
        store: &TradingStore,
        symbol: &str,
    ) -> Result<Vec<OperatorConstraint>> {
        let mut out = store.list_constraints_for_symbols(symbol)?;
        out.retain(|c| !Self::is_expired(c));
        Ok(out)
    }

    // -- replay cache -------------------------------------------------------
    // The replay path (backtests/training) is the engine as sole writer, so
    // per-symbol store reads are cached for the whole replay. Live mode
    // (`replay: None`) always reads the store fresh.

    fn cached_constraints(
        &self,
        store: &TradingStore,
        symbol: &str,
    ) -> Result<Vec<OperatorConstraint>> {
        let key = symbol.to_uppercase();
        if let Some(rc) = &self.replay {
            let mut rc = rc.borrow_mut();
            if let Some(c) = rc.constraints.get(&key) {
                return Ok(c.clone());
            }
            let c = self.active_constraints_for(store, &key)?;
            rc.constraints.insert(key, c.clone());
            Ok(c)
        } else {
            self.active_constraints_for(store, &key)
        }
    }

    fn cached_last_entry(&self, store: &TradingStore, symbol: &str) -> Result<Option<f64>> {
        let key = symbol.to_uppercase();
        if let Some(rc) = &self.replay {
            let mut rc = rc.borrow_mut();
            if let Some(v) = rc.last_entry.get(&key) {
                return Ok(*v);
            }
            let v = store.last_entry_price(&key)?;
            rc.last_entry.insert(key, v);
            Ok(v)
        } else {
            store.last_entry_price(&key)
        }
    }

    fn cached_allocation(
        &self,
        store: &TradingStore,
        symbol: &str,
    ) -> Result<Option<StrategyAllocation>> {
        let key = symbol.to_uppercase();
        if let Some(rc) = &self.replay {
            let mut rc = rc.borrow_mut();
            if let Some(v) = rc.allocation.get(&key) {
                return Ok(v.clone());
            }
            let v = store.get_active_allocation(&key)?;
            rc.allocation.insert(key, v.clone());
            Ok(v)
        } else {
            store.get_active_allocation(&key)
        }
    }

    fn cached_lots(&self, store: &TradingStore, symbol: &str) -> Result<Vec<TradeLot>> {
        let key = symbol.to_uppercase();
        if let Some(rc) = &self.replay {
            let mut rc = rc.borrow_mut();
            let v = if let Some(v) = rc.lots.get(&key) {
                v.clone()
            } else {
                let v = store.list_lots_active_for(&key)?;
                rc.lots.insert(key, v.clone());
                v
            };
            // Apply trailing-stop peaks kept in the cache (replay mode skips
            // the per-tick SQLite UPDATE — see `ReplayCache::peaks`).
            let mut out = v;
            for lot in &mut out {
                if let Some(p) = rc.peaks.get(&lot.id) {
                    lot.peak_price = *p;
                }
            }
            Ok(out)
        } else {
            store.list_lots_active_for(&key)
        }
    }

    fn invalidate_lots(&self, symbol: &str) {
        if let Some(rc) = &self.replay {
            rc.borrow_mut().lots.remove(&symbol.to_uppercase());
        }
    }

    fn invalidate_allocation(&self, symbol: &str) {
        if let Some(rc) = &self.replay {
            rc.borrow_mut().allocation.remove(&symbol.to_uppercase());
        }
    }

    fn invalidate_last_entry(&self, symbol: &str) {
        if let Some(rc) = &self.replay {
            rc.borrow_mut().last_entry.remove(&symbol.to_uppercase());
        }
    }

    // -- buy planning -------------------------------------------------------

    /// Decide whether to buy `symbol` at `price` given the current store state.
    /// Pure: never writes to the store.
    pub fn plan_buy(
        &self,
        store: &TradingStore,
        symbol: &str,
        price: f64,
        bid: f64,
        ask: f64,
    ) -> Result<Decision<BuyPlan>> {
        let symbol = symbol.to_uppercase();
        if !price.is_finite() || price <= 0.0 {
            bail!("price must be finite and > 0");
        }
        if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask <= 0.0 {
            bail!("bid/ask must be finite and > 0");
        }
        if ask + EPSILON < bid {
            bail!("ask must be >= bid");
        }

        // 1. Operator constraints.
        for c in self.cached_constraints(store, &symbol)? {
            match c.rule_type {
                RuleType::PauseBuy => {
                    return Ok(Decision::skip(format!("pause-buy active [{}]", c.id)))
                }
                RuleType::MaxPriceCeiling => {
                    if price >= c.threshold_value - EPSILON {
                        return Ok(Decision::skip(format!(
                            "price {price:.2} >= ceiling {} [{}]",
                            c.threshold_value, c.id
                        )));
                    }
                }
                RuleType::MinPriceFloor => {
                    if price <= c.threshold_value + EPSILON {
                        return Ok(Decision::skip(format!(
                            "price {price:.2} <= floor {} [{}]",
                            c.threshold_value, c.id
                        )));
                    }
                }
                RuleType::MaxDrawdownStep => {
                    if let Some(drop) = self.max_drop_pct_in_window(&symbol) {
                        if drop >= c.threshold_value - EPSILON {
                            return Ok(Decision::skip(format!(
                                "drawdown {drop:.2}% >= limit {}% [{}]",
                                c.threshold_value, c.id
                            )));
                        }
                    }
                }
                RuleType::PauseSell => {} // irrelevant for buys
            }
        }

        // 2. Circuit breakers.
        let mid = (bid + ask) / 2.0;
        let spread_pct = (ask - bid) / mid * 100.0;
        if spread_pct >= self.config.max_spread_pct - EPSILON {
            return Ok(Decision::skip(format!(
                "spread {spread_pct:.2}% >= limit {:.2}%",
                self.config.max_spread_pct
            )));
        }
        if let Some(drop) = self.max_drop_pct_in_window(&symbol) {
            if drop >= self.config.crash_drop_pct - EPSILON {
                return Ok(Decision::skip(format!(
                    "crash: {drop:.2}% drop inside window"
                )));
            }
        }

        // 3. ATH guard: never buy near the recent high — wait for a pullback.
        //    This is what stops the accumulator from buying the top of a run
        //    (the crash window alone cannot see multi-day tops).
        if let Some(drop) = self.drop_from_recent_high(&symbol) {
            if drop < self.config.max_buy_from_high_pct - EPSILON {
                return Ok(Decision::skip(format!(
                    "price within {drop:.2}% of {}-day high (ATH guard {:.1}%)",
                    self.config.ath_lookback_days, self.config.max_buy_from_high_pct
                )));
            }
        }

        // 3a. Conviction filter: when enabled, only buy on a deep pullback
        //     from the recent high ("rare but accurate" mode). Requires the
        //     dip to be confirmed — no history means no buy.
        if self.config.min_dip_from_high_pct > 0.0 {
            match self.drop_from_recent_high(&symbol) {
                Some(drop) if drop < self.config.min_dip_from_high_pct - EPSILON => {
                    return Ok(Decision::skip(format!(
                        "price only {drop:.2}% below {}-day high (conviction needs {:.1}%)",
                        self.config.ath_lookback_days, self.config.min_dip_from_high_pct
                    )))
                }
                None => {
                    return Ok(Decision::skip(format!(
                        "no {}-day history for conviction check",
                        self.config.ath_lookback_days
                    )))
                }
                _ => {}
            }
        }

        // 3b. Run-up guard: never buy after a big run-up from the recent low.
        //    The ATH guard only sees the top; this one sees how far the price
        //    has climbed from the bottom (e.g. BTC 59k -> 81k = +37%).
        if let Some(runup) = self.runup_from_recent_low(&symbol) {
            if runup > self.config.max_runup_pct - EPSILON {
                return Ok(Decision::skip(format!(
                    "price {runup:.2}% above {}-day low (run-up guard {:.1}%)",
                    self.config.ath_lookback_days, self.config.max_runup_pct
                )));
            }
        }

        // 3c. Trend filter: never buy into a confirmed downtrend. The ATH and
        //     run-up guards stop tops; this stops falling knives in bear regimes.
        if !matches!(self.config.trend_filter, TrendFilter::Off) {
            let s20 = self.sma(&symbol, 20);
            let s50 = self.sma(&symbol, 50);
            let ok = match self.config.trend_filter {
                TrendFilter::Sma50 => match s50 {
                    Some(s50) => price > s50,
                    // Not enough history — stay out until the trend is known.
                    None => false,
                },
                TrendFilter::GoldenCross => match (s20, s50) {
                    (Some(s20), Some(s50)) => s20 > s50,
                    _ => false,
                },
                TrendFilter::Off => true,
            };
            if !ok {
                return Ok(Decision::skip(format!(
                    "trend filter {}: price {price:.4} vs SMA50 {:.4}",
                    self.config.trend_filter.as_str(),
                    s50.unwrap_or(0.0)
                )));
            }
        }

        // 4. Never average up: reject while price is at/above the last entry
        //    of any open or cold-hold lot for this symbol.
        if let Some(last_entry) = self.cached_last_entry(store, &symbol)? {
            if price + EPSILON >= last_entry {
                return Ok(Decision::skip(format!(
                    "price {price:.4} not below last entry {last_entry:.4} (no averaging up)"
                )));
            }
        }

        // 5. Find an active allocation with free funds.
        let alloc = match self.cached_allocation(store, &symbol)? {
            Some(a) => a,
            None => {
                return Ok(Decision::skip(format!(
                    "no active allocation with free funds for {symbol}"
                )))
            }
        };

        // 6. Tranche sizing: one tranche per buy, capped by available funds.
        let tranche =
            (alloc.total_budget_usd / self.config.tranche_count as f64).min(alloc.available_usd);
        if tranche <= EPSILON {
            return Ok(Decision::skip(format!(
                "tranche {tranche:.2} USD too small"
            )));
        }
        // Dust guard: never open a lot smaller than `min_tranche_fraction` of
        // the pool. Rounding remainders after deploy/release can otherwise
        // produce 1e-8 USD lots that churn the ledger and trip the funds check.
        let min_tranche = alloc.total_budget_usd * self.config.min_tranche_fraction;
        if tranche < min_tranche - EPSILON {
            return Ok(Decision::skip(format!(
                "tranche {tranche:.2} USD below min {min_tranche:.2} ({}% of pool)",
                self.config.min_tranche_fraction * 100.0
            )));
        }
        // Amount chosen so cost_basis (entry*amount + fee) ≈ tranche.
        let amount = tranche / (price * (1.0 + self.config.taker_fee_rate));
        let fee_usd = amount * price * self.config.taker_fee_rate;
        let cost_basis = amount * price + fee_usd;
        // Exit target sits above the no-loss floor by `take_profit_pct`, so a
        // lot in profit is not sold the moment it crosses the floor — it runs
        // toward the target, and trend exit locks the floor if the trend turns.
        let target_exit_price =
            self.min_exit_price(cost_basis, amount) * (1.0 + self.config.take_profit_pct);

        Ok(Decision::Proceed(BuyPlan {
            symbol: symbol.clone(),
            allocation_id: alloc.id.clone(),
            entry_price: price,
            amount,
            fee_usd,
            cost_basis_usd: cost_basis,
            target_exit_price,
            tranche_usd: tranche,
            rationale: format!(
                "tranche {tranche:.2} USD of pool {:.2} at {price:.4}",
                alloc.total_budget_usd
            ),
        }))
    }

    // -- sell planning ------------------------------------------------------

    /// Decide whether lot `lot_id` may be sold at `price`. Pure: never writes.
    pub fn plan_sell(
        &self,
        store: &TradingStore,
        lot_id: &str,
        price: f64,
    ) -> Result<Decision<SellPlan>> {
        if !price.is_finite() || price <= 0.0 {
            bail!("price must be finite and > 0");
        }
        let lot = store
            .get_lot(lot_id)?
            .ok_or_else(|| anyhow::anyhow!("lot {lot_id} not found"))?;
        if lot.status == LotStatus::Closed {
            return Ok(Decision::skip("lot already closed"));
        }

        for c in self.cached_constraints(store, &lot.pair)? {
            if matches!(c.rule_type, RuleType::PauseSell) {
                return Ok(Decision::skip(format!("pause-sell active [{}]", c.id)));
            }
        }

        if !self.invariant_ok(lot.cost_basis_usd, lot.amount, price) {
            return Ok(Decision::skip(format!(
                "price {price:.4} below no-loss exit {:.4} (cost {:.2}, amount {})",
                self.min_exit_price(lot.cost_basis_usd, lot.amount),
                lot.cost_basis_usd,
                lot.amount
            )));
        }

        let fee_usd = lot.amount * price * self.config.taker_fee_rate;
        let net_usd = lot.amount * price - fee_usd;
        let profit_usd = net_usd - lot.cost_basis_usd;
        Ok(Decision::Proceed(SellPlan {
            lot_id: lot.id.clone(),
            exit_price: price,
            amount: lot.amount,
            fee_usd,
            net_usd,
            profit_usd,
            rationale: format!(
                "net {net_usd:.2} vs cost {:.2} (+{profit_usd:.2})",
                lot.cost_basis_usd
            ),
        }))
    }

    // -- execution ----------------------------------------------------------

    /// Execute a buy plan: open the lot (deploys funds atomically) and record
    /// the execution. Re-validates nothing beyond the store's own guards —
    /// the plan was already verified by [`TradingEngine::plan_buy`].
    pub fn execute_buy(&self, store: &TradingStore, plan: &BuyPlan) -> Result<TradeLot> {
        let lot = store.open_lot(NewLot {
            allocation_id: Some(plan.allocation_id.clone()),
            pair: plan.symbol.clone(),
            entry_price: plan.entry_price,
            amount: plan.amount,
            fee_paid_usd: plan.fee_usd,
            target_exit_price: plan.target_exit_price,
            cost_basis_usd: Some(plan.cost_basis_usd),
            peak_price: Some(plan.entry_price),
        })?;
        self.invalidate_lots(&plan.symbol);
        self.invalidate_last_entry(&plan.symbol);
        self.invalidate_allocation(&plan.symbol);
        store.record_execution(NewExecution {
            lot_id: Some(lot.id.clone()),
            side: Side::Buy,
            price: plan.entry_price,
            amount: plan.amount,
            fee_usd: plan.fee_usd,
            net_usd: plan.cost_basis_usd,
        })?;
        Ok(lot)
    }

    /// Execute a sell plan: re-verify the no-loss invariant (defense in depth),
    /// close the lot, return deployed funds and credit realized profit to the
    /// allocation, then record the execution.
    pub fn execute_sell(&self, store: &TradingStore, plan: &SellPlan) -> Result<TradeExecution> {
        let lot = store
            .get_lot(&plan.lot_id)?
            .ok_or_else(|| anyhow::anyhow!("lot {} not found", plan.lot_id))?;
        if lot.status == LotStatus::Closed {
            bail!("lot {} already closed", plan.lot_id);
        }
        if !self.invariant_ok(lot.cost_basis_usd, lot.amount, plan.exit_price) {
            bail!(
                "no-loss invariant violated at execution: exit {:.4} < required {:.4}",
                plan.exit_price,
                self.min_exit_price(lot.cost_basis_usd, lot.amount)
            );
        }
        let closed = store.close_lot(&plan.lot_id)?;
        self.invalidate_lots(&lot.pair);
        self.invalidate_allocation(&lot.pair);
        if let Some(aid) = &closed.allocation_id {
            store.release_funds(aid, closed.cost_basis_usd)?;
            if plan.profit_usd > EPSILON {
                // Route realized profit per the active strategy (plan §5.4).
                // `total_profit_usd` is tracked cumulatively by both paths.
                match self.config.profit_mode {
                    ProfitMode::Reinvest => {
                        store.credit_profit(aid, plan.profit_usd)?;
                    }
                    ProfitMode::Separate => {
                        store.credit_profit_reserve(aid, plan.profit_usd)?;
                    }
                    ProfitMode::Partial(f) => {
                        let reinvest = plan.profit_usd * f;
                        let reserve = plan.profit_usd - reinvest;
                        if reinvest > EPSILON {
                            store.credit_profit(aid, reinvest)?;
                        }
                        if reserve > EPSILON {
                            store.credit_profit_reserve(aid, reserve)?;
                        }
                    }
                }
            }
        }
        store.record_execution(NewExecution {
            lot_id: Some(closed.id.clone()),
            side: Side::Sell,
            price: plan.exit_price,
            amount: plan.amount,
            fee_usd: plan.fee_usd,
            net_usd: plan.net_usd,
        })
    }

    // -- full cycle ---------------------------------------------------------

    /// Deterministic cycle for one tick: manage lot statuses (OPEN ↔
    /// COLD_HOLD), plan sells for lots at/above target, then plan a buy.
    /// Returns plans WITHOUT executing them — the debate room / caller decides.
    /// Use [`TradingEngine::apply_tick`] to execute directly.
    pub fn on_tick(&mut self, store: &TradingStore, tick: &MarketTick) -> Result<TickOutcome> {
        tick.validate()?;
        self.observe(tick);
        let symbol = tick.symbol.to_uppercase();
        let mut out = TickOutcome {
            symbol: symbol.clone(),
            price: tick.price,
            ..Default::default()
        };

        for lot in self.cached_lots(store, &symbol)? {
            // Trailing-stop reference: track the peak since entry (only raise).
            // Replay mode keeps peaks in the engine cache (sole writer, no
            // SQLite write per tick); live mode persists them to the store.
            if tick.price > lot.peak_price + EPSILON {
                if let Some(rc) = &self.replay {
                    rc.borrow_mut().peaks.insert(lot.id.clone(), tick.price);
                } else {
                    store.update_lot_peak(&lot.id, tick.price)?;
                }
            }
            let peak = lot.peak_price.max(tick.price);
            let at_target = tick.price >= lot.target_exit_price - EPSILON;
            // Trailing stop: price dropped `trail_pct` below the peak while the
            // lot is still in profit — lock in the gain instead of waiting for
            // the fixed target (lets winners run in bull markets).
            let trail_break = !at_target
                && self.config.trail_pct > 0.0
                && peak > 0.0
                && tick.price <= peak * (1.0 - self.config.trail_pct)
                && self.invariant_ok(lot.cost_basis_usd, lot.amount, tick.price);
            // Trend-break exit: the lot is in profit (no-loss invariant holds)
            // but the 50-day trend has turned down — lock in gains now instead
            // of waiting for a target that may never come in this window.
            let trend_break = !at_target
                && !trail_break
                && self.config.trend_exit
                && self.invariant_ok(lot.cost_basis_usd, lot.amount, tick.price)
                && self.sma(&symbol, 50).is_some_and(|s50| tick.price < s50);
            if at_target || trail_break || trend_break {
                match self.plan_sell(store, &lot.id, tick.price)? {
                    Decision::Proceed(p) => {
                        if trail_break {
                            out.notes.push(format!(
                                "trail exit: {:.4} <= peak {:.4} - {:.1}% (locked gains)",
                                tick.price,
                                peak,
                                self.config.trail_pct * 100.0
                            ));
                        }
                        if trend_break {
                            out.notes.push(format!(
                                "trend exit: {:.4} < SMA50 (locked gains)",
                                tick.price
                            ));
                        }
                        out.sells.push(p);
                    }
                    Decision::Skip(r) => {
                        out.holds.push(lot.id.clone());
                        out.notes.push(r);
                    }
                }
            } else if tick.price < lot.entry_price - EPSILON {
                if lot.status != LotStatus::ColdHold {
                    store.set_lot_status(&lot.id, LotStatus::ColdHold)?;
                    self.invalidate_lots(&symbol);
                    out.transitions.push(format!("{} -> COLD_HOLD", lot.id));
                }
            } else if lot.status != LotStatus::Open {
                store.set_lot_status(&lot.id, LotStatus::Open)?;
                self.invalidate_lots(&symbol);
                out.transitions.push(format!("{} -> OPEN", lot.id));
            }
        }

        match self.plan_buy(store, &symbol, tick.price, tick.bid, tick.ask)? {
            Decision::Proceed(p) => out.buys.push(p),
            Decision::Skip(r) => out.notes.push(r),
        }
        Ok(out)
    }

    /// [`TradingEngine::on_tick`] plus immediate execution of every plan.
    /// Used by the sim/backtest loop and the `tick --execute` CLI.
    pub fn apply_tick(&mut self, store: &TradingStore, tick: &MarketTick) -> Result<TickOutcome> {
        let mut out = self.on_tick(store, tick)?;
        let buys = std::mem::take(&mut out.buys);
        let sells = std::mem::take(&mut out.sells);
        for p in &buys {
            let lot = self.execute_buy(store, p)?;
            out.transitions.push(format!(
                "BOUGHT {} {} @ {:.4} (lot {})",
                p.symbol, p.amount, p.entry_price, lot.id
            ));
        }
        for p in &sells {
            let ex = self.execute_sell(store, p)?;
            out.transitions.push(format!(
                "SOLD {} @ {:.4} net {:.2} (lot {})",
                p.amount, p.exit_price, ex.net_usd, p.lot_id
            ));
        }
        out.buys = buys;
        out.sells = sells;
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::store::{NewAllocation, NewConstraint, NewLot, TradingMode};
    use rand::{rngs::StdRng, Rng, SeedableRng};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_store() -> TradingStore {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = format!("target/test_trading_engine_{}_{}.db", std::process::id(), n);
        let _ = std::fs::remove_file(&path);
        TradingStore::open_path(&path, TradingMode::Simulation).unwrap()
    }

    fn alloc(store: &TradingStore, symbol: &str, usd: f64) -> String {
        store
            .create_allocation(NewAllocation {
                asset: symbol.to_string(),
                total_budget_usd: usd,
            })
            .unwrap()
            .id
    }

    fn engine() -> TradingEngine {
        TradingEngine::new(EngineConfig::default()).unwrap()
    }

    fn engine_with_mode(mode: ProfitMode) -> TradingEngine {
        let mut cfg = EngineConfig::default();
        cfg.profit_mode = mode;
        TradingEngine::new(cfg).unwrap()
    }

    /// Open a lot via the engine and return the sell plan for its target.
    fn buy_then_sell_plan(
        e: &TradingEngine,
        store: &TradingStore,
        _aid: &str,
        symbol: &str,
        price: f64,
    ) -> (TradeLot, SellPlan) {
        let plan = match e.plan_buy(store, symbol, price, price, price).unwrap() {
            Decision::Proceed(p) => p,
            Decision::Skip(r) => panic!("expected buy: {r}"),
        };
        let lot = e.execute_buy(store, &plan).unwrap();
        let sell = match e.plan_sell(store, &lot.id, plan.target_exit_price).unwrap() {
            Decision::Proceed(p) => p,
            Decision::Skip(r) => panic!("expected sell: {r}"),
        };
        (lot, sell)
    }

    fn tick(symbol: &str, price: f64, ts: i64) -> MarketTick {
        MarketTick::new(symbol, price, price, price, ts)
    }

    // -- config / tick validation ------------------------------------------

    #[test]
    fn config_validation_rejects_bad_values() {
        let mut c = EngineConfig::default();
        c.min_profit_margin = -0.01;
        assert!(TradingEngine::new(c).is_err());
        let mut c = EngineConfig::default();
        c.taker_fee_rate = 1.0;
        assert!(TradingEngine::new(c).is_err());
        let mut c = EngineConfig::default();
        c.taker_fee_rate = -0.1;
        assert!(TradingEngine::new(c).is_err());
        let mut c = EngineConfig::default();
        c.tranche_count = 0;
        assert!(TradingEngine::new(c).is_err());
        let mut c = EngineConfig::default();
        c.crash_drop_pct = 0.0;
        assert!(TradingEngine::new(c).is_err());
        let mut c = EngineConfig::default();
        c.crash_window_secs = 0;
        assert!(TradingEngine::new(c).is_err());
        let mut c = EngineConfig::default();
        c.max_spread_pct = f64::NAN;
        assert!(TradingEngine::new(c).is_err());
    }

    #[test]
    fn tick_validation_rejects_garbage() {
        assert!(tick("SOL", f64::NAN, 1).validate().is_err());
        assert!(tick("SOL", -1.0, 1).validate().is_err());
        assert!(tick("SOL", 0.0, 1).validate().is_err());
        assert!(tick("", 10.0, 1).validate().is_err());
        assert!(MarketTick::new("SOL", 10.0, 11.0, 10.0, 1)
            .validate()
            .is_err());
        assert!(MarketTick::new("SOL", 10.0, 10.0, 11.0, 1)
            .validate()
            .is_ok());
    }

    // -- buy planning -------------------------------------------------------

    #[test]
    fn buy_plan_creates_tranche_and_target() {
        let store = test_store();
        alloc(&store, "SOL", 100.0);
        let e = engine();
        match e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap() {
            Decision::Proceed(p) => {
                assert_eq!(p.symbol, "SOL");
                assert!((p.tranche_usd - 33.3333).abs() < 0.01);
                assert!((p.cost_basis_usd - p.tranche_usd).abs() < 0.01);
                assert!(p.target_exit_price > p.entry_price);
                assert!(p.amount > 0.0);
                // target must satisfy the invariant exactly
                assert!(e.invariant_ok(p.cost_basis_usd, p.amount, p.target_exit_price));
            }
            Decision::Skip(r) => panic!("expected buy, got skip: {r}"),
        }
    }

    #[test]
    fn buy_skips_when_no_allocation() {
        let store = test_store();
        let e = engine();
        match e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap() {
            Decision::Skip(r) => assert!(r.contains("no active allocation")),
            Decision::Proceed(_) => panic!("expected skip"),
        }
    }

    #[test]
    fn buy_skips_when_price_above_ceiling() {
        let store = test_store();
        alloc(&store, "SOL", 100.0);
        store
            .add_constraint(NewConstraint {
                asset: "SOL".to_string(),
                rule_type: RuleType::MaxPriceCeiling,
                threshold_value: 90.0,
                raw_prompt: "test".to_string(),
                expires_at: None,
            })
            .unwrap();
        let e = engine();
        match e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap() {
            Decision::Skip(r) => assert!(r.contains("ceiling")),
            Decision::Proceed(_) => panic!("expected skip"),
        }
        // price below ceiling is fine
        assert!(matches!(
            e.plan_buy(&store, "SOL", 80.0, 80.0, 80.0).unwrap(),
            Decision::Proceed(_)
        ));
    }

    #[test]
    fn buy_skips_when_price_below_floor() {
        let store = test_store();
        alloc(&store, "SOL", 100.0);
        store
            .add_constraint(NewConstraint {
                asset: "SOL".to_string(),
                rule_type: RuleType::MinPriceFloor,
                threshold_value: 110.0,
                raw_prompt: "test".to_string(),
                expires_at: None,
            })
            .unwrap();
        let e = engine();
        assert!(matches!(
            e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap(),
            Decision::Skip(_)
        ));
    }

    #[test]
    fn buy_skips_on_pause_buy() {
        let store = test_store();
        alloc(&store, "SOL", 100.0);
        store
            .add_constraint(NewConstraint {
                asset: "ALL".to_string(),
                rule_type: RuleType::PauseBuy,
                threshold_value: 1.0,
                raw_prompt: "test".to_string(),
                expires_at: None,
            })
            .unwrap();
        let e = engine();
        assert!(matches!(
            e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap(),
            Decision::Skip(_)
        ));
    }

    #[test]
    fn buy_skips_when_spread_too_wide() {
        let store = test_store();
        alloc(&store, "SOL", 100.0);
        let e = engine();
        // spread = (101.5 - 98.5) / 100 = 3% > 0.75%
        assert!(matches!(
            e.plan_buy(&store, "SOL", 100.0, 98.5, 101.5).unwrap(),
            Decision::Skip(_)
        ));
        // tight spread passes
        assert!(matches!(
            e.plan_buy(&store, "SOL", 100.0, 99.9, 100.1).unwrap(),
            Decision::Proceed(_)
        ));
    }

    #[test]
    fn buy_skips_on_crash_in_window() {
        let store = test_store();
        alloc(&store, "SOL", 100.0);
        let mut e = engine();
        // feed a 20% crash inside the window
        e.observe(&tick("SOL", 100.0, 1_000));
        e.observe(&tick("SOL", 80.0, 1_100));
        assert!(matches!(
            e.plan_buy(&store, "SOL", 80.0, 80.0, 80.0).unwrap(),
            Decision::Skip(_)
        ));
        // after recovery (price back near the window high) the crash breaker
        // no longer fires, but the ATH guard still refuses to buy at the high
        e.observe(&tick("SOL", 99.0, 1_200));
        let d = e.plan_buy(&store, "SOL", 99.0, 99.0, 99.0).unwrap();
        assert!(
            matches!(d, Decision::Skip(ref r) if r.contains("ATH guard")),
            "buy near the high must be skipped by the ATH guard: {d:?}"
        );
        // a deeper pullback (6% below the high) is allowed again
        e.observe(&tick("SOL", 94.0, 1_300));
        assert!(matches!(
            e.plan_buy(&store, "SOL", 94.0, 94.0, 94.0).unwrap(),
            Decision::Proceed(_)
        ));
    }

    #[test]
    fn buy_skips_when_averaging_up() {
        let store = test_store();
        let aid = alloc(&store, "SOL", 100.0);
        store
            .open_lot(NewLot {
                allocation_id: Some(aid),
                pair: "SOL".to_string(),
                entry_price: 100.0,
                amount: 0.3,
                fee_paid_usd: 0.0,
                target_exit_price: 105.0,
                cost_basis_usd: None,
                peak_price: Some(100.0),
            })
            .unwrap();
        let e = engine();
        // at/above last entry -> skip
        assert!(matches!(
            e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap(),
            Decision::Skip(_)
        ));
        assert!(matches!(
            e.plan_buy(&store, "SOL", 101.0, 101.0, 101.0).unwrap(),
            Decision::Skip(_)
        ));
        // below last entry -> dip buy allowed
        assert!(matches!(
            e.plan_buy(&store, "SOL", 99.0, 99.0, 99.0).unwrap(),
            Decision::Proceed(_)
        ));
    }

    #[test]
    fn buy_skips_when_drawdown_constraint_exceeded() {
        let store = test_store();
        alloc(&store, "SOL", 100.0);
        store
            .add_constraint(NewConstraint {
                asset: "SOL".to_string(),
                rule_type: RuleType::MaxDrawdownStep,
                threshold_value: 5.0,
                raw_prompt: "test".to_string(),
                expires_at: None,
            })
            .unwrap();
        let mut e = engine();
        e.observe(&tick("SOL", 100.0, 1_000));
        e.observe(&tick("SOL", 93.0, 1_100)); // 7% drawdown > 5%
        assert!(matches!(
            e.plan_buy(&store, "SOL", 93.0, 93.0, 93.0).unwrap(),
            Decision::Skip(_)
        ));
    }

    #[test]
    fn expired_constraint_is_ignored() {
        let store = test_store();
        alloc(&store, "SOL", 100.0);
        let past = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::hours(1))
            .unwrap()
            .to_rfc3339();
        store
            .add_constraint(NewConstraint {
                asset: "SOL".to_string(),
                rule_type: RuleType::MaxPriceCeiling,
                threshold_value: 1.0, // would block everything if active
                raw_prompt: "test".to_string(),
                expires_at: Some(past),
            })
            .unwrap();
        let e = engine();
        assert!(matches!(
            e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap(),
            Decision::Proceed(_)
        ));
    }

    #[test]
    fn tranche_never_exceeds_available() {
        let store = test_store();
        alloc(&store, "SOL", 10.0); // tiny pool
        let e = engine();
        match e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap() {
            Decision::Proceed(p) => assert!(p.tranche_usd <= 10.0 + 1e-6),
            Decision::Skip(r) => panic!("expected buy, got skip: {r}"),
        }
    }

    #[test]
    fn buy_skips_dust_tranche_below_min_fraction() {
        let store = test_store();
        let aid = alloc(&store, "SOL", 1000.0);
        // Drain the pool to a rounding dust remainder (~1e-8 USD).
        store.deploy_funds(&aid, 999.99999999).unwrap();
        let e = engine();
        match e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap() {
            Decision::Proceed(p) => panic!("expected skip for dust tranche, got {p:?}"),
            Decision::Skip(r) => assert!(r.contains("below min"), "unexpected reason: {r}"),
        }
    }

    // -- sell planning ------------------------------------------------------

    fn open_lot(store: &TradingStore, symbol: &str, entry: f64, amount: f64) -> String {
        let aid = alloc(store, symbol, 1000.0);
        store
            .open_lot(NewLot {
                allocation_id: Some(aid),
                pair: symbol.to_string(),
                entry_price: entry,
                amount,
                fee_paid_usd: 0.0,
                target_exit_price: entry * 1.1,
                cost_basis_usd: None,
                peak_price: Some(entry),
            })
            .unwrap()
            .id
    }

    #[test]
    fn sell_plan_blocks_below_invariant() {
        let store = test_store();
        let lot_id = open_lot(&store, "SOL", 100.0, 1.0); // cost 100
        let e = engine();
        // exit 100 -> net 99.7 < 101 required -> skip
        assert!(matches!(
            e.plan_sell(&store, &lot_id, 100.0).unwrap(),
            Decision::Skip(_)
        ));
        // exit 105 -> net 104.685 >= 101 -> proceed
        match e.plan_sell(&store, &lot_id, 105.0).unwrap() {
            Decision::Proceed(p) => {
                assert!(p.profit_usd > 0.0);
                assert!(p.net_usd > 100.0);
            }
            Decision::Skip(r) => panic!("expected sell, got skip: {r}"),
        }
    }

    #[test]
    fn sell_skips_on_pause_sell() {
        let store = test_store();
        let lot_id = open_lot(&store, "SOL", 100.0, 1.0);
        store
            .add_constraint(NewConstraint {
                asset: "SOL".to_string(),
                rule_type: RuleType::PauseSell,
                threshold_value: 1.0,
                raw_prompt: "test".to_string(),
                expires_at: None,
            })
            .unwrap();
        let e = engine();
        assert!(matches!(
            e.plan_sell(&store, &lot_id, 105.0).unwrap(),
            Decision::Skip(_)
        ));
    }

    #[test]
    fn sell_skips_already_closed() {
        let store = test_store();
        let lot_id = open_lot(&store, "SOL", 100.0, 1.0);
        store.close_lot(&lot_id).unwrap();
        let e = engine();
        assert!(matches!(
            e.plan_sell(&store, &lot_id, 105.0).unwrap(),
            Decision::Skip(_)
        ));
    }

    // -- execution ----------------------------------------------------------

    #[test]
    fn execute_buy_deploys_funds_and_records_execution() {
        let store = test_store();
        let aid = alloc(&store, "SOL", 100.0);
        let e = engine();
        let plan = match e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap() {
            Decision::Proceed(p) => p,
            Decision::Skip(r) => panic!("expected buy: {r}"),
        };
        let lot = e.execute_buy(&store, &plan).unwrap();
        assert_eq!(lot.status, LotStatus::Open);
        let alloc = store.get_allocation(&aid).unwrap().unwrap();
        assert!((alloc.deployed_usd - plan.cost_basis_usd).abs() < 0.01);
        assert!((alloc.available_usd - (100.0 - plan.cost_basis_usd)).abs() < 0.01);
        let exs = store.list_executions(10).unwrap();
        assert_eq!(exs.len(), 1);
        assert_eq!(exs[0].side, Side::Buy);
    }

    #[test]
    fn execute_sell_releases_and_credits_profit() {
        let store = test_store();
        let aid = alloc(&store, "SOL", 100.0);
        let e = engine();
        let plan = match e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap() {
            Decision::Proceed(p) => p,
            Decision::Skip(r) => panic!("expected buy: {r}"),
        };
        let lot = e.execute_buy(&store, &plan).unwrap();
        let before = store.get_allocation(&aid).unwrap().unwrap();
        let sell = match e
            .plan_sell(&store, &lot.id, plan.target_exit_price)
            .unwrap()
        {
            Decision::Proceed(p) => p,
            Decision::Skip(r) => panic!("expected sell: {r}"),
        };
        e.execute_sell(&store, &sell).unwrap();
        let after = store.get_allocation(&aid).unwrap().unwrap();
        // deployed back to 0, available grew by net proceeds
        assert!(after.deployed_usd < 1e-6);
        assert!((after.available_usd - (before.available_usd + sell.net_usd)).abs() < 0.01);
        // total budget grew by profit (compounding)
        assert!(
            (after.total_budget_usd - (before.total_budget_usd + sell.profit_usd)).abs() < 0.01
        );
        // cumulative profit tracked
        assert!((after.total_profit_usd - sell.profit_usd).abs() < 0.01);
        assert!(after.profit_reserve_usd < 1e-6);
        let exs = store.list_executions(10).unwrap();
        assert_eq!(exs.len(), 2);
        assert!(exs.iter().any(|e| e.side == Side::Sell));
    }

    #[test]
    fn execute_sell_separate_keeps_working_capital() {
        let store = test_store();
        let aid = alloc(&store, "SOL", 100.0);
        let e = engine_with_mode(ProfitMode::Separate);
        let (lot, sell) = buy_then_sell_plan(&e, &store, &aid, "SOL", 100.0);
        e.execute_sell(&store, &sell).unwrap();
        let after = store.get_allocation(&aid).unwrap().unwrap();
        // Working pool untouched: total stays at the original budget.
        assert!((after.total_budget_usd - 100.0).abs() < 0.01);
        assert!(after.deployed_usd < 1e-6);
        assert!((after.available_usd - 100.0).abs() < 0.01);
        // Profit parked in the reserve + tracked cumulatively.
        assert!((after.profit_reserve_usd - sell.profit_usd).abs() < 0.01);
        assert!((after.total_profit_usd - sell.profit_usd).abs() < 0.01);
        assert!(sell.profit_usd > 0.0);
        let _ = lot;
    }

    #[test]
    fn execute_sell_partial_splits_profit() {
        let store = test_store();
        let aid = alloc(&store, "SOL", 100.0);
        let e = engine_with_mode(ProfitMode::Partial(0.5));
        let (_lot, sell) = buy_then_sell_plan(&e, &store, &aid, "SOL", 100.0);
        e.execute_sell(&store, &sell).unwrap();
        let after = store.get_allocation(&aid).unwrap().unwrap();
        let half = sell.profit_usd / 2.0;
        // Half compounds into the pool, half parks in the reserve.
        assert!((after.total_budget_usd - (100.0 + half)).abs() < 0.01);
        assert!((after.profit_reserve_usd - half).abs() < 0.01);
        // Cumulative profit is the FULL realized profit.
        assert!((after.total_profit_usd - sell.profit_usd).abs() < 0.01);
    }

    #[test]
    fn cumulative_profit_sums_across_many_trades() {
        let store = test_store();
        let aid = alloc(&store, "SOL", 100.0);
        let e = engine();
        let mut total = 0.0;
        for price in [100.0, 95.0, 90.0] {
            let (_lot, sell) = buy_then_sell_plan(&e, &store, &aid, "SOL", price);
            e.execute_sell(&store, &sell).unwrap();
            total += sell.profit_usd;
        }
        let after = store.get_allocation(&aid).unwrap().unwrap();
        assert!((after.total_profit_usd - total).abs() < 0.01);
        assert!(after.total_profit_usd > 0.0);
        // Reinvest mode: no reserve, everything compounded into the pool.
        assert!(after.profit_reserve_usd < 1e-6);
        assert!((after.total_budget_usd - (100.0 + total)).abs() < 0.01);
    }

    #[test]
    fn execute_sell_refuses_violated_invariant() {
        let store = test_store();
        let lot_id = open_lot(&store, "SOL", 100.0, 1.0);
        let e = engine();
        let bad = SellPlan {
            lot_id: lot_id.clone(),
            exit_price: 100.0, // below invariant
            amount: 1.0,
            fee_usd: 0.3,
            net_usd: 99.7,
            profit_usd: -0.3,
            rationale: "forced".to_string(),
        };
        assert!(e.execute_sell(&store, &bad).is_err());
        // lot still open
        assert_eq!(
            store.get_lot(&lot_id).unwrap().unwrap().status,
            LotStatus::Open
        );
    }

    // -- full cycle ---------------------------------------------------------

    #[test]
    fn on_tick_transitions_underwater_lot_to_cold_hold() {
        let store = test_store();
        let lot_id = open_lot(&store, "SOL", 100.0, 1.0);
        let mut e = engine();
        let out = e.on_tick(&store, &tick("SOL", 90.0, 1_000)).unwrap();
        assert!(out.transitions.iter().any(|t| t.contains("COLD_HOLD")));
        assert_eq!(
            store.get_lot(&lot_id).unwrap().unwrap().status,
            LotStatus::ColdHold
        );
    }

    #[test]
    fn on_tick_recovers_cold_hold_to_open() {
        let store = test_store();
        let lot_id = open_lot(&store, "SOL", 100.0, 1.0);
        let mut e = engine();
        e.on_tick(&store, &tick("SOL", 90.0, 1_000)).unwrap();
        assert_eq!(
            store.get_lot(&lot_id).unwrap().unwrap().status,
            LotStatus::ColdHold
        );
        let out = e.on_tick(&store, &tick("SOL", 101.0, 1_100)).unwrap();
        assert!(out.transitions.iter().any(|t| t.contains("-> OPEN")));
        assert_eq!(
            store.get_lot(&lot_id).unwrap().unwrap().status,
            LotStatus::Open
        );
    }

    #[test]
    fn on_tick_plans_sell_at_target() {
        let store = test_store();
        let lot_id = open_lot(&store, "SOL", 100.0, 1.0);
        let mut e = engine();
        let out = e.on_tick(&store, &tick("SOL", 110.0, 1_000)).unwrap();
        assert_eq!(out.sells.len(), 1);
        assert_eq!(out.sells[0].lot_id, lot_id);
        assert!(out.buys.is_empty()); // no allocation for new buys at this price
    }

    #[test]
    fn trail_stop_sells_below_target_after_peak_drop() {
        let store = test_store();
        alloc(&store, "SOL", 1000.0);
        let mut cfg = EngineConfig::default();
        cfg.take_profit_pct = 0.30;
        cfg.trail_pct = 0.15;
        cfg.ath_lookback_days = 0; // allow the buy at the first tick
        let mut e = TradingEngine::new(cfg).unwrap();
        // Buy at 100.
        let out0 = e.apply_tick(&store, &tick("SOL", 100.0, 1_000)).unwrap();
        assert_eq!(out0.buys.len(), 1);
        let lot = store.list_lots(None).unwrap().remove(0);
        // Pump to 120 — below the 30% target (~130), so no fixed-target sell.
        let out1 = e.apply_tick(&store, &tick("SOL", 120.0, 2_000)).unwrap();
        assert_eq!(out1.sells.len(), 0, "below target: no fixed-target sell");
        // Drop 15% from the peak (120 * 0.85 = 102) — trail exit locks gains.
        let out2 = e.apply_tick(&store, &tick("SOL", 102.0, 3_000)).unwrap();
        assert_eq!(out2.sells.len(), 1, "trail exit must fire");
        assert!(out2.notes.iter().any(|n| n.contains("trail exit")));
        assert_eq!(
            store.get_lot(&lot.id).unwrap().unwrap().status,
            LotStatus::Closed
        );
    }

    #[test]
    fn apply_tick_executes_buy_and_sell_full_cycle() {
        let store = test_store();
        alloc(&store, "SOL", 100.0);
        let mut e = engine();
        // at the high -> ATH guard parks cash
        let out0 = e.apply_tick(&store, &tick("SOL", 100.0, 1_000)).unwrap();
        assert_eq!(out0.buys.len(), 0, "no buy at the recent high");
        // 6% pullback -> buy
        let out1 = e.apply_tick(&store, &tick("SOL", 94.0, 1_100)).unwrap();
        assert_eq!(out1.buys.len(), 1);
        assert!(out1.transitions.iter().any(|t| t.starts_with("BOUGHT")));
        let lot = store.list_lots(None).unwrap().remove(0);
        // pump above target -> sell
        let target = lot.target_exit_price;
        let out2 = e
            .apply_tick(&store, &tick("SOL", target * 1.05, 2_000))
            .unwrap();
        assert_eq!(out2.sells.len(), 1);
        assert!(out2.transitions.iter().any(|t| t.starts_with("SOLD")));
        assert_eq!(
            store.get_lot(&lot.id).unwrap().unwrap().status,
            LotStatus::Closed
        );
    }

    #[test]
    fn apply_tick_never_sells_at_loss() {
        let store = test_store();
        alloc(&store, "SOL", 100.0);
        let mut e = engine();
        e.apply_tick(&store, &tick("SOL", 100.0, 1_000)).unwrap();
        // 6% pullback -> buy
        e.apply_tick(&store, &tick("SOL", 94.0, 1_100)).unwrap();
        let lot = store.list_lots(None).unwrap().remove(0);
        // brutal crash: engine must hold, not sell
        let out = e.apply_tick(&store, &tick("SOL", 50.0, 1_200)).unwrap();
        assert!(out.sells.is_empty());
        assert_eq!(
            store.get_lot(&lot.id).unwrap().unwrap().status,
            LotStatus::ColdHold
        );
        // no executions recorded for a sell
        let exs = store.list_executions(10).unwrap();
        assert!(exs.iter().all(|e| e.side == Side::Buy));
    }

    // -- stress: flash crashes, halving-style events, extreme volatility -----

    #[test]
    fn stress_flash_crash_series_never_sells_at_loss() {
        let store = test_store();
        alloc(&store, "SOL", 1000.0);
        let mut e = engine();
        // accumulate a few lots on the way down, then crash 90%+
        let mut price = 100.0;
        let mut ts = 1_000i64;
        for _ in 0..5 {
            let out = e.apply_tick(&store, &tick("SOL", price, ts)).unwrap();
            assert!(out.sells.is_empty(), "no sells during accumulation");
            price *= 0.97;
            ts += 86_400;
        }
        // 90% flash crash in one tick
        let out = e
            .apply_tick(&store, &tick("SOL", price * 0.10, ts))
            .unwrap();
        assert!(out.sells.is_empty(), "never sell into a 90% crash");
        let lots = store.list_lots(None).unwrap();
        assert!(!lots.is_empty());
        assert!(
            lots.iter().all(|l| l.status == LotStatus::ColdHold),
            "all underwater lots must be COLD_HOLD"
        );
        // no sell executions at all
        let exs = store.list_executions(100).unwrap();
        assert!(exs.iter().all(|e| e.side == Side::Buy));
    }

    #[test]
    fn stress_halving_style_event_holds_then_sells_on_recovery() {
        let store = test_store();
        alloc(&store, "SOL", 1000.0);
        let mut e = engine();
        // at the high -> parked; 6% pullback -> buy at 94
        e.apply_tick(&store, &tick("SOL", 100.0, 1_000)).unwrap();
        let out = e.apply_tick(&store, &tick("SOL", 94.0, 1_100)).unwrap();
        assert_eq!(out.buys.len(), 1);
        let lot = store.list_lots(None).unwrap().remove(0);
        // halving-style crash: -50% overnight
        let out = e.apply_tick(&store, &tick("SOL", 50.0, 1_200)).unwrap();
        assert!(out.sells.is_empty());
        assert_eq!(
            store.get_lot(&lot.id).unwrap().unwrap().status,
            LotStatus::ColdHold
        );
        // slow recovery above the target exit -> sell at a profit
        let target = lot.target_exit_price;
        let out = e
            .apply_tick(&store, &tick("SOL", target * 1.02, 1_300))
            .unwrap();
        assert_eq!(out.sells.len(), 1);
        assert!(out.sells[0].profit_usd > 0.0);
        assert_eq!(
            store.get_lot(&lot.id).unwrap().unwrap().status,
            LotStatus::Closed
        );
    }

    #[test]
    fn stress_extreme_volatility_never_panics_or_sells_at_loss() {
        let store = test_store();
        alloc(&store, "SOL", 1000.0);
        let mut e = engine();
        // deterministic pseudo-random walk with brutal swings (up to 40% per tick)
        let mut price = 100.0_f64;
        let mut seed = 0x9E37_79B9u64;
        let mut ts = 1_000i64;
        for _ in 0..200 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let swing = ((seed >> 33) as f64 / (u32::MAX as f64)) * 0.8 - 0.4; // -40%..+40%
            price = (price * (1.0 + swing)).max(1.0);
            let out = e.apply_tick(&store, &tick("SOL", price, ts)).unwrap();
            for s in &out.sells {
                assert!(s.profit_usd > 0.0, "never a loss sell: {s:?}");
            }
            ts += 3_600;
        }
        // invariant holds for every remaining lot at the final price
        for lot in store.list_lots(None).unwrap() {
            if lot.status != LotStatus::Closed {
                assert!(!e.invariant_ok(lot.cost_basis_usd, lot.amount, price));
            }
        }
    }

    // -- fuzz-style edge cases ---------------------------------------------

    #[test]
    fn fuzz_garbage_inputs_never_panic() {
        let store = test_store();
        alloc(&store, "SOL", 100.0);
        let e = engine();
        let garbage = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1.0,
            0.0,
            1e-300,
            1e300,
        ];
        for &p in &garbage {
            // plan_buy must return Ok (Skip) or Err — never panic
            let _ = e.plan_buy(&store, "SOL", p, p, p);
            let _ = e.plan_buy(&store, "SOL", 100.0, p, 100.0);
            let _ = e.plan_buy(&store, "SOL", 100.0, 100.0, p);
            let _ = e.plan_buy(&store, "SOL", 100.0, 101.0, 100.0); // inverted book
        }
        // plan_sell with garbage price
        if let Some(lot) = store.list_lots(None).unwrap().first() {
            for &p in &garbage {
                let _ = e.plan_sell(&store, &lot.id, p);
            }
        }
        // on_tick with garbage tick must not panic
        let mut e2 = engine();
        for &p in &garbage {
            let _ = e2.on_tick(&store, &tick("SOL", p, 1));
        }
    }

    #[test]
    fn fuzz_engine_level_partial_fills_stale_books_garbage_configs() {
        // Garbage configs: extreme values must not panic on construction or
        // on any plan/execute path.
        let configs = [
            EngineConfig {
                min_profit_margin: 0.0,
                taker_fee_rate: 0.999,
                tranche_count: 1,
                min_tranche_fraction: 0.0,
                max_spread_pct: 100.0,
                crash_drop_pct: 0.01,
                crash_window_secs: 1,
                take_profit_pct: 0.0,
                trail_pct: 0.0,
                ..EngineConfig::default()
            },
            EngineConfig {
                min_profit_margin: 1.0,
                taker_fee_rate: 0.0,
                tranche_count: 100,
                min_tranche_fraction: 0.99,
                max_spread_pct: 0.0,
                crash_drop_pct: 100.0,
                crash_window_secs: 1,
                take_profit_pct: 100.0,
                trail_pct: 100.0,
                ..EngineConfig::default()
            },
        ];
        for cfg in configs {
            let store = test_store();
            alloc(&store, "SOL", 100.0);
            let mut e = TradingEngine::new(cfg).unwrap();
            let _ = e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0);
            let _ = e.on_tick(&store, &tick("SOL", 101.0, 1));
        }

        // Partial fill: execute a buy with a smaller amount than planned — the
        // lot must open at the reduced size and the no-loss invariant must hold.
        let store = test_store();
        let _aid = alloc(&store, "SOL", 100.0);
        let e = engine();
        let plan = match e.plan_buy(&store, "SOL", 100.0, 100.0, 100.0).unwrap() {
            Decision::Proceed(p) => p,
            Decision::Skip(r) => panic!("expected buy: {r}"),
        };
        let mut partial = plan.clone();
        partial.amount = plan.amount * 0.5;
        partial.cost_basis_usd = plan.cost_basis_usd * 0.5;
        partial.tranche_usd = plan.tranche_usd * 0.5;
        let lot = e.execute_buy(&store, &partial).unwrap();
        assert!(lot.amount <= plan.amount + 1e-9);
        assert!(e.invariant_ok(lot.cost_basis_usd, lot.amount, lot.target_exit_price));

        // Stale / inverted order books: bid above ask, zero-volume ticks.
        let mut e2 = engine();
        let _ = e2.on_tick(&store, &MarketTick::new("SOL", 100.0, 110.0, 90.0, 1));
        let _ = e2.on_tick(&store, &MarketTick::new("SOL", 100.0, 0.0, 0.0, 2));
        let _ = e2.on_tick(&store, &MarketTick::new("SOL", 100.0, 100.0, 100.0, 0));
    }

    #[test]
    fn fuzz_engine_random_walk_keeps_invariants() {
        // Randomized market walk + randomized configs: no realized loss, no
        // overdraft, no averaging up — the same invariants as the strategies
        // fuzz, but at the engine level through plan/execute.
        let mut rng = StdRng::seed_from_u64(99);
        for cfg_i in 0..8 {
            let cfg = EngineConfig {
                min_profit_margin: 0.005 + rng.gen::<f64>() * 0.095,
                taker_fee_rate: 0.001 + rng.gen::<f64>() * 0.009,
                tranche_count: 2 + rng.gen_range(0..7),
                min_tranche_fraction: 0.05 + rng.gen::<f64>() * 0.4,
                take_profit_pct: 0.2 + rng.gen::<f64>() * 1.8,
                trail_pct: 0.05 + rng.gen::<f64>() * 0.5,
                trend_filter: if rng.gen() {
                    TrendFilter::GoldenCross
                } else {
                    TrendFilter::Off
                },
                trend_exit: rng.gen(),
                ..EngineConfig::default()
            };
            let store = test_store();
            let aid = alloc(&store, "SOL", 100.0);
            let mut e = TradingEngine::new(cfg).unwrap();
            let mut price = 50.0 + rng.gen::<f64>() * 50.0;
            let mut ts = 0i64;
            for _ in 0..400 {
                ts += 86_400;
                price *= 1.0 + (rng.gen::<f64>() - 0.5) * 0.06;
                let _ = e.on_tick(&store, &tick("SOL", price, ts));
                if rng.gen_bool(0.05) {
                    let _ = e.plan_buy(&store, "SOL", price, price, price);
                }
            }
            // No realized loss: every closed execution must be profitable.
            let allocs = store.list_allocations(None).unwrap();
            let a = allocs.iter().find(|a| a.id == aid).unwrap();
            assert!(
                a.total_profit_usd >= -1e-6,
                "cfg {cfg_i}: profit {}",
                a.total_profit_usd
            );
            // No overdraft: deployed funds can never exceed the budget.
            assert!(
                a.deployed_usd <= 100.0 + 1e-6,
                "cfg {cfg_i}: deployed {}",
                a.deployed_usd
            );
        }
    }

    #[test]
    fn max_drop_window_trimming() {
        let mut e = engine();
        e.observe(&tick("SOL", 100.0, 0));
        e.observe(&tick("SOL", 90.0, 100));
        let drop = e.max_drop_pct_in_window("SOL").unwrap();
        assert!((drop - 10.0).abs() < 1e-9);
        e.observe(&tick("SOL", 95.0, 200));
        let drop = e.max_drop_pct_in_window("SOL").unwrap();
        assert!((drop - 5.0).abs() < 1e-9);
        // old ticks fall out of the window
        e.observe(&tick("SOL", 94.0, 1_000_000));
        let drop = e.max_drop_pct_in_window("SOL").unwrap();
        assert!(drop < 1.1);
    }

    // -- ATH guard ----------------------------------------------------------

    /// Feed a rising series so the recent high is well established.
    fn rise(e: &mut TradingEngine, symbol: &str, from: f64, to: f64, days: i64) {
        for i in 0..days {
            let p = from + (to - from) * (i as f64) / (days as f64);
            e.observe(&tick(symbol, p, i * 86_400));
        }
    }

    #[test]
    fn ath_guard_blocks_buy_near_high() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut e = engine();
        rise(&mut e, "BTC", 100.0, 110.0, 10);
        // 109 is within 5% of the 110 high -> must be skipped.
        let d = e.plan_buy(&store, "BTC", 109.0, 109.0, 109.0).unwrap();
        match d {
            Decision::Skip(r) => assert!(r.contains("ATH guard"), "{r}"),
            Decision::Proceed(_) => panic!("buy near the high must be skipped"),
        }
    }

    #[test]
    fn ath_guard_allows_buy_after_pullback() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut e = engine();
        rise(&mut e, "BTC", 100.0, 110.0, 11);
        e.observe(&tick("BTC", 110.0, 11 * 86_400));
        // 104 is 5.45% below the 110 high -> pullback, buy allowed.
        e.observe(&tick("BTC", 104.0, 12 * 86_400));
        let d = e.plan_buy(&store, "BTC", 104.0, 104.0, 104.0).unwrap();
        assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
    }

    #[test]
    fn ath_guard_disabled_when_lookback_zero() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut cfg = EngineConfig::default();
        cfg.ath_lookback_days = 0;
        let mut e = TradingEngine::new(cfg).unwrap();
        rise(&mut e, "BTC", 100.0, 110.0, 10);
        // Guard disabled -> buying at the high is allowed again.
        let d = e.plan_buy(&store, "BTC", 109.0, 109.0, 109.0).unwrap();
        assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
    }

    #[test]
    fn ath_guard_blocks_from_first_observation() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut e = engine();
        // A single observation is already the "recent high": buying at it
        // would be buying the top, so the guard blocks from the first tick.
        e.observe(&tick("BTC", 100.0, 0));
        let d = e.plan_buy(&store, "BTC", 100.0, 100.0, 100.0).unwrap();
        assert!(
            matches!(d, Decision::Skip(ref r) if r.contains("ATH guard")),
            "buy at the first observed price must be skipped: {d:?}"
        );
    }

    // -- Run-up guard --------------------------------------------------------

    /// Feed a dip-then-rise series: price falls to `low`, then climbs to `high`.
    fn dip_then_rise(
        e: &mut TradingEngine,
        symbol: &str,
        start: f64,
        low: f64,
        high: f64,
        days: i64,
    ) {
        for i in 0..days / 2 {
            let p = start + (low - start) * (i as f64) / (days as f64 / 2.0);
            e.observe(&tick(symbol, p, i * 86_400));
        }
        for i in 0..days / 2 {
            let p = low + (high - low) * (i as f64) / (days as f64 / 2.0);
            e.observe(&tick(symbol, p, (days / 2 + i) * 86_400));
        }
    }

    #[test]
    fn runup_guard_blocks_buy_after_big_rise() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut cfg = EngineConfig::default();
        cfg.max_runup_pct = 20.0;
        let mut e = TradingEngine::new(cfg).unwrap();
        // 59k -> 81k is +37%: the user's exact "July 59k, September 81k" case.
        dip_then_rise(&mut e, "BTC", 60_000.0, 59_000.0, 81_000.0, 20);
        e.observe(&tick("BTC", 81_000.0, 20 * 86_400));
        // Pull back 8% from the high (below the ATH guard's 5% band) so only
        // the run-up guard can stop this buy: 74.5k is still +26% above the low.
        e.observe(&tick("BTC", 74_500.0, 21 * 86_400));
        let d = e
            .plan_buy(&store, "BTC", 74_500.0, 74_500.0, 74_500.0)
            .unwrap();
        assert!(
            matches!(d, Decision::Skip(ref r) if r.contains("run-up guard")),
            "buy after a +26% run-up from the low must be skipped: {d:?}"
        );
    }

    #[test]
    fn runup_guard_allows_buy_after_pullback_to_low() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut cfg = EngineConfig::default();
        cfg.max_runup_pct = 20.0;
        let mut e = TradingEngine::new(cfg).unwrap();
        dip_then_rise(&mut e, "BTC", 60_000.0, 59_000.0, 81_000.0, 20);
        // Price falls back near the low: run-up ~1.7%, buy allowed.
        e.observe(&tick("BTC", 60_000.0, 21 * 86_400));
        let d = e
            .plan_buy(&store, "BTC", 60_000.0, 60_000.0, 60_000.0)
            .unwrap();
        assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
    }

    #[test]
    fn runup_guard_disabled_when_zero() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut cfg = EngineConfig::default();
        cfg.max_runup_pct = 0.0;
        let mut e = TradingEngine::new(cfg).unwrap();
        dip_then_rise(&mut e, "BTC", 60_000.0, 59_000.0, 81_000.0, 20);
        e.observe(&tick("BTC", 74_500.0, 21 * 86_400));
        let d = e
            .plan_buy(&store, "BTC", 74_500.0, 74_500.0, 74_500.0)
            .unwrap();
        assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
    }

    // -- Trend filter & trend exit -------------------------------------------

    /// Feed a linear series over `days` days from `start` to `end`.
    fn linear(e: &mut TradingEngine, symbol: &str, start: f64, end: f64, days: i64) {
        for i in 0..days {
            let p = start + (end - start) * (i as f64) / (days as f64 - 1.0);
            e.observe(&tick(symbol, p, i * 86_400));
        }
    }

    #[test]
    fn trend_filter_sma50_blocks_buy_in_downtrend() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut cfg = EngineConfig::default();
        cfg.trend_filter = TrendFilter::Sma50;
        let mut e = TradingEngine::new(cfg).unwrap();
        // 80 days of decline: price is below its own 50-day SMA.
        linear(&mut e, "BTC", 100.0, 50.0, 80);
        let d = e.plan_buy(&store, "BTC", 50.0, 50.0, 50.0).unwrap();
        assert!(
            matches!(d, Decision::Skip(ref r) if r.contains("trend filter")),
            "buy into a downtrend must be skipped: {d:?}"
        );
    }

    #[test]
    fn trend_filter_sma50_allows_buy_in_uptrend() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut cfg = EngineConfig::default();
        cfg.trend_filter = TrendFilter::Sma50;
        cfg.ath_lookback_days = 0; // guards off: this test exercises the trend lever only
        let mut e = TradingEngine::new(cfg).unwrap();
        // 80 days of rise: price sits above its 50-day SMA.
        linear(&mut e, "BTC", 50.0, 100.0, 80);
        let d = e.plan_buy(&store, "BTC", 100.0, 100.0, 100.0).unwrap();
        assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
    }

    #[test]
    fn trend_filter_off_does_not_block_downtrend() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut e = TradingEngine::new(EngineConfig::default()).unwrap();
        linear(&mut e, "BTC", 100.0, 50.0, 80);
        let d = e.plan_buy(&store, "BTC", 50.0, 50.0, 50.0).unwrap();
        assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
    }

    #[test]
    fn trend_exit_locks_gains_when_trend_breaks() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut cfg = EngineConfig::default();
        cfg.trend_exit = true;
        cfg.max_runup_pct = 0.0; // this test exercises the trend lever only
        let mut e = TradingEngine::new(cfg).unwrap();
        // Rise 100 -> 400 over 60 days, then hold at 400 for 50 days so the
        // 50-day SMA settles near the high, then pull back to 300 (25% below
        // the high so the ATH guard passes).
        linear(&mut e, "BTC", 100.0, 400.0, 60);
        for i in 0..50 {
            e.observe(&tick("BTC", 400.0, (60 + i) * 86_400));
        }
        // Buy at 300 (apply_tick executes the plan).
        let out = e
            .apply_tick(&store, &tick("BTC", 300.0, 110 * 86_400))
            .unwrap();
        assert_eq!(out.buys.len(), 1, "expected one buy: {out:?}");
        let lot = store.list_lots(None).unwrap().remove(0);
        // min_exit ≈ 300 * (1+fee)(1+margin)/(1-fee) ≈ 305 — the lot is in
        // profit as soon as price exceeds ~305.
        let min_exit = e.min_exit_price(lot.cost_basis_usd, lot.amount);
        assert!(
            min_exit < 308.0,
            "min_exit {min_exit} must sit below the hover price"
        );
        // Trend turns: price hovers at ~305-308 while the 50-day SMA (still
        // near 400) sits above it. The lot must be sold (locked gains) once
        // price is above min_exit but below the SMA.
        let mut sold = false;
        let mut saw_note = false;
        for i in 0..30 {
            let p = 300.0 + 8.0 * (i as f64) / 29.0;
            let out = e
                .apply_tick(&store, &tick("BTC", p, (111 + i) * 86_400))
                .unwrap();
            if out.sells.iter().any(|s| s.lot_id == lot.id) {
                sold = true;
            }
            if out.notes.iter().any(|n| n.contains("trend exit")) {
                saw_note = true;
            }
        }
        assert!(sold, "trend exit must sell the in-profit lot");
        assert!(saw_note, "expected a trend-exit note");
    }

    #[test]
    fn trend_exit_off_holds_through_trend_break() {
        let store = test_store();
        alloc(&store, "BTC", 1000.0);
        let mut cfg = EngineConfig::default();
        cfg.max_runup_pct = 0.0; // same setup as the trend-exit test
        let mut e = TradingEngine::new(cfg).unwrap();
        linear(&mut e, "BTC", 100.0, 400.0, 60);
        for i in 0..50 {
            e.observe(&tick("BTC", 400.0, (60 + i) * 86_400));
        }
        e.apply_tick(&store, &tick("BTC", 300.0, 110 * 86_400))
            .unwrap();
        let lot = store.list_lots(None).unwrap().remove(0);
        // Same hover as the trend-exit test: price stays above min_exit but
        // below the 50-day SMA. Without trend_exit the lot is held.
        let mut sold = false;
        for i in 0..30 {
            let p = 300.0 + 8.0 * (i as f64) / 29.0;
            let out = e
                .apply_tick(&store, &tick("BTC", p, (111 + i) * 86_400))
                .unwrap();
            if out.sells.iter().any(|s| s.lot_id == lot.id) {
                sold = true;
            }
        }
        assert!(!sold, "without trend_exit the lot must be held");
    }
}
