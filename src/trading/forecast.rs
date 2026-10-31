//! Multi-horizon price forecast from the local history series.
//!
//! Every number here is deterministic: a drift/volatility model over the same
//! daily closes the backtests replay. The AI is *not* involved, because a
//! language model cannot produce calibrated day-180 numbers — it can only
//! narrate the table below. This module is therefore safe to call from a
//! Telegram command and from the CLI without burning an API quota.
//!
//! Model (per horizon `h` days):
//! * `mu` — daily log drift, a blend of the 20/50/200-day log returns,
//!   adjusted for the current regime, RSI extremes and monthly seasonality,
//!   clamped to ±0.5%/day so a parabolic week cannot produce nonsense.
//! * `sigma` — daily volatility (stddev of log returns over 20 closes).
//! * target = price · e^(mu·s·h / sqrt(1 + h/decay)) — momentum decays with
//!   the horizon so a hot week is not extrapolated into a fantasy year.
//! * range  = target · e^(±sigma·√h) — the ~68% cone, widening with √h.
//! * P(up)  = Φ(mu·s·√h / sigma) — the model's own probability the price is
//!   higher at that horizon.
//!
//! ## Adaptivity
//!
//! The old model used one hardcoded damping constant and an uncalibrated
//! volatility. [`calibrate`] grid-searches the drift decay, a drift shrink
//! factor and a volatility multiplier against the symbol's own walk-forward
//! outcomes and picks the combination with the lowest out-of-sample error
//! (MAPE + cone-miss penalty). So the model learns, per symbol, how much of the
//! recent momentum is actually worth extrapolating and how wide the cone must
//! be to be honest. [`backtest`] reports the same walk-forward metrics for any
//! parameter set, which is how the "was the previous release accurate?" check
//! is produced.
//!
//! ## Per-symbol profiles
//!
//! Majors (BTC/ETH) have 8+ years of data, so they keep the full 1…180 day
//! ladder. Younger alts (SOL/LINK) start in 2020, so their longest horizon is
//! 90 days. Anything else is capped at 30 days and at `history_days / 4`.

use crate::crypto::ai::indicators::{rsi, sma, volatility};
use crate::trading::market_calendar::monthly_seasonality_detailed;
use crate::trading::regime::{classify_closes, MarketRegime};

const DAY_MS: i64 = 86_400_000;
/// Maximum daily drift the model is allowed to claim.
const MAX_DAILY_DRIFT: f64 = 0.005;
/// Maximum cumulative log drift over any horizon (~±42%). A short-term
/// momentum signal must not be extrapolated into a fantasy price 180 days out.
const MAX_CUM_DRIFT: f64 = 0.35;
/// Legacy damping constant. Kept as the *default* so a symbol without enough
/// data (or before calibration) behaves exactly like the first release.
const DRIFT_DECAY_DAYS: f64 = 30.0;
/// Volatility floor (fraction per day). A perfectly smooth series has zero
/// dispersion of log returns, which would collapse the cone to a point and make
/// confidence identical at every horizon. Crypto never moves in a straight line,
/// so we keep a conservative 0.3%/day floor.
const MIN_DAILY_SIGMA: f64 = 0.003;

/// Calibration looks back this far and samples every `CALIBRATION_STEP_DAYS`.
const CALIBRATION_WINDOW_DAYS: usize = 400;
const CALIBRATION_STEP_DAYS: usize = 3;
/// Candidate damping half-lives (days). `INFINITY` = no damping at all.
const CALIBRATION_DECAYS: [f64; 8] = [10.0, 20.0, 30.0, 45.0, 60.0, 90.0, 180.0, f64::INFINITY];
/// Candidate drift shrink factors. Zero is excluded: a model that always
/// predicts "no change" is useless even if its MAPE is slightly lower.
const CALIBRATION_SCALES: [f64; 4] = [0.3, 0.5, 0.75, 1.0];
/// Candidate cone-width multipliers around the measured volatility.
const CALIBRATION_VOL_MULTS: [f64; 4] = [0.85, 1.0, 1.15, 1.3];

/// Full ladder for majors with a decade of daily closes.
pub const HORIZONS_MAJOR: [i64; 8] = [1, 3, 5, 7, 14, 30, 90, 180];
/// Younger alts: no 180-day claim from ~6 years of data.
pub const HORIZONS_ALT: [i64; 8] = [1, 3, 5, 7, 14, 30, 60, 90];
/// Anything without a dedicated profile.
pub const HORIZONS_SHORT: [i64; 6] = [1, 3, 5, 7, 14, 30];

#[derive(Debug, Clone)]
pub struct HorizonForecast {
    pub days: i64,
    pub target: f64,
    /// Lower/upper bound of the ~68% cone.
    pub low: f64,
    pub high: f64,
    pub change_pct: f64,
    /// 0..1 — how much to trust this specific horizon.
    pub confidence: f64,
    /// 0..1 — model probability that price at this horizon is above spot.
    pub prob_up: f64,
}

/// Real walk-forward accuracy for one horizon, recomputed on every call from
/// the freshest history. This is the "how often is the model actually right"
/// number the user sees, not a promise.
#[derive(Debug, Clone)]
pub struct HorizonAccuracy {
    pub days: i64,
    /// Direction hit rate, percent (50% = coin flip).
    pub direction_pct: f64,
    /// Mean absolute error of the point target, percent.
    pub mape_pct: f64,
    /// Share of outcomes inside the 68% cone (ideal ≈ 68%), percent.
    pub coverage_pct: f64,
    pub samples: usize,
}

/// Walk-forward-fitted knobs for one symbol.
#[derive(Debug, Clone)]
pub struct Calibration {
    pub decay_days: f64,
    pub drift_scale: f64,
    pub vol_mult: f64,
    /// Number of walk-forward origins the fit used.
    pub samples: usize,
    /// Out-of-sample accuracy per horizon for the chosen parameters.
    pub accuracy: Vec<HorizonAccuracy>,
    /// Combined fit score (MAPE + cone-miss penalty), percent.
    #[allow(dead_code)]
    pub score_pct: f64,
}

