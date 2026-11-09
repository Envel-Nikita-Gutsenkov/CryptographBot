//! Phase 9: AI self-tuning loop ("the model learns, the engine executes").
//!
//! The deterministic engine ([`TradingEngine`]) decides *whether* to trade;
//! the tuner decides *the settings*. It summarizes recent backtest runs and
//! strategy lessons from the sim DB, asks an LLM for a JSON proposal of
//! `EngineConfig` changes, validates every field deterministically (bounds,
//! NaN, garbage), and persists the accepted config to `engine_configs`.
//!
//! Failure handling is strict: any API error, rate limit, malformed output or
//! out-of-bounds value falls back to the current config. The tuner NEVER
//! panics and NEVER applies garbage — the deterministic gate is the last word.

use anyhow::{bail, Result};
use serde::Deserialize;

use super::engine::{EngineConfig, ProfitMode, TrendFilter};
use super::store::{NewEngineConfig, TradingStore};

// ---------------------------------------------------------------------------
// Sane bounds for tuner proposals. Wider than the engine's hard validation so
// the model cannot set absurd values even when they are technically valid.
// ---------------------------------------------------------------------------

pub const MARGIN_MIN: f64 = 0.0;
pub const MARGIN_MAX: f64 = 0.25;
pub const FEE_MIN: f64 = 0.0;
pub const FEE_MAX: f64 = 0.05;
pub const TRANCHES_MIN: u32 = 1;
pub const TRANCHES_MAX: u32 = 12;
pub const MIN_TRANCHE_FRAC_MIN: f64 = 0.0;
pub const MIN_TRANCHE_FRAC_MAX: f64 = 0.5;
pub const SPREAD_MIN: f64 = 0.0;
pub const SPREAD_MAX: f64 = 5.0;
pub const CRASH_DROP_MIN: f64 = 1.0;
pub const CRASH_DROP_MAX: f64 = 60.0;
pub const CRASH_WINDOW_MIN: i64 = 60;
pub const CRASH_WINDOW_MAX: i64 = 30 * 86_400;
pub const ATH_LOOKBACK_MIN: i64 = 0;
pub const ATH_LOOKBACK_MAX: i64 = 365;
pub const MAX_BUY_FROM_HIGH_MIN: f64 = 0.0;
pub const MAX_BUY_FROM_HIGH_MAX: f64 = 25.0;
pub const MAX_RUNUP_MIN: f64 = 0.0;
pub const MAX_RUNUP_MAX: f64 = 100.0;
pub const TAKE_PROFIT_MIN: f64 = 0.0;
pub const TAKE_PROFIT_MAX: f64 = 1.0;

/// Number of model attempts before giving up (rate-limit resilience).
const MAX_ATTEMPTS: u32 = 2;

// ---------------------------------------------------------------------------
// Proposal
// ---------------------------------------------------------------------------

/// A partial `EngineConfig` change set. `None` fields mean "keep current".
#[derive(Debug, Clone, PartialEq)]
pub struct TunerProposal {
    pub min_profit_margin: Option<f64>,
    pub taker_fee_rate: Option<f64>,
    pub tranche_count: Option<u32>,
    pub min_tranche_fraction: Option<f64>,
    pub max_spread_pct: Option<f64>,
    pub crash_drop_pct: Option<f64>,
    pub crash_window_secs: Option<i64>,
    /// ATH guard: look back this many days for the recent high (0 = off).
    pub ath_lookback_days: Option<i64>,
    /// ATH guard: skip buys within this percent of the recent high.
    pub max_buy_from_high_pct: Option<f64>,
    /// Run-up guard: skip buys when price is more than this percent above the
    /// recent low over `ath_lookback_days` (0 = off).
    pub max_runup_pct: Option<f64>,
    /// Trend filter: `OFF` | `SMA50` | `GOLDEN_CROSS`.
    pub trend_filter: Option<TrendFilter>,
    /// Trend exit: sell in-profit lots when price crosses below the 50-day SMA.
    pub trend_exit: Option<bool>,
    /// Take-profit target: exit at `min_exit * (1 + take_profit_pct)`.
    pub take_profit_pct: Option<f64>,
    pub profit_mode: Option<ProfitMode>,
    pub rationale: String,
}

