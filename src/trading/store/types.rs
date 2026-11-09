use super::*;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Which execution engine a row belongs to. Stored per-row in every table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TradingMode {
    Live,
    Simulation,
}

impl TradingMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            TradingMode::Live => "LIVE",
            TradingMode::Simulation => "SIMULATION",
        }
    }

    pub fn db_file(&self) -> &'static str {
        match self {
            TradingMode::Live => LIVE_DB_FILE,
            TradingMode::Simulation => SIM_DB_FILE,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "LIVE" | "REAL" => Some(TradingMode::Live),
            "SIM" | "SIMULATION" | "PAPER" | "TEST" => Some(TradingMode::Simulation),
            _ => None,
        }
    }
}

/// Operator constraint kinds (NLP-parsed, always re-checked at decision time).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RuleType {
    MaxPriceCeiling,
    MinPriceFloor,
    MaxDrawdownStep,
    PauseBuy,
    PauseSell,
}

impl RuleType {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuleType::MaxPriceCeiling => "MAX_PRICE_CEILING",
            RuleType::MinPriceFloor => "MIN_PRICE_FLOOR",
            RuleType::MaxDrawdownStep => "MAX_DRAWDOWN_STEP",
            RuleType::PauseBuy => "PAUSE_BUY",
            RuleType::PauseSell => "PAUSE_SELL",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "MAX_PRICE_CEILING" => Some(RuleType::MaxPriceCeiling),
            "MIN_PRICE_FLOOR" => Some(RuleType::MinPriceFloor),
            "MAX_DRAWDOWN_STEP" => Some(RuleType::MaxDrawdownStep),
            "PAUSE_BUY" => Some(RuleType::PauseBuy),
            "PAUSE_SELL" => Some(RuleType::PauseSell),
            _ => None,
        }
    }
}

/// Lifecycle of an individual trade lot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum LotStatus {
    Open,
    ColdHold,
    Closed,
}

impl LotStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            LotStatus::Open => "OPEN",
            LotStatus::ColdHold => "COLD_HOLD",
            LotStatus::Closed => "CLOSED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "OPEN" => Some(LotStatus::Open),
            "COLD_HOLD" => Some(LotStatus::ColdHold),
            "CLOSED" => Some(LotStatus::Closed),
            _ => None,
        }
    }
}

/// Allocation pool lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum AllocationStatus {
    Active,
    Paused,
    Completed,
}

impl AllocationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            AllocationStatus::Active => "ACTIVE",
            AllocationStatus::Paused => "PAUSED",
            AllocationStatus::Completed => "COMPLETED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ACTIVE" => Some(AllocationStatus::Active),
            "PAUSED" => Some(AllocationStatus::Paused),
            "COMPLETED" => Some(AllocationStatus::Completed),
            _ => None,
        }
    }
}

/// Verdict of the adversarial debate room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum VerdictAction {
    Buy,
    Pass,
    Defer,
    /// AI debate room decided to sell an open lot now.
    Sell,
    /// AI debate room decided to hold an open lot.
    Hold,
}

impl VerdictAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            VerdictAction::Buy => "BUY",
            VerdictAction::Pass => "PASS",
            VerdictAction::Defer => "DEFER",
            VerdictAction::Sell => "SELL",
            VerdictAction::Hold => "HOLD",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "BUY" => Some(VerdictAction::Buy),
            "PASS" => Some(VerdictAction::Pass),
            "DEFER" => Some(VerdictAction::Defer),
            "SELL" => Some(VerdictAction::Sell),
            "HOLD" => Some(VerdictAction::Hold),
            _ => None,
        }
    }
}

/// Execution side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    pub fn as_str(&self) -> &'static str {
        match self {
            Side::Buy => "BUY",
            Side::Sell => "SELL",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "BUY" => Some(Side::Buy),
            "SELL" => Some(Side::Sell),
            _ => None,
        }
    }
}

/// Lifecycle of a trade recommendation (commander/advice mode).
///
/// The bot proposes a trade (`PENDING`); the operator either executes it on
/// their own wallet and reports the fill (`EXECUTED`), declines it
/// (`REJECTED`), or lets it go stale (`EXPIRED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum RecommendationStatus {
    Pending,
    Executed,
    Rejected,
    Expired,
}

