//! Dip zones and first-passage probabilities — the "is this the bottom, should
//! I buy the dip?" layer.
//!
//! The forecast module answers "where might the price be at horizon H". This
//! module answers the two questions a buy-the-dip investor actually asks:
//!
//! 1. **What levels below spot are worth buying?** — the 20/50/200-day moving
//!    averages, the recent 20/90-day lows and the lower bounds of the 30/90-day
//!    forecast cones, deduplicated into a ladder of zones.
//! 2. **How likely is the price to actually reach a level within N days?** —
//!    the first-passage probability of a driftless/biased Brownian motion in
//!    log-price, i.e. the probability that the *running minimum* touches a
//!    level, not a directional guess. Direction accuracy of the forecast is
//!    only ~50%, so a touch probability is the honest framing.
//!
//! The verdict combines three deterministic inputs:
//! * the user's average entry price (from the portfolio),
//! * where the current price sits relative to the zone ladder,
//! * the probability of touching the next zone down within 7 days.
//!
//! **The average entry is a reference, not a magnet.** Above it the verdict
//! still gives a plan — a small starter tranche plus a bid on the next zone —
//! because the price may never return to the average. Only a weak, distant dip
//! with no support signal is a flat "hold".

use crate::trading::forecast::{norm_cdf, student_t_cdf, Forecast, HorizonMoments, STUDENT_DF};

/// Horizons shown in the touch-probability table.
pub const TOUCH_HORIZONS: [i64; 4] = [7, 14, 30, 90];

/// Minimum daily sigma used by the diffusion (keeps the formula stable on a
/// perfectly flat series).
const MIN_SIGMA: f64 = 1e-4;
/// Broadie–Glasserman–Kou continuity correction for barrier monitoring that
/// happens once per day rather than continuously. The model is scored against
/// the *running minimum of daily closes*, and a discretely sampled path touches
/// a level less often than a continuous one; shifting the log barrier away from
/// spot by `beta * sigma` removes that bias (beta = sqrt(ln 2 / (2 pi)) ~ 0.5826).
const DISCRETE_MONITOR_BETA: f64 = 0.5826;
/// Two candidate zones closer than this are the same level.
const ZONE_MERGE_BAND: f64 = 0.01;
/// A level closer than this to spot is not a dip worth acting on. Such levels
/// are always "touched" (96–100% at every horizon) and are noise, not support.
const MIN_DIP_GAP_PCT: f64 = 2.0;
/// Minimum distance for a *limit order* recommendation. A 2–4% pullback is
/// noise, not an entry: bidding there barely differs from buying at market yet
/// drags the average up when the position is already above it. The bid leg of a
/// split plan must therefore be a real dip.
const MIN_SCALE_IN_GAP_PCT: f64 = 5.0;

/// One level below spot worth buying at.
#[derive(Debug, Clone)]
pub struct DipZone {
    pub price: f64,
    /// Compact source label, e.g. `SMA50+90d low`.
    pub label: String,
    /// Distance from spot, percent (always negative).
    pub distance_pct: f64,
    /// `distance_pct` is from spot to [`Self::price`]; `band_top` is the nearest
    /// raw level inside the same support band, so a caller can tell whether spot
    /// is already sitting in the band even when the representative price is deeper.
    pub band_top: f64,
    /// `(days, P(touch within days))` for every horizon in [`TOUCH_HORIZONS`].
    pub touches: Vec<(i64, f64)>,
    /// `P(close below this level at the longest tracked horizon)`.
    pub prob_below_long: f64,
}

impl DipZone {
    pub fn prob_at(&self, days: i64) -> Option<f64> {
        self.touches
            .iter()
            .find(|(d, _)| *d == days)
            .map(|(_, p)| *p)
    }
}

/// What to do right now.
#[derive(Debug, Clone, PartialEq)]
pub enum DipVerdict {
    /// Price is at/below the average entry and a further dip is unlikely.
    BuyNow,
    /// Split the purchase: this share now, the rest at `zone_price`.
    ScaleIn { now_pct: u32, zone_price: f64 },
    /// Price is not in a zone yet and the ladder below is reachable.
    Wait { zone_price: Option<f64> },
    /// Price is above the user's average entry and there is no setup worth a
    /// tranche — hold, do not chase.
    AboveAverage { zone_price: Option<f64> },
    /// Not enough data to say anything useful.
    NoData,
}

impl DipVerdict {
    pub fn is_buy(&self) -> bool {
        matches!(self, DipVerdict::BuyNow | DipVerdict::ScaleIn { .. })
    }

    pub fn label(&self) -> &'static str {
        match self {
            DipVerdict::BuyNow => "BUY NOW",
            DipVerdict::ScaleIn { .. } => "SCALE IN",
            DipVerdict::Wait { .. } => "WAIT",
            DipVerdict::AboveAverage { .. } => "HOLD — ABOVE AVERAGE",
            DipVerdict::NoData => "NO VERDICT",
        }
    }

    /// Emoji badge for the verdict, used by every rendering that shows it.
    pub fn icon(&self) -> &'static str {
        match self {
            DipVerdict::BuyNow => "🟢",
            DipVerdict::ScaleIn { .. } => "🟡",
            DipVerdict::Wait { .. } => "⏳",
            DipVerdict::AboveAverage { .. } => "🔴",
            DipVerdict::NoData => "⚪",
        }
    }
}

impl DipReport {
    /// One imperative sentence that tells the user what to do, derived only from
    /// the verdict and the user's own average entry. Every dip message ends with
    /// this so a technical dump always has a conclusion.
    pub fn action_line(&self) -> String {
        let avg = self.avg_entry.filter(|a| a.is_finite() && *a > 0.0);
        let avg_txt = avg
            .map(money)
            .unwrap_or_else(|| "unknown (run /add for entry-relative advice)".to_string());
        match &self.verdict {
            DipVerdict::BuyNow => format!(
                "Action: this is the entry vs your average {avg_txt} — execute your planned tranche now, don't wait for a deeper level."
            ),
            DipVerdict::ScaleIn { now_pct, zone_price } => {
                // A limit bid only makes sense at a genuine dip. If the named level
                // sits inside the noise band (< `MIN_DIP_GAP_PCT` under spot) it is
                // effectively a market order, so never dress it up as a "limit bid".
                let real_dip = zone_price.is_finite()
                    && *zone_price > 0.0
                    && *zone_price <= self.price * (1.0 - MIN_DIP_GAP_PCT / 100.0);
                if real_dip {
                    format!(
                        "Action: take {now_pct}% now and keep the rest as a limit bid at {} — that is how you average down instead of chasing.",
                        money(*zone_price)
                    )
                } else {
                    format!(
                        "Action: take {now_pct}% now — spot is already at the deepest level on record, so there is no real dip below to park the rest at; add the remainder only on a fresh ≥{MIN_SCALE_IN_GAP_PCT:.0}% pullback."
                    )
                }
            }
            DipVerdict::Wait { zone_price: Some(z) } => format!(
                "Action: do not buy at spot — wait for {} and add there in tranches.",
                money(*z)
            ),
            DipVerdict::Wait { zone_price: None } => {
                "Action: no buy setup below spot — hold and keep your dry powder.".to_string()
            }
            DipVerdict::AboveAverage { .. } => format!(
                "Action: hold — don't average up above your average {avg_txt}; only add on a real dip."
            ),
            DipVerdict::NoData => {
                "Action: not enough data to act — no trade.".to_string()
            }
        }
    }
}

/// Full deterministic dip report for one symbol.
#[derive(Debug, Clone)]
pub struct DipReport {
    pub symbol: String,
    pub price: f64,
    pub avg_entry: Option<f64>,
    /// Zones strictly below spot, ascending (the last element is the nearest).
    pub zones: Vec<DipZone>,
    pub verdict: DipVerdict,
    /// One-line, deterministic reason for the verdict.
    pub reason: String,
    pub vol_daily_pct: f64,
    pub drift_daily_pct: f64,
    pub rsi14: Option<f64>,
    pub sma200: Option<f64>,
    pub history_days: usize,
    pub horizons: Vec<i64>,
}

// ── Probability math ────────────────────────────────────────────────────────

/// `ln Φ(x)` without underflow for very negative `x`.
fn ln_norm_cdf(x: f64) -> f64 {
    if x > -30.0 {
        norm_cdf(x).max(f64::MIN_POSITIVE).ln()
    } else {
        // Mills-ratio asymptotic: Φ(x) ≈ φ(x)/|x|.
        -0.5 * x * x - (-x).ln() - 0.5 * (2.0 * std::f64::consts::PI).ln()
    }
}

/// Probability that the price touches `level` (below spot) at least once in
/// the next `days` days, under a log-normal diffusion with daily log drift
/// `mu` and daily volatility `sigma`.
///
/// Standard first-passage result for the running minimum of a Brownian motion
/// with drift:
///
/// ```text
/// P = Phi((a - mu*H)/(sigma*sqrt(H))) + exp(2*mu*a/sigma^2) * Phi((a + mu*H)/(sigma*sqrt(H)))
/// a = ln(L/S)
/// ```
///
/// Because the barrier is only observed on daily closes, `a` is shifted away
/// from spot by the Broadie–Glasserman–Kou continuity correction so the
/// continuous-monitoring formula answers the discrete-monitoring question it is
/// actually used for.
///
/// The second term is evaluated in log space — with a small sigma and a
/// negative drift its exponent can otherwise overflow to inf while the CDF
/// underflows to zero, producing NaN.
pub fn touch_probability(price: f64, level: f64, mu: f64, sigma: f64, days: i64) -> f64 {
    if !price.is_finite()
        || !level.is_finite()
        || !mu.is_finite()
        || !sigma.is_finite()
        || price <= 0.0
        || level <= 0.0
        || days <= 0
    {
        return 0.0;
    }
    // A level at or above spot is already "touched".
    if level >= price {
        return 0.0;
    }
    let s = sigma.max(MIN_SIGMA);
    let h = days as f64;
    first_passage(price, level, mu * h, s * h.sqrt(), s)
        .max(prob_below_at_horizon(price, level, mu, s, days))
}

/// Gaussian probability that the *close* at horizon `days` is below `level`,
/// given a total log drift and a total log dispersion.
fn prob_below_gaussian(price: f64, level: f64, log_drift: f64, sigma_h: f64) -> f64 {
    if !(sigma_h > 0.0) || !sigma_h.is_finite() || !log_drift.is_finite() {
        return 0.0;
    }
    let a = (level / price).ln();
    norm_cdf((a - log_drift) / sigma_h).clamp(0.0, 1.0)
}

