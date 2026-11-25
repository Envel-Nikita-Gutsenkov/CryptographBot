//! "Hardcore Dip Sniper" — long-horizon accumulation on left-tail anomalies.
//!
//! The bot already answers "is this a dip?" ([`crate::trading::dipzones`]). This
//! module answers a different, stricter question: **where do I park limit orders
//! so that I only buy when the market is statistically panicking**, and what
//! does each fill do to my average entry?
//!
//! Everything here is deterministic and pure:
//! * one drawdown anchor (rolling 14-day high) so a slow bleed does not drag the
//!   "dip" down with it;
//! * three martingale buckets whose limit prices are *solved* so their
//!   first-passage touch probability over [`TRAP_HORIZON_DAYS`] equals a fixed
//!   target (60% / 35% / 18%) — the number printed is the real market chance of
//!   the level filling, not a hand-picked discount;
//! * a first-passage touch probability per bucket, using the *same* effective
//!   sigma as the drawn cone (`σ·t68`);
//! * an average-entry guard that flags any fill which would dilute a profitable
//!   position and shifts the cash pyramid deeper;
//! * a 0–100 capitulation score for "buy at market right now".
//!
//! No network, no clock, no randomness — so the whole thing is unit-tested.

use crate::models::onchain::{DerivativesRead, DerivativesSignal, OnChainMetrics};
use crate::trading::{dipzones::touch_probability, forecast::{cone_sigma_h, Forecast}};

/// Horizon over which a limit "trap" is expected to fill. Two weeks is a
/// realistic window for a resting buy limit; at the same touch probability a
/// shorter horizon yields a shallower, more tradeable level than 30/90 days.
pub const TRAP_HORIZON_DAYS: i64 = 14;
/// Target first-passage touch probabilities for the three buckets, shallow →
/// deep. The limit *price* is solved for each probability via
/// [`level_for_touch_target`], so a printed "60% / 35% / 18%" is the actual
/// model chance of the level being touched inside [`TRAP_HORIZON_DAYS`] — the
/// label and the number agree by construction, not by a bot's whim.
pub const TRAP_TARGET_TOUCH: [f64; 3] = [0.60, 0.35, 0.18];
/// Martingale pyramid: deeper blood gets more cash.
pub const BUCKET_WEIGHTS: [f64; 3] = [0.20, 0.35, 0.45];
/// Same pyramid when protecting an average entry: the shallow bucket (the one
/// most likely to sit above the average) is starved, the deep ones get more.
pub const PROTECT_WEIGHTS: [f64; 3] = [0.10, 0.35, 0.55];
/// A fill that lifts the average entry by more than this is called out.
pub const GUARD_DILUTION: f64 = 0.10;
/// "Strongly in profit" threshold that switches the avg-entry guard on.
pub const GUARD_PROFIT: f64 = 0.25;

/// Bucket labels ordered shallowest → deepest. The first bucket is a mild dip
/// (highest touch probability), the last is a tail "swan" event (lowest
/// probability, deepest limit) — matching the TRAP_TARGET_TOUCH ordering.
pub const BUCKET_LABELS: [&str; 3] = ["dip", "cascade", "swan"];
pub const DAY_MS: i64 = 86_400_000;

/// One aggregated daily candle. `open/high/low` may equal `close` when the
/// provider only gave us closes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candle {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

