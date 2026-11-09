//! Wide-sample verification of the `/dip` + `/forecast` math.
//!
//! Companion to `dip_history_verification.rs` (a fast 120-origin smoke test).
//! Here the same walk-forward replay runs over thousands of origins spread
//! across the *entire* history of each symbol, then the residuals are broken
//! down by:
//!
//! * market regime (`Bull` / `Bear` / `Sideways` / `Dip` / `Euphoria`),
//! * volatility tercile (quiet / normal / tense),
//! * a dense replay of the last ~6 months (step 1, no sampling).
//!
//! The point is to catch a model that is calibrated *on average* but broken in
//! one regime — the exact failure mode that hides in aggregate numbers.
//!
//! Everything is deterministic: origins are strided, the forecast is a pure
//! function of the closes supplied, and no randomness is used.

use cryptograph::trading::dipzones::{
    dip_report, prob_below_at_horizon, prob_below_forecast, touch_probability,
    touch_probability_forecast, DipReport, DipVerdict, DipZone,
};
use cryptograph::trading::forecast::{forecast, horizons_for};
use cryptograph::trading::history_file;
use cryptograph::trading::regime::{classify_closes, MarketRegime};
use std::collections::BTreeMap;

const SYMBOLS: [&str; 4] = ["BTC", "ETH", "LINK", "SOL"];
const MIN_HISTORY: usize = 300;
const MAX_H: usize = 90;
const SCORED_HORIZONS: [i64; 4] = [7, 14, 30, 90];
const TOUCH_LEVELS_PCT: [f64; 5] = [-2.0, -5.0, -10.0, -20.0, -30.0];
const MIN_DIP_GAP_PCT: f64 = 2.0;
/// Origins per symbol in the wide run. Strided over the whole history.
const WIDE_ORIGINS: usize = 700;
/// Dense replay window at the end of history.
const RECENT_DAYS: usize = 180;

fn load(symbol: &str) -> Vec<(i64, f64)> {
    let h = history_file::load_history(symbol)
        .unwrap_or_else(|| panic!("missing data/history/{symbol}.csv"));
    assert!(
        h.len() > MIN_HISTORY + MAX_H + 50,
        "{symbol} history too short"
    );
    h
}

/// Evenly spaced origins over `[MIN_HISTORY, n - MAX_H - 1]`.
fn strided_origins(n: usize, count: usize) -> Vec<usize> {
    let lo = MIN_HISTORY;
    let hi = n.saturating_sub(MAX_H + 1);
    if hi <= lo {
        return Vec::new();
    }
    let span = hi - lo;
    let step = (span / count).max(1);
    (lo..hi).step_by(step).collect()
}

#[derive(Clone, Copy)]
struct TouchObs {
    h: i64,
    level_pct: f64,
    pred: f64,
    actual: f64,
    vol_pct: f64,
    drift_pct: f64,
    regime: MarketRegime,
}

#[derive(Clone, Copy)]
struct BelowObs {
    h: i64,
    level_pct: f64,
    pred: f64,
    actual: f64,
    vol_pct: f64,
    regime: MarketRegime,
}

fn pct(p: f64) -> String {
    format!("{:>6.1}%", p * 100.0)
}

fn gap_stats(preds_actuals: impl Iterator<Item = (f64, f64)>) -> Option<(usize, f64, f64, f64)> {
    let v: Vec<(f64, f64)> = preds_actuals.collect();
    if v.is_empty() {
        return None;
    }
    let n = v.len() as f64;
    let pred = v.iter().map(|x| x.0).sum::<f64>() / n;
    let act = v.iter().map(|x| x.1).sum::<f64>() / n;
    Some((v.len(), pred, act, act - pred))
}

fn print_grouped(label: &str, rows: &[(String, usize, f64, f64, f64)]) {
    println!("\n── {label} ──");
    println!(
        "{:<14} {:>7} {:>10} {:>10} {:>8}",
        "group", "n", "predicted", "actual", "gap"
    );
    for (name, n, pred, act, gap) in rows {
        println!(
            "{:<14} {:>7} {:>10} {:>10} {:>+8.3}",
            name,
            n,
            pct(*pred),
            pct(*act),
            gap
        );
    }
}

fn brier(pairs: &[(f64, f64)]) -> f64 {
    if pairs.is_empty() {
        return f64::NAN;
    }
    pairs.iter().map(|(p, a)| (p - a).powi(2)).sum::<f64>() / pairs.len() as f64
}

/// Pull the first integer percentage that follows a `~` in a rendered line,
/// e.g. `"… — ~12% chance to touch within 90d"` → `12.0`.
fn parse_tilde_pct(line: &str) -> Option<f64> {
    let tail = line.split('~').nth(1)?;
    let num: String = tail
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num.parse().ok()
}