/// Running-minimum probability for one pair of horizon moments. Gaussian: the
/// first-passage formula has no closed form for a Student-t increment, while the
/// fat tail is already carried by [`cone_sigma_h`] and the drift-error term.
fn first_passage(price: f64, level: f64, log_drift: f64, sigma_h: f64, sigma_daily: f64) -> f64 {
    let sd = sigma_daily.max(MIN_SIGMA);
    let sh = sigma_h.max(MIN_SIGMA);
    // Discrete daily monitoring: move the barrier one sigma-step further away.
    let a = (level / price).ln() - DISCRETE_MONITOR_BETA * sd;
    let z1 = (a - log_drift) / sh;
    let z2 = (a + log_drift) / sh;
    // Second-term exponent in total-horizon units: the classic form is
    // `2*mu_daily*a / sigma_daily^2`, and since `D = mu_daily*h` and
    // `sigma_h^2 = sigma_daily^2*h`, it equals `2*D*a / sigma_h^2`.
    let log_term2 = 2.0 * log_drift * a / (sh * sh) + ln_norm_cdf(z2);
    let term2 = if log_term2 < -745.0 {
        0.0
    } else {
        log_term2.exp()
    };
    (norm_cdf(z1) + term2).clamp(0.0, 1.0)
}

/// P(close below `level`) under a Student-t with dispersion `sigma_h`. One
/// shared implementation so the cone probability, the touch floor and the raw
/// path all agree on the distribution.
fn prob_below_student_t(price: f64, level: f64, log_drift: f64, sigma_h: f64) -> f64 {
    if !price.is_finite()
        || !level.is_finite()
        || price <= 0.0
        || level <= 0.0
        || !log_drift.is_finite()
        || !(sigma_h > 0.0)
        || !sigma_h.is_finite()
    {
        return 0.0;
    }
    let a = (level / price).ln();
    student_t_cdf((a - log_drift) / sigma_h, STUDENT_DF).clamp(0.0, 1.0)
}

/// P(close below `level`) under the *same* moments the cone is drawn with.
///
/// Uses the Student-t of the cone, so the two numbers `/forecast` prints
/// (the 68% cone bounds) and the probabilities `/dip` prints are one model:
/// `prob_below_moments(price, cone_low, m) == 0.159` by construction.
pub fn prob_below_moments(price: f64, level: f64, m: HorizonMoments) -> f64 {
    // No `level >= price` shortcut here: this is the CDF of the horizon close,
    // so a level above spot must return the upper cone probability (e.g. the
    // 84th percentile for the cone high), not a blanket 1.0.
    prob_below_student_t(price, level, m.log_drift, m.sigma_h)
}

/// P(touch within the horizon) under the forecast's cone moments.
///
/// Uses the *pure diffusion* dispersion `sigma_daily * sqrt(days)` instead of
/// the cone's inflated `sigma_h`. `sigma_h` adds a drift-*estimation*-error term
/// that grows like `h²`, which is correct for the terminal cone but makes the
/// running-minimum probability non-monotone at long horizons (it can dip when
/// the horizon is extended). Touch probabilities must satisfy
/// `P(7d) <= P(14d) <= P(30d) <= P(90d)` by construction, so they use the plain
/// `sigma²·h` diffusion here. The terminal cone still comes from `sigma_h`.
pub fn touch_probability_moments(price: f64, level: f64, m: HorizonMoments) -> f64 {
    if !price.is_finite()
        || !level.is_finite()
        || price <= 0.0
        || level <= 0.0
        || !m.log_drift.is_finite()
        || !(m.sigma_daily > 0.0)
        || !m.sigma_daily.is_finite()
    {
        return 0.0;
    }
    if level >= price {
        return 0.0;
    }
    let sigma_h_pure = m.sigma_daily * (m.days.max(1) as f64).sqrt();
    // Touching a level at least once is a superset of closing below it at the
    // horizon, but the two published numbers use *different* increment models on
    // purpose: the running-minimum (touch) core is Gaussian with the pure
    // diffusion variance `sigma²·h` (the `sigma_h` drift-error term grows like
    // `h²` and would make touch non-monotone), while the terminal cone is a
    // fatter-tailed Student-t with the inflated `sigma_h`. The floors therefore
    // use the SAME pure-diffusion Gaussian as the core so the table stays
    // monotone in the horizon (`P(7d) <= P(14d) <= P(30d) <= P(90d)`).
    //
    // `/dip` never prints a touch and a close-below probability for the *same*
    // horizon, so the two models are never juxtaposed as a logical pair: it
    // shows e.g. `P(touch 30d)` next to `P(below your average in 90d)`.
    first_passage(price, level, m.log_drift, sigma_h_pure, m.sigma_daily).max(prob_below_gaussian(
        price,
        level,
        m.log_drift,
        sigma_h_pure,
    ))
}

/// Product path: `P(close at \`days\` below \`level\`)` from the forecast itself.
pub fn prob_below_forecast(forecast: &Forecast, level: f64, days: i64) -> f64 {
    prob_below_moments(forecast.price, level, forecast.moments(days))
}

/// Product path: `P(touch \`level\` within \`days\`)` from the forecast itself.
pub fn touch_probability_forecast(forecast: &Forecast, level: f64, days: i64) -> f64 {
    touch_probability_moments(forecast.price, level, forecast.moments(days))
}

/// Project a `(days, P(touch))` table onto the monotone set: the probability of
/// touching a level within a *longer* window can never be smaller than within a
/// shorter one, because the running minimum over the longer window is a superset.
///
/// The analytic first-passage core is already monotone in the horizon; the
/// Gaussian close-below floor that `touch_probability_moments` mixes in is
/// monotone too, but a table can still be assembled from hand-built moments,
/// so the user-facing table is projected by construction rather than left to
/// floating-point luck.
pub fn monotone_touches(touches: &mut [(i64, f64)]) {
    touches.sort_by_key(|(d, _)| *d);
    let mut running = 0.0_f64;
    for (_, p) in touches.iter_mut() {
        running = running.max(*p).clamp(0.0, 1.0);
        *p = running;
    }
}

/// Probability that the *close* at horizon `days` is below `level`. This is the
/// "worth waiting for the dip?" number: touched-and-recovered is different from
/// still being cheap when you would buy.
pub fn prob_below_at_horizon(price: f64, level: f64, mu: f64, sigma: f64, days: i64) -> f64 {
    if !price.is_finite()
        || !level.is_finite()
        || !mu.is_finite()
        || !sigma.is_finite()
        || price <= 0.0
        || level <= 0.0
        || days <= 0
    {
        return 0.0;
    }
    if level >= price {
        return 1.0;
    }
    let s = sigma.max(MIN_SIGMA);
    let h = days as f64;
    prob_below_gaussian(price, level, mu * h, s * h.sqrt())
}

// ── Zone ladder ─────────────────────────────────────────────────────────────

fn lowest(closes: &[f64], window: usize) -> Option<f64> {
    if closes.is_empty() || window == 0 {
        return None;
    }
    let start = closes.len().saturating_sub(window);
    let v = closes[start..]
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    v.is_finite().then_some(v)
}

/// Candidate levels from the forecast and the raw closes, before dedup.
fn candidates(forecast: &Forecast, history: &[(i64, f64)]) -> Vec<(f64, String)> {
    let closes: Vec<f64> = history.iter().map(|&(_, c)| c).collect();
    let mut out = Vec::new();
    for (value, label) in [
        (forecast.sma20, "SMA20"),
        (forecast.sma50, "SMA50"),
        (forecast.sma200, "SMA200"),
    ] {
        if let Some(v) = value {
            out.push((v, label.to_string()));
        }
    }
    if let Some(v) = lowest(&closes, 20) {
        out.push((v, "20d low".to_string()));
    }
    if let Some(v) = lowest(&closes, 90) {
        out.push((v, "90d low".to_string()));
    }
    for h in &forecast.horizons {
        let label = match h.days {
            30 => Some("30d cone"),
            90 => Some("90d cone"),
            _ => None,
        };
        if let Some(label) = label {
            out.push((h.low, label.to_string()));
        }
    }
    out
}

/// Sort ascending and collapse levels within [`ZONE_MERGE_BAND`] of the cluster's
/// *deepest* member.
///
/// Returns `(deepest, mean, nearest, label)` per cluster. Anchoring on the
/// deepest member keeps a run of sub-1% steps from drifting above spot as the
/// cluster grows.
fn merge_levels(mut levels: Vec<(f64, String)>) -> Vec<(f64, f64, f64, String)> {
    levels.retain(|(p, _)| p.is_finite() && *p > 0.0);
    levels.sort_by(|a, b| a.0.total_cmp(&b.0));
    // (deepest, sum, count, nearest, label)
    let mut out: Vec<(f64, f64, usize, f64, String)> = Vec::new();
    for (price, label) in levels {
        if let Some(last) = out.last_mut() {
            if (price - last.0) / last.0 < ZONE_MERGE_BAND {
                last.1 += price;
                last.2 += 1;
                last.3 = price;
                if !last.4.split('+').any(|part| part == label) {
                    last.4 = format!("{}+{}", last.4, label);
                }
                continue;
            }
        }
        out.push((price, price, 1, price, label));
    }
    out.into_iter()
        .map(|(deepest, sum, n, nearest, label)| (deepest, sum / n as f64, nearest, label))
        .collect()
}

fn touch_horizons(history_days: usize) -> Vec<i64> {
    let max_h = (history_days / 4).max(TOUCH_HORIZONS[0] as usize) as i64;
    let kept: Vec<i64> = TOUCH_HORIZONS
        .iter()
        .copied()
        .filter(|h| *h <= max_h)
        .collect();
    if kept.is_empty() {
        vec![TOUCH_HORIZONS[0]]
    } else {
        kept
    }
}