impl Candle {
    pub fn is_valid(&self) -> bool {
        self.open > 0.0
            && self.close > 0.0
            && self.high > 0.0
            && self.low > 0.0
            && self.high >= self.low
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CapBand {
    Sleep,
    Watch,
    Alert,
    Capitulation,
}

impl CapBand {
    pub fn label(self) -> &'static str {
        match self {
            CapBand::Sleep => "SLEEP",
            CapBand::Watch => "WATCH",
            CapBand::Alert => "ALERT",
            CapBand::Capitulation => "CAPITULATION",
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            CapBand::Sleep => "😴",
            CapBand::Watch => "👀",
            CapBand::Alert => "⚠️",
            CapBand::Capitulation => "🔥",
        }
    }
}

/// Real-time "buy at market now" score. Components that need data we do not
/// have (intraday volume history) are skipped and the max is reduced, so the
/// displayed ratio never lies about completeness.
#[derive(Debug, Clone, Copy)]
pub struct Capitulation {
    pub score: u8,
    pub max_score: u8,
    pub band: CapBand,
    pub z_hit: bool,
    pub rsi_hit: bool,
    pub volume_spike: bool,
    pub long_wick: bool,
    /// Open interest is being flushed on a drop (longs capitulated).
    pub oi_flush: bool,
}

#[derive(Debug, Clone)]
pub struct SniperBucket {
    /// 1-based bucket number.
    pub index: usize,
    pub label: &'static str,
    /// Target first-passage touch probability the level is solved for
    /// (0.60 = shallow / most likely, 0.18 = deep / least likely). Equal to
    /// [`Self::prob_touch`] when the target is reachable within the clamp range.
    pub quantile: f64,
    /// Standardised cone multiple, e.g. -1.27σ.
    pub sigma_mult: f64,
    pub limit_price: f64,
    pub discount_pct: f64,
    pub weight: f64,
    pub spend_usd: f64,
    /// First-passage chance the limit fills within [`TRAP_HORIZON_DAYS`]; by
    /// construction it equals the bucket's [`TRAP_TARGET_TOUCH`] target.
    pub prob_touch: f64,
    pub new_avg_entry: Option<f64>,
    /// The fill would push the average entry up by more than [`GUARD_DILUTION`].
    pub dilutes_avg: bool,
    /// Spot is already at/below the limit.
    pub triggered: bool,
}

#[derive(Debug, Clone)]
pub struct DipSniperReport {
    pub symbol: String,
    pub price: f64,
    pub regime: String,
    pub rsi14: Option<f64>,
    pub vol_daily_pct: f64,
    pub drift_daily_pct: f64,
    pub anchor_price: f64,
    pub z14: f64,
    pub avg_entry: Option<f64>,
    pub position_size: f64,
    pub cash: f64,
    pub protect_avg_entry: bool,
    /// The guard is actually engaged (user asked for it *and* the position is
    /// already in strong profit, so a shallow fill would dilute it).
    pub guard_active: bool,
    pub capitulation: Capitulation,
    /// Futures tape (funding / OI trend / long-short). `None` when unavailable.
    pub derivatives: Option<DerivativesSignal>,
    pub buckets: Vec<SniperBucket>,
    pub history_days: usize,
}

/// Aggregate the intraday rows of the most recent UTC day into one candle.
/// `rows` are `(ts_ms, open, high, low, close)` in ascending order and may carry
/// `None` for legacy rows that predate the OHLCV migration.
pub fn aggregate_last_day(
    rows: &[(i64, Option<f64>, Option<f64>, Option<f64>, f64)],
) -> Option<Candle> {
    let last_ts = rows.last()?.0;
    if last_ts <= 0 {
        return None;
    }
    let day_start = last_ts - last_ts.rem_euclid(DAY_MS);
    let today: Vec<&(i64, Option<f64>, Option<f64>, Option<f64>, f64)> =
        rows.iter().filter(|r| r.0 >= day_start).collect();
    let first = today.first()?;
    let open = first.1.unwrap_or(first.4);
    let close = today.last()?.4;
    let mut high = f64::NEG_INFINITY;
    let mut low = f64::INFINITY;
    for r in &today {
        high = high.max(r.2.unwrap_or(r.4));
        low = low.min(r.3.unwrap_or(r.4));
    }
    let candle = Candle {
        open,
        high,
        low,
        close,
    };
    candle.is_valid().then_some(candle)
}

fn rolling_high(closes: &[f64], window: usize) -> Option<f64> {
    if closes.is_empty() || window == 0 {
        return None;
    }
    let start = closes.len().saturating_sub(window);
    let v = closes[start..]
        .iter()
        .copied()
        .fold(f64::NEG_INFINITY, f64::max);
    v.is_finite().then_some(v)
}

fn sma(values: &[f64], window: usize) -> Option<f64> {
    if values.len() < window || window == 0 {
        return None;
    }
    let slice = &values[values.len() - window..];
    Some(slice.iter().sum::<f64>() / window as f64)
}

fn stddev(values: &[f64], window: usize) -> Option<f64> {
    let mean = sma(values, window)?;
    let slice = &values[values.len() - window..];
    let var = slice.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / window as f64;
    Some(var.sqrt())
}

/// Drawdown Z-score over 14 days: how many 14-day sigmas below the recent peak
/// the price sits, in log space.
pub fn drawdown_z(price: f64, anchor: f64, sigma_daily: f64, days: f64) -> f64 {
    if price <= 0.0 || anchor <= 0.0 || sigma_daily <= 0.0 || days <= 0.0 {
        return 0.0;
    }
    let denom = sigma_daily * days.sqrt();
    if denom <= f64::EPSILON {
        return 0.0;
    }
    ((price / anchor).ln() / denom).clamp(-10.0, 10.0)
}

fn capitulation_score(
    z14: f64,
    rsi14: Option<f64>,
    candle: Option<Candle>,
    volumes: Option<&[f64]>,
    derivatives: Option<&DerivativesSignal>,
) -> Capitulation {
    let mut score: u32 = 0;
    let mut max_score: u32 = 0;

    max_score += 40;
    let z_hit = z14 <= -2.0;
    if z_hit {
        score += 40;
    }

    max_score += 25;
    let rsi_hit = rsi14.map(|r| r < 25.0).unwrap_or(false);
    if rsi_hit {
        score += 25;
    }

    let volume_spike = match volumes.and_then(|v| {
        if v.len() < 20 {
            return None;
        }
        let base = sma(v, 20)?;
        let sd = stddev(v, 20)?;
        Some(v[v.len() - 1] > base + 2.0 * sd)
    }) {
        Some(hit) => {
            max_score += 20;
            if hit {
                score += 20;
            }
            hit
        }
        None => false,
    };

    let oi_flush = match derivatives {
        Some(d) => {
            max_score += 15;
            let hit = d.read == DerivativesRead::LongsFlushed;
            if hit {
                score += 15;
            }
            hit
        }
        None => false,
    };

    let long_wick = match candle.filter(|c| c.is_valid()) {
        Some(c) => {
            max_score += 15;
            let range = c.high - c.low;
            let hit = range > 0.0 && ((c.open.min(c.close) - c.low) / range) > 0.5;
            if hit {
                score += 15;
            }
            hit
        }
        None => false,
    };

    let max_score = max_score.max(1);
    let ratio = score as f64 / max_score as f64;
    let band = if ratio >= 0.80 {
        CapBand::Capitulation
    } else if ratio >= 0.50 {
        CapBand::Alert
    } else if ratio >= 0.25 {
        CapBand::Watch
    } else {
        CapBand::Sleep
    };

    Capitulation {
        score: score.min(100) as u8,
        max_score: max_score.min(100) as u8,
        band,
        z_hit,
        rsi_hit,
        volume_spike,
        long_wick,
        oi_flush,
    }
}

/// Build the full sniper report from the already-computed deterministic
/// forecast (so sigma/drift/RSI/regime use the exact same calibration as
/// `/forecast` and `/dip`).
///
/// Convenience wrapper without the futures tape; production paths go through
/// [`sniper_report_with_derivatives`] directly, so this exists for tests.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub fn sniper_report(
    forecast: &Forecast,
    history: &[(i64, f64)],
    candle: Option<Candle>,
    volumes: Option<&[f64]>,
    avg_entry: Option<f64>,
    position_size: f64,
    cash: f64,
    protect_avg_entry: bool,
) -> DipSniperReport {
    sniper_report_with_derivatives(
        forecast,
        history,
        candle,
        volumes,
        avg_entry,
        position_size,
        cash,
        protect_avg_entry,
        None,
    )
}

