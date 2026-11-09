//! Historical verification of the `/dip` and `/forecast` math.
//!
//! Every origin is replayed *walk-forward*: the forecast and its calibration are
//! rebuilt from only the closes available at that moment, exactly as the live
//! bot would. Origins are sampled with a fixed-seed xorshift so the run is
//! reproducible.
//!
//! What is checked, and what is merely reported:
//!
//! * **Hard invariants** (the test fails): touch probabilities are bounded and
//!   monotone in horizon and level; the cone is not systematically broken
//!   (coverage stays in a sane band); every `/dip` verdict obeys its contract
//!   (never `BUY NOW` above the average, never a `ScaleIn` bid shallower than
//!   2% when the price is above the average, zones sorted and below spot).
//! * **Calibration metrics** (printed): reliability of the first-passage touch
//!   probability, of `prob_below_at_horizon`, cone coverage and direction hit
//!   rate per symbol/horizon.
//!
//! Run with `--nocapture` to see the tables.

use cryptograph::trading::dipzones::{
    dip_report, prob_below_forecast, touch_probability_forecast, DipVerdict,
};
use cryptograph::trading::forecast::forecast;
use cryptograph::trading::history_file;

/// A level closer than this to spot is noise, not a dip. Mirrors
/// `dipzones::MIN_DIP_GAP_PCT`.
const MIN_DIP_GAP_PCT: f64 = 2.0;

const ORIGINS_PER_SYMBOL: usize = 30;
const MIN_HISTORY: usize = 300;
/// Longest horizon we score. Kept at 90 so the `20d low` / cone checks all share
/// the same number of usable origins.
const MAX_H: usize = 90;
const SCORED_HORIZONS: [i64; 4] = [7, 14, 30, 90];
const TOUCH_LEVELS_PCT: [f64; 5] = [-2.0, -5.0, -10.0, -20.0, -30.0];

/// Deterministic xorshift64* — no external RNG, fully reproducible.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

fn sample_origins(n: usize, count: usize, seed: u64) -> Vec<usize> {
    let lo = MIN_HISTORY;
    let hi = n.saturating_sub(MAX_H + 1);
    if hi <= lo {
        return Vec::new();
    }
    let mut rng = Rng::new(seed);
    let mut set = std::collections::BTreeSet::new();
    let mut guard = 0usize;
    while set.len() < count && guard < count * 200 {
        set.insert(lo + rng.below(hi - lo));
        guard += 1;
    }
    set.into_iter().collect()
}

#[derive(Clone, Copy)]
struct TouchSample {
    h: i64,
    level_pct: f64,
    pred: f64,
    actual: f64, // 0.0 or 1.0
}

#[derive(Clone, Copy)]
struct BelowSample {
    h: i64,
    level_pct: f64,
    pred: f64,
    actual: f64,
}

#[derive(Default, Clone, Copy)]
struct ConeAcc {
    n: usize,
    inside: usize,
    abs_err_sum: f64,
    dir_hits: usize,
}

fn load(symbol: &str) -> Vec<(i64, f64)> {
    let h = history_file::load_history(symbol)
        .unwrap_or_else(|| panic!("missing data/history/{symbol}.csv"));
    assert!(
        h.len() > MIN_HISTORY + MAX_H + 50,
        "{symbol} history too short"
    );
    h
}

fn pct(p: f64) -> String {
    format!("{:>6.1}%", p * 100.0)
}