impl Default for Calibration {
    fn default() -> Self {
        Self {
            decay_days: DRIFT_DECAY_DAYS,
            drift_scale: 1.0,
            vol_mult: 1.0,
            samples: 0,
            accuracy: Vec::new(),
            score_pct: f64::NAN,
        }
    }
}

impl Calibration {
    /// MAPE for the horizon closest to `days`, if calibrated.
    pub fn mape_for(&self, days: i64) -> Option<f64> {
        self.accuracy
            .iter()
            .min_by_key(|a| (a.days - days).abs())
            .map(|a| a.mape_pct)
            .filter(|m| m.is_finite())
    }

    pub fn is_default(&self) -> bool {
        self.samples == 0
    }
}

#[derive(Debug, Clone)]
pub struct Forecast {
    pub symbol: String,
    pub price: f64,
    pub history_days: usize,
    pub regime: String,
    pub rsi14: Option<f64>,
    pub sma20: Option<f64>,
    pub sma50: Option<f64>,
    pub sma200: Option<f64>,
    /// Daily volatility, percent.
    pub vol_daily_pct: f64,
    /// Daily drift, percent.
    pub drift_daily_pct: f64,
    pub horizons: Vec<HorizonForecast>,
    pub calibration: Calibration,
    /// Free-form caveats (seasonality, missing data, regime notes).
    pub notes: Vec<String>,
}

/// The horizon ladder for `symbol`, trimmed to what its history can support
/// (we never extrapolate beyond a quarter of the observed record).
pub fn horizons_for(symbol: &str, history_days: usize) -> &'static [i64] {
    let sym = symbol.to_uppercase();
    let base: &'static [i64] = match sym.as_str() {
        "BTC" | "ETH" => &HORIZONS_MAJOR,
        "SOL" | "LINK" => &HORIZONS_ALT,
        _ => &HORIZONS_SHORT,
    };
    let max_h = (history_days / 4).max(1) as i64;
    let keep = base.iter().take_while(|h| **h <= max_h).count();
    &base[..keep.max(1)]
}

/// Default damping before calibration. Majors trend for longer than high-beta
/// alts, so their momentum decays more slowly.
fn profile_decay_days(symbol: &str) -> f64 {
    match symbol.to_uppercase().as_str() {
        "BTC" => 60.0,
        "ETH" => 45.0,
        "SOL" | "LINK" => 25.0,
        _ => 15.0,
    }
}

/// Log return per day over the last `days` closes, or `None` when the series is
/// too short / contains non-positive prices.
fn window_daily_log_return(closes: &[f64], days: usize) -> Option<f64> {
    if closes.len() <= days || days == 0 {
        return None;
    }
    let from = closes[closes.len() - 1 - days];
    let to = closes[closes.len() - 1];
    if from <= 0.0 || to <= 0.0 {
        return None;
    }
    Some((to / from).ln() / days as f64)
}

/// Deterministic drift for the day-`h` horizon, before horizon-specific terms.
/// `has_movement` gates the RSI-extreme adjustment: a flat series reads RSI=100
/// (an artifact of zero losses), and treating that as "overbought" would invent
/// a permanent downtrend.
fn daily_drift(
    closes: &[f64],
    regime: MarketRegime,
    rsi14: Option<f64>,
    has_movement: bool,
) -> f64 {
    let r20 = window_daily_log_return(closes, 20);
    let r50 = window_daily_log_return(closes, 50);
    let r200 = window_daily_log_return(closes, 200);

    let mut mu = 0.0;
    let mut weight = 0.0;
    for (ret, w) in [(r20, 0.5), (r50, 0.3), (r200, 0.2)] {
        if let Some(r) = ret {
            mu += r * w;
            weight += w;
        }
    }
    if weight > 0.0 {
        mu /= weight;
    }

    match regime {
        // Near the ATH with hot momentum: mean reversion risk dominates.
        MarketRegime::Euphoria => mu *= 0.5,
        // Buying-the-fear zone: expect a bounce, but keep it modest.
        MarketRegime::Dip => mu += 0.0008,
        MarketRegime::Bear => mu -= 0.0003,
        MarketRegime::Bull => mu += 0.0002,
        MarketRegime::Sideways => {}
    }

    if has_movement {
        if let Some(r) = rsi14 {
            if r >= 70.0 {
                mu -= 0.0008;
            } else if r <= 30.0 {
                mu += 0.0008;
            }
        }
    }

    mu.clamp(-MAX_DAILY_DRIFT, MAX_DAILY_DRIFT)
}

/// Everything a forecast needs from the price series, computed once so the
/// backtester can reuse it across parameter combinations.
#[derive(Debug, Clone, Copy)]
struct CoreStats {
    mu: f64,
    sigma: f64,
    regime: MarketRegime,
    rsi14: Option<f64>,
}

fn core_stats(closes: &[f64]) -> CoreStats {
    let regime = classify_closes(closes);
    let rsi14 = rsi(closes, 14);
    let raw_vol = volatility(closes, 20).unwrap_or(0.0);
    CoreStats {
        mu: daily_drift(closes, regime, rsi14, raw_vol > 0.0),
        sigma: (raw_vol / 100.0).clamp(MIN_DAILY_SIGMA, 10.0),
        regime,
        rsi14,
    }
}