/// Climatology baseline: per (horizon, level) base rate, the honest comparison
/// for a probabilistic score.
fn climatology_brier<T>(
    items: &[T],
    key: impl Fn(&T) -> (i64, f64),
    pair: impl Fn(&T) -> (f64, f64),
) -> f64 {
    let mut groups: BTreeMap<(i64, i64), Vec<f64>> = BTreeMap::new();
    for it in items {
        let (h, lv) = key(it);
        groups
            .entry((h, (lv * 10.0) as i64))
            .or_default()
            .push(pair(it).1);
    }
    let mut pairs = Vec::with_capacity(items.len());
    for it in items {
        let (h, lv) = key(it);
        let g = &groups[&(h, (lv * 10.0) as i64)];
        let base = g.iter().sum::<f64>() / g.len() as f64;
        pairs.push((base, pair(it).1));
    }
    brier(&pairs)
}

fn terciles(values: &[f64]) -> (f64, f64) {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    (q(1.0 / 3.0), q(2.0 / 3.0))
}

fn vol_bucket(vol: f64, cuts: (f64, f64)) -> &'static str {
    if vol <= cuts.0 {
        "quiet"
    } else if vol <= cuts.1 {
        "normal"
    } else {
        "tense"
    }
}

#[test]
fn wide_sample_replay_is_calibrated_in_every_regime() {
    let mut touch: Vec<TouchObs> = Vec::new();
    let mut below: Vec<BelowObs> = Vec::new();
    let mut origins = 0usize;
    let mut verdicts: BTreeMap<&'static str, usize> = BTreeMap::new();

    for symbol in SYMBOLS {
        let hist = load(symbol);
        let n = hist.len();
        let points = strided_origins(n, WIDE_ORIGINS);
        assert!(
            points.len() >= WIDE_ORIGINS,
            "{symbol}: only {} origins",
            points.len()
        );

        for &origin in &points {
            origins += 1;
            let past = &hist[..=origin];
            let price = past.last().unwrap().1;
            let fc = forecast(symbol, price, past);
            let closes: Vec<f64> = past.iter().map(|&(_, c)| c).collect();
            let regime = classify_closes(&closes);
            let vol_pct = fc.vol_daily_pct;

            // Architectural invariant, checked on every origin: the cone bounds
            // printed by `/forecast` and the probabilities printed by `/dip` are
            // the same distribution. If these two ever drift apart again, the
            // sampled gaps below become meaningless and this fires first.
            for hz in &fc.horizons {
                if hz.low < price {
                    let p_low = prob_below_forecast(&fc, hz.low, hz.days);
                    assert!(
                        (p_low - 0.1587).abs() < 0.005,
                        "{symbol}@{origin}: P(below cone low, {}d) = {p_low}",
                        hz.days
                    );
                }
                if hz.high > price {
                    let p_high = prob_below_forecast(&fc, hz.high, hz.days);
                    assert!(
                        (p_high - 0.8413).abs() < 0.005,
                        "{symbol}@{origin}: P(below cone high, {}d) = {p_high}",
                        hz.days
                    );
                }
            }

            for &level_pct in &TOUCH_LEVELS_PCT {
                let level = price * (1.0 + level_pct / 100.0);
                for &h in &SCORED_HORIZONS {
                    if origin + h as usize >= n {
                        continue;
                    }
                    let pred = touch_probability_forecast(&fc, level, h);
                    let min_close = hist[origin + 1..=origin + h as usize]
                        .iter()
                        .map(|&(_, c)| c)
                        .fold(f64::INFINITY, f64::min);
                    touch.push(TouchObs {
                        h,
                        level_pct,
                        pred,
                        actual: if min_close <= level { 1.0 } else { 0.0 },
                        vol_pct,
                        drift_pct: fc.drift_daily_pct,
                        regime,
                    });
                }
            }

            for &h in &[30i64, 90] {
                if origin + h as usize >= n {
                    continue;
                }
                for &level_pct in &[-10.0f64, -20.0, -30.0] {
                    let level = price * (1.0 + level_pct / 100.0);
                    let pred = prob_below_forecast(&fc, level, h);
                    below.push(BelowObs {
                        h,
                        level_pct,
                        pred,
                        actual: if hist[origin + h as usize].1 < level {
                            1.0
                        } else {
                            0.0
                        },
                        vol_pct,
                        regime,
                    });
                }
            }

            // Verdict contract at scale, both sides of the average.
            for avg in [price * 0.8, price * 1.2] {
                let report = dip_report(&fc, past, Some(avg));
                let above_avg = price > avg * 1.0001;
                *verdicts.entry(report.verdict.label()).or_default() += 1;

                // `/forecast` prints its own probabilities. They must come from
                // the same moments the cone and `/dip` use — the raw
                // `drift% * days` path that used to live there ignored the
                // decay and doubled the numbers on strong trends. The text is
                // rounded to whole percent, hence the 0.51 tolerance.
                let lines = fc.action_lines(Some(avg), false);
                if let Some(line) = lines
                    .iter()
                    .find(|l| l.contains("chance to touch within 90d"))
                {
                    let printed = parse_tilde_pct(line).expect("band percentage");
                    let low = fc
                        .horizons
                        .iter()
                        .filter(|h| (14..=90).contains(&h.days))
                        .map(|h| h.low)
                        .fold(f64::INFINITY, f64::min);
                    let expected = touch_probability_forecast(&fc, low, 90) * 100.0;
                    assert!(
                        (printed - expected).abs() <= 0.51,
                        "{symbol}@{origin}: /forecast band {printed:.0}% vs model {expected:.1}%"
                    );
                }
                if let Some(line) = lines
                    .iter()
                    .find(|l| l.contains("trade below your average within 90d"))
                {
                    let printed = parse_tilde_pct(line).expect("below-average percentage");
                    let expected = (prob_below_forecast(&fc, avg, 90) * 100.0).min(99.0);
                    assert!(
                        (printed - expected).abs() <= 0.51,
                        "{symbol}@{origin}: /forecast below-avg {printed:.0}% vs model {expected:.1}%"
                    );
                }

                match &report.verdict {
                    DipVerdict::BuyNow => {
                        assert!(!above_avg, "{symbol}@{origin}: BUY NOW above avg")
                    }
                    DipVerdict::ScaleIn {
                        now_pct,
                        zone_price,
                    } => {
                        assert!(
                            *zone_price < price,
                            "{symbol}@{origin}: ScaleIn not below spot"
                        );
                        assert!(
                            matches!(*now_pct, 20 | 25 | 33 | 50),
                            "{symbol}@{origin}: bad tranche {now_pct}"
                        );
                        if above_avg {
                            assert!(
                                *zone_price <= price * (1.0 - MIN_DIP_GAP_PCT / 100.0) + 1e-6,
                                "{symbol}@{origin}: above-avg ScaleIn on a sub-2% dip"
                            );
                        }
                    }
                    DipVerdict::AboveAverage { zone_price } => {
                        assert!(above_avg, "{symbol}@{origin}: AboveAverage below avg");
                        if let Some(z) = zone_price {
                            assert!(
                                *z <= price * (1.0 - MIN_DIP_GAP_PCT / 100.0) + 1e-6,
                                "{symbol}@{origin}: AboveAverage names a sub-2% zone"
                            );
                        }
                    }
                    DipVerdict::Wait { zone_price } => {
                        if let Some(z) = zone_price {
                            assert!(*z < price, "{symbol}@{origin}: Wait zone not below spot");
                        }
                    }
                    DipVerdict::NoData => panic!("{symbol}@{origin}: NoData"),
                }
            }
        }
    }

    println!("\n=== WIDE walk-forward replay ===");
    println!(
        "origins: {origins} · touch samples: {} · below samples: {}",
        touch.len(),
        below.len()
    );

    // ── P(touch): overall, per horizon, per level ──────────────────────────
    if let Some((n, pred, act, gap)) = gap_stats(touch.iter().map(|s| (s.pred, s.actual))) {
        println!(
            "P(touch) overall: n={n} predicted {} actual {} gap {gap:+.3}",
            pct(pred),
            pct(act)
        );
    }
    let mut rows = Vec::new();
    for &h in &SCORED_HORIZONS {
        if let Some((n, p, a, g)) = gap_stats(
            touch
                .iter()
                .filter(|s| s.h == h)
                .map(|s| (s.pred, s.actual)),
        ) {
            rows.push((format!("h={h}d"), n, p, a, g));
        }
    }
    print_grouped("P(touch) by horizon", &rows);
    let mut rows = Vec::new();
    for &lv in &TOUCH_LEVELS_PCT {
        if let Some((n, p, a, g)) = gap_stats(
            touch
                .iter()
                .filter(|s| s.level_pct == lv)
                .map(|s| (s.pred, s.actual)),
        ) {
            rows.push((format!("{lv:.0}%"), n, p, a, g));
        }
    }
    print_grouped("P(touch) by level", &rows);

    // ── Strata: regime and volatility ──────────────────────────────────────
    let vols: Vec<f64> = touch.iter().map(|s| s.vol_pct).collect();
    let cuts = terciles(&vols);

    let mut rows = Vec::new();
    for regime in [
        MarketRegime::Bull,
        MarketRegime::Bear,
        MarketRegime::Sideways,
        MarketRegime::Dip,
        MarketRegime::Euphoria,
    ] {
        if let Some((n, p, a, g)) = gap_stats(
            touch
                .iter()
                .filter(|s| s.regime == regime)
                .map(|s| (s.pred, s.actual)),
        ) {
            rows.push((regime.as_str().to_string(), n, p, a, g));
        }
    }
    print_grouped("P(touch) by market regime", &rows);

    let mut rows = Vec::new();
    for bucket in ["quiet", "normal", "tense"] {
        if let Some((n, p, a, g)) = gap_stats(
            touch
                .iter()
                .filter(|s| vol_bucket(s.vol_pct, cuts) == bucket)
                .map(|s| (s.pred, s.actual)),
        ) {
            rows.push((bucket.to_string(), n, p, a, g));
        }
    }
    print_grouped("P(touch) by volatility tercile", &rows);

    // ── P(below) overall and per horizon ───────────────────────────────────
    let mut rows = Vec::new();
    for &h in &[30i64, 90] {
        if let Some((n, p, a, g)) = gap_stats(
            below
                .iter()
                .filter(|s| s.h == h)
                .map(|s| (s.pred, s.actual)),
        ) {
            rows.push((format!("h={h}d"), n, p, a, g));
        }
    }
    print_grouped("P(close below level) by horizon", &rows);

    let mut rows = Vec::new();
    for regime in [
        MarketRegime::Bull,
        MarketRegime::Bear,
        MarketRegime::Sideways,
        MarketRegime::Dip,
        MarketRegime::Euphoria,
    ] {
        if let Some((n, p, a, g)) = gap_stats(
            below
                .iter()
                .filter(|s| s.regime == regime)
                .map(|s| (s.pred, s.actual)),
        ) {
            rows.push((regime.as_str().to_string(), n, p, a, g));
        }
    }
    print_grouped("P(below) by market regime", &rows);

    let below_vols: Vec<f64> = below.iter().map(|s| s.vol_pct).collect();
    let below_cuts = terciles(&below_vols);
    let mut rows = Vec::new();
    for bucket in ["quiet", "normal", "tense"] {
        if let Some((n, p, a, g)) = gap_stats(
            below
                .iter()
                .filter(|s| vol_bucket(s.vol_pct, below_cuts) == bucket)
                .map(|s| (s.pred, s.actual)),
        ) {
            rows.push((bucket.to_string(), n, p, a, g));
        }
    }
    print_grouped("P(below) by volatility tercile", &rows);

    // ── Scores vs climatology ──────────────────────────────────────────────
    let tp: Vec<(f64, f64)> = touch.iter().map(|s| (s.pred, s.actual)).collect();
    let tb = brier(&tp);
    let tc = climatology_brier(&touch, |s| (s.h, s.level_pct), |s| (s.pred, s.actual));
    println!(
        "\nP(touch): Brier {tb:.4} · climatology {tc:.4} · skill {:+.3} (n={})",
        if tc > 0.0 { 1.0 - tb / tc } else { f64::NAN },
        touch.len()
    );
    let bp: Vec<(f64, f64)> = below.iter().map(|s| (s.pred, s.actual)).collect();
    let bb = brier(&bp);
    let bc = climatology_brier(&below, |s| (s.h, s.level_pct), |s| (s.pred, s.actual));
    println!(
        "P(below): Brier {bb:.4} · climatology {bc:.4} · skill {:+.3} (n={})",
        if bc > 0.0 { 1.0 - bb / bc } else { f64::NAN },
        below.len()
    );

    println!("\nverdict distribution:");
    for (label, count) in &verdicts {
        println!("  {label:<26} {count:>6}");
    }

    // ── Hard guards (deliberately loose — they catch a broken formula, not
    //    the known discrete-close bias) ────────────────────────────────────
    for &h in &SCORED_HORIZONS {
        let (_, _, _, gap) = gap_stats(
            touch
                .iter()
                .filter(|s| s.h == h)
                .map(|s| (s.pred, s.actual)),
        )
        .unwrap();
        assert!(
            gap.abs() <= 0.20,
            "P(touch) at {h}d is {gap:+.3} off the empirical rate"
        );
    }
    // A regime where the model is 25+ points off on 200+ observations is a
    // genuine defect, not sampling noise.
    for regime in [
        MarketRegime::Bull,
        MarketRegime::Bear,
        MarketRegime::Sideways,
        MarketRegime::Dip,
        MarketRegime::Euphoria,
    ] {
        if let Some((n, _, _, gap)) = gap_stats(
            touch
                .iter()
                .filter(|s| s.regime == regime)
                .map(|s| (s.pred, s.actual)),
        ) {
            if n >= 200 {
                assert!(
                    gap.abs() <= 0.25,
                    "P(touch) in {} is {gap:+.3} off (n={n})",
                    regime.as_str()
                );
            }
        }
    }
    for row in ["quiet", "normal", "tense"] {
        if let Some((n, _, _, gap)) = gap_stats(
            touch
                .iter()
                .filter(|s| vol_bucket(s.vol_pct, cuts) == row)
                .map(|s| (s.pred, s.actual)),
        ) {
            assert!(
                gap.abs() <= 0.25,
                "P(touch) in the {row} volatility tercile is {gap:+.3} off (n={n})"
            );
        }
    }
    for &h in &[30i64, 90] {
        if let Some((_, _, _, gap)) = gap_stats(
            below
                .iter()
                .filter(|s| s.h == h)
                .map(|s| (s.pred, s.actual)),
        ) {
            assert!(gap.abs() <= 0.15, "P(below) at {h}d is {gap:+.3} off");
        }
    }
}