/// Raw JSON shape the model is asked to produce. Numeric fields are parsed as
/// `f64` first so `4.0`-style output for integer fields is accepted.
#[derive(Debug, Deserialize)]
struct RawProposal {
    min_profit_margin: Option<f64>,
    taker_fee_rate: Option<f64>,
    tranche_count: Option<f64>,
    min_tranche_fraction: Option<f64>,
    max_spread_pct: Option<f64>,
    crash_drop_pct: Option<f64>,
    crash_window_secs: Option<f64>,
    ath_lookback_days: Option<f64>,
    max_buy_from_high_pct: Option<f64>,
    max_runup_pct: Option<f64>,
    trend_filter: Option<String>,
    trend_exit: Option<bool>,
    take_profit_pct: Option<f64>,
    profit_mode: Option<String>,
    profit_reinvest_fraction: Option<f64>,
    rationale: Option<String>,
}

fn check_f64(v: f64, name: &str, lo: f64, hi: f64) -> Result<f64> {
    if !v.is_finite() {
        bail!("{name} must be finite, got {v}");
    }
    if v < lo || v > hi {
        bail!("{name} out of bounds [{lo}, {hi}], got {v}");
    }
    Ok(v)
}

fn check_int(v: f64, name: &str, lo: i64, hi: i64) -> Result<i64> {
    if !v.is_finite() || v.fract() != 0.0 {
        bail!("{name} must be an integer, got {v}");
    }
    let n = v as i64;
    if n < lo || n > hi {
        bail!("{name} out of bounds [{lo}, {hi}], got {n}");
    }
    Ok(n)
}

/// Parse and validate the model's raw output into a proposal. Extracts the
/// first JSON object (models love to wrap output in prose or fences).
pub fn parse_tuner_response(raw: &str) -> Result<TunerProposal> {
    let start = raw
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON object in output"))?;
    let end = raw
        .rfind('}')
        .ok_or_else(|| anyhow::anyhow!("no closing brace in output"))?;
    if end <= start {
        bail!("malformed JSON object");
    }
    let body = &raw[start..=end];
    let parsed: RawProposal =
        serde_json::from_str(body).map_err(|e| anyhow::anyhow!("invalid JSON: {e}"))?;

    let min_profit_margin = parsed
        .min_profit_margin
        .map(|v| check_f64(v, "min_profit_margin", MARGIN_MIN, MARGIN_MAX))
        .transpose()?;
    let taker_fee_rate = parsed
        .taker_fee_rate
        .map(|v| check_f64(v, "taker_fee_rate", FEE_MIN, FEE_MAX))
        .transpose()?;
    let tranche_count = parsed
        .tranche_count
        .map(|v| check_int(v, "tranche_count", TRANCHES_MIN as i64, TRANCHES_MAX as i64))
        .transpose()?
        .map(|n| n as u32);
    let min_tranche_fraction = parsed
        .min_tranche_fraction
        .map(|v| {
            check_f64(
                v,
                "min_tranche_fraction",
                MIN_TRANCHE_FRAC_MIN,
                MIN_TRANCHE_FRAC_MAX,
            )
        })
        .transpose()?;
    let max_spread_pct = parsed
        .max_spread_pct
        .map(|v| check_f64(v, "max_spread_pct", SPREAD_MIN, SPREAD_MAX))
        .transpose()?;
    let crash_drop_pct = parsed
        .crash_drop_pct
        .map(|v| check_f64(v, "crash_drop_pct", CRASH_DROP_MIN, CRASH_DROP_MAX))
        .transpose()?;
    let crash_window_secs = parsed
        .crash_window_secs
        .map(|v| check_int(v, "crash_window_secs", CRASH_WINDOW_MIN, CRASH_WINDOW_MAX))
        .transpose()?;
    let ath_lookback_days = parsed
        .ath_lookback_days
        .map(|v| check_int(v, "ath_lookback_days", ATH_LOOKBACK_MIN, ATH_LOOKBACK_MAX))
        .transpose()?;
    let max_buy_from_high_pct = parsed
        .max_buy_from_high_pct
        .map(|v| {
            check_f64(
                v,
                "max_buy_from_high_pct",
                MAX_BUY_FROM_HIGH_MIN,
                MAX_BUY_FROM_HIGH_MAX,
            )
        })
        .transpose()?;
    let max_runup_pct = parsed
        .max_runup_pct
        .map(|v| check_f64(v, "max_runup_pct", MAX_RUNUP_MIN, MAX_RUNUP_MAX))
        .transpose()?;
    let trend_filter = parsed
        .trend_filter
        .as_deref()
        .map(|s| {
            TrendFilter::parse(s).ok_or_else(|| {
                anyhow::anyhow!("unknown trend_filter '{s}' (OFF|SMA50|GOLDEN_CROSS)")
            })
        })
        .transpose()?;
    let trend_exit = parsed.trend_exit;
    let take_profit_pct = parsed
        .take_profit_pct
        .map(|v| check_f64(v, "take_profit_pct", TAKE_PROFIT_MIN, TAKE_PROFIT_MAX))
        .transpose()?;

    let profit_mode = match parsed.profit_mode.as_deref() {
        None => None,
        Some("REINVEST") => Some(ProfitMode::Reinvest),
        Some("SEPARATE") => Some(ProfitMode::Separate),
        Some("PARTIAL") => {
            let f = parsed.profit_reinvest_fraction.ok_or_else(|| {
                anyhow::anyhow!("profit_mode PARTIAL requires profit_reinvest_fraction")
            })?;
            Some(ProfitMode::Partial(check_f64(
                f,
                "profit_reinvest_fraction",
                0.0,
                1.0,
            )?))
        }
        Some(other) => {
            bail!("unknown profit_mode '{other}' (expected REINVEST|SEPARATE|PARTIAL)")
        }
    };

    Ok(TunerProposal {
        min_profit_margin,
        taker_fee_rate,
        tranche_count,
        min_tranche_fraction,
        max_spread_pct,
        crash_drop_pct,
        crash_window_secs,
        ath_lookback_days,
        max_buy_from_high_pct,
        max_runup_pct,
        trend_filter,
        trend_exit,
        take_profit_pct,
        profit_mode,
        rationale: parsed
            .rationale
            .unwrap_or_else(|| "no rationale provided".to_string()),
    })
}