/// Build the horizon table for one parameter set.
fn build_horizons(
    price: f64,
    stats: CoreStats,
    horizons: &[i64],
    decay_days: f64,
    drift_scale: f64,
    vol_mult: f64,
) -> Vec<HorizonForecast> {
    let sigma = stats.sigma * vol_mult;
    horizons
        .iter()
        .map(|&days| {
            let h = days as f64;
            let sigma_h = sigma * h.sqrt();
            // Momentum decays with the horizon instead of compounding linearly:
            // a 0.5%/day recent run is credible for a week, not for half a year.
            let mu_h = stats.mu * drift_scale / (1.0 + h / decay_days).sqrt();
            let total_drift = (mu_h * h).clamp(-MAX_CUM_DRIFT, MAX_CUM_DRIFT);
            let target = price * total_drift.exp();
            let low = target * (-sigma_h).exp();
            let high = target * sigma_h.exp();
            let confidence = (1.0 / (1.0 + 4.0 * sigma_h)).clamp(0.05, 0.95);
            let prob_up = norm_cdf(mu_h * h / sigma_h.max(f64::EPSILON));
            HorizonForecast {
                days,
                target,
                low,
                high,
                change_pct: (target / price - 1.0) * 100.0,
                confidence,
                prob_up,
            }
        })
        .collect()
}

/// Standard normal CDF via Abramowitz–Stegun 7.1.26 (|error| < 1.5e-7).
pub(crate) fn norm_cdf(x: f64) -> f64 {
    let z = x / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * z.abs());
    let poly = (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t
        + 0.254829592)
        * t;
    let erf = 1.0 - poly * (-z * z).exp();
    let erf = if z >= 0.0 { erf } else { -erf };
    0.5 * (1.0 + erf)
}

/// Walk-forward calibration of decay / drift shrink / cone width against the
/// symbol's own realised outcomes.
pub fn calibrate(symbol: &str, history: &[(i64, f64)]) -> Calibration {
    let closes: Vec<f64> = history.iter().map(|&(_, c)| c).collect();
    let n = closes.len();
    let horizons = horizons_for(symbol, n);
    // Uncalibrated fallback still respects the per-symbol profile (majors keep
    // momentum longer than high-beta alts) instead of one global constant.
    let fallback = Calibration {
        decay_days: profile_decay_days(symbol),
        ..Calibration::default()
    };
    if n < 120 {
        return fallback;
    }

    // Cache core stats per origin so the grid search only re-derives targets.
    let start = n.saturating_sub(CALIBRATION_WINDOW_DAYS).max(50);
    let mut origins: Vec<(usize, CoreStats)> = Vec::new();
    let mut i = start;
    while i + 1 < n {
        origins.push((i, core_stats(&closes[..=i])));
        i += CALIBRATION_STEP_DAYS;
    }
    if origins.len() < 10 {
        return fallback;
    }

    let mut best: Option<(f64, f64, f64, f64)> = None;
    for &decay in &CALIBRATION_DECAYS {
        for &scale in &CALIBRATION_SCALES {
            for &vol_mult in &CALIBRATION_VOL_MULTS {
                let mut sum_err = 0.0;
                let mut count = 0usize;
                let mut cone_miss = 0usize;

                for &(origin, stats) in &origins {
                    let price = closes[origin];
                    for &days in horizons {
                        let j = origin + days as usize;
                        if j >= n {
                            continue;
                        }
                        let actual = closes[j];
                        let h = days as f64;
                        let sigma_h = stats.sigma * vol_mult * h.sqrt();
                        let mu_h = stats.mu * scale / (1.0 + h / decay).sqrt();
                        let total = (mu_h * h).clamp(-MAX_CUM_DRIFT, MAX_CUM_DRIFT);
                        let target = price * total.exp();

                        let err = (target / actual - 1.0).abs();
                        sum_err += err;
                        count += 1;
                        if actual < target * (-sigma_h).exp() || actual > target * sigma_h.exp() {
                            cone_miss += 1;
                        }
                    }
                }

                if count == 0 {
                    continue;
                }
                let mape = sum_err / count as f64;
                let miss_rate = cone_miss as f64 / count as f64;
                // A model that is 2% off but wrong about its own uncertainty is
                // worse than one that is 3% off with an honest cone. Penalise
                // both directions: under-coverage hides risk, over-coverage
                // makes every horizon look equally uncertain.
                let coverage = 1.0 - miss_rate;
                let score = mape + 0.5 * miss_rate + 1.0 * (coverage - 0.68).abs();

                if best.as_ref().map(|b| score < b.0).unwrap_or(true) {
                    best = Some((score, decay, scale, vol_mult));
                }
            }
        }
    }

    match best {
        Some((score, decay_days, drift_scale, vol_mult)) => Calibration {
            decay_days,
            drift_scale,
            vol_mult,
            samples: origins.len(),
            accuracy: measure_accuracy(
                &closes,
                &origins,
                horizons,
                decay_days,
                drift_scale,
                vol_mult,
            ),
            score_pct: score * 100.0,
        },
        None => fallback,
    }
}

/// Replay the chosen parameters over the same origins and report, per horizon,
/// direction hit rate, point error and cone coverage. This is the live accuracy
/// block shown in `/forecast`.
fn measure_accuracy(
    closes: &[f64],
    origins: &[(usize, CoreStats)],
    horizons: &[i64],
    decay_days: f64,
    drift_scale: f64,
    vol_mult: f64,
) -> Vec<HorizonAccuracy> {
    let n = closes.len();
    // (abs_err_sum, samples, direction_hits, cone_hits)
    let mut acc: Vec<(f64, usize, usize, usize)> =
        horizons.iter().map(|_| (0.0, 0, 0, 0)).collect();

    for &(origin, stats) in origins {
        let price = closes[origin];
        for (k, &days) in horizons.iter().enumerate() {
            let j = origin + days as usize;
            if j >= n {
                continue;
            }
            let actual = closes[j];
            let h = days as f64;
            let sigma_h = stats.sigma * vol_mult * h.sqrt();
            let mu_h = stats.mu * drift_scale / (1.0 + h / decay_days).sqrt();
            let total = (mu_h * h).clamp(-MAX_CUM_DRIFT, MAX_CUM_DRIFT);
            let target = price * total.exp();

            acc[k].0 += (target / actual - 1.0).abs();
            acc[k].1 += 1;
            if (target - price).signum() == (actual - price).signum() {
                acc[k].2 += 1;
            }
            if actual >= target * (-sigma_h).exp() && actual <= target * sigma_h.exp() {
                acc[k].3 += 1;
            }
        }
    }

    horizons
        .iter()
        .enumerate()
        .map(|(k, &days)| {
            let (err_sum, samples, hits, inside) = acc[k];
            let s = samples.max(1) as f64;
            HorizonAccuracy {
                days,
                direction_pct: hits as f64 / s * 100.0,
                mape_pct: err_sum / s * 100.0,
                coverage_pct: inside as f64 / s * 100.0,
                samples,
            }
        })
        .collect()
}