/// Build the full report. `forecast` provides price, drift, volatility and the
/// cone; `history` the raw closes used for the recent lows.
pub fn dip_report(
    forecast: &Forecast,
    history: &[(i64, f64)],
    avg_entry: Option<f64>,
) -> DipReport {
    let price = forecast.price;
    let horizons = touch_horizons(history.len());
    let long_h = *horizons.last().unwrap_or(&30);

    let mut zones: Vec<DipZone> = merge_levels(candidates(forecast, history))
        .into_iter()
        // Keep the band if its *deepest* member is below spot: a band that
        // straddles spot still contains real support the price is sitting in.
        .filter(|(deepest, _, _, _)| *deepest < price * 0.999)
        .map(|(deepest, mean, nearest, label)| {
            let level = mean.min(price * 0.999).max(deepest);
            DipZone {
                price: level,
                label,
                distance_pct: (level / price - 1.0) * 100.0,
                band_top: nearest,
                touches: horizons
                    .iter()
                    .map(|&d| (d, touch_probability_forecast(forecast, level, d)))
                    .collect(),
                prob_below_long: prob_below_forecast(forecast, level, long_h),
            }
        })
        .collect();
    // The running minimum over a longer window is a superset: never let a longer
    // horizon report a lower touch chance than a shorter one.
    for z in zones.iter_mut() {
        monotone_touches(&mut z.touches);
    }
    // Ascending order, so the nearest zone is the last element.
    zones.sort_by(|a, b| a.price.total_cmp(&b.price));

    let (verdict, reason) = decide(
        price,
        avg_entry,
        &zones,
        forecast.rsi14,
        forecast.sma200,
        forecast,
        lowest(&history.iter().map(|&(_, c)| c).collect::<Vec<_>>(), 20)
            .map(|low| price > low * 1.005)
            .unwrap_or(false),
    );

    DipReport {
        symbol: forecast.symbol.clone(),
        price,
        avg_entry,
        zones,
        verdict,
        reason,
        vol_daily_pct: forecast.vol_daily_pct,
        drift_daily_pct: forecast.drift_daily_pct,
        rsi14: forecast.rsi14,
        sma200: forecast.sma200,
        history_days: history.len(),
        horizons,
    }
}

fn decide(
    price: f64,
    avg_entry: Option<f64>,
    zones: &[DipZone],
    rsi: Option<f64>,
    sma200: Option<f64>,
    forecast: &Forecast,
    bouncing: bool,
) -> (DipVerdict, String) {
    if !price.is_finite() || price <= 0.0 {
        return (DipVerdict::NoData, "no usable price".to_string());
    }

    if let Some(avg) = avg_entry.filter(|a| *a > 0.0) {
        if price > avg * 1.0001 {
            let vs_avg = (price / avg - 1.0) * 100.0;
            let vs_avg_txt = crate::format::fmt_pct_signed(vs_avg, 1);
            // Only a level at least [`MIN_DIP_GAP_PCT`] under spot counts as a
            // dip worth acting on; a 20-day low 1% under spot is noise. The bid
            // itself needs a wider gap ([`MIN_SCALE_IN_GAP_PCT`]), otherwise the
            // "wait for the dip" leg is just a market order in disguise.
            // Two tiers below spot, kept apart on purpose:
            //   * `deep` — a zone at least [`MIN_SCALE_IN_GAP_PCT`] down; the
            //     only level that can carry a limit order.
            //   * `shallow` — a level in the 2–5% noise band. It may be the
            //     nearest thing the chart can do, so the text must name it
            //     instead of quoting a far level as "nearest".
            let deep = zones
                .iter()
                .rev()
                .find(|z| z.distance_pct <= -MIN_SCALE_IN_GAP_PCT);
            let shallow = zones
                .iter()
                .rev()
                .find(|z| z.distance_pct <= -MIN_DIP_GAP_PCT);
            let target = deep.or(shallow);
            let p30 = target.map(|z| z.prob_at(30).unwrap_or(0.0)).unwrap_or(0.0);
            // The average is a reference, not a magnet: price may never trade
            // there again. Give that probability instead of a blanket "don't add".
            let p_avg_90 = prob_below_forecast(forecast, avg, 90).min(0.99);

            // Still above the average, but deeply oversold at trend support:
            // a small first tranche is defensible even though it raises the average.
            // This mirrors `/forecast`, which already calls for a small starter
            // tranche in exactly this state — the two commands must not disagree.
            // A bounce that is already underway only sizes the starter leg up.
            let oversold_support = rsi.is_some_and(|r| r <= 30.0)
                && sma200.is_some_and(|s| s > 0.0 && price <= s * 1.03);

            // Above the average but the next zone is reachable: that is a split
            // plan, not a prohibition. Hoarding 100% of the cash because the
            // average may come back is a bet the bot must not force.
            if let Some(z) = target {
                if (oversold_support || p30 >= 0.50) && z.distance_pct <= -MIN_SCALE_IN_GAP_PCT {
                    let now_pct = if oversold_support {
                        if bouncing {
                            25
                        } else {
                            15
                        }
                    } else {
                        20
                    };
                    let why = if oversold_support {
                        let bounce = if bouncing { ", bounce underway" } else { "" };
                        format!(
                            "RSI {} with price at trend support{bounce}",
                            rsi.map(|r| format!("{r:.0}"))
                                .unwrap_or_else(|| "n/a".into())
                        )
                    } else {
                        format!(
                            "the {} dip is likely: P(touch 30d) {:.0}%",
                            money(z.price),
                            p30 * 100.0
                        )
                    };
                    return (
                        DipVerdict::ScaleIn {
                            now_pct,
                            zone_price: z.price,
                        },
                        format!(
                            "spot is {vs_avg_txt} vs your average {} and {why} — take {now_pct}% now, keep the rest as a bid at {} ({:+.1}%). The average is a reference, not a magnet: P(below it in 90d) {:.0}%",
                            money(avg),
                            money(z.price),
                            z.distance_pct,
                            p_avg_90 * 100.0
                        ),
                    );
                }
            }

            let reason = match target {
                Some(z) => {
                    let target_vs_avg = (z.price / avg - 1.0) * 100.0;
                    let improves = target_vs_avg < 0.0;
                    let avg_part = if improves {
                        format!(
                            " — bidding there would put the add {:.1}% below your average",
                            -target_vs_avg
                        )
                    } else {
                        " — even that dip sits above your average, so it would not lower it".to_string()
                    };
                    // If a shallower level sits between spot and the bid, say so:
                    // the ladder prints it, so calling the far level "nearest"
                    // reads like the bot cannot see its own table.
                    let shallow_part = match shallow.filter(|s| s.price > z.price) {
                        Some(s) if z.distance_pct <= -MIN_SCALE_IN_GAP_PCT => format!(
                            " · the nearer {} {} ({}) is inside the {MIN_SCALE_IN_GAP_PCT:.0}% noise band, so no bid there",
                            s.label,
                            money(s.price),
                            crate::format::fmt_pct_signed(s.distance_pct, 1)
                        ),
                        _ => String::new(),
                    };
                    let label = if z.distance_pct <= -MIN_SCALE_IN_GAP_PCT {
                        "nearest dip"
                    } else {
                        "only level below spot"
                    };
                    let target_part = if z.distance_pct <= -MIN_SCALE_IN_GAP_PCT {
                        format!(
                            "{label} {} ({:+.1}%) · P(touch 30d) {:.0}%",
                            money(z.price),
                            z.distance_pct,
                            p30 * 100.0
                        )
                    } else {
                        format!(
                            "{label} {} has only P(touch 30d) {:.0}% and sits inside the {MIN_SCALE_IN_GAP_PCT:.0}% noise band — waiting for it is not a plan",
                            money(z.price),
                            p30 * 100.0
                        )
                    };
                    format!(
                        "spot is {vs_avg_txt} vs your average {} · {target_part} · P(below your average in 90d) {:.0}%{avg_part}{shallow_part}",
                        money(avg),
                        p_avg_90 * 100.0
                    )
                }
                None => format!(
                    "spot is {vs_avg_txt} vs your average {} and no meaningful dip (≥{MIN_DIP_GAP_PCT:.0}%) sits below spot · P(below your average in 90d) {:.0}%",
                    money(avg),
                    p_avg_90 * 100.0
                ),
            };

            // No reachable dip: holding is honest. The reason still carries the
            // probability that the average is ever revisited, so "wait" is not
            // framed as a guarantee. `zone_price` only ever names a level that
            // could actually carry a limit order.
            return (
                DipVerdict::AboveAverage {
                    zone_price: deep.map(|z| z.price),
                },
                reason,
            );
        }
    }

    let below_average = avg_entry.is_some_and(|a| a > 0.0 && price <= a * 1.0001);
    let cheap_oscillator = rsi.is_some_and(|r| r < 40.0);
    let below_trend = sma200.is_some_and(|s| s > 0.0 && price < s);
    // "At a zone" = a level within 1% below spot, or a band whose *nearest*
    // member is within 1% (spot is already inside that support band). A sub-1%
    // gap is already at the level.
    let at_zone = zones
        .iter()
        .rev()
        .find(|z| z.distance_pct >= -1.0 || z.band_top >= price * 0.99);
    // Nearest level at least 1% down: used for messaging and the probability
    // read, because it is the next thing the chart can actually do.
    let near = zones.iter().rev().find(|z| z.distance_pct < -1.0);
    // A limit order must be a real dip. A zone 2–4% under spot is noise: bidding
    // there is a market order in disguise that also drags the average up. Such
    // shallow zones cannot become the bid leg of a split plan.
    let bid = zones
        .iter()
        .rev()
        .find(|z| z.distance_pct <= -MIN_SCALE_IN_GAP_PCT);

    let level_desc =
        |z: &DipZone| format!("{} {} ({:+.1}%)", z.label, money(z.price), z.distance_pct);
    let p7_text = |z: &DipZone| format!("{:.0}%", z.prob_at(7).unwrap_or(0.0) * 100.0);

    let Some(near) = near else {
        if let Some(z) = at_zone {
            let zone = format!("{} {}", z.label, money(z.price));
            if below_average {
                let vs_avg_txt = crate::format::fmt_pct_signed(
                    avg_entry.map(|a| (price / a - 1.0) * 100.0).unwrap_or(0.0),
                    1,
                );
                return (
                    DipVerdict::BuyNow,
                    format!(
                        "spot is sitting at {zone}, the deepest zone below spot, and {vs_avg_txt} vs your average — this is the entry"
                    ),
                );
            }
            return (
                DipVerdict::ScaleIn {
                    now_pct: 50,
                    zone_price: z.price,
                },
                format!(
                    "spot is sitting at {zone}, the deepest zone on record; no average entry configured — split the buy and /add your purchases for entry-relative advice"
                ),
            );
        }
        if below_average {
            let reason = avg_entry
                .map(|a| {
                    format!(
                        "spot is {:+.1}% below your average {}; no support zone below spot — this is already the deep end",
                        (price / a - 1.0) * 100.0,
                        money(a)
                    )
                })
                .unwrap_or_else(|| "no zone below spot".to_string());
            return (DipVerdict::BuyNow, reason);
        }
        return (
            DipVerdict::Wait { zone_price: None },
            "no dip zone below spot — nothing to wait for".to_string(),
        );
    };

    // Price already sits on a zone: buy now, unless a real dip is likely next.
    if let Some(z) = at_zone {
        let zone_desc = format!("{} {}", z.label, money(z.price));
        if let Some(b) = bid.filter(|b| b.prob_at(7).unwrap_or(0.0) >= 0.55) {
            return (
                DipVerdict::ScaleIn {
                    now_pct: 33,
                    zone_price: b.price,
                },
                format!(
                    "spot is sitting at {zone_desc}, but {} is likely next: P(7d) {} · buy 1/3 now, 2/3 there",
                    level_desc(b),
                    p7_text(b)
                ),
            );
        }
        if below_average {
            return (
                DipVerdict::BuyNow,
                format!(
                    "spot is sitting at {zone_desc} and is {:+.1}% vs your average; the next real dip {} is only P(7d) {} — buy now",
                    avg_entry.map(|a| (price / a - 1.0) * 100.0).unwrap_or(0.0),
                    level_desc(near),
                    p7_text(near)
                ),
            );
        }
        return (
            DipVerdict::ScaleIn {
                now_pct: 50,
                // Park the remainder at the next *real* dip, not at the level
                // spot already sits on (that is not a bid, just a market order).
                zone_price: bid.map(|b| b.price).unwrap_or(z.price),
            },
            match bid {
                Some(b) => format!(
                    "spot is sitting at {zone_desc}; the next real dip {} is only P(7d) {} — split the buy (no average entry configured)",
                    level_desc(b),
                    p7_text(b)
                ),
                None => format!(
                    "spot is sitting at {zone_desc}; every level below spot is within the 5% noise band — split the buy now (no average entry configured)"
                ),
            },
        );
    }

    // A dip at least 5% under spot exists: it is the only legitimate bid.
    if let Some(b) = bid {
        let p7 = b.prob_at(7).unwrap_or(0.0);
        let b_desc = level_desc(b);
        let p7_txt = p7_text(b);
        if p7 >= 0.55 {
            return (
                DipVerdict::ScaleIn {
                    now_pct: 33,
                    zone_price: b.price,
                },
                format!(
                    "{b_desc} is likely to be touched: P(7d) {p7_txt} · buy 1/3 now, 2/3 there"
                ),
            );
        }

        // A further dip is unlikely: waiting costs more than buying now.
        if p7 <= 0.35 {
            if below_average {
                return (
                    DipVerdict::BuyNow,
                    format!(
                        "P(touch {b_desc} in 7d) is only {p7_txt} and spot is {}; waiting is not worth it",
                        cheap_hint(below_average, cheap_oscillator, below_trend)
                    ),
                );
            }
            if cheap_oscillator || below_trend {
                return (
                    DipVerdict::ScaleIn {
                        now_pct: 50,
                        zone_price: b.price,
                    },
                    format!(
                        "P(touch {b_desc} in 7d) is only {p7_txt} and spot looks cheap ({}); do not wait for it",
                        cheap_hint(below_average, cheap_oscillator, below_trend)
                    ),
                );
            }
            return (
                DipVerdict::Wait {
                    zone_price: Some(b.price),
                },
                format!(
                    "{b_desc} has only P(7d) {p7_txt}; no average entry is known — /add purchases for entry-relative advice"
                ),
            );
        }

        if below_average {
            return (
                DipVerdict::ScaleIn {
                    now_pct: 50,
                    zone_price: b.price,
                },
                format!(
                    "spot is under your average, but {b_desc} is close: P(7d) {p7_txt} · split the buy"
                ),
            );
        }

        let reason = format!(
            "wait for {b_desc} — P(touch 7d) {p7_txt}, P(below at {}d) {:.0}%",
            *TOUCH_HORIZONS.last().unwrap_or(&90),
            b.prob_below_long * 100.0
        );
        return (
            DipVerdict::Wait {
                zone_price: Some(b.price),
            },
            reason,
        );
    }

    // Every level below spot is inside the 5% noise band. There is no dip to
    // wait for: a limit there is a market order that also drags the average up.
    if below_average {
        return (
            DipVerdict::BuyNow,
            format!(
                "spot is {:+.1}% below your average {} and every level below spot is a sub-5% wiggle — buy now",
                avg_entry.map(|a| (price / a - 1.0) * 100.0).unwrap_or(0.0),
                avg_entry.map(money).unwrap_or_default()
            ),
        );
    }
    (
        DipVerdict::Wait {
            zone_price: Some(near.price),
        },
        format!(
            "the only level below spot is {} — inside the 5% noise band, not a dip. /add your average for entry-relative advice",
            level_desc(near)
        ),
    )
}

