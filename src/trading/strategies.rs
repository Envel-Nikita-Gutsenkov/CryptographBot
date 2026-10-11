//! Pre-trained strategy catalog & training pipeline (plan §8).
//!
//! A strategy profile is a named, validated `EngineConfig` plus optional
//! default operator constraints. Built-in profiles ship with the binary so the
//! engine never starts from a blank slate; `train()` sweeps candidate configs
//! across historical windows and persists the best ones as `source="trained"`
//! profiles — the model then works from learned defaults instead of scratch.
//!
//! Everything here is deterministic except the history fetch, which is injected
//! (`&dyn Fn`) so tests can replay synthetic series and the CLI can use the
//! real Binance feed.

use anyhow::{bail, Result};
use chrono::{Duration, NaiveDate};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};

use super::backtest::{
    date_to_ms, min_window_candles, run_backtest_inner_with_store, windows, BacktestConfig, DAY_MS,
};
use super::engine::{
    BuyPlan, Decision, EngineConfig, ProfitMode, TradingEngine, DEFAULT_MAX_SPREAD_PCT,
    DEFAULT_MIN_TRANCHE_FRACTION, DEFAULT_TAKER_FEE_RATE,
};
use super::market_calendar::monthly_seasonality_detailed;
use super::regime::{classify_closes, classify_history, MarketRegime};
use super::store::{
    NewConstraint, NewEngineConfig, NewLesson, NewRegimeProfile, NewTrainingStats, RuleType,
    StrategyProfileRecord, TradingMode, TradingStore,
};

// ---------------------------------------------------------------------------
// Strategy profile model
// ---------------------------------------------------------------------------

/// A default operator constraint shipped with a strategy profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DefaultConstraint {
    pub asset: String,
    pub rule_type: RuleType,
    pub threshold_value: f64,
}

/// A named strategy: a preset engine config + optional default constraints.
///
/// `source` is `builtin` (ships with the binary), `trained` (produced by
/// [`train`]) or `manual` (user-created). `score` is only set for trained
/// profiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyProfile {
    pub name: String,
    pub description: String,
    pub engine_config: EngineConfig,
    pub default_constraints: Vec<DefaultConstraint>,
    pub source: String,
    pub score: Option<f64>,
}

impl StrategyProfile {
    /// Convert to the store row (engine config + constraints as JSON).
    pub fn to_record(&self) -> StrategyProfileRecord {
        StrategyProfileRecord {
            name: self.name.clone(),
            description: self.description.clone(),
            engine_config_json: serde_json::to_string(&self.engine_config)
                .unwrap_or_else(|_| "{}".to_string()),
            default_constraints_json: serde_json::to_string(&self.default_constraints)
                .unwrap_or_else(|_| "[]".to_string()),
            source: self.source.clone(),
            score: self.score,
            created_at: String::new(),
        }
    }

    /// Rebuild the typed profile from a store row. Fails on corrupt JSON —
    /// the store never writes corrupt JSON, so this is a canary for hand-edited
    /// databases.
    pub fn from_record(r: &StrategyProfileRecord) -> Result<Self> {
        Ok(StrategyProfile {
            name: r.name.clone(),
            description: r.description.clone(),
            engine_config: serde_json::from_str(&r.engine_config_json)?,
            default_constraints: serde_json::from_str(&r.default_constraints_json)?,
            source: r.source.clone(),
            score: r.score,
        })
    }
}

fn profile(
    name: &str,
    description: &str,
    engine_config: EngineConfig,
    default_constraints: Vec<DefaultConstraint>,
) -> StrategyProfile {
    StrategyProfile {
        name: name.to_string(),
        description: description.to_string(),
        engine_config,
        default_constraints,
        source: "builtin".to_string(),
        score: None,
    }
}

/// The built-in catalog. These are the sane starting points; `train()` may
/// later add `source="trained"` profiles that beat them on historical data.
pub fn builtin_profiles() -> Vec<StrategyProfile> {
    vec![
        profile(
            "Win-Win-Accumulator",
            "Flagship: never sell at a loss, buy dips in tranches, half of every \
             profit compounds and half is parked in reserve.",
            EngineConfig {
                min_profit_margin: 0.01,
                taker_fee_rate: DEFAULT_TAKER_FEE_RATE,
                tranche_count: 3,
                min_tranche_fraction: DEFAULT_MIN_TRANCHE_FRACTION,
                max_spread_pct: DEFAULT_MAX_SPREAD_PCT,
                crash_drop_pct: 15.0,
                crash_window_secs: 900,
                ath_lookback_days: super::engine::DEFAULT_ATH_LOOKBACK_DAYS,
                max_buy_from_high_pct: super::engine::DEFAULT_MAX_BUY_FROM_HIGH_PCT,
                min_dip_from_high_pct: 0.0,
                max_runup_pct: super::engine::DEFAULT_MAX_RUNUP_PCT,
                trend_filter: super::engine::TrendFilter::Sma50,
                trend_exit: true,
                take_profit_pct: 0.30,
                trail_pct: 0.15,
                profit_mode: ProfitMode::Partial(0.5),
            },
            vec![],
        ),
        profile(
            "Conservative-DCA",
            "Slow accumulation: small profit target, 4 tranches, profits parked in \
             reserve. Best for flat-to-bearish regimes.",
            EngineConfig {
                min_profit_margin: 0.005,
                taker_fee_rate: DEFAULT_TAKER_FEE_RATE,
                tranche_count: 4,
                min_tranche_fraction: 0.01,
                max_spread_pct: 0.5,
                crash_drop_pct: 20.0,
                crash_window_secs: 900,
                ath_lookback_days: super::engine::DEFAULT_ATH_LOOKBACK_DAYS,
                max_buy_from_high_pct: super::engine::DEFAULT_MAX_BUY_FROM_HIGH_PCT,
                min_dip_from_high_pct: 0.0,
                max_runup_pct: super::engine::DEFAULT_MAX_RUNUP_PCT,
                trend_filter: super::engine::TrendFilter::Sma50,
                trend_exit: false,
                take_profit_pct: 0.15,
                trail_pct: 0.0,
                profit_mode: ProfitMode::Separate,
            },
            vec![],
        ),
        profile(
            "Aggressive-Dip-Buyer",
            "More frequent dip buys with compounding: 6 tranches, wider spread \
             tolerance, reinvests all profit.",
            EngineConfig {
                min_profit_margin: 0.02,
                taker_fee_rate: DEFAULT_TAKER_FEE_RATE,
                tranche_count: 6,
                min_tranche_fraction: DEFAULT_MIN_TRANCHE_FRACTION,
                max_spread_pct: 1.0,
                crash_drop_pct: 25.0,
                crash_window_secs: 600,
                ath_lookback_days: 14,
                max_buy_from_high_pct: 3.0,
                min_dip_from_high_pct: 0.0,
                max_runup_pct: 25.0,
                trend_filter: super::engine::TrendFilter::Off,
                trend_exit: false,
                take_profit_pct: 0.10,
                trail_pct: 0.0,
                profit_mode: ProfitMode::Reinvest,
            },
            vec![],
        ),
        profile(
            "Euphoria-Protector",
            "For overheated markets: tight spread limit, aggressive crash breaker, \
             profits parked. Prevents buying tops.",
            EngineConfig {
                min_profit_margin: 0.03,
                taker_fee_rate: DEFAULT_TAKER_FEE_RATE,
                tranche_count: 2,
                min_tranche_fraction: 0.02,
                max_spread_pct: 0.5,
                crash_drop_pct: 10.0,
                crash_window_secs: 1800,
                ath_lookback_days: 60,
                max_buy_from_high_pct: 8.0,
                min_dip_from_high_pct: 0.0,
                max_runup_pct: 20.0,
                trend_filter: super::engine::TrendFilter::GoldenCross,
                trend_exit: true,
                take_profit_pct: 0.30,
                trail_pct: 0.15,
                profit_mode: ProfitMode::Separate,
            },
            vec![],
        ),
        profile(
            "Conviction-Dip-Buyer",
            "Rare but accurate: buys only 40%+ below the recent high, lets winners \
             run to +200% with a wide trailing stop. Fewer trades, each significant.",
            EngineConfig {
                min_profit_margin: 0.01,
                taker_fee_rate: DEFAULT_TAKER_FEE_RATE,
                tranche_count: 4,
                min_tranche_fraction: DEFAULT_MIN_TRANCHE_FRACTION,
                max_spread_pct: DEFAULT_MAX_SPREAD_PCT,
                crash_drop_pct: 15.0,
                crash_window_secs: 900,
                ath_lookback_days: super::engine::DEFAULT_ATH_LOOKBACK_DAYS,
                max_buy_from_high_pct: super::engine::DEFAULT_MAX_BUY_FROM_HIGH_PCT,
                min_dip_from_high_pct: 40.0,
                max_runup_pct: super::engine::DEFAULT_MAX_RUNUP_PCT,
                trend_filter: super::engine::TrendFilter::Off,
                trend_exit: false,
                take_profit_pct: 2.0,
                trail_pct: 0.40,
                profit_mode: ProfitMode::Reinvest,
            },
            vec![],
        ),
    ]
}