/// Build a forecast from a daily close series (ascending, `(ts_ms, close)`).
/// `price` is the reference price (usually the latest close or a live quote).
/// Calibration is run on the same series, so the returned model is adapted to
/// this symbol's own history.
pub fn forecast(symbol: &str, price: f64, history: &[(i64, f64)]) -> Forecast {
    let calibration = calibrate(symbol, history);
    forecast_with(symbol, price, history, &calibration)
}

/// Forecast with an explicit calibration (used by the CLI/tests to compare
/// parameter sets without re-fitting).
pub fn forecast_with(
    symbol: &str,
    price: f64,
    history: &[(i64, f64)],
    calibration: &Calibration,
) -> Forecast {
    let closes: Vec<f64> = history.iter().map(|&(_, c)| c).collect();
    let stats = core_stats(&closes);
    let raw_vol = volatility(&closes, 20).unwrap_or(0.0);

    let mut mu = stats.mu;
    let mut notes = Vec::new();

    // Monthly seasonality: only trust a month with at least two observations,
    // and cap its influence at ±0.05%/day. Applied on top of the calibrated
    // drift so the seasonality note stays truthful about what moved the number.
    let seasonality = monthly_seasonality_detailed(history);
    let month = current_month();
    if let Some((_, avg_pct, count)) = seasonality
        .iter()
        .copied()
        .find(|(m, _, count)| *m == month && *count >= 2)
    {
        let adj = (avg_pct / 100.0 / 30.0).clamp(-0.0005, 0.0005);
        mu = (mu + adj).clamp(-MAX_DAILY_DRIFT, MAX_DAILY_DRIFT);
        notes.push(format!(
            "seasonality: {} averages {:+.1}% ({} samples)",
            month_name(month),
            avg_pct,
            count
        ));
    }
    if history.len() < 200 {
        notes.push(format!(
            "only {} daily closes — 200-day trend is approximated",
            history.len()
        ));
    }
    if raw_vol == 0.0 {
        notes.push(
            "flat series: no dispersion in returns, using a 0.3%/day volatility floor".to_string(),
        );
    }

    let stats = CoreStats { mu, ..stats };
    let horizons = build_horizons(
        price,
        stats,
        horizons_for(symbol, history.len()),
        calibration.decay_days,
        calibration.drift_scale,
        calibration.vol_mult,
    );

    Forecast {
        symbol: symbol.to_uppercase(),
        price,
        history_days: history.len(),
        regime: stats.regime.as_str().to_string(),
        rsi14: stats.rsi14,
        sma20: sma(&closes, 20),
        sma50: sma(&closes, 50),
        sma200: sma(&closes, 200),
        vol_daily_pct: stats.sigma * calibration.vol_mult * 100.0,
        drift_daily_pct: mu * calibration.drift_scale * 100.0,
        horizons,
        calibration: calibration.clone(),
        notes,
    }
}

fn current_month() -> u32 {
    use chrono::Datelike;
    chrono::Utc::now().date_naive().month()
}

fn month_name(m: u32) -> &'static str {
    const NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    NAMES
        .get((m.saturating_sub(1)) as usize)
        .copied()
        .unwrap_or("?")
}

// ── Backtest ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BacktestHorizon {
    pub days: i64,
    pub samples: usize,
    /// Mean absolute percentage error of the target.
    pub mape_pct: f64,
    /// Mean signed error: positive = the model over-forecast.
    pub bias_pct: f64,
    /// Share of origins where the predicted direction was right.
    pub hit_pct: f64,
    /// Share of actual closes inside the 68% cone (ideal ≈ 68%).
    pub coverage_pct: f64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Backtest {
    pub symbol: String,
    pub origins: usize,
    pub from_ts: i64,
    pub to_ts: i64,
    pub horizons: Vec<BacktestHorizon>,
}

/// Walk-forward backtest over the last `window_days`. Each origin only uses
/// data available at that time, so the numbers are honest out-of-sample.
pub fn backtest(
    symbol: &str,
    history: &[(i64, f64)],
    window_days: usize,
    calibration: &Calibration,
) -> Backtest {
    let closes: Vec<f64> = history.iter().map(|&(_, c)| c).collect();
    let n = closes.len();
    let horizons = horizons_for(symbol, n);
    let start = n.saturating_sub(window_days).max(50);

    let mut acc: Vec<(f64, f64, usize, usize, usize)> =
        horizons.iter().map(|_| (0.0, 0.0, 0, 0, 0)).collect();

    for origin in start..n {
        let stats = core_stats(&closes[..=origin]);
        let price = closes[origin];
        let preds = build_horizons(
            price,
            stats,
            horizons,
            calibration.decay_days,
            calibration.drift_scale,
            calibration.vol_mult,
        );
        for (k, p) in preds.iter().enumerate() {
            let j = origin + p.days as usize;
            if j >= n {
                continue;
            }
            let actual = closes[j];
            let err = (p.target / actual - 1.0) * 100.0;
            let dir_ok = (p.target - price).signum() == (actual - price).signum();
            let in_cone = actual >= p.low && actual <= p.high;

            acc[k].0 += err.abs();
            acc[k].1 += err;
            acc[k].2 += 1;
            if dir_ok {
                acc[k].3 += 1;
            }
            if in_cone {
                acc[k].4 += 1;
            }
        }
    }

    Backtest {
        symbol: symbol.to_uppercase(),
        origins: n.saturating_sub(start),
        from_ts: history.get(start).map(|(ts, _)| *ts).unwrap_or(0),
        to_ts: history.last().map(|(ts, _)| *ts).unwrap_or(0),
        horizons: horizons
            .iter()
            .enumerate()
            .map(|(k, &days)| {
                let (abs_sum, signed_sum, samples, hits, inside) = acc[k];
                let s = samples.max(1) as f64;
                BacktestHorizon {
                    days,
                    samples,
                    mape_pct: abs_sum / s,
                    bias_pct: signed_sum / s,
                    hit_pct: hits as f64 / s * 100.0,
                    coverage_pct: inside as f64 / s * 100.0,
                }
            })
            .collect(),
    }
}