fn cheap_hint(below_average: bool, oversold: bool, below_trend: bool) -> String {
    let mut parts = Vec::new();
    if below_average {
        parts.push("below your average");
    }
    if oversold {
        parts.push("RSI < 40");
    }
    if below_trend {
        parts.push("below SMA200");
    }
    parts.join(", ")
}

// ── Rendering ───────────────────────────────────────────────────────────────

fn decimals(price: f64) -> usize {
    crate::format::price_decimals(price)
}

fn money(price: f64) -> String {
    // Thousands-grouped (`$60,571`) so BTC-sized levels are readable at a
    // glance; the ladder's LEVEL column is 10 wide, which still fits.
    format!(
        "${}",
        crate::format::format_thousands(price, decimals(price))
    )
}

/// Merged zones can carry several labels; show the first plus a count so the
/// table stays narrow and aligned.
fn short_label(label: &str) -> String {
    let parts: Vec<&str> = label.split('+').collect();
    if parts.len() <= 2 {
        label.to_string()
    } else {
        format!("{}+{}", parts[0], parts.len() - 1)
    }
}

fn pct(p: f64) -> String {
    format!("{:.0}%", (p * 100.0).clamp(0.0, 100.0))
}

impl DipReport {
    /// Zones worth showing: levels at least [`MIN_DIP_GAP_PCT`] under spot.
    /// Shallower levels are always "touched" and only make the ladder look like
    /// a guaranteed bottom. Also drops distant tails the model gives less than a
    /// 10% chance of reaching at the longest horizon, so the merged `/analyze`
    /// screen only offers realistic top-up levels.
    pub fn display_zones(&self) -> Vec<&DipZone> {
        let candidates: Vec<&DipZone> = self
            .zones
            .iter()
            .filter(|z| z.distance_pct <= -MIN_DIP_GAP_PCT)
            .collect();
        // Drop distant tails that the model gives less than a 10% chance of ever
        // reaching at the longest horizon (e.g. a -35% level with a 0-1% touch
        // chance only made the ladder look like a fantasy). The nearest level is
        // always kept so the user still sees the next real bid.
        let longest = self.horizons.last().copied();
        let nearest = candidates.len().saturating_sub(1);
        candidates
            .into_iter()
            .enumerate()
            .filter(|(i, z)| {
                *i == nearest
                    || longest
                        .and_then(|d| z.prob_at(d))
                        .map(|p| p >= 0.10)
                        .unwrap_or(true)
            })
            .map(|(_, z)| z)
            .collect()
    }

    fn table_lines(&self) -> Vec<String> {
        let shown = self.display_zones();
        let labels: Vec<String> = shown.iter().map(|z| short_label(&z.label)).collect();
        // Dynamic source width keeps the columns aligned even for a merged
        // "SMA50+3" label, without overflowing a phone screen.
        let label_w = labels
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(10, 16);
        let mut lines = Vec::new();
        let mut header = format!(
            "{:<10}{:<width$}{:>8}",
            "LEVEL",
            "SOURCE",
            "DIST",
            width = label_w
        );
        for h in &self.horizons {
            header.push_str(&format!("{:>6}", format!("{h}d")));
        }
        lines.push(header);
        for (z, label) in shown.iter().zip(labels.iter()) {
            let mut row = format!(
                "{:<10}{:<width$}{:>8}",
                money(z.price),
                label,
                format!("{:+.1}%", z.distance_pct),
                width = label_w
            );
            for h in &self.horizons {
                row.push_str(&format!("{:>6}", pct(z.prob_at(*h).unwrap_or(0.0))));
            }
            lines.push(row);
        }
        if shown.is_empty() {
            lines.push(format!(
                "(price is sitting on support — no zone ≥{MIN_DIP_GAP_PCT:.0}% below spot)"
            ));
        }
        lines
    }

    fn header_line(&self) -> String {
        let avg = match self.avg_entry {
            Some(a) if a > 0.0 => format!(
                "avg entry <code>{}</code> · spot <b>{}</b> vs average",
                money(a),
                crate::format::fmt_pct_signed((self.price / a - 1.0) * 100.0, 1)
            ),
            _ => "average entry unknown — run <code>/add</code> to enable entry-relative advice"
                .to_string(),
        };
        format!(
            "🎯 <b>{} dip zones</b> · <code>{}</code>\n{}\nvol <code>{:.2}%/day</code> · drift <code>{:+.3}%/day</code> · RSI <code>{}</code> · SMA200 <code>{}</code> · {} closes\n",
            html_escape::encode_text(&self.symbol),
            money(self.price),
            avg,
            self.vol_daily_pct,
            self.drift_daily_pct,
            self.rsi14
                .map(|r| format!("{r:.0}"))
                .unwrap_or_else(|| "n/a".into()),
            self.sma200.map(money).unwrap_or_else(|| "n/a".into()),
            self.history_days,
        )
    }

    /// Telegram HTML: bold header, the ladder in a single `<pre>` for aligned
    /// columns, then a bold BOTTOM LINE with the deterministic reason.
    pub fn render_html(&self) -> String {
        let esc = html_escape::encode_text;
        let mut out = self.header_line();
        out.push_str(&format!(
            "\n<b>P(touch within)</b> — model-implied chance of reaching each level:\n<pre>{}</pre>\n",
            esc(&self.table_lines().join("\n"))
        ));
        out.push_str(&format!(
            "\n{} <b>BOTTOM LINE — {}</b>\n{}\n",
            self.verdict.icon(),
            self.verdict.label(),
            esc(&self.reason)
        ));
        out.push_str(&format!("\n<b>{}</b>\n", esc(&self.action_line())));
        out.push_str(
            "<i>Touch probabilities from a drift/volatility diffusion over the local closes.</i>",
        );
        out
    }