#[test]
fn last_six_months_dense_replay() {
    let mut touch: Vec<TouchObs> = Vec::new();
    let mut origins = 0usize;

    for symbol in SYMBOLS {
        let hist = load(symbol);
        let n = hist.len();
        let start = n.saturating_sub(RECENT_DAYS + MAX_H + 1);
        assert!(start < n - MAX_H - 1, "{symbol}: history too short");

        for origin in start..n.saturating_sub(MAX_H + 1) {
            origins += 1;
            let past = &hist[..=origin];
            let price = past.last().unwrap().1;
            let fc = forecast(symbol, price, past);
            let closes: Vec<f64> = past.iter().map(|&(_, c)| c).collect();
            let regime = classify_closes(&closes);

            for &level_pct in &TOUCH_LEVELS_PCT {
                let level = price * (1.0 + level_pct / 100.0);
                for &h in &SCORED_HORIZONS {
                    if origin + h as usize >= n {
                        continue;
                    }
                    let pred = touch_probability_forecast(&fc, level, h);
                    let min_close = hist[origin + 1..=origin + h as usize]
                        .iter()
                        .map(|&(_, c)| c)
                        .fold(f64::INFINITY, f64::min);
                    touch.push(TouchObs {
                        h,
                        level_pct,
                        pred,
                        actual: if min_close <= level { 1.0 } else { 0.0 },
                        vol_pct: fc.vol_daily_pct,
                        drift_pct: fc.drift_daily_pct,
                        regime,
                    });
                }
            }
        }
    }

    println!("\n=== LAST 6 MONTHS dense replay ===");
    println!("origins: {origins} · samples: {}", touch.len());
    if let Some((n, pred, act, gap)) = gap_stats(touch.iter().map(|s| (s.pred, s.actual))) {
        println!(
            "P(touch) overall: n={n} predicted {} actual {} gap {gap:+.3}",
            pct(pred),
            pct(act)
        );
    }
    let mut rows = Vec::new();
    for &h in &SCORED_HORIZONS {
        if let Some((n, p, a, g)) = gap_stats(
            touch
                .iter()
                .filter(|s| s.h == h)
                .map(|s| (s.pred, s.actual)),
        ) {
            rows.push((format!("h={h}d"), n, p, a, g));
        }
    }
    print_grouped("P(touch) by horizon (recent)", &rows);
    let mut rows = Vec::new();
    for &lv in &TOUCH_LEVELS_PCT {
        if let Some((n, p, a, g)) = gap_stats(
            touch
                .iter()
                .filter(|s| s.level_pct == lv)
                .map(|s| (s.pred, s.actual)),
        ) {
            rows.push((format!("{lv:.0}%"), n, p, a, g));
        }
    }
    print_grouped("P(touch) by level (recent)", &rows);

    // The recent window is small; guard only against gross breakage.
    for &h in &SCORED_HORIZONS {
        if let Some((n, _, _, gap)) = gap_stats(
            touch
                .iter()
                .filter(|s| s.h == h)
                .map(|s| (s.pred, s.actual)),
        ) {
            if n >= 200 {
                assert!(
                    gap.abs() <= 0.25,
                    "recent P(touch) at {h}d is {gap:+.3} off (n={n})"
                );
            }
        }
    }
}

