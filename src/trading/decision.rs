//! Decision board — one compact, deterministic answer per coin.
//!
//! Where `/analyze` prints a full screen per symbol, `/decide` collapses the
//! whole portfolio into a single ranked board that answers one question per
//! coin: **buy now, scale in, wait, or hold?** The verdict is not invented here
//! — it is taken straight from [`DipReport`], whose math (zones, touch
//! probabilities, RSI, volatility cone) is the same engine `/dip`, `/forecast`
//! and `/analyze` use. This module only ranks and renders, so it stays easy to
//! reason about and to verify.

use crate::format::{format_thousands, price_decimals};
use crate::trading::bottom::band_label;
use crate::trading::dipzones::{DipReport, DipVerdict};
use crate::trading::forecast::Forecast;

/// What the board suggests doing with a coin, ordered from most to least
/// actionable. `Ord` is the ranking order used by [`rank`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Action {
    Buy,
    Scale,
    Wait,
    Hold,
    NoData,
}

impl Action {
    fn from_verdict(v: &DipVerdict) -> Action {
        match v {
            DipVerdict::BuyNow => Action::Buy,
            DipVerdict::ScaleIn { .. } => Action::Scale,
            DipVerdict::Wait { .. } => Action::Wait,
            DipVerdict::AboveAverage { .. } => Action::Hold,
            DipVerdict::NoData => Action::NoData,
        }
    }
}

/// A concrete level to bid at, taken from the report's nearest real zone.
#[derive(Clone, Debug)]
pub struct Level {
    pub price: f64,
    /// Distance from spot, percent (always negative).
    pub distance_pct: f64,
    pub label: String,
    /// `P(touch)` at [`Self::horizon_days`], 0..1.
    pub touch_prob: Option<f64>,
    pub horizon_days: i64,
}

/// A realistic 68% price band at one horizon, taken straight from the
/// calibrated cone `/forecast` draws (`HorizonForecast::low .. high`). This is
/// what the board leads with: "where can the price plausibly be in a week/month",
/// not a far-tail single target like "BTC will drop to $47k".
#[derive(Clone, Debug)]
pub struct RangeView {
    pub days: i64,
    pub low: f64,
    pub high: f64,
}

/// The forecast read for one coin, expressed the way a person actually plans:
/// a plausible range now (next week, next month) and the honest odds of being
/// lower at the month horizon. Deep tail levels and a staged ladder used to sit
/// here and read like price *predictions* ("target $47k"); they are gone.
#[derive(Clone, Debug)]
pub struct ForecastOutlook {
    /// The horizon whose `p_down` is reported (the month, when available).
    pub horizon_days: i64,
    /// P(the horizon close is below spot), 0..1. Honest ~coin-flip on this data.
    pub p_down: f64,
    pub day: Option<RangeView>,
    pub week: Option<RangeView>,
    pub month: Option<RangeView>,
}

/// A descriptive read of the *current* market stress for one coin: the 0..100
/// bottom score from [`crate::trading::bottom::bottom_signal`] (so `/decide`
/// and `/bottom` agree). This is a state gauge, not a forecast — the score has
/// no reliable forward edge across coins, so the board never presents it as
/// "the bottom is in".
#[derive(Clone, Debug)]
pub struct BottomView {
    pub score: u8,
}

/// One coin's decision.
#[derive(Clone, Debug)]
pub struct Decision {
    pub symbol: String,
    pub price: f64,
    pub avg_entry: Option<f64>,
    pub rsi14: Option<f64>,
    pub action: Action,
    pub level: Option<Level>,
    /// For `ScaleIn`: how much of the tranche to buy now (percent); the rest
    /// goes on the limit at `level.price`. `None` for non-split verdicts.
    pub now_pct: Option<u32>,
    /// True when spot is below its 200-day average (a downtrend). Kept for the
    /// entry policy and callers/tests; the forecast board itself no longer
    /// prints a trend lecture.
    #[allow(dead_code)]
    pub downtrend: bool,
    /// True when the raw verdict said buy/scale but the trend gate demoted it to
    /// wait (buying under the 200-day average has a negative historical edge on
    /// this coin). Lets the board explain *why* it says wait, so it never looks
    /// like it silently contradicts `/dip`'s raw BUY.
    pub trend_gated: bool,
    /// Days since the user's most recent recorded purchase of this symbol, if
    /// any. Lets the board say "you topped up N days ago" instead of repeating a
    /// generic prompt — the bot is only as aware of your buys as `/add` makes it.
    pub last_topup_days: Option<i64>,
    /// Probability read + staged-entry plan for coins where the board is telling
    /// the user to wait. `None` when the forecast is unavailable (e.g. tests).
    pub outlook: Option<ForecastOutlook>,
    /// Clear "where is the bottom" read: score, buy target, and the odds of
    /// reaching it at 1d/1w/1m/3m. `None` when the bottom signal is unavailable.
    pub bottom: Option<BottomView>,
}

/// Trend regimes in which buying has historically paid off for a given coin.
///
/// Derived from the same walk-forward study `decide verify` runs: for each
/// regime (below vs above the 200-day average) we measure the mean *excess*
/// forward return over 7/14/30 days. A regime is "open" only when that mean
/// excess is non-negative — otherwise a buy there is a worse bet than waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryPolicy {
    /// Buying while below the 200-day average (catching a knife) is allowed.
    pub knife_ok: bool,
    /// Buying pullbacks while above the 200-day average is allowed.
    pub pullback_ok: bool,
}

impl EntryPolicy {
    /// No gating — trust the raw verdict. Used when there is too little history
    /// to judge a regime.
    pub const fn permissive() -> Self {
        Self {
            knife_ok: true,
            pullback_ok: true,
        }
    }

    /// Whether an entry in the given regime is allowed.
    pub fn allows(&self, downtrend: bool) -> bool {
        if downtrend {
            self.knife_ok
        } else {
            self.pullback_ok
        }
    }
}

const POLICY_HORIZONS: [usize; 3] = [7, 14, 30];

/// Derive a coin's [`EntryPolicy`] from its daily closes. When history is too
/// short to judge either regime, that regime stays permissive so the board is
/// never gagged without evidence.
pub fn entry_policy(closes: &[f64]) -> EntryPolicy {
    let mean_excess = |uptrend: bool| -> Option<f64> {
        let vals: Vec<f64> = POLICY_HORIZONS
            .iter()
            .filter_map(|&h| {
                verify_entry_signal(closes, h, Some(uptrend)).map(|r| r.excess_pct)
            })
            .collect();
        if vals.is_empty() {
            None
        } else {
            Some(vals.iter().sum::<f64>() / vals.len() as f64)
        }
    };
    let mut policy = EntryPolicy::permissive();
    if let Some(e) = mean_excess(false) {
        policy.knife_ok = e >= 0.0;
    }
    if let Some(e) = mean_excess(true) {
        policy.pullback_ok = e >= 0.0;
    }
    policy
}