    /// Compact version for the AI fallback path.
    pub fn summary_html(&self) -> String {
        let mut out = format!(
            "🎯 <b>{} dip read</b> · <code>{}</code>\n{} <b>{}</b>\n{}\n",
            html_escape::encode_text(&self.symbol),
            money(self.price),
            self.verdict.icon(),
            self.verdict.label(),
            html_escape::encode_text(&self.reason)
        );
        if let Some(z) = self
            .zones
            .iter()
            .rev()
            .find(|z| z.distance_pct <= -MIN_DIP_GAP_PCT)
        {
            out.push_str(&format!(
                "nearest zone <code>{}</code> ({}), P(7d) <code>{}</code>\n",
                money(z.price),
                html_escape::encode_text(&z.label),
                pct(z.prob_at(7).unwrap_or(0.0))
            ));
        }
        out.push_str(&format!(
            "\n{}",
            html_escape::encode_text(&self.action_line())
        ));
        out
    }

    /// Plain text for the CLI.
    pub fn plain(&self) -> String {
        let mut out = format!(
            "{} @ {}  |  avg entry {}  |  vol {:.2}%/day  drift {:+.3}%/day  RSI {}\n",
            self.symbol,
            money(self.price),
            self.avg_entry.map(money).unwrap_or_else(|| "n/a".into()),
            self.vol_daily_pct,
            self.drift_daily_pct,
            self.rsi14
                .map(|r| format!("{r:.0}"))
                .unwrap_or_else(|| "n/a".into()),
        );
        out.push_str("P(touch within) — first-passage probabilities\n");
        for line in self.table_lines() {
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str(&format!(
            "\nBOTTOM LINE — {}\n{}\n",
            self.verdict.label(),
            self.reason
        ));
        out.push_str(&self.action_line());
        out.push('\n');
        out.push_str("Touch probability ≠ prediction.\n");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::forecast;

    fn series(prices: &[f64]) -> Vec<(i64, f64)> {
        let mut ts = 1_600_000_000_000i64;
        prices
            .iter()
            .map(|&p| {
                let row = (ts, p);
                ts += 86_400_000;
                row
            })
            .collect()
    }

    fn report(prices: &[f64], avg: Option<f64>) -> DipReport {
        let h = series(prices);
        let price = *prices.last().unwrap();
        let f = forecast::forecast("BTC", price, &h);
        dip_report(&f, &h, avg)
    }

    /// Broad monotonicity regression on the analytic core: for *any* drift /
    /// volatility / decay, the first-passage probability of touching a level
    /// within a longer window can never be lower than for a shorter one (the
    /// running minimum over a longer window is a superset). A hard clamp on the
    /// *cumulative* drift used to break this (e.g. -10% level, -1%/day drift,
    /// 2%/day vol: 30d 97% > 90d 94%).
    #[test]
    fn money_groups_thousands_without_losing_precision() {
        // The ladder must read like the rest of the bot (`$60,571`), not `$60571`,
        // and cheap assets keep their decimals.
        assert_eq!(money(60_571.0), "$60,571");
        assert_eq!(money(8.5591), "$8.559");
        assert_eq!(money(0.123456), "$0.12346");
    }

    #[test]
    fn first_passage_is_monotone_over_a_wide_grid() {
        let price = 100.0_f64;
        let horizons = [1i64, 7, 14, 30, 90, 180];
        let level_pcts = [
            -1.0f64, -2.0, -3.0, -5.0, -8.0, -10.0, -15.0, -20.0, -25.0, -30.0, -40.0, -50.0, -60.0,
        ];
        let mus = [
            -0.01f64, -0.005, -0.002, -0.0005, 0.0, 0.0005, 0.002, 0.005, 0.01,
        ];
        let sigmas = [0.005f64, 0.01, 0.02, 0.03, 0.05, 0.08];
        let decays = [15.0f64, 30.0, 60.0, 90.0, 180.0, f64::INFINITY];
        let mut checked = 0usize;
        for &lvl_pct in &level_pcts {
            let level = price * (1.0 + lvl_pct / 100.0);
            for &mu in &mus {
                for &sigma in &sigmas {
                    for &decay in &decays {
                        let probs: Vec<f64> = horizons
                            .iter()
                            .map(|&d| {
                                let m =
                                    crate::trading::forecast::horizon_moments(mu, sigma, d, decay);
                                touch_probability_moments(price, level, m)
                            })
                            .collect();
                        for p in &probs {
                            assert!(
                                p.is_finite() && (0.0..=1.0).contains(p),
                                "lvl {lvl_pct}% mu {mu} sig {sigma} decay {decay}: {probs:?}"
                            );
                        }
                        for w in probs.windows(2) {
                            checked += 1;
                            assert!(
                                w[1] + 1e-9 >= w[0],
                                "non-monotone touch probability: lvl {lvl_pct}% mu {mu} \
                                 sig {sigma} decay {decay}: {probs:?}"
                            );
                        }
                    }
                }
            }
        }
        assert!(checked >= 20_000, "grid too small: {checked} pairs");
    }

    /// The *user-facing* dip table must be monotone across horizons for a wide
    /// range of synthetic market shapes (trend, chop, crash, flat, high-vol).
    #[test]
    fn dip_tables_are_monotone_over_many_synthetic_series() {
        let mut series_checked = 0usize;
        for seed in 0..400u64 {
            let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut rnd = || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x as f64 / u64::MAX as f64) * 2.0 - 1.0
            };
            let (drift, vol, n) = match seed % 5 {
                0 => (0.002, 0.01, 300),   // steady uptrend
                1 => (-0.002, 0.01, 300),  // steady downtrend
                2 => (0.0, 0.05, 400),     // high-vol chop
                3 => (-0.006, 0.06, 350),  // crash + high vol
                _ => (0.0005, 0.005, 250), // quiet
            };
            let mut price = 100.0_f64;
            let mut prices = Vec::with_capacity(n);
            for _ in 0..n {
                price *= (drift + vol * rnd()).exp();
                prices.push(price);
            }
            let history = series(&prices);
            let spot = *prices.last().unwrap();
            let f = forecast::forecast("SOL", spot, &history);
            let report = dip_report(&f, &history, Some(spot * 0.9));
            for z in &report.zones {
                for w in z.touches.windows(2) {
                    assert!(
                        w[1].1 + 1e-12 >= w[0].1,
                        "seed {seed} zone {}: {:?} not monotone",
                        z.price,
                        z.touches
                    );
                }
                for (_, p) in &z.touches {
                    assert!(p.is_finite() && (0.0..=1.0).contains(p));
                }
            }
            series_checked += 1;
        }
        assert_eq!(series_checked, 400);
    }

    #[test]
    fn touch_probability_is_bounded_and_monotonic() {
        let mu = 0.0;
        let sigma = 0.04;
        let p7 = touch_probability(100.0, 90.0, mu, sigma, 7);
        let p30 = touch_probability(100.0, 90.0, mu, sigma, 30);
        assert!((0.0..=1.0).contains(&p7));
        assert!(p30 >= p7, "longer window must not lower the touch chance");
        // A closer level is easier to touch.
        let near = touch_probability(100.0, 95.0, mu, sigma, 7);
        let far = touch_probability(100.0, 80.0, mu, sigma, 7);
        assert!(near > far);
        // A level at or above spot is already touched — we report 0 (not a dip).
        assert_eq!(touch_probability(100.0, 100.0, mu, sigma, 7), 0.0);
        assert_eq!(touch_probability(100.0, 110.0, mu, sigma, 7), 0.0);
    }

    #[test]
    fn touch_probability_is_finite_for_extreme_drift() {
        // Small sigma plus a strong negative drift overflows the exp() term.
        let p = touch_probability(100.0, 70.0, -0.005, 0.0026, 90);
        assert!(p.is_finite() && (0.0..=1.0).contains(&p), "p = {p}");
    }

    #[test]
    fn prob_below_matches_drift_direction() {
        let up = prob_below_at_horizon(100.0, 90.0, 0.002, 0.04, 30);
        let down = prob_below_at_horizon(100.0, 90.0, -0.002, 0.04, 30);
        assert!(down > up);
        assert_eq!(prob_below_at_horizon(100.0, 110.0, 0.0, 0.04, 30), 1.0);
    }

    /// The cone printed by `/forecast` and the probabilities printed by `/dip`
    /// must come from one model: by definition of the cone, the close at the
    /// horizon is below its lower bound ~15.9% of the time and below its upper
    /// bound ~84.1%. If this fails, `/dip` is once again answering with its own
    /// private diffusion instead of the calibrated one.
    #[test]
    fn cone_bounds_and_dip_probabilities_are_one_model() {
        let prices: Vec<f64> = (0..400)
            .map(|i| {
                let i = i as f64;
                100.0 * 1.0004f64.powf(i) * (1.0 + 0.03 * (i * 0.7).sin())
            })
            .collect();
        let history = series(&prices);
        let price = *prices.last().unwrap();

        for symbol in ["BTC", "ETH", "SOL", "LINK"] {
            let f = forecast::forecast(symbol, price, &history);
            for hz in &f.horizons {
                let m = f.moments(hz.days);
                assert!(m.log_drift.abs() <= 0.35 + 1e-12);
                assert!(m.sigma_h >= m.sigma_daily);
                if hz.low < price {
                    let p = prob_below_forecast(&f, hz.low, hz.days);
                    assert!(
                        (p - 0.1587).abs() < 5e-3,
                        "{symbol} {}d cone low: P(below) = {p}",
                        hz.days
                    );
                    let t = touch_probability_forecast(&f, hz.low, hz.days);
                    // Touch probabilities deliberately use the pure diffusion
                    // variance (`sigma_daily·√h`) while the cone uses the wider
                    // `sigma_h` (diffusion + drift-estimation error). So `t` is
                    // NOT guaranteed to exceed the Student-t terminal CDF `p`
                    // here — only to be a sane probability. Monotonicity of `t`
                    // across horizons is checked by the grid test.
                    assert!(
                        (0.0..=1.0).contains(&t) && t.is_finite(),
                        "{symbol} {}d: P(touch) {t} out of range",
                        hz.days
                    );
                    assert!(
                        t >= prob_below_gaussian(
                            f.price,
                            hz.low,
                            m.log_drift,
                            m.sigma_daily * (hz.days.max(1) as f64).sqrt(),
                        ) - 1e-12,
                        "{symbol} {}d: P(touch) {t} below Gaussian close-below",
                        hz.days
                    );
                }
                if hz.high > price {
                    let p = prob_below_forecast(&f, hz.high, hz.days);
                    assert!(
                        (p - 0.8413).abs() < 5e-3,
                        "{symbol} {}d cone high: P(below) = {p}",
                        hz.days
                    );
                }
            }

            // Depth monotonicity through the same path `/dip` uses.
            let mut prev = 1.0;
            for level_pct in [-2.0f64, -5.0, -10.0, -20.0, -30.0] {
                let level = price * (1.0 + level_pct / 100.0);
                let p = prob_below_forecast(&f, level, 30);
                assert!(p <= prev + 1e-12, "{symbol} {level_pct}%: {p} > {prev}");
                prev = p;
            }
        }
    }