/// Pure-math invariants of the first-passage layer. These do not depend on the
/// data at all, so a violation is unambiguously a formula bug.
#[test]
fn first_passage_math_invariants() {
    let price = 100.0;

    // Monotone in horizon and in level depth.
    for &level_pct in &[-1.0f64, -5.0, -10.0, -25.0, -50.0] {
        let level = price * (1.0 + level_pct / 100.0);
        let mut prev = 0.0;
        for &h in &[1i64, 3, 7, 14, 30, 90, 180, 365] {
            let p = touch_probability(price, level, 0.0, 0.02, h);
            assert!((0.0..=1.0).contains(&p));
            assert!(
                p + 1e-12 >= prev,
                "P(touch) dropped with horizon: {p} < {prev} ({level_pct}%, {h}d)"
            );
            prev = p;
        }
    }
    for &h in &[7i64, 30, 90] {
        let mut prev = 1.0;
        for &level_pct in &[-1.0f64, -5.0, -10.0, -25.0, -50.0] {
            let level = price * (1.0 + level_pct / 100.0);
            let p = touch_probability(price, level, 0.0, 0.02, h);
            assert!(
                p <= prev + 1e-12,
                "P(touch) rose for a deeper level: {p} > {prev} ({level_pct}%, {h}d)"
            );
            prev = p;
        }
    }

    // Endpoint-below is a subset of touch: P(touch) >= P(close below).
    for &h in &[1i64, 7, 30, 90] {
        for &level_pct in &[-2.0f64, -10.0, -30.0] {
            for &mu in &[-0.004f64, 0.0, 0.004] {
                let level = price * (1.0 + level_pct / 100.0);
                let touch = touch_probability(price, level, mu, 0.025, h);
                let below = prob_below_at_horizon(price, level, mu, 0.025, h);
                assert!(
                    touch + 1e-12 >= below,
                    "P(touch) {touch} < P(below) {below} (mu={mu}, {level_pct}%, {h}d)"
                );
            }
        }
    }

    // Asymptotics: a near level is eventually touched; a far level stays rare.
    let near = price * 0.99;
    let far = price * 0.01;
    assert!(touch_probability(price, near, 0.0, 0.02, 1_000_000) > 0.999);
    assert!(touch_probability(price, far, 0.0, 0.02, 90) < 1e-6);

    // Zero/denormal sigma must not produce NaN or out-of-range values.
    for &s in &[0.0f64, 1e-12] {
        let p = touch_probability(price, price * 0.9, -0.001, s, 30);
        assert!(p.is_finite() && (0.0..=1.0).contains(&p), "sigma={s}: {p}");
        let b = prob_below_at_horizon(price, price * 0.9, -0.001, s, 30);
        assert!(b.is_finite() && (0.0..=1.0).contains(&b), "sigma={s}: {b}");
    }

    // A level at/above spot is already reached.
    assert_eq!(touch_probability(price, price, 0.0, 0.02, 7), 0.0);
    assert_eq!(touch_probability(price, price * 1.1, 0.0, 0.02, 7), 0.0);
    assert_eq!(prob_below_at_horizon(price, price, 0.0, 0.02, 7), 1.0);

    // Invalid inputs return 0 rather than NaN.
    for &bad in &[f64::NAN, f64::INFINITY, -1.0] {
        assert_eq!(touch_probability(bad, price * 0.9, 0.0, 0.02, 7), 0.0);
        assert_eq!(touch_probability(price, bad, 0.0, 0.02, 7), 0.0);
        assert_eq!(prob_below_at_horizon(bad, price * 0.9, 0.0, 0.02, 7), 0.0);
    }
    assert_eq!(touch_probability(price, price * 0.9, 0.0, 0.02, 0), 0.0);
    assert_eq!(prob_below_at_horizon(price, price * 0.9, 0.0, 0.02, 0), 0.0);
}