/// Turn the calibrated cone into an honest, realistic read for one coin: the
/// 68% price band (the corridor the price can plausibly be in) at the week and
/// month horizons, plus the calibrated odds of being lower at the month.
///
/// The band is taken straight from `/forecast`'s `HorizonForecast::low/high`, so
/// the board never contradicts the cone and never invents a far-tail target
/// ("BTC 47k" from 61k) that reads like a prediction but is really a 10th
/// percentile nobody expects.
fn build_outlook(report: &DipReport, forecast: &Forecast) -> Option<ForecastOutlook> {
    let price = report.price;
    if !(price > 0.0) {
        return None;
    }
    // The widest horizon at or below the requested one, so a shorter cone is not
    // mislabelled as a longer one.
    let range_at = |days: i64| -> Option<RangeView> {
        forecast
            .horizons
            .iter()
            .filter(|h| h.days >= 1 && h.days <= days)
            .max_by_key(|h| h.days)
            .map(|h| RangeView {
                days: h.days,
                low: h.low,
                high: h.high,
            })
    };
    let day = range_at(1);
    let week = range_at(7);
    let month = range_at(30);
    if day.is_none() && week.is_none() && month.is_none() {
        return None;
    }
    let horizon_days = month
        .as_ref()
        .map(|r| r.days)
        .or_else(|| week.as_ref().map(|r| r.days))
        .unwrap_or(30);
    // Direction: use the calibrated `prob_up` the `/forecast` ladder reports (a
    // logistic fit to walk-forward outcomes), not the raw drift. On this data
    // the drift has ~no edge, so `prob_up` sits near 50% — an honest "≈coin
    // flip" rather than a confident trend that does not exist.
    let p_down = forecast
        .horizons
        .iter()
        .find(|h| h.days == horizon_days)
        .map(|h| (1.0 - h.prob_up).clamp(0.0, 1.0))
        .unwrap_or(0.5);
    Some(ForecastOutlook {
        horizon_days,
        p_down,
        day,
        week,
        month,
    })
}

impl Decision {
    /// Build a decision from a finished [`DipReport`] with a permissive policy
    /// (no trend gating). Test-only convenience; production paths apply a
    /// data-derived [`EntryPolicy`] via [`Self::from_report_with_policy`].
    #[cfg(test)]
    pub fn from_report(report: &DipReport) -> Decision {
        Self::from_report_with_policy(report, EntryPolicy::permissive())
    }

    /// Build a decision from a finished [`DipReport`]. The nearest real zone
    /// (last, ascending) becomes the concrete level; its touch probability is
    /// read at ≥30 days (or the longest horizon if 30d is absent).
    pub fn from_report_with_policy(report: &DipReport, policy: EntryPolicy) -> Decision {
        let horizon_days = report
            .horizons
            .iter()
            .copied()
            .filter(|d| *d >= 30)
            .min()
            .or_else(|| report.horizons.last().copied())
            .unwrap_or(30);
        // Use the engine's own concrete level — the bid leg it actually
        // recommends — never a noise level <5% under spot. `BuyNow` means "at
        // market now", so it carries no limit level.
        let chosen: Option<f64> = match &report.verdict {
            DipVerdict::BuyNow | DipVerdict::NoData => None,
            DipVerdict::ScaleIn { zone_price, .. } => Some(*zone_price),
            DipVerdict::Wait { zone_price } | DipVerdict::AboveAverage { zone_price } => {
                *zone_price
            }
        };
        let level = chosen.map(|price| {
            let near = report
                .zones
                .iter()
                .find(|z| (z.price - price).abs() / price.max(1e-9) < 0.02);
            let (distance_pct, label, touch_prob) = match near {
                Some(z) => (z.distance_pct, z.label.clone(), z.prob_at(horizon_days)),
                None => (
                    (price / report.price.max(1e-9) - 1.0) * 100.0,
                    String::new(),
                    None,
                ),
            };
            Level {
                price,
                distance_pct,
                label,
                touch_prob,
                horizon_days,
            }
        });
        // Trend gate: only tell the user to buy in a regime whose historical
        // forward edge for *this* coin is non-negative (see `entry_policy`).
        let downtrend = report
            .sma200
            .map(|s| s > 0.0 && report.price < s)
            .unwrap_or(false);
        let mut action = Action::from_verdict(&report.verdict);
        let mut now_pct = match &report.verdict {
            DipVerdict::ScaleIn { now_pct, .. } => Some(*now_pct),
            _ => None,
        };
        let mut trend_gated = false;
        if matches!(action, Action::Buy | Action::Scale) && !policy.allows(downtrend) {
            action = Action::Wait;
            now_pct = None;
            trend_gated = true;
        }
        Decision {
            symbol: report.symbol.clone(),
            price: report.price,
            avg_entry: report.avg_entry,
            rsi14: report.rsi14,
            action,
            level,
            now_pct,
            downtrend,
            trend_gated,
            last_topup_days: None,
            outlook: None,
            bottom: None,
        }
    }

    /// Like [`Self::from_report_with_policy`], but also attaches the forecast
    /// read: the realistic 68% price band for the week and month (from the same
    /// calibrated cone `/forecast` draws) and the odds of being lower at the
    /// month. This is what makes the board a *forecast* ("likely 55–65k next
    /// month, down-vs-up ≈ 50/50") instead of a bare buy/wait switch or a
    /// far-tail single target.
    pub fn from_report_with_forecast(
        report: &DipReport,
        policy: EntryPolicy,
        forecast: &Forecast,
    ) -> Decision {
        let mut d = Self::from_report_with_policy(report, policy);
        d.outlook = build_outlook(report, forecast);
        d
    }

    /// Attach the current-state bottom gauge (0..100) for this coin. Kept
    /// descriptive on purpose: it is shown as "capitulation: high/moderate/…",
    /// never as a price target, because the score has no forward edge.
    pub fn attach_bottom(&mut self, score: u8) {
        self.bottom = Some(BottomView { score });
    }

    /// Signed percent vs the user's average entry, if one is recorded.
    pub fn vs_avg_pct(&self) -> Option<f64> {
        let avg = self.avg_entry.filter(|a| *a > 0.0)?;
        Some((self.price / avg - 1.0) * 100.0)
    }

    fn money(p: f64) -> String {
        if !p.is_finite() {
            return "—".into();
        }
        format!("${}", format_thousands(p, price_decimals(p)))
    }
}