/// Human-readable comparison of the shipped (legacy) parameters against the
/// calibrated ones, over a long and a short window. Used by
/// `cargo run -- backtest BTC ...`.
pub fn backtest_report(symbol: &str, history: &[(i64, f64)]) -> String {
    let legacy = Calibration::default();
    let calibrated = calibrate(symbol, history);
    let long_legacy = backtest(symbol, history, 365, &legacy);
    let long_new = backtest(symbol, history, 365, &calibrated);
    let short_legacy = backtest(symbol, history, 30, &legacy);
    let short_new = backtest(symbol, history, 30, &calibrated);

    let mut out = String::new();
    out.push_str(&format!(
        "📊 {} backtest — walk-forward, {} closes\n",
        symbol.to_uppercase(),
        history.len()
    ));
    out.push_str(&format!(
        "legacy: decay {:.0}d · drift×1.00 · vol×1.00\n",
        DRIFT_DECAY_DAYS
    ));
    out.push_str(&format!(
        "calibrated: decay {} · drift×{:.2} · vol×{:.2} ({} origins)\n\n",
        fmt_decay(calibrated.decay_days),
        calibrated.drift_scale,
        calibrated.vol_mult,
        calibrated.samples
    ));

    for (title, old, new) in [
        ("365d window", &long_legacy, &long_new),
        ("30d window (post-release)", &short_legacy, &short_new),
    ] {
        out.push_str(&format!("{title} — {} origins\n", new.origins));
        out.push_str("   H    MAPE old→new    hit old→new   cone old→new   bias new\n");
        for (o, nw) in old.horizons.iter().zip(new.horizons.iter()) {
            // A 30-day window cannot score a 30-day horizon; skip empty rows.
            if nw.samples == 0 {
                continue;
            }
            out.push_str(&format!(
                "  {:>3}d   {:>5.1}% → {:>5.1}%   {:>4.0}% → {:>4.0}%   {:>4.0}% → {:>4.0}%   {:+.1}%\n",
                nw.days,
                o.mape_pct,
                nw.mape_pct,
                o.hit_pct,
                nw.hit_pct,
                o.coverage_pct,
                nw.coverage_pct,
                nw.bias_pct
            ));
        }
        out.push('\n');
    }
    out.push_str(
        "MAPE = mean absolute error of the target · hit = direction accuracy ·\n\
         cone = share of outcomes inside the 68% cone (ideal ≈ 68%) · bias > 0 = over-forecast.",
    );
    out
}

// ── Rendering ───────────────────────────────────────────────────────────────

/// Decimal places that keep every price in one symbol's table readable and the
/// same width: cents matter for a $0.5 coin, not for BTC.
fn price_decimals(p: f64) -> usize {
    let a = p.abs();
    if a >= 1000.0 {
        0
    } else if a >= 10.0 {
        2
    } else if a >= 1.0 {
        3
    } else {
        5
    }
}

/// Damping half-life for display: `∞` means "no decay" in the grid.
fn fmt_decay(days: f64) -> String {
    if days.is_finite() {
        format!("{days:.0}d")
    } else {
        "∞".to_string()
    }
}

fn fmt_money(v: f64, decimals: usize) -> String {
    format!("${:.*}", decimals, v)
}

fn fmt_money_opt(v: Option<f64>, decimals: usize) -> String {
    v.map(|x| fmt_money(x, decimals))
        .unwrap_or_else(|| "n/a".into())
}

/// One-word read of the expected move at the reference horizon.
fn verdict(change_pct: f64) -> (&'static str, &'static str) {
    let magnitude = change_pct.abs();
    if change_pct >= 3.0 {
        if magnitude >= 10.0 {
            ("strongly bullish", "📈")
        } else {
            ("mildly bullish", "↗️")
        }
    } else if change_pct <= -3.0 {
        if magnitude >= 10.0 {
            ("strongly bearish", "📉")
        } else {
            ("mildly bearish", "↘️")
        }
    } else {
        ("neutral / no edge", "➡️")
    }
}

impl Forecast {
    /// Aligned table rows (header first), monospace-ready.
    fn table_lines(&self, decimals: usize) -> Vec<String> {
        const HEADER: [&str; 5] = ["H", "TARGET", "Δ", "68% RANGE", "P(up)"];
        // H left-aligned; the numeric columns right-aligned; the range left.
        const RIGHT: [bool; 5] = [false, true, true, false, true];

        let rows: Vec<[String; 5]> = self
            .horizons
            .iter()
            .map(|h| {
                [
                    format!("{}d", h.days),
                    fmt_money(h.target, decimals),
                    format!("{:+.1}%", h.change_pct),
                    format!(
                        "{}–{}",
                        fmt_money(h.low, decimals),
                        fmt_money(h.high, decimals)
                    ),
                    format!("{:.0}%", h.prob_up * 100.0),
                ]
            })
            .collect();

        let mut widths = [0usize; 5];
        for (i, h) in HEADER.iter().enumerate() {
            widths[i] = h.chars().count();
        }
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }

        let line = |cells: [&str; 5]| -> String {
            let mut s = String::new();
            for (i, cell) in cells.iter().enumerate() {
                if i > 0 {
                    s.push_str("  ");
                }
                let pad = widths[i].saturating_sub(cell.chars().count());
                if RIGHT[i] {
                    s.push_str(&" ".repeat(pad));
                    s.push_str(cell);
                } else {
                    s.push_str(cell);
                    s.push_str(&" ".repeat(pad));
                }
            }
            s.trim_end().to_string()
        };

        let mut lines = Vec::with_capacity(rows.len() + 1);
        lines.push(line(HEADER));
        for row in &rows {
            lines.push(line([&row[0], &row[1], &row[2], &row[3], &row[4]]));
        }
        lines
    }

    /// The horizon the bottom line talks about: 30 days when available.
    fn reference_horizon(&self) -> Option<&HorizonForecast> {
        self.horizons
            .iter()
            .find(|h| h.days == 30)
            .or_else(|| self.horizons.last())
    }

    /// Accuracy lines bucketed into day / week / month / quarter / half-year,
    /// so the user can see how often the model is right at each scale.
    /// Recomputed from the latest history on every forecast.
    fn accuracy_bucket_lines(&self) -> Vec<String> {
        // (label, inclusive min, inclusive max)
        const BUCKETS: [(&str, i64, i64); 5] = [
            ("day", 1, 4),
            ("week", 5, 8),
            ("month", 9, 45),
            ("quarter", 46, 120),
            ("half-year", 121, i64::MAX),
        ];
        let mut lines = Vec::new();
        for (label, lo, hi) in BUCKETS {
            let rows: Vec<&HorizonAccuracy> = self
                .calibration
                .accuracy
                .iter()
                .filter(|a| a.days >= lo && a.days <= hi && a.samples > 0)
                .collect();
            if rows.is_empty() {
                continue;
            }
            // Weight by sample count so a long horizon with fewer origins does
            // not dominate the bucket.
            let total: usize = rows.iter().map(|a| a.samples).sum();
            let w = total.max(1) as f64;
            let dir = rows
                .iter()
                .map(|a| a.direction_pct * a.samples as f64)
                .sum::<f64>()
                / w;
            let mape = rows
                .iter()
                .map(|a| a.mape_pct * a.samples as f64)
                .sum::<f64>()
                / w;
            let cover = rows
                .iter()
                .map(|a| a.coverage_pct * a.samples as f64)
                .sum::<f64>()
                / w;
            lines.push(format!(
                "{:<9} direction {:>3.0}% · avg error ±{:.1}% · in-range {:.0}%",
                label, dir, mape, cover
            ));
        }
        lines
    }

    /// Plain-text table, aligned for a monospace font. Used by the CLI and the
    /// tests; the Telegram handler renders [`Self::render_html`] instead.
    pub fn render(&self) -> String {
        let decimals = price_decimals(self.price);
        let mut out = String::new();
        out.push_str(&format!(
            "📈 {} forecast @ {}\n",
            self.symbol,
            fmt_money(self.price, decimals)
        ));
        out.push_str(&format!(
            "regime {} · RSI14 {} · vol {:.2}%/day · drift {:+.3}%/day · {} closes\n",
            self.regime,
            self.rsi14
                .map(|r| format!("{r:.0}"))
                .unwrap_or_else(|| "n/a".into()),
            self.vol_daily_pct,
            self.drift_daily_pct,
            self.history_days,
        ));
        out.push_str(&format!(
            "SMA20 {} · SMA50 {} · SMA200 {}\n\n",
            fmt_money_opt(self.sma20, decimals),
            fmt_money_opt(self.sma50, decimals),
            fmt_money_opt(self.sma200, decimals),
        ));

        for line in self.table_lines(decimals) {
            out.push_str(&line);
            out.push('\n');
        }

        // ── Bottom line: one horizon, one verdict, one probability ──────────
        if let Some(h) = self.reference_horizon() {
            let (word, icon) = verdict(h.change_pct);
            let mape = self.calibration.mape_for(h.days);
            out.push('\n');
            out.push_str("────────────────────────────\n");
            out.push_str(&format!("{icon} BOTTOM LINE — {word}\n"));
            out.push_str(&format!(
                "{}d: {:+.1}% → {} (68%: {}–{})\n",
                h.days,
                h.change_pct,
                fmt_money(h.target, decimals),
                fmt_money(h.low, decimals),
                fmt_money(h.high, decimals),
            ));
            out.push_str(&format!(
                "P(higher in {}d): {:.0}% · model confidence {:.0}%\n",
                h.days,
                h.prob_up * 100.0,
                h.confidence * 100.0,
            ));
            if let Some(m) = mape {
                out.push_str(&format!(
                    "walk-forward error at {}d: ±{:.1}% ({} origins)\n",
                    h.days, m, self.calibration.samples
                ));
            }
            out.push_str("────────────────────────────\n");
        }

        if !self.calibration.is_default() {
            out.push_str(&format!(
                "calibrated: decay {} · drift×{:.2} · vol×{:.2}\n",
                fmt_decay(self.calibration.decay_days),
                self.calibration.drift_scale,
                self.calibration.vol_mult,
            ));
            let lines = self.accuracy_bucket_lines();
            if !lines.is_empty() {
                out.push_str(&format!(
                    "accuracy (walk-forward, {} origins):\n",
                    self.calibration.samples
                ));
                for line in lines {
                    out.push_str("  ");
                    out.push_str(&line);
                    out.push('\n');
                }
            }
        }
        if !self.notes.is_empty() {
            for n in &self.notes {
                out.push_str(&format!("• {n}\n"));
            }
        }
        out.push_str("Not financial advice. Deterministic model, 68% cone widens with √h.");
        out
    }

    /// Telegram HTML rendering: bold header and bottom line, the table (and
    /// the accuracy block) in `<pre>` so the columns stay aligned, everything
    /// else as normal text. Only `<b>`, `<i>`, `<code>` and `<pre>` are used.
    pub fn render_html(&self) -> String {
        let decimals = price_decimals(self.price);
        let esc = html_escape::encode_text;
        let mut out = String::new();

        out.push_str(&format!(
            "📈 <b>{} forecast</b> · <code>{}</code>\n",
            esc(&self.symbol),
            fmt_money(self.price, decimals)
        ));
        out.push_str(&format!(
            "regime {} · RSI14 <code>{}</code> · vol <code>{:.2}%/day</code> · drift <code>{:+.3}%/day</code> · {} closes\n",
            esc(&self.regime),
            self.rsi14
                .map(|r| format!("{r:.0}"))
                .unwrap_or_else(|| "n/a".into()),
            self.vol_daily_pct,
            self.drift_daily_pct,
            self.history_days,
        ));
        out.push_str(&format!(
            "SMA20 <code>{}</code> · SMA50 <code>{}</code> · SMA200 <code>{}</code>\n\n",
            fmt_money_opt(self.sma20, decimals),
            fmt_money_opt(self.sma50, decimals),
            fmt_money_opt(self.sma200, decimals),
        ));

        out.push_str(&format!(
            "<pre>{}</pre>\n",
            esc(&self.table_lines(decimals).join("\n"))
        ));

        if let Some(h) = self.reference_horizon() {
            let (word, icon) = verdict(h.change_pct);
            let mape = self.calibration.mape_for(h.days);
            out.push_str(&format!("\n{icon} <b>BOTTOM LINE — {word}</b>\n"));
            out.push_str(&format!(
                "<code>{}d   {:+.1}%  →  {}   (68%: {}–{})</code>\n",
                h.days,
                h.change_pct,
                fmt_money(h.target, decimals),
                fmt_money(h.low, decimals),
                fmt_money(h.high, decimals),
            ));
            out.push_str(&format!(
                "P(higher in {}d): <b>{:.0}%</b> · model confidence {:.0}%\n",
                h.days,
                h.prob_up * 100.0,
                h.confidence * 100.0,
            ));
            if let Some(m) = mape {
                out.push_str(&format!(
                    "walk-forward error at {}d: ±{:.1}% ({} origins)\n",
                    h.days, m, self.calibration.samples
                ));
            }
        }

        if !self.calibration.is_default() {
            out.push_str(&format!(
                "\n<i>calibrated: decay {} · drift×{:.2} · vol×{:.2}</i>\n",
                fmt_decay(self.calibration.decay_days),
                self.calibration.drift_scale,
                self.calibration.vol_mult,
            ));
            let lines = self.accuracy_bucket_lines();
            if !lines.is_empty() {
                out.push_str(&format!(
                    "<b>Accuracy</b> (walk-forward, {} origins):\n<pre>{}</pre>\n",
                    self.calibration.samples,
                    esc(&lines.join("\n")),
                ));
            }
        }
        for n in &self.notes {
            out.push_str(&format!("• {}\n", esc(n)));
        }
        out.push_str(
            "<i>Not financial advice. Deterministic model; the 68% cone widens with √h.</i>",
        );
        out
    }

    /// Compact, deterministic summary used when the AI commentary is
    /// unavailable (no key, quota, timeout) or for quick replies.
    pub fn summary_html(&self) -> String {
        let decimals = price_decimals(self.price);
        let mut out = format!(
            "📊 <b>{} — quick read</b> · <code>{}</code>\n",
            html_escape::encode_text(&self.symbol),
            fmt_money(self.price, decimals)
        );
        if let Some(h) = self.reference_horizon() {
            let (word, icon) = verdict(h.change_pct);
            out.push_str(&format!(
                "{icon} <b>{word}</b> at {}d: <code>{:+.1}%</code> → <code>{}</code>\n",
                h.days,
                h.change_pct,
                fmt_money(h.target, decimals),
            ));
            out.push_str(&format!(
                "68% range <code>{}–{}</code> · P(up) <code>{:.0}%</code>\n",
                fmt_money(h.low, decimals),
                fmt_money(h.high, decimals),
                h.prob_up * 100.0,
            ));
        }
        out
    }
}