    /// Regression for the first-passage second term: it must be evaluated with
    /// the *total* horizon drift and dispersion. Passing the total drift to a
    /// per-day exponent inflated it by `h` days and collapsed every probability
    /// to 1.0 (which silently broke `/sniper` bucket ordering).
    #[test]
    fn first_passage_matches_the_closed_form_in_total_units() {
        let price = 100.0;
        let reference = |price: f64, level: f64, mu: f64, sigma: f64, days: i64| -> f64 {
            let s = sigma.max(MIN_SIGMA);
            let h = days as f64;
            let a = (level / price).ln() - DISCRETE_MONITOR_BETA * s;
            let sh = s * h.sqrt();
            let z1 = (a - mu * h) / sh;
            let z2 = (a + mu * h) / sh;
            let term2 = (2.0 * mu * a / (s * s)).exp() * norm_cdf(z2);
            (norm_cdf(z1) + term2).clamp(0.0, 1.0)
        };
        for &days in &[1i64, 7, 30, 90] {
            for &mu in &[-0.004f64, -0.001, 0.001, 0.004] {
                for &level_pct in &[-2.0f64, -10.0, -25.0] {
                    let level = price * (1.0 + level_pct / 100.0);
                    let sigma = 0.02;
                    let got = first_passage(
                        price,
                        level,
                        mu * days as f64,
                        sigma * (days as f64).sqrt(),
                        sigma,
                    );
                    let want = reference(price, level, mu, sigma, days);
                    assert!(
                        (got - want).abs() < 1e-9,
                        "days={days} mu={mu} lvl={level_pct}%: got {got}, want {want}"
                    );
                }
            }
        }
    }

    /// Regression for the P=1.0 collapse under a small sigma and a negative
    /// drift: a dip 2–10% below spot was reported as a certain touch.
    #[test]
    fn moments_touch_stays_probabilistic_under_negative_drift() {
        let price = 100.0;
        let m = HorizonMoments {
            log_drift: -0.005,
            sigma_h: 0.016,
            sigma_daily: 0.003,
            days: 30,
        };
        let mut prev = 1.0;
        for level_pct in [-2.0f64, -5.0, -10.0, -20.0] {
            let level = price * (1.0 + level_pct / 100.0);
            let p = touch_probability_moments(price, level, m);
            assert!(p < 0.999, "{level_pct}%: P(touch) collapsed to {p}");
            assert!(p <= prev + 1e-12, "{level_pct}%: {p} > {prev}");
            prev = p;
        }
    }

    #[test]
    fn dip_and_forecast_report_the_same_probability() {
        // The user-visible contradiction was `/dip` and `/forecast` disagreeing
        // about the same level. Both must route through the forecast's own
        // moments, so the numbers are bit-identical rather than merely close.
        let prices: Vec<f64> = (0..400)
            .map(|i| {
                let i = i as f64;
                100.0 * 1.0004f64.powf(i) * (1.0 + 0.03 * (i * 0.7).sin())
            })
            .collect();
        let history = series(&prices);
        let price = *prices.last().unwrap();
        let f = forecast::forecast("BTC", price, &history);
        let report = dip_report(&f, &history, Some(price * 0.8));
        assert!(!report.zones.is_empty());
        for z in &report.zones {
            let long_h = *report.horizons.last().unwrap();
            let via_forecast = prob_below_forecast(&f, z.price, long_h);
            assert_eq!(z.prob_below_long, via_forecast);
            for &(days, p) in &z.touches {
                assert_eq!(p, touch_probability_forecast(&f, z.price, days));
            }
        }
    }

    #[test]
    fn zones_are_sorted_deduped_and_below_spot() {
        let mut prices = vec![100.0; 300];
        prices.extend((0..100).map(|i| 100.0 + (i as f64 % 20.0)));
        let r = report(&prices, None);
        assert!(!r.zones.is_empty());
        for z in &r.zones {
            assert!(z.price < r.price);
            assert!(z.distance_pct < 0.0);
            assert!(z.price > 0.0);
        }
        for w in r.zones.windows(2) {
            assert!(w[0].price < w[1].price);
            // Merged band guarantees at least 1% separation.
            assert!((w[1].price - w[0].price) / w[0].price >= ZONE_MERGE_BAND - 1e-9);
        }
    }

    fn report_with_verdict(price: f64, verdict: DipVerdict) -> DipReport {
        DipReport {
            symbol: "LINK".to_string(),
            price,
            avg_entry: None,
            zones: Vec::new(),
            verdict,
            reason: String::new(),
            vol_daily_pct: 1.0,
            drift_daily_pct: 0.0,
            rsi14: Some(25.0),
            sma200: None,
            history_days: 100,
            horizons: vec![7, 14, 30, 90],
        }
    }

    /// A level inside the noise band must never be sold to the user as a
    /// "limit bid" — that would be a market order in disguise.
    #[test]
    fn scale_in_action_does_not_dress_a_noise_level_as_a_limit_bid() {
        let r = report_with_verdict(
            8.579,
            DipVerdict::ScaleIn {
                now_pct: 50,
                zone_price: 8.545, // -0.4%: inside the noise band
            },
        );
        let line = r.action_line();
        assert!(!line.contains("limit bid"), "{line}");
        assert!(line.contains("no real dip"), "{line}");
    }

    /// A genuine (>5% down) level must still be named as the limit-bid leg.
    #[test]
    fn scale_in_action_keeps_a_real_dip_as_a_limit_bid() {
        let r = report_with_verdict(
            8.579,
            DipVerdict::ScaleIn {
                now_pct: 50,
                zone_price: 8.0, // -6.7%
            },
        );
        let line = r.action_line();
        assert!(line.contains("limit bid at"), "{line}");
        assert!(!line.contains("no real dip"), "{line}");
    }

    /// Spot already sits on a zone with a real dip far below: the split plan's
    /// bid leg must point at that real dip, not at the zone under our feet.
    #[test]
    fn at_zone_split_bids_on_the_next_real_dip_not_on_spot() {
        let prices: Vec<f64> = (0..400).map(|i| 100.0 * 1.0005f64.powi(i)).collect();
        let h = series(&prices);
        let spot = *prices.last().unwrap();
        let f = forecast::forecast("LINK", spot, &h);
        let mk = |price: f64, dist: f64, p7: f64| DipZone {
            price,
            label: "zone".to_string(),
            distance_pct: dist,
            band_top: price,
            touches: vec![(7, p7), (30, p7 + 0.1), (90, p7 + 0.2)],
            prob_below_long: 0.2,
        };
        let at = mk(spot * 0.995, -0.5, 0.2);
        let deep = mk(spot * 0.80, -20.0, 0.10);
        let zones = vec![deep, at];
        let (verdict, _) = decide(spot, None, &zones, Some(20.0), None, &f, false);
        match verdict {
            DipVerdict::ScaleIn { zone_price, .. } => assert!(
                (zone_price - spot * 0.80).abs() < 1e-6,
                "bid leg points at {zone_price}, spot {spot}"
            ),
            other => panic!("unexpected verdict {other:?}"),
        }
    }

    #[test]
    fn above_average_never_bids_on_a_sub_five_percent_dip() {
        // Choppy series whose only zone below spot is ~3% down. That is noise:
        // a limit order there is a market order that also drags the average up,
        // so the verdict must not be a split buy.
        let mut prices = Vec::new();
        for i in 0..400 {
            let base = 100.0 + (i as f64 * 0.02);
            prices.push(if i % 7 == 0 { base * 0.97 } else { base });
        }
        let spot = *prices.last().unwrap();
        let r = report(&prices, Some(spot * 0.8));
        if let DipVerdict::ScaleIn { zone_price, .. } = r.verdict {
            panic!(
                "shallow bid at {zone_price} vs spot {spot} ({:+.2}%)",
                (zone_price / spot - 1.0) * 100.0
            );
        }
    }

    #[test]
    fn above_average_oversold_at_support_takes_a_starter_tranche() {
        // Above the average but deeply oversold at trend support: `/forecast`
        // calls this a small starter tranche, so `/dip` must agree — even when
        // no bounce has started yet. A bounce already underway only sizes the
        // same starter leg up.
        let prices: Vec<f64> = (0..400).map(|i| 100.0 * 1.0005f64.powi(i)).collect();
        let h = series(&prices);
        let spot = *prices.last().unwrap();
        let f = forecast::forecast("BTC", spot, &h);
        let zone = DipZone {
            price: spot * 0.92,
            label: "90d low".to_string(),
            distance_pct: -8.0,
            band_top: spot * 0.92,
            touches: vec![(7, 0.10), (30, 0.20), (90, 0.30)],
            prob_below_long: 0.20,
        };
        let zones = vec![zone];
        let (verdict, reason) = decide(
            spot,
            Some(spot * 0.9),
            &zones,
            Some(28.0),
            Some(spot * 1.01),
            &f,
            false,
        );
        assert!(
            matches!(verdict, DipVerdict::ScaleIn { now_pct: 15, .. }),
            "verdict = {verdict:?}"
        );
        assert!(reason.contains("trend support"), "reason = {reason}");
        assert!(!reason.contains("bounce underway"), "reason = {reason}");

        let (verdict, reason) = decide(
            spot,
            Some(spot * 0.9),
            &zones,
            Some(28.0),
            Some(spot * 1.01),
            &f,
            true,
        );
        assert!(
            matches!(verdict, DipVerdict::ScaleIn { now_pct: 25, .. }),
            "verdict = {verdict:?}"
        );
        assert!(reason.contains("bounce underway"), "reason = {reason}");
    }