impl RecommendationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RecommendationStatus::Pending => "PENDING",
            RecommendationStatus::Executed => "EXECUTED",
            RecommendationStatus::Rejected => "REJECTED",
            RecommendationStatus::Expired => "EXPIRED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "PENDING" => Some(RecommendationStatus::Pending),
            "EXECUTED" => Some(RecommendationStatus::Executed),
            "REJECTED" => Some(RecommendationStatus::Rejected),
            "EXPIRED" => Some(RecommendationStatus::Expired),
            _ => None,
        }
    }
}

/// Execution style of a trade task: the operator executes the advice on their
/// own wallet (`Manual`) or the engine executes it (`Auto` — planned).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TaskExecution {
    Manual,
    Auto,
}

impl TaskExecution {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskExecution::Manual => "MANUAL",
            TaskExecution::Auto => "AUTO",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "MANUAL" => Some(TaskExecution::Manual),
            "AUTO" => Some(TaskExecution::Auto),
            _ => None,
        }
    }
}

/// Lifecycle of a trade task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TaskStatus {
    Active,
    Paused,
    Completed,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Active => "ACTIVE",
            TaskStatus::Paused => "PAUSED",
            TaskStatus::Completed => "COMPLETED",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "ACTIVE" => Some(TaskStatus::Active),
            "PAUSED" => Some(TaskStatus::Paused),
            "COMPLETED" => Some(TaskStatus::Completed),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Row structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OperatorConstraint {
    pub id: String,
    pub mode: TradingMode,
    pub asset: String,
    pub rule_type: RuleType,
    pub threshold_value: f64,
    pub raw_prompt: String,
    pub is_active: bool,
    pub created_at: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StrategyAllocation {
    pub id: String,
    pub mode: TradingMode,
    pub asset: String,
    pub total_budget_usd: f64,
    pub deployed_usd: f64,
    pub available_usd: f64,
    /// Profit parked separately (`ProfitMode::Separate` / `Partial`). Never
    /// deployed into new buys — only the working pool (`available_usd`) is.
    pub profit_reserve_usd: f64,
    /// Cumulative realized profit across every closed lot (all profit modes).
    pub total_profit_usd: f64,
    pub status: AllocationStatus,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TradeLot {
    pub id: String,
    pub allocation_id: Option<String>,
    pub mode: TradingMode,
    pub pair: String,
    pub entry_price: f64,
    pub amount: f64,
    pub fee_paid_usd: f64,
    pub cost_basis_usd: f64,
    pub target_exit_price: f64,
    pub status: LotStatus,
    /// Highest price seen since entry — trailing-stop reference. 0 when the
    /// trail exit is disabled (or never updated).
    pub peak_price: f64,
    pub opened_at: String,
    pub closed_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AgentDebate {
    pub id: String,
    pub lot_id: Option<String>,
    pub mode: TradingMode,
    pub market_context_json: String,
    pub bull_rationale: String,
    pub bear_rationale: String,
    pub arbiter_verdict: String,
    pub verdict_action: VerdictAction,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TradeExecution {
    pub id: String,
    pub lot_id: Option<String>,
    pub mode: TradingMode,
    pub side: Side,
    pub price: f64,
    pub amount: f64,
    pub fee_usd: f64,
    pub net_usd: f64,
    pub executed_at: String,
}

/// A proposed trade the operator may execute on their own wallet. The bot's
/// ledger mirrors the outcome so accounting stays consistent and the no-loss
/// invariant keeps governing any future sell advice.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TradeRecommendation {
    pub id: String,
    pub mode: TradingMode,
    pub side: Side,
    pub symbol: String,
    /// Lot this recommendation refers to (SELL recs always have one; BUY recs
    /// get one when the operator reports execution).
    pub lot_id: Option<String>,
    pub recommended_price: f64,
    pub amount: f64,
    /// USD value of the trade: deployed tranche for BUY, net proceeds for SELL.
    pub value_usd: f64,
    /// No-loss exit target (BUY recs only).
    pub target_exit_price: Option<f64>,
    pub rationale: String,
    pub status: RecommendationStatus,
    pub created_at: String,
    pub executed_at: Option<String>,
    /// Reported fill data once the operator executes the recommendation.
    pub actual_price: Option<f64>,
    pub actual_amount: Option<f64>,
    pub fee_usd: Option<f64>,
    pub net_usd: Option<f64>,
    pub profit_usd: Option<f64>,
    pub user_note: Option<String>,
}

/// Exchange fee configuration (per mode): buy-side and sell-side fees as
/// percentages (e.g. 5.0 = 5%). The trade-task advisor uses these so its
/// recommendations account for the real cost of the operator's exchange.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ExchangeFeeConfig {
    pub mode: TradingMode,
    pub buy_fee_pct: f64,
    pub sell_fee_pct: f64,
    pub updated_at: String,
}

/// A standing "trade task": a monthly budget, a maximum alert frequency and
/// an execution style. The advisor turns market data into concrete buy/sell
/// recommendations within these bounds, spending the budget in tranches.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TradeTask {
    pub id: String,
    pub mode: TradingMode,
    pub name: String,
    /// Comma-separated symbols, e.g. "BTC,ETH,SOL,LINK".
    pub assets: String,
    pub execution: TaskExecution,
    pub monthly_budget_usd: f64,
    pub max_alerts_per_day: i64,
    pub tranche_count: u32,
    pub min_profit_margin: f64,
    pub status: TaskStatus,
    /// Cash deployed this calendar month (fees included).
    pub month_spend_usd: f64,
    pub month_started_at: String,
    pub alerts_today: i64,
    pub alerts_date: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BacktestRun {
    pub id: String,
    pub mode: TradingMode,
    pub symbol: String,
    pub start_date: String,
    pub end_date: String,
    pub initial_capital_usd: f64,
    pub final_capital_usd: f64,
    pub total_trades: i64,
    pub wins: i64,
    pub losses: i64,
    pub pnl_usd: f64,
    pub pnl_pct: f64,
    pub max_drawdown_pct: f64,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct StrategyLesson {
    pub id: String,
    pub mode: TradingMode,
    pub symbol: String,
    pub lesson: String,
    pub context_json: String,
    pub created_at: String,
}

/// A persisted `EngineConfig` snapshot — the output of the AI auto-tuner or a
/// manual `trade config --set` override. The latest row is the active config.
#[derive(Debug, Clone)]
pub struct EngineConfigRecord {
    pub id: String,
    #[allow(dead_code)] // stored for provenance; not surfaced in CLI output yet
    pub mode: TradingMode,
    pub min_profit_margin: f64,
    pub taker_fee_rate: f64,
    pub tranche_count: u32,
    pub min_tranche_fraction: f64,
    pub max_spread_pct: f64,
    pub crash_drop_pct: f64,
    pub crash_window_secs: i64,
    /// ATH guard: look back this many days for the recent high (0 = off).
    pub ath_lookback_days: i64,
    /// ATH guard: skip buys within this percent of the recent high.
    pub max_buy_from_high_pct: f64,
    /// Conviction filter: only buy when price is at least this percent below
    /// the recent high over `ath_lookback_days` (0 = off).
    pub min_dip_from_high_pct: f64,
    /// Run-up guard: skip buys when price is more than this percent above the
    /// recent low over `ath_lookback_days` (0 = off).
    pub max_runup_pct: f64,
    /// Stored encoding: `OFF` | `SMA50` | `GOLDEN_CROSS`.
    pub trend_filter: String,
    /// Trend exit: sell in-profit lots when price crosses below the 50-day SMA.
    pub trend_exit: bool,
    /// Take-profit target: exit at `min_exit * (1 + take_profit_pct)`.
    pub take_profit_pct: f64,
    /// Trailing stop: sell in-profit lots when price drops this fraction below
    /// the peak since entry (0 = disabled). Lets winners run in bull markets.
    pub trail_pct: f64,
    /// Stored encoding: `REINVEST` | `SEPARATE` | `PARTIAL:<fraction>`.
    pub profit_mode: String,
    pub rationale: String,
    pub source: String,
    pub created_at: String,
}

impl EngineConfigRecord {
    /// Convert back into an engine config (already validated at insert time).
    pub fn to_engine_config(&self) -> crate::trading::engine::EngineConfig {
        crate::trading::engine::EngineConfig {
            min_profit_margin: self.min_profit_margin,
            taker_fee_rate: self.taker_fee_rate,
            tranche_count: self.tranche_count,
            min_tranche_fraction: self.min_tranche_fraction,
            max_spread_pct: self.max_spread_pct,
            crash_drop_pct: self.crash_drop_pct,
            crash_window_secs: self.crash_window_secs,
            ath_lookback_days: self.ath_lookback_days,
            max_buy_from_high_pct: self.max_buy_from_high_pct,
            min_dip_from_high_pct: self.min_dip_from_high_pct,
            max_runup_pct: self.max_runup_pct,
            trend_filter: crate::trading::engine::TrendFilter::parse(&self.trend_filter)
                .unwrap_or(crate::trading::engine::TrendFilter::Off),
            trend_exit: self.trend_exit,
            take_profit_pct: self.take_profit_pct,
            trail_pct: self.trail_pct,
            profit_mode: crate::trading::engine::ProfitMode::parse(&self.profit_mode)
                .unwrap_or(crate::trading::engine::ProfitMode::Reinvest),
        }
    }
}

/// A persisted strategy profile row (plan §8). Engine config + default
/// constraints are stored as JSON snapshots; `strategies::StrategyProfile`
/// is the typed view over this record.
#[derive(Debug, Clone, PartialEq)]
pub struct StrategyProfileRecord {
    pub name: String,
    pub description: String,
    pub engine_config_json: String,
    pub default_constraints_json: String,
    /// `builtin` | `trained` | `manual`.
    pub source: String,
    /// Training score (only for `source = "trained"` profiles).
    pub score: Option<f64>,
    pub created_at: String,
}

/// One row of the per-symbol training sweep (see [`NewTrainingStats`]).
/// Some fields are informational (full row mirror of `training_stats`);
/// the AI context block reads the scoring-relevant subset.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TrainingStatsRow {
    pub id: String,
    pub mode: TradingMode,
    pub symbol: String,
    pub config_name: String,
    pub min_profit_margin: f64,
    pub taker_fee_rate: f64,
    pub tranche_count: u32,
    pub crash_drop_pct: f64,
    pub profit_mode: String,
    pub runs: i64,
    pub avg_pnl_pct: f64,
    pub avg_max_dd_pct: f64,
    pub win_rate: f64,
    pub cold_ratio: f64,
    pub profitable_ratio: f64,
    pub score: f64,
    pub best_window_pnl_pct: f64,
    pub worst_window_pnl_pct: f64,
    pub trained_at: String,
}

/// One livepatch experiment row: a config sweep over windows × capitals,
/// persisted so the next run can be compared against it ("is this change
/// better than the last one?").
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ExperimentRow {
    pub id: String,
    pub label: String,
    pub config_name: String,
    pub symbols: String,
    pub from_date: String,
    pub to_date: String,
    pub window_days: i64,
    pub step_days: i64,
    pub capitals: String,
    pub runs: i64,
    pub score: f64,
    pub avg_pnl_pct: f64,
    pub avg_max_dd_pct: f64,
    pub win_rate: f64,
    pub profitable_ratio: f64,
    pub cold_ratio: f64,
    /// Windows where the config beat buy-and-hold (across capitals).
    pub windows_beaten_bh: i64,
    pub windows_total: i64,
    /// Average (config PnL − B&H) per window, in percentage points.
    pub avg_pnl_vs_bh: f64,
    pub created_at: String,
}

/// Summary counts for the `trade status` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreCounts {
    pub constraints: i64,
    pub allocations: i64,
    pub open_lots: i64,
    pub cold_lots: i64,
    pub closed_lots: i64,
    pub debates: i64,
    pub executions: i64,
    pub recommendations: i64,
    pub backtests: i64,
    pub lessons: i64,
    pub engine_configs: i64,
    pub strategy_profiles: i64,
}

// ---------------------------------------------------------------------------
// New-entity inputs
// ---------------------------------------------------------------------------

pub struct NewConstraint {
    pub asset: String,
    pub rule_type: RuleType,
    pub threshold_value: f64,
    pub raw_prompt: String,
    pub expires_at: Option<String>,
}

pub struct NewAllocation {
    pub asset: String,
    pub total_budget_usd: f64,
}

pub struct NewLot {
    pub allocation_id: Option<String>,
    pub pair: String,
    pub entry_price: f64,
    pub amount: f64,
    pub fee_paid_usd: f64,
    pub target_exit_price: f64,
    /// Precomputed cost basis (entry*amount + fee). When `None`, it is
    /// recomputed from entry/amount/fee and rounded to 8 decimals. The engine
    /// passes its plan's exact cost basis so the stored lot and the exit
    /// target stay numerically consistent (no float drift between plan and
    /// ledger).
    pub cost_basis_usd: Option<f64>,
    /// Highest price seen since entry (trailing-stop reference). `None`
    /// defaults to `entry_price`.
    pub peak_price: Option<f64>,
}

/// Input for [`TradingStore::record_debate`] — the bull/bear/arbiter outcome.
#[allow(dead_code)] // constructed by the planned AI debate room; tests use it today
pub struct NewDebate {
    pub lot_id: Option<String>,
    pub market_context_json: String,
    pub bull_rationale: String,
    pub bear_rationale: String,
    pub arbiter_verdict: String,
    pub verdict_action: VerdictAction,
}

pub struct NewExecution {
    pub lot_id: Option<String>,
    pub side: Side,
    pub price: f64,
    pub amount: f64,
    pub fee_usd: f64,
    pub net_usd: f64,
}

pub struct NewRecommendation {
    pub side: Side,
    pub symbol: String,
    pub lot_id: Option<String>,
    pub recommended_price: f64,
    pub amount: f64,
    pub value_usd: f64,
    pub target_exit_price: Option<f64>,
    pub rationale: String,
}

pub struct NewTradeTask {
    pub name: String,
    pub assets: String,
    pub execution: TaskExecution,
    pub monthly_budget_usd: f64,
    pub max_alerts_per_day: i64,
    pub tranche_count: u32,
    pub min_profit_margin: f64,
}

pub struct NewBacktest {
    pub symbol: String,
    pub start_date: String,
    pub end_date: String,
    pub initial_capital_usd: f64,
    pub final_capital_usd: f64,
    pub total_trades: i64,
    pub wins: i64,
    pub losses: i64,
    pub pnl_usd: f64,
    pub pnl_pct: f64,
    pub max_drawdown_pct: f64,
}

pub struct NewLesson {
    pub symbol: String,
    pub lesson: String,
    pub context_json: String,
}

pub struct NewEngineConfig {
    pub min_profit_margin: f64,
    pub taker_fee_rate: f64,
    pub tranche_count: u32,
    pub min_tranche_fraction: f64,
    pub max_spread_pct: f64,
    pub crash_drop_pct: f64,
    pub crash_window_secs: i64,
    /// ATH guard: look back this many days for the recent high (0 = off).
    pub ath_lookback_days: i64,
    /// ATH guard: skip buys within this percent of the recent high.
    pub max_buy_from_high_pct: f64,
    /// Conviction filter: only buy when price is at least this percent below
    /// the recent high over `ath_lookback_days` (0 = off).
    pub min_dip_from_high_pct: f64,
    /// Run-up guard: skip buys when price is more than this percent above the
    /// recent low over `ath_lookback_days` (0 = off).
    pub max_runup_pct: f64,
    /// Stored encoding: `OFF` | `SMA50` | `GOLDEN_CROSS`.
    pub trend_filter: String,
    /// Trend exit: sell in-profit lots when price crosses below the 50-day SMA.
    pub trend_exit: bool,
    /// Take-profit target: exit at `min_exit * (1 + take_profit_pct)`.
    pub take_profit_pct: f64,
    /// Trailing stop: sell in-profit lots when price drops this fraction below
    /// the peak since entry (0 = disabled).
    pub trail_pct: f64,
    /// Stored encoding: `REINVEST` | `SEPARATE` | `PARTIAL:<fraction>`.
    pub profit_mode: String,
    pub rationale: String,
    pub source: String,
}

/// One row of the training sweep: aggregate backtest statistics for a single
/// (symbol × engine config) pair. Compact enough that a full grid (hundreds of
/// configs × 4 symbols) stays well under 1 MB, and it lets the bot pick the
/// best config *per symbol* instead of only a global top-N.
pub struct NewTrainingStats {
    pub symbol: String,
    /// Stable config fingerprint, e.g. `m0.010-t4-REINVEST-c15`.
    pub config_name: String,
    pub min_profit_margin: f64,
    pub taker_fee_rate: f64,
    pub tranche_count: u32,
    pub crash_drop_pct: f64,
    pub profit_mode: String,
    pub runs: usize,
    pub avg_pnl_pct: f64,
    pub avg_max_dd_pct: f64,
    pub win_rate: f64,
    pub cold_ratio: f64,
    pub profitable_ratio: f64,
    pub score: f64,
    pub best_window_pnl_pct: f64,
    pub worst_window_pnl_pct: f64,
}

/// Input for a per-regime winner row: the best-scoring config for one
/// (symbol × market regime) bucket from the last training sweep. The bot
/// auto-selects these at runtime so it self-adapts to the current regime.
#[derive(Debug, Clone)]
pub struct NewRegimeProfile {
    pub symbol: String,
    /// Stored encoding: `BULL` | `BEAR` | `EUPHORIA` | `SIDEWAYS` | `DIP`.
    pub regime: String,
    /// Config fingerprint, e.g. `m0.010-t4-REINVEST-c15`.
    pub config_name: String,
    /// Serialized `EngineConfig` (same JSON the strategy profiles store).
    pub engine_config_json: String,
    pub score: f64,
    pub avg_pnl_pct: f64,
    pub runs: usize,
}

/// One row of `regime_profiles`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RegimeProfileRow {
    pub id: String,
    pub mode: TradingMode,
    pub symbol: String,
    pub regime: String,
    pub config_name: String,
    pub engine_config_json: String,
    pub score: f64,
    pub avg_pnl_pct: f64,
    pub runs: i64,
}