/// Same as [`sniper_report`], additionally folding the futures tape into the
/// capitulation score and the rendered verdict.
#[allow(clippy::too_many_arguments)]
pub fn sniper_report_with_derivatives(
    forecast: &Forecast,
    history: &[(i64, f64)],
    candle: Option<Candle>,
    volumes: Option<&[f64]>,
    avg_entry: Option<f64>,
    position_size: f64,
    cash: f64,
    protect_avg_entry: bool,
    onchain: Option<&OnChainMetrics>,
) -> DipSniperReport {
    let price = forecast.price;
    let closes: Vec<f64> = history.iter().map(|&(_, c)| c).collect();
    let sigma = (forecast.vol_daily_pct / 100.0).max(0.003);
    let mu = forecast.drift_daily_pct / 100.0;
    let anchor = rolling_high(&closes, 14).unwrap_or(price);
    let z14 = drawdown_z(price, anchor, sigma, 14.0);

    // 24h price move for the derivatives read (closes are daily, so compare the
    // two most recent values; the date gap is irrelevant for the sign/magnitude).
    let price_change_24h = match closes.as_slice() {
        [.., prev, last] if *prev > 0.0 => (last / prev - 1.0) * 100.0,
        _ => 0.0,
    };
    let derivatives = onchain.and_then(|m| DerivativesSignal::from_metrics(m, price_change_24h));

    let avg = avg_entry.filter(|a| a.is_finite() && *a > 0.0);
    let in_profit = avg
        .map(|a| price > a * (1.0 + GUARD_PROFIT))
        .unwrap_or(false);
    let guard_active = protect_avg_entry && in_profit;
    let weights = if guard_active {
        PROTECT_WEIGHTS
    } else {
        BUCKET_WEIGHTS
    };

    // The cone's own width at 30 days, in log space. Using the same function as
    // Touch probabilities use the model's own daily diffusion. The cone is
    // drawn with the Student-t q68 multiplier, but the first-passage formula
    // below is Gaussian — scaling sigma there too double-counts the fat tail
    // and overstated every level's touch chance.
    let sigma_eff = sigma;
    let cash = cash.max(0.0);
    let position_size = position_size.max(0.0);

    let mut buckets = Vec::with_capacity(3);
    // Each level must sit strictly below the previous one, so we walk the
    // targets shallow → deep and cap the search at the prior limit.
    let mut hi_cap = price * 0.999;
    let sigma_h = cone_sigma_h(sigma, TRAP_HORIZON_DAYS as f64);
    let mu_h_trap = mu * TRAP_HORIZON_DAYS as f64;
    for (i, &target) in TRAP_TARGET_TOUCH.iter().enumerate() {
        let limit_price =
            level_for_touch_target(price, target, mu, sigma_eff, TRAP_HORIZON_DAYS, hi_cap);
        hi_cap = limit_price;
        let weight = weights[i];
        let spend = cash * weight;
        let new_avg = new_avg_entry(avg, position_size, spend, limit_price);
        let dilutes = match (avg, new_avg) {
            (Some(a), Some(n)) => guard_active && n > a * (1.0 + GUARD_DILUTION),
            _ => false,
        };
        // z-score of the solved level within the trap-horizon cone (display only).
        let sigma_mult = if sigma_h > 0.0 {
            ((limit_price / price).ln() - mu_h_trap) / sigma_h
        } else {
            0.0
        };
        buckets.push(SniperBucket {
            index: i + 1,
            label: BUCKET_LABELS[i],
            quantile: target,
            sigma_mult,
            limit_price,
            discount_pct: (limit_price / price - 1.0) * 100.0,
            weight,
            spend_usd: spend,
            prob_touch: touch_probability(price, limit_price, mu, sigma_eff, TRAP_HORIZON_DAYS),
            new_avg_entry: new_avg,
            dilutes_avg: dilutes,
            triggered: price <= limit_price * 1.0001,
        });
    }

    let capitulation =
        capitulation_score(z14, forecast.rsi14, candle, volumes, derivatives.as_ref());

    DipSniperReport {
        symbol: forecast.symbol.clone(),
        price,
        regime: forecast.regime.clone(),
        rsi14: forecast.rsi14,
        vol_daily_pct: forecast.vol_daily_pct,
        drift_daily_pct: forecast.drift_daily_pct,
        anchor_price: anchor,
        z14,
        avg_entry: avg,
        position_size,
        cash,
        protect_avg_entry,
        guard_active,
        capitulation,
        derivatives,
        buckets,
        history_days: history.len(),
    }
}