/// Merge a proposal into the current config and validate the result with the
/// engine's own validator AND the tuner's sane bounds (defense in depth: a
/// `TunerProposal` built outside `parse_tuner_response` still cannot set
/// absurd values). Returns `None` when nothing changed.
pub fn apply_proposal(current: &EngineConfig, p: &TunerProposal) -> Result<Option<EngineConfig>> {
    let merged = EngineConfig {
        min_profit_margin: p.min_profit_margin.unwrap_or(current.min_profit_margin),
        taker_fee_rate: p.taker_fee_rate.unwrap_or(current.taker_fee_rate),
        tranche_count: p.tranche_count.unwrap_or(current.tranche_count),
        min_tranche_fraction: p
            .min_tranche_fraction
            .unwrap_or(current.min_tranche_fraction),
        max_spread_pct: p.max_spread_pct.unwrap_or(current.max_spread_pct),
        crash_drop_pct: p.crash_drop_pct.unwrap_or(current.crash_drop_pct),
        crash_window_secs: p.crash_window_secs.unwrap_or(current.crash_window_secs),
        ath_lookback_days: p.ath_lookback_days.unwrap_or(current.ath_lookback_days),
        max_buy_from_high_pct: p
            .max_buy_from_high_pct
            .unwrap_or(current.max_buy_from_high_pct),
        min_dip_from_high_pct: current.min_dip_from_high_pct,
        max_runup_pct: p.max_runup_pct.unwrap_or(current.max_runup_pct),
        trend_filter: p.trend_filter.unwrap_or(current.trend_filter),
        trend_exit: p.trend_exit.unwrap_or(current.trend_exit),
        take_profit_pct: p.take_profit_pct.unwrap_or(current.take_profit_pct),
        trail_pct: current.trail_pct,
        profit_mode: p.profit_mode.unwrap_or(current.profit_mode),
    };
    merged.validate()?;
    check_tuner_bounds(&merged)?;
    let changed = merged.min_profit_margin != current.min_profit_margin
        || merged.taker_fee_rate != current.taker_fee_rate
        || merged.tranche_count != current.tranche_count
        || merged.min_tranche_fraction != current.min_tranche_fraction
        || merged.max_spread_pct != current.max_spread_pct
        || merged.crash_drop_pct != current.crash_drop_pct
        || merged.crash_window_secs != current.crash_window_secs
        || merged.ath_lookback_days != current.ath_lookback_days
        || merged.max_buy_from_high_pct != current.max_buy_from_high_pct
        || merged.max_runup_pct != current.max_runup_pct
        || merged.trend_filter != current.trend_filter
        || merged.trend_exit != current.trend_exit
        || merged.take_profit_pct != current.take_profit_pct
        || merged.profit_mode != current.profit_mode;
    Ok(changed.then_some(merged))
}

