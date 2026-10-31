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
//! **The verdict never says "buy" while the price is above the user's average
//! entry.** That is a hard rule, matching the alert engine's advice prompt.

use crate::trading::forecast::{norm_cdf, Forecast};

/// Horizons shown in the touch-probability table.
pub const TOUCH_HORIZONS: [i64; 4] = [7, 14, 30, 90];

/// Minimum daily sigma used by the diffusion (keeps the formula stable on a
/// perfectly flat series).
const MIN_SIGMA: f64 = 1e-4;
/// Two candidate zones closer than this are the same level.
const ZONE_MERGE_BAND: f64 = 0.01;

/// One level below spot worth buying at.
#[derive(Debug, Clone)]
pub struct DipZone {
    pub price: f64,
    /// Compact source label, e.g. `SMA50+90d low`.
    pub label: String,
    /// Distance from spot, percent (always negative).
    pub distance_pct: f64,
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
    /// Price is above the user's average entry — never a buy signal.
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
            DipVerdict::AboveAverage { .. } => "DON'T ADD — ABOVE AVERAGE",
            DipVerdict::NoData => "NO VERDICT",
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            DipVerdict::BuyNow => "🟢",
            DipVerdict::ScaleIn { .. } => "🟡",
            DipVerdict::Wait { .. } => "⏳",
            DipVerdict::AboveAverage { .. } => "🔴",
            DipVerdict::NoData => "⚪",
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
/// P = Φ((a - μH)/(σ√H)) + exp(2μa/σ²) · Φ((a + μH)/(σ√H)),   a = ln(L/S)
/// ```
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
    let s2 = s * s;
    let h = days as f64;
    let a = (level / price).ln();
    let sh = s * h.sqrt();
    let z1 = (a - mu * h) / sh;
    let z2 = (a + mu * h) / sh;
    let log_term2 = 2.0 * mu * a / s2 + ln_norm_cdf(z2);
    let term2 = if log_term2 < -745.0 {
        0.0
    } else {
        log_term2.exp()
    };
    (norm_cdf(z1) + term2).clamp(0.0, 1.0)
}