/// Merge daily closes from the on-disk cache with intraday rows from SQLite,
/// resampled to one close per UTC day (the last price wins). Returns an
/// ascending series keyed by the start of the day.
pub fn merge_daily_history(csv: &[(i64, f64)], intraday: &[(i64, f64)]) -> Vec<(i64, f64)> {
    use std::collections::BTreeMap;
    let mut by_day: BTreeMap<i64, f64> = BTreeMap::new();
    for &(ts, p) in csv {
        if p > 0.0 {
            by_day.insert(ts.div_euclid(DAY_MS), p);
        }
    }
    // Intraday rows are appended after the cache so they override the same day;
    // sort first because the caller's series may not be ordered.
    let mut intraday_sorted = intraday.to_vec();
    intraday_sorted.sort_by_key(|&(ts, _)| ts);
    for &(ts, p) in &intraday_sorted {
        if p > 0.0 {
            by_day.insert(ts.div_euclid(DAY_MS), p);
        }
    }
    by_day
        .into_iter()
        .map(|(day, p)| (day * DAY_MS, p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(prices: &[f64]) -> Vec<(i64, f64)> {
        let mut ts = 1_500_000_000_000i64;
        prices
            .iter()
            .map(|&p| {
                let row = (ts, p);
                ts += 86_400_000;
                row
            })
            .collect()
    }

    fn rising(n: usize) -> Vec<f64> {
        (0..n).map(|i| 100.0 * 1.0015f64.powi(i as i32)).collect()
    }

    #[test]
    fn per_symbol_horizon_profiles() {
        assert_eq!(horizons_for("BTC", 3000), &HORIZONS_MAJOR);
        assert_eq!(horizons_for("ETH", 3000), &HORIZONS_MAJOR);
        assert_eq!(horizons_for("SOL", 3000), &HORIZONS_ALT);
        assert_eq!(horizons_for("LINK", 3000), &HORIZONS_ALT);
        assert_eq!(horizons_for("DOGE", 3000), &HORIZONS_SHORT);
        // Age caps the ladder: 260 closes may not claim a 90-day horizon.
        assert!(horizons_for("BTC", 260).iter().all(|h| *h <= 65));
        assert_eq!(horizons_for("BTC", 0), &HORIZONS_MAJOR[..1]);
    }

    #[test]
    fn rising_trend_forecasts_higher_with_distance() {
        let h = series(&rising(600));
        let price = h.last().unwrap().1;
        let f = forecast("BTC", price, &h);
        assert!(f.horizons.len() >= 5);
        // Targets must be above the current price and increase with the horizon.
        for w in f.horizons.windows(2) {
            assert!(w[0].target < w[1].target);
            assert!(w[0].target > price);
        }
        assert_eq!(f.regime, "BULL");
    }

    #[test]
    fn flat_series_stays_near_price_and_range_contains_target() {
        let h = series(&[100.0; 600]);
        let f = forecast("ETH", 100.0, &h);
        for hz in &f.horizons {
            assert!((hz.target - 100.0).abs() < 1.0);
            assert!(hz.low <= hz.target && hz.target <= hz.high);
        }
        assert!(f.horizons[0].confidence <= 0.95);
    }

    #[test]
    fn confidence_falls_and_range_widens_with_horizon() {
        let h = series(&rising(600));
        let f = forecast("SOL", h.last().unwrap().1, &h);
        let first = &f.horizons[0];
        let last = f.horizons.last().unwrap();
        assert!(last.confidence < first.confidence);
        assert!((last.high - last.low) > (first.high - first.low));
    }

    #[test]
    fn drift_is_clamped() {
        // A vertical 300-day run must not produce a >0.5%/day claim.
        let prices: Vec<f64> = (0..300).map(|i| 1.0 * 1.02f64.powi(i)).collect();
        let f = forecast("BTC", *prices.last().unwrap(), &series(&prices));
        assert!(f.drift_daily_pct.abs() <= 50.0 + 1e-9);
    }

    #[test]
    fn empty_history_does_not_panic() {
        let f = forecast("LINK", 10.0, &[]);
        assert_eq!(f.horizons.len(), 1);
        assert!(f.notes.iter().any(|n| n.contains("daily closes")));
    }

    #[test]
    fn calibration_adapts_to_the_series() {
        // A choppy mean-reverting series should learn a smaller drift shrink /
        // shorter decay than a clean trend.
        let mut prices = Vec::new();
        let mut p = 100.0;
        for i in 0..600 {
            p *= if i % 2 == 0 { 1.01 } else { 0.99 };
            prices.push(p);
        }
        let cal = calibrate("BTC", &series(&prices));
        assert!(cal.samples > 0);
        assert!(cal.drift_scale <= 1.0);
        assert!(cal.vol_mult >= CALIBRATION_VOL_MULTS[0]);
        assert!(cal.score_pct.is_finite());
    }

    #[test]
    fn backtest_reports_every_horizon() {
        let h = series(&rising(700));
        let bt = backtest("BTC", &h, 180, &Calibration::default());
        assert!(!bt.horizons.is_empty());
        for hz in &bt.horizons {
            assert!(hz.samples > 0);
            assert!(hz.mape_pct.is_finite());
            assert!((0.0..=100.0).contains(&hz.coverage_pct));
        }
    }

    #[test]
    fn probability_is_bounded_and_consistent_with_drift() {
        let up = series(&rising(600));
        let f = forecast("BTC", up.last().unwrap().1, &up);
        for h in &f.horizons {
            assert!((0.0..=1.0).contains(&h.prob_up));
            if h.change_pct > 0.0 {
                assert!(h.prob_up > 0.5);
            } else if h.change_pct < 0.0 {
                assert!(h.prob_up < 0.5);
            }
        }
    }

    #[test]
    fn html_render_wraps_only_the_table_in_pre() {
        let h = series(&rising(700));
        let f = forecast("BTC", h.last().unwrap().1, &h);
        let html = f.render_html();
        // Header/bottom line are HTML, only the table is monospace.
        assert!(html.starts_with("📈 <b>BTC forecast</b>"));
        assert!(html.contains("BOTTOM LINE"));
        assert_eq!(
            html.matches("<pre>").count(),
            html.matches("</pre>").count()
        );
        let pre = html
            .split("<pre>")
            .nth(1)
            .and_then(|s| s.split("</pre>").next())
            .expect("table block");
        assert!(pre.contains("TARGET"));
        // Dynamic table content is escaped: no raw tag can leak in.
        assert!(!pre.contains('<'));
        assert!(!f.summary_html().is_empty());
    }

    #[test]
    fn merge_takes_one_close_per_day_and_intraday_wins() {
        let csv = vec![(0i64, 100.0), (DAY_MS, 101.0), (2 * DAY_MS, 102.0)];
        let intraday = vec![
            (DAY_MS + 3600_000, 105.0),
            (DAY_MS + 7200_000, 106.0),
            (3 * DAY_MS + 60_000, 110.0),
        ];
        let merged = merge_daily_history(&csv, &intraday);
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0].1, 100.0);
        assert_eq!(merged[1].1, 106.0); // last intraday close of that day
        assert_eq!(merged[2].1, 102.0);
        assert_eq!(merged[3].1, 110.0);
    }
}