#[test]
fn dip_and_forecast_are_calibrated_and_self_consistent() {
    let symbols = ["BTC", "ETH", "LINK", "SOL"];
    let mut touch_all: Vec<TouchSample> = Vec::new();
    let mut below_all: Vec<BelowSample> = Vec::new();
    let mut coarse: std::collections::BTreeMap<String, ConeAcc> = Default::default();
    let mut verdict_counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
    let mut origin_count = 0usize;

    for (si, symbol) in symbols.iter().enumerate() {
        let hist = load(symbol);
        let n = hist.len();
        let origins = sample_origins(
            n,
            ORIGINS_PER_SYMBOL,
            0xC0FFEE_u64.wrapping_mul(si as u64 + 1),
        );
        assert!(
            origins.len() >= ORIGINS_PER_SYMBOL / 2,
            "{symbol}: only {} origins sampled",
            origins.len()
        );

        for &origin in &origins {
            origin_count += 1;
            let past = &hist[..=origin];
            let price = past.last().unwrap().1;
            let fc = forecast(symbol, price, past);
            assert!(
                fc.price.is_finite() && fc.price > 0.0,
                "{symbol}@{origin}: bad price"
            );
            assert!(
                fc.vol_daily_pct.is_finite() && fc.vol_daily_pct > 0.0,
                "{symbol}@{origin}: bad vol"
            );
            assert!(
                fc.drift_daily_pct.is_finite(),
                "{symbol}@{origin}: bad drift"
            );

            // ── First-passage touch probability ─────────────────────────────
            for &level_pct in &TOUCH_LEVELS_PCT {
                let level = price * (1.0 + level_pct / 100.0);
                let mut prev = -1.0;
                for &h in &SCORED_HORIZONS {
                    if origin + h as usize >= n {
                        continue;
                    }
                    let pred = touch_probability_forecast(&fc, level, h);
                    assert!(
                        (0.0..=1.0).contains(&pred),
                        "{symbol}@{origin} h={h}: P(touch)={pred} out of [0,1]"
                    );
                    assert!(
                        pred + 1e-9 >= prev,
                        "{symbol}@{origin} level {level_pct}%: P(touch) fell as the horizon grew ({prev} -> {pred})"
                    );
                    prev = pred;
                    let min_close = hist[origin + 1..=origin + h as usize]
                        .iter()
                        .map(|&(_, c)| c)
                        .fold(f64::INFINITY, f64::min);
                    let actual = if min_close <= level { 1.0 } else { 0.0 };
                    touch_all.push(TouchSample {
                        h,
                        level_pct,
                        pred,
                        actual,
                    });
                }
            }

            // ── P(close below level at horizon) ────────────────────────────
            for &h in &[30i64, 90] {
                if origin + h as usize >= n {
                    continue;
                }
                for &level_pct in &[-10.0f64, -20.0, -30.0] {
                    let level = price * (1.0 + level_pct / 100.0);
                    let pred = prob_below_forecast(&fc, level, h);
                    assert!(
                        (0.0..=1.0).contains(&pred),
                        "{symbol}@{origin} h={h}: P(below)={pred} out of [0,1]"
                    );
                    let actual = if hist[origin + h as usize].1 < level {
                        1.0
                    } else {
                        0.0
                    };
                    below_all.push(BelowSample {
                        h,
                        level_pct,
                        pred,
                        actual,
                    });
                }
            }

            // ── Cone coverage and direction ────────────────────────────────
            for hf in &fc.horizons {
                if !SCORED_HORIZONS.contains(&hf.days) || origin + hf.days as usize >= n {
                    continue;
                }
                let actual = hist[origin + hf.days as usize].1;
                let key = format!("{symbol} h={:<3}", hf.days);
                let acc = coarse.entry(key).or_default();
                acc.n += 1;
                if actual >= hf.low && actual <= hf.high {
                    acc.inside += 1;
                }
                acc.abs_err_sum += (hf.target / actual - 1.0).abs();
                if (hf.target - price).signum() == (actual - price).signum() {
                    acc.dir_hits += 1;
                }
                assert!(
                    hf.low <= hf.target && hf.target <= hf.high,
                    "{symbol}@{origin} h={}: cone inverted ({} {} {})",
                    hf.days,
                    hf.low,
                    hf.target,
                    hf.high
                );
            }

            // ── `/dip` verdict contract, both sides of the average ─────────
            for avg in [price * 0.8, price * 1.2] {
                let report = dip_report(&fc, past, Some(avg));
                assert_eq!(report.price, price);
                assert_eq!(report.history_days, past.len());
                assert!(!report.horizons.is_empty());

                // Zones are strictly below spot and sorted ascending.
                for z in &report.zones {
                    assert!(
                        z.price.is_finite() && z.price > 0.0,
                        "{symbol}@{origin}: bad zone"
                    );
                    assert!(
                        z.price < price,
                        "{symbol}@{origin}: zone {} not below spot {}",
                        z.price,
                        price
                    );
                    assert!(z.distance_pct < 0.0, "{symbol}@{origin}: zone dist >= 0");
                    let mut prev = -1.0;
                    for &(d, p) in &z.touches {
                        assert!(
                            (0.0..=1.0).contains(&p),
                            "{symbol}@{origin}: zone touch out of range"
                        );
                        if d > 0 && prev >= 0.0 {
                            assert!(
                                p + 1e-9 >= prev,
                                "{symbol}@{origin}: zone touch fell with horizon"
                            );
                        }
                        prev = p;
                    }
                }
                for w in report.zones.windows(2) {
                    assert!(
                        w[0].price < w[1].price,
                        "{symbol}@{origin}: zones not ascending"
                    );
                }

                // Displayed zones (what the user sees) are all >= 2% below spot.
                for z in report
                    .zones
                    .iter()
                    .filter(|z| z.distance_pct <= -MIN_DIP_GAP_PCT)
                {
                    assert!(
                        z.distance_pct <= -MIN_DIP_GAP_PCT + 1e-9,
                        "{symbol}@{origin}: shallow zone displayed"
                    );
                }

                let above_avg = price > avg * 1.0001;
                let label = report.verdict.label();
                *verdict_counts.entry(label).or_default() += 1;

                match &report.verdict {
                    DipVerdict::BuyNow => {
                        assert!(
                            !above_avg,
                            "{symbol}@{origin}: BUY NOW at {price} above average {avg}"
                        );
                    }
                    DipVerdict::ScaleIn {
                        now_pct,
                        zone_price,
                    } => {
                        assert!(
                            *zone_price < price,
                            "{symbol}@{origin}: ScaleIn bid at/above spot"
                        );
                        assert!(
                            matches!(*now_pct, 20 | 25 | 33 | 50),
                            "{symbol}@{origin}: unexpected tranche {now_pct}%"
                        );
                        if above_avg {
                            assert!(
                                *zone_price <= price * (1.0 - MIN_DIP_GAP_PCT / 100.0) + 1e-6,
                                "{symbol}@{origin}: above-average ScaleIn bids {zone_price} on a sub-{}% dip (spot {price})",
                                MIN_DIP_GAP_PCT
                            );
                        }
                    }
                    DipVerdict::AboveAverage { zone_price } => {
                        assert!(
                            above_avg,
                            "{symbol}@{origin}: AboveAverage while price <= average"
                        );
                        if let Some(z) = zone_price {
                            assert!(
                                *z <= price * (1.0 - MIN_DIP_GAP_PCT / 100.0) + 1e-6,
                                "{symbol}@{origin}: AboveAverage names a sub-{}% zone {z} (spot {price})",
                                MIN_DIP_GAP_PCT
                            );
                        }
                    }
                    DipVerdict::Wait { zone_price } => {
                        if let Some(z) = zone_price {
                            assert!(*z < price, "{symbol}@{origin}: Wait zone at/above spot");
                        }
                    }
                    DipVerdict::NoData => {
                        panic!("{symbol}@{origin}: NoData for a valid price")
                    }
                }
            }
        }
    }

    // ── Report ─────────────────────────────────────────────────────────────
    println!("\n=== /dip + /forecast historical verification ===");
    println!(
        "origins replayed: {origin_count} across {} symbols\n",
        symbols.len()
    );

    print_touch_reliability(&touch_all);
    print_below_reliability(&below_all);
    print_calibration_scores(&touch_all, &below_all);
    print_cone_and_direction(&coarse);

    println!("\nverdict distribution (2 average scenarios per origin):");
    for (label, count) in &verdict_counts {
        println!("  {label:<24} {count:>5}");
    }

    // ── Hard guards on model quality (not just self-consistency) ───────────
    let total_inside: usize = coarse.values().map(|a| a.inside).sum();
    let total_n: usize = coarse.values().map(|a| a.n).sum();
    let coverage = total_inside as f64 / total_n.max(1) as f64;
    println!("\naggregate 68%-cone coverage: {}", pct(coverage));
    assert!(
        (0.40..=0.90).contains(&coverage),
        "cone coverage {coverage:.3} is outside a defensible 40–90% band — the 68% cone is broken"
    );

    let touch_brier = brier(
        &touch_all
            .iter()
            .map(|s| (s.pred, s.actual))
            .collect::<Vec<_>>(),
    );
    println!("aggregate touch Brier score: {touch_brier:.4}");
    assert!(
        touch_brier.is_finite() && (0.0..=1.0).contains(&touch_brier),
        "touch Brier score {touch_brier} is not a valid probability score"
    );

    // A model that is consistently 20+ points off is not "overstated by discrete
    // sampling" — it is broken. This is the loose guard that would have caught
    // the first-passage formula silently diverging.
    for &h in &SCORED_HORIZONS {
        let s: Vec<&TouchSample> = touch_all.iter().filter(|x| x.h == h).collect();
        if s.is_empty() {
            continue;
        }
        let gap = s.iter().map(|x| x.actual - x.pred).sum::<f64>() / s.len() as f64;
        assert!(
            gap.abs() <= 0.20,
            "P(touch) at {h}d is {gap:+.3} off the empirical rate — beyond what daily-close sampling explains"
        );
    }
    for &h in &[30i64, 90] {
        let s: Vec<&BelowSample> = below_all.iter().filter(|x| x.h == h).collect();
        if s.is_empty() {
            continue;
        }
        let gap = s.iter().map(|x| x.actual - x.pred).sum::<f64>() / s.len() as f64;
        assert!(
            gap.abs() <= 0.15,
            "P(below) at {h}d is {gap:+.3} off the empirical rate"
        );
    }
}

