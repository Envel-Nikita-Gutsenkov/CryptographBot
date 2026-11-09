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
//!   clamped to ±0.5%/day so a parabolic week cannot produce nonsense. In a
//!   `SIDEWAYS` regime the momentum term is zeroed: a flat market has no trend
//!   to extrapolate, and letting a 20-day slope leak in as -0.1%/day turned a
//!   coin-flip into a fake 30-day downtrend.
//! * `sigma` — daily volatility: the 20-close stddev of log returns blended
//!   50/50 with a RiskMetrics EWMA (λ=0.94) so recent shocks weigh more.
//! * target = price · e^(mu·s·h / sqrt(1 + h/decay)) — momentum decays with
//!   the horizon so a hot week is not extrapolated into a fantasy year.
//! * range  = target · e^(±q·sigma·√h) where `q` is the Student-t quantile
//!   (ν=4, q≈1.14) instead of the normal 1σ: crypto has fat tails, and a pure
//!   Gaussian cone under-covered the 90-day horizon (48–58% inside vs 68%).
//! * P(up)  = 1 − T(−mu·s·√h / sigma) — same Student-t distribution.
//!
//! Cone width and the drift term are then re-fitted per symbol by
//! [`calibrate`], so the wider t-quantile is not double-counted with the
//! volatility multiplier.
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
use crate::trading::market_calendar::monthly_seasonality_stats;
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
/// Effective sample size behind the drift estimate. The drift is fitted to the
/// last few hundred closes, so it carries an estimation error of ~sigma/√N that
/// compounds *linearly* with the horizon — the term a pure random-walk cone
/// ignores, and the reason a 90-day cone built from sigma·√h covered only
/// ~50% of outcomes instead of 68%.
const DRIFT_ESTIMATE_DAYS: f64 = 200.0;

/// Calibration looks back this far and samples every `CALIBRATION_STEP_DAYS`.
const CALIBRATION_WINDOW_DAYS: usize = 400;
const CALIBRATION_STEP_DAYS: usize = 3;
/// Candidate damping half-lives (days). `INFINITY` = no damping at all.
const CALIBRATION_DECAYS: [f64; 8] = [10.0, 20.0, 30.0, 45.0, 60.0, 90.0, 180.0, f64::INFINITY];
/// Candidate drift shrink factors. Zero is excluded: a model that always
/// predicts "no change" is useless even if its MAPE is slightly lower.
const CALIBRATION_SCALES: [f64; 4] = [0.3, 0.5, 0.75, 1.0];
/// Candidate cone-width multipliers around the measured volatility. The range
/// reaches below 0.8 so calibration can *narrow* the cone after the Student-t
/// quantile widened it — otherwise the two effects would double-count.
const CALIBRATION_VOL_MULTS: [f64; 6] = [0.7, 0.85, 1.0, 1.15, 1.3, 1.45];

/// Degrees of freedom of the Student-t cone. ν=4 gives ±14% wider 1σ bounds
/// than the Gaussian and matches the fat-tailed daily returns of crypto.
pub(crate) const STUDENT_DF: f64 = 4.0;
/// EWMA decay for the volatility estimate (RiskMetrics daily λ).
const EWMA_LAMBDA: f64 = 0.94;

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

/// Drift and dispersion the cone actually uses at one horizon.
///
/// Every module that needs a probability over that horizon (`/dip`, the bottom
/// radar) must take its numbers from here instead of re-deriving them from the
/// raw daily drift: the raw drift is undamped and ignores the estimation error
/// of the fit, which is exactly how `/dip` and `/forecast` ended up disagreeing
/// about the same cone.
#[derive(Debug, Clone, Copy)]
pub struct HorizonMoments {
    /// Total log drift over the horizon (damped and clamped).
    pub log_drift: f64,
    /// Log-space dispersion over the horizon: diffusion plus drift error.
    pub sigma_h: f64,
    /// One-day log sigma, used for the discrete-monitoring correction.
    pub sigma_daily: f64,
}

/// Real walk-forward accuracy for one horizon, recomputed on every call from
/// the freshest history. This is the "how often is the model actually right"
/// number the user sees, not a promise.
#[derive(Debug, Clone)]
pub struct HorizonAccuracy {
    pub days: i64,
    /// Origins where the predicted direction was right. Kept as a count (not
    /// only a percentage) so a binomial interval can be computed.
    pub hits: usize,
    /// Mean absolute error of the point target, percent.
    pub mape_pct: f64,
    /// Share of outcomes inside the 68% cone (ideal ≈ 68%), percent.
    pub coverage_pct: f64,
    pub samples: usize,
}

/// Two-sided z for the 95% interval reported to the user.
const EDGE_Z: f64 = 1.96;

/// What a direction hit rate can honestly claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeVerdict {
    /// The 95% interval straddles 50%: indistinguishable from a coin flip.
    NoEdge,
    /// The whole interval is above 50%: a real (if small) directional edge.
    AboveChance,
    /// The whole interval is below 50%: the drift reliably points the wrong
    /// way, so the honest read is the opposite of the model's arrow.
    BelowChance,
}

impl EdgeVerdict {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoEdge => "no edge",
            Self::AboveChance => "above 50%",
            Self::BelowChance => "below 50%",
        }
    }
}

/// Direction hit rate with the uncertainty that belongs to it.
#[derive(Debug, Clone, Copy)]
pub struct DirectionEdge {
    /// Observed hit rate, percent.
    pub pct: f64,
    /// 95% Wilson score interval, percent.
    pub ci_low: f64,
    pub ci_high: f64,
    /// Origins that are effectively independent for this horizon — *not* the
    /// raw sample count. Walk-forward windows overlap; see [`effective_origins`].
    pub effective_n: f64,
    pub verdict: EdgeVerdict,
}

impl DirectionEdge {
    /// Interval as `27–57%`.
    pub fn ci(&self) -> String {
        format!("{:.0}–{:.0}%", self.ci_low, self.ci_high)
    }
}

/// Wilson score interval for a binomial proportion, returned in percent.
/// Preferred over the normal approximation because it stays inside 0–100 and
/// is not garbage at the small effective counts long horizons leave us with.
fn wilson_ci(p: f64, n: f64, z: f64) -> (f64, f64) {
    if !p.is_finite() || !n.is_finite() || n < 1.0 {
        return (0.0, 100.0);
    }
    let p = p.clamp(0.0, 1.0);
    let n = n.max(1.0);
    let z2 = z * z;
    let denom = 1.0 + z2 / n;
    let centre = (p + z2 / (2.0 * n)) / denom;
    let half = z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt() / denom;
    (
        ((centre - half) * 100.0).clamp(0.0, 100.0),
        ((centre + half) * 100.0).clamp(0.0, 100.0),
    )
}

/// Number of origins that behave as independent samples for a horizon of
/// `days`.
///
/// Calibration walks forward in `CALIBRATION_STEP_DAYS` steps, so a 30-day
/// horizon scored on 133 origins really only contains ~13 non-overlapping
/// 30-day windows; a 180-day horizon has ~2. Using the raw count would shrink
/// the interval by √10 and turn a coin flip into a fake edge. This denominator
/// is what makes the reported CI honest rather than flattering.
pub fn effective_origins(samples: usize, days: i64) -> f64 {
    if samples == 0 {
        return 0.0;
    }
    let span = samples as f64 * CALIBRATION_STEP_DAYS as f64;
    (span / days.max(1) as f64).min(samples as f64).max(2.0)
}

/// Build the edge report from a hit count and an explicit effective count.
fn edge_from_counts(hits: usize, samples: usize, effective_n: f64) -> DirectionEdge {
    if samples == 0 {
        return DirectionEdge {
            pct: f64::NAN,
            ci_low: f64::NAN,
            ci_high: f64::NAN,
            effective_n,
            verdict: EdgeVerdict::NoEdge,
        };
    }
    let pct = hits as f64 / samples as f64 * 100.0;
    let (ci_low, ci_high) = wilson_ci(pct / 100.0, effective_n, EDGE_Z);
    let verdict = if ci_low > 50.0 {
        EdgeVerdict::AboveChance
    } else if ci_high < 50.0 {
        EdgeVerdict::BelowChance
    } else {
        EdgeVerdict::NoEdge
    };
    DirectionEdge {
        pct,
        ci_low,
        ci_high,
        effective_n,
        verdict,
    }
}

impl HorizonAccuracy {
    pub fn edge(&self) -> DirectionEdge {
        edge_from_counts(
            self.hits,
            self.samples,
            effective_origins(self.samples, self.days),
        )
    }
}

/// Walk-forward-fitted knobs for one symbol.
#[derive(Debug, Clone)]
pub struct Calibration {
    pub decay_days: f64,
    pub drift_scale: f64,
    pub vol_mult: f64,
    /// Shrinkage applied to the drift *when converting it into `prob_up`*.
    ///
    /// The point target and cone keep the full drift; `prob_up` is a logistic
    /// blend of the series' base rate and `drift · prob_drift_scale`. The scale
    /// is fitted so the probability never scores worse than the base rate alone.
    /// When momentum carries no directional information the fit returns `0.0`.
    pub prob_drift_scale: f64,
    /// Base rate (`P(up)` over all scored origin/horizon pairs) used as the
    /// intercept of the probability model. Clamped away from 0 and 1 so the
    /// logit is finite.
    pub prob_base: f64,
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
            prob_drift_scale: 0.0,
            prob_base: 0.5,
            samples: 0,
            accuracy: Vec::new(),
            score_pct: f64::NAN,
        }
    }
}