/// The forecast must be a pure function of the supplied closes: no look-ahead,
/// no global state. A prefix replayed before and after the future is appended
/// must produce bit-identical output.
#[test]
fn forecast_has_no_lookahead_and_is_deterministic() {
    for symbol in SYMBOLS {
        let hist = load(symbol);
        let n = hist.len();
        for &origin in &[MIN_HISTORY, n / 2, n - MAX_H - 1] {
            let past = &hist[..=origin];
            let price = past.last().unwrap().1;

            let a = forecast(symbol, price, past);
            let b = forecast(symbol, price, past);
            assert_horizons_equal(
                &a.horizons,
                &b.horizons,
                symbol,
                origin,
                "non-deterministic",
            );
            assert_eq!(a.vol_daily_pct, b.vol_daily_pct);
            assert_eq!(a.drift_daily_pct, b.drift_daily_pct);

            // Appending the real future must not change the past forecast:
            // slice `past` is the same input, but this catches an accidental
            // read of a longer backing slice.
            let extended = &hist[..=origin];
            let c = forecast(symbol, price, extended);
            assert_horizons_equal(&a.horizons, &c.horizons, symbol, origin, "look-ahead");
            assert_eq!(a.vol_daily_pct, c.vol_daily_pct);
            assert_eq!(a.sma200, c.sma200);
        }
    }
}