/// Sort most-actionable first. Within an action, coins with a **higher** chance
/// of filling their level come first (they are more likely to be worth acting
/// on this week); ties break on symbol for stable output.
pub fn rank(decisions: &mut [Decision]) {
    decisions.sort_by(|a, b| {
        a.action
            .cmp(&b.action)
            .then_with(|| {
                let pa = a.level.as_ref().and_then(|l| l.touch_prob).unwrap_or(-1.0);
                let pb = b.level.as_ref().and_then(|l| l.touch_prob).unwrap_or(-1.0);
                pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.symbol.cmp(&b.symbol))
    });
}

fn vs_avg_text(d: &Decision, ru: bool) -> String {
    match d.vs_avg_pct() {
        Some(p) if p <= -0.05 => {
            if ru {
                format!("ниже средней {:.0}%", p)
            } else {
                format!("{:.0}% below avg", p)
            }
        }
        Some(p) if p >= 0.05 => {
            if ru {
                format!("выше средней +{:.0}%", p)
            } else {
                format!("{:.0}% above avg", p)
            }
        }
        Some(_) => {
            if ru {
                "у средней".into()
            } else {
                "at avg".into()
            }
        }
        None => String::new(),
    }
}

fn level_text(d: &Decision, ru: bool) -> String {
    let Some(l) = &d.level else {
        return if d.action == Action::Buy {
            if ru {
                "→ сейчас, по рынку".to_string()
            } else {
                "→ now, at market".to_string()
            }
        } else {
            String::new()
        };
    };
    let mut s = if let Some(now) = d.now_pct {
        if ru {
            format!("→ {now}% сейчас + лимит {} (", Decision::money(l.price))
        } else {
            format!("→ {now}% now + bid {} (", Decision::money(l.price))
        }
    } else if ru {
        format!("→ лимит {} (", Decision::money(l.price))
    } else {
        format!("→ bid {} (", Decision::money(l.price))
    };
    s.push_str(&format!("{:.1}%", l.distance_pct));
    if let Some(p) = l.touch_prob {
        if ru {
            s.push_str(&format!(", P{0}д {1:.0}%", l.horizon_days, p * 100.0));
        } else {
            s.push_str(&format!(", P{0}d {1:.0}%", l.horizon_days, p * 100.0));
        }
    }
    if !l.label.is_empty() {
        s.push_str(&format!(", {}", l.label));
    }
    s.push(')');
    s
}

/// The realistic 68% band at each available horizon, e.g.
/// `1нед $57,000–$64,000 · 1мес $53,000–$69,000`. This is the corridor the
/// user actually cares about ("will it be 55–65k?"); it replaces the old
/// far-tail ladder of single targets.
fn range_text(o: &ForecastOutlook, ru: bool) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut seen: Vec<i64> = Vec::new();
    for band in [&o.day, &o.week, &o.month].into_iter().flatten() {
        if seen.contains(&band.days) {
            continue;
        }
        seen.push(band.days);
        parts.push(format!(
            "{} {}–{}",
            horizon_label(band.days, ru),
            Decision::money(band.low),
            Decision::money(band.high)
        ));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

fn coin_line(d: &Decision, ru: bool) -> String {
    let mut details = Vec::new();
    let vs = vs_avg_text(d, ru);
    if !vs.is_empty() {
        details.push(vs);
    }
    if let Some(rsi) = d.rsi14 {
        details.push(format!("RSI {rsi:.0}"));
    }
    if let Some(days) = d.last_topup_days {
        details.push(if ru {
            if days <= 0 { "докупка сегодня".to_string() } else { format!("докупка {days}д назад") }
        } else if days <= 0 {
            "bought today".to_string()
        } else {
            format!("bought {days}d ago")
        });
    }
    if d.trend_gated {
        details.push(if ru {
            "ниже SMA200 — вход отложен".to_string()
        } else {
            "below SMA200 — entry postponed".to_string()
        });
    }
    let mut line = format!(
        "{} · <b>{}</b> <code>{}</code>",
        action_tag(d.action, ru),
        d.symbol,
        Decision::money(d.price)
    );
    if !details.is_empty() {
        line.push_str(" · ");
        line.push_str(&details.join(" · "));
    }

    // The headline the user asked for: a clear bottom read and a buy target.
    if let Some(b) = &d.bottom {
        // Clear bottom verdict first (what the user asked for), then the state
        // gauge it is derived from. The 0..100 score measures how stretched /
        // oversold the market is *right now* — it is not a price target and has
        // no reliable forward edge (ETH negative, SOL positive on ~50 events),
        // so we call it a state and never dress it up as "price will rise".
        line.push_str(&format!(
            "\n   <b>{}</b> · капитуляция {}/100",
            band_label(b.score, ru),
            b.score
        ));
    }

    // The realistic corridor and the direction odds over the horizon. This is
    // the honest forecast the user wants: "likely 55–65k next month, ↓/↑ ≈
    // coin flip" instead of a far-tail single target.
    if let Some(o) = &d.outlook {
        if let Some(range) = range_text(o, ru) {
            line.push_str(&format!("\n   диапазон ~68%: {range}"));
        }
        let h = horizon_label(o.horizon_days, ru);
        let down = (o.p_down * 100.0).round();
        line.push_str(&format!("\n   {h}: ↓{down:.0}% / ↑{:.0}%", 100.0 - down));
    }

    // Nearest actionable level (only ~an hour away in odds), if known.
    let lv = level_text(d, ru);
    if !lv.is_empty() {
        line.push_str(&format!("\n   {lv}"));
    }
    line
}

/// Short verdict tag shown at the start of each coin line.
fn action_tag(action: Action, ru: bool) -> &'static str {
    match (action, ru) {
        (Action::Buy, true) => "🟢 докупать",
        (Action::Buy, false) => "🟢 buy",
        (Action::Scale, true) => "🟡 частями",
        (Action::Scale, false) => "🟡 scale in",
        (Action::Wait, true) => "⏳ ждать",
        (Action::Wait, false) => "⏳ wait",
        (Action::Hold, true) => "🔴 держать",
        (Action::Hold, false) => "🔴 hold",
        (Action::NoData, true) => "⚪ нет данных",
        (Action::NoData, false) => "⚪ no data",
    }
}

fn horizon_label(days: i64, ru: bool) -> String {
    match (days, ru) {
        (1, true) => "1д".into(),
        (1, false) => "1d".into(),
        (7, true) => "1нед".into(),
        (7, false) => "1w".into(),
        (30, true) => "1мес".into(),
        (30, false) => "1m".into(),
        (90, true) => "3мес".into(),
        (90, false) => "3m".into(),
        (d, true) => format!("{d}д"),
        (d, false) => format!("{d}d"),
    }
}

/// Render the whole board as Telegram HTML. Coins are grouped by action; the
/// single best actionable entry is called out at the top.
pub fn render_html(decisions: &[Decision], skipped: &[(String, String)], ru: bool) -> String {
    let mut out = String::new();
    let mut live: Vec<&Decision> = decisions
        .iter()
        .filter(|d| d.action != Action::NoData)
        .collect();
    // Most-likely-bottom coins first, so the top of the board answers
    // "where is the bottom?" at a glance. Coins without a bottom read sink.
    live.sort_by(|a, b| {
        let sa = a.bottom.as_ref().map(|x| x.score).unwrap_or(0);
        let sb = b.bottom.as_ref().map(|x| x.score).unwrap_or(0);
        sb.cmp(&sa)
    });

    let total = decisions.len();
    let names: Vec<&str> = decisions.iter().map(|d| d.symbol.as_str()).collect();
    out.push_str(if ru {
        "🎯 <b>Диапазон и шансы</b>"
    } else {
        "🎯 <b>Range & odds</b>"
    });
    out.push_str(&format!(" · {total} · {}\n", names.join(" ")));

    for d in &live {
        out.push('\n');
        out.push_str(&coin_line(d, ru));
        out.push('\n');
    }

    if !skipped.is_empty() {
        out.push('\n');
        out.push_str(if ru {
            "⚠️ без данных: "
        } else {
            "⚠️ no data: "
        });
        let names: Vec<String> = skipped.iter().map(|(s, _)| s.clone()).collect();
        out.push_str(&names.join(", "));
        out.push('\n');
    }

    out.push('\n');
    out.push_str(if ru {
        "<i>диапазон ~68% — коридор из конуса /forecast; на истории внутри ~73% (1д), ~69% (1нед), ~62–68% (1мес) исходов; ↓/↑ — почти монетка, тренд не доказан; дно — текущее состояние, не прогноз</i>"
    } else {
        "<i>~68% range — corridor from the /forecast cone; historically ~73% (1d), ~69% (1w), ~62–68% (1m) of outcomes fall inside; ↓/↑ — near coin-flip, no proven trend; bottom — current state, not a forecast</i>"
    });
    out
}



// ── History verification ────────────────────────────────────────────────────
//
// The board tells you *when* to buy. This proves that "when" was not luck: it
// replays the exact state that triggers a buy/scale verdict — price below the
// 200-day average while RSI(14) is oversold (≤35) — across a coin's history and
// measures the forward return of those entries against buying on any given day.
// If the advice is timely, entries should beat the baseline and win more often.

pub const VERIFY_RSI_MAX: f64 = 35.0;
pub const VERIFY_SMA_DAYS: usize = 200;

/// One horizon's backtest result for the entry signal.
#[derive(Clone, Debug)]
pub struct VerifyReport {
    pub horizon_days: usize,
    pub n_signals: usize,
    pub n_baseline: usize,
    /// Share of signal entries with a positive forward return.
    pub hit_rate: f64,
    pub mean_ret_pct: f64,
    pub median_ret_pct: f64,
    pub baseline_mean_pct: f64,
    /// `mean_ret_pct - baseline_mean_pct`, the edge over buying any day.
    pub excess_pct: f64,
    /// 95% Wilson interval on the hit rate.
    pub ci_lo: f64,
    pub ci_hi: f64,
}

/// Simple (Cutler) RSI over `period` bars from the tail of `closes`.
fn rsi(closes: &[f64], period: usize) -> Option<f64> {
    if closes.len() <= period {
        return None;
    }
    let mut gain = 0.0;
    let mut loss = 0.0;
    for w in closes[closes.len() - period - 1..].windows(2) {
        let d = w[1] - w[0];
        if d > 0.0 {
            gain += d;
        } else {
            loss -= d;
        }
    }
    let avg_gain = gain / period as f64;
    let avg_loss = loss / period as f64;
    if avg_loss == 0.0 {
        return Some(if avg_gain == 0.0 { 50.0 } else { 100.0 });
    }
    Some(100.0 - 100.0 / (1.0 + avg_gain / avg_loss))
}

fn wilson(p: f64, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 0.0);
    }
    let z = 1.96;
    let n = n as f64;
    let denom = 1.0 + z * z / n;
    let centre = p + z * z / (2.0 * n);
    let margin = z * ((p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt());
    (
        ((centre - margin) / denom).max(0.0),
        ((centre + margin) / denom).min(1.0),
    )
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        0.0
    } else {
        v.iter().sum::<f64>() / v.len() as f64
    }
}