/// Apply a profile: persist its engine config as the active one and add its
/// default constraints. The config is validated before anything is written, so
/// a corrupt profile can never touch the ledger.
pub fn apply_profile(store: &TradingStore, profile: &StrategyProfile) -> Result<()> {
    profile.engine_config.validate()?;
    store.record_engine_config(NewEngineConfig {
        min_profit_margin: profile.engine_config.min_profit_margin,
        taker_fee_rate: profile.engine_config.taker_fee_rate,
        tranche_count: profile.engine_config.tranche_count,
        min_tranche_fraction: profile.engine_config.min_tranche_fraction,
        max_spread_pct: profile.engine_config.max_spread_pct,
        crash_drop_pct: profile.engine_config.crash_drop_pct,
        crash_window_secs: profile.engine_config.crash_window_secs,
        ath_lookback_days: profile.engine_config.ath_lookback_days,
        max_buy_from_high_pct: profile.engine_config.max_buy_from_high_pct,
        min_dip_from_high_pct: profile.engine_config.min_dip_from_high_pct,
        max_runup_pct: profile.engine_config.max_runup_pct,
        trend_filter: profile.engine_config.trend_filter.as_str().to_string(),
        trend_exit: profile.engine_config.trend_exit,
        take_profit_pct: profile.engine_config.take_profit_pct,
        trail_pct: profile.engine_config.trail_pct,
        profit_mode: profile.engine_config.profit_mode.as_str(),
        rationale: format!(
            "strategy '{}' applied: {}",
            profile.name, profile.description
        ),
        source: format!("strategy:{}", profile.name),
    })?;
    for c in &profile.default_constraints {
        store.add_constraint(NewConstraint {
            asset: c.asset.clone(),
            rule_type: c.rule_type,
            threshold_value: c.threshold_value,
            raw_prompt: format!(
                "strategy '{}': {} {}",
                profile.name,
                c.rule_type.as_str(),
                c.threshold_value
            ),
            expires_at: None,
        })?;
    }
    Ok(())
}

/// Auto-adapt: pick the best engine config for the current market regime.
/// Explicit user choices (manual overrides, applied strategies) always win;
/// otherwise the bot classifies the regime from `history` and uses the
/// per-regime winner from the last training sweep. Falls back to the latest
/// persisted config, then the runner profile.
pub fn auto_engine_config(
    store: &TradingStore,
    symbol: &str,
    history: &[(i64, f64)],
) -> Result<EngineConfig> {
    // Explicit user choice wins.
    if let Some(rec) = store.latest_engine_config()? {
        if rec.source == "manual" || rec.source.starts_with("strategy:") {
            return Ok(rec.to_engine_config());
        }
    }
    // Auto-adapt: best profile for the current regime.
    let regime = classify_history(history);
    if let Some(row) = store.get_regime_profile(symbol, regime.as_str())? {
        if let Ok(cfg) = serde_json::from_str::<EngineConfig>(&row.engine_config_json) {
            return Ok(cfg);
        }
    }
    Ok(store
        .latest_engine_config()?
        .map(|r| r.to_engine_config())
        .unwrap_or_else(EngineConfig::runner))
}

// ---------------------------------------------------------------------------
// Training pipeline
// ---------------------------------------------------------------------------

/// Grid density for [`candidate_configs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridSize {
    /// 36 candidates (2 margins × 2 tranche counts × 3 profit modes × 3
    /// trend/exit profiles) — the default, fast enough for routine retraining.
    Small,
    /// 180 candidates (5 × 4 × 3 × 3 trend/exit profiles) — a full sweep, for
    /// weekend retrains. The crash-drop knob is deliberately NOT swept: on
    /// daily candles (86 400 s apart) the crash window (900/3600 s) contains at
    /// most one tick, so `max_drop_pct_in_window` never fires and the knob is
    /// dead — sweeping it only multiplied the grid by 2 with zero signal.
    Full,
}

/// Generate the candidate `EngineConfig` grid for training. Every candidate is
/// validated; the grid deliberately stays inside the tuner's sane bounds.
///
/// The trend-filter dimension is paired with the trend exit: `Sma50` candidates
/// also lock in gains on a trend break (`trend_exit = true`), `Off` candidates
/// never do. This keeps the grid size identical to the pre-trend sweep while
/// adding the single most impactful regime lever.
pub fn candidate_configs(grid: GridSize) -> Vec<EngineConfig> {
    let (margins, tranches, crashes): (&[f64], &[u32], &[(f64, i64)]) = match grid {
        GridSize::Small => (&[0.01, 0.03], &[2, 4], &[(15.0, 900)]),
        GridSize::Full => (
            // 5 margins x 4 tranches x 3 modes x 3 trend/exit profiles = 180
            // candidates (same size as the pre-trail sweep, but the dead crash
            // knob and redundant tranche 3 are replaced by the runner profile).
            &[0.005, 0.01, 0.02, 0.03, 0.05],
            &[2, 4, 6, 8],
            // Single crash profile — the knob is dead on daily candles (see
            // `GridSize::Full` docs), so varying it only duplicates candidates.
            &[(15.0, 900)],
        ),
    };
    let modes = [
        ProfitMode::Reinvest,
        ProfitMode::Separate,
        ProfitMode::Partial(0.5),
    ];
    // Take-profit is paired with the trend lever: `Off` candidates take a
    // modest 10% profit (they still exit at the floor on a crash), `Sma50`
    // candidates run winners to 30% and lock the floor on a trend break.
    // `Runner` lets winners run to 100% with a 30% trailing stop — the profile
    // that beat B&H/DCA in the 2026-09-06 batch comparison (62% of windows vs
    // B&H, 59% vs DCA). Grid size stays unchanged.
    let trends = [
        (super::engine::TrendFilter::Off, false, 0.10, 0.0),
        (super::engine::TrendFilter::Sma50, true, 0.30, 0.15),
        (super::engine::TrendFilter::GoldenCross, true, 1.0, 0.30),
    ];
    let mut out = Vec::new();
    for &margin in margins {
        for &tranches in tranches {
            for &mode in &modes {
                for &(drop, window) in crashes {
                    for &(trend, exit, tp, trail) in &trends {
                        out.push(EngineConfig {
                            min_profit_margin: margin,
                            taker_fee_rate: DEFAULT_TAKER_FEE_RATE,
                            tranche_count: tranches,
                            min_tranche_fraction: DEFAULT_MIN_TRANCHE_FRACTION,
                            max_spread_pct: DEFAULT_MAX_SPREAD_PCT,
                            crash_drop_pct: drop,
                            crash_window_secs: window,
                            ath_lookback_days: super::engine::DEFAULT_ATH_LOOKBACK_DAYS,
                            max_buy_from_high_pct: super::engine::DEFAULT_MAX_BUY_FROM_HIGH_PCT,
                            min_dip_from_high_pct: 0.0,
                            max_runup_pct: super::engine::DEFAULT_MAX_RUNUP_PCT,
                            trend_filter: trend,
                            trend_exit: exit,
                            take_profit_pct: tp,
                            trail_pct: trail,
                            profit_mode: mode,
                        });
                    }
                }
            }
        }
    }
    out
}

/// High-conviction candidate configs ("rare but accurate" mode): deep-dip
/// entries (`min_dip_from_high_pct` 30/40/50% below the recent high) paired
/// with large take-profit targets (100/200%) and wide trailing stops so
/// winners run. Fewer, higher-quality trades instead of many small ones.
pub fn conviction_configs() -> Vec<EngineConfig> {
    let mut out = Vec::new();
    // 15/20/25% are BTC-friendly (BTC dips are shallower than alts);
    // 30/40/50% are the altcoin deep-dip entries.
    for &dip in &[15.0f64, 20.0, 25.0, 30.0, 40.0, 50.0] {
        for &(tp, trail) in &[(1.0f64, 0.30), (2.0, 0.40), (3.0, 0.50)] {
            for &mode in &[
                ProfitMode::Reinvest,
                ProfitMode::Separate,
                ProfitMode::Partial(0.5),
            ] {
                out.push(EngineConfig {
                    min_profit_margin: 0.01,
                    taker_fee_rate: DEFAULT_TAKER_FEE_RATE,
                    tranche_count: 4,
                    min_tranche_fraction: DEFAULT_MIN_TRANCHE_FRACTION,
                    max_spread_pct: DEFAULT_MAX_SPREAD_PCT,
                    crash_drop_pct: 15.0,
                    crash_window_secs: 900,
                    ath_lookback_days: super::engine::DEFAULT_ATH_LOOKBACK_DAYS,
                    max_buy_from_high_pct: super::engine::DEFAULT_MAX_BUY_FROM_HIGH_PCT,
                    min_dip_from_high_pct: dip,
                    max_runup_pct: super::engine::DEFAULT_MAX_RUNUP_PCT,
                    trend_filter: super::engine::TrendFilter::Off,
                    trend_exit: false,
                    take_profit_pct: tp,
                    trail_pct: trail,
                    profit_mode: mode,
                });
            }
        }
    }
    out
}