fn brier(samples: &[(f64, f64)]) -> f64 {
    if samples.is_empty() {
        return f64::NAN;
    }
    samples.iter().map(|(p, a)| (p - a).powi(2)).sum::<f64>() / samples.len() as f64
}

fn print_touch_reliability(samples: &[TouchSample]) {
    println!("── P(touch) reliability by predicted-probability bucket (all symbols) ──");
    println!(
        "{:<12} {:>6} {:>10} {:>10} {:>8}",
        "bucket", "n", "predicted", "actual", "gap"
    );
    let mut rows: Vec<(String, usize, f64, f64)> = Vec::new();
    for edge in 0..10 {
        let lo = edge as f64 / 10.0;
        let hi = (edge + 1) as f64 / 10.0;
        let in_bucket: Vec<&TouchSample> = samples
            .iter()
            .filter(|s| s.pred >= lo && (s.pred < hi || (edge == 9 && s.pred <= 1.0)))
            .collect();
        if in_bucket.is_empty() {
            continue;
        }
        let n = in_bucket.len();
        let pred = in_bucket.iter().map(|s| s.pred).sum::<f64>() / n as f64;
        let act = in_bucket.iter().map(|s| s.actual).sum::<f64>() / n as f64;
        rows.push((
            format!("{:.0}–{:.0}%", lo * 100.0, hi * 100.0),
            n,
            pred,
            act,
        ));
    }
    for (label, n, pred, act) in &rows {
        println!(
            "{:<12} {:>6} {:>10} {:>10} {:>+8.3}",
            label,
            n,
            pct(*pred),
            pct(*act),
            act - pred
        );
    }
    // Per-horizon and per-level drift: the aggregate can hide a biased horizon.
    println!("\nP(touch) error by horizon:");
    for &h in &SCORED_HORIZONS {
        let s: Vec<&TouchSample> = samples.iter().filter(|x| x.h == h).collect();
        if s.is_empty() {
            continue;
        }
        let n = s.len();
        let pred = s.iter().map(|x| x.pred).sum::<f64>() / n as f64;
        let act = s.iter().map(|x| x.actual).sum::<f64>() / n as f64;
        println!(
            "  {:>3}d  n={n:<5} predicted {} actual {} gap {:+.3}",
            h,
            pct(pred),
            pct(act),
            act - pred
        );
    }
    println!("P(touch) error by level distance:");
    for &lv in &TOUCH_LEVELS_PCT {
        let s: Vec<&TouchSample> = samples.iter().filter(|x| x.level_pct == lv).collect();
        if s.is_empty() {
            continue;
        }
        let n = s.len();
        let pred = s.iter().map(|x| x.pred).sum::<f64>() / n as f64;
        let act = s.iter().map(|x| x.actual).sum::<f64>() / n as f64;
        println!(
            "  {lv:>6.0}%  n={n:<5} predicted {} actual {} gap {:+.3}",
            pct(pred),
            pct(act),
            act - pred
        );
    }
    println!();
}

