//! Diagnostic: does the directional forecast ever beat a coin flip?
//!
//! This is not a pass/fail gate. It walks every origin forward, records the
//! model's own `prob_up` and its point-target direction, and prints:
//!
//! * hit rate of the point direction with a Wilson 95% interval,
//! * Brier score of `prob_up` versus the base-rate (climatology) score,
//! * tail calibration (when the model says 60–100% up, how often is it up?).
//!
//! Run with `cargo test --release --test direction_edge_diagnostic -- --nocapture`.

use cryptograph::trading::forecast::{forecast, horizons_for};
use cryptograph::trading::history_file;

const SYMBOLS: [&str; 4] = ["BTC", "ETH", "LINK", "SOL"];
const WARMUP: usize = 300;

struct Acc {
    n: usize,
    hits: usize,
    brier: f64,
    base: f64,
    // prob_up buckets: (count, ups)
    buckets: Vec<(usize, usize)>,
}

impl Acc {
    fn new() -> Self {
        Acc {
            n: 0,
            hits: 0,
            brier: 0.0,
            base: 0.0,
            buckets: vec![(0, 0); 5],
        }
    }

    fn push(&mut self, prob_up: f64, pred_up: bool, actual_up: bool) {
        self.n += 1;
        if pred_up == actual_up {
            self.hits += 1;
        }
        let y = if actual_up { 1.0 } else { 0.0 };
        self.brier += (prob_up - y).powi(2);
        self.base += y;
        let b = ((prob_up - 0.5) * 10.0).floor().clamp(0.0, 4.0) as usize;
        self.buckets[b].0 += 1;
        if actual_up {
            self.buckets[b].1 += 1;
        }
    }
}

/// Independent-sample view: advance the origin by `h` each step so the
/// forward windows do not overlap. Overlapping windows share their future
/// path, which makes the Wilson interval above far too narrow — this variant
/// is the one whose intervals can be trusted at face value.
#[test]
fn non_overlapping_direction_diagnostic() {
    println!("\n# non-overlapping origins (stride = horizon) — independent-ish samples");
    for symbol in SYMBOLS {
        let hist = history_file::load_history(symbol)
            .unwrap_or_else(|| panic!("missing data/history/{symbol}.csv"));
        let n = hist.len();
        let horizons = horizons_for(symbol, n);
        let max_h = *horizons.iter().max().unwrap() as usize;
        if n <= WARMUP + max_h + 2 {
            continue;
        }

        println!("\n=== {symbol} · non-overlapping ({n} closes) ===");
        println!(
            "{:>6} {:>6} {:>10} {:>16} {:>9} {:>9} {:>9}",
            "h", "n", "dir hit", "wilson 95% CI", "brier", "base", "skill"
        );
        for &days in horizons {
            let mut acc = Acc::new();
            let mut origin = WARMUP;
            while origin + (days as usize) < n {
                let price = hist[origin].1;
                let past = &hist[..=origin];
                let fc = forecast(symbol, price, past);
                if let Some(hz) = fc.horizons.iter().find(|h| h.days == days) {
                    let actual = hist[origin + days as usize].1;
                    acc.push(hz.prob_up, hz.target > price, actual > price);
                }
                origin += days as usize;
            }
            if acc.n < 5 {
                continue;
            }
            let (lo, hi) = wilson(acc.hits, acc.n);
            let base = acc.base / acc.n as f64;
            let skill = 1.0 - (acc.brier / acc.n as f64) / (base * (1.0 - base));
            let star = if lo > 0.5 || hi < 0.5 { " *" } else { "" };
            println!(
                "{:>6} {:>6} {:>9.1}% {:>6.1}–{:>6.1}% {:>9.4} {:>8.1}% {:>+8.1}%{}",
                days,
                acc.n,
                acc.hits as f64 / acc.n as f64 * 100.0,
                lo * 100.0,
                hi * 100.0,
                acc.brier / acc.n as f64,
                base * 100.0,
                skill * 100.0,
                star
            );
        }
    }
}