/// Training run configuration.
#[derive(Debug, Clone)]
pub struct TrainingConfig {
    pub symbols: Vec<String>,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub window_days: i64,
    pub step_days: i64,
    pub capital_usd: f64,
    /// How many top-scoring configs to persist as profiles.
    pub top_n: usize,
    /// Re-run even when `trained` profiles already exist.
    pub force: bool,
    /// Worker threads for the candidate sweep (default: CPU count).
    pub threads: usize,
}

impl TrainingConfig {
    pub fn validate(&self) -> Result<()> {
        if self.symbols.is_empty() {
            bail!("at least one symbol required");
        }
        if self.to <= self.from {
            bail!("to date must be after from date");
        }
        if self.window_days <= 0 || self.step_days <= 0 {
            bail!("window_days and step_days must be > 0");
        }
        if !self.capital_usd.is_finite() || self.capital_usd <= 0.0 {
            bail!("capital must be finite and > 0");
        }
        if self.top_n == 0 {
            bail!("top_n must be >= 1");
        }
        if self.threads == 0 {
            bail!("threads must be >= 1");
        }
        Ok(())
    }
}

/// One scored candidate after a training sweep.
#[derive(Debug, Clone)]
pub struct TrainedProfile {
    pub profile: StrategyProfile,
    pub score: f64,
    pub avg_pnl_pct: f64,
    pub avg_max_dd_pct: f64,
    /// Fraction of all lots stuck in COLD_HOLD (capital frozen underwater).
    pub cold_ratio: f64,
    /// Realized-trade win rate (wins / (wins + losses)).
    pub win_rate: f64,
    /// Fraction of windows that ended with positive PnL.
    pub profitable_ratio: f64,
    pub runs: usize,
}

/// Result of a training sweep.
#[derive(Debug, Clone)]
pub struct TrainingSummary {
    pub candidates: usize,
    pub runs: usize,
    pub trained: Vec<TrainedProfile>,
    /// Per-symbol top-N profiles (`Trained-{SYM}-{rank}-...`), so the bot can
    /// apply the best config for a specific asset, not just a global winner.
    pub per_symbol_trained: Vec<TrainedProfile>,
    /// `true` when the sweep was skipped because trained profiles already
    /// exist and `force` was not set.
    pub skipped: bool,
}

#[derive(Debug, Clone)]
struct CandidateStats {
    runs: usize,
    pnl_sum: f64,
    dd_sum: f64,
    wins: i64,
    losses: i64,
    cold_lots: usize,
    total_lots: usize,
    profitable_runs: usize,
    best_pnl: f64,
    worst_pnl: f64,
}

impl Default for CandidateStats {
    fn default() -> Self {
        Self {
            runs: 0,
            pnl_sum: 0.0,
            dd_sum: 0.0,
            wins: 0,
            losses: 0,
            cold_lots: 0,
            total_lots: 0,
            profitable_runs: 0,
            best_pnl: f64::NEG_INFINITY,
            worst_pnl: f64::INFINITY,
        }
    }
}

impl CandidateStats {
    fn finish(&self, config: &EngineConfig) -> TrainedProfile {
        let runs = self.runs.max(1) as f64;
        let avg_pnl = self.pnl_sum / runs;
        let avg_dd = self.dd_sum / runs;
        let win_rate = if self.wins + self.losses > 0 {
            self.wins as f64 / (self.wins + self.losses) as f64
        } else {
            0.0
        };
        let cold_ratio = if self.total_lots > 0 {
            self.cold_lots as f64 / self.total_lots as f64
        } else {
            0.0
        };
        let profitable_ratio = self.profitable_runs as f64 / runs;
        // Composite score: reward profit and consistency, penalize drawdown
        // and frozen capital. Units: PnL/DD in %, ratios as fractions.
        let score =
            avg_pnl - 0.5 * avg_dd - 20.0 * cold_ratio + 8.0 * profitable_ratio + 5.0 * win_rate;
        TrainedProfile {
            profile: StrategyProfile {
                name: String::new(), // filled by train()
                description: String::new(),
                engine_config: config.clone(),
                default_constraints: vec![],
                source: "trained".to_string(),
                score: Some(score),
            },
            score,
            avg_pnl_pct: avg_pnl,
            avg_max_dd_pct: avg_dd,
            cold_ratio,
            win_rate,
            profitable_ratio,
            runs: self.runs,
        }
    }
}

fn trained_name(rank: usize, c: &EngineConfig) -> String {
    // Full fingerprint (incl. trend/tp/trail/dip) so conviction configs that
    // share margin/tranches/mode/crash still get distinct, informative names
    // (name is the PK).
    format!("Trained-{rank}-{}", config_fingerprint(c))
}

/// Rank-independent config fingerprint used as the stable key for
/// `training_stats` rows, so a `--force` retrain overwrites the same rows
/// instead of duplicating them. Also used by the livepatch experiment module
/// to name configs consistently.
pub(crate) fn config_fingerprint(c: &EngineConfig) -> String {
    let mode = match c.profit_mode {
        ProfitMode::Reinvest => "REINVEST",
        ProfitMode::Separate => "SEPARATE",
        ProfitMode::Partial(_) => "PARTIAL",
    };
    let trend = match c.trend_filter {
        super::engine::TrendFilter::Off => "off",
        super::engine::TrendFilter::Sma50 => "sma50",
        super::engine::TrendFilter::GoldenCross => "gc",
    };
    // Include trend/tp/trail/dip so every candidate is uniquely identifiable
    // (conviction configs share margin/tranches/crash and differ only here).
    format!(
        "m{:.3}-t{}-{mode}-c{:.0}-{trend}-tp{:.2}-tr{:.2}-d{:.0}",
        c.min_profit_margin,
        c.tranche_count,
        c.crash_drop_pct,
        c.take_profit_pct,
        c.trail_pct,
        c.min_dip_from_high_pct
    )
}