/// Input for a persisted livepatch experiment row.
#[derive(Debug, Clone)]
pub struct NewExperiment {
    pub label: String,
    pub config_name: String,
    pub symbols: String,
    pub from_date: String,
    pub to_date: String,
    pub window_days: i64,
    pub step_days: i64,
    pub capitals: String,
    pub runs: usize,
    pub score: f64,
    pub avg_pnl_pct: f64,
    pub avg_max_dd_pct: f64,
    pub win_rate: f64,
    pub profitable_ratio: f64,
    pub cold_ratio: f64,
    pub windows_beaten_bh: usize,
    pub windows_total: usize,
    pub avg_pnl_vs_bh: f64,
}

/// One advisor task-simulation run (parameter sweep over real history),
/// persisted so results survive across CLI invocations.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TaskSimRunRow {
    pub id: String,
    pub label: String,
    pub symbols: String,
    pub start_date: String,
    pub end_date: String,
    pub monthly_budget_usd: f64,
    pub buy_fee_pct: f64,
    pub sell_fee_pct: f64,
    pub tranche_count: i64,
    pub min_profit_margin: f64,
    pub max_alerts_per_day: i64,
    pub signal: String,
    pub total_buys: i64,
    pub total_sells: i64,
    pub realized_profit_usd: f64,
    pub unrealized_profit_usd: f64,
    pub fees_paid_usd: f64,
    pub open_lots: i64,
    pub total_deployed_usd: f64,
    pub win_rate: f64,
    pub total_pnl_pct: f64,
    pub score: f64,
    pub created_at: String,
}