impl Calibration {
    /// Accuracy row for the horizon closest to `days`.
    pub fn accuracy_for(&self, days: i64) -> Option<&HorizonAccuracy> {
        self.accuracy
            .iter()
            .filter(|a| a.samples > 0)
            .min_by_key(|a| (a.days - days).abs())
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

/// RiskMetrics EWMA volatility of daily log returns (λ=0.94), as a fraction
/// per day. Seeded from the first 20 squared returns so the recursion does not
/// start from zero variance. `None` when there are too few usable closes.
fn ewma_vol(closes: &[f64], lambda: f64) -> Option<f64> {
    let rets: Vec<f64> = closes
        .windows(2)
        .filter(|w| w[0] > 0.0 && w[1] > 0.0)
        .map(|w| (w[1] / w[0]).ln())
        .collect();
    if rets.len() < 20 {
        return None;
    }
    let seed = rets[..20].iter().map(|r| r * r).sum::<f64>() / 20.0;
    let mut var = seed;
    for r in &rets {
        var = lambda * var + (1.0 - lambda) * r * r;
    }
    Some(var.sqrt())
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
        // A flat market has no trend to extrapolate. Extrapolating a 20-day
        // slope here is what produced "SIDEWAYS · drift -0.097%/day" and a
        // fabricated -2% 30-day target; zero it and let RSI carry the only
        // tilt below.
        MarketRegime::Sideways => mu = 0.0,
        // Near the ATH with hot momentum: mean reversion risk dominates.
        MarketRegime::Euphoria => mu *= 0.5,
        // Buying-the-fear zone: expect a bounce, but keep it modest.
        MarketRegime::Dip => mu += 0.0008,
        MarketRegime::Bear => mu -= 0.0003,
        MarketRegime::Bull => mu += 0.0002,
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
    // Blend the 20-day sample stddev (stable, slow) with the EWMA estimate
    // (responsive, weights the latest shocks). Either alone has a known bias:
    // the sample stddev lags a regime change, the EWMA overreacts to one spike.
    let raw_vol = volatility(closes, 20).unwrap_or(0.0);
    let ewma_pct = ewma_vol(closes, EWMA_LAMBDA)
        .map(|v| v * 100.0)
        .unwrap_or(0.0);
    let blended_vol = match (raw_vol > 0.0, ewma_pct > 0.0) {
        (true, true) => 0.5 * raw_vol + 0.5 * ewma_pct,
        _ => raw_vol.max(ewma_pct),
    };
    CoreStats {
        mu: daily_drift(closes, regime, rsi14, raw_vol > 0.0),
        sigma: (blended_vol / 100.0).clamp(MIN_DAILY_SIGMA, 10.0),
        regime,
        rsi14,
    }
}

/// Build the horizon table for one parameter set.
/// The single horizon model shared by the cone and every probability built on
/// top of it.
///
/// `mu_daily` and `sigma_daily` are already scaled by the calibration
/// (`drift_scale`, `vol_mult`). Momentum decays with the horizon instead of
/// compounding linearly: a 0.5%/day recent run is credible for a week, not for
/// half a year.
pub(crate) fn horizon_moments(
    mu_daily: f64,
    sigma_daily: f64,
    days: i64,
    decay_days: f64,
) -> HorizonMoments {
    let h = days.max(1) as f64;
    let decay = if decay_days.is_finite() && decay_days > 0.0 {
        decay_days
    } else {
        f64::INFINITY
    };
    let sigma_daily = if sigma_daily.is_finite() && sigma_daily > 0.0 {
        sigma_daily
    } else {
        MIN_DAILY_SIGMA
    };
    let mu_h = mu_daily / (1.0 + h / decay).sqrt();
    HorizonMoments {
        log_drift: (mu_h * h).clamp(-MAX_CUM_DRIFT, MAX_CUM_DRIFT),
        sigma_h: cone_sigma_h(sigma_daily, h),
        sigma_daily,
    }
}

impl Forecast {
    /// Drift and dispersion this forecast actually applies at `days`.
    ///
    /// Reconstructed from the two percent fields the report prints, so a caller
    /// that only has a `Forecast` (the `/dip` command, the alert engine) gets
    /// bit-for-bit the same numbers `build_horizons` used.
    pub fn moments(&self, days: i64) -> HorizonMoments {
        horizon_moments(
            self.drift_daily_pct / 100.0,
            self.vol_daily_pct / 100.0,
            days,
            self.calibration.decay_days,
        )
    }
}

fn build_horizons(
    price: f64,
    stats: CoreStats,
    horizons: &[i64],
    decay_days: f64,
    drift_scale: f64,
    vol_mult: f64,
    prob_drift_scale: f64,
    prob_base: f64,
) -> Vec<HorizonForecast> {
    // Student-t instead of Gaussian: one quantile, computed once per call.
    let q68 = t68_mult();
    // Base-rate probability as a logit, so the drift term can shift it
    // symmetrically without ever leaving (0, 1).
    let base = prob_base.clamp(0.02, 0.98);
    let intercept = (base / (1.0 - base)).ln();
    horizons
        .iter()
        .map(|&days| {
            let m = horizon_moments(
                stats.mu * drift_scale,
                stats.sigma * vol_mult,
                days,
                decay_days,
            );
            let sigma_h = m.sigma_h;
            let total_drift = m.log_drift;
            let target = price * total_drift.exp();
            let low = target * (-q68 * sigma_h).exp();
            let high = target * (q68 * sigma_h).exp();
            let confidence = (1.0 / (1.0 + 4.0 * sigma_h)).clamp(0.05, 0.95);
            // The cone and target report the fitted drift. `prob_up` deliberately
            // shrinks it: the walk-forward fit measures how much directional
            // information that drift actually carries, and on this data it is
            // close to none. Keeping the raw drift here is what produced 80–90%
            // "up" buckets that resolved 40–67% of the time (negative Brier
            // skill). With scale 0 this is exactly the base rate.
            let z = total_drift * prob_drift_scale / sigma_h.max(f64::EPSILON);
            let prob_up = logistic(intercept + z);
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

/// Width of the ~68% cone in log-space for a `days` horizon.
///
/// Two sources of uncertainty:
/// * diffusion — `sigma·√h`, the random-walk term;
/// * drift estimation error — `(sigma/√N)·h`: the fitted drift is only known to
///   that precision, and its error grows linearly with the horizon.
/// They add in quadrature. At `h=1` the second term is negligible, at `h=90` it
/// adds ~20% width, at `h=180` ~38% — where a pure Gaussian cone under-covers.
pub(crate) fn cone_sigma_h(sigma: f64, h: f64) -> f64 {
    let diffusion = sigma * sigma * h;
    let drift_err = sigma * sigma / DRIFT_ESTIMATE_DAYS * h * h;
    (diffusion + drift_err).sqrt()
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

/// Lanczos approximation of ln Γ(x) for x > 0 (g=7, n=9). Accurate to ~1e-13,
/// which is far beyond what a quantile bisection needs.
fn ln_gamma(x: f64) -> f64 {
    const COF: [f64; 9] = [
        0.999_999_999_999_809_9,
        676.520_368_121_885_1,
        -1_259.139_216_722_402_8,
        771.323_428_777_653_1,
        -176.615_029_162_140_6,
        12.507_343_278_686_905,
        -0.138_571_095_265_720_12,
        9.984_369_578_019_572e-6,
        1.505_632_735_149_311_6e-7,
    ];
    if x < 0.5 {
        // Reflection: Γ(x)Γ(1-x) = π / sin(πx)
        return (std::f64::consts::PI / (std::f64::consts::PI * x).sin()).ln() - ln_gamma(1.0 - x);
    }
    let x = x - 1.0;
    let mut a = COF[0];
    let t = x + 7.5;
    for (i, &c) in COF.iter().enumerate().skip(1) {
        a += c / (x + i as f64);
    }
    0.5 * (2.0 * std::f64::consts::PI).ln() + (x + 0.5) * t.ln() - t + a.ln()
}

/// Continued fraction for the incomplete beta:
/// `I_x(a,b) = x^a(1-x)^b / (a·B(a,b)) · CF` with
/// `CF = 1/(1 + d₁/(1 + d₂/(1 + …)))`.
///
/// Evaluated backwards from a deep term, which is unconditionally stable for
/// fractional `b` (the forward Lentz variant drifted by up to 2× for b=0.5 and
/// even went negative — and that is the exact case the t-distribution uses).
fn beta_cf(a: f64, b: f64, x: f64) -> f64 {
    const DEPTH: usize = 400;
    fn d_term(a: f64, b: f64, x: f64, m: usize) -> f64 {
        if m % 2 == 1 {
            let k = ((m - 1) / 2) as f64;
            -(a + k) * (a + b + k) * x / ((a + 2.0 * k) * (a + 2.0 * k + 1.0))
        } else {
            let k = (m / 2) as f64;
            k * (b - k) * x / ((a + 2.0 * k - 1.0) * (a + 2.0 * k))
        }
    }
    let mut f = 1.0;
    for m in (1..=DEPTH).rev() {
        f = 1.0 / (1.0 + d_term(a, b, x, m) * f);
    }
    f
}

/// Regularised incomplete beta I_x(a, b), 0..1.
fn beta_regularized(a: f64, b: f64, x: f64) -> f64 {
    if !(0.0..=1.0).contains(&x) {
        return f64::NAN;
    }
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    // ln B(a,b) = lnΓ(a)+lnΓ(b)-lnΓ(a+b); the beta function is exp(-ln_beta).
    let ln_beta = ln_gamma(a + b) - ln_gamma(a) - ln_gamma(b);
    let cf = beta_cf(a, b, x);
    let log_front = a * x.ln() + b * (1.0 - x).ln() + ln_beta - a.ln();
    (log_front.exp() * cf).clamp(0.0, 1.0)
}

/// Student-t CDF with `df` degrees of freedom, via the incomplete beta:
/// `F(t) = 1 − ½·I_{ν/(ν+t²)}(ν/2, ½)` for `t ≥ 0`.
pub(crate) fn student_t_cdf(t: f64, df: f64) -> f64 {
    if t.is_nan() {
        return f64::NAN;
    }
    if t == f64::INFINITY {
        return 1.0;
    }
    if t == f64::NEG_INFINITY {
        return 0.0;
    }
    let x = df / (df + t * t);
    let tail = 0.5 * beta_regularized(df / 2.0, 0.5, x);
    if t >= 0.0 {
        1.0 - tail
    } else {
        tail
    }
}

/// Inverse Student-t CDF by bisection. The tails are monotone and bounded in
/// practice, so ~120 halvings reach machine precision.
pub(crate) fn student_t_quantile(p: f64, df: f64) -> f64 {
    let p = p.clamp(1e-9, 1.0 - 1e-9);
    let (mut lo, mut hi) = (-1e3, 1e3);
    for _ in 0..120 {
        let mid = 0.5 * (lo + hi);
        if student_t_cdf(mid, df) < p {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// The ~68% Student-t quantile (q ≈ 1.14 for ν=4), cached: it only depends on
/// the fixed `STUDENT_DF`, but it is used inside calibration loops.
pub(crate) fn t68_mult() -> f64 {
    static CACHE: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| student_t_quantile(0.841_344_7, STUDENT_DF))
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
    let q68 = t68_mult();
    for &decay in &CALIBRATION_DECAYS {
        for &scale in &CALIBRATION_SCALES {
            for &vol_mult in &CALIBRATION_VOL_MULTS {
                let mut sum_err = 0.0;
                let mut count = 0usize;
                let mut cone_miss = 0usize;
                // Per-horizon counts, so a well-covered 1-day cone cannot hide an
                // under-covered 90-day one behind a single global average.
                let mut h_count = vec![0usize; horizons.len()];
                let mut h_miss = vec![0usize; horizons.len()];

                for &(origin, stats) in &origins {
                    let price = closes[origin];
                    for (k, &days) in horizons.iter().enumerate() {
                        let j = origin + days as usize;
                        if j >= n {
                            continue;
                        }
                        let actual = closes[j];
                        let h = days as f64;
                        let sigma_h = cone_sigma_h(stats.sigma * vol_mult, h);
                        let mu_h = stats.mu * scale / (1.0 + h / decay).sqrt();
                        let total = (mu_h * h).clamp(-MAX_CUM_DRIFT, MAX_CUM_DRIFT);
                        let target = price * total.exp();

                        let err = (target / actual - 1.0).abs();
                        sum_err += err;
                        count += 1;
                        h_count[k] += 1;
                        if actual < target * (-q68 * sigma_h).exp()
                            || actual > target * (q68 * sigma_h).exp()
                        {
                            cone_miss += 1;
                            h_miss[k] += 1;
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
                // makes every horizon look equally uncertain. The per-horizon
                // term is what pushes the 90-day cone from ~50% to ~68% instead
                // of letting five short horizons satisfy the average.
                let coverage = 1.0 - miss_rate;
                let mut coverage_dev = 0.0;
                let mut used = 0usize;
                for k in 0..horizons.len() {
                    if h_count[k] == 0 {
                        continue;
                    }
                    let cov = 1.0 - h_miss[k] as f64 / h_count[k] as f64;
                    coverage_dev += (cov - 0.68).abs();
                    used += 1;
                }
                let coverage_dev = coverage_dev / used.max(1) as f64;
                let score =
                    mape + 0.5 * miss_rate + 1.0 * (coverage - 0.68).abs() + 2.0 * coverage_dev;

                if best.as_ref().map(|b| score < b.0).unwrap_or(true) {
                    best = Some((score, decay, scale, vol_mult));
                }
            }
        }
    }

    match best {
        Some((score, decay_days, drift_scale, vol_mult)) => {
            let (prob_drift_scale, prob_base) = fit_probability_model(
                &closes,
                &origins,
                horizons,
                decay_days,
                drift_scale,
                vol_mult,
            );
            Calibration {
                decay_days,
                drift_scale,
                vol_mult,
                prob_drift_scale,
                prob_base,
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
            }
        }
        None => fallback,
    }
}

/// Fit the direction-probability model: a base rate (intercept) plus a shrunk
/// drift term.
///
/// `prob_up = logistic(logit(base) + k · drift_z)`. `k` is chosen by minimising
/// the walk-forward Brier score over the same origins the rest of the calibration
/// uses, and is accepted **only if** the drift direction clears a Wilson 95%
/// interval on the effective (non-overlapping) sample **and** the held-out Brier
/// improvement is statistically significant and at least one full percentage
/// point. Otherwise `k = 0`, `prob_base = 0.5`, and the reported probability is
/// exactly 50%.
///
/// Why this exists: the raw drift is a momentum extrapolation. Measured
/// walk-forward, its direction hit rate is statistically indistinguishable from
/// a coin flip (all Wilson 95% intervals straddle 50%), yet converting it
/// directly into `prob_up` produced confidence buckets of 80–90% that resolved
/// 40–67% of the time — a negative Brier skill, i.e. worse than saying nothing.
/// Shrinking to the empirically justified amount turns that into "no edge"
/// instead of a confident lie.
fn fit_probability_model(
    closes: &[f64],
    origins: &[(usize, CoreStats)],
    horizons: &[i64],
    decay_days: f64,
    drift_scale: f64,
    vol_mult: f64,
) -> (f64, f64) {
    let n = closes.len();

    // (drift z-score at k=1, outcome) for every scored origin/horizon pair,
    // plus per-horizon direction tallies to test for a genuine edge.
    let mut points: Vec<(f64, f64)> = Vec::new();
    let mut dir: Vec<(usize, usize)> = vec![(0, 0); horizons.len()];
    for &(origin, stats) in origins {
        let price = closes[origin];
        for (hi, &days) in horizons.iter().enumerate() {
            let j = origin + days as usize;
            if j >= n {
                continue;
            }
            let h = days as f64;
            let sigma_h = cone_sigma_h(stats.sigma * vol_mult, h);
            let mu_h = stats.mu * drift_scale / (1.0 + h / decay_days).sqrt();
            let total = (mu_h * h).clamp(-MAX_CUM_DRIFT, MAX_CUM_DRIFT);
            let z = total / sigma_h.max(f64::EPSILON);
            let up = if closes[j] > price { 1.0 } else { 0.0 };
            if (z > 0.0) == (up > 0.5) {
                dir[hi].0 += 1;
            }
            dir[hi].1 += 1;
            points.push((z, up));
        }
    }
    // Too few samples to say anything about skill; claim nothing.
    if points.len() < 100 {
        return (0.0, 0.5);
    }

    // A positive `k` can only ever push `prob_up` further from 50% when the
    // drift is positive, so it is legitimate only when the drift direction
    // itself has a *significant* hit rate on at least one horizon. Judge that
    // on the effective number of non-overlapping windows, exactly like the
    // accuracy block shown to the user — the raw origin count would shrink the
    // interval by √10 and turn a coin flip into a fake edge. Only horizons with
    // enough independent windows (≥ 10) can establish an edge; the long end of
    // the ladder simply cannot, because a 400-day calibration window holds ~2
    // independent 180-day outcomes. On real market history no short horizon
    // clears the bar, so the global `k` stays 0 and every horizon reports 50%.
    let mut any_edge = false;
    for (hi, &days) in horizons.iter().enumerate() {
        let (hits, cnt) = dir[hi];
        if cnt < 30 {
            continue;
        }
        let effective = effective_origins(cnt, days);
        if effective < 10.0 {
            continue;
        }
        match edge_from_counts(hits, cnt, effective).verdict {
            EdgeVerdict::AboveChance => any_edge = true,
            // The drift reliably points the wrong way: a positive `k` would
            // make things worse, so refuse to claim anything.
            EdgeVerdict::BelowChance => return (0.0, 0.5),
            EdgeVerdict::NoEdge => {}
        }
    }
    if !any_edge {
        return (0.0, 0.5);
    }

    let raw_base = points.iter().map(|&(_, y)| y).sum::<f64>() / points.len() as f64;
    let raw_base = raw_base.clamp(0.02, 0.98);
    let intercept = (raw_base / (1.0 - raw_base)).ln();

    // Fit on the earlier part, validate on the tail. Fitting and judging on the
    // same points is how a noise-level "edge" gets promoted to a real one.
    let split = points.len() * 7 / 10;
    let (train, valid) = points.split_at(split);

    // (base shrink m, drift scale k, mean Brier)
    let mut best = (0.0_f64, 0.0_f64, f64::INFINITY);
    for m in [0.0, 0.25, 0.5, 0.75, 1.0] {
        let b0 = m * intercept;
        // Integer grid: `k += 0.05` accumulates float error and can overshoot
        // the [0, 1] bound, so derive k from an integer index instead.
        for ki in 0..=20 {
            let k = ki as f64 * 0.05;
            let mut sum = 0.0;
            for &(z, y) in train {
                let p = logistic(b0 + k * z);
                sum += (p - y) * (p - y);
            }
            let brier = sum / train.len() as f64;
            // Strict `<` with an ascending grid keeps the smallest k on ties.
            if brier < best.2 {
                best = (m, k, brier);
            }
        }
    }

    let (m, k, _) = best;
    let b0 = m * intercept;

    // Out-of-sample gate: the candidate must beat a coin flip on held-out data,
    // and by enough that the paired difference is not noise. Otherwise the
    // honest answer is "no edge" and `prob_up` stays at exactly 50%.
    let vn = valid.len() as f64;
    if vn < 20.0 {
        return (0.0, 0.5);
    }
    let mut sum = 0.0;
    let mut sum_sq = 0.0;
    for &(z, y) in valid {
        let p = logistic(b0 + k * z);
        let diff = (p - y) * (p - y) - 0.25;
        sum += diff;
        sum_sq += diff * diff;
    }
    let mean = sum / vn;
    let variance = (sum_sq / vn - mean * mean).max(0.0);
    let se = (variance / vn).sqrt();

    // Gate on the held-out tail. Two conditions, both required:
    //
    // 1. `mean < 0`: the candidate beats a coin flip on average.
    // 2. The paired improvement is statistically significant (`1.96·SE`).
    //
    // Plus an absolute floor. With thousands of held-out points a 0.001 Brier
    // edge is "significant" yet economically meaningless, and worse, it is the
    // signature of a short trend that already reversed — that is exactly how
    // the old model produced 60–80% "up" buckets that resolved 13% of the
    // time. Demanding at least a full percentage point of Brier improvement
    // ($0.01$) rejects noise-fitting while a genuine trend clears it by a
    // huge margin. On real market history no candidate passes, so the reported
    // probability stays at exactly 50% instead of overclaiming.
    let improvement = -mean;
    let significant = se > f64::EPSILON && mean < -1.96 * se;
    if significant && improvement >= 0.01 {
        (k, logistic(b0))
    } else {
        (0.0, 0.5)
    }
}

/// Numerically stable logistic function.
fn logistic(x: f64) -> f64 {
    if x >= 0.0 {
        1.0 / (1.0 + (-x).exp())
    } else {
        let e = x.exp();
        e / (1.0 + e)
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
    let q68 = t68_mult();
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
            let sigma_h = cone_sigma_h(stats.sigma * vol_mult, h);
            let mu_h = stats.mu * drift_scale / (1.0 + h / decay_days).sqrt();
            let total = (mu_h * h).clamp(-MAX_CUM_DRIFT, MAX_CUM_DRIFT);
            let target = price * total.exp();

            acc[k].0 += (target / actual - 1.0).abs();
            acc[k].1 += 1;
            if (target - price).signum() == (actual - price).signum() {
                acc[k].2 += 1;
            }
            if actual >= target * (-q68 * sigma_h).exp() && actual <= target * (q68 * sigma_h).exp()
            {
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
                hits,
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
    // and cap its influence at ±0.02%/day (≈0.6% over a month). The *median*
    // drives the adjustment — with N≈8 a single parabolic year owns the mean
    // (`Nov avg +7.2%`) while the median says what a typical November did — and
    // the cap keeps a 7-sample statistic from meaningfully moving the drift.
    let seasonality = monthly_seasonality_stats(history);
    let month = current_month();
    if let Some(s) = seasonality
        .iter()
        .find(|s| s.month == month && s.count >= 2)
    {
        let adj = (s.median_pct / 100.0 / 30.0).clamp(-0.0002, 0.0002);
        mu = (mu + adj).clamp(-MAX_DAILY_DRIFT, MAX_DAILY_DRIFT);
        notes.push(format!(
            "seasonality: {} median {:+.1}% · {:.0}% of {} months up · mean {:+.1}% (N={}, ignore)",
            month_name(month),
            s.median_pct,
            s.win_rate * 100.0,
            s.count,
            s.avg_pct,
            s.count
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
        calibration.prob_drift_scale,
        calibration.prob_base,
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
    /// Origins where the direction was right (for the binomial interval).
    pub hits: usize,
    /// Share of actual closes inside the 68% cone (ideal ≈ 68%).
    pub coverage_pct: f64,
}

impl BacktestHorizon {
    /// 95% Wilson interval on the direction hit rate, using an effective count
    /// that accounts for overlapping walk-forward windows.
    pub fn edge(&self) -> DirectionEdge {
        edge_from_counts(
            self.hits,
            self.samples,
            effective_origins(self.samples, self.days),
        )
    }
}

#[derive(Debug, Clone)]
pub struct RegimeAccuracy {
    pub regime: String,
    pub samples: usize,
    /// Direction hit rate across every horizon in this regime.
    pub hit_pct: f64,
    /// Share of outcomes inside the 68% cone in this regime.
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
    /// Same metrics split by the regime at each origin, so it is visible where
    /// the model actually holds and where it does not.
    pub by_regime: Vec<RegimeAccuracy>,
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
    // regime -> (samples, hits, inside)
    let mut by_regime: std::collections::BTreeMap<&'static str, (usize, usize, usize)> =
        std::collections::BTreeMap::new();

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
            calibration.prob_drift_scale,
            calibration.prob_base,
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
            let entry = by_regime.entry(stats.regime.as_str()).or_default();
            entry.0 += 1;
            if dir_ok {
                entry.1 += 1;
            }
            if in_cone {
                entry.2 += 1;
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
                    hits,
                    coverage_pct: inside as f64 / s * 100.0,
                }
            })
            .collect(),
        by_regime: by_regime
            .into_iter()
            .map(|(regime, (samples, hits, inside))| {
                let s = samples.max(1) as f64;
                RegimeAccuracy {
                    regime: regime.to_string(),
                    samples,
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
        "uncalibrated: decay {:.0}d · drift×1.00 · vol×1.00\n",
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
        out.push_str(
            "   H    MAPE old→new    hit old→new   95% CI       cone old→new   bias new\n",
        );
        for (o, nw) in old.horizons.iter().zip(new.horizons.iter()) {
            // A 30-day window cannot score a 30-day horizon; skip empty rows.
            if nw.samples == 0 {
                continue;
            }
            let e = nw.edge();
            out.push_str(&format!(
                "  {:>3}d   {:>5.1}% → {:>5.1}%   {:>4.0}% → {:>4.0}%   {:>9}  {:>4.0}% → {:>4.0}%   {:+.1}%\n",
                nw.days,
                o.mape_pct,
                nw.mape_pct,
                o.hit_pct,
                nw.hit_pct,
                e.ci(),
                o.coverage_pct,
                nw.coverage_pct,
                nw.bias_pct
            ));
        }
        out.push('\n');
    }

    // Where the model actually holds: cone coverage and direction by regime.
    // A regime with 60% coverage is where the cone lies; a 45% direction hit
    // rate is a coin flip and the text should never be read as a signal.
    out.push_str("by regime (365d window, all horizons):\n");
    for r in &long_new.by_regime {
        out.push_str(&format!(
            "  {:<8} n={:<5} hit {:>3.0}% · cone {:>3.0}%\n",
            r.regime, r.samples, r.hit_pct, r.coverage_pct
        ));
    }
    out.push('\n');

    out.push_str(
        "MAPE = mean absolute error of the target · hit = direction accuracy ·\n\
         cone = share of outcomes inside the fitted band (the band's nominal width\n\
         is set by a Student-t quantile with ν=4, and this column is its measured\n\
         coverage on your history, not a claim) · bias > 0 = over-forecast.\n\
         CI = 95% Wilson interval on the hit rate, computed on non-overlapping\n\
         windows (origins×step/h), because adjacent walk-forward windows share\n\
         days. A CI that covers 50% means the horizon has no directional edge.\n",
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

/// Below this the one-decimal percentage would round to `±0.0%`, which reads
/// like a real (tiny) forecast instead of "no trend".
const FLAT_CHANGE_PCT: f64 = 0.5;

/// Expected move for display. A model that sees no trend must say so instead of
/// printing fake precision: `-0.0%` is not a prediction, it is a rounding bug.
fn fmt_change(change_pct: f64) -> String {
    if change_pct.abs() < FLAT_CHANGE_PCT {
        "≈0%".to_string()
    } else {
        format!("{change_pct:+.1}%")
    }
}

/// Under this the target is statistically indistinguishable from spot; the
/// bottom line then points at the cone instead of the arrow.
const FLAT_TARGET_PCT: f64 = 0.5;

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

/// One accuracy bucket (day / week / month / ...), aggregated over the
/// horizons that fall into it. Kept as a struct instead of a formatted string
/// because the direction rate now carries a confidence interval.
#[derive(Debug, Clone)]
struct AccuracyBucket {
    label: &'static str,
    hits: usize,
    samples: usize,
    effective_n: f64,
    mape_pct: f64,
    coverage_pct: f64,
}

impl AccuracyBucket {
    fn direction_pct(&self) -> f64 {
        if self.samples == 0 {
            return f64::NAN;
        }
        self.hits as f64 / self.samples as f64 * 100.0
    }

    fn edge(&self) -> DirectionEdge {
        edge_from_counts(self.hits, self.samples, self.effective_n)
    }

    /// `month    dir 41% · CI 27–57% · no edge · err ±9.3% · in-range 67%`
    fn line(&self) -> String {
        let e = self.edge();
        format!(
            "{:<9} dir {:>3.0}% · CI {} · {} · err ±{:.1}% · in-range {:.0}%",
            self.label,
            self.direction_pct(),
            e.ci(),
            e.verdict.label(),
            self.mape_pct,
            self.coverage_pct,
        )
    }
}

impl Forecast {
    /// Whether the walk-forward fit found a directional edge worth showing.
    /// When it did not, `prob_up` is pinned to the 50% base rate and a bare
    /// "50%" next to a non-flat target reads as a contradiction, so the
    /// renderers mark it as "no measured edge" instead of faking precision.
    pub fn direction_has_edge(&self) -> bool {
        self.calibration.prob_drift_scale > 0.0
    }

    /// The direction cell, or `None` when the fit found no edge. A column that
    /// can only ever read "≈50%" is noise, so it is dropped entirely instead of
    /// being printed. It reappears the moment a real edge is measured.
    fn p_up_text(&self, h: &HorizonForecast) -> Option<String> {
        self.direction_has_edge()
            .then(|| format!("{:.0}%", h.prob_up * 100.0))
    }

    /// Realised share of outcomes inside this horizon's cone, from the same
    /// walk-forward run that produced the cone. `None` when the sample is too
    /// thin to mean anything — then the column is dropped, not faked.
    fn coverage_text(&self, days: i64) -> Option<String> {
        const MIN_SAMPLES: usize = 30;
        self.calibration
            .accuracy
            .iter()
            .find(|a| a.days == days && a.samples >= MIN_SAMPLES)
            .map(|a| format!("{:.0}%", a.coverage_pct))
    }

    /// Aligned table rows (header first), monospace-ready.
    ///
    /// Columns are conditional on measured evidence:
    /// * `P(up)` appears only when the fit found a directional edge;
    /// * `IN-RANGE` appears only when the cone was actually scored on enough
    ///   outcomes. Its value is the *measured* coverage, so "68% band" is never
    ///   a claim the user has to take on faith.
    fn table_lines(&self, decimals: usize) -> Vec<String> {
        let show_p_up = self.direction_has_edge();
        let show_cover = self
            .horizons
            .iter()
            .any(|h| self.coverage_text(h.days).is_some());

        let mut header: Vec<String> = vec!["H".into(), "TARGET".into(), "Δ".into(), "BAND".into()];
        // H and the band are left-aligned; numbers right-aligned.
        let mut right = vec![false, true, true, false];
        if show_cover {
            header.push("IN-RANGE".into());
            right.push(true);
        }
        if show_p_up {
            header.push("P(up)".into());
            right.push(true);
        }

        let rows: Vec<Vec<String>> = self
            .horizons
            .iter()
            .map(|h| {
                let mut row = vec![
                    format!("{}d", h.days),
                    fmt_money(h.target, decimals),
                    fmt_change(h.change_pct),
                    format!(
                        "{}–{}",
                        fmt_money(h.low, decimals),
                        fmt_money(h.high, decimals)
                    ),
                ];
                if show_cover {
                    row.push(self.coverage_text(h.days).unwrap_or_else(|| "—".into()));
                }
                if show_p_up {
                    row.push(self.p_up_text(h).unwrap_or_else(|| "—".into()));
                }
                row
            })
            .collect();

        let cols = header.len();
        let mut widths = vec![0usize; cols];
        for (i, h) in header.iter().enumerate() {
            widths[i] = h.chars().count();
        }
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }

        let line = |cells: &[String]| -> String {
            let mut s = String::new();
            for (i, cell) in cells.iter().enumerate() {
                if i > 0 {
                    s.push_str("  ");
                }
                let pad = widths[i].saturating_sub(cell.chars().count());
                if right[i] {
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
        lines.push(line(&header));
        for row in &rows {
            lines.push(line(row));
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

    /// Accuracy rows bucketed into day / week / month / quarter / half-year,
    /// so the user can see how often the model is right at each scale.
    /// Recomputed from the latest history on every forecast.
    fn accuracy_buckets(&self) -> Vec<AccuracyBucket> {
        // (label, inclusive min, inclusive max)
        const BUCKETS: [(&str, i64, i64); 5] = [
            ("day", 1, 4),
            ("week", 5, 8),
            ("month", 9, 45),
            ("quarter", 46, 120),
            ("half-year", 121, i64::MAX),
        ];
        let mut buckets = Vec::new();
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
            // Weight error/coverage by sample count so a long horizon with fewer
            // origins does not dominate the bucket. Direction is aggregated from
            // raw hits, which is what the binomial interval needs.
            let total: usize = rows.iter().map(|a| a.samples).sum();
            let w = total.max(1) as f64;
            let hits: usize = rows.iter().map(|a| a.hits).sum();
            // Sum of per-horizon effective counts: the horizons inside a bucket
            // overlap each other too, so this is mildly optimistic — but far
            // more honest than `total`.
            let effective_n = rows
                .iter()
                .map(|a| effective_origins(a.samples, a.days))
                .sum::<f64>();
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
            buckets.push(AccuracyBucket {
                label,
                hits,
                samples: total,
                effective_n,
                mape_pct: mape,
                coverage_pct: cover,
            });
        }
        buckets
    }

    fn accuracy_bucket_lines(&self) -> Vec<String> {
        self.accuracy_buckets().iter().map(|b| b.line()).collect()
    }

    /// One honest sentence about whether *any* horizon beats a coin flip.
    /// Several horizons are tested at once, so a single cell just clear of 50%
    /// is called suggestive, not proven.
    fn edge_summary(&self) -> Option<String> {
        let buckets = self.accuracy_buckets();
        if buckets.is_empty() {
            return None;
        }
        let tested = buckets.len();
        match buckets.iter().find(|b| b.edge().verdict != EdgeVerdict::NoEdge) {
            None => Some(
                "direction: coin flip at every horizon (no 95% interval excludes 50%) — use the cone, not the arrow"
                    .to_string(),
            ),
            Some(b) => Some(format!(
                "direction: only {} is {} ({}) of {} horizons tested — suggestive, not proven",
                b.label,
                b.edge().verdict.label(),
                b.edge().ci(),
                tested,
            )),
        }
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
        out.push('\n');
        out.push_str("────────────────────────────\n");
        out.push_str("🎯 BOTTOM LINE\n");
        for line in self.action_lines(None, false) {
            out.push_str(&format!("• {line}\n"));
        }
        out.push_str("────────────────────────────\n");

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
                if let Some(s) = self.edge_summary() {
                    out.push_str(&format!("  {s}\n"));
                }
            }
        }
        if !self.notes.is_empty() {
            for n in &self.notes {
                out.push_str(&format!("• {n}\n"));
            }
        }
        out
    }

    /// Telegram HTML rendering with no portfolio context. Kept for callers that
    /// only have a [`Forecast`] (CLI, tests); the bot uses [`Self::render_html_for`].
    #[allow(dead_code)]
    pub fn render_html(&self) -> String {
        self.render_html_for(None, false)
    }

    /// Same as [`Self::render_html`], but portfolio-aware: when `avg_entry` is
    /// given the bottom line says how far spot is from the user's own break-even
    /// and what to do about it. `russian` localizes the verdict lines.
    pub fn render_html_for(&self, avg_entry: Option<f64>, russian: bool) -> String {
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
            out.push_str(&format!(
                "\n🎯 <b>{}</b>\n",
                if russian { "Итог" } else { "Bottom line" }
            ));
            for line in self.action_lines(avg_entry, russian) {
                out.push_str(&format!("• {}\n", esc(&line)));
            }
            out.push_str(&format!(
                "<i>{}: {:.0}%</i>\n",
                if russian {
                    "уверенность горизонта"
                } else {
                    "horizon confidence"
                },
                h.confidence * 100.0
            ));
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
                if let Some(s) = self.edge_summary() {
                    out.push_str(&format!("<i>{}</i>\n", esc(&s)));
                }
            }
        }
        for n in &self.notes {
            out.push_str(&format!("• {}\n", esc(n)));
        }
        out
    }

    /// The lowest cone bound across the 14–90 day horizons — the model's own
    /// "where a normal dip would end" band. Used for the buy-the-dip read.
    fn dip_zone(&self) -> Option<(f64, i64)> {
        self.horizons
            .iter()
            .filter(|h| (14..=90).contains(&h.days))
            .min_by(|a, b| {
                a.low
                    .partial_cmp(&b.low)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|h| (h.low, h.days))
    }

    /// Actionable bottom line: distance to the user's average entry, how likely
    /// the dip band is to be touched, and a plain buy / scale-in / wait call.
    ///
    /// Hard rule (same as the dip-zone command): never suggest adding above the
    /// average entry. Forecasts are honest coin-flips on direction, so the text
    /// always carries the walk-forward error instead of a confident claim.
    pub fn action_lines(&self, avg_entry: Option<f64>, russian: bool) -> Vec<String> {
        let mut out = Vec::new();
        let decimals = price_decimals(self.price);
        // Every probability below is read off `self.moments(days)` — the exact
        // moments the cone is drawn from (decayed drift, drift-error term,
        // Student-t). `/dip` uses the same accessors, so the two commands cannot
        // disagree: the raw `drift_daily_pct %/day * days` path used here before
        // ignored the decay and quoted up to 2x the touch chance the dip ladder
        // showed for the same level on the same run.
        use crate::trading::dipzones::{prob_below_forecast, touch_probability_forecast};

        if let Some(h) = self.reference_horizon() {
            // The direction cell is omitted unless a real edge was measured —
            // printing "P(up) 50%" next to a non-flat target is a
            // contradiction, and a number that cannot move is not information.
            let p_up = self
                .p_up_text(h)
                .map(|p| {
                    if russian {
                        format!(" · P(выше) {p}")
                    } else {
                        format!(" · P(up) {p}")
                    }
                })
                .unwrap_or_default();
            out.push(if russian {
                format!(
                    "{}д: {} → {} · коридор: {}–{}{}",
                    h.days,
                    fmt_change(h.change_pct),
                    fmt_money(h.target, decimals),
                    fmt_money(h.low, decimals),
                    fmt_money(h.high, decimals),
                    p_up
                )
            } else {
                format!(
                    "{}d: {} → {} · band: {}–{}{}",
                    h.days,
                    fmt_change(h.change_pct),
                    fmt_money(h.target, decimals),
                    fmt_money(h.low, decimals),
                    fmt_money(h.high, decimals),
                    p_up
                )
            });
            if h.change_pct.abs() < FLAT_TARGET_PCT {
                out.push(if russian {
                    "Тренда нет: цель — примерно текущая цена. Смотри на коридор, а не на стрелку."
                        .to_string()
                } else {
                    "No trend: the target is essentially today's price. Read the band, not the arrow."
                        .to_string()
                });
            }
        }

        let avg = avg_entry.filter(|a| a.is_finite() && *a > 0.0);
        let dip = self.dip_zone();

        // Probability of returning to the user's own break-even level.
        let p_avg_90 = avg.map(|a| prob_below_forecast(self, a, 90));
        let p_dip_90 = dip.map(|(low, _)| touch_probability_forecast(self, low, 90));

        if let Some(avg) = avg {
            let dist = (self.price / avg - 1.0) * 100.0;
            out.push(if russian {
                format!(
                    "Средний вход: {} · цена {:+.1}% к нему",
                    fmt_money(avg, decimals),
                    dist
                )
            } else {
                format!(
                    "Average entry: {} · price is {:+.1}% vs it",
                    fmt_money(avg, decimals),
                    dist
                )
            });
            if let (Some((low, days)), Some(p)) = (dip, p_dip_90) {
                out.push(if russian {
                    format!(
                        "Зона откупа (нижняя граница конуса {}д): {} — шанс задеть за 90д ≈ {:.0}%",
                        days,
                        fmt_money(low, decimals),
                        p * 100.0
                    )
                } else {
                    format!(
                        "Dip band (cone lower bound at {}d): {} — ~{:.0}% chance to touch within 90d",
                        days,
                        fmt_money(low, decimals),
                        p * 100.0
                    )
                });
            }
            if let Some(p) = p_avg_90 {
                // 100% would read like a certainty; the model never is.
                let pct = (p * 100.0).min(99.0);
                out.push(if russian {
                    format!("Шанс снова оказаться ниже средней за 90д ≈ {:.0}%", pct)
                } else {
                    format!("~{:.0}% chance to trade below your average within 90d", pct)
                });
            }
        }

        // One action sentence, derived only from the numbers above. Without an
        // average entry there is nothing to average down against, so say that
        // instead of pretending we compared it to something.
        let below_avg = avg.map(|a| self.price <= a).unwrap_or(false);
        let dip_below_avg = match (dip, avg) {
            (Some((low, _)), Some(a)) => low < a,
            _ => false,
        };
        let action = if avg.is_none() {
            if russian {
                "Действие: средней закупки нет — добавь покупки через /add, и появится сигнал «докупать/ждать»."
                    .to_string()
            } else {
                "Action: no average entry on file — add purchases with /add to get a buy/wait call."
                    .to_string()
            }
        } else if below_avg {
            if russian {
                "Действие: цена ниже твоей средней — докупать частями по сетке, не одной суммой."
                    .to_string()
            } else {
                "Action: price is below your average — scale in in tranches, not all at once."
                    .to_string()
            }
        } else if dip_below_avg && p_dip_90.unwrap_or(0.0) >= 0.30 {
            let p_avg = p_avg_90.map(|p| (p * 100.0).min(99.0)).unwrap_or(0.0);
            if russian {
                format!(
                    "Действие: выше средней — не усредняйся, но и не жди возврата к средней как гарантии (шанс ≈ {p_avg:.0}%). Зона откупа реальна: набирай там частями, а не всё по текущей."
                )
            } else {
                format!(
                    "Action: above your average — don't average up, but don't treat the average as a magnet either (~{p_avg:.0}% chance to return). The dip band is realistic: add in tranches there, not all at today's price."
                )
            }
        } else {
            let p_avg = p_avg_90.map(|p| (p * 100.0).min(99.0)).unwrap_or(0.0);
            if russian {
                format!(
                    "Действие: цена выше средней, а зона откупа маловероятна. Средняя может не вернуться (шанс ≈ {p_avg:.0}%): ожидание — это ставка, а не план. Если хочешь больше позиции — бери небольшую долю сейчас (она поднимет среднюю), остальное держи лимиткой ниже."
                )
            } else {
                format!(
                    "Action: above average with a distant dip band. Your average may never come back (~{p_avg:.0}% chance) — waiting is a bet, not a plan. If you want more exposure, take a small tranche now (it raises the average) and keep the rest as a lower bid."
                )
            }
        };
        out.push(action);

        if let Some(acc) = self
            .calibration
            .accuracy_for(self.reference_horizon().map(|h| h.days).unwrap_or(30))
        {
            let e = acc.edge();
            out.push(if russian {
                format!(
                    "Направление: попадание {:.0}% · 95% CI {} · {} · ошибка ±{:.1}% (walk-forward, n≈{:.0})",
                    e.pct,
                    e.ci(),
                    e.verdict.label(),
                    acc.mape_pct,
                    e.effective_n
                )
            } else {
                format!(
                    "Direction: hit {:.0}% · 95% CI {} · {} · error ±{:.1}% (walk-forward, n≈{:.0})",
                    e.pct,
                    e.ci(),
                    e.verdict.label(),
                    acc.mape_pct,
                    e.effective_n
                )
            });
        }
        out
    }

    /// Rich-message markdown rendering: a real GFM table (Telegram renders it
    /// as a styled table, not monospace text), the portfolio-aware bottom line
    /// and the accuracy block. Sent via `sendRichMessage`, so it can still
    /// carry an inline keyboard.
    pub fn render_markdown(&self, avg_entry: Option<f64>, russian: bool) -> String {
        let decimals = price_decimals(self.price);
        // `$` opens LaTeX math in rich markdown, so print bare numbers and put
        // the unit in the header instead.
        let money =
            |v: f64| -> String { fmt_money(v, decimals).trim_start_matches('$').to_string() };
        let mut out = format!(
            "📈 **{} forecast** · {} USD\n\n",
            self.symbol,
            money(self.price)
        );
        out.push_str(&format!(
            "regime {} · RSI14 {} · vol {:.2}%/day · drift {:+.3}%/day · {} closes\n\n",
            self.regime,
            self.rsi14
                .map(|r| format!("{r:.0}"))
                .unwrap_or_else(|| "n/a".into()),
            self.vol_daily_pct,
            self.drift_daily_pct,
            self.history_days,
        ));

        // A monospace fence, not a GFM table. Telegram's rich-message renderer
        // turns a markdown table into its own block (clients show a stray
        // "Table" caption) and re-flows the cells on narrow screens, which
        // broke the column alignment the numbers need. A fenced block renders
        // identically everywhere and needs no per-cell escaping. The direction
        // column is still omitted when no edge was measured.
        out.push_str("```\n");
        out.push_str(&self.table_lines(decimals).join("\n"));
        out.push_str("\n```\n");

        out.push_str(&format!(
            "\n🎯 **{}**\n",
            if russian { "Итог" } else { "Bottom line" }
        ));
        for line in self.action_lines(avg_entry, russian) {
            out.push_str(&format!("- {line}\n"));
        }
        if let Some(h) = self.reference_horizon() {
            out.push_str(&format!(
                "*{}: {:.0}%*\n",
                if russian {
                    "уверенность горизонта"
                } else {
                    "horizon confidence"
                },
                h.confidence * 100.0
            ));
        }

        if !self.calibration.is_default() {
            let lines = self.accuracy_bucket_lines();
            if !lines.is_empty() {
                out.push_str(&format!(
                    "\n**{}** (walk-forward, {} origins)\n```\n{}\n```\n",
                    if russian {
                        "Точность по горизонтам"
                    } else {
                        "Accuracy"
                    },
                    self.calibration.samples,
                    lines.join("\n")
                ));
                if let Some(s) = self.edge_summary() {
                    out.push_str(&format!("*{s}*\n"));
                }
            }
        }
        for n in &self.notes {
            out.push_str(&format!("- {n}\n"));
        }
        // Rich markdown treats `$...$` as LaTeX math; drop every currency sign
        // (units are already spelled out in the headers).
        out.replace('$', "")
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
                "{icon} <b>{word}</b> at {}d: <code>{}</code> → <code>{}</code>\n",
                h.days,
                fmt_change(h.change_pct),
                fmt_money(h.target, decimals),
            ));
            let p_up = self
                .p_up_text(h)
                .map(|p| format!(" · P(up) <code>{p}</code>"))
                .unwrap_or_default();
            out.push_str(&format!(
                "band <code>{}–{}</code>{}\n",
                fmt_money(h.low, decimals),
                fmt_money(h.high, decimals),
                p_up,
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

/// Append the live spot price as the current day's close when the series tail is
/// older than `max_age_ms`.
///
/// This is the last line of defence against a stale series: if every provider is
/// down and the on-disk cache ends days ago, RSI/drift/zones would otherwise be
/// computed on old closes while the report shows the live price. Returns the
/// series unchanged when the tail is fresh enough (or the price is unusable).
pub fn anchor_to_spot(
    series: &[(i64, f64)],
    price: f64,
    now_ms: i64,
    max_age_ms: i64,
) -> Vec<(i64, f64)> {
    if !(price > 0.0) {
        return series.to_vec();
    }
    let stale = series
        .last()
        .map(|&(ts, _)| now_ms.saturating_sub(ts) > max_age_ms)
        .unwrap_or(true);
    if !stale {
        return series.to_vec();
    }
    merge_daily_history(series, &[(now_ms, price)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_to_spot_appends_when_tail_is_stale() {
        let s = series(&[100.0, 101.0, 102.0]);
        let last = s.last().unwrap().0;
        let now = last + 10 * 86_400_000;
        let out = anchor_to_spot(&s, 90.0, now, 86_400_000);
        assert_eq!(out.len(), s.len() + 1);
        assert_eq!(out.last().unwrap().1, 90.0);
        assert_eq!(
            out.last().unwrap().0,
            now.div_euclid(86_400_000) * 86_400_000
        );
    }

    #[test]
    fn anchor_to_spot_keeps_fresh_series() {
        let s = series(&[100.0, 101.0, 102.0]);
        let last = s.last().unwrap().0;
        let out = anchor_to_spot(&s, 90.0, last + 3_600_000, 86_400_000);
        assert_eq!(out, s);
    }

    #[test]
    fn anchor_to_spot_ignores_bad_price() {
        let s = series(&[100.0, 101.0, 102.0]);
        let last = s.last().unwrap().0;
        assert_eq!(anchor_to_spot(&s, 0.0, last + 10 * 86_400_000, 1), s);
        assert_eq!(anchor_to_spot(&[], 90.0, 10 * 86_400_000, 1).len(), 1);
    }

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

    /// Strong, persistent downtrend: the regime where a raw `drift% * days`
    /// path and the decayed cone moments diverge the most.
    fn falling(n: usize) -> Vec<f64> {
        (0..n).map(|i| 300.0 * 0.997f64.powi(i as i32)).collect()
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
    fn sub_half_percent_move_is_reported_as_flat() {
        // The drift/vol math can legitimately land at ±0.1–0.4%; printing
        // "-0.3%" there is fake precision (the target is inside the noise).
        assert_eq!(fmt_change(0.0), "≈0%");
        assert_eq!(fmt_change(0.1), "≈0%");
        assert_eq!(fmt_change(-0.3), "≈0%");
        assert_eq!(fmt_change(0.49), "≈0%");
        assert_eq!(fmt_change(0.6), "+0.6%");
        assert_eq!(fmt_change(-5.0), "-5.0%");
    }

    #[test]
    fn flat_market_reports_no_trend_instead_of_negative_zero() {
        // A flat series produces a target equal to spot. Printing "-0.0%" for
        // that reads like a real forecast; the renderer must say "no trend" and
        // point at the cone instead.
        let h = series(&[100.0; 600]);
        let f = forecast("ETH", 100.0, &h);

        let table = f.table_lines(price_decimals(f.price)).join("\n");
        assert!(table.contains("≈0%"), "flat table must say ≈0%: {table}");
        assert!(!table.contains("-0.0%"), "fake -0.0% in table: {table}");
        assert!(!table.contains("+0.0%"), "fake +0.0% in table: {table}");

        let actions = f.action_lines(None, false).join("\n");
        assert!(
            actions.contains("No trend"),
            "flat target must be called out: {actions}"
        );
        assert!(
            !actions.contains("-0.0%"),
            "fake -0.0% in actions: {actions}"
        );

        for text in [
            f.render(),
            f.render_html_for(None, false),
            f.render_markdown(None, false),
        ] {
            assert!(text.contains("≈0%"), "flat target must say ≈0%: {text}");
            assert!(!text.contains("-0.0%"), "fake -0.0% leaked: {text}");
        }
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
    fn probability_is_bounded_and_follows_the_fitted_model() {
        let up = series(&rising(600));
        let f = forecast("BTC", up.last().unwrap().1, &up);
        let cal = &f.calibration;
        assert!((0.0..=1.0).contains(&cal.prob_base));
        assert!((0.0..=1.0).contains(&cal.prob_drift_scale));
        for h in &f.horizons {
            assert!((0.0..=1.0).contains(&h.prob_up), "prob_up out of range");
            if cal.prob_drift_scale == 0.0 {
                // No fitted directional edge: every horizon reports the base rate.
                assert!(
                    (h.prob_up - cal.prob_base).abs() < 1e-12,
                    "scale 0 must collapse prob_up to the base rate"
                );
            } else if h.change_pct > 0.0 {
                assert!(h.prob_up > cal.prob_base, "positive drift must add up");
            } else if h.change_pct < 0.0 {
                assert!(h.prob_up < cal.prob_base, "negative drift must add down");
            }
        }
    }

    #[test]
    fn probability_model_never_scores_worse_than_the_base_rate() {
        // The fit accepts a drift term only if its Brier score strictly beats the
        // base-rate model. This is the guard against the reported confidence
        // drifting back to "80% up" on a series where that is worthless.
        let path = cryptograph_history("BTC");
        let closes: Vec<f64> = path.iter().map(|&(_, c)| c).collect();
        let n = closes.len();
        let horizons = horizons_for("BTC", n);
        let mut origins = Vec::new();
        let mut i = n.saturating_sub(400).max(50);
        while i + 1 < n {
            origins.push((i, core_stats(&closes[..=i])));
            i += 3;
        }
        let (scale, base) = fit_probability_model(&closes, &origins, horizons, 30.0, 0.75, 1.0);
        assert!((0.02..=0.98).contains(&base));

        let intercept = (base / (1.0 - base)).ln();
        let mut model = 0.0;
        let mut baseline = 0.0;
        let mut count = 0usize;
        for &(origin, stats) in &origins {
            let price = closes[origin];
            for &days in horizons {
                let j = origin + days as usize;
                if j >= n {
                    continue;
                }
                let h = days as f64;
                let sigma_h = cone_sigma_h(stats.sigma, h);
                let mu_h = stats.mu * 0.75 / (1.0 + h / 30.0).sqrt();
                let z = (mu_h * h).clamp(-MAX_CUM_DRIFT, MAX_CUM_DRIFT) / sigma_h;
                let y = if closes[j] > price { 1.0 } else { 0.0 };
                let p = logistic(intercept + scale * z);
                model += (p - y) * (p - y);
                baseline += (base - y) * (base - y);
                count += 1;
            }
        }
        assert!(count > 50);
        assert!(
            model <= baseline + 1e-9,
            "fitted probability model is worse than the base rate ({model} > {baseline})"
        );
    }

    #[test]
    fn probability_model_finds_a_real_edge_in_a_clean_trend() {
        // A strictly rising series is perfectly predictable, so the fit must keep
        // a non-zero drift term and push P(up) above the base rate.
        let up = series(&rising(700));
        let cal = calibrate("BTC", &up);
        assert!(
            cal.prob_drift_scale > 0.0,
            "clean trend must retain a positive drift scale"
        );
        let f = forecast("BTC", up.last().unwrap().1, &up);
        assert!(f.horizons.iter().all(|h| h.prob_up >= cal.prob_base));
    }

    /// Load a real history CSV, or synthesise one when the fixture is absent.
    fn cryptograph_history(symbol: &str) -> Vec<(i64, f64)> {
        match crate::trading::history_file::load_history(symbol) {
            Some(h) if h.len() > 500 => h,
            _ => series(&rising(700)),
        }
    }

    #[test]
    fn html_render_wraps_only_the_table_in_pre() {
        let h = series(&rising(700));
        let f = forecast("BTC", h.last().unwrap().1, &h);
        let html = f.render_html();
        // Header + bottom line are HTML, only the table is monospace.
        assert!(html.starts_with("📈 <b>BTC forecast</b>"));
        assert!(html.contains("Bottom line"));
        assert!(html.contains('🎯'));
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
    fn action_lines_respect_the_average_entry_rule() {
        let h = series(&rising(700));
        let f = forecast("BTC", h.last().unwrap().1, &h);

        // Price above the user's average entry → never advise averaging up;
        // it may offer a small tranche, but must say the average may not return.
        let above = f
            .action_lines(Some(f.price * 0.5), false)
            .join("\n")
            .to_lowercase();
        assert!(above.contains("average entry"));
        assert!(
            above.contains("don't average up")
                || above.contains("average may never come back")
                || above.contains("may never come back"),
            "above-entry advice must stay honest about the average: {above}"
        );

        // Price below the average → scale in, in tranches.
        let below = f
            .action_lines(Some(f.price * 2.0), true)
            .join("\n")
            .to_lowercase();
        assert!(below.contains("средний вход"));
        assert!(below.contains("докупать частями"), "{below}");

        // No average known → still yields the horizon read and one action.
        let none = f.action_lines(None, false);
        assert!(none.len() >= 2);
    }

    #[test]
    fn dip_band_probability_matches_the_dip_command() {
        // `/forecast` and `/dip` must quote the same numbers for the same
        // event. They used to take different paths: `/dip` read the cone
        // moments (decayed drift, drift error, Student-t) while `/forecast`
        // multiplied the raw daily drift by the horizon, which on a strong
        // negative trend doubled every probability. A fading series is the
        // case that exposes it.
        let h = series(&falling(700));
        let f = forecast("ETH", h.last().unwrap().1, &h);
        let (low, days) = f.dip_zone().expect("a dip zone");
        let expected = crate::trading::dipzones::touch_probability_forecast(&f, low, 90);
        let avg = f.price * 1.2;
        let expected_below = crate::trading::dipzones::prob_below_forecast(&f, avg, 90);

        // Guard: the series must be one on which the old raw-drift path gave a
        // materially different answer, otherwise this test proves nothing.
        let raw = crate::trading::dipzones::touch_probability(
            f.price,
            low,
            f.drift_daily_pct / 100.0,
            f.vol_daily_pct / 100.0,
            90,
        );
        assert!(
            (raw - expected).abs() > 0.02,
            "series must expose the decay gap: raw {raw:.3} vs cone {expected:.3}"
        );

        let lines = f.action_lines(Some(avg), false);
        let line = lines
            .iter()
            .find(|l| l.contains(&format!("cone lower bound at {days}d")))
            .expect("dip-band line");
        assert!(
            line.contains(&format!("~{:.0}%", expected * 100.0)),
            "line = {line}, expected {:.0}%",
            expected * 100.0
        );
        let below = lines
            .iter()
            .find(|l| l.contains("trade below your average within 90d"))
            .expect("below-average line");
        assert!(
            below.contains(&format!("~{:.0}%", (expected_below * 100.0).min(99.0))),
            "line = {below}, expected {:.0}%",
            expected_below * 100.0
        );
    }

    #[test]
    fn no_edge_rendering_does_not_fake_a_probability() {
        // A direction column that can only ever read "≈50%" is noise: with no
        // measured edge it must be dropped from every renderer, not printed.
        let h = series(&rising(700));
        let f = forecast("BTC", h.last().unwrap().1, &h);
        if f.direction_has_edge() {
            return;
        }
        let md = f.render_markdown(None, false);
        let plain = f.render();
        let html = f.render_html();
        let html_ru = f.render_html_for(None, true);
        for text in [&md, &plain, &html, &html_ru] {
            assert!(
                !text.contains("P(up)"),
                "direction shown without an edge:\n{text}"
            );
            assert!(
                !text.contains("P(выше)"),
                "direction shown without an edge (ru):\n{text}"
            );
            assert!(!text.contains("≈50%"), "placeholder 50% shown:\n{text}");
        }
        // The horizon table is a fenced monospace block, and the direction
        // column is absent. No ragged rows, no stray table markers.
        assert!(md.contains("```"), "{md}");
        assert!(!md.contains("| H |"), "{md}");
        let fence = md
            .split("```")
            .nth(1)
            .expect("first fenced block must be the horizon table");
        assert!(fence.contains("TARGET"), "{md}");
        assert!(fence.contains("BAND"), "{md}");
        for h in &f.horizons {
            assert!(
                fence
                    .lines()
                    .any(|l| l.trim_start().starts_with(&format!("{}d ", h.days))),
                "missing horizon row {}d in:\n{fence}",
                h.days
            );
        }
        let cols = fence.lines().next().unwrap().split_whitespace().count();
        for line in fence.lines() {
            assert!(
                line.split_whitespace().count() <= cols,
                "ragged row: {line}"
            );
        }
    }

    #[test]
    fn markdown_render_is_a_monospace_table_without_dollar_math() {
        let h = series(&rising(700));
        let f = forecast("BTC", h.last().unwrap().1, &h);
        let md = f.render_markdown(Some(f.price * 0.5), false);
        // Fence + aligned header + one row per horizon. A GFM table would let
        // the client re-flow cells and add its own block chrome.
        let fence = md.split("```").nth(1).expect("horizon table fence").trim();
        let header = fence.lines().next().expect("header row");
        assert!(header.starts_with("H "), "{header}");
        assert!(
            header.contains("TARGET") && header.contains("BAND"),
            "{header}"
        );
        for h in &f.horizons {
            let row = format!("{}d ", h.days);
            assert!(
                fence.lines().any(|l| l.starts_with(&row)),
                "missing table row {row}"
            );
        }
        // A lone `$` would be parsed as LaTeX math by Telegram.
        assert!(!md.contains('$'));
        assert!(md.contains("Bottom line") || md.contains("Итог"));
        // Russian variant localizes the verdict.
        let ru = f.render_markdown(None, true);
        assert!(ru.contains("Итог"));
    }

    #[test]
    fn student_t_helpers_match_known_values() {
        // F(0) = 0.5 for any symmetric t.
        assert!((student_t_cdf(0.0, 4.0) - 0.5).abs() < 1e-9);
        // Reference: t_{0.975, 4} = 2.776445, t_{0.84135, 4} = 1.1424.
        assert!((student_t_cdf(2.776_445, 4.0) - 0.975).abs() < 1e-5);
        assert!((student_t_cdf(1.0, 4.0) - 0.813_0).abs() < 2e-3);
        // Symmetry.
        assert!((student_t_cdf(-1.5, 4.0) - (1.0 - student_t_cdf(1.5, 4.0))).abs() < 1e-9);
        let q = t68_mult();
        assert!((q - 1.142).abs() < 0.005, "q68 = {q}");
        // And it must stay a probability everywhere.
        for t in [-50.0, -1.0, 0.0, 1.0, 50.0] {
            assert!((0.0..=1.0).contains(&student_t_cdf(t, 4.0)));
        }
    }

    #[test]
    fn sideways_regime_does_not_invent_a_downtrend() {
        // Choppy around a flat level: classification says SIDEWAYS and the model
        // must not extrapolate a slope into the 30-day target.
        let mut prices = Vec::new();
        for i in 0..600 {
            prices.push(100.0 + if i % 2 == 0 { 0.6 } else { -0.6 });
        }
        let h = series(&prices);
        let f = forecast("BTC", *prices.last().unwrap(), &h);
        assert_eq!(f.regime, "SIDEWAYS");
        // RSI may add at most ±0.08%/day; momentum is not allowed to leak in.
        assert!(
            f.drift_daily_pct.abs() <= 8.0,
            "drift={}",
            f.drift_daily_pct
        );
    }

    #[test]
    fn ewma_vol_is_between_sample_and_recent_shock() {
        // A calm series then a violent last week: EWMA must sit above the
        // long-run dispersion after the shock.
        let mut closes = vec![100.0; 200];
        for i in 0..20 {
            closes.push(if i % 2 == 0 { 90.0 } else { 110.0 });
        }
        let fold = 0.5 * volatility(&closes[..180], 20).unwrap();
        let ewma = ewma_vol(&closes, EWMA_LAMBDA).unwrap() * 100.0;
        assert!(ewma > fold, "ewma={ewma} pre-shock={fold}");
        assert!(ewma.is_finite() && ewma > 0.0);
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

    #[test]
    fn wilson_interval_is_calibrated() {
        // 50/100 is the textbook sanity point: 40.4–59.6%, straddling 50.
        let (lo, hi) = wilson_ci(0.5, 100.0, 1.96);
        assert!((lo - 40.4).abs() < 0.2, "lo={lo}");
        assert!((hi - 59.6).abs() < 0.2, "hi={hi}");
        assert!(lo < 50.0 && hi > 50.0);
        // Degenerate inputs must not escape 0..100 or produce NaN.
        let (lo, hi) = wilson_ci(0.0, 2.0, 1.96);
        assert!((0.0..=100.0).contains(&lo) && (0.0..=100.0).contains(&hi));
        let (lo, hi) = wilson_ci(0.5, f64::NAN, 1.96);
        assert_eq!((lo, hi), (0.0, 100.0));
    }

    #[test]
    fn effective_origins_discounts_overlap() {
        // Adjacent origins at a 3-day step do not overlap on a 1-day outcome.
        assert!((effective_origins(133, 1) - 133.0).abs() < 1e-9);
        // A 30-day horizon on the same origins holds only ~13 disjoint windows.
        assert!((effective_origins(133, 30) - 13.3).abs() < 1e-6);
        // Never below 2, otherwise an interval would be meaningless.
        assert_eq!(effective_origins(73, 180), 2.0);
        assert_eq!(effective_origins(0, 30), 0.0);
    }

    #[test]
    fn coin_flip_is_never_flagged_as_an_edge() {
        assert_eq!(
            edge_from_counts(50, 100, 100.0).verdict,
            EdgeVerdict::NoEdge
        );
        // The real prod case: 41% over 133 overlapping 90-day origins. Even the
        // raw count would need ~±8.5pp, and the effective count is far smaller.
        let e = edge_from_counts(55, 133, effective_origins(133, 90));
        assert_eq!(e.verdict, EdgeVerdict::NoEdge);
    }

    #[test]
    fn clear_bias_is_flagged_but_tiny_n_is_not() {
        let up = edge_from_counts(70, 100, 100.0);
        assert_eq!(up.verdict, EdgeVerdict::AboveChance);
        assert!(up.ci_low > 50.0);
        let down = edge_from_counts(30, 100, 100.0);
        assert_eq!(down.verdict, EdgeVerdict::BelowChance);
        assert!(down.ci_high < 50.0);
        // 70% on 200 hits but only 2 independent windows: not evidence.
        assert_eq!(edge_from_counts(140, 200, 2.0).verdict, EdgeVerdict::NoEdge);
    }

    #[test]
    fn accuracy_block_reports_ci_and_verdict() {
        let h = series(&rising(700));
        let f = forecast("BTC", h.last().unwrap().1, &h);
        let lines = f.accuracy_bucket_lines();
        assert!(!lines.is_empty());
        for line in &lines {
            assert!(line.contains("CI "), "missing CI: {line}");
            assert!(
                line.contains("no edge")
                    || line.contains("above 50%")
                    || line.contains("below 50%"),
                "missing verdict: {line}"
            );
        }
        let summary = f.edge_summary().expect("edge summary");
        assert!(summary.starts_with("direction:"), "{summary}");
    }

    #[test]
    fn backtest_horizons_carry_their_interval() {
        let h = series(&rising(700));
        let bt = backtest("BTC", &h, 365, &Calibration::default());
        for hz in &bt.horizons {
            if hz.samples == 0 {
                continue;
            }
            let e = hz.edge();
            // Wilson always contains the point estimate; allow for f64 epsilon.
            assert!(
                e.ci_low <= e.pct + 1e-6 && e.pct <= e.ci_high + 1e-6,
                "{:?}",
                e
            );
            assert!((0.0..=100.0).contains(&e.ci_low));
            assert!((0.0..=100.0).contains(&e.ci_high));
        }
    }
}