    #[test]
    fn hold_names_the_shallow_level_instead_of_calling_a_far_zone_nearest() {
        // A 3% level and a 10% level both sit below spot. Only the deep one can
        // carry a limit order, but the text must not call the far level
        // "nearest" while its own ladder prints the shallow one.
        let prices: Vec<f64> = (0..400).map(|i| 100.0 * 1.001f64.powi(i)).collect();
        let h = series(&prices);
        let spot = *prices.last().unwrap();
        let f = forecast::forecast("BTC", spot, &h);
        let zone = |pct: f64, label: &str| DipZone {
            price: spot * (1.0 + pct / 100.0),
            label: label.to_string(),
            distance_pct: pct,
            band_top: spot * (1.0 + pct / 100.0),
            touches: vec![(7, 0.10), (30, 0.20), (90, 0.30)],
            prob_below_long: 0.20,
        };
        let zones = vec![zone(-10.0, "90d low"), zone(-3.0, "SMA200")];
        let (verdict, reason) = decide(
            spot,
            Some(spot * 0.5),
            &zones,
            Some(50.0),
            Some(spot * 0.97),
            &f,
            false,
        );
        assert!(
            matches!(verdict, DipVerdict::AboveAverage { zone_price: Some(z) } if (z - spot * 0.90).abs() < 1e-6),
            "verdict = {verdict:?}"
        );
        assert!(reason.contains("nearest dip"), "reason = {reason}");
        assert!(reason.contains("SMA200"), "reason = {reason}");
        assert!(reason.contains("noise band"), "reason = {reason}");

        // Same setup without the deep level: nothing to bid on, and the text
        // must say the only level is noise rather than promise a dip.
        let shallow_only = vec![zone(-3.0, "SMA200")];
        let (verdict, reason) = decide(
            spot,
            Some(spot * 0.5),
            &shallow_only,
            Some(50.0),
            Some(spot * 0.97),
            &f,
            false,
        );
        assert!(
            matches!(verdict, DipVerdict::AboveAverage { zone_price: None }),
            "verdict = {verdict:?}"
        );
        assert!(
            reason.contains("only level below spot"),
            "reason = {reason}"
        );
        assert!(reason.contains("noise band"), "reason = {reason}");
    }

    #[test]
    fn no_average_never_limits_in_on_a_sub_five_percent_pullback() {
        // Same shallow pullback without a known average: the CLI path must also
        // refuse to make a 2–4% wiggle the bid leg of a split plan.
        let mut prices = Vec::new();
        for i in 0..400 {
            let base = 100.0 + (i as f64 * 0.02);
            prices.push(base * if i % 2 == 0 { 1.05 } else { 0.95 });
        }
        let spot = *prices.last().unwrap();
        let r = report(&prices, None);
        if let DipVerdict::ScaleIn { zone_price, .. } = r.verdict {
            assert!(
                zone_price <= spot * (1.0 - MIN_SCALE_IN_GAP_PCT / 100.0) + 1e-6
                    || r.reason.contains("sitting at"),
                "shallow bid {zone_price} vs spot {spot}: {}",
                r.reason
            );
        }
    }

    #[test]
    fn above_average_price_still_gets_a_split_plan() {
        // Mild uptrend with a real recurring dip ~6% under spot: the nearest
        // actionable zone is reachable and likely. The verdict must be a
        // two-leg plan, not a blanket "do not add" — the average may never be
        // revisited.
        let mut prices = Vec::new();
        for i in 0..400 {
            let base = 100.0 + (i as f64 * 0.02);
            prices.push(if i % 5 == 0 { base * 0.94 } else { base });
        }
        let spot = *prices.last().unwrap();
        let r = report(&prices, Some(spot * 0.9));
        assert!(r.verdict.is_buy(), "verdict = {:?}", r.verdict);
        assert!(
            matches!(r.verdict, DipVerdict::ScaleIn { .. }),
            "verdict = {:?}",
            r.verdict
        );
        assert!(r.reason.contains("average"));
        // The plan names both legs and the chance the average is not revisited.
        assert!(r.reason.contains("rest"), "reason = {}", r.reason);
        assert!(r.reason.contains("90d"), "reason = {}", r.reason);
    }

    #[test]
    fn above_average_never_bids_on_a_sub_two_percent_dip() {
        // Choppy upward series: the 20-day low sits only ~1% under spot. It is
        // noise, so it must never be shown or used as the bid zone.
        let mut prices = Vec::new();
        for i in 0..400 {
            let base = 100.0 + (i as f64 * 0.02);
            prices.push(base * if i % 2 == 0 { 1.05 } else { 0.95 });
        }
        let spot = *prices.last().unwrap();
        let r = report(&prices, Some(spot * 0.9));
        if let DipVerdict::ScaleIn { zone_price, .. } = r.verdict {
            assert!(
                zone_price <= spot * (1.0 - MIN_DIP_GAP_PCT / 100.0) + 1e-6,
                "bid {zone_price} is too close to spot {spot}"
            );
        }
        for z in r.display_zones() {
            assert!(
                z.distance_pct <= -MIN_DIP_GAP_PCT + 1e-9,
                "shallow zone shown: {} {:+.2}%",
                z.label,
                z.distance_pct
            );
        }
    }

    #[test]
    fn above_average_with_an_unreachable_dip_holds() {
        // Steady uptrend well above a low average: the nearest dip is far away
        // and the model's drift is up, so there is no setup to buy into.
        let prices: Vec<f64> = (0..400).map(|i| 100.0 * 1.002f64.powi(i)).collect();
        let spot = *prices.last().unwrap();
        let r = report(&prices, Some(spot * 0.5));
        assert!(!r.verdict.is_buy(), "verdict = {:?}", r.verdict);
        assert!(
            matches!(r.verdict, DipVerdict::AboveAverage { .. }),
            "verdict = {:?}",
            r.verdict
        );
        assert!(r.reason.contains("average"), "reason = {}", r.reason);
    }

    #[test]
    fn above_average_dip_that_cannot_lower_the_average_says_so() {
        // Steady uptrend well above a low average: the nearest dip is still far
        // above that average, so an add there would not average anything down.
        let prices: Vec<f64> = (0..400).map(|i| 100.0 * 1.002f64.powi(i)).collect();
        let spot = *prices.last().unwrap();
        let r = report(&prices, Some(spot * 0.5));
        assert!(!r.verdict.is_buy());
        assert!(r.reason.contains("average"), "reason = {}", r.reason);
        assert!(
            r.reason.contains("would not lower it") || r.reason.contains("90d"),
            "reason = {}",
            r.reason
        );
    }

    #[test]
    fn buys_when_below_average_and_further_dip_unlikely() {
        // Strong uptrend: spot below entry is a discount and the drift is up,
        // so a deeper dip is improbable.
        let prices: Vec<f64> = (0..400).map(|i| 100.0 * 1.002f64.powi(i)).collect();
        let spot = *prices.last().unwrap();
        let r = report(&prices, Some(spot * 1.05));
        assert!(r.verdict.is_buy(), "verdict = {:?}", r.verdict);
    }

    #[test]
    fn scales_in_when_next_zone_is_likely() {
        // Choppy, high-volatility series → a nearby zone is likely touched.
        let mut prices = Vec::new();
        for i in 0..400 {
            let base = 100.0 + (i as f64 * 0.02);
            prices.push(base * if i % 2 == 0 { 1.05 } else { 0.95 });
        }
        let spot = *prices.last().unwrap();
        let r = report(&prices, Some(spot * 2.0));
        assert!(r.verdict.is_buy() || matches!(r.verdict, DipVerdict::Wait { .. }));
    }

    #[test]
    fn price_sitting_on_a_zone_never_waits_for_a_tiny_dip() {
        // A sub-1% gap must be treated as "already at the zone", not as a dip
        // worth waiting for.
        let mut prices = vec![100.0; 300];
        prices.extend([99.0, 99.2, 99.6]);
        let below_avg = report(&prices, Some(110.0));
        assert_eq!(below_avg.verdict, DipVerdict::BuyNow);
        assert!(!below_avg.reason.starts_with("wait for"));
        let no_avg = report(&prices, None);
        assert!(
            !matches!(no_avg.verdict, DipVerdict::Wait { .. }),
            "verdict = {:?} reason = {}",
            no_avg.verdict,
            no_avg.reason
        );
        eprintln!(
            "no_avg verdict={:?} reason={} zones={:?}",
            no_avg.verdict,
            no_avg.reason,
            no_avg
                .zones
                .iter()
                .map(|z| (z.label.clone(), z.price, z.distance_pct))
                .collect::<Vec<_>>()
        );
        assert!(
            no_avg.reason.contains("sitting at"),
            "verdict = {:?} reason = {}",
            no_avg.verdict,
            no_avg.reason
        );
    }

    #[test]
    fn html_keeps_table_in_pre_and_has_a_bottom_line() {
        let prices: Vec<f64> = (0..400).map(|i| 100.0 + (i as f64 % 30.0)).collect();
        let r = report(&prices, Some(130.0));
        let html = r.render_html();
        assert!(html.contains("BOTTOM LINE"));
        assert_eq!(html.matches("<pre>").count(), 1);
        assert_eq!(html.matches("</pre>").count(), 1);
        assert!(html.contains("P(touch within)"));
        let plain = r.plain();
        assert!(plain.contains("Touch probability"));
        assert!(!r.summary_html().is_empty());
    }

    #[test]
    fn empty_history_does_not_panic() {
        let f = forecast::forecast("BTC", 100.0, &[]);
        let r = dip_report(&f, &[], None);
        assert_eq!(r.horizons, vec![7]);
        assert!(!r.render_html().is_empty());
    }

    // ------------------------------------------------------------------
    // Wide validation over a multi-thousand synthetic-sample grid.
    //
    // These tests exist to make the math "safe by construction": every
    // invariant the bot's probabilities rely on is checked on thousands of
    // market shapes (bull, crash, chop, high-vol, dead-flat) and at both
    // short (1–14d) and far (90–180d) horizons. Any single regression in the
    // first-passage / cone / calibration code fails at least one of these.
    // ------------------------------------------------------------------