/// Tuner's sane bounds, applied to a fully merged config. Wider than the
/// engine's hard validation so the model cannot set absurd values even when
/// they are technically valid.
pub(crate) fn check_tuner_bounds(c: &EngineConfig) -> Result<()> {
    check_f64(
        c.min_profit_margin,
        "min_profit_margin",
        MARGIN_MIN,
        MARGIN_MAX,
    )?;
    check_f64(c.taker_fee_rate, "taker_fee_rate", FEE_MIN, FEE_MAX)?;
    if c.tranche_count < TRANCHES_MIN || c.tranche_count > TRANCHES_MAX {
        bail!(
            "tranche_count out of bounds [{TRANCHES_MIN}, {TRANCHES_MAX}], got {}",
            c.tranche_count
        );
    }
    check_f64(
        c.min_tranche_fraction,
        "min_tranche_fraction",
        MIN_TRANCHE_FRAC_MIN,
        MIN_TRANCHE_FRAC_MAX,
    )?;
    check_f64(c.max_spread_pct, "max_spread_pct", SPREAD_MIN, SPREAD_MAX)?;
    check_f64(
        c.crash_drop_pct,
        "crash_drop_pct",
        CRASH_DROP_MIN,
        CRASH_DROP_MAX,
    )?;
    if c.crash_window_secs < CRASH_WINDOW_MIN || c.crash_window_secs > CRASH_WINDOW_MAX {
        bail!(
            "crash_window_secs out of bounds [{CRASH_WINDOW_MIN}, {CRASH_WINDOW_MAX}], got {}",
            c.crash_window_secs
        );
    }
    if c.ath_lookback_days < ATH_LOOKBACK_MIN || c.ath_lookback_days > ATH_LOOKBACK_MAX {
        bail!(
            "ath_lookback_days out of bounds [{ATH_LOOKBACK_MIN}, {ATH_LOOKBACK_MAX}], got {}",
            c.ath_lookback_days
        );
    }
    check_f64(
        c.max_buy_from_high_pct,
        "max_buy_from_high_pct",
        MAX_BUY_FROM_HIGH_MIN,
        MAX_BUY_FROM_HIGH_MAX,
    )?;
    check_f64(
        c.max_runup_pct,
        "max_runup_pct",
        MAX_RUNUP_MIN,
        MAX_RUNUP_MAX,
    )?;
    check_f64(
        c.take_profit_pct,
        "take_profit_pct",
        TAKE_PROFIT_MIN,
        TAKE_PROFIT_MAX,
    )?;
    if let ProfitMode::Partial(f) = c.profit_mode {
        check_f64(f, "profit_reinvest_fraction", 0.0, 1.0)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Prompt building
// ---------------------------------------------------------------------------

fn config_block(c: &EngineConfig) -> String {
    format!(
        "{{\n  \"min_profit_margin\": {},\n  \"taker_fee_rate\": {},\n  \
         \"tranche_count\": {},\n  \"min_tranche_fraction\": {},\n  \
         \"max_spread_pct\": {},\n  \"crash_drop_pct\": {},\n  \
         \"crash_window_secs\": {},\n  \"ath_lookback_days\": {},\n  \
         \"max_buy_from_high_pct\": {},\n  \"max_runup_pct\": {},\n  \"trend_filter\": \"{}\",\n  \"trend_exit\": {},\n  \"take_profit_pct\": {},\n  \"profit_mode\": \"{}\n}}",
        c.min_profit_margin,
        c.taker_fee_rate,
        c.tranche_count,
        c.min_tranche_fraction,
        c.max_spread_pct,
        c.crash_drop_pct,
        c.crash_window_secs,
        c.ath_lookback_days,
        c.max_buy_from_high_pct,
        c.max_runup_pct,
        c.trend_filter.as_str(),
        c.trend_exit,
        c.take_profit_pct,
        c.profit_mode.as_str()
    )
}

/// Compact, model-readable summary of what the backtests taught us.
pub fn build_tuner_prompt(
    store: &TradingStore,
    current: &EngineConfig,
    limit: i64,
) -> Result<String> {
    let mut backtests = String::new();
    for b in store.list_backtests(limit)? {
        backtests.push_str(&format!(
            "- {} {}..{}: {} trades ({}W/{}L), PnL {:+.2}%, max DD {:.2}%\n",
            b.symbol,
            b.start_date,
            b.end_date,
            b.total_trades,
            b.wins,
            b.losses,
            b.pnl_pct,
            b.max_drawdown_pct
        ));
    }
    if backtests.is_empty() {
        backtests.push_str("- (no backtests recorded yet — run `trade backtest` first)\n");
    }

    let mut lessons = String::new();
    for l in store.list_lessons(limit)? {
        lessons.push_str(&format!("- [{}] {}\n", l.symbol, l.lesson));
    }
    if lessons.is_empty() {
        lessons.push_str("- (no lessons yet)\n");
    }

    Ok(format!(
        "You are the strategy tuner for a dip-buying accumulation bot (\"Win-Win Accumulator\").\n\
         The deterministic engine executes trades; YOU decide the SETTINGS based on what\n\
         backtests and lessons taught us.\n\n\
         CURRENT ENGINE CONFIG (JSON):\n{config}\n\n\
         RECENT BACKTEST RESULTS (most recent first):\n{backtests}\n\
         RECENT STRATEGY LESSONS:\n{lessons}\n\
         TASK:\n\
         Propose changes to the engine config that would improve future performance.\n\
         Rules:\n\
         - Output ONLY valid JSON. No markdown fences, no commentary outside the JSON.\n\
         - Include ONLY the fields you want to change.\n\
         - All values must be finite and within these bounds:\n\
           min_profit_margin: {MARGIN_MIN}..={MARGIN_MAX} (fraction, 0.01 = 1%%)\n\
           taker_fee_rate: {FEE_MIN}..={FEE_MAX}\n\
           tranche_count: {TRANCHES_MIN}..={TRANCHES_MAX} (integer)\n\
           min_tranche_fraction: {MIN_TRANCHE_FRAC_MIN}..={MIN_TRANCHE_FRAC_MAX}\n\
           max_spread_pct: {SPREAD_MIN}..={SPREAD_MAX}\n\
           crash_drop_pct: {CRASH_DROP_MIN}..={CRASH_DROP_MAX}\n\
           crash_window_secs: {CRASH_WINDOW_MIN}..={CRASH_WINDOW_MAX} (integer seconds)\n\
           ath_lookback_days: {ATH_LOOKBACK_MIN}..={ATH_LOOKBACK_MAX} (integer days, 0 = off)\n\
           max_buy_from_high_pct: {MAX_BUY_FROM_HIGH_MIN}..={MAX_BUY_FROM_HIGH_MAX}\n\
           max_runup_pct: {MAX_RUNUP_MIN}..={MAX_RUNUP_MAX} (0 = off)\n\
           trend_filter: \"OFF\" | \"SMA50\" | \"GOLDEN_CROSS\" (buy only when the\n\
             50-day trend is up; SMA50 = price above SMA50, GOLDEN_CROSS =\n\
             SMA20 above SMA50)\n\
           trend_exit: true | false (sell in-profit lots when price crosses\n\
             below SMA50 — locks in gains before a reversal)\n\
           take_profit_pct: {TAKE_PROFIT_MIN}..={TAKE_PROFIT_MAX} (fraction;\n\
             lots exit at the no-loss floor * (1 + take_profit_pct), so winners\n\
             run in bull markets; 0 = exit at the floor immediately)\n\
           profit_mode: \"REINVEST\" | \"SEPARATE\" | \"PARTIAL\" (with\n\
             \"profit_reinvest_fraction\" 0..=1 when PARTIAL)\n\
         - profit_mode decides what happens to realized profit: REINVEST compounds\n\
           it into the working pool, SEPARATE parks it in a reserve (working\n\
           capital stays at the original budget), PARTIAL splits it by fraction.\n\
         - trend_filter OFF + trend_exit false = classic dip-buyer; SMA50 + true\n\
           = regime-aware mode that sits out bear markets and locks gains.\n\
         - Pair take_profit_pct with trend_exit: a high take-profit without\n\
           trend_exit risks giving gains back in a reversal; trend_exit without\n\
           take-profit sells at the floor too early in a bull market.\n\
         - Always include a short \"rationale\" (2-3 sentences) explaining WHY.\n\
         - If no change is justified, output {{\"rationale\": \"...\"}} with no other fields.\n\
         Example: {{\"min_profit_margin\": 0.015, \"tranche_count\": 4, \"rationale\": \"...\"}}",
        config = config_block(current),
        backtests = backtests,
        lessons = lessons,
    ))
}

// ---------------------------------------------------------------------------
// Outcome & application
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TuneOutcome {
    /// Whether a new config was accepted (and persisted unless dry-run).
    #[allow(dead_code)] // read by tests; surfaced via CLI note text
    pub applied: bool,
    /// The config in effect after the tune (current when nothing changed).
    pub config: EngineConfig,
    pub rationale: String,
    pub source: String,
    /// Human-readable note: what happened (applied / no change / fallback).
    pub note: String,
}

/// Apply a raw model output to the store. Never returns `Err` for garbage —
/// it degrades to "keep current config" with an explanatory note.
pub fn tune_with_output(
    store: &TradingStore,
    raw: &str,
    dry_run: bool,
    source: &str,
) -> Result<TuneOutcome> {
    let current = store
        .latest_engine_config()?
        .map(|r| r.to_engine_config())
        .unwrap_or_default();

    let proposal = match parse_tuner_response(raw) {
        Ok(p) => p,
        Err(e) => {
            return Ok(TuneOutcome {
                applied: false,
                config: current,
                rationale: String::new(),
                source: source.to_string(),
                note: format!("model output rejected by deterministic gate: {e}"),
            })
        }
    };

    match apply_proposal(&current, &proposal)? {
        None => Ok(TuneOutcome {
            applied: false,
            config: current,
            rationale: proposal.rationale,
            source: source.to_string(),
            note: "model proposed no changes — current config kept".to_string(),
        }),
        Some(next) => {
            if !dry_run {
                store.record_engine_config(NewEngineConfig {
                    min_profit_margin: next.min_profit_margin,
                    taker_fee_rate: next.taker_fee_rate,
                    tranche_count: next.tranche_count,
                    min_tranche_fraction: next.min_tranche_fraction,
                    max_spread_pct: next.max_spread_pct,
                    crash_drop_pct: next.crash_drop_pct,
                    crash_window_secs: next.crash_window_secs,
                    ath_lookback_days: next.ath_lookback_days,
                    max_buy_from_high_pct: next.max_buy_from_high_pct,
                    min_dip_from_high_pct: next.min_dip_from_high_pct,
                    max_runup_pct: next.max_runup_pct,
                    trend_filter: next.trend_filter.as_str().to_string(),
                    trend_exit: next.trend_exit,
                    take_profit_pct: next.take_profit_pct,
                    trail_pct: next.trail_pct,
                    profit_mode: next.profit_mode.as_str(),
                    rationale: proposal.rationale.clone(),
                    source: source.to_string(),
                })?;
            }
            Ok(TuneOutcome {
                applied: true,
                config: next,
                rationale: proposal.rationale,
                source: source.to_string(),
                note: if dry_run {
                    "dry-run: config accepted but NOT persisted".to_string()
                } else {
                    "config updated".to_string()
                },
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Live model call
// ---------------------------------------------------------------------------

/// Pick a model alias for tuning: explicit override, else the best available
/// "main" model, else `None` (caller then falls back gracefully).
pub fn pick_model(override_alias: Option<&str>) -> Option<String> {
    if let Some(a) = override_alias {
        if !a.trim().is_empty() {
            return Some(a.trim().to_string());
        }
    }
    crate::crypto::ai::pools::get_available_models(Some("main"))
        .into_iter()
        .next()
}

async fn call_tuner_model(
    client: &reqwest::Client,
    gemini_key: &str,
    alias: &str,
    prompt: &str,
) -> Result<String> {
    let mut last_err = anyhow::anyhow!("tuner model call failed");
    for attempt in 0..MAX_ATTEMPTS {
        match crate::crypto::ai::providers::call_any_model(client, gemini_key, alias, prompt, None)
            .await
        {
            Ok(text) => return Ok(text),
            Err(e) => {
                tracing::warn!("tuner model attempt {} failed: {e}", attempt + 1);
                last_err = e;
                tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt as u64 + 1)))
                    .await;
            }
        }
    }
    Err(last_err)
}

/// Run the full tuning loop against a live model. Any failure (missing key,
/// rate limit, dead model, garbage output) degrades to the current config.
pub fn run_tuner(
    store: &TradingStore,
    dry_run: bool,
    model_alias: Option<&str>,
    limit: i64,
) -> Result<TuneOutcome> {
    let current = store
        .latest_engine_config()?
        .map(|r| r.to_engine_config())
        .unwrap_or_default();

    let gemini_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();
    if gemini_key.is_empty() {
        return Ok(TuneOutcome {
            applied: false,
            config: current,
            rationale: String::new(),
            source: "none".to_string(),
            note: "no GEMINI_API_KEY in env — kept current config".to_string(),
        });
    }

    let alias = match pick_model(model_alias) {
        Some(a) => a,
        None => {
            return Ok(TuneOutcome {
                applied: false,
                config: current,
                rationale: String::new(),
                source: "none".to_string(),
                note: "no available model (check models.json / API keys) — kept current config"
                    .to_string(),
            })
        }
    };

    let prompt = build_tuner_prompt(store, &current, limit)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .use_rustls_tls()
        .build()?;

    let raw = match futures::executor::block_on(call_tuner_model(
        &client,
        &gemini_key,
        &alias,
        &prompt,
    )) {
        Ok(r) => r,
        Err(e) => {
            return Ok(TuneOutcome {
                applied: false,
                config: current,
                rationale: String::new(),
                source: alias,
                note: format!(
                    "model unavailable after {MAX_ATTEMPTS} attempts ({e}) — kept current config"
                ),
            })
        }
    };

    tune_with_output(store, &raw, dry_run, &alias)
}

// ---------------------------------------------------------------------------
// Tests (deterministic — no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::store::TradingMode;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TUNER_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_store() -> TradingStore {
        let n = TUNER_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = format!("target/test_trading_tuner_{}_{}.db", std::process::id(), n);
        let _ = std::fs::remove_file(&path);
        TradingStore::open_path(&path, TradingMode::Simulation).unwrap()
    }

    fn default_cfg() -> EngineConfig {
        EngineConfig::default()
    }

    // -- parse gate ---------------------------------------------------------

    #[test]
    fn parse_rejects_garbage() {
        for bad in [
            "",
            "not json at all",
            "{",
            "[]",
            "{\"min_profit_margin\": \"high\"}",
            "{\"min_profit_margin\": NaN}",
            "{\"min_profit_margin\": 1e999}",
            "{\"min_profit_margin\": -0.5}",
            "{\"min_profit_margin\": 0.5}",
            "{\"taker_fee_rate\": 0.99}",
            "{\"tranche_count\": 0}",
            "{\"tranche_count\": 13}",
            "{\"tranche_count\": 3.5}",
            "{\"crash_drop_pct\": 0.0}",
            "{\"crash_drop_pct\": 100.0}",
            "{\"crash_window_secs\": 0}",
            "{\"crash_window_secs\": 1e12}",
            "{\"crash_window_secs\": 1.5}",
            "{\"max_spread_pct\": -1.0}",
            "{\"min_tranche_fraction\": 2.0}",
            "{\"profit_mode\": \"MOON\"}",
            "{\"profit_mode\": \"PARTIAL\"}",
            "{\"profit_mode\": \"PARTIAL\", \"profit_reinvest_fraction\": 1.5}",
            "{\"profit_mode\": \"PARTIAL\", \"profit_reinvest_fraction\": -0.1}",
        ] {
            assert!(parse_tuner_response(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn parse_accepts_partial_and_wrapped_output() {
        // prose + fences around the JSON must not matter
        let raw = "Here is my proposal:\n```json\n{\"min_profit_margin\": 0.015, \"tranche_count\": 4, \"rationale\": \"wider margin\"}\n```";
        let p = parse_tuner_response(raw).unwrap();
        assert_eq!(p.min_profit_margin, Some(0.015));
        assert_eq!(p.tranche_count, Some(4));
        assert_eq!(p.rationale, "wider margin");
        assert_eq!(p.taker_fee_rate, None);

        // integer fields accept float notation
        let p = parse_tuner_response(
            "{\"crash_window_secs\": 604800.0, \"tranche_count\": 3.0, \"rationale\": \"x\"}",
        )
        .unwrap();
        assert_eq!(p.crash_window_secs, Some(604800));
        assert_eq!(p.tranche_count, Some(3));

        // empty proposal (no changes) is valid
        let p = parse_tuner_response("{\"rationale\": \"nothing to change\"}").unwrap();
        assert_eq!(p.min_profit_margin, None);

        // profit_mode parsing: all three modes + PARTIAL fraction
        let p =
            parse_tuner_response("{\"profit_mode\": \"REINVEST\", \"rationale\": \"r\"}").unwrap();
        assert_eq!(p.profit_mode, Some(ProfitMode::Reinvest));
        let p =
            parse_tuner_response("{\"profit_mode\": \"SEPARATE\", \"rationale\": \"r\"}").unwrap();
        assert_eq!(p.profit_mode, Some(ProfitMode::Separate));
        let p = parse_tuner_response(
            "{\"profit_mode\": \"PARTIAL\", \"profit_reinvest_fraction\": 0.4, \"rationale\": \"r\"}",
        )
        .unwrap();
        assert_eq!(p.profit_mode, Some(ProfitMode::Partial(0.4)));
    }

    // -- apply gate ---------------------------------------------------------

    #[test]
    fn apply_merges_and_validates() {
        let cur = default_cfg();
        let p = parse_tuner_response(
            "{\"min_profit_margin\": 0.02, \"tranche_count\": 5, \"rationale\": \"r\"}",
        )
        .unwrap();
        let next = apply_proposal(&cur, &p).unwrap().unwrap();
        assert_eq!(next.min_profit_margin, 0.02);
        assert_eq!(next.tranche_count, 5);
        // untouched fields keep current values
        assert_eq!(next.taker_fee_rate, cur.taker_fee_rate);
        assert_eq!(next.crash_window_secs, cur.crash_window_secs);
        next.validate().unwrap();

        // profit_mode merges and is validated by the engine gate
        let p = parse_tuner_response(
            "{\"profit_mode\": \"PARTIAL\", \"profit_reinvest_fraction\": 0.5, \"rationale\": \"r\"}",
        )
        .unwrap();
        let next = apply_proposal(&cur, &p).unwrap().unwrap();
        assert_eq!(next.profit_mode, ProfitMode::Partial(0.5));
        next.validate().unwrap();

        // trend filter + trend exit merge too
        let p = parse_tuner_response(
            "{\"trend_filter\": \"SMA50\", \"trend_exit\": true, \"rationale\": \"r\"}",
        )
        .unwrap();
        let next = apply_proposal(&cur, &p).unwrap().unwrap();
        assert_eq!(next.trend_filter, TrendFilter::Sma50);
        assert!(next.trend_exit);
        next.validate().unwrap();
    }

    #[test]
    fn apply_no_changes_returns_none() {
        let cur = default_cfg();
        let p = parse_tuner_response("{\"rationale\": \"no change\"}").unwrap();
        assert!(apply_proposal(&cur, &p).unwrap().is_none());
    }

    #[test]
    fn apply_rejects_out_of_bounds_even_if_engine_would_accept() {
        // 0.5 margin passes EngineConfig::validate but exceeds tuner bounds.
        // Build the proposal directly (bypassing the parse gate) so this test
        // exercises the apply gate's own bounds check.
        let cur = default_cfg();
        let p = TunerProposal {
            min_profit_margin: Some(0.5),
            taker_fee_rate: None,
            tranche_count: None,
            min_tranche_fraction: None,
            max_spread_pct: None,
            crash_drop_pct: None,
            crash_window_secs: None,
            ath_lookback_days: None,
            max_buy_from_high_pct: None,
            max_runup_pct: None,
            trend_filter: None,
            trend_exit: None,
            take_profit_pct: None,
            profit_mode: None,
            rationale: "r".to_string(),
        };
        assert!(apply_proposal(&cur, &p).is_err());
    }

    // -- full flow (no network) --------------------------------------------

    #[test]
    fn tune_with_output_applies_valid_and_persists() {
        let store = test_store();
        let out = tune_with_output(
            &store,
            "{\"min_profit_margin\": 0.02, \"rationale\": \"learned from wins\"}",
            false,
            "test-model",
        )
        .unwrap();
        assert!(out.applied);
        assert_eq!(out.config.min_profit_margin, 0.02);
        let latest = store.latest_engine_config().unwrap().unwrap();
        assert_eq!(latest.min_profit_margin, 0.02);
        assert_eq!(latest.source, "test-model");

        // profit_mode persists through the full flow
        let out = tune_with_output(
            &store,
            "{\"profit_mode\": \"SEPARATE\", \"rationale\": \"keep capital safe\"}",
            false,
            "test-model",
        )
        .unwrap();
        assert!(out.applied);
        assert_eq!(out.config.profit_mode, ProfitMode::Separate);
        let latest = store.latest_engine_config().unwrap().unwrap();
        assert_eq!(latest.profit_mode, "SEPARATE");
    }

    #[test]
    fn tune_with_output_garbage_keeps_current_config() {
        let store = test_store();
        let out =
            tune_with_output(&store, "{\"min_profit_margin\": 999}", false, "test-model").unwrap();
        assert!(!out.applied);
        assert_eq!(
            out.config.min_profit_margin,
            default_cfg().min_profit_margin
        );
        assert!(out.note.contains("rejected"));
        assert!(store.latest_engine_config().unwrap().is_none());
    }

    #[test]
    fn tune_with_output_dry_run_does_not_persist() {
        let store = test_store();
        let out = tune_with_output(
            &store,
            "{\"tranche_count\": 6, \"rationale\": \"dry\"}",
            true,
            "test-model",
        )
        .unwrap();
        assert!(out.applied);
        assert_eq!(out.config.tranche_count, 6);
        assert!(store.latest_engine_config().unwrap().is_none());
    }

    #[test]
    fn tune_with_output_never_panics_on_garbage() {
        let store = test_store();
        let garbage = [
            "",
            "{}",
            "{\"min_profit_margin\": NaN}",
            "{\"tranche_count\": -1}",
            "{\"crash_window_secs\": \"soon\"}",
            "not json",
            "{\"min_profit_margin\": 1e999, \"rationale\": \"x\"}",
        ];
        for g in garbage {
            let out = tune_with_output(&store, g, false, "test-model").unwrap();
            assert!(!out.applied, "garbage must never apply: {g}");
        }
        assert!(store.latest_engine_config().unwrap().is_none());
    }

    #[test]
    fn prompt_includes_context_and_bounds() {
        let store = test_store();
        let p = build_tuner_prompt(&store, &default_cfg(), 10).unwrap();
        assert!(p.contains("CURRENT ENGINE CONFIG"));
        assert!(p.contains("no backtests recorded yet"));
        assert!(p.contains("min_profit_margin"));
        assert!(p.contains("rationale"));
    }
}