/// What the calibration actually fitted for direction probability.
#[test]
fn fitted_probability_model() {
    use cryptograph::trading::forecast::calibrate;
    println!("\n# fitted direction-probability model");
    println!(
        "{:>6} {:>8} {:>10} {:>10} {:>12} {:>12}",
        "sym", "origins", "base", "drift k", "dir hit", "model Brier"
    );
    for symbol in SYMBOLS {
        let hist = history_file::load_history(symbol)
            .unwrap_or_else(|| panic!("missing data/history/{symbol}.csv"));
        let cal = calibrate(symbol, &hist);
        let hits: u64 = cal.accuracy.iter().map(|a| a.hits as u64).sum();
        let samples: u64 = cal.accuracy.iter().map(|a| a.samples as u64).sum();
        let hit_rate = if samples > 0 {
            hits as f64 / samples as f64 * 100.0
        } else {
            f64::NAN
        };
        println!(
            "{:>6} {:>8} {:>9.3} {:>10.2} {:>11.1}% {:>12}",
            symbol, cal.samples, cal.prob_base, cal.prob_drift_scale, hit_rate, "vs base rate"
        );
    }
}

/// Wilson score interval for a binomial proportion.
fn wilson(hits: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let z = 1.959_963_984_540_054;
    let p = hits as f64 / n as f64;
    let z2 = z * z;
    let denom = 1.0 + z2 / n as f64;
    let centre = (p + z2 / (2.0 * n as f64)) / denom;
    let half = z * ((p * (1.0 - p) / n as f64) + z2 / (4.0 * n as f64 * n as f64)).sqrt() / denom;
    ((centre - half).max(0.0), (centre + half).min(1.0))
}

#[test]
fn direction_and_prob_up_edge_diagnostic() {
    for symbol in SYMBOLS {
        let hist = history_file::load_history(symbol)
            .unwrap_or_else(|| panic!("missing data/history/{symbol}.csv"));
        let n = hist.len();
        let max_h = *horizons_for(symbol, n).iter().max().unwrap() as usize;
        if n <= WARMUP + max_h + 2 {
            continue;
        }

        let mut accs: Vec<(i64, Acc)> = Vec::new();
        for &days in horizons_for(symbol, n) {
            accs.push((days, Acc::new()));
        }

        for origin in WARMUP..(n - max_h) {
            let price = hist[origin].1;
            let past = &hist[..=origin];
            let fc = forecast(symbol, price, past);
            for (days, acc) in accs.iter_mut() {
                let j = origin + *days as usize;
                if j >= n {
                    continue;
                }
                let actual = hist[j].1;
                // `forecast` trims horizons to a quarter of the record it was
                // given, so early origins may lack the longer ones.
                let Some(hz) = fc.horizons.iter().find(|h| h.days == *days) else {
                    continue;
                };
                acc.push(hz.prob_up, hz.target > price, actual > price);
            }
        }

        println!("\n=== {symbol} · all origins ({n} closes) ===");
        println!(
            "{:>6} {:>6} {:>10} {:>16} {:>9} {:>9} {:>9}",
            "h", "n", "dir hit", "wilson 95% CI", "brier", "base", "skill"
        );
        for (days, a) in &accs {
            if a.n == 0 {
                continue;
            }
            let (lo, hi) = wilson(a.hits, a.n);
            let base = a.base / a.n as f64;
            let base_brier = base * (1.0 - base);
            let skill = 1.0 - (a.brier / a.n as f64) / base_brier;
            let star = if lo > 0.5 || hi < 0.5 { " *" } else { "" };
            println!(
                "{:>6} {:>6} {:>9.1}% {:>6.1}–{:>6.1}% {:>9.4} {:>8.1}% {:>+8.1}%{}",
                days,
                a.n,
                a.hits as f64 / a.n as f64 * 100.0,
                lo * 100.0,
                hi * 100.0,
                a.brier / a.n as f64,
                base * 100.0,
                skill * 100.0,
                star
            );
            print!("        P(up) bucket [50–60):");
            for (i, (cnt, ups)) in a.buckets.iter().enumerate() {
                if *cnt > 0 {
                    print!(
                        " {}-{}%: n={} up={:.0}%",
                        50 + i * 10,
                        60 + i * 10,
                        cnt,
                        *ups as f64 / *cnt as f64 * 100.0
                    );
                }
            }
            println!();
        }
    }
}