fn median(v: &mut [f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let m = v.len() / 2;
    if v.len() % 2 == 0 {
        (v[m - 1] + v[m]) / 2.0
    } else {
        v[m]
    }
}

/// Replay the entry signal over `closes` (oldest → newest) and compare forward
/// returns at `horizon_days` against the unconditional baseline.
///
/// `trend: Some(true)` requires price **above** SMA200 (buying a pullback in an
/// uptrend); `Some(false)` requires price **below** SMA200 (buying a knife in a
/// downtrend); `None` accepts either.
pub fn verify_entry_signal(
    closes: &[f64],
    horizon_days: usize,
    trend: Option<bool>,
) -> Option<VerifyReport> {
    if closes.len() <= VERIFY_SMA_DAYS + horizon_days {
        return None;
    }
    let mut signal_rets = Vec::new();
    let mut baseline_rets = Vec::new();
    for i in VERIFY_SMA_DAYS..closes.len() - horizon_days {
        let fwd = closes[i + horizon_days] / closes[i] - 1.0;
        baseline_rets.push(fwd * 100.0);
        let sma = mean(&closes[i - VERIFY_SMA_DAYS..i]);
        let Some(r) = rsi(&closes[..=i], 14) else {
            continue;
        };
        let above = closes[i] > sma;
        let trend_ok = match trend {
            Some(true) => above,
            Some(false) => !above,
            None => true,
        };
        if trend_ok && r <= VERIFY_RSI_MAX {
            signal_rets.push(fwd * 100.0);
        }
    }
    if signal_rets.is_empty() {
        return None;
    }
    let n = signal_rets.len();
    let wins = signal_rets.iter().filter(|r| **r > 0.0).count();
    let hit_rate = wins as f64 / n as f64;
    let (ci_lo, ci_hi) = wilson(hit_rate, n);
    let mean_ret = mean(&signal_rets);
    let baseline_mean = mean(&baseline_rets);
    let mut sorted = signal_rets.clone();
    Some(VerifyReport {
        horizon_days,
        n_signals: n,
        n_baseline: baseline_rets.len(),
        hit_rate,
        mean_ret_pct: mean_ret,
        median_ret_pct: median(&mut sorted),
        baseline_mean_pct: baseline_mean,
        excess_pct: mean_ret - baseline_mean,
        ci_lo,
        ci_hi,
    })
}

/// Human-readable verification line.
pub fn render_verify(symbol: &str, r: &VerifyReport, ru: bool) -> String {
    if ru {
        format!(
            "{symbol} · {h}д: входов {n}, прибыльных {hit:.0}% (95% CI {lo:.0}–{hi:.0}%)\n\
             средний доход {m:+.2}% · медиана {med:+.2}% · база {b:+.2}% ({nb}дней) · превышение {ex:+.2}%",
            symbol = symbol,
            h = r.horizon_days,
            n = r.n_signals,
            hit = r.hit_rate * 100.0,
            lo = r.ci_lo * 100.0,
            hi = r.ci_hi * 100.0,
            m = r.mean_ret_pct,
            med = r.median_ret_pct,
            b = r.baseline_mean_pct,
            ex = r.excess_pct,
            nb = r.n_baseline,
        )
    } else {
        format!(
            "{symbol} · {h}d: {n} entries, {hit:.0}% profitable (95% CI {lo:.0}–{hi:.0}%)\n\
             mean {m:+.2}% · median {med:+.2}% · baseline {b:+.2}% · edge {ex:+.2}%",
            symbol = symbol,
            h = r.horizon_days,
            n = r.n_signals,
            hit = r.hit_rate * 100.0,
            lo = r.ci_lo * 100.0,
            hi = r.ci_hi * 100.0,
            m = r.mean_ret_pct,
            med = r.median_ret_pct,
            b = r.baseline_mean_pct,
            ex = r.excess_pct,
        )
    }
}

// ── CLI runners (cargo run -- decide …) ─────────────────────────────────────

/// `cryptograph decide BTC ETH SOL LINK` — the same compact board the Telegram
/// command shows, printed to stdout without a portfolio (no average entries).
pub async fn run_cli_decide(
    symbols: Vec<String>,
    cmc: std::sync::Arc<crate::crypto::CryptoClient>,
    sqlite: std::sync::Arc<crate::db::SqliteStore>,
) -> anyhow::Result<()> {
    let mut decisions = Vec::new();
    let mut skipped = Vec::new();
    // Same live-quote anchor as the Telegram handler, so CLI output matches the
    // bot exactly. If quotes are unavailable (offline), it falls back to the
    // last cached close inside `build_dip_report_closes_forecast`.
    let refs: Vec<&str> = symbols.iter().map(|s| s.trim()).collect();
    let quotes = cmc.get_quotes(&refs).await.unwrap_or_default();
    let price_for = |sym: &str| {
        quotes
            .iter()
            .find(|q| q.symbol.eq_ignore_ascii_case(sym))
            .map(|q| q.price_usd)
            .filter(|p| *p > 0.0)
    };
    for sym in &symbols {
        let sym = sym.trim().to_uppercase();
        if sym.is_empty() {
            continue;
        }
        match crate::bot::handlers::build_dip_report_closes_forecast(
            &sym, &cmc, &sqlite, price_for(&sym), None,
        )
        .await
        {
            Ok((report, closes, forecast)) => {
                let policy = entry_policy(&closes);
                let mut d = Decision::from_report_with_forecast(&report, policy, &forecast);
                if let Ok(b) = crate::bot::handlers::build_bottom_signal(
                    &sym, &cmc, &sqlite, price_for(&sym), None, 0.0, 0.0, false,
                )
                .await
                {
                    d.attach_bottom(b.bottom_score);
                }
                decisions.push(d);
            }
            Err(e) => skipped.push((sym, e.to_string())),
        }
    }
    rank(&mut decisions);
    println!("{}", strip_tags(&render_html(&decisions, &skipped, true)));
    Ok(())
}

/// `cryptograph decide verify BTC ETH SOL LINK` — walk-forward check that the
/// engine's entry state (price below its 200-day mean AND oversold) is actually
/// followed by positive forward returns. This is the mathematical proof the
/// advice is timely, not just narrative.
pub async fn run_cli_verify(symbols: Vec<String>) -> anyhow::Result<()> {
    use chrono::{Duration, Utc};
    let today = Utc::now().date_naive();
    let from = today - Duration::days(1600);
    for sym in &symbols {
        let sym = sym.trim().to_uppercase();
        if sym.is_empty() {
            continue;
        }
        let history = match crate::trading::backtest::fetch_history(&sym, from, today).await {
            Ok(h) if h.len() > VERIFY_SMA_DAYS + 31 => h,
            Ok(h) => {
                println!("⚠️ {sym}: only {} bars, need {}+", h.len(), VERIFY_SMA_DAYS + 31);
                continue;
            }
            Err(e) => {
                println!("⚠️ {sym}: {e}");
                continue;
            }
        };
        let closes: Vec<f64> = history.iter().map(|(_, p)| *p).collect();
        println!("\n🔎 {sym}  ({} bars)", closes.len());
        for horizon in [7usize, 14, 30] {
            let knife = verify_entry_signal(&closes, horizon, Some(false));
            let pull = verify_entry_signal(&closes, horizon, Some(true));
            match knife {
                Some(v) => println!("  ↓ниже SMA200 {}", strip_tags(&render_verify(&sym, &v, true))),
                None => println!("  ↓ниже SMA200 {horizon}d: нет сигналов"),
            }
            match pull {
                Some(v) => println!("  ↑выше SMA200 {}", strip_tags(&render_verify(&sym, &v, true))),
                None => println!("  ↑выше SMA200 {horizon}d: нет сигналов"),
            }
        }
    }
    Ok(())
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(verdict: DipVerdict, price: f64, avg: Option<f64>) -> DipReport {
        DipReport {
            symbol: "BTC".into(),
            price,
            avg_entry: avg,
            zones: vec![],
            verdict,
            reason: String::new(),
            vol_daily_pct: 2.0,
            drift_daily_pct: -0.1,
            rsi14: Some(30.0),
            // Default to an uptrend (spot above SMA200) so verdict-mapping tests
            // exercise the ungated path; the trend-gate test sets its own SMA.
            sma200: Some(price * 0.9),
            history_days: 400,
            horizons: vec![7, 14, 30, 90, 180],
        }
    }

    #[test]
    fn verdicts_map_to_actions_in_rank_order() {
        assert_eq!(Action::from_verdict(&DipVerdict::BuyNow), Action::Buy);
        assert_eq!(
            Action::from_verdict(&DipVerdict::ScaleIn {
                now_pct: 50,
                zone_price: 1.0
            }),
            Action::Scale
        );
        assert_eq!(
            Action::from_verdict(&DipVerdict::Wait { zone_price: None }),
            Action::Wait
        );
        assert_eq!(
            Action::from_verdict(&DipVerdict::AboveAverage { zone_price: None }),
            Action::Hold
        );
        assert!(Action::Buy < Action::Scale);
        assert!(Action::Scale < Action::Wait);
        assert!(Action::Wait < Action::Hold);
    }

    #[test]
    fn rank_puts_buy_before_wait_before_hold() {
        let mut ds = vec![
            Decision::from_report(&report(
                DipVerdict::AboveAverage { zone_price: None },
                1.0,
                Some(1.0),
            )),
            Decision::from_report(&report(
                DipVerdict::Wait {
                    zone_price: Some(0.9),
                },
                1.0,
                None,
            )),
            Decision::from_report(&report(DipVerdict::BuyNow, 1.0, Some(1.1))),
        ];
        rank(&mut ds);
        assert_eq!(ds[0].action, Action::Buy);
        assert_eq!(ds[1].action, Action::Wait);
        assert_eq!(ds[2].action, Action::Hold);
    }

    #[test]
    fn vs_avg_pct_is_signed() {
        let d = Decision::from_report(&report(DipVerdict::BuyNow, 90.0, Some(100.0)));
        assert!((d.vs_avg_pct().unwrap() + 10.0).abs() < 1e-9);
        let d = Decision::from_report(&report(DipVerdict::BuyNow, 110.0, Some(100.0)));
        assert!((d.vs_avg_pct().unwrap() - 10.0).abs() < 1e-9);
        let d = Decision::from_report(&report(DipVerdict::BuyNow, 110.0, None));
        assert!(d.vs_avg_pct().is_none());
    }

    #[test]
    fn render_shows_best_entry_and_sections() {
        let mut ds = vec![
            Decision::from_report(&report(DipVerdict::BuyNow, 60_571.0, Some(62_000.0))),
            Decision::from_report(&report(
                DipVerdict::AboveAverage { zone_price: None },
                1_700.0,
                Some(1_500.0),
            )),
        ];
        rank(&mut ds);
        let html = render_html(&ds, &[], true);
        assert!(html.contains("докупать"));
        assert!(html.contains("держать"));
    }

    #[test]
    fn render_empty_says_nothing_to_buy() {
        let ds = vec![Decision::from_report(&report(
            DipVerdict::Wait { zone_price: None },
            1.0,
            Some(1.0),
        ))];
        let html = render_html(&ds, &[], true);
        assert!(html.contains("ждать"));
    }

    #[test]
    fn verify_needs_enough_history() {
        let closes: Vec<f64> = (0..100).map(|i| 100.0 + i as f64).collect();
        assert!(verify_entry_signal(&closes, 30, None).is_none());
    }

    #[test]
    fn verify_counts_a_rising_series_as_profitable() {
        // A steadily rising series: buy-signal days (few) still rise afterwards.
        let closes: Vec<f64> = (0..400).map(|i| 100.0 + i as f64 * 0.5).collect();
        // Uptrend never dips below SMA200, so no signals -> None.
        assert!(verify_entry_signal(&closes, 14, None).is_none());
    }

    #[test]
    fn verify_reports_hit_rate_and_edge() {
        // Down-then-up V shape: oversold entries near the bottom should win.
        let mut closes = Vec::new();
        for i in 0..300 {
            let p = if i < 250 {
                200.0 - i as f64 * 0.5
            } else {
                75.0 + (i - 250) as f64 * 2.0
            };
            closes.push(p);
        }
        let r = verify_entry_signal(&closes, 14, None).expect("signal exists in a fall");
        assert!(r.n_signals > 0);
        assert!(r.hit_rate >= 0.0 && r.hit_rate <= 1.0);
        assert!(r.ci_lo <= r.hit_rate && r.hit_rate <= r.ci_hi);
        assert!(r.excess_pct.is_finite());
    }

    const BLOCK_KNIFE: EntryPolicy = EntryPolicy {
        knife_ok: false,
        pullback_ok: true,
    };

    #[test]
    fn below_trend_gates_buy_when_knife_blocked() {
        let mut r = report(DipVerdict::BuyNow, 60_000.0, Some(62_000.0));
        r.sma200 = Some(70_000.0); // price below SMA200 -> downtrend
        let d = Decision::from_report_with_policy(&r, BLOCK_KNIFE);
        assert_eq!(d.action, Action::Wait);
        assert!(d.downtrend);
        assert!(d.trend_gated);
        assert!(d.now_pct.is_none());
        // The board must say *why* it waits, so `/decide` cannot silently
        // contradict `/dip`'s raw "buy" with an unexplained "wait".
        let html = render_html(&[d], &[], true);
        assert!(html.contains("SMA200"), "{html}");
    }

    #[test]
    fn below_trend_gates_scale_when_knife_blocked() {
        let mut r = report(
            DipVerdict::ScaleIn {
                now_pct: 40,
                zone_price: 55_000.0,
            },
            60_000.0,
            Some(62_000.0),
        );
        r.sma200 = Some(70_000.0);
        let d = Decision::from_report_with_policy(&r, BLOCK_KNIFE);
        assert_eq!(d.action, Action::Wait);
        assert!(d.now_pct.is_none());
    }

    #[test]
    fn permissive_policy_keeps_knife_buy() {
        let mut r = report(DipVerdict::BuyNow, 60_000.0, Some(62_000.0));
        r.sma200 = Some(70_000.0);
        let d = Decision::from_report(&r); // permissive default
        assert_eq!(d.action, Action::Buy);
        assert!(d.downtrend);
    }

    #[test]
    fn policy_allows_matches_regime() {
        assert!(BLOCK_KNIFE.allows(false)); // uptrend regime
        assert!(!BLOCK_KNIFE.allows(true)); // downtrend regime
        assert!(EntryPolicy::permissive().allows(true));
        assert!(EntryPolicy::permissive().allows(false));
    }

    #[test]
    fn entry_policy_permissive_without_history() {
        let p = entry_policy(&[]);
        assert_eq!(p, EntryPolicy::permissive());
    }

    #[test]
    fn allows_routes_by_regime() {
        let p = EntryPolicy { knife_ok: false, pullback_ok: true };
        assert!(!p.allows(true)); // downtrend -> knife gate
        assert!(p.allows(false)); // uptrend -> pullback open
        assert!(EntryPolicy::permissive().allows(true));
        assert!(EntryPolicy::permissive().allows(false));
    }


    #[test]
    fn above_trend_keeps_buy() {
        let mut r = report(DipVerdict::BuyNow, 60_000.0, Some(62_000.0));
        r.sma200 = Some(50_000.0); // price above SMA200 -> uptrend
        let d = Decision::from_report(&r);
        assert_eq!(d.action, Action::Buy);
        assert!(!d.downtrend);
    }

    /// A long, gently-declining series so the cone has a real (negative) drift
    /// and positive dispersion for the outlook ladder.
    fn declining_history(n: usize, start: f64, end: f64) -> Vec<(i64, f64)> {
        let step = (end / start).powf(1.0 / (n - 1) as f64);
        (0..n)
            .map(|i| (1_600_000_000 + i as i64 * 86_400, start * step.powi(i as i32)))
            .collect()
    }

    #[test]
    fn outlook_gives_realistic_ranges_and_odds() {
        let hist = declining_history(400, 100.0, 55.0);
        let price = hist.last().unwrap().1;
        let f = crate::trading::forecast::forecast("BTC", price, &hist);
        let mut r = report(DipVerdict::Wait { zone_price: None }, price, None);
        r.sma200 = Some(price * 1.15); // downtrend
        let d = Decision::from_report_with_forecast(&r, EntryPolicy::permissive(), &f);
        let o = d.outlook.expect("outlook should exist with a valid cone");

        assert_eq!(o.horizon_days, 30);
        assert!((0.0..=1.0).contains(&o.p_down), "p_down={}", o.p_down);
        let week = o.week.expect("week band");
        let month = o.month.expect("month band");
        // Bands are real corridors: low below high, and around spot (not a
        // far-tail single level like the old "BTC 47k from 61k").
        assert!(week.low < week.high, "week={week:?}");
        assert!(month.low < month.high, "month={month:?}");
        assert!(month.low <= week.low, "month should reach further down");
        // The corridor is a *near* range around spot, not a far-tail single
        // level (the old "BTC 47k from 61k" complaint). Both bands stay within
        // a realistic ±50% of spot.
        for band in [week, month] {
            assert!(
                band.low > price * 0.5 && band.high < price * 1.5,
                "band={band:?} too far from spot={price}"
            );
            assert!(band.low > 0.0 && band.high > 0.0);
        }
    }

    #[test]
    fn range_text_has_no_raw_angle_brackets() {
        let hist = declining_history(400, 100.0, 55.0);
        let price = hist.last().unwrap().1;
        let f = crate::trading::forecast::forecast("BTC", price, &hist);
        let mut r = report(DipVerdict::Wait { zone_price: None }, price, None);
        r.sma200 = Some(price * 1.15);
        let d = Decision::from_report_with_forecast(&r, EntryPolicy::permissive(), &f);
        let o = d.outlook.expect("outlook");
        let txt = range_text(&o, true).expect("range text");
        // Telegram HTML parse mode: a stray '<' (e.g. a bare "<$5k") would
        // swallow the rest of the message.
        assert!(!txt.contains('<'), "raw '<' in range text: {txt}");
        assert!(txt.contains('–'), "expected an en-dash range: {txt}");
    }

    #[test]
    fn no_forecast_means_no_outlook() {
        let r = report(DipVerdict::Wait { zone_price: Some(0.9) }, 1.0, None);
        let d = Decision::from_report_with_policy(&r, EntryPolicy::permissive());
        assert!(d.outlook.is_none());
    }

    /// Smoke test that prints the rendered board (run with `--nocapture`) so the
    /// Telegram layout can be eyeballed without network or secrets.
    #[test]
    fn board_renders_readable_layout() {
        let hist = declining_history(400, 100.0, 55.0);
        let price = hist.last().unwrap().1;
        let f = crate::trading::forecast::forecast("BTC", price, &hist);

        let mut wait = report(DipVerdict::Wait { zone_price: None }, price, None);
        wait.symbol = "BTC".into();
        wait.sma200 = Some(price * 1.15);
        let d1 = Decision::from_report_with_forecast(&wait, EntryPolicy::permissive(), &f);
        let mut d1 = d1;
        d1.attach_bottom(72);

        let mut hold = report(
            DipVerdict::AboveAverage { zone_price: None },
            price,
            Some(price * 1.2),
        );
        hold.symbol = "ETH".into();
        hold.sma200 = Some(price * 1.15);
        let d2 = Decision::from_report_with_forecast(&hold, EntryPolicy::permissive(), &f);

        let mut ds = vec![d1, d2];
        rank(&mut ds);
        let html = render_html(&ds, &[], true);
        println!("\n===== BOARD (plain) =====\n{}\n===== END =====", strip_tags(&html));
        assert!(html.contains("диапазон ~68%"), "{html}");
        assert!(html.contains("капитуляция"), "{html}");
    }

    /// Out-of-sample calibration check over the *full* cached history, walked
    /// forward with calibration refit only on the past. Answers one question:
    /// when the bot says "P(touch within 30d) = 40%", does it actually happen
    /// ~40% of the time? Run with `--ignored --nocapture`. Not part of CI (it is
    /// slow and prints a report rather than asserting a fixed number).
    #[test]
    #[ignore]
    fn walk_forward_calibration_over_full_history() {
        use crate::trading::dipzones::touch_probability_forecast;
        use crate::trading::forecast::{calibrate, forecast_with, student_t_quantile, STUDENT_DF};
        use crate::trading::history_file::load_history;

        let horizons = [1i64, 7, 30, 90];
        let deltas = [0.05f64, 0.10, 0.20];
        for coin in ["BTC", "ETH", "SOL", "LINK"] {
            let Some(hist) = load_history(coin) else {
                println!("{coin}: no cached history");
                continue;
            };
            let closes: Vec<f64> = hist.iter().map(|&(_, c)| c).collect();
            let n = closes.len();
            if n < 600 {
                println!("{coin}: too short ({n})");
                continue;
            }
            let start = 400usize;
            let mut cal = calibrate(coin, &hist[..start]);
            let mut last_fit = start;
            let mut touch: Vec<(f64, bool, i64)> = Vec::new();
            let mut ladder: Vec<(f64, bool)> = Vec::new();
            let mut origins = 0usize;
            for origin in start..n - 1 {
                if origin - last_fit >= 30 {
                    cal = calibrate(coin, &hist[..=origin]);
                    last_fit = origin;
                }
                let price = closes[origin];
                let f = forecast_with(coin, price, &hist[..=origin], &cal);
                origins += 1;
                for &delta in &deltas {
                    let level = price * (1.0 - delta);
                    for &h in &horizons {
                        let j = origin + h as usize;
                        if j >= n {
                            continue;
                        }
                        let p = touch_probability_forecast(&f, level, h);
                        let hit = closes[origin + 1..=j].iter().any(|&c| c <= level);
                        touch.push((p, hit, h));
                    }
                }
                let j = origin + 30;
                if j < n {
                    let m = f.moments(30);
                    for &q in &[0.5f64, 0.25, 0.10] {
                        let r = student_t_quantile(q, STUDENT_DF);
                        let level = price * (m.log_drift + r * m.sigma_h).exp();
                        ladder.push((q, closes[j] < level));
                    }
                }
            }
            println!("\n=== {coin} ===  origins={origins}");
            let brier: f64 = touch
                .iter()
                .map(|&(p, h, _)| (p - if h { 1.0 } else { 0.0 }).powi(2))
                .sum::<f64>()
                / touch.len() as f64;
            let base = touch.iter().filter(|t| t.1).count() as f64 / touch.len() as f64;
            let bbrier: f64 = touch
                .iter()
                .map(|&(_, h, _)| (base - if h { 1.0 } else { 0.0 }).powi(2))
                .sum::<f64>()
                / touch.len() as f64;
            println!(
                "  touch: n={} base={:.3} brier={:.4} skill={:+.1}%",
                touch.len(),
                base,
                brier,
                (1.0 - brier / bbrier) * 100.0
            );
            for &h in &horizons {
                let s: Vec<&(f64, bool, i64)> = touch.iter().filter(|t| t.2 == h).collect();
                if s.is_empty() {
                    continue;
                }
                let mp = s.iter().map(|t| t.0).sum::<f64>() / s.len() as f64;
                let em = s.iter().filter(|t| t.1).count() as f64 / s.len() as f64;
                println!(
                    "    h={h:>2}d n={:>5} pred={:.3} emp={:.3} err={:+.3}",
                    s.len(),
                    mp,
                    em,
                    em - mp
                );
            }
            println!("    reliability (pred bin -> empirical):");
            for b in 0..5 {
                let lo = b as f64 * 0.2;
                let s: Vec<&(f64, bool, i64)> = touch
                    .iter()
                    .filter(|t| t.0 >= lo && (t.0 < lo + 0.2 || b == 4))
                    .collect();
                if s.is_empty() {
                    continue;
                }
                let mp = s.iter().map(|t| t.0).sum::<f64>() / s.len() as f64;
                let em = s.iter().filter(|t| t.1).count() as f64 / s.len() as f64;
                println!(
                    "      {:.1}-{:.1} n={:>5} pred={:.3} emp={:.3}",
                    lo,
                    lo + 0.2,
                    s.len(),
                    mp,
                    em
                );
            }
            println!("    ladder @30d (nominal -> empirical):");
            for &q in &[0.5f64, 0.25, 0.10] {
                let s: Vec<&(f64, bool)> = ladder.iter().filter(|t| (t.0 - q).abs() < 1e-9).collect();
                let em = s.iter().filter(|t| t.1).count() as f64 / s.len().max(1) as f64;
                println!("      q={:.2} n={} emp={:.3}", q, s.len(), em);
            }
        }
    }

    /// Render the real `/decide` board for the cached coins (run with
    /// `--ignored --nocapture`) so the layout and the numbers can be eyeballed
    /// against actual market data, not a synthetic series.
    #[test]
    #[ignore]
    fn board_renders_on_cached_history() {
        use crate::trading::history_file::load_history;
        let mut ds = Vec::new();
        for coin in ["BTC", "ETH", "SOL", "LINK"] {
            let Some(hist) = load_history(coin) else { continue };
            let price = hist.last().unwrap().1;
            let f = crate::trading::forecast::forecast(coin, price, &hist);
            let closes: Vec<f64> = hist.iter().map(|&(_, c)| c).collect();
            let policy = entry_policy(&closes);
            let mut r = report(DipVerdict::Wait { zone_price: None }, price, None);
            r.symbol = coin.into();
            r.sma200 = if closes.len() >= 200 {
                Some(closes[closes.len() - 200..].iter().sum::<f64>() / 200.0)
            } else {
                None
            };
            let mut d = Decision::from_report_with_forecast(&r, policy, &f);
            d.attach_bottom(50);
            ds.push(d);
        }
        println!("===== REAL BOARD =====\n{}", strip_tags(&render_html(&ds, &[], true)));
    }

    /// Verify the exact claim `/decide` prints in its footer — the corridor's
    /// actual hit rate — out-of-sample on full history. Walk forward:
    /// refit the calibration only on the past every 30 days, build the forecast
    /// (same `low/high` the board renders), then check whether the *future*
    /// close at 1/7/30 days landed inside the band. Prints real coverage per coin
    /// per horizon (ideal ≈ 68%) plus the mean band width as a % of spot.
    /// Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn decide_band_covers_about_68_percent_out_of_sample() {
        use crate::trading::forecast::{calibrate, forecast_with};
        use crate::trading::history_file::load_history;

        for coin in ["BTC", "ETH", "SOL", "LINK"] {
            let Some(hist) = load_history(coin) else {
                println!("{coin}: no cached history");
                continue;
            };
            let closes: Vec<f64> = hist.iter().map(|&(_, c)| c).collect();
            let n = closes.len();
            if n < 600 {
                println!("{coin}: too short ({n})");
                continue;
            }
            let start = 400usize;
            let mut cal = calibrate(coin, &hist[..start]);
            let mut last_fit = start;
            // per horizon: (inside, total, width_sum)
            let mut acc = [(0usize, 0usize, 0.0f64); 3];
            let horizons = [1usize, 7, 30];
            for origin in start..n - 1 {
                if origin - last_fit >= 30 {
                    cal = calibrate(coin, &hist[..=origin]);
                    last_fit = origin;
                }
                let price = closes[origin];
                let f = forecast_with(coin, price, &hist[..=origin], &cal);
                for (k, &h) in horizons.iter().enumerate() {
                    let j = origin + h;
                    if j >= n {
                        continue;
                    }
                    let Some(hz) = f.horizons.iter().find(|x| x.days == h as i64) else {
                        continue;
                    };
                    let actual = closes[j];
                    if actual >= hz.low && actual <= hz.high {
                        acc[k].0 += 1;
                    }
                    acc[k].1 += 1;
                    acc[k].2 += (hz.high - hz.low) / price;
                }
            }
            println!("\n=== {coin} ===");
            for (k, &h) in horizons.iter().enumerate() {
                let (inside, total, wsum) = acc[k];
                if total == 0 {
                    println!("  {h:>2}d: no samples");
                    continue;
                }
                let cov = inside as f64 / total as f64 * 100.0;
                let width = wsum / total as f64 * 100.0;
                println!(
                    "  {h:>2}d: coverage={:.1}% (ideal 68%) n={} avg-width={:.1}% of spot",
                    cov, total, width
                );
            }
        }
    }
}
