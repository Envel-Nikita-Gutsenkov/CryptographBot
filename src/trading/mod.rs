//! Autonomous pluggable trading & accumulation subsystem ("Win-Win Accumulator").
//!
//! See `docs/trading/TRADING_MODULE_PLAN.md` for the full engineering plan.
//!
//! Phase status:
//! * Phase 1 (schema + store) — done: [`store`] with dual-DB isolation.
//! * Phase 2 (deterministic engine) — done: [`engine`] with no-loss invariant,
//!   tranche sizing, circuit breakers and constraint enforcement.
//! * Phase 3 (sim engine + CLI) — CLI skeleton in [`cli`].
//! * Phase 6 (backtest + walk-forward) — [`backtest`] replays real history
//!   through the engine and records runs + lessons.
//! * Phase 9 (AI auto-tuner) — [`tuner`] lets the model adjust `EngineConfig`
//!   from backtest results + lessons; deterministic gate rejects garbage.
//! * Phase 8 (pre-trained strategy catalog + training pipeline) — [`strategies`]
//!   ships built-in profiles and can sweep historical windows to persist
//!   trained defaults, so the engine never starts from a blank slate.
//! * Phases 4–5, 7 — planned (debate room, NLP parser, Telegram, fuzz).

pub mod advisor;
pub mod analyze;
pub mod backtest;
pub mod cli;
pub mod debate;
pub mod dex;
pub mod engine;
pub mod experiment;
pub mod history_file;
pub mod market_calendar;
pub mod nlp;
pub mod regime;
pub mod store;
pub mod strategies;
pub mod tuner;

// Re-exported public API for the trading subsystem. Some items are consumed
// only by later phases (debate room, backtest, Telegram) — allow unused here.
#[allow(unused_imports)]
pub use advisor::{
    buy_signal, decide, grid_summary, report, simulate_grid, simulate_task, BuySignal, TaskAdvice,
    TaskGridConfig, TaskSimConfig, TaskSimResult,
};
#[allow(unused_imports)]
pub use backtest::{
    batch_summary, fetch_history, fetch_history_blocking, run_backtest, run_batch, windows,
    BacktestConfig, BacktestResult, BatchConfig, TradeRecord, DEFAULT_CRASH_WINDOW_DAILY_SECS,
};
#[allow(unused_imports)]
pub use debate::{build_sell_debate_prompt, parse_verdict, run_sell_debate, DebateOutcome};
#[allow(unused_imports)]
pub use dex::{
    build_swap_payload, quote_blocking, reconcile_fill, token_address, Chain, FillRecord,
    SwapQuote, SwapSide,
};
#[allow(unused_imports)]
pub use engine::{
    BuyPlan, Decision, EngineConfig, MarketTick, ProfitMode, SellPlan, TickOutcome, TradingEngine,
    DEFAULT_CRASH_DROP_PCT, DEFAULT_CRASH_WINDOW_SECS, DEFAULT_MAX_SPREAD_PCT,
    DEFAULT_MIN_PROFIT_MARGIN, DEFAULT_MIN_TRANCHE_FRACTION, DEFAULT_TAKER_FEE_RATE,
    DEFAULT_TRANCHE_COUNT,
};
#[allow(unused_imports)]
pub use experiment::{run_experiment, ExperimentConfig, ExperimentResult};
#[allow(unused_imports)]
pub use market_calendar::{
    days_to_next_halving, halving_phase, monthly_seasonality, monthly_seasonality_detailed,
    seasonality_block, seasonality_block_from_stats, HalvingPhase, HALVING_DATES,
};
#[allow(unused_imports)]
pub use nlp::{parse_trade_report, ParsedTradeReport};
#[allow(unused_imports)]
pub use regime::{classify_closes, classify_history, MarketRegime};
#[allow(unused_imports)]
pub use store::{
    AgentDebate, AllocationStatus, BacktestRun, EngineConfigRecord, LotStatus, NewAllocation,
    NewBacktest, NewConstraint, NewDebate, NewEngineConfig, NewExecution, NewLesson, NewLot,
    OperatorConstraint, RuleType, Side, StrategyAllocation, StrategyLesson, StrategyProfileRecord,
    TradeExecution, TradeLot, TradingMode, TradingStore, VerdictAction, EPSILON, LIVE_DB_FILE,
    SIM_DB_FILE,
};
#[allow(unused_imports)]
pub use strategies::{
    apply_profile, auto_engine_config, builtin_profiles, candidate_configs, plan_buy_ensemble,
    train, DefaultConstraint, GridSize, StrategyProfile, TrainedProfile, TrainingConfig,
    TrainingSummary,
};
#[allow(unused_imports)]
pub use tuner::{
    apply_proposal, build_tuner_prompt, parse_tuner_response, run_tuner, tune_with_output,
    TuneOutcome, TunerProposal,
};