/// Input for persisting a task-sim run.
#[derive(Debug, Clone)]
pub struct NewTaskSimRun {
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

/// One virtual (fake) trade executed by `faketrade` mode.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FakeTradeRow {
    pub id: String,
    pub ts: String,
    pub symbol: String,
    pub side: String,
    pub price: f64,
    pub amount: f64,
    pub usd: f64,
    pub fee_usd: f64,
    pub pnl_usd: f64,
    pub note: String,
}

/// Input for recording a virtual trade.
#[derive(Debug, Clone)]
pub struct NewFakeTrade {
    pub symbol: String,
    pub side: String,
    pub price: f64,
    pub amount: f64,
    pub usd: f64,
    pub fee_usd: f64,
    pub pnl_usd: f64,
    pub note: String,
}

/// Aggregated fake-account stats (spent / earned / fees / open lots).
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct FakeStats {
    pub buy_usd: f64,
    pub buy_fees: f64,
    pub buy_count: i64,
    pub sell_usd: f64,
    pub sell_fees: f64,
    pub sell_count: i64,
    pub realized_pnl: f64,
    pub net_pnl: f64,
    pub net_pct: f64,
    /// (symbol, bought_amount, bought_usd, sold_amount, sold_usd)
    pub per_symbol: Vec<(String, f64, f64, f64, f64)>,
}