/// Solve for the limit price whose first-passage touch probability over
/// `days` — under the drift/vol diffusion used by `/dip` and `/forecast` —
/// equals `target`.
///
/// [`touch_probability`] is monotonically increasing in `level` on
/// `(0, price)` (a closer, higher limit is easier to touch) and returns 0 at or
/// above spot. We binary-search the level in `[price*0.25, hi_cap]`, so every
/// bucket carries a *known, honest* probability instead of an arbitrary far-tail
/// quantile (−23% / −35% / −50% style levels the user rightly rejected).
fn level_for_touch_target(
    price: f64,
    target: f64,
    mu: f64,
    sigma: f64,
    days: i64,
    hi_cap: f64,
) -> f64 {
    let hi_cap = hi_cap.min(price * 0.999);
    // Deepest level we are willing to print: −75%. Deeper targets (−98% etc.)
    // are not actionable and look like false precision, so we clamp here.
    let floor = price * 0.25;
    if !(price > 0.0) || !target.is_finite() {
        return hi_cap;
    }
    let mut lo = floor;
    let mut hi = hi_cap.max(floor * 1.0001);
    // Even the closest allowed level cannot reach the target: use the closest.
    if touch_probability(price, hi, mu, sigma, days) < target {
        return hi;
    }
    // Even the deepest allowed level is already more likely than target: floor.
    if touch_probability(price, lo, mu, sigma, days) > target {
        return lo;
    }
    for _ in 0..64 {
        let mid = 0.5 * (lo + hi);
        if touch_probability(price, mid, mu, sigma, days) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// Average price after spending `spend_usd` at `limit_price` on top of an
/// existing `position_size` at `avg_entry`.
pub fn new_avg_entry(
    avg_entry: Option<f64>,
    position_size: f64,
    spend_usd: f64,
    limit_price: f64,
) -> Option<f64> {
    let avg = avg_entry?;
    if spend_usd <= 0.0 || limit_price <= 0.0 {
        return Some(avg);
    }
    let bought = spend_usd / limit_price;
    let total_qty = position_size + bought;
    if total_qty <= 0.0 {
        return Some(avg);
    }
    Some((avg * position_size + spend_usd) / total_qty)
}

pub(crate) fn price_decimals(p: f64) -> usize {
    crate::format::price_decimals(p)
}

pub(crate) fn fmt_money(v: f64, decimals: usize) -> String {
    if !v.is_finite() {
        return "—".into();
    }
    // Group thousands so a limit like `47277` reads as `47,277`, matching the
    // rest of the bot (`/portfolio`, alerts).
    crate::format::format_thousands(v, decimals)
}

fn fmt_prob(p: f64) -> String {
    if !p.is_finite() || p <= 0.0 {
        "0%".into()
    } else if p < 0.005 {
        "<1%".into()
    } else {
        format!("{:.0}%", p * 100.0)
    }
}

fn pct_str(v: f64) -> String {
    if !v.is_finite() {
        "—".into()
    } else {
        format!("{:+.1}%", v)
    }
}

impl DipSniperReport {
    /// The deepest bucket spot has already reached, if any. On a gap-down all
    /// buckets can be triggered; the deepest one is the level that actually
    /// matters for sizing the next tranche.
    pub fn triggered_bucket(&self) -> Option<&SniperBucket> {
        self.buckets.iter().rev().find(|b| b.triggered)
    }

    /// One actionable sentence derived only from the numbers in this report.
    pub fn verdict(&self, russian: bool) -> String {
        let body = self.verdict_body(russian);
        if russian {
            format!("Действие: {body}")
        } else {
            format!("Action: {body}")
        }
    }

    fn verdict_body(&self, russian: bool) -> String {
        let dec = price_decimals(self.price);
        if let Some(b) = self.triggered_bucket() {
            return if russian {
                format!(
                    "Цена уже в зоне ведра {} ({}) — дроби вход, не бери всю сумму одной свечой.",
                    b.index,
                    fmt_money(b.limit_price, dec)
                )
            } else {
                format!(
                    "Price is already inside bucket {} ({}) — scale in, do not spend the whole lot on one candle.",
                    b.index,
                    fmt_money(b.limit_price, dec)
                )
            };
        }
        if self.capitulation.band == CapBand::Capitulation {
            return if russian {
                "Капитуляция по модели: это статистическая аномалия, можно брать по рынку частями."
                    .into()
            } else {
                "Model capitulation: this is a statistical anomaly — buy at market in tranches."
                    .into()
            };
        }
        if self
            .derivatives
            .map(|d| d.read == DerivativesRead::ShortsPiling)
            .unwrap_or(false)
        {
            let b1 = &self.buckets[0];
            return if russian {
                format!(
                    "Падение идёт на новых шортах — это ещё не дно. Не лови нож, держи лимитку {}.",
                    fmt_money(b1.limit_price, dec)
                )
            } else {
                format!(
                    "The drop is driven by fresh shorts — not the bottom yet. Don't catch the knife, keep the bid at {}.",
                    fmt_money(b1.limit_price, dec)
                )
            };
        }
        // No cash means there is nothing to allocate: saying "keep the cash
        // parked" or printing a $0 spend / unchanged "new avg" is meaningless.
        if self.cash <= 0.0 {
            return if russian {
                "Кэш не задан — укажи /sniper cash <сумма>, и я разложу его по вёдрам.".into()
            } else {
                "No cash configured — set /sniper cash <amount> to get bucket sizing.".into()
            };
        }
        match self.avg_entry {
            Some(avg) if self.price > avg * (1.0 + GUARD_PROFIT) => {
                let dist = (self.price / avg - 1.0) * 100.0;
                let b1 = &self.buckets[0];
                let b2 = &self.buckets[1];
                if russian {
                    format!(
                        "Цена высоко ({:+.0}% к средней). Копим кэш, держим лимитки {} и {}.",
                        dist,
                        fmt_money(b1.limit_price, dec),
                        fmt_money(b2.limit_price, dec)
                    )
                } else {
                    format!(
                        "Price is high ({:+.0}% vs average). Keep the cash parked, hold bids at {} and {}.",
                        dist,
                        fmt_money(b1.limit_price, dec),
                        fmt_money(b2.limit_price, dec)
                    )
                }
            }
            _ => {
                let b1 = &self.buckets[0];
                if russian {
                    format!(
                        "Спокойно: ждём первое ведро на {} (шанс задеть за {}д ≈ {:.0}%).",
                        fmt_money(b1.limit_price, dec),
                        TRAP_HORIZON_DAYS,
                        b1.prob_touch * 100.0
                    )
                } else {
                    format!(
                        "Steady: first bucket at {} (touch chance over {}d ≈ {:.0}%).",
                        fmt_money(b1.limit_price, dec),
                        TRAP_HORIZON_DAYS,
                        b1.prob_touch * 100.0
                    )
                }
            }
        }
    }

    /// Compact monospace table (header + one row per bucket).
    pub fn table_lines(&self) -> Vec<String> {
        let dec = price_decimals(self.price);
        // Sub-cent tokens produce limit prices like `0.000002429`, wider than
        // the old fixed columns. Size the price-bearing columns to their widest
        // cell so a cheap coin's table stays aligned instead of colliding.
        let limit_w = self
            .buckets
            .iter()
            .map(|b| fmt_money(b.limit_price, dec).chars().count())
            .max()
            .unwrap_or(0)
            .max("limit".len());
        let avg_w = self
            .buckets
            .iter()
            .filter_map(|b| b.new_avg_entry.map(|v| fmt_money(v, dec).chars().count()))
            .max()
            .unwrap_or(0)
            .max("new avg".len());
        let header = format!(
            "{:<2} {:<8} {:>7} {:>limit_w$} {:>7} {:>6} {:>7} {:>7} {:>avg_w$}",
            "#", "zone", "sig", "limit", "disc", "weight", "spend", "P(t14d)", "new avg"
        );
        let mut out = vec![header];
        for b in &self.buckets {
            let has_cash = b.spend_usd > 0.0;
            let new_avg = if has_cash {
                b.new_avg_entry
                    .map(|v| fmt_money(v, dec))
                    .unwrap_or_else(|| "—".into())
            } else {
                "—".into()
            };
            let mut row = format!(
                "{:<2} {:<8} {:>7} {:>limit_w$} {:>7} {:>6} {:>7} {:>7} {:>avg_w$}",
                b.index,
                b.label,
                format!("{:.1}σ", b.sigma_mult),
                fmt_money(b.limit_price, dec),
                pct_str(b.discount_pct),
                format!("{:.0}%", b.weight * 100.0),
                if has_cash {
                    format!("${:.0}", b.spend_usd)
                } else {
                    "—".into()
                },
                fmt_prob(b.prob_touch),
                new_avg,
            );
            if b.dilutes_avg {
                row.push_str(" ⚠️");
            } else if b.triggered {
                row.push_str(" 🔥");
            }
            out.push(row);
        }
        out
    }

    pub fn render_html(&self, russian: bool) -> String {
        let dec = price_decimals(self.price);
        let cap = &self.capitulation;
        let cash = if self.cash > 0.0 {
            format!("${}", crate::format::format_thousands(self.cash, 0))
        } else {
            "—".to_string()
        };
        let avg = self
            .avg_entry
            .map(|v| fmt_money(v, dec))
            .unwrap_or_else(|| "—".into());
        // Adaptive precision: a 0.00740318 BTC position must not round to
        // `0.01 BTC`, which reads as a different (larger) holding.
        let pos = if self.position_size > 0.0 {
            crate::format::fmt_crypto(self.position_size, &self.symbol)
        } else {
            "—".into()
        };
        let rsi = self
            .rsi14
            .map(|v| format!("{v:.0}"))
            .unwrap_or_else(|| "—".into());
        let guard = if self.guard_active {
            if russian {
                "ON"
            } else {
                "ON"
            }
        } else if self.protect_avg_entry {
            "armed"
        } else {
            "OFF"
        };

        let mut text = format!(
            "🩸 <b>HARD DIP SNIPER</b> · {} · {} USD\n\
             Z(14d) {:+.2}σ · capitulation {}/{} ({}) · RSI14 {} · vol {:.2}%/day\n\
             regime {} · anchor {} · drift {:+.3}%/day\n\
             Cash {} · avg {} · pos {} · guard {}",
            self.symbol,
            fmt_money(self.price, dec),
            self.z14,
            cap.score,
            cap.max_score,
            format!("{} {}", cap.band.label(), cap.band.icon()),
            rsi,
            self.vol_daily_pct,
            self.regime,
            fmt_money(self.anchor_price, dec),
            self.drift_daily_pct,
            cash,
            avg,
            pos,
            guard,
        );

        text.push_str(&format!(
            "\n\n<pre>{}</pre>",
            html_escape::encode_text(&self.table_lines().join("\n"))
        ));

        // Which capitulation components actually fired this scan.
        let mut hits: Vec<&str> = Vec::new();
        if cap.z_hit {
            hits.push("Z14 ≤ -2σ");
        }
        if cap.rsi_hit {
            hits.push("RSI < 25");
        }
        if cap.volume_spike {
            hits.push("volume spike");
        }
        if cap.long_wick {
            hits.push("long lower wick");
        }
        if cap.oi_flush {
            hits.push("OI flush");
        }
        if !hits.is_empty() {
            text.push_str(&format!(
                "\n{} <i>{}</i>",
                if russian {
                    "Сигналы:"
                } else {
                    "Signals:"
                },
                html_escape::encode_text(&hits.join(" · "))
            ));
        }

        text.push_str(&format!(
            "\n\n🎯 <b>{}</b>\n{}",
            if russian { "Итог" } else { "Bottom line" },
            html_escape::encode_text(&self.verdict(russian))
        ));

        if self.guard_active {
            text.push_str(&format!(
                "\n• {}",
                html_escape::encode_text(if russian {
                    "Защита средней включена: самые безопасные докупки — под средней, не над ней."
                } else {
                    "Avg-entry guard is on: the only safe adds are below your average, not above it."
                })
            ));
        }
        if let Some(d) = &self.derivatives {
            let mut parts: Vec<String> = Vec::new();
            parts.push(format!("px 24h {:+.1}%", d.price_change_24h_pct));
            if let Some(f) = d.funding_avg_pct {
                parts.push(format!("funding {:+.3}%/8h", f * 100.0));
            }
            if let Some(oi) = d.oi_change_24h_pct {
                parts.push(format!("OI 24h {:+.1}%", oi));
            }
            if let Some(oi7) = d.oi_change_7d_pct {
                parts.push(format!("OI 7d {:+.1}%", oi7));
            }
            if let Some(ls) = d.long_short_ratio {
                parts.push(format!("L/S {ls:.2}"));
            }
            if let Some(t) = d.taker_buy_sell {
                parts.push(format!("taker {t:.2}"));
            }
            if !parts.is_empty() {
                text.push_str(&format!(
                    "\n{} <i>{}</i> · {}",
                    if russian {
                        "Фьючерсы:"
                    } else {
                        "Futures:"
                    },
                    html_escape::encode_text(&parts.join(" · ")),
                    html_escape::encode_text(d.read.verdict(russian))
                ));
            }
        }
        if cap.volume_spike {
            text.push_str(&format!(
                "\n• {}",
                html_escape::encode_text(if russian {
                    "Всплеск объёма: подтверждает капитуляцию."
                } else {
                    "Volume spike confirms the capitulation."
                })
            ));
        }

        let tails = self
            .buckets
            .iter()
            .map(|b| format!("{:.0}%", b.quantile * 100.0))
            .collect::<Vec<_>>()
            .join(" / ");
        text.push_str(&format!(
            "\n<i>{} {} · {} closes</i>",
            if russian {
                format!("Шанс касания вёдер ({}д):", TRAP_HORIZON_DAYS)
            } else {
                format!("Bucket touch odds ({}d):", TRAP_HORIZON_DAYS)
            },
            tails,
            self.history_days
        ));
        crate::format::guard_slash_units(&text)
    }

    /// Plain-text variant for logs / CLI.
    pub fn plain(&self, russian: bool) -> String {
        let dec = price_decimals(self.price);
        let cap = &self.capitulation;
        let mut text = format!(
            "DIP SNIPER {} {}\nZ14 {:+.2}sigma  cap {}/{} {}\ncash ${}  avg {}\n{}",
            self.symbol,
            fmt_money(self.price, dec),
            self.z14,
            cap.score,
            cap.max_score,
            cap.band.label(),
            crate::format::format_thousands(self.cash, 0),
            self.avg_entry
                .map(|v| fmt_money(v, dec))
                .unwrap_or_else(|| "-".into()),
            self.table_lines().join("\n"),
        );
        if let Some(d) = &self.derivatives {
            let mut parts: Vec<String> = Vec::new();
            parts.push(format!("px24h {:+.1}%", d.price_change_24h_pct));
            if let Some(f) = d.funding_avg_pct {
                parts.push(format!("funding {:+.3}%/8h", f * 100.0));
            }
            if let Some(oi) = d.oi_change_24h_pct {
                parts.push(format!("OI24h {:+.1}%", oi));
            }
            if let Some(oi7) = d.oi_change_7d_pct {
                parts.push(format!("OI7d {:+.1}%", oi7));
            }
            if let Some(ls) = d.long_short_ratio {
                parts.push(format!("L/S {ls:.2}"));
            }
            if !parts.is_empty() {
                text.push_str(&format!(
                    "\nfutures: {} · {}",
                    parts.join(" · "),
                    d.read.verdict(russian)
                ));
            }
        }
        text.push_str(&format!("\n{}", self.verdict(russian)));
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::forecast;

    fn series(n: usize, start: f64, step: f64) -> Vec<(i64, f64)> {
        (0..n)
            .map(|i| (i as i64 * DAY_MS, start + step * i as f64))
            .collect()
    }

    fn forecast_for(history: &[(i64, f64)]) -> Forecast {
        let price = history.last().unwrap().1;
        forecast::forecast("TEST", price, history)
    }

    fn report(cash: f64, protect: bool) -> DipSniperReport {
        let history = series(400, 100.0, 0.05);
        let f = forecast_for(&history);
        sniper_report(&f, &history, None, None, Some(80.0), 5.0, cash, protect)
    }

    #[test]
    fn small_position_keeps_full_precision() {
        // Regression: a 0.0074 BTC holding used to render as `pos 0.01 BTC`, i.e.
        // a visibly larger position than the user actually has.
        let history = series(400, 100.0, 0.05);
        let f = forecast_for(&history);
        let r = sniper_report(&f, &history, None, None, Some(100.0), 0.00740318, 0.0, false);
        let html = r.render_html(false);
        assert!(html.contains("0.00740318 TEST"), "html = {html}");
        assert!(!html.contains("pos 0.01 "), "html = {html}");
    }

    #[test]
    fn buckets_descend_and_stay_below_spot() {
        let r = report(1000.0, false);
        assert_eq!(r.buckets.len(), 3);
        for w in r.buckets.windows(2) {
            assert!(w[1].limit_price < w[0].limit_price);
        }
        for b in &r.buckets {
            assert!(b.limit_price < r.price);
            assert!(b.discount_pct < 0.0);
        }
    }

    #[test]
    fn deeper_buckets_are_less_likely_to_be_touched() {
        let r = report(1000.0, false);
        for w in r.buckets.windows(2) {
            assert!(w[1].prob_touch < w[0].prob_touch);
        }
        for b in &r.buckets {
            assert!((0.0..=1.0).contains(&b.prob_touch));
        }
    }

    /// Deterministic ~`daily_vol`-per-day random-walk-ish series (alternating
    /// log-returns), so a known volatility drives the solver without RNG.
    fn volatile_series(n: usize, base: f64, daily_vol: f64) -> Vec<(i64, f64)> {
        let mut px = base;
        (0..n)
            .map(|i| {
                let sign = if i % 2 == 0 { 1.0 } else { -1.0 };
                px *= (sign * daily_vol).exp();
                (i as i64 * DAY_MS, px)
            })
            .collect()
    }

    #[test]
    fn trap_levels_are_tradeable_not_absurd() {
        // Regression: the old far-tail (12/4/1%) 30-day cone quantiles placed
        // limits at roughly -23%/-35%/-50% on a normal-vol coin — nobody sets
        // those. Levels are now solved for 60/35/18% touch odds over 14 days.
        let history = volatile_series(400, 100.0, 0.03);
        let f = forecast_for(&history);
        let r = sniper_report(&f, &history, None, None, None, 0.0, 1000.0, false);

        for b in &r.buckets {
            assert!(
                b.discount_pct > -30.0,
                "bucket {} unreachable: {:.1}%",
                b.index,
                b.discount_pct
            );
        }
        // Shallow target → shallow level; deepest target → deepest level.
        assert!(r.buckets[0].discount_pct > r.buckets[1].discount_pct);
        assert!(r.buckets[1].discount_pct > r.buckets[2].discount_pct);

        // The printed probability must equal the requested touch target, i.e.
        // the label and the number agree by construction.
        for (b, &target) in r.buckets.iter().zip(TRAP_TARGET_TOUCH.iter()) {
            assert!(
                (b.prob_touch - target).abs() < 0.05,
                "bucket {} prob {:.3} vs target {:.3}",
                b.index,
                b.prob_touch,
                target
            );
        }
    }

    #[test]
    fn martingale_weights_sum_to_one_and_deepen() {
        let r = report(1000.0, false);
        let sum: f64 = r.buckets.iter().map(|b| b.weight).sum();
        assert!((sum - 1.0).abs() < 1e-9);
        assert!(r.buckets[2].weight > r.buckets[1].weight);
        assert!(r.buckets[1].weight > r.buckets[0].weight);
        assert!((r.buckets[2].spend_usd - 450.0).abs() < 1e-6);
    }

    #[test]
    fn protect_mode_starves_the_shallow_bucket() {
        let history = series(400, 100.0, 0.05);
        let f = forecast_for(&history);
        // Price 119.95 > avg 80 * 1.25 → guard active.
        let r = sniper_report(&f, &history, None, None, Some(80.0), 5.0, 1000.0, true);
        assert!(r.guard_active);
        assert!((r.buckets[0].weight - 0.10).abs() < 1e-9);
        assert!((r.buckets[2].weight - 0.55).abs() < 1e-9);
    }

    #[test]
    fn new_avg_entry_moves_towards_the_fill() {
        let v = new_avg_entry(Some(80.0), 10.0, 800.0, 40.0).unwrap();
        // Bought 20 units at 40 → (800 + 800) / 30 = 53.33
        assert!((v - 53.3333).abs() < 1e-3);
        assert!(v < 80.0);
    }

    #[test]
    fn dilution_guard_flags_buys_above_the_average() {
        let history = series(400, 100.0, 0.05);
        let f = forecast_for(&history);
        // Average far below spot: every bucket likely dilutes.
        let r = sniper_report(&f, &history, None, None, Some(50.0), 1.0, 1000.0, true);
        assert!(r.guard_active);
        assert!(r.buckets.iter().any(|b| b.dilutes_avg));
    }

    #[test]
    fn no_cash_still_produces_a_report() {
        let r = report(0.0, true);
        assert!(r.buckets.iter().all(|b| b.spend_usd == 0.0));
        assert!(r.buckets.iter().all(|b| b.new_avg_entry == Some(80.0)));
        assert!(
            r.verdict(true).contains("/sniper cash"),
            "{}",
            r.verdict(true)
        );
    }

    #[test]
    fn z_score_is_negative_below_the_anchor() {
        let z = drawdown_z(90.0, 100.0, 0.03, 14.0);
        assert!(z < -0.5 && z > -2.0);
        assert_eq!(drawdown_z(100.0, 100.0, 0.03, 14.0), 0.0);
        assert_eq!(drawdown_z(0.0, 100.0, 0.03, 14.0), 0.0);
    }

    #[test]
    fn capitulation_components_add_up() {
        let c = capitulation_score(
            -2.5,
            Some(20.0),
            Some(Candle {
                open: 100.0,
                high: 101.0,
                low: 88.0,
                close: 95.0,
            }),
            None,
            None,
        );
        // z 40 + rsi 25 + wick 15 = 80 of max 80 → capitulation.
        assert_eq!(c.score, 80);
        assert_eq!(c.max_score, 80);
        assert_eq!(c.band, CapBand::Capitulation);
        assert!(c.long_wick);
        assert!(!c.volume_spike);
    }

    #[test]
    fn calm_market_is_sleep() {
        let c = capitulation_score(-0.2, Some(55.0), None, None, None);
        assert_eq!(c.score, 0);
        assert_eq!(c.band, CapBand::Sleep);
    }

    #[test]
    fn aggregate_last_day_picks_the_latest_utc_day() {
        let rows = vec![
            (DAY_MS - 10, Some(1.0), Some(2.0), Some(0.5), 1.5),
            (DAY_MS, Some(1.5), Some(3.0), Some(1.0), 2.0),
            (DAY_MS + 1000, None, None, None, 2.5),
            (DAY_MS + 2000, Some(2.5), Some(2.6), Some(0.8), 1.2),
        ];
        let c = aggregate_last_day(&rows).unwrap();
        assert_eq!(c.open, 1.5);
        assert_eq!(c.high, 3.0);
        assert_eq!(c.low, 0.8);
        assert_eq!(c.close, 1.2);
        assert!(aggregate_last_day(&[]).is_none());
    }

    #[test]
    fn empty_history_does_not_panic() {
        let f = forecast::forecast("TEST", 10.0, &[]);
        let r = sniper_report(&f, &[], None, None, None, 0.0, 0.0, true);
        assert_eq!(r.buckets.len(), 3);
        assert!(!r.verdict(false).is_empty());
    }

    #[test]
    fn html_has_table_and_bottom_line() {
        let r = report(1000.0, false);
        let html = r.render_html(true);
        assert!(html.contains("<pre>"));
        assert!(html.contains("Итог"));
        assert!(html.contains("HARD DIP SNIPER"));
        assert!(html.ends_with("</i>"));
    }

    #[test]
    fn table_has_header_and_three_rows() {
        let r = report(500.0, false);
        let lines = r.table_lines();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].contains("limit"));
        assert!(lines[3].contains("swan"));
    }

    #[test]
    fn oi_flush_is_scored_and_rendered() {
        // A 5% one-day drop plus shrinking open interest = longs flushed.
        let mut history = series(400, 100.0, 0.02);
        let last_ts = history.last().unwrap().0 + DAY_MS;
        let prev = history.last().unwrap().1;
        history.push((last_ts, prev * 0.95));
        let f = forecast_for(&history);

        let mut m = OnChainMetrics::empty("TEST");
        m.oi_change_24h_pct = Some(-9.0);
        m.funding_rate_avg_8h = Some(0.0001);

        let r = sniper_report_with_derivatives(
            &f,
            &history,
            None,
            None,
            Some(80.0),
            5.0,
            1000.0,
            false,
            Some(&m),
        );
        assert!(r.derivatives.is_some());
        assert!(r.capitulation.oi_flush);
        let html = r.render_html(true);
        assert!(html.contains("Фьючерсы"));
        assert!(html.contains("лонги вымыты"));
    }

    #[test]
    fn missing_onchain_keeps_report_intact() {
        let r = report(1000.0, false);
        assert!(r.derivatives.is_none());
        assert!(!r.capitulation.oi_flush);
        assert!(!r.render_html(true).contains("Фьючерсы"));
    }

    #[test]
    fn shorts_piling_warns_against_catching_the_knife() {
        let mut history = series(400, 100.0, 0.02);
        let last_ts = history.last().unwrap().0 + DAY_MS;
        let prev = history.last().unwrap().1;
        history.push((last_ts, prev * 0.97));
        let f = forecast_for(&history);

        let mut m = OnChainMetrics::empty("TEST");
        m.oi_change_24h_pct = Some(7.0);

        let r = sniper_report_with_derivatives(
            &f,
            &history,
            None,
            None,
            None,
            0.0,
            500.0,
            false,
            Some(&m),
        );
        assert_eq!(
            r.derivatives.unwrap().read,
            crate::models::onchain::DerivativesRead::ShortsPiling
        );
        assert!(r.verdict(true).contains("не дно"));
    }
}
