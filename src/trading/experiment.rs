//! Livepatch experiment engine: replay configs over windows × capitals with a
//! buy-and-hold benchmark, so every change can be judged "better than the last
//! experiment" without a full retrain.
//!
//! Why this exists: the full-grid `strategy train` sweep takes ~10 minutes and
//! only answers "which config is best on 2019–2026". The experiment loop
//! answers the *livepatch* question — "did this one tweak (margin, take-profit,
//! trend filter, capital size) actually help?" — in seconds, across different
//! time windows and different virtual capitals, and it always reports the
//! buy-and-hold comparison so we never fool ourselves with absolute PnL.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};

use anyhow::{bail, Result};
use chrono::NaiveDate;

use super::backtest::{
    date_to_ms, min_window_candles, run_backtest_inner_with_store, windows, BacktestConfig, DAY_MS,
};
use super::engine::EngineConfig;
use super::store::{NewExperiment, TradingMode, TradingStore};
use super::strategies::config_fingerprint;

/// One livepatch sweep: configs × symbols × windows × capitals.
#[derive(Debug, Clone)]
pub struct ExperimentConfig {
    pub label: String,
    pub symbols: Vec<String>,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub window_days: i64,
    pub step_days: i64,
    pub capitals: Vec<f64>,
    pub configs: Vec<EngineConfig>,
    pub threads: usize,
}

impl ExperimentConfig {
    pub fn validate(&self) -> Result<()> {
        if self.symbols.is_empty() {
            bail!("at least one symbol required");
        }
        if self.to <= self.from {
            bail!("to date must be after from date");
        }
        if self.window_days <= 0 || self.step_days <= 0 {
            bail!("window_days and step_days must be > 0");
        }
        if self.capitals.is_empty() {
            bail!("at least one capital required");
        }
        for c in &self.capitals {
            if !c.is_finite() || *c <= 0.0 {
                bail!("capitals must be finite and > 0");
            }
        }
        if self.configs.is_empty() {
            bail!("no configs to test");
        }
        if self.threads == 0 {
            bail!("threads must be >= 1");
        }
        Ok(())
    }
}

/// Aggregated outcome of one config across all windows × capitals, plus the
/// buy-and-hold benchmark comparison.
#[derive(Debug, Clone)]
pub struct ExperimentResult {
    pub config_name: String,
    pub runs: usize,
    pub score: f64,
    pub avg_pnl_pct: f64,
    pub avg_max_dd_pct: f64,
    pub win_rate: f64,
    pub profitable_ratio: f64,
    pub cold_ratio: f64,
    /// Windows where the config's average PnL (across capitals) beat B&H.
    pub windows_beaten_bh: usize,
    pub windows_total: usize,
    /// Average (config PnL − B&H) per window, in percentage points.
    pub avg_pnl_vs_bh: f64,
}

#[derive(Debug, Clone, Default)]
struct RunStats {
    runs: usize,
    pnl_sum: f64,
    dd_sum: f64,
    wins: i64,
    losses: i64,
    cold_lots: usize,
    total_lots: usize,
    profitable_runs: usize,
}