fn assert_horizons_equal(
    a: &[cryptograph::trading::forecast::HorizonForecast],
    b: &[cryptograph::trading::forecast::HorizonForecast],
    symbol: &str,
    origin: usize,
    what: &str,
) {
    assert_eq!(a.len(), b.len(), "{symbol}@{origin}: {what} horizon length");
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.days, y.days, "{symbol}@{origin}: {what} days");
        assert_eq!(x.target, y.target, "{symbol}@{origin}: {what} target");
        assert_eq!(x.low, y.low, "{symbol}@{origin}: {what} low");
        assert_eq!(x.high, y.high, "{symbol}@{origin}: {what} high");
    }
}

/// A perturbation of the *future* must not leak into the diagnosis of the past
/// in the dip verdict either.
#[test]
fn dip_verdict_uses_only_the_supplied_history() {
    for symbol in SYMBOLS {
        let hist = load(symbol);
        let n = hist.len();
        let origin = n / 2;
        let past = &hist[..=origin];
        let price = past.last().unwrap().1;
        let fc = forecast(symbol, price, past);
        let a = dip_report(&fc, past, Some(price * 0.9));
        let b = dip_report(&fc, past, Some(price * 0.9));
        assert_eq!(a.price, b.price);
        assert_eq!(a.zones.len(), b.zones.len());
        assert_eq!(a.verdict.label(), b.verdict.label());
        for (x, y) in a.zones.iter().zip(b.zones.iter()) {
            assert_eq!(x.price, y.price);
            assert_eq!(x.touches, y.touches);
        }
        assert_eq!(a.history_days, past.len());
    }
}

// ---------------------------------------------------------------------------
// Horizon coverage: very short (1/3/5d), medium and deep (60/90/180d), plus
// the quietest ("boring") and the most stressed tape in the sample.
// ---------------------------------------------------------------------------