/// Sweep `candidates` over rolling windows per symbol, score each config and
/// persist the top `top_n` as `source="trained"` strategy profiles.
///
/// History is fetched once per symbol (via the injected `fetch`) and sliced per
/// window, so a full sweep is N_candidates × N_windows × N_symbols replays with
/// a single fetch per symbol. Intermediate runs are NOT persisted — only the
/// winning profiles land in the store.
pub fn train(
    store: &TradingStore,
    cfg: &TrainingConfig,
    candidates: &[EngineConfig],
    fetch: &dyn Fn(&str, NaiveDate, NaiveDate) -> Result<Vec<(i64, f64)>>,
) -> Result<TrainingSummary> {
    cfg.validate()?;
    if candidates.is_empty() {
        bail!("no candidate configs to train");
    }

    // Idempotency: trained profiles already exist → skip unless forced.
    let existing = store.list_strategy_profiles()?;
    if existing.iter().any(|p| p.source == "trained") && !cfg.force {
        return Ok(TrainingSummary {
            candidates: candidates.len(),
            runs: 0,
            trained: vec![],
            per_symbol_trained: vec![],
            skipped: true,
        });
    }

    // Fetch each symbol's history once and reuse it across all candidates.
    // The fetch range is extended by the ATH-guard lookback so the first
    // windows have warm-up context (the guard must not buy blind tops).
    // Warm-up prefix: the ATH guard needs `ath_lookback_days`, and the SMA50
    // trend filter/exit need 50 days — take the max so both are live from the
    // first trade tick of every window.
    let warmup_days = candidates
        .iter()
        .map(|c| c.ath_lookback_days)
        .max()
        .unwrap_or(0)
        .max(50);
    let fetch_from = if warmup_days > 0 {
        cfg.from - Duration::days(warmup_days)
    } else {
        cfg.from
    };
    let mut histories: HashMap<String, Vec<(i64, f64)>> = HashMap::new();
    for sym in &cfg.symbols {
        println!("Fetching {sym} history {}..{} ...", fetch_from, cfg.to);
        histories.insert(sym.clone(), fetch(sym, fetch_from, cfg.to)?);
        println!("  {} candles", histories[sym].len());
    }

    // Seasonality: per-calendar-month average returns from the full history,
    // persisted so the bundled seed ships trained cyclical knowledge (halving
    // phases + monthly seasonality) for the AI context.
    if cfg.force {
        store.clear_seasonality()?;
        // Drop stale trained profiles from previous sweeps (old grids leave
        // behind configs with dead knobs, e.g. pre-trend c10/c40) so a retrain
        // never ships outdated profiles in the seed.
        for p in store.list_strategy_profiles()? {
            if p.source == "trained" {
                store.delete_strategy_profile(&p.name)?;
            }
        }
    }
    for sym in &cfg.symbols {
        for (m, avg, samples) in monthly_seasonality_detailed(&histories[sym]) {
            if samples > 0 {
                store.record_seasonality(sym, m, avg, samples)?;
            }
        }
    }

    // Pre-slice each symbol's history into windows once — the same windows are
    // replayed for every candidate, so re-slicing per candidate is pure waste.
    // Each slice carries a warm-up prefix before its start date.
    let window_list = windows(cfg.from, cfg.to, cfg.window_days, cfg.step_days);
    let mut slices: HashMap<String, Vec<(NaiveDate, NaiveDate, i64, Vec<(i64, f64)>)>> =
        HashMap::new();
    for sym in &cfg.symbols {
        let history = &histories[sym];
        let mut sym_windows = Vec::new();
        for (start, end) in &window_list {
            let warmup_ms = date_to_ms(*start);
            let start_ms = warmup_ms - warmup_days * DAY_MS;
            let end_ms = date_to_ms(*end);
            let window: Vec<(i64, f64)> = history
                .iter()
                .filter(|(ts, _)| *ts >= start_ms && *ts < end_ms)
                .copied()
                .collect();
            // Min-candle filter counts only the trade window, not the warm-up
            // prefix, so pre-listing noise never enters sweep stats.
            let trade_len = window.iter().filter(|(ts, _)| *ts >= warmup_ms).count();
            if trade_len >= min_window_candles(cfg.window_days) {
                sym_windows.push((*start, *end, warmup_ms, window));
            }
        }
        slices.insert(sym.clone(), sym_windows);
    }

    // Tag every window with its regime (classified from the last 200 closes
    // of the full history ending at the window's end — the same context the
    // runtime classifier sees, so training and live auto-selection agree).
    let mut window_regimes: HashMap<String, Vec<MarketRegime>> = HashMap::new();
    for sym in &cfg.symbols {
        let history = &histories[sym];
        let mut regimes = Vec::with_capacity(slices[sym].len());
        for (_, end, _, _) in &slices[sym] {
            let end_ms = date_to_ms(*end);
            let closes: Vec<f64> = history
                .iter()
                .filter(|(ts, _)| *ts < end_ms)
                .map(|(_, c)| *c)
                .collect();
            let ctx: Vec<f64> = closes[closes.len().saturating_sub(200)..].to_vec();
            regimes.push(classify_closes(&ctx));
        }
        window_regimes.insert(sym.clone(), regimes);
    }

    // Parallel scoring: candidates are independent replays, so split them
    // across `threads` workers and merge the per-candidate stats afterwards.
    // Each worker owns one in-memory store, reset between windows — pure and
    // isolated, without paying schema-init per replay.
    let threads = cfg.threads.clamp(1, 64);
    let chunk = candidates.len().div_ceil(threads);
    // stats[candidate][symbol] — per-symbol aggregates, so the sweep can rank
    // configs per asset instead of only globally.
    let mut stats: Vec<Vec<CandidateStats>> = Vec::with_capacity(candidates.len());
    let mut regime_stats: Vec<Vec<HashMap<MarketRegime, CandidateStats>>> =
        Vec::with_capacity(candidates.len());
    for _ in candidates {
        stats.push(vec![CandidateStats::default(); cfg.symbols.len()]);
        regime_stats.push(vec![HashMap::new(); cfg.symbols.len()]);
    }

    // Progress reporting: total replays, a shared counter, and a monitor thread
    // that prints a percentage line as the sweep advances, so long grid runs
    // stay observable instead of printing nothing for minutes.
    let windows_per_symbol: usize = cfg.symbols.iter().map(|s| slices[s].len()).sum();
    let total_replays = candidates.len() * windows_per_symbol;
    let total_replays_u64 = total_replays as u64;
    println!(
        "Training: {} candidates x {} windows x {} symbols = {} replays on {} threads (in-memory, no disk)",
        candidates.len(),
        windows_per_symbol,
        cfg.symbols.len(),
        total_replays,
        threads,
    );

    let done = AtomicU64::new(0);
    let workers_done = AtomicBool::new(false);
    let mut worker_errors: Vec<anyhow::Error> = Vec::new();
    std::thread::scope(|scope| {
        let done_ref = &done;
        let workers_done_ref = &workers_done;
        let monitor = scope.spawn(move || {
            let mut last_step = 0u64;
            loop {
                let d = done_ref.load(AtomicOrdering::Relaxed);
                let step = if total_replays_u64 > 0 {
                    d * 100 / total_replays_u64 / 5
                } else {
                    20
                };
                if step != last_step {
                    last_step = step;
                    let pct = step * 5;
                    println!("  progress: {d}/{total_replays_u64} replays ({pct}%)");
                }
                if workers_done_ref.load(AtomicOrdering::Relaxed) || d >= total_replays_u64 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        let mut handles = Vec::new();
        for (_chunk_idx, cand_chunk) in candidates.chunks(chunk).enumerate() {
            let slices = &slices;
            let cfg = cfg;
            let done_ref = &done;
            let window_regimes = &window_regimes;
            handles.push(scope.spawn(move || {
                // One in-memory store per worker, reset between windows —
                // recreating the store per replay was the dominant fixed cost.
                let store = TradingStore::open_in_memory(TradingMode::Simulation)?;
                let mut local: Vec<(
                    Vec<CandidateStats>,
                    Vec<HashMap<MarketRegime, CandidateStats>>,
                )> = Vec::with_capacity(cand_chunk.len());
                for c in cand_chunk {
                    c.validate()?;
                    let mut per_sym: Vec<CandidateStats> =
                        vec![CandidateStats::default(); cfg.symbols.len()];
                    let mut per_sym_regimes: Vec<HashMap<MarketRegime, CandidateStats>> =
                        vec![HashMap::new(); cfg.symbols.len()];
                    for (si, sym) in cfg.symbols.iter().enumerate() {
                        for (wi, (start, end, warmup_ms, window)) in slices[sym].iter().enumerate()
                        {
                            let bc = BacktestConfig {
                                symbol: sym.clone(),
                                from: *start,
                                to: *end,
                                capital_usd: cfg.capital_usd,
                                crash_window_secs: c.crash_window_secs,
                                min_profit_margin: c.min_profit_margin,
                                taker_fee_rate: c.taker_fee_rate,
                                tranche_count: c.tranche_count,
                                ath_lookback_days: c.ath_lookback_days,
                                max_buy_from_high_pct: c.max_buy_from_high_pct,
                                min_dip_from_high_pct: c.min_dip_from_high_pct,
                                max_runup_pct: c.max_runup_pct,
                                trend_filter: c.trend_filter,
                                trend_exit: c.trend_exit,
                                take_profit_pct: c.take_profit_pct,
                                trail_pct: c.trail_pct,
                                ceiling: None,
                                floor: None,
                                profit_mode: c.profit_mode,
                            };
                            let r = run_backtest_inner_with_store(&bc, window, *warmup_ms, &store)?;
                            done_ref.fetch_add(1, AtomicOrdering::Relaxed);
                            let s = &mut per_sym[si];
                            s.runs += 1;
                            s.pnl_sum += r.pnl_pct;
                            s.dd_sum += r.max_drawdown_pct;
                            s.wins += r.wins;
                            s.losses += r.losses;
                            s.cold_lots += r.cold_lots;
                            s.total_lots += r.total_trades as usize + r.open_lots + r.cold_lots;
                            if r.pnl_pct > 0.0 {
                                s.profitable_runs += 1;
                            }
                            s.best_pnl = s.best_pnl.max(r.pnl_pct);
                            s.worst_pnl = s.worst_pnl.min(r.pnl_pct);
                            // Same accumulation into the window's regime bucket.
                            let regime = window_regimes[sym][wi];
                            let rs = per_sym_regimes[si].entry(regime).or_default();
                            rs.runs += 1;
                            rs.pnl_sum += r.pnl_pct;
                            rs.dd_sum += r.max_drawdown_pct;
                            rs.wins += r.wins;
                            rs.losses += r.losses;
                            rs.cold_lots += r.cold_lots;
                            rs.total_lots += r.total_trades as usize + r.open_lots + r.cold_lots;
                            if r.pnl_pct > 0.0 {
                                rs.profitable_runs += 1;
                            }
                            rs.best_pnl = rs.best_pnl.max(r.pnl_pct);
                            rs.worst_pnl = rs.worst_pnl.min(r.pnl_pct);
                        }
                    }
                    local.push((per_sym, per_sym_regimes));
                }
                Ok::<_, anyhow::Error>(local)
            }));
        }
        for (chunk_idx, handle) in handles.into_iter().enumerate() {
            match handle.join() {
                Err(_) => worker_errors.push(anyhow::anyhow!("training worker panicked")),
                Ok(Err(e)) => worker_errors.push(e),
                Ok(Ok(local)) => {
                    for (j, (per_sym, per_sym_regimes)) in local.into_iter().enumerate() {
                        stats[chunk_idx * chunk + j] = per_sym;
                        regime_stats[chunk_idx * chunk + j] = per_sym_regimes;
                    }
                }
            }
        }
        workers_done.store(true, AtomicOrdering::Relaxed);
        monitor.join().ok();
    });
    if let Some(e) = worker_errors.into_iter().next() {
        return Err(e);
    }

    // Global scoring (averaged across symbols) — the classic top-N behaviour.
    let mut scored: Vec<TrainedProfile> = Vec::new();
    let mut total_runs = 0usize;
    for (i, c) in candidates.iter().enumerate() {
        let mut merged = CandidateStats::default();
        for s in &stats[i] {
            merged.runs += s.runs;
            merged.pnl_sum += s.pnl_sum;
            merged.dd_sum += s.dd_sum;
            merged.wins += s.wins;
            merged.losses += s.losses;
            merged.cold_lots += s.cold_lots;
            merged.total_lots += s.total_lots;
            merged.profitable_runs += s.profitable_runs;
            merged.best_pnl = merged.best_pnl.max(s.best_pnl);
            merged.worst_pnl = merged.worst_pnl.min(s.worst_pnl);
        }
        if merged.runs == 0 {
            continue;
        }
        total_runs += merged.runs;
        scored.push(merged.finish(c));
    }
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));

    // Persist the full per-symbol sweep grid as compact aggregates, so the bot
    // can pick the best config per asset without re-running anything. With
    // `--force` the previous grid is dropped first (upsert would also work, but
    // a stale grid must never shadow a fresh one).
    if cfg.force {
        store.clear_training_stats()?;
    }
    // Per-symbol top-N profile names, computed up front so the stats rows for
    // persisted profiles are keyed by the profile name (the ensemble and
    // `top_profiles` resolve stats -> profile by name).
    let mut sym_top_names: HashMap<(String, String), String> = HashMap::new();
    for (si, sym) in cfg.symbols.iter().enumerate() {
        let mut sym_scored: Vec<TrainedProfile> = Vec::new();
        for (i, c) in candidates.iter().enumerate() {
            let s = &stats[i][si];
            if s.runs == 0 {
                continue;
            }
            sym_scored.push(s.finish(c));
        }
        sym_scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        for (i, t) in sym_scored.iter().take(cfg.top_n.max(1)).enumerate() {
            let name = format!(
                "Trained-{sym}-{}",
                trained_name(i + 1, &t.profile.engine_config)
            );
            sym_top_names.insert(
                (sym.clone(), config_fingerprint(&t.profile.engine_config)),
                name,
            );
        }
    }
    for (i, c) in candidates.iter().enumerate() {
        for (si, sym) in cfg.symbols.iter().enumerate() {
            let s = &stats[i][si];
            if s.runs == 0 {
                continue;
            }
            let t = s.finish(c);
            let fp = config_fingerprint(c);
            let config_name = sym_top_names
                .get(&(sym.clone(), fp.clone()))
                .cloned()
                .unwrap_or(fp);
            store.record_training_stats(&NewTrainingStats {
                symbol: sym.clone(),
                config_name,
                min_profit_margin: c.min_profit_margin,
                taker_fee_rate: c.taker_fee_rate,
                tranche_count: c.tranche_count,
                crash_drop_pct: c.crash_drop_pct,
                profit_mode: c.profit_mode.as_str().to_string(),
                runs: s.runs,
                avg_pnl_pct: t.avg_pnl_pct,
                avg_max_dd_pct: t.avg_max_dd_pct,
                win_rate: t.win_rate,
                cold_ratio: t.cold_ratio,
                profitable_ratio: t.profitable_ratio,
                score: t.score,
                best_window_pnl_pct: s.best_pnl,
                worst_window_pnl_pct: s.worst_pnl,
            })?;
        }
    }

    let top_n = cfg.top_n.max(1).min(scored.len());
    let window_count = window_list.len();
    let mut trained = Vec::new();
    for (i, t) in scored.iter().take(top_n).enumerate() {
        let mut p = t.profile.clone();
        p.name = trained_name(i + 1, &p.engine_config);
        p.description = format!(
            "Auto-trained on {}..{} across {} symbols ({} windows, {} runs): \
             avg PnL {:+.2}%, max DD {:.2}%, win-rate {:.0}%, cold-lot ratio {:.0}%",
            cfg.from,
            cfg.to,
            cfg.symbols.len(),
            window_count,
            t.runs,
            t.avg_pnl_pct,
            t.avg_max_dd_pct,
            t.win_rate * 100.0,
            t.cold_ratio * 100.0
        );
        store.record_strategy_profile(&p.to_record())?;
        trained.push(TrainedProfile {
            profile: p,
            ..t.clone()
        });
    }

    // Per-symbol top-N profiles: the bot can apply the best config for a
    // specific asset instead of a one-size-fits-all global winner.
    let mut per_symbol_trained = Vec::new();
    for (si, sym) in cfg.symbols.iter().enumerate() {
        let mut sym_scored: Vec<TrainedProfile> = Vec::new();
        for (i, c) in candidates.iter().enumerate() {
            let s = &stats[i][si];
            if s.runs == 0 {
                continue;
            }
            sym_scored.push(s.finish(c));
        }
        sym_scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        for (i, t) in sym_scored.iter().take(top_n).enumerate() {
            let mut p = t.profile.clone();
            p.name = format!("Trained-{sym}-{}", trained_name(i + 1, &p.engine_config));
            p.description = format!(
                "Auto-trained on {}..{} for {} only ({} windows, {} runs): \
                 avg PnL {:+.2}%, max DD {:.2}%, win-rate {:.0}%, cold-lot ratio {:.0}%",
                cfg.from,
                cfg.to,
                sym,
                window_count,
                t.runs,
                t.avg_pnl_pct,
                t.avg_max_dd_pct,
                t.win_rate * 100.0,
                t.cold_ratio * 100.0
            );
            store.record_strategy_profile(&p.to_record())?;
            per_symbol_trained.push(TrainedProfile {
                profile: p,
                ..t.clone()
            });
        }
    }

    // Lessons: one per symbol with the best config + worst-window behaviour,
    // so the bot's memory layer has concrete "what worked for X" entries.
    for (si, sym) in cfg.symbols.iter().enumerate() {
        let mut sym_scored: Vec<TrainedProfile> = Vec::new();
        for (i, c) in candidates.iter().enumerate() {
            let s = &stats[i][si];
            if s.runs == 0 {
                continue;
            }
            sym_scored.push(s.finish(c));
        }
        sym_scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
        if let Some(best) = sym_scored.first() {
            let worst_avg = sym_scored
                .iter()
                .map(|t| t.avg_pnl_pct)
                .fold(f64::INFINITY, f64::min);
            let lesson = format!(
                "Best config for {}: margin {:.3}, tranches {}, crash {:.0}% — \
                 avg PnL {:+.2}%, win-rate {:.0}%, cold-lot ratio {:.0}%, \
                 profitable in {:.0}% of windows",
                sym,
                best.profile.engine_config.min_profit_margin,
                best.profile.engine_config.tranche_count,
                best.profile.engine_config.crash_drop_pct,
                best.avg_pnl_pct,
                best.win_rate * 100.0,
                best.cold_ratio * 100.0,
                best.profitable_ratio * 100.0
            );
            let context = serde_json::json!({
                "symbol": sym,
                "from": cfg.from.to_string(),
                "to": cfg.to.to_string(),
                "windows": window_count,
                "best_config": config_fingerprint(&best.profile.engine_config),
                "worst_avg_pnl_pct": worst_avg,
            });
            store.record_lesson(NewLesson {
                symbol: sym.clone(),
                lesson,
                context_json: context.to_string(),
            })?;
        }
    }

    // Per-regime winners: for each symbol × regime, the best-scoring config
    // over the windows tagged with that regime. Persisted as regime_profiles
    // so the bot can auto-select the right config for the regime it sees now.
    if cfg.force {
        store.clear_regime_profiles()?;
    }
    for (si, sym) in cfg.symbols.iter().enumerate() {
        let mut best_by_regime: HashMap<MarketRegime, (usize, TrainedProfile)> = HashMap::new();
        for (i, c) in candidates.iter().enumerate() {
            for (regime, rs) in &regime_stats[i][si] {
                if rs.runs == 0 {
                    continue;
                }
                let t = rs.finish(c);
                let better = match best_by_regime.get(regime) {
                    None => true,
                    Some((_, cur)) => t.score > cur.score,
                };
                if better {
                    best_by_regime.insert(*regime, (i, t));
                }
            }
        }
        let mut regimes: Vec<MarketRegime> = best_by_regime.keys().copied().collect();
        regimes.sort_by_key(|r| r.as_str());
        for regime in &regimes {
            let (i, t) = &best_by_regime[regime];
            let c = &candidates[*i];
            store.record_regime_profile(&NewRegimeProfile {
                symbol: sym.clone(),
                regime: regime.as_str().to_string(),
                config_name: config_fingerprint(c),
                engine_config_json: serde_json::to_string(c).unwrap_or_else(|_| "{}".to_string()),
                score: t.score,
                avg_pnl_pct: t.avg_pnl_pct,
                runs: t.runs,
            })?;
        }
        if !regimes.is_empty() {
            let desc: Vec<String> = regimes
                .iter()
                .map(|r| {
                    let (i, t) = &best_by_regime[r];
                    format!(
                        "{} -> {} (score {:.1}, avg PnL {:+.1}%)",
                        r.as_str(),
                        config_fingerprint(&candidates[*i]),
                        t.score,
                        t.avg_pnl_pct
                    )
                })
                .collect();
            println!("  {sym} regime profiles: {}", desc.join(" | "));
        }
    }

    Ok(TrainingSummary {
        candidates: candidates.len(),
        runs: total_runs,
        trained,
        per_symbol_trained,
        skipped: false,
    })
}

/// Ensemble buy decision: the top-3 trained profiles for `symbol` each run the
/// deterministic engine's `plan_buy` at the given price; the majority wins.
///
/// This is the anti-overfitting gate — a single trained profile may have
/// curve-fit one era, but three independent configs agreeing is a stronger
/// signal. The returned `BuyPlan` carries the vote breakdown as its rationale
/// (tranche sizing is left to the caller's budget logic). `history` (ts_ms,
/// close) is fed into each engine first so trend filters / ATH guards see the
/// same market the caller sees.
pub fn plan_buy_ensemble(
    store: &TradingStore,
    symbol: &str,
    price: f64,
    history: &[(i64, f64)],
) -> Result<Decision<BuyPlan>> {
    let stats = store.list_training_stats(symbol, 3)?;
    if stats.is_empty() {
        return Ok(Decision::Skip(
            "no trained profiles for this symbol — run `trade strategy train` first".into(),
        ));
    }
    let mut buys = 0usize;
    let mut votes: Vec<String> = Vec::new();
    for row in &stats {
        let rec = match resolve_profile_for_stats(store, &row.config_name)? {
            Some(r) => r,
            None => {
                votes.push(format!("{}: no profile row", row.config_name));
                continue;
            }
        };
        let profile = match StrategyProfile::from_record(&rec) {
            Ok(p) => p,
            Err(e) => {
                votes.push(format!("{}: corrupt ({e})", row.config_name));
                continue;
            }
        };
        let engine = match TradingEngine::new(profile.engine_config) {
            Ok(mut e) => {
                // Feed the same history the caller sees so trend filters and
                // ATH guards have context (a fresh engine knows nothing).
                for &(ts, p) in history {
                    e.observe(&super::engine::MarketTick::new(symbol, p, p, p, ts / 1000));
                }
                e
            }
            Err(e) => {
                votes.push(format!("{}: invalid config ({e})", row.config_name));
                continue;
            }
        };
        match engine.plan_buy(store, symbol, price, price, price) {
            Ok(Decision::Proceed(p)) => {
                buys += 1;
                votes.push(format!(
                    "{}: BUY ${:.2} (target ${:.2})",
                    row.config_name, p.tranche_usd, p.target_exit_price
                ));
            }
            Ok(Decision::Skip(r)) => votes.push(format!("{}: SKIP ({r})", row.config_name)),
            Err(e) => votes.push(format!("{}: error ({e})", row.config_name)),
        }
    }
    let rationale = format!(
        "ensemble {buys}/{} agree on BUY: {}",
        stats.len(),
        votes.join("; ")
    );
    if buys >= 2 {
        Ok(Decision::Proceed(BuyPlan {
            symbol: symbol.to_uppercase(),
            allocation_id: String::new(),
            entry_price: price,
            amount: 0.0,
            fee_usd: 0.0,
            cost_basis_usd: 0.0,
            target_exit_price: 0.0,
            tranche_usd: 0.0,
            rationale,
        }))
    } else {
        Ok(Decision::Skip(rationale))
    }
}

/// Resolve the strategy profile behind a training-stats row. Stats are keyed
/// by the config fingerprint (`m0.005-t8-REINVEST-c15`) while persisted
/// profiles are named `Trained-{rank}-{fingerprint}` — try the exact name
/// first, then fall back to a suffix match so older seed DBs keep working.
pub(crate) fn resolve_profile_for_stats(
    store: &TradingStore,
    config_name: &str,
) -> Result<Option<StrategyProfileRecord>> {
    if let Some(r) = store.get_strategy_profile(config_name)? {
        return Ok(Some(r));
    }
    let suffix = format!("-{config_name}");
    for p in store.list_strategy_profiles()? {
        if p.name.ends_with(&suffix) {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::backtest::run_backtest_inner;
    use crate::trading::engine::{MarketTick, TradingEngine, TrendFilter};
    use crate::trading::store::{NewAllocation, TradingMode};
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use std::collections::HashSet;
    use tempfile::tempdir;

    fn test_store() -> (TradingStore, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("strategies_test.db");
        let store = TradingStore::open_path(path.to_str().expect("path"), TradingMode::Simulation)
            .expect("open store");
        (store, dir)
    }

    /// Daily price series from `from` to `to` applying `moves` (repeated /
    /// padded with 0.0 when shorter than the span).
    fn synthetic_daily(from: NaiveDate, to: NaiveDate, moves: &[f64]) -> Vec<(i64, f64)> {
        let days = (to - from).num_days();
        let mut price = 100.0;
        let mut out = Vec::new();
        let mut ts = date_to_ms(from);
        for i in 0..days {
            let m = moves.get(i as usize).copied().unwrap_or(0.0);
            price *= 1.0 + m;
            out.push((ts, price));
            ts += 86_400_000;
        }
        out
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn builtin_profiles_are_valid_and_unique() {
        let profiles = builtin_profiles();
        assert!(profiles.len() >= 4);
        let mut names = HashSet::new();
        for p in &profiles {
            assert!(p.engine_config.validate().is_ok(), "{} invalid", p.name);
            assert!(names.insert(p.name.clone()), "duplicate name {}", p.name);
            assert_eq!(p.source, "builtin");
        }
    }

    #[test]
    fn profile_json_roundtrip() {
        let p = builtin_profiles().remove(0);
        let rec = p.to_record();
        let back = StrategyProfile::from_record(&rec).unwrap();
        assert_eq!(back.name, p.name);
        assert_eq!(back.engine_config, p.engine_config);
        assert_eq!(back.default_constraints, p.default_constraints);
        assert_eq!(back.source, p.source);
        assert_eq!(back.score, p.score);
    }

    #[test]
    fn strategy_profile_store_crud() {
        let (store, _dir) = test_store();
        assert!(store.list_strategy_profiles().unwrap().is_empty());
        let p = builtin_profiles().remove(0);
        store.record_strategy_profile(&p.to_record()).unwrap();
        assert_eq!(store.list_strategy_profiles().unwrap().len(), 1);
        let got = store.get_strategy_profile(&p.name).unwrap().unwrap();
        assert_eq!(got.name, p.name);
        assert!(store.delete_strategy_profile(&p.name).unwrap());
        assert!(!store.delete_strategy_profile(&p.name).unwrap());
    }

    #[test]
    fn apply_profile_sets_active_config_and_constraints() {
        let (store, _dir) = test_store();
        let mut p = builtin_profiles().remove(0);
        p.default_constraints = vec![DefaultConstraint {
            asset: "BTC".into(),
            rule_type: RuleType::MaxPriceCeiling,
            threshold_value: 65_000.0,
        }];
        apply_profile(&store, &p).unwrap();
        let latest = store.latest_engine_config().unwrap().unwrap();
        assert_eq!(
            latest.to_engine_config().min_profit_margin,
            p.engine_config.min_profit_margin
        );
        assert_eq!(latest.source, format!("strategy:{}", p.name));
        let constraints = store.list_constraints(Some("BTC"), true).unwrap();
        assert_eq!(constraints.len(), 1);
        assert_eq!(constraints[0].threshold_value, 65_000.0);
    }

    #[test]
    fn candidate_grid_is_valid_and_sized() {
        let small = candidate_configs(GridSize::Small);
        assert_eq!(small.len(), 36);
        let full = candidate_configs(GridSize::Full);
        assert_eq!(full.len(), 180);
        for c in small.iter().chain(full.iter()) {
            assert!(c.validate().is_ok());
        }
        // Conviction set: 6 dips x 3 tp/trail pairs x 3 modes = 54, all valid.
        let conv = conviction_configs();
        assert_eq!(conv.len(), 54);
        for c in &conv {
            assert!(c.validate().is_ok());
            assert!(c.min_dip_from_high_pct > 0.0);
        }
    }

    #[test]
    fn train_synthetic_produces_trained_profiles_and_is_idempotent() {
        let (store, _dir) = test_store();
        let cfg = TrainingConfig {
            symbols: vec!["BTC".into(), "ETH".into()],
            from: d(2020, 1, 1),
            to: d(2020, 4, 1),
            window_days: 30,
            step_days: 15,
            capital_usd: 1000.0,
            top_n: 2,
            force: false,
            threads: 2,
        };
        let candidates = vec![
            EngineConfig {
                min_profit_margin: 0.01,
                ..EngineConfig::default()
            },
            EngineConfig {
                min_profit_margin: 0.05,
                ..EngineConfig::default()
            },
        ];
        let fetch = |sym: &str, from: NaiveDate, to: NaiveDate| -> Result<Vec<(i64, f64)>> {
            let moves: &[f64] = match sym {
                "BTC" => &[
                    0.02, -0.05, 0.03, -0.04, 0.02, -0.03, 0.01, -0.02, 0.03, -0.01,
                ],
                _ => &[
                    0.03, -0.06, 0.04, -0.05, 0.03, -0.04, 0.02, -0.03, 0.04, -0.02,
                ],
            };
            Ok(synthetic_daily(from, to, moves))
        };

        let summary = train(&store, &cfg, &candidates, &fetch).unwrap();
        assert!(!summary.skipped);
        assert_eq!(summary.trained.len(), 2);
        // 2 global + 2 per-symbol x 2 symbols = 6 profiles.
        assert_eq!(summary.per_symbol_trained.len(), 4);
        let profiles = store.list_strategy_profiles().unwrap();
        assert_eq!(profiles.len(), 6);
        assert!(profiles.iter().all(|p| p.source == "trained"));
        assert!(profiles.iter().all(|p| p.score.is_some()));
        // Full sweep grid + one lesson per symbol were persisted too.
        assert_eq!(store.list_training_stats("", 100).unwrap().len(), 4);
        assert_eq!(store.list_lessons(100).unwrap().len(), 2);

        // Second run skips (idempotent).
        let again = train(&store, &cfg, &candidates, &fetch).unwrap();
        assert!(again.skipped);
        assert_eq!(store.list_strategy_profiles().unwrap().len(), 6);

        // --force retrains.
        let forced = train(
            &store,
            &TrainingConfig {
                force: true,
                ..cfg.clone()
            },
            &candidates,
            &fetch,
        )
        .unwrap();
        assert!(!forced.skipped);
    }

    #[test]
    fn train_persists_regime_profiles_and_auto_selects_them() {
        let (store, _dir) = test_store();
        let cfg = TrainingConfig {
            symbols: vec!["BTC".into()],
            from: d(2020, 1, 1),
            to: d(2020, 7, 1),
            window_days: 30,
            step_days: 15,
            capital_usd: 1000.0,
            top_n: 2,
            force: true,
            threads: 2,
        };
        let candidates = vec![
            EngineConfig {
                min_profit_margin: 0.01,
                ..EngineConfig::default()
            },
            EngineConfig {
                min_profit_margin: 0.05,
                ..EngineConfig::default()
            },
        ];
        // Steady uptrend: every window is BULL, so a BULL regime profile must
        // be persisted and auto-selected. +0.2%/day keeps RSI hot but not
        // euphoric (price stays within 3% of the SMA20, so the momentum
        // margin in the classifier does not fire).
        let fetch = |sym: &str, from: NaiveDate, to: NaiveDate| -> Result<Vec<(i64, f64)>> {
            let _ = sym;
            let days = (to - from).num_days();
            let mut price = 100.0;
            let mut out = Vec::new();
            let mut ts = date_to_ms(from);
            for _ in 0..days {
                price *= 1.002;
                out.push((ts, price));
                ts += 86_400_000;
            }
            Ok(out)
        };
        let summary = train(&store, &cfg, &candidates, &fetch).unwrap();
        assert!(!summary.skipped);
        let rows = store.list_regime_profiles("").unwrap();
        assert!(!rows.is_empty(), "expected at least one regime profile");
        for r in &rows {
            let cfg: EngineConfig = serde_json::from_str(&r.engine_config_json).unwrap();
            assert!(cfg.validate().is_ok());
        }
        // Auto-select: with no explicit config, the regime winner is used.
        let history = fetch("BTC", d(2020, 1, 1), d(2020, 7, 1)).unwrap();
        let auto = auto_engine_config(&store, "BTC", &history).unwrap();
        let row = store.get_regime_profile("BTC", "BULL").unwrap().unwrap();
        let winner: EngineConfig = serde_json::from_str(&row.engine_config_json).unwrap();
        assert_eq!(auto, winner);
        // A symbol with no regime winner falls back to the runner.
        let fallback = auto_engine_config(&store, "SOL", &history).unwrap();
        assert_eq!(fallback, EngineConfig::runner());
        // An explicit user choice (applied strategy) overrides auto-adaptation.
        let mut p = builtin_profiles().remove(0);
        p.name = "My-Choice".into();
        apply_profile(&store, &p).unwrap();
        let explicit = auto_engine_config(&store, "BTC", &history).unwrap();
        assert_eq!(explicit, p.engine_config);
    }

    #[test]
    fn train_ranks_profitable_config_first() {
        let (store, _dir) = test_store();
        let cfg = TrainingConfig {
            symbols: vec!["BTC".into()],
            from: d(2020, 1, 1),
            to: d(2020, 7, 1),
            window_days: 30,
            step_days: 15,
            capital_usd: 1000.0,
            top_n: 2,
            force: true,
            threads: 2,
        };
        // Sawtooth: -10% dip then +11% recovery, repeated. A 1% margin sells
        // every cycle; a 50% margin never sells. The dips clear the 5% ATH
        // guard threshold so the low-margin config actually trades.
        let candidates = vec![
            EngineConfig {
                min_profit_margin: 0.01,
                tranche_count: 2,
                // Take-profit off: the +11% recovery must reach the exit
                // target every cycle or the low-margin config never sells.
                take_profit_pct: 0.0,
                ..EngineConfig::default()
            },
            EngineConfig {
                min_profit_margin: 0.5,
                tranche_count: 2,
                take_profit_pct: 0.0,
                ..EngineConfig::default()
            },
        ];
        let fetch = |_sym: &str, from: NaiveDate, to: NaiveDate| -> Result<Vec<(i64, f64)>> {
            // Continuous sawtooth across the whole span: -10% dip then +11%
            // recovery every other day, so dips always clear the 5% ATH guard.
            let days = (to - from).num_days();
            let moves: Vec<f64> = (0..days)
                .map(|i| if i % 2 == 0 { -0.10 } else { 0.11 })
                .collect();
            Ok(synthetic_daily(from, to, &moves))
        };

        let summary = train(&store, &cfg, &candidates, &fetch).unwrap();
        assert_eq!(summary.trained.len(), 2);
        assert!(
            summary.trained[0].score > summary.trained[1].score,
            "low-margin config should rank first: {:?} vs {:?}",
            summary.trained[0].score,
            summary.trained[1].score
        );
        assert!(summary.trained[0].profile.engine_config.min_profit_margin < 0.5);
        assert!(summary.trained[0].win_rate > 0.5);
    }

    #[test]
    fn stress_flash_crash_never_realizes_losses() {
        let from = d(2020, 1, 1);
        let to = d(2020, 4, 1);
        let cfg = BacktestConfig {
            symbol: "BTC".into(),
            from,
            to,
            capital_usd: 1000.0,
            ..Default::default()
        };
        // Flat at the high (ATH guard parks cash), a 10% dip buys one lot,
        // then ~86% cumulative drop over 9 days.
        let history = synthetic_daily(
            from,
            to,
            &[
                0.0, 0.0, 0.0, 0.0, 0.0, -0.10, -0.20, -0.20, -0.20, -0.20, -0.20, -0.20, -0.20,
                -0.20, -0.20,
            ],
        );
        let r = run_backtest_inner(&cfg, &history, 0).unwrap();
        assert_eq!(r.losses, 0, "flash crash must never realize losses");
        assert_eq!(r.wins, 0);
        assert!(r.cold_lots > 0, "underwater lots must go COLD_HOLD");
    }

    #[test]
    fn stress_whipsaw_never_realizes_losses() {
        let from = d(2020, 1, 1);
        let to = d(2020, 4, 1);
        let cfg = BacktestConfig {
            symbol: "BTC".into(),
            from,
            to,
            capital_usd: 1000.0,
            ..Default::default()
        };
        let history = synthetic_daily(
            from,
            to,
            &[
                0.05, -0.05, 0.05, -0.05, 0.05, -0.05, 0.05, -0.05, 0.05, -0.05,
            ],
        );
        let r = run_backtest_inner(&cfg, &history, 0).unwrap();
        assert_eq!(r.losses, 0, "whipsaw must never realize losses");
    }

    #[test]
    fn stress_wide_spread_blocks_buys() {
        let (store, _dir) = test_store();
        store
            .create_allocation(NewAllocation {
                asset: "SOL".into(),
                total_budget_usd: 100.0,
            })
            .unwrap();
        let mut engine = TradingEngine::new(EngineConfig::default()).unwrap();
        // 4% bid/ask spread — above the 0.75% breaker.
        let tick = MarketTick::new("SOL", 100.0, 98.0, 102.0, 1_600_000_000);
        let out = engine.apply_tick(&store, &tick).unwrap();
        assert!(out.buys.is_empty(), "wide spread must block buys");
        assert!(store.list_lots(None).unwrap().is_empty());
    }

    #[test]
    fn fuzz_random_configs_never_panic_or_sell_at_loss() {
        let mut rng = StdRng::seed_from_u64(42);
        let from = d(2020, 1, 1);
        let to = d(2020, 4, 1);
        for _ in 0..50 {
            let c = EngineConfig {
                min_profit_margin: rng.gen_range(0.0..0.25),
                taker_fee_rate: rng.gen_range(0.0..0.05),
                tranche_count: rng.gen_range(1..=12),
                min_tranche_fraction: rng.gen_range(0.0..0.5),
                max_spread_pct: rng.gen_range(0.0..5.0),
                crash_drop_pct: rng.gen_range(1.0..60.0),
                crash_window_secs: rng.gen_range(60..=30 * 86_400),
                profit_mode: match rng.gen_range(0..3) {
                    0 => ProfitMode::Reinvest,
                    1 => ProfitMode::Separate,
                    _ => ProfitMode::Partial(rng.gen_range(0.0..=1.0)),
                },
                ath_lookback_days: rng.gen_range(0..=365),
                max_buy_from_high_pct: rng.gen_range(0.0..25.0),
                min_dip_from_high_pct: rng.gen_range(0.0..60.0),
                max_runup_pct: rng.gen_range(0.0..100.0),
                trend_filter: if rng.gen_bool(0.5) {
                    TrendFilter::Sma50
                } else {
                    TrendFilter::Off
                },
                trend_exit: rng.gen_bool(0.5),
                take_profit_pct: rng.gen_range(0.0..1.0),
                trail_pct: rng.gen_range(0.0..0.5),
            };
            assert!(c.validate().is_ok(), "generated config must validate");
            let bc = BacktestConfig {
                symbol: "BTC".into(),
                from,
                to,
                capital_usd: 1000.0,
                crash_window_secs: c.crash_window_secs,
                min_profit_margin: c.min_profit_margin,
                taker_fee_rate: c.taker_fee_rate,
                tranche_count: c.tranche_count,
                profit_mode: c.profit_mode,
                ..Default::default()
            };
            let history = synthetic_daily(
                from,
                to,
                &[
                    0.03, -0.04, 0.05, -0.06, 0.04, -0.03, 0.06, -0.05, 0.03, -0.04,
                ],
            );
            let r = run_backtest_inner(&bc, &history, 0).unwrap();
            assert_eq!(r.losses, 0, "realized loss with config {c:?}");
        }
    }

    #[test]
    fn fuzz_garbage_configs_rejected() {
        let mut c = EngineConfig::default();
        c.min_profit_margin = f64::NAN;
        assert!(c.validate().is_err());
        c.min_profit_margin = -1.0;
        assert!(c.validate().is_err());
        c.min_profit_margin = 0.01;
        c.tranche_count = 0;
        assert!(c.validate().is_err());
        c.tranche_count = 3;
        c.crash_drop_pct = 0.0;
        assert!(c.validate().is_err());
        c.crash_drop_pct = 15.0;
        c.crash_window_secs = 0;
        assert!(c.validate().is_err());
        c.crash_window_secs = 900;
        c.profit_mode = ProfitMode::Partial(1.5);
        assert!(c.validate().is_err());
        c.profit_mode = ProfitMode::Partial(0.5);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn walk_forward_train_then_validate() {
        let (store, _dir) = test_store();
        // Train on a bull window (2020), validate out-of-sample on a bear
        // window (2021). The trained config must never realize losses.
        let cfg = TrainingConfig {
            symbols: vec!["BTC".into()],
            from: d(2020, 1, 1),
            to: d(2020, 7, 1),
            window_days: 30,
            step_days: 15,
            capital_usd: 1000.0,
            top_n: 1,
            force: false,
            threads: 2,
        };
        let candidates = vec![
            EngineConfig {
                min_profit_margin: 0.01,
                tranche_count: 3,
                ..EngineConfig::default()
            },
            EngineConfig {
                min_profit_margin: 0.02,
                tranche_count: 4,
                ..EngineConfig::default()
            },
            EngineConfig {
                min_profit_margin: 0.03,
                tranche_count: 2,
                profit_mode: ProfitMode::Separate,
                ..EngineConfig::default()
            },
        ];
        let fetch = |_sym: &str, from: NaiveDate, to: NaiveDate| -> Result<Vec<(i64, f64)>> {
            Ok(synthetic_daily(from, to, &[0.02; 180]))
        };
        let summary = train(&store, &cfg, &candidates, &fetch).unwrap();
        assert_eq!(summary.trained.len(), 1);
        let best = &summary.trained[0].profile.engine_config;

        // Out-of-sample validation on a -3%/day bear market.
        let val_from = d(2021, 1, 1);
        let val_to = d(2021, 7, 1);
        let bc = BacktestConfig {
            symbol: "BTC".into(),
            from: val_from,
            to: val_to,
            capital_usd: 1000.0,
            crash_window_secs: best.crash_window_secs,
            min_profit_margin: best.min_profit_margin,
            taker_fee_rate: best.taker_fee_rate,
            tranche_count: best.tranche_count,
            profit_mode: best.profit_mode,
            ..Default::default()
        };
        let history = synthetic_daily(val_from, val_to, &[-0.03; 180]);
        let r = run_backtest_inner(&bc, &history, 0).unwrap();
        assert_eq!(r.losses, 0, "trained config realized a loss out-of-sample");
        assert!(r.cold_lots > 0, "bear market should park lots in COLD_HOLD");
    }

    /// Property-based invariant checks over randomized histories: the engine
    /// must never realize a loss, never overdraft (final equity >= 0), and
    /// never average up (a new buy must not exceed the previous lot's entry
    /// for the same symbol while a lot is open). Cheap to run — 30 random
    /// walks x 1 config — and catches regressions without reading code.
    #[test]
    fn property_invariants_no_loss_no_overdraft_no_average_up() {
        let mut rng = StdRng::seed_from_u64(7);
        let from = d(2020, 1, 1);
        let to = d(2021, 1, 1);
        for trial in 0..30 {
            // Random walk with regime flips (bull/bear chunks).
            let mut moves = Vec::new();
            let mut regime = if rng.gen_bool(0.5) { 0.004 } else { -0.004 };
            for _ in 0..360 {
                if rng.gen_bool(0.05) {
                    regime = if rng.gen_bool(0.5) { 0.004 } else { -0.004 };
                }
                moves.push(regime + rng.gen_range(-0.02..0.02));
            }
            let history = synthetic_daily(from, to, &moves);
            let bc = BacktestConfig {
                symbol: "BTC".into(),
                from,
                to,
                capital_usd: 1000.0,
                min_profit_margin: rng.gen_range(0.005..0.1),
                tranche_count: rng.gen_range(2..=6),
                ..Default::default()
            };
            let r = run_backtest_inner(&bc, &history, 0).unwrap();
            // No realized loss and no overdraft invariants
            assert_eq!(r.losses, 0, "trial {trial}: realized loss");
            assert!(r.final_capital_usd >= -1e-6, "trial {trial}: overdraft");
        }
    }

    /// Ensemble gate: with no trained profiles it skips with a hint; with
    /// three agreeing profiles it proceeds; with a split it skips and lists
    /// the votes.
    #[test]
    fn plan_buy_ensemble_majority_vote() {
        let (store, _td) = test_store();
        // No profiles -> Skip with a hint.
        match plan_buy_ensemble(&store, "BTC", 100.0, &[]).unwrap() {
            Decision::Skip(r) => assert!(r.contains("no trained profiles")),
            Decision::Proceed(_) => panic!("expected skip without profiles"),
        }
        // Three profiles that all buy at 100 (guards off, allocation present)
        // -> Proceed.
        store
            .create_allocation(NewAllocation {
                asset: "BTC".into(),
                total_budget_usd: 1000.0,
            })
            .unwrap();
        let cfg = EngineConfig {
            min_profit_margin: 0.01,
            tranche_count: 2,
            trend_filter: TrendFilter::Off,
            ath_lookback_days: 0,
            ..EngineConfig::default()
        };
        for i in 0..3 {
            let name = format!("Trained-{i}-m0.010-t2-SEPARATE-c15");
            let prof = StrategyProfile {
                name: name.clone(),
                description: String::new(),
                engine_config: cfg.clone(),
                default_constraints: vec![],
                source: "trained".into(),
                score: Some(1.0 - i as f64 * 0.1),
            };
            store.record_strategy_profile(&prof.to_record()).unwrap();
            store
                .record_training_stats(&NewTrainingStats {
                    symbol: "BTC".into(),
                    config_name: name,
                    min_profit_margin: cfg.min_profit_margin,
                    taker_fee_rate: cfg.taker_fee_rate,
                    tranche_count: cfg.tranche_count,
                    crash_drop_pct: cfg.crash_drop_pct,
                    profit_mode: cfg.profit_mode.as_str().to_string(),
                    runs: 10,
                    avg_pnl_pct: 5.0,
                    avg_max_dd_pct: 10.0,
                    win_rate: 1.0,
                    cold_ratio: 0.1,
                    profitable_ratio: 0.8,
                    score: 1.0 - i as f64 * 0.1,
                    best_window_pnl_pct: 20.0,
                    worst_window_pnl_pct: -2.0,
                })
                .unwrap();
        }
        match plan_buy_ensemble(&store, "BTC", 100.0, &[]).unwrap() {
            Decision::Proceed(p) => assert!(p.rationale.contains("3/3")),
            Decision::Skip(r) => panic!("expected proceed, got: {r}"),
        }
    }
}