/// Sweep `cfg.configs` over rolling windows × capitals per symbol, score each
/// config with the same composite formula as `train()`, benchmark every window
/// against buy-and-hold, and persist the outcomes so the next run can be
/// compared ("is this change better than the last one?").
///
/// History is fetched once per symbol via the injected `fetch` and sliced per
/// window, exactly like `train()`. Intermediate runs are in-memory only.
pub fn run_experiment(
    store: &TradingStore,
    cfg: &ExperimentConfig,
    fetch: &dyn Fn(&str, NaiveDate, NaiveDate) -> Result<Vec<(i64, f64)>>,
) -> Result<Vec<ExperimentResult>> {
    cfg.validate()?;

    // Warm-up prefix: the ATH guard needs `ath_lookback_days`, the SMA50 trend
    // filter/exit need 50 days — take the max so both are live from the first
    // trade tick of every window.
    let warmup_days = cfg
        .configs
        .iter()
        .map(|c| c.ath_lookback_days.max(50))
        .max()
        .unwrap_or(50);
    let fetch_from = cfg.from - chrono::Duration::days(warmup_days);

    let mut histories: HashMap<String, Vec<(i64, f64)>> = HashMap::new();
    for sym in &cfg.symbols {
        println!("Fetching {sym} history {}..{} ...", fetch_from, cfg.to);
        histories.insert(sym.clone(), fetch(sym, fetch_from, cfg.to)?);
        println!("  {} candles", histories[sym].len());
    }

    // Pre-slice windows once per symbol. Each slice carries its warm-up prefix
    // and the buy-and-hold return measured on the trade window only (warm-up
    // ticks never trade, so B&H must not include them).
    let window_list = windows(cfg.from, cfg.to, cfg.window_days, cfg.step_days);
    let mut slices: HashMap<String, Vec<(NaiveDate, NaiveDate, i64, Vec<(i64, f64)>, f64)>> =
        HashMap::new();
    for sym in &cfg.symbols {
        let history = &histories[sym];
        let mut sym_windows = Vec::new();
        for (start, end) in &window_list {
            let warmup_ms = date_to_ms(*start);
            let start_ms = warmup_ms - warmup_days * DAY_MS;
            let end_ms = date_to_ms(*end);
            let window: Vec<(i64, f64)> = history
                .iter()
                .filter(|(ts, _)| *ts >= start_ms && *ts < end_ms)
                .copied()
                .collect();
            let trade_len = window.iter().filter(|(ts, _)| *ts >= warmup_ms).count();
            if trade_len >= min_window_candles(cfg.window_days) {
                // B&H: buy the first trade-window close, hold to the last close.
                let trade: Vec<f64> = window
                    .iter()
                    .filter(|(ts, _)| *ts >= warmup_ms)
                    .map(|(_, p)| *p)
                    .collect();
                let bh = if trade.len() >= 2 && trade[0] > 0.0 {
                    (trade[trade.len() - 1] / trade[0] - 1.0) * 100.0
                } else {
                    0.0
                };
                sym_windows.push((*start, *end, warmup_ms, window, bh));
            }
        }
        slices.insert(sym.clone(), sym_windows);
    }

    let windows_per_symbol: usize = cfg.symbols.iter().map(|s| slices[s].len()).sum();
    let total_replays = cfg.configs.len() * windows_per_symbol * cfg.capitals.len();
    let total_replays_u64 = total_replays as u64;
    let threads = cfg.threads.clamp(1, 64);
    println!(
        "Experiment '{}': {} configs x {} windows x {} symbols x {} capitals = {} replays on {} threads (in-memory, no disk)",
        cfg.label,
        cfg.configs.len(),
        windows_per_symbol,
        cfg.symbols.len(),
        cfg.capitals.len(),
        total_replays,
        threads,
    );

    let chunk = cfg.configs.len().div_ceil(threads);
    let mut stats: Vec<RunStats> = vec![RunStats::default(); cfg.configs.len()];
    let mut bh_wins: Vec<usize> = vec![0; cfg.configs.len()];
    let mut bh_total: Vec<usize> = vec![0; cfg.configs.len()];
    let mut bh_diff_sum: Vec<f64> = vec![0.0; cfg.configs.len()];

    let done = AtomicU64::new(0);
    let workers_done = AtomicBool::new(false);
    let mut worker_errors: Vec<anyhow::Error> = Vec::new();
    std::thread::scope(|scope| {
        let done_ref = &done;
        let workers_done_ref = &workers_done;
        let monitor = scope.spawn(move || {
            let mut last_step = 0u64;
            loop {
                let d = done_ref.load(AtomicOrdering::Relaxed);
                let step = if total_replays_u64 > 0 {
                    d * 100 / total_replays_u64 / 5
                } else {
                    20
                };
                if step != last_step {
                    last_step = step;
                    println!(
                        "  progress: {d}/{total_replays_u64} replays ({}%)",
                        step * 5
                    );
                }
                if workers_done_ref.load(AtomicOrdering::Relaxed) || d >= total_replays_u64 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
        let mut handles = Vec::new();
        for (_chunk_idx, cand_chunk) in cfg.configs.chunks(chunk).enumerate() {
            let slices = &slices;
            let cfg = cfg;
            let done_ref = &done;
            handles.push(scope.spawn(move || {
                // One in-memory store per worker, reset between replays.
                let store = TradingStore::open_in_memory(TradingMode::Simulation)?;
                let mut local: Vec<(RunStats, usize, usize, f64)> =
                    Vec::with_capacity(cand_chunk.len());
                for c in cand_chunk {
                    c.validate()?;
                    let mut s = RunStats::default();
                    let mut beaten = 0usize;
                    let mut total_w = 0usize;
                    let mut diff_sum = 0.0f64;
                    for sym in &cfg.symbols {
                        for (start, end, warmup_ms, window, bh) in &slices[sym] {
                            let mut window_pnl_sum = 0.0f64;
                            for &capital in &cfg.capitals {
                                let bc = BacktestConfig {
                                    symbol: sym.clone(),
                                    from: *start,
                                    to: *end,
                                    capital_usd: capital,
                                    crash_window_secs: c.crash_window_secs,
                                    min_profit_margin: c.min_profit_margin,
                                    taker_fee_rate: c.taker_fee_rate,
                                    tranche_count: c.tranche_count,
                                    ath_lookback_days: c.ath_lookback_days,
                                    max_buy_from_high_pct: c.max_buy_from_high_pct,
                                    min_dip_from_high_pct: c.min_dip_from_high_pct,
                                    max_runup_pct: c.max_runup_pct,
                                    trend_filter: c.trend_filter,
                                    trend_exit: c.trend_exit,
                                    take_profit_pct: c.take_profit_pct,
                                    trail_pct: c.trail_pct,
                                    ceiling: None,
                                    floor: None,
                                    profit_mode: c.profit_mode,
                                };
                                let r =
                                    run_backtest_inner_with_store(&bc, window, *warmup_ms, &store)?;
                                done_ref.fetch_add(1, AtomicOrdering::Relaxed);
                                s.runs += 1;
                                s.pnl_sum += r.pnl_pct;
                                s.dd_sum += r.max_drawdown_pct;
                                s.wins += r.wins;
                                s.losses += r.losses;
                                s.cold_lots += r.cold_lots;
                                s.total_lots += r.total_trades as usize + r.open_lots + r.cold_lots;
                                if r.pnl_pct > 0.0 {
                                    s.profitable_runs += 1;
                                }
                                window_pnl_sum += r.pnl_pct;
                            }
                            // B&H comparison: average the config's PnL across
                            // capitals for this window, then compare to B&H.
                            let avg_pnl = window_pnl_sum / cfg.capitals.len() as f64;
                            total_w += 1;
                            diff_sum += avg_pnl - bh;
                            if avg_pnl > *bh {
                                beaten += 1;
                            }
                        }
                    }
                    local.push((s, beaten, total_w, diff_sum));
                }
                Ok::<_, anyhow::Error>(local)
            }));
        }
        for (chunk_idx, handle) in handles.into_iter().enumerate() {
            match handle.join() {
                Err(_) => worker_errors.push(anyhow::anyhow!("experiment worker panicked")),
                Ok(Err(e)) => worker_errors.push(e),
                Ok(Ok(local)) => {
                    for (j, (s, beaten, total_w, diff_sum)) in local.into_iter().enumerate() {
                        stats[chunk_idx * chunk + j] = s;
                        bh_wins[chunk_idx * chunk + j] = beaten;
                        bh_total[chunk_idx * chunk + j] = total_w;
                        bh_diff_sum[chunk_idx * chunk + j] = diff_sum;
                    }
                }
            }
        }
        workers_done.store(true, AtomicOrdering::Relaxed);
        monitor.join().ok();
    });
    if let Some(e) = worker_errors.into_iter().next() {
        return Err(e);
    }

    // Score with the same composite formula as `CandidateStats::finish` so an
    // experiment is directly comparable to a training sweep.
    let mut results = Vec::new();
    for (i, c) in cfg.configs.iter().enumerate() {
        let s = &stats[i];
        if s.runs == 0 {
            continue;
        }
        let runs = s.runs as f64;
        let avg_pnl = s.pnl_sum / runs;
        let avg_dd = s.dd_sum / runs;
        let win_rate = if s.wins + s.losses > 0 {
            s.wins as f64 / (s.wins + s.losses) as f64
        } else {
            0.0
        };
        let cold_ratio = if s.total_lots > 0 {
            s.cold_lots as f64 / s.total_lots as f64
        } else {
            0.0
        };
        let profitable_ratio = s.profitable_runs as f64 / runs;
        let score =
            avg_pnl - 0.5 * avg_dd - 20.0 * cold_ratio + 8.0 * profitable_ratio + 5.0 * win_rate;
        results.push(ExperimentResult {
            config_name: config_fingerprint(c),
            runs: s.runs,
            score,
            avg_pnl_pct: avg_pnl,
            avg_max_dd_pct: avg_dd,
            win_rate,
            profitable_ratio,
            cold_ratio,
            windows_beaten_bh: bh_wins[i],
            windows_total: bh_total[i],
            avg_pnl_vs_bh: if bh_total[i] > 0 {
                bh_diff_sum[i] / bh_total[i] as f64
            } else {
                0.0
            },
        });
    }
    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Persist every scored config so `experiment list` can compare runs.
    for r in &results {
        store.record_experiment(&NewExperiment {
            label: cfg.label.clone(),
            config_name: r.config_name.clone(),
            symbols: cfg.symbols.join(","),
            from_date: cfg.from.to_string(),
            to_date: cfg.to.to_string(),
            window_days: cfg.window_days,
            step_days: cfg.step_days,
            capitals: cfg
                .capitals
                .iter()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(","),
            runs: r.runs,
            score: r.score,
            avg_pnl_pct: r.avg_pnl_pct,
            avg_max_dd_pct: r.avg_max_dd_pct,
            win_rate: r.win_rate,
            profitable_ratio: r.profitable_ratio,
            cold_ratio: r.cold_ratio,
            windows_beaten_bh: r.windows_beaten_bh,
            windows_total: r.windows_total,
            avg_pnl_vs_bh: r.avg_pnl_vs_bh,
        })?;
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::engine::{ProfitMode, TrendFilter};
    use crate::trading::store::TradingMode;
    use tempfile::tempdir;

    fn test_store() -> (TradingStore, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("experiment_test.db");
        let store = TradingStore::open_path(path.to_str().expect("path"), TradingMode::Simulation)
            .expect("open store");
        (store, dir)
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    /// Daily price series from `from` to `to` applying `moves` (repeated /
    /// padded with 0.0 when shorter than the span). Moves are percentages.
    fn synthetic_daily(from: NaiveDate, to: NaiveDate, moves: &[f64]) -> Vec<(i64, f64)> {
        let days = (to - from).num_days();
        let mut price = 100.0;
        let mut out = Vec::new();
        let mut ts = date_to_ms(from);
        for i in 0..days {
            let m = moves.get(i as usize).copied().unwrap_or(0.0);
            price *= 1.0 + m;
            out.push((ts, price));
            ts += 86_400_000;
        }
        out
    }

    fn base_config() -> EngineConfig {
        EngineConfig {
            min_profit_margin: 0.01,
            taker_fee_rate: 0.003,
            tranche_count: 2,
            min_tranche_fraction: 0.5,
            max_spread_pct: 0.5,
            crash_drop_pct: 15.0,
            crash_window_secs: 7 * 86_400,
            ath_lookback_days: 30,
            max_buy_from_high_pct: 5.0,
            min_dip_from_high_pct: 0.0,
            max_runup_pct: 30.0,
            trend_filter: TrendFilter::Off,
            trend_exit: false,
            take_profit_pct: 0.10,
            trail_pct: 0.0,
            profit_mode: ProfitMode::Reinvest,
        }
    }

    /// Sideways market with a dip and recovery: the accumulator buys the dip
    /// and sells at take-profit, so it should beat buy-and-hold in most
    /// windows. This is the "win-win on chop" scenario.
    #[test]
    fn accumulator_beats_bh_on_sideways_dip_recovery() {
        let (store, _dir) = test_store();
        let from = d(2019, 1, 1);
        let to = d(2020, 6, 1);
        let mut moves = vec![0.0f64; 50]; // warm-up prefix: fetch starts 50d before cfg.from
                                          // Repeating dip/recovery cycle aligned with the 60d window step, so
                                          // every window contains a full dip -> recovery and the accumulator
                                          // gets a chance to buy low and exit (or mark-to-market) above B&H.
        for _cycle in 0..8 {
            for _ in 0..10 {
                moves.push(-0.01);
            }
            for _ in 0..10 {
                moves.push(0.012);
            }
            for _ in 0..40 {
                moves.push(0.0);
            }
        }
        let cfg = ExperimentConfig {
            label: "sideways".into(),
            symbols: vec!["BTC".into()],
            from,
            to,
            window_days: 180,
            step_days: 60,
            capitals: vec![20.0, 50.0, 200.0],
            configs: vec![base_config()],
            threads: 2,
        };
        let results = run_experiment(&store, &cfg, &|_sym, f, t| {
            Ok(synthetic_daily(f, t, &moves))
        })
        .expect("run");
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert!(r.runs > 0);
        assert!(r.windows_total > 0);
        assert!(
            r.windows_beaten_bh * 2 >= r.windows_total,
            "accumulator should beat B&H in >=50% of sideways windows: {} / {}",
            r.windows_beaten_bh,
            r.windows_total
        );
        assert!(r.avg_pnl_vs_bh > 0.0, "avg PnL vs B&H should be positive");
        assert!(r.score.is_finite());
    }

    /// Pure bull market: steady +0.3%/day. B&H is huge, the accumulator never
    /// buys (price is always near its high) — honesty check that the benchmark
    /// does NOT claim a win where none exists.
    #[test]
    fn bh_beats_accumulator_on_pure_bull() {
        let (store, _dir) = test_store();
        let from = d(2019, 1, 1);
        let to = d(2020, 6, 1);
        let moves = vec![0.003f64; 400];
        let cfg = ExperimentConfig {
            label: "bull".into(),
            symbols: vec!["BTC".into()],
            from,
            to,
            window_days: 180,
            step_days: 60,
            capitals: vec![50.0],
            configs: vec![base_config()],
            threads: 2,
        };
        let results = run_experiment(&store, &cfg, &|_sym, f, t| {
            Ok(synthetic_daily(f, t, &moves))
        })
        .expect("run");
        let r = &results[0];
        assert_eq!(r.windows_beaten_bh, 0, "must not beat B&H in a pure bull");
        assert!(r.avg_pnl_vs_bh < 0.0, "B&H should outperform on bull");
    }

    /// Configs are ranked by the same composite score as `train()`: a tight
    /// margin that harvests the dip should outrank a wide one that rarely
    /// exits.
    #[test]
    fn experiment_ranks_better_config_first() {
        let (store, _dir) = test_store();
        let from = d(2019, 1, 1);
        let to = d(2020, 6, 1);
        let mut moves = vec![0.0f64; 400];
        for i in 0..10 {
            moves[i] = -0.01;
        }
        for i in 10..20 {
            moves[i] = 0.012;
        }
        let mut tight = base_config();
        tight.min_profit_margin = 0.01;
        let mut wide = base_config();
        wide.min_profit_margin = 0.08;
        let cfg = ExperimentConfig {
            label: "rank".into(),
            symbols: vec!["BTC".into()],
            from,
            to,
            window_days: 180,
            step_days: 60,
            capitals: vec![50.0],
            configs: vec![wide.clone(), tight.clone()],
            threads: 2,
        };
        let results = run_experiment(&store, &cfg, &|_sym, f, t| {
            Ok(synthetic_daily(f, t, &moves))
        })
        .expect("run");
        assert_eq!(results.len(), 2);
        assert!(
            results[0].score >= results[1].score,
            "tight margin should rank first: {:.2} vs {:.2}",
            results[0].score,
            results[1].score
        );
    }

    /// Every scored config is persisted; `list` returns newest first and
    /// `clear` drops everything.
    #[test]
    fn experiment_rows_persist_list_and_clear() {
        let (store, _dir) = test_store();
        let from = d(2019, 1, 1);
        let to = d(2020, 6, 1);
        let cfg = ExperimentConfig {
            label: "persist".into(),
            symbols: vec!["BTC".into()],
            from,
            to,
            window_days: 180,
            step_days: 60,
            capitals: vec![50.0],
            configs: vec![base_config()],
            threads: 2,
        };
        let results = run_experiment(&store, &cfg, &|_sym, f, t| {
            Ok(synthetic_daily(f, t, &[0.0f64; 400]))
        })
        .expect("run");
        assert_eq!(results.len(), 1);
        let rows = store.list_experiments(10).expect("list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label, "persist");
        assert_eq!(rows[0].windows_total, results[0].windows_total as i64);
        assert_eq!(
            rows[0].windows_beaten_bh,
            results[0].windows_beaten_bh as i64
        );
        store.clear_experiments().expect("clear");
        assert!(store.list_experiments(10).expect("list").is_empty());
    }

    #[test]
    fn experiment_validate_rejects_bad_input() {
        let cfg = ExperimentConfig {
            label: "bad".into(),
            symbols: vec!["BTC".into()],
            from: d(2019, 1, 1),
            to: d(2020, 1, 1),
            window_days: 180,
            step_days: 60,
            capitals: vec![],
            configs: vec![base_config()],
            threads: 1,
        };
        assert!(cfg.validate().is_err());
    }
}