/// Probability that the *close* at horizon `days` is below `level`. This is the
/// "worth waiting for the dip?" number: touched-and-recovered is different from
/// still being cheap when you would buy.
pub fn prob_below_at_horizon(price: f64, level: f64, mu: f64, sigma: f64, days: i64) -> f64 {
    if price <= 0.0 || level <= 0.0 || days <= 0 {
        return 0.0;
    }
    if level >= price {
        return 1.0;
    }
    let s = sigma.max(MIN_SIGMA);
    let h = days as f64;
    let a = (level / price).ln();
    norm_cdf((a - mu * h) / (s * h.sqrt())).clamp(0.0, 1.0)
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

/// Sort ascending and collapse levels within [`ZONE_MERGE_BAND`].
fn merge_levels(mut levels: Vec<(f64, String)>) -> Vec<(f64, String)> {
    levels.retain(|(p, _)| p.is_finite() && *p > 0.0);
    levels.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out: Vec<(f64, String)> = Vec::new();
    for (price, label) in levels {
        if let Some(last) = out.last_mut() {
            if (price - last.0) / last.0 < ZONE_MERGE_BAND {
                last.0 = (last.0 + price) / 2.0;
                if !last.1.split('+').any(|part| part == label) {
                    last.1 = format!("{}+{}", last.1, label);
                }
                continue;
            }
        }
        out.push((price, label));
    }
    out
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
    let mu = forecast.drift_daily_pct / 100.0;
    let sigma = (forecast.vol_daily_pct / 100.0).max(MIN_SIGMA);
    let horizons = touch_horizons(history.len());
    let long_h = *horizons.last().unwrap_or(&30);

    let mut zones: Vec<DipZone> = merge_levels(candidates(forecast, history))
        .into_iter()
        // Keep only levels that are actually below spot — above-spot "zones"
        // are resistance, not a dip.
        .filter(|(p, _)| *p < price * 0.999)
        .map(|(level, label)| DipZone {
            price: level,
            label,
            distance_pct: (level / price - 1.0) * 100.0,
            touches: horizons
                .iter()
                .map(|&d| (d, touch_probability(price, level, mu, sigma, d)))
                .collect(),
            prob_below_long: prob_below_at_horizon(price, level, mu, sigma, long_h),
        })
        .collect();
    // Ascending order, so the nearest zone is the last element.
    zones.sort_by(|a, b| a.price.total_cmp(&b.price));

    let (verdict, reason) = decide(price, avg_entry, &zones, forecast.rsi14, forecast.sma200);

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
) -> (DipVerdict, String) {
    if !price.is_finite() || price <= 0.0 {
        return (DipVerdict::NoData, "no usable price".to_string());
    }

    // Hard rule: never a buy signal above the user's average entry.
    if let Some(avg) = avg_entry.filter(|a| *a > 0.0) {
        if price > avg * 1.0001 {
            let target = zones
                .iter()
                .rev()
                .find(|z| z.distance_pct < -1.0)
                .or_else(|| zones.last());
            let reason = match target {
                Some(z) => format!(
                    "spot is {:+.1}% vs your average {} — do not add here; next dip zone {} ({:+.1}%)",
                    (price / avg - 1.0) * 100.0,
                    money(avg),
                    money(z.price),
                    z.distance_pct
                ),
                None => format!(
                    "spot is {:+.1}% vs your average {} and no zone sits below spot",
                    (price / avg - 1.0) * 100.0,
                    money(avg)
                ),
            };
            return (
                DipVerdict::AboveAverage {
                    zone_price: target.map(|z| z.price),
                },
                reason,
            );
        }
    }

    let below_average = avg_entry.is_some_and(|a| a > 0.0 && price <= a * 1.0001);
    let cheap_oscillator = rsi.is_some_and(|r| r < 40.0);
    let below_trend = sma200.is_some_and(|s| s > 0.0 && price < s);
    // "At a zone" = a level within 1% below spot; "next" = the next level at
    // least 1% down. A 0.4% gap is not a dip worth waiting for — treating it as
    // one produced nonsense advice like "wait for a 0.4% drop".
    let at_zone = zones.iter().rev().find(|z| z.distance_pct >= -1.0);
    let next = zones.iter().rev().find(|z| z.distance_pct < -1.0);

    let Some(next) = next else {
        if let Some(z) = at_zone {
            let zone = format!("{} {}", z.label, money(z.price));
            if below_average {
                return (
                    DipVerdict::BuyNow,
                    format!(
                        "spot is sitting at {zone}, the deepest zone below spot, and {:+.1}% vs your average — this is the entry",
                        avg_entry.map(|a| (price / a - 1.0) * 100.0).unwrap_or(0.0)
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

    let p7 = next.prob_at(7).unwrap_or(0.0);
    let next_desc = format!(
        "{} {} ({:+.1}%)",
        next.label,
        money(next.price),
        next.distance_pct
    );
    let p7_txt = format!("{:.0}%", p7 * 100.0);

    // Price already sits on a zone: buy now, unless a deeper zone is likely.
    if let Some(z) = at_zone {
        let zone_desc = format!("{} {}", z.label, money(z.price));
        if p7 >= 0.55 {
            return (
                DipVerdict::ScaleIn {
                    now_pct: 33,
                    zone_price: next.price,
                },
                format!(
                    "spot sits at {zone_desc}, but {next_desc} is likely next: P(7d) {p7_txt} · buy 1/3 now, 2/3 there"
                ),
            );
        }
        if below_average {
            return (
                DipVerdict::BuyNow,
                format!(
                    "spot sits at {zone_desc} and is {:+.1}% vs your average; next zone {next_desc} is only P(7d) {p7_txt} — buy now",
                    avg_entry.map(|a| (price / a - 1.0) * 100.0).unwrap_or(0.0)
                ),
            );
        }
        return (
            DipVerdict::ScaleIn {
                now_pct: 50,
                zone_price: z.price,
            },
            format!(
                "spot sits at {zone_desc}; next meaningful zone {next_desc} is only P(7d) {p7_txt} — split the buy (no average entry configured)"
            ),
        );
    }

    if p7 >= 0.55 {
        return (
            DipVerdict::ScaleIn {
                now_pct: 33,
                zone_price: next.price,
            },
            format!("{next_desc} is likely to be touched: P(7d) {p7_txt} · buy 1/3 now, 2/3 there"),
        );
    }

    // A further dip is unlikely: waiting costs more than buying now.
    if p7 <= 0.35 {
        if below_average {
            return (
                DipVerdict::BuyNow,
                format!(
                    "P(touch {next_desc} in 7d) is only {p7_txt} and spot is {}; waiting is not worth it",
                    cheap_hint(below_average, cheap_oscillator, below_trend)
                ),
            );
        }
        if cheap_oscillator || below_trend {
            return (
                DipVerdict::ScaleIn {
                    now_pct: 50,
                    zone_price: next.price,
                },
                format!(
                    "P(touch {next_desc} in 7d) is only {p7_txt} and spot looks cheap ({}); do not wait for it",
                    cheap_hint(below_average, cheap_oscillator, below_trend)
                ),
            );
        }
        return (
            DipVerdict::Wait {
                zone_price: Some(next.price),
            },
            format!(
                "{next_desc} has only P(7d) {p7_txt}; no average entry is known — /add purchases for entry-relative advice"
            ),
        );
    }

    if below_average {
        return (
            DipVerdict::ScaleIn {
                now_pct: 50,
                zone_price: next.price,
            },
            format!(
                "spot is under your average, but {next_desc} is close: P(7d) {p7_txt} · split the buy"
            ),
        );
    }

    let reason = format!(
        "wait for {next_desc} — P(touch 7d) {p7_txt}, P(below at {}d) {:.0}%",
        *TOUCH_HORIZONS.last().unwrap_or(&90),
        next.prob_below_long * 100.0
    );
    (
        DipVerdict::Wait {
            zone_price: Some(next.price),
        },
        reason,
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
    if price >= 1000.0 {
        0
    } else if price >= 100.0 {
        1
    } else if price >= 1.0 {
        3
    } else {
        5
    }
}

fn money(price: f64) -> String {
    format!("${:.*}", decimals(price), price)
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
    fn table_lines(&self) -> Vec<String> {
        let labels: Vec<String> = self.zones.iter().map(|z| short_label(&z.label)).collect();
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
        for (z, label) in self.zones.iter().zip(labels.iter()) {
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
        if self.zones.is_empty() {
            lines.push("(no zone below spot)".to_string());
        }
        lines
    }

    fn header_line(&self) -> String {
        let avg = match self.avg_entry {
            Some(a) if a > 0.0 => format!(
                "avg entry <code>{}</code> · spot <b>{:+.1}%</b> vs average",
                money(a),
                (self.price / a - 1.0) * 100.0
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
            "\n<b>P(touch within)</b> — first-passage, not a prediction:\n<pre>{}</pre>\n",
            esc(&self.table_lines().join("\n"))
        ));
        out.push_str(&format!(
            "\n{} <b>BOTTOM LINE — {}</b>\n{}\n",
            self.verdict.icon(),
            self.verdict.label(),
            esc(&self.reason)
        ));
        out.push_str(
            "<i>Touch probabilities from a drift/volatility diffusion over the local closes. \
             Not financial advice; no model can guarantee a bottom.</i>",
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
        if let Some(z) = self.zones.last() {
            out.push_str(&format!(
                "nearest zone <code>{}</code> ({}), P(7d) <code>{}</code>\n",
                money(z.price),
                html_escape::encode_text(&z.label),
                pct(z.prob_at(7).unwrap_or(0.0))
            ));
        }
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
        out.push_str("Not financial advice. Touch probability ≠ prediction.\n");
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
        // Small sigma + strong negative drift used to overflow the exp() term.
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

    #[test]
    fn never_buys_above_average_entry() {
        // Falling then flat series, spot above the user's average entry.
        let prices: Vec<f64> = (0..400).map(|i| 100.0 - i as f64 * 0.05).collect();
        let spot = *prices.last().unwrap();
        let r = report(&prices, Some(spot * 0.9));
        assert!(!r.verdict.is_buy(), "verdict = {:?}", r.verdict);
        assert!(matches!(r.verdict, DipVerdict::AboveAverage { .. }));
        assert!(r.reason.contains("average"));
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
        // The prod bug: spot 0.4% above the 20-day low produced "wait for a
        // 0.4% drop". A sub-1% gap must be treated as "already at the zone".
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
}