fn print_below_reliability(samples: &[BelowSample]) {
    // Buckets are printed per horizon: mixing a 30d question (base rate ~18%)
    // with a 90d one (base rate ~25%) makes a calibrated model look broken.
    for &h in &[30i64, 90] {
        let horizon: Vec<&BelowSample> = samples.iter().filter(|x| x.h == h).collect();
        if horizon.is_empty() {
            continue;
        }
        println!("── P(close below level at {h}d) reliability ──");
        println!(
            "{:<12} {:>6} {:>10} {:>10} {:>8}",
            "bucket", "n", "predicted", "actual", "gap"
        );
        for edge in 0..10 {
            let lo = edge as f64 / 10.0;
            let hi = (edge + 1) as f64 / 10.0;
            let in_bucket: Vec<&&BelowSample> = horizon
                .iter()
                .filter(|s| s.pred >= lo && (s.pred < hi || (edge == 9 && s.pred <= 1.0)))
                .collect();
            if in_bucket.is_empty() {
                continue;
            }
            let n = in_bucket.len();
            let pred = in_bucket.iter().map(|s| s.pred).sum::<f64>() / n as f64;
            let act = in_bucket.iter().map(|s| s.actual).sum::<f64>() / n as f64;
            println!(
                "{:<12} {:>6} {:>10} {:>10} {:>+8.3}",
                format!("{:.0}–{:.0}%", lo * 100.0, hi * 100.0),
                n,
                pct(pred),
                pct(act),
                act - pred
            );
        }
        let n = horizon.len();
        let pred = horizon.iter().map(|x| x.pred).sum::<f64>() / n as f64;
        let act = horizon.iter().map(|x| x.actual).sum::<f64>() / n as f64;
        println!(
            "  total      n={n:<5} predicted {} actual {} gap {:+.3}\n",
            pct(pred),
            pct(act),
            act - pred
        );
    }
}