    /// Deterministic xorshift-ish RNG, so the grid is reproducible without a
    /// dependency and costs almost nothing.
    struct Lcg(u64);
    impl Lcg {
        fn next_u01(&mut self) -> f64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            // 53-bit mantissa -> [0,1)
            ((x >> 11) as f64) / ((1u64 << 53) as f64)
        }
        /// Approximate standard normal via the 12-uniform CLT trick.
        fn gauss(&mut self) -> f64 {
            let mut s = 0.0;
            for _ in 0..12 {
                s += self.next_u01();
            }
            s - 6.0
        }
    }

    /// Build a synthetic close series for one market regime.
    ///
    /// `regime` 0 bull · 1 bear/crash · 2 chop · 3 high-vol · 4 dead-flat ·
    /// 5 recent-six-months (180 points). All are positive, finite prices.
    fn synth_series(seed: u64, regime: u8) -> Vec<(i64, f64)> {
        let len = match regime {
            5 => 180, // ~6 months: the segment the user cares most about
            3 => 300,
            _ => 240,
        };
        let (mu, sigma) = match regime {
            0 => (0.0015, 0.02),   // steady bull
            1 => (-0.0035, 0.045), // crash / strong downtrend
            2 => (0.0, 0.018),     // chop: no drift
            3 => (0.0005, 0.075),  // high volatility
            4 => (0.0, 0.0015),    // dead-flat, near-zero vol (floor path)
            _ => (0.0, 0.022),     // recent six months, mixed
        };
        let mut rng = Lcg(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1));
        let mut ts = 1_600_000_000_000i64;
        let mut price = 100.0_f64;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            let r = mu + sigma * rng.gauss();
            price *= r.exp();
            // Keep prices strictly positive and sane.
            price = price.clamp(1e-3, 1e7);
            out.push((ts, price));
            ts += 86_400_000;
        }
        out
    }

    #[test]
    fn wide_math_invariants_over_thousands_of_series() {
        let horizons = [1i64, 7, 14, 30, 90, 180];
        let depths = [-1.0f64, -2.0, -5.0, -10.0, -20.0, -35.0, -50.0];
        let mut series = 0usize;
        let mut pairs = 0usize;
        let mut cone_checks = 0usize;

        for symbol in ["BTC", "ETH", "SOL", "LINK"] {
            for seed in 0..400u64 {
                let regime = (seed % 6) as u8;
                let hist = synth_series(seed, regime);
                let spot = hist.last().unwrap().1;
                let f = forecast::forecast(symbol, spot, &hist);

                // --- cone / terminal invariants -----------------------------
                for hz in &f.horizons {
                    assert!(
                        hz.target.is_finite()
                            && hz.low.is_finite()
                            && hz.high.is_finite()
                            && hz.target > 0.0
                            && hz.low > 0.0
                            && hz.high > 0.0,
                        "{symbol} seed {seed} regime {regime} {}d: non-finite cone",
                        hz.days
                    );
                    assert!(
                        hz.low <= hz.target && hz.target <= hz.high,
                        "{symbol} seed {seed} {}d: target outside cone [{} {} {}",
                        hz.days,
                        hz.low,
                        hz.target,
                        hz.high
                    );
                    assert!((0.0..=1.0).contains(&hz.prob_up) && hz.prob_up.is_finite());
                    assert!((0.0..=1.0).contains(&hz.confidence) && hz.confidence.is_finite());
                    // change_pct must agree with target vs spot.
                    let implied = (hz.target / f.price - 1.0) * 100.0;
                    assert!((hz.change_pct - implied).abs() < 1e-9);

                    // A Student-t cone is exact by construction: the horizon
                    // close falls below the lower bound ~15.87% of the time and
                    // below the upper bound ~84.13%.
                    if hz.low < f.price {
                        let p = prob_below_forecast(&f, hz.low, hz.days);
                        assert!(
                            (p - 0.1587).abs() < 6e-3,
                            "{symbol} seed {seed} r{regime} {}d cone-low P={p}",
                            hz.days
                        );
                        cone_checks += 1;
                    }
                    if hz.high > f.price {
                        let p = prob_below_forecast(&f, hz.high, hz.days);
                        assert!(
                            (p - 0.8413).abs() < 6e-3,
                            "{symbol} seed {seed} r{regime} {}d cone-high P={p}",
                            hz.days
                        );
                        cone_checks += 1;
                    }
                }

                // --- touch monotonicity & bounds at every depth -------------
                for &depth in &depths {
                    let level = spot * (1.0 + depth / 100.0);
                    let mut prev = -1.0_f64;
                    for &d in &horizons {
                        let p = touch_probability_forecast(&f, level, d);
                        assert!(
                            p.is_finite() && (0.0..=1.0).contains(&p),
                            "{symbol} seed {seed} r{regime} {depth}% {d}d: P={p}"
                        );
                        assert!(
                            p + 1e-9 >= prev,
                            "{symbol} seed {seed} r{regime} {depth}%: non-monotone {prev} -> {p} at {d}d"
                        );
                        // Touch is a superset of closing below under the same
                        // pure-diffusion variance the touch path uses.
                        let m = f.moments(d);
                        let sh = m.sigma_daily * (d.max(1) as f64).sqrt();
                        let floor = prob_below_gaussian(f.price, level, m.log_drift, sh);
                        assert!(
                            p + 1e-9 >= floor,
                            "{symbol} seed {seed} r{regime} {depth}% {d}d: touch {p} < close-below {floor}"
                        );
                        prev = p;
                        pairs += 1;
                    }
                }

                // --- deeper levels are strictly harder at a fixed horizon ---
                let d = 30;
                let near = touch_probability_forecast(&f, spot * 0.95, d);
                let far = touch_probability_forecast(&f, spot * 0.70, d);
                assert!(
                    near + 1e-12 >= far,
                    "{symbol} seed {seed} r{regime}: near {near} < far {far}"
                );

                // --- dip table: zones sorted, bounded, monotone -------------
                let report = dip_report(&f, &hist, Some(spot * 0.85));
                let mut last = 0.0_f64;
                for z in &report.zones {
                    assert!(z.price > 0.0 && z.price < spot, "zone price out of range");
                    assert!(
                        z.price > last,
                        "{symbol} seed {seed}: zones not strictly ascending"
                    );
                    last = z.price;
                    for w in z.touches.windows(2) {
                        assert!(
                            w[1].1 + 1e-12 >= w[0].1,
                            "{symbol} seed {seed} zone {}: {:?}",
                            z.price,
                            z.touches
                        );
                    }
                    for (_, p) in &z.touches {
                        assert!(p.is_finite() && (0.0..=1.0).contains(p));
                    }
                }

                // --- direction honesty --------------------------------------
                if !f.direction_has_edge() {
                    for hz in &f.horizons {
                        assert!(
                            (hz.prob_up - 0.5).abs() < 1e-6,
                            "{symbol} seed {seed}: no edge but P(up)={}",
                            hz.prob_up
                        );
                    }
                }

                series += 1;
            }
        }

        assert_eq!(series, 1600, "expected 1600 synthetic series");
        assert!(pairs >= 60_000, "grid too small: {pairs} horizon pairs");
        assert!(cone_checks >= 5_000, "cone grid too small: {cone_checks}");
    }

    /// Stress the *guarded* public entry points with hostile inputs. Every one
    /// must return a finite value in `[0, 1]` and never panic.
    #[test]
    fn hostile_inputs_never_panic_or_escape_unit_interval() {
        let bad = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -1.0,
            -1e9,
            1e300,
        ];
        let good = [1.0_f64, 50.0, 100.0, 1e6];
        let all: Vec<f64> = bad.iter().chain(good.iter()).copied().collect();
        for &price in &all {
            for &level in &all {
                for &mu in &all {
                    for &sigma in &all {
                        for &days in &[0i64, 1, 7, 90, 180, 100_000] {
                            let p = touch_probability(price, level, mu, sigma, days);
                            assert!(
                                p.is_finite() && (0.0..=1.0).contains(&p),
                                "touch_probability({price},{level},{mu},{sigma},{days}) = {p}"
                            );
                            let q = prob_below_at_horizon(price, level, mu, sigma, days);
                            assert!(
                                q.is_finite() && (0.0..=1.0).contains(&q),
                                "prob_below_at_horizon({price},{level},{mu},{sigma},{days}) = {q}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A forecast built from a pathological history must still produce a
    /// monotone, bounded dip table (or an empty one) rather than panicking.
    #[test]
    fn pathological_histories_yield_safe_reports() {
        let shapes: Vec<Vec<f64>> = vec![
            vec![100.0],                           // one point
            vec![100.0, 100.0, 100.0, 100.0],      // dead flat
            vec![100.0, 50.0, 200.0, 25.0, 400.0], // wild swings
            (0..500)
                .map(|i| 100.0 * (i as f64 * 0.3).sin().abs() + 1.0)
                .collect(),
            (0..300)
                .map(|i| 1000.0 * 0.5f64.powf(i as f64 / 30.0))
                .collect(), // decay
        ];
        for prices in shapes {
            let hist = series(&prices);
            let spot = *prices.last().unwrap();
            let f = forecast::forecast("ETH", spot, &hist);
            let r = dip_report(&f, &hist, Some(spot));
            assert!(!r.render_html().is_empty());
            for z in &r.zones {
                for w in z.touches.windows(2) {
                    assert!(w[1].1 + 1e-12 >= w[0].1, "non-monotone {:?}", z.touches);
                }
                for (d, p) in &z.touches {
                    assert!(
                        *d > 0 && p.is_finite() && (0.0..=1.0).contains(p),
                        "bad touch {d}d = {p}"
                    );
                }
            }
        }
    }

    /// The exact refutation of the reported bug: a level 8.834 below a 10.x
    /// spot must not have a *lower* P(touch) over a longer window. This is the
    /// minimal single-case guard that the wide grid generalises.
    #[test]
    fn regression_7d_91_percent_vs_14d_80_percent_is_impossible() {
        // LINK-like series around the values from the incident.
        let prices: Vec<f64> = (0..240)
            .map(|i| {
                let i = i as f64;
                10.0 * (1.0 + 0.05 * (i * 0.13).sin()) * 1.0002f64.powf(i)
            })
            .collect();
        let hist = series(&prices);
        let spot = *prices.last().unwrap();
        let f = forecast::forecast("LINK", spot, &hist);
        let level = spot * 0.92;
        let p7 = touch_probability_forecast(&f, level, 7);
        let p14 = touch_probability_forecast(&f, level, 14);
        let p30 = touch_probability_forecast(&f, level, 30);
        let p90 = touch_probability_forecast(&f, level, 90);
        assert!(p7 <= p14 + 1e-12, "{p7} > {p14}");
        assert!(p14 <= p30 + 1e-12, "{p14} > {p30}");
        assert!(p30 <= p90 + 1e-12, "{p30} > {p90}");
    }
}