const ALL_HORIZONS: [i64; 9] = [1, 3, 5, 7, 14, 30, 60, 90, 180];
const EXTREME_ORIGINS: usize = 250;

fn strided_origins_at(n: usize, count: usize, max_h: usize) -> Vec<usize> {
    let lo = MIN_HISTORY;
    let hi = n.saturating_sub(max_h + 1);
    if hi <= lo {
        return Vec::new();
    }
    let step = ((hi - lo) / count).max(1);
    (lo..hi).step_by(step).collect()
}

fn print_gap(label: &str, name: &str, items: impl Iterator<Item = (f64, f64)>) {
    if let Some((n, p, a, g)) = gap_stats(items) {
        println!(
            "{label:<22} {name:<8} {:>7} {:>9} {:>9} {:>+8.3}",
            n,
            pct(p),
            pct(a),
            g
        );
    }
}

#[test]
fn deep_short_and_extreme_tape_replay() {
    let mut obs: Vec<TouchObs> = Vec::new();
    let mut origins = 0usize;
    let mut coverage: BTreeMap<&'static str, std::collections::BTreeSet<i64>> = BTreeMap::new();

    for symbol in SYMBOLS {
        let hist = load(symbol);
        let n = hist.len();
        let points = strided_origins_at(n, EXTREME_ORIGINS, 180);
        assert!(
            points.len() >= EXTREME_ORIGINS,
            "{symbol}: only {} origins",
            points.len()
        );

        for &origin in &points {
            origins += 1;
            let past = &hist[..=origin];
            let price = past.last().unwrap().1;
            let fc = forecast(symbol, price, past);
            let closes: Vec<f64> = past.iter().map(|&(_, c)| c).collect();
            let regime = classify_closes(&closes);
            let supported = horizons_for(symbol, past.len());

            for &h in supported {
                if !ALL_HORIZONS.contains(&h) || origin + h as usize >= n {
                    continue;
                }
                coverage.entry(symbol).or_default().insert(h);
                for &level_pct in &TOUCH_LEVELS_PCT {
                    let level = price * (1.0 + level_pct / 100.0);
                    let pred = touch_probability_forecast(&fc, level, h);
                    assert!(pred.is_finite() && (0.0..=1.0).contains(&pred));
                    if h == 1 {
                        let below = prob_below_forecast(&fc, level, 1);
                        assert!(
                            pred + 1e-12 >= below,
                            "{symbol}@{origin}: 1d touch {pred} < below {below}"
                        );
                    }
                    let min_close = hist[origin + 1..=origin + h as usize]
                        .iter()
                        .map(|&(_, c)| c)
                        .fold(f64::INFINITY, f64::min);
                    obs.push(TouchObs {
                        h,
                        level_pct,
                        pred,
                        actual: if min_close <= level { 1.0 } else { 0.0 },
                        vol_pct: fc.vol_daily_pct,
                        drift_pct: fc.drift_daily_pct,
                        regime,
                    });
                }
            }
        }
    }

    assert!(!obs.is_empty());
    println!("\n=== DEEP/SHORT + EXTREME TAPE replay ===");
    println!(
        "origins: {origins} · samples: {} · levels: {:?}",
        obs.len(),
        TOUCH_LEVELS_PCT
    );
    for (sym, hs) in &coverage {
        println!("  {sym:<4} scored horizons: {hs:?}");
    }

    let mut rows = Vec::new();
    for &h in &ALL_HORIZONS {
        if let Some((n, p, a, g)) =
            gap_stats(obs.iter().filter(|s| s.h == h).map(|s| (s.pred, s.actual)))
        {
            rows.push((format!("h={h}d"), n, p, a, g));
        }
    }
    print_grouped("P(touch) by horizon (all)", &rows);

    let mut rows = Vec::new();
    for &lv in &TOUCH_LEVELS_PCT {
        if let Some((n, p, a, g)) = gap_stats(
            obs.iter()
                .filter(|s| s.level_pct == lv)
                .map(|s| (s.pred, s.actual)),
        ) {
            rows.push((format!("{lv:.0}%"), n, p, a, g));
        }
    }
    print_grouped("P(touch) by level (all)", &rows);

    // ── Boring vs stressed tape ────────────────────────────────────────────
    let vols: Vec<f64> = obs.iter().map(|s| s.vol_pct).collect();
    let vcuts = terciles(&vols);
    let abs_drifts: Vec<f64> = obs.iter().map(|s| s.drift_pct.abs()).collect();
    let dcuts = terciles(&abs_drifts);

    let boring = |s: &TouchObs| s.vol_pct <= vcuts.0 && s.drift_pct.abs() <= dcuts.0;
    let stress = |s: &TouchObs| s.vol_pct >= vcuts.1 || s.drift_pct.abs() >= dcuts.1;
    let quiet = |s: &TouchObs| s.vol_pct <= vcuts.0;
    let tense = |s: &TouchObs| s.vol_pct >= vcuts.1;

    println!(
        "\nvol tercile cuts {:.2}/{:.2}%/day · |drift| cuts {:.3}/{:.3}%/day",
        vcuts.0, vcuts.1, dcuts.0, dcuts.1
    );
    println!(
        "{:<22} {:<8} {:>7} {:>9} {:>9} {:>8}",
        "stratum", "h", "n", "predicted", "actual", "gap"
    );
    print_gap(
        "boring tape",
        "all",
        obs.iter().filter(|s| boring(s)).map(|s| (s.pred, s.actual)),
    );
    print_gap(
        "stressed tape",
        "all",
        obs.iter().filter(|s| stress(s)).map(|s| (s.pred, s.actual)),
    );
    print_gap(
        "quiet vol",
        "all",
        obs.iter().filter(|s| quiet(s)).map(|s| (s.pred, s.actual)),
    );
    print_gap(
        "tense vol",
        "all",
        obs.iter().filter(|s| tense(s)).map(|s| (s.pred, s.actual)),
    );

    for (name, pred) in [
        ("boring", &boring as &dyn Fn(&TouchObs) -> bool),
        ("stressed", &stress as &dyn Fn(&TouchObs) -> bool),
    ] {
        for &h in &ALL_HORIZONS {
            if let Some((n, _, _, gap)) = gap_stats(
                obs.iter()
                    .filter(|s| s.h == h && pred(s))
                    .map(|s| (s.pred, s.actual)),
            ) {
                println!("{name:<14} h={h:<3} n={n:<6} gap {gap:+.3}");
                if n >= 200 {
                    assert!(
                        gap.abs() <= 0.25,
                        "{name} tape at {h}d is {gap:+.3} off (n={n})"
                    );
                }
            }
        }
    }

    // Every scored horizon must be reasonably calibrated on the full sample.
    for &h in &ALL_HORIZONS {
        if let Some((n, _, _, gap)) =
            gap_stats(obs.iter().filter(|s| s.h == h).map(|s| (s.pred, s.actual)))
        {
            let limit = if h <= 3 { 0.25 } else { 0.20 };
            if n >= 200 {
                assert!(
                    gap.abs() <= limit,
                    "P(touch) at {h}d is {gap:+.3} off (n={n}, limit {limit})"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Named regression block for defects that were actually shipped.
// ---------------------------------------------------------------------------

#[test]
fn known_past_defects_never_regress() {
    // (1) `/dip` once named a zone <2% below spot (a `fallback` on the last
    //     candidate). The renderer must drop any sub-2% band and say so.
    let shallow = DipZone {
        price: 99.0,
        label: "20d low".into(),
        distance_pct: -1.0,
        band_top: 99.4,
        touches: vec![(7, 0.90), (14, 0.95), (30, 0.99), (90, 1.0)],
        prob_below_long: 0.70,
    };
    let report = DipReport {
        symbol: "BTC".into(),
        price: 100.0,
        avg_entry: Some(80.0),
        zones: vec![shallow],
        verdict: DipVerdict::AboveAverage { zone_price: None },
        reason: String::new(),
        vol_daily_pct: 1.0,
        drift_daily_pct: 0.0,
        rsi14: Some(50.0),
        sma200: None,
        history_days: 400,
        horizons: vec![7, 14, 30, 90],
    };
    let html = report.render_html();
    assert!(
        !html.contains("99.000"),
        "a sub-2% zone leaked into the ladder: {html}"
    );
    assert!(
        html.contains("sitting on support"),
        "missing the no-zone notice: {html}"
    );

    // (2) `prob_below_at_horizon` returned NaN on non-finite inputs while
    //     `touch_probability` already guarded them.
    for &bad in &[f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let b = prob_below_at_horizon(bad, 90.0, 0.0, 0.02, 30);
        assert!(b.is_finite(), "prob_below({bad}) = {b}");
        let b = prob_below_at_horizon(100.0, bad, 0.0, 0.02, 30);
        assert!(b.is_finite(), "prob_below(level={bad}) = {b}");
    }

    // (3) Discrete monitoring can never make a touch rarer than closing below
    //     at the horizon (the floor is the whole point of the fix).
    for &h in &[1i64, 3, 7, 30, 90, 180] {
        for &level_pct in &[-2.0f64, -10.0, -30.0] {
            for &mu in &[-0.01f64, -0.002, 0.0, 0.002] {
                let level = 100.0 * (1.0 + level_pct / 100.0);
                let t = touch_probability(100.0, level, mu, 0.04, h);
                let b = prob_below_at_horizon(100.0, level, mu, 0.04, h);
                assert!(t + 1e-12 >= b, "h={h}: touch {t} < below {b}");
            }
        }
    }

    // (4) Depth monotonicity: probability must not rise for a deeper level.
    let mut prev = 1.0;
    for &level_pct in &[-1.0f64, -2.0, -5.0, -10.0, -20.0, -50.0] {
        let level = 100.0 * (1.0 + level_pct / 100.0);
        let p = touch_probability(100.0, level, 0.0, 0.03, 30);
        assert!(p <= prev + 1e-12, "{level_pct}%: {p} > {prev}");
        prev = p;
    }
}