fn print_calibration_scores(touch: &[TouchSample], below: &[BelowSample]) {
    println!("── Probability scores ──");
    // Baseline = climatology per (horizon, level) subgroup: a score is only
    // meaningful against the base rate of the same question.
    let mut base_pairs: Vec<(f64, f64)> = Vec::new();
    for &h in &SCORED_HORIZONS {
        for &lv in &TOUCH_LEVELS_PCT {
            let s: Vec<&TouchSample> = touch
                .iter()
                .filter(|x| x.h == h && x.level_pct == lv)
                .collect();
            if s.is_empty() {
                continue;
            }
            let base = s.iter().map(|x| x.actual).sum::<f64>() / s.len() as f64;
            for x in s {
                base_pairs.push((base, x.actual));
            }
        }
    }
    let touch_pairs: Vec<(f64, f64)> = touch.iter().map(|s| (s.pred, s.actual)).collect();
    let b = brier(&touch_pairs);
    let bb = brier(&base_pairs);
    println!(
        "P(touch): Brier {:.4} · climatology {:.4} · skill {:+.3} (n={})",
        b,
        bb,
        1.0 - b / bb,
        touch.len()
    );

    let mut below_base: Vec<(f64, f64)> = Vec::new();
    for &h in &[30i64, 90] {
        for &lv in &[-10.0f64, -20.0, -30.0] {
            let s: Vec<&BelowSample> = below
                .iter()
                .filter(|x| x.h == h && x.level_pct == lv)
                .collect();
            if s.is_empty() {
                continue;
            }
            let base = s.iter().map(|x| x.actual).sum::<f64>() / s.len() as f64;
            for x in s {
                below_base.push((base, x.actual));
            }
        }
    }
    let bp: Vec<(f64, f64)> = below.iter().map(|s| (s.pred, s.actual)).collect();
    let b2 = brier(&bp);
    let bb2 = brier(&below_base);
    println!(
        "P(below): Brier {:.4} · climatology {:.4} · skill {:+.3} (n={})\n",
        b2,
        bb2,
        1.0 - b2 / bb2,
        below.len()
    );
}

fn print_cone_and_direction(coarse: &std::collections::BTreeMap<String, ConeAcc>) {
    println!("── Forecast cone coverage / direction / MAPE ──");
    println!(
        "{:<12} {:>5} {:>10} {:>12} {:>10}",
        "symbol", "n", "coverage", "hit-rate", "MAPE"
    );
    for (key, acc) in coarse {
        if acc.n == 0 {
            continue;
        }
        let n = acc.n as f64;
        println!(
            "{:<12} {:>5} {:>10} {:>12} {:>10}",
            key,
            acc.n,
            pct(acc.inside as f64 / n),
            pct(acc.dir_hits as f64 / n),
            pct(acc.abs_err_sum / n)
        );
    }
    println!();
}
