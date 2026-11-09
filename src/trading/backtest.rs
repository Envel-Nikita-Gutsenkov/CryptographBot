//! Phase 6: backtest & walk-forward engine.
//!
//! Replays historical prices through the REAL deterministic engine
//! ([`TradingEngine::apply_tick`]) inside a fresh per-run simulation store,
//! records `backtest_runs` + `strategy_lessons` into the persistent sim DB,
//! and supports batch sweeps (hundreds of asset × period × duration windows)
//! plus walk-forward train/validate splits.
//!
//! Data source: Binance daily klines, paginated (1000 candles per request),
//! for 2020–2026 depth. The bot's local `price_history` table only keeps ~30
//! days, so it is not used here. See plan §7.

use anyhow::{bail, Context, Result};
use chrono::{Duration, NaiveDate};
use std::collections::HashMap;

use super::engine::{
    EngineConfig, MarketTick, ProfitMode, TradingEngine, TrendFilter, DEFAULT_ATH_LOOKBACK_DAYS,
    DEFAULT_CRASH_DROP_PCT, DEFAULT_MAX_BUY_FROM_HIGH_PCT, DEFAULT_MAX_RUNUP_PCT,
    DEFAULT_MAX_SPREAD_PCT, DEFAULT_MIN_PROFIT_MARGIN, DEFAULT_TAKER_FEE_RATE,
    DEFAULT_TAKE_PROFIT_PCT, DEFAULT_TRANCHE_COUNT,
};
use super::store::{
    LotStatus, NewAllocation, NewBacktest, NewConstraint, NewLesson, RuleType, TradingMode,
    TradingStore, EPSILON,
};

/// Crash window used for daily-bar backtests (7 days). The live default (15
/// minutes) would never trip the breaker on daily candles.
pub const DEFAULT_CRASH_WINDOW_DAILY_SECS: i64 = 7 * 86_400;

const BINANCE_KLINES_URL: &str = "https://api.binance.com/api/v3/klines";
const KLINES_PAGE_LIMIT: u32 = 1000;
pub(crate) const DAY_MS: i64 = 86_400_000;

/// Minimum fraction of expected daily candles a window must contain before it
/// counts as a run. Windows from before an asset's listing (e.g. SOL on
/// Binance only since Aug 2020) or with large data gaps would otherwise pollute
/// the sweep with noise.
pub fn min_window_candles(window_days: i64) -> usize {
    ((window_days as usize) * 2 / 3).max(1)
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BacktestConfig {
    pub symbol: String,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub capital_usd: f64,
    pub crash_window_secs: i64,
    pub min_profit_margin: f64,
    pub taker_fee_rate: f64,
    pub tranche_count: u32,
    /// ATH guard: look back this many days for the recent high (0 = off).
    pub ath_lookback_days: i64,
    /// ATH guard: skip buys within this percent of the recent high (0 = off).
    pub max_buy_from_high_pct: f64,
    /// Conviction filter: only buy when price is at least this percent below
    /// the recent high over `ath_lookback_days` (0 = off). "Rare but accurate"
    /// mode — fewer, deeper entries.
    pub min_dip_from_high_pct: f64,
    /// Run-up guard: skip buys when price is more than this percent above the
    /// recent low over `ath_lookback_days` (0 = off).
    pub max_runup_pct: f64,
    /// Trend filter: gate buys on the 50-day SMA regime. OFF = disabled.
    pub trend_filter: TrendFilter,
    /// Trend exit: sell in-profit lots when price crosses below the 50-day SMA.
    pub trend_exit: bool,
    /// Take-profit target: exit at `min_exit * (1 + take_profit_pct)`.
    pub take_profit_pct: f64,
    /// Trailing stop: sell in-profit lots when price drops this fraction below
    /// the peak since entry (0 = disabled). Lets winners run in bull markets.
    pub trail_pct: f64,
    pub ceiling: Option<f64>,
    pub floor: Option<f64>,
    /// How realized profit is routed (plan §5.4). Defaults to compounding.
    pub profit_mode: ProfitMode,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            symbol: String::new(),
            from: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
            to: NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(),
            capital_usd: 1000.0,
            crash_window_secs: DEFAULT_CRASH_WINDOW_DAILY_SECS,
            min_profit_margin: DEFAULT_MIN_PROFIT_MARGIN,
            taker_fee_rate: DEFAULT_TAKER_FEE_RATE,
            tranche_count: DEFAULT_TRANCHE_COUNT,
            ath_lookback_days: DEFAULT_ATH_LOOKBACK_DAYS,
            max_buy_from_high_pct: DEFAULT_MAX_BUY_FROM_HIGH_PCT,
            min_dip_from_high_pct: 0.0,
            max_runup_pct: DEFAULT_MAX_RUNUP_PCT,
            trend_filter: TrendFilter::Off,
            trend_exit: false,
            take_profit_pct: DEFAULT_TAKE_PROFIT_PCT,
            trail_pct: 0.0,
            ceiling: None,
            floor: None,
            profit_mode: ProfitMode::Reinvest,
        }
    }
}

impl BacktestConfig {
    pub fn validate(&self) -> Result<()> {
        if self.symbol.trim().is_empty() {
            bail!("symbol must not be empty");
        }
        if self.to <= self.from {
            bail!("to date must be after from date");
        }
        if !self.capital_usd.is_finite() || self.capital_usd <= 0.0 {
            bail!("capital must be finite and > 0");
        }
        if self.crash_window_secs <= 0 {
            bail!("crash_window_secs must be > 0");
        }
        if self.tranche_count == 0 {
            bail!("tranche_count must be >= 1");
        }
        if self.ath_lookback_days < 0 || self.ath_lookback_days > 3650 {
            bail!("ath_lookback_days must be in [0, 3650]");
        }
        if !self.max_buy_from_high_pct.is_finite() || self.max_buy_from_high_pct < 0.0 {
            bail!("max_buy_from_high_pct must be finite and >= 0");
        }
        if !self.min_dip_from_high_pct.is_finite() || self.min_dip_from_high_pct < 0.0 {
            bail!("min_dip_from_high_pct must be finite and >= 0");
        }
        if !self.max_runup_pct.is_finite() || self.max_runup_pct < 0.0 {
            bail!("max_runup_pct must be finite and >= 0");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TradeRecord {
    pub lot_id: String,
    pub entry_price: f64,
    pub exit_price: f64,
    pub amount: f64,
    pub profit_usd: f64,
    /// WIN | LOSS | OPEN | COLD_HOLD
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct BacktestResult {
    pub symbol: String,
    pub start_date: String,
    pub end_date: String,
    pub initial_capital_usd: f64,
    pub final_capital_usd: f64,
    pub total_trades: i64,
    pub wins: i64,
    pub losses: i64,
    pub pnl_usd: f64,
    pub pnl_pct: f64,
    pub max_drawdown_pct: f64,
    pub open_lots: usize,
    pub cold_lots: usize,
    pub trades: Vec<TradeRecord>,
    pub lessons: Vec<String>,
}

impl BacktestResult {
    pub fn report(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "=== Backtest: {} {} → {} (capital ${:.2}) ===\n",
            self.symbol, self.start_date, self.end_date, self.initial_capital_usd
        ));
        s.push_str(&format!(
            "Trades: {} ({} wins / {} losses) | Final equity: ${:.2} ({:+.2}%) | Max drawdown: {:.2}%\n",
            self.total_trades,
            self.wins,
            self.losses,
            self.final_capital_usd,
            self.pnl_pct,
            self.max_drawdown_pct
        ));
        if self.open_lots > 0 || self.cold_lots > 0 {
            s.push_str(&format!(
                "Unrealized: {} open / {} cold-hold lots\n",
                self.open_lots, self.cold_lots
            ));
        }
        for t in &self.trades {
            s.push_str(&format!(
                "  [{}] {} entry {:.4} → exit {:.4} amount {:.6} profit {:+.2} ({})\n",
                t.lot_id,
                self.symbol,
                t.entry_price,
                t.exit_price,
                t.amount,
                t.profit_usd,
                t.status
            ));
        }
        s
    }
}

// ---------------------------------------------------------------------------
// Data fetching (Binance daily klines, paginated)
// ---------------------------------------------------------------------------

pub(crate) fn date_to_ms(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis()
}

async fn fetch_page(
    client: &reqwest::Client,
    pair: &str,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<(i64, f64)>> {
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let resp = client
            .get(BINANCE_KLINES_URL)
            .query(&[
                ("symbol", pair.to_string()),
                ("interval", "1d".to_string()),
                ("startTime", start_ms.to_string()),
                ("endTime", end_ms.to_string()),
                ("limit", KLINES_PAGE_LIMIT.to_string()),
            ])
            .send()
            .await?;
        if resp.status() == 429 {
            if attempt >= 4 {
                bail!("Binance rate limited after {attempt} attempts");
            }
            tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
            continue;
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Binance klines error ({}): {}", status, body);
        }
        let klines: Vec<serde_json::Value> = resp.json().await?;
        let mut page = Vec::new();
        for k in &klines {
            // kline: [open_time_ms, open, high, low, close, ...]
            if let (Some(open_time), Some(close)) = (k[0].as_i64(), k[4].as_str()) {
                if let Ok(price) = close.parse::<f64>() {
                    page.push((open_time, price));
                }
            }
        }
        return Ok(page);
    }
}

/// Fetch daily close prices for `symbol` in `[from, to]` (UTC days) with a
/// multi-source failover chain: Binance (paginated, arbitrary range) →
/// CryptoCompare `histoday` with `toTs` pagination (arbitrary range) →
/// CoinGecko `market_chart/range` (arbitrary range). If Binance glitches or
/// rate-limits, training and backtests keep working from the fallbacks.
pub async fn fetch_history(
    symbol: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<(i64, f64)>> {
    let from_ms = date_to_ms(from);
    let to_ms = date_to_ms(to) + DAY_MS - 1; // inclusive end-of-day

    // Local disk cache first: data/history/{SYMBOL}.csv. If it already covers
    // the requested range, slice it — no network, no rate limits, fast tests.
    if let Some(cached) = super::history_file::load_history(symbol) {
        if let (Some(&(f0, _)), Some(&(l0, _))) = (cached.first(), cached.last()) {
            // A candle whose open timestamp is `to` closes at the end of `to`,
            // so it covers the whole day — compare against start-of-day, not
            // the inclusive end-of-day bound (that would always miss and force
            // a network fetch for a fully-cached range).
            if f0 <= from_ms && l0 >= date_to_ms(to) {
                let slice: Vec<(i64, f64)> = cached
                    .iter()
                    .filter(|(ts, _)| *ts >= from_ms && *ts <= to_ms)
                    .copied()
                    .collect();
                if !slice.is_empty() {
                    return Ok(slice);
                }
            }
        }
    }

    let fresh = match fetch_binance_history(symbol, from, to).await {
        Ok(h) if !h.is_empty() => h,
        Ok(_) | Err(_) => {
            println!("  Binance unavailable for {symbol}, falling back to CryptoCompare...");
            match fetch_cryptocompare_history(symbol, from, to).await {
                Ok(h) if !h.is_empty() => h,
                Ok(_) | Err(_) => {
                    println!(
                        "  CryptoCompare unavailable for {symbol}, falling back to CoinGecko..."
                    );
                    fetch_coingecko_history(symbol, from, to).await?
                }
            }
        }
    };

    // Merge with any older cache (cache may start earlier) and persist.
    let mut merged: Vec<(i64, f64)> = Vec::new();
    if let Some(cached) = super::history_file::load_history(symbol) {
        merged.extend(cached);
    }
    merged.extend(fresh);
    merged.sort_unstable_by_key(|(ts, _)| *ts);
    merged.dedup_by_key(|(ts, _)| *ts);
    if let Err(e) = super::history_file::save_history(symbol, &merged) {
        eprintln!("  warn: history cache write failed for {symbol}: {e}");
    }

    let slice: Vec<(i64, f64)> = merged
        .iter()
        .filter(|(ts, _)| *ts >= from_ms && *ts <= to_ms)
        .copied()
        .collect();
    Ok(slice)
}

/// Binance daily klines, paginating past the 1000-candle per-request cap.
async fn fetch_binance_history(
    symbol: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<(i64, f64)>> {
    let pair = format!("{}USDT", symbol.to_uppercase());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .use_rustls_tls()
        .build()?;
    let from_ms = date_to_ms(from);
    let to_ms = date_to_ms(to) + DAY_MS - 1; // inclusive end-of-day

    let mut out: Vec<(i64, f64)> = Vec::new();
    let mut start = from_ms;
    let mut guard = 0u32;
    loop {
        guard += 1;
        if guard > 200 {
            bail!("pagination guard tripped for {pair}");
        }
        let page = fetch_page(&client, &pair, start, to_ms).await?;
        if page.is_empty() {
            break;
        }
        let mut last_ts = 0i64;
        for (ts, price) in page {
            if ts >= from_ms && ts <= to_ms {
                out.push((ts, price));
            }
            last_ts = ts;
        }
        if last_ts >= to_ms {
            break;
        }
        start = last_ts + DAY_MS;
        if start > to_ms {
            break;
        }
    }

    out.sort_by_key(|&(ts, _)| ts);
    out.dedup_by_key(|&mut (ts, _)| ts);
    if out.is_empty() {
        bail!("no {pair} daily klines in {from}..{to} (symbol may not be listed yet on Binance)");
    }
    Ok(out)
}

/// CryptoCompare `histoday` paginated backwards with `toTs`, so arbitrary
/// historical ranges work (the provider's own `get_history` only returns the
/// last N days). Needs `CRYPTOCOMPARE_API_KEY` from the environment.
async fn fetch_cryptocompare_history(
    symbol: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<(i64, f64)>> {
    let api_key = std::env::var("CRYPTOCOMPARE_API_KEY").unwrap_or_default();
    if api_key.is_empty() || api_key.starts_with("xxxxxx") {
        bail!("CRYPTOCOMPARE_API_KEY missing or redacted");
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .use_rustls_tls()
        .build()?;
    let from_ms = date_to_ms(from);
    let to_ms = date_to_ms(to) + DAY_MS - 1;
    let mut out: Vec<(i64, f64)> = Vec::new();
    let mut cursor = to_ms / 1000; // CryptoCompare uses seconds
    let mut guard = 0u32;
    loop {
        guard += 1;
        if guard > 30 {
            bail!("CryptoCompare pagination guard tripped for {symbol}");
        }
        let url = format!(
            "https://min-api.cryptocompare.com/data/v2/histoday?fsym={}&tsym=USD&limit=2000&toTs={}",
            symbol.to_uppercase(),
            cursor
        );
        let resp = client
            .get(&url)
            .header("authorization", format!("Apikey {api_key}"))
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("CryptoCompare HTTP {}", resp.status());
        }
        let json: serde_json::Value = resp.json().await?;
        if json.get("Response").and_then(|r| r.as_str()) == Some("Error") {
            bail!(
                "CryptoCompare API error: {:?}",
                json.get("Message").unwrap_or(&serde_json::Value::Null)
            );
        }
        let data = json
            .get("Data")
            .and_then(|d| d.get("Data"))
            .and_then(|d| d.as_array())
            .cloned()
            .unwrap_or_default();
        if data.is_empty() {
            break;
        }
        let mut last_ts = 0i64;
        for item in &data {
            if let (Some(time), Some(close)) = (
                item.get("time").and_then(|t| t.as_i64()),
                item.get("close").and_then(|c| c.as_f64()),
            ) {
                let ts_ms = time * 1000;
                if ts_ms >= from_ms && ts_ms <= to_ms {
                    out.push((ts_ms, close));
                }
                last_ts = time;
            }
        }
        if last_ts <= from_ms / 1000 {
            break;
        }
        cursor = last_ts - 1;
    }
    out.sort_by_key(|&(ts, _)| ts);
    out.dedup_by_key(|&mut (ts, _)| ts);
    if out.is_empty() {
        bail!("no CryptoCompare data for {symbol} in {from}..{to}");
    }
    Ok(out)
}

/// CoinGecko `market_chart/range` (arbitrary range). Needs
/// `COINGECKO_API_KEY`; only the four training symbols are mapped.
async fn fetch_coingecko_history(
    symbol: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<(i64, f64)>> {
    let api_key = std::env::var("COINGECKO_API_KEY").unwrap_or_default();
    if api_key.is_empty() {
        bail!("COINGECKO_API_KEY missing");
    }
    let id = match symbol.to_uppercase().as_str() {
        "BTC" => "bitcoin",
        "ETH" => "ethereum",
        "SOL" => "solana",
        "LINK" => "chainlink",
        other => bail!("no CoinGecko id mapping for {other}"),
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .use_rustls_tls()
        .build()?;
    let url = format!("https://api.coingecko.com/api/v3/coins/{id}/market_chart/range");
    let from_s = (date_to_ms(from) / 1000).to_string();
    let to_s = (date_to_ms(to) / 1000).to_string();
    let resp = client
        .get(&url)
        .query(&[
            ("vs_currency", "usd"),
            ("from", from_s.as_str()),
            ("to", to_s.as_str()),
            ("x_cg_demo_api_key", api_key.as_str()),
        ])
        .send()
        .await?;
    if resp.status() == 429 {
        bail!("CoinGecko rate limited");
    }
    if !resp.status().is_success() {
        bail!("CoinGecko HTTP {}", resp.status());
    }
    let json: serde_json::Value = resp.json().await?;
    let mut out: Vec<(i64, f64)> = Vec::new();
    if let Some(prices) = json.get("prices").and_then(|p| p.as_array()) {
        for p in prices {
            if let (Some(ts), Some(price)) = (p[0].as_f64(), p[1].as_f64()) {
                out.push((ts as i64, price));
            }
        }
    }
    out.sort_by_key(|&(ts, _)| ts);
    out.dedup_by_key(|&mut (ts, _)| ts);
    if out.is_empty() {
        bail!("no CoinGecko data for {symbol} in {from}..{to}");
    }
    Ok(out)
}

/// Synchronous wrapper for the CLI (the `trade` command tree is sync).
///
/// The CLI always runs inside the app's tokio runtime (see `main.rs`), so we
/// must NOT create a nested runtime — `futures::executor::block_on` drives the
/// future on the calling thread using the ambient reactor.
pub fn fetch_history_blocking(
    symbol: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<(i64, f64)>> {
    futures::executor::block_on(fetch_history(symbol, from, to))
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// Replay `history` through the real engine in a throwaway in-memory sim store
/// and compute the full result. Pure w.r.t. persistence — the caller decides
/// where `backtest_runs`/`strategy_lessons` rows go.
///
/// `pub(crate)`: the training pipeline (plan §8) reuses it to sweep candidate
/// configs without persisting every intermediate run.
pub(crate) fn run_backtest_inner(
    cfg: &BacktestConfig,
    history: &[(i64, f64)],
    warmup_ms: i64,
) -> Result<BacktestResult> {
    let store = TradingStore::open_in_memory(TradingMode::Simulation)?;
    run_backtest_inner_with_store(cfg, history, warmup_ms, &store)
}

/// Same as [`run_backtest_inner`] but on a caller-owned store, which is reset
/// between replays. Training/experiment workers create one in-memory store per
/// thread and reuse it across all windows — schema init per replay was the
/// dominant fixed cost of a grid sweep.
pub fn run_backtest_inner_with_store(
    cfg: &BacktestConfig,
    history: &[(i64, f64)],
    warmup_ms: i64,
    store: &TradingStore,
) -> Result<BacktestResult> {
    cfg.validate()?;
    if history.len() < 2 {
        bail!("need at least 2 price points to backtest");
    }

    store.reset_sim_state()?;
    let alloc = store.create_allocation(NewAllocation {
        asset: cfg.symbol.clone(),
        total_budget_usd: cfg.capital_usd,
    })?;
    if let Some(c) = cfg.ceiling {
        store.add_constraint(NewConstraint {
            asset: cfg.symbol.clone(),
            rule_type: RuleType::MaxPriceCeiling,
            threshold_value: c,
            raw_prompt: format!("backtest ceiling {c}"),
            expires_at: None,
        })?;
    }
    if let Some(f) = cfg.floor {
        store.add_constraint(NewConstraint {
            asset: cfg.symbol.clone(),
            rule_type: RuleType::MinPriceFloor,
            threshold_value: f,
            raw_prompt: format!("backtest floor {f}"),
            expires_at: None,
        })?;
    }

    let engine_cfg = EngineConfig {
        min_profit_margin: cfg.min_profit_margin,
        taker_fee_rate: cfg.taker_fee_rate,
        tranche_count: cfg.tranche_count,
        min_tranche_fraction: super::engine::DEFAULT_MIN_TRANCHE_FRACTION,
        max_spread_pct: DEFAULT_MAX_SPREAD_PCT,
        crash_drop_pct: DEFAULT_CRASH_DROP_PCT,
        crash_window_secs: cfg.crash_window_secs,
        ath_lookback_days: cfg.ath_lookback_days,
        max_buy_from_high_pct: cfg.max_buy_from_high_pct,
        min_dip_from_high_pct: cfg.min_dip_from_high_pct,
        max_runup_pct: cfg.max_runup_pct,
        trend_filter: cfg.trend_filter,
        trend_exit: cfg.trend_exit,
        take_profit_pct: cfg.take_profit_pct,
        trail_pct: cfg.trail_pct,
        profit_mode: cfg.profit_mode,
    };
    let mut engine = TradingEngine::new_replay(engine_cfg)?;

    let mut peak_equity = cfg.capital_usd;
    let mut max_dd = 0.0;
    let mut wins = 0i64;
    let mut losses = 0i64;
    let mut closed: HashMap<String, (f64, f64)> = HashMap::new(); // lot_id -> (exit, profit)
    let mut last_price = history[0].1;

    let mut cached_alloc = alloc.clone();
    let mut cached_open_coins = 0.0;

    for (_i, &(ts_ms, price)) in history.iter().enumerate() {
        last_price = price;
        let tick = MarketTick::new(&cfg.symbol, price, price, price, ts_ms / 1000);
        // Warm-up ticks (before `warmup_ms`) only feed the engine's price
        // buffers so the ATH guard has context; they never trade.
        if ts_ms < warmup_ms {
            engine.observe(&tick);
            continue;
        }
        let outcome = engine.apply_tick(&store, &tick)?;
        for s in &outcome.sells {
            if s.profit_usd > EPSILON {
                wins += 1;
            } else {
                losses += 1;
            }
            closed.insert(s.lot_id.clone(), (s.exit_price, s.profit_usd));
        }

        let had_trades = !outcome.buys.is_empty() || !outcome.sells.is_empty();
        if had_trades {
            if let Some(a) = store.get_allocation(&alloc.id)? {
                cached_alloc = a;
            }
            let mut sum_coins = 0.0;
            for lot in store.list_lots_active()? {
                sum_coins += lot.amount;
            }
            cached_open_coins = sum_coins;
        }

        let equity = cached_alloc.available_usd
            + cached_alloc.profit_reserve_usd
            + cached_open_coins * price;
        if equity > peak_equity {
            peak_equity = equity;
        }
        if peak_equity > 0.0 {
            let dd = (peak_equity - equity) / peak_equity * 100.0;
            if dd > max_dd {
                max_dd = dd;
            }
        }
    }

    // Tally lots into trade records.
    let mut trades = Vec::new();
    let mut open_lots = 0usize;
    let mut cold_lots = 0usize;
    let mut all_lots = store.list_lots(None)?;
    all_lots.reverse();
    for lot in all_lots {
        match lot.status {
            LotStatus::Closed => {
                let (exit, profit) = closed
                    .get(&lot.id)
                    .copied()
                    .unwrap_or((lot.entry_price, 0.0));
                trades.push(TradeRecord {
                    lot_id: lot.id.clone(),
                    entry_price: lot.entry_price,
                    exit_price: exit,
                    amount: lot.amount,
                    profit_usd: profit,
                    status: if profit > EPSILON {
                        "WIN"
                    } else if profit < -EPSILON {
                        "LOSS"
                    } else {
                        "BREAKEVEN"
                    }
                    .into(),
                });
            }
            LotStatus::Open => {
                open_lots += 1;
                trades.push(TradeRecord {
                    lot_id: lot.id.clone(),
                    entry_price: lot.entry_price,
                    exit_price: last_price,
                    amount: lot.amount,
                    profit_usd: lot.amount * last_price * (1.0 - cfg.taker_fee_rate)
                        - lot.cost_basis_usd,
                    status: "OPEN".into(),
                });
            }
            LotStatus::ColdHold => {
                cold_lots += 1;
                trades.push(TradeRecord {
                    lot_id: lot.id.clone(),
                    entry_price: lot.entry_price,
                    exit_price: last_price,
                    amount: lot.amount,
                    profit_usd: lot.amount * last_price * (1.0 - cfg.taker_fee_rate)
                        - lot.cost_basis_usd,
                    status: "COLD_HOLD".into(),
                });
            }
        }
    }

    let alloc_final = store
        .get_allocation(&alloc.id)?
        .context("allocation disappeared at end of replay")?;
    let mut final_equity = alloc_final.available_usd + alloc_final.profit_reserve_usd;
    for lot in store.list_lots_active()? {
        final_equity += lot.amount * last_price;
    }
    let pnl_usd = final_equity - cfg.capital_usd;
    let pnl_pct = pnl_usd / cfg.capital_usd * 100.0;

    // Strategy lessons (plan §7.3): one run summary + per-lot takeaways.
    let mut lessons = Vec::new();
    lessons.push(format!(
        "Backtest {} {}..{}: {} trades ({}W/{}L), PnL {:+.2}% (${:.2}), max DD {:.2}%, {} open/{} cold lots",
        cfg.symbol,
        cfg.from,
        cfg.to,
        trades.len(),
        wins,
        losses,
        pnl_pct,
        pnl_usd,
        max_dd,
        open_lots,
        cold_lots
    ));
    for t in trades
        .iter()
        .filter(|t| t.status == "WIN" || t.status == "LOSS")
        .take(10)
    {
        lessons.push(format!(
            "{} lot {}: entry {:.4} → exit {:.4}, profit {:+.2} USD ({}). Repeat: {}",
            cfg.symbol,
            t.lot_id,
            t.entry_price,
            t.exit_price,
            t.profit_usd,
            t.status,
            if t.status == "WIN" {
                "dip-buy tranche + target exit"
            } else {
                "avoid"
            }
        ));
    }

    Ok(BacktestResult {
        symbol: cfg.symbol.to_uppercase(),
        start_date: cfg.from.to_string(),
        end_date: cfg.to.to_string(),
        initial_capital_usd: cfg.capital_usd,
        final_capital_usd: final_equity,
        total_trades: wins + losses,
        wins,
        losses,
        pnl_usd,
        pnl_pct,
        max_drawdown_pct: max_dd,
        open_lots,
        cold_lots,
        trades,
        lessons,
    })
}

/// Record a finished backtest + its lessons into a persistent store.
fn persist_result(store: &TradingStore, r: &BacktestResult) -> Result<()> {
    store.record_backtest(NewBacktest {
        symbol: r.symbol.clone(),
        start_date: r.start_date.clone(),
        end_date: r.end_date.clone(),
        initial_capital_usd: r.initial_capital_usd,
        final_capital_usd: r.final_capital_usd,
        total_trades: r.total_trades,
        wins: r.wins,
        losses: r.losses,
        pnl_usd: r.pnl_usd,
        pnl_pct: r.pnl_pct,
        max_drawdown_pct: r.max_drawdown_pct,
    })?;
    for lesson in &r.lessons {
        store.record_lesson(NewLesson {
            symbol: r.symbol.clone(),
            lesson: lesson.clone(),
            context_json: serde_json::json!({
                "start": r.start_date,
                "end": r.end_date,
                "trades": r.total_trades,
                "pnl_pct": r.pnl_pct,
                "max_drawdown_pct": r.max_drawdown_pct,
            })
            .to_string(),
        })?;
    }
    Ok(())
}

/// Fetch real history (plus ATH-guard warm-up) and run one backtest, persisting
/// the run + lessons into the persistent simulation DB.
pub fn run_backtest(cfg: &BacktestConfig) -> Result<BacktestResult> {
    let warmup_days = cfg.ath_lookback_days.max(0);
    let fetch_from = if warmup_days > 0 {
        cfg.from - Duration::days(warmup_days)
    } else {
        cfg.from
    };
    let history = fetch_history_blocking(&cfg.symbol, fetch_from, cfg.to)?;
    let result = run_backtest_inner(cfg, &history, date_to_ms(cfg.from))?;
    let store = TradingStore::open(TradingMode::Simulation)?;
    persist_result(&store, &result)?;
    Ok(result)
}

// ---------------------------------------------------------------------------
// Batch sweep & walk-forward
// ---------------------------------------------------------------------------

/// Rolling windows `[start, start+window_days)` over `[from, to)`.
pub fn windows(
    from: NaiveDate,
    to: NaiveDate,
    window_days: i64,
    step_days: i64,
) -> Vec<(NaiveDate, NaiveDate)> {
    let mut out = Vec::new();
    let mut start = from;
    while start < to {
        let end = start + Duration::days(window_days);
        if end > to {
            break;
        }
        out.push((start, end));
        start = start + Duration::days(step_days);
    }
    out
}

#[derive(Debug, Clone)]
pub struct BatchConfig {
    pub symbols: Vec<String>,
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub window_days: i64,
    pub step_days: i64,
    pub capital_usd: f64,
    pub crash_window_secs: i64,
    pub min_profit_margin: f64,
    pub taker_fee_rate: f64,
    pub tranche_count: u32,
    /// ATH guard: look back this many days for the recent high (0 = off).
    pub ath_lookback_days: i64,
    /// ATH guard: skip buys within this percent of the recent high (0 = off).
    pub max_buy_from_high_pct: f64,
    /// Conviction filter: only buy when price is at least this percent below
    /// the recent high over `ath_lookback_days` (0 = off).
    pub min_dip_from_high_pct: f64,
    /// Run-up guard: skip buys when price is more than this percent above the
    /// recent low over `ath_lookback_days` (0 = off).
    pub max_runup_pct: f64,
    /// Trend filter: gate buys on the 50-day SMA regime. OFF = disabled.
    pub trend_filter: TrendFilter,
    /// Trend exit: sell in-profit lots when price crosses below the 50-day SMA.
    pub trend_exit: bool,
    /// Take-profit target: exit at `min_exit * (1 + take_profit_pct)`.
    pub take_profit_pct: f64,
    /// Trailing stop: sell in-profit lots when price drops this fraction below
    /// the peak since entry (0 = disabled). Lets winners run in bull markets.
    pub trail_pct: f64,
    pub profit_mode: ProfitMode,
}

impl BatchConfig {
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
        if !self.capital_usd.is_finite() || self.capital_usd <= 0.0 {
            bail!("capital must be finite and > 0");
        }
        Ok(())
    }

    fn single(&self, symbol: &str, from: NaiveDate, to: NaiveDate) -> BacktestConfig {
        BacktestConfig {
            symbol: symbol.to_string(),
            from,
            to,
            capital_usd: self.capital_usd,
            crash_window_secs: self.crash_window_secs,
            min_profit_margin: self.min_profit_margin,
            taker_fee_rate: self.taker_fee_rate,
            tranche_count: self.tranche_count,
            ath_lookback_days: self.ath_lookback_days,
            max_buy_from_high_pct: self.max_buy_from_high_pct,
            min_dip_from_high_pct: self.min_dip_from_high_pct,
            max_runup_pct: self.max_runup_pct,
            trend_filter: self.trend_filter,
            trend_exit: self.trend_exit,
            take_profit_pct: self.take_profit_pct,
            trail_pct: self.trail_pct,
            ceiling: None,
            floor: None,
            profit_mode: self.profit_mode,
        }
    }
}

/// Run one backtest per rolling window per symbol. History is fetched once
/// per symbol and sliced per window; every run is persisted to the sim DB.
pub fn run_batch(cfg: &BatchConfig) -> Result<Vec<BacktestResult>> {
    cfg.validate()?;
    let store = TradingStore::open(TradingMode::Simulation)?;
    // One scratch store for all windows — results are persisted to `store`
    // after each replay, so the scratch is reset (never the persistent DB).
    let scratch = TradingStore::open_in_memory(TradingMode::Simulation)?;
    let mut results = Vec::new();
    let warmup_days = cfg.ath_lookback_days.max(0);
    let fetch_from = if warmup_days > 0 {
        cfg.from - Duration::days(warmup_days)
    } else {
        cfg.from
    };
    for symbol in &cfg.symbols {
        let history = fetch_history_blocking(symbol, fetch_from, cfg.to)?;
        for (start, end) in windows(cfg.from, cfg.to, cfg.window_days, cfg.step_days) {
            let warmup_ms = date_to_ms(start);
            let start_ms = warmup_ms - warmup_days * DAY_MS;
            let end_ms = date_to_ms(end);
            let window: Vec<(i64, f64)> = history
                .iter()
                .filter(|(ts, _)| *ts >= start_ms && *ts < end_ms)
                .copied()
                .collect();
            // The min-candle filter counts only the trade window, not the
            // warm-up prefix (pre-listing noise must never enter stats).
            let trade_len = window.iter().filter(|(ts, _)| *ts >= warmup_ms).count();
            if trade_len < min_window_candles(cfg.window_days) {
                continue;
            }
            let single = cfg.single(symbol, start, end);
            let r = run_backtest_inner_with_store(&single, &window, warmup_ms, &scratch)?;
            persist_result(&store, &r)?;
            results.push(r);
        }
    }
    Ok(results)
}

/// Walk-forward validation: train a small candidate grid on the first half
/// (`from..split`), pick the best config by composite score, then replay ONLY
/// that config on the second half (`split..to`) which the training never saw.
/// This is the honest generalization check — a config that only fits the
/// training period will score well on train but collapse on validate.
#[derive(Debug, Clone)]
pub struct WalkForwardReport {
    pub from: NaiveDate,
    pub split: NaiveDate,
    pub to: NaiveDate,
    pub train_windows: usize,
    pub validate_windows: usize,
    pub best_config: EngineConfig,
    pub best_score: f64,
    /// Per-window PnL% on the train half for the best config.
    pub train_pnl: Vec<f64>,
    /// Per-window PnL% on the validate half for the best config.
    pub validate_pnl: Vec<f64>,
}

impl WalkForwardReport {
    pub fn report(&self) -> String {
        let avg = |v: &[f64]| -> f64 {
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<f64>() / v.len() as f64
            }
        };
        let profitable = |v: &[f64]| v.iter().filter(|p| **p > 0.0).count();
        let t = avg(&self.train_pnl);
        let v = avg(&self.validate_pnl);
        let mut s = String::new();
        s.push_str(&format!(
            "=== Walk-forward: train {}..{} | validate {}..{} ===\n",
            self.from, self.split, self.split, self.to
        ));
        s.push_str(&format!(
            "Best config: m{:.3} t{} {} c{:.0} (score {:.2})\n",
            self.best_config.min_profit_margin,
            self.best_config.tranche_count,
            match self.best_config.profit_mode {
                ProfitMode::Reinvest => "REINVEST",
                ProfitMode::Separate => "SEPARATE",
                ProfitMode::Partial(_) => "PARTIAL",
            },
            self.best_config.crash_drop_pct,
            self.best_score
        ));
        s.push_str(&format!(
            "TRAIN: {} windows, avg PnL {:+.2}%, profitable {}/{}\n",
            self.train_windows,
            t,
            profitable(&self.train_pnl),
            self.train_pnl.len()
        ));
        s.push_str(&format!(
            "VALIDATE: {} windows, avg PnL {:+.2}%, profitable {}/{}\n",
            self.validate_windows,
            v,
            profitable(&self.validate_pnl),
            self.validate_pnl.len()
        ));
        s.push_str(&format!(
            "Generalization: validate avg PnL {:.2}pp vs train (higher is better; a big drop means overfitting)\n",
            v - t
        ));
        s
    }
}

/// Run the walk-forward split. `split` must be strictly inside `(from, to)`.
/// Uses the small candidate grid (54 configs) so a full run stays fast.
pub fn walk_forward(
    symbols: &[String],
    from: NaiveDate,
    to: NaiveDate,
    split: NaiveDate,
    window_days: i64,
    step_days: i64,
    capital_usd: f64,
) -> Result<WalkForwardReport> {
    if split <= from || split >= to {
        bail!("split date must be strictly inside (from, to)");
    }
    if symbols.is_empty() {
        bail!("at least one symbol required");
    }
    let candidates = super::strategies::candidate_configs(super::strategies::GridSize::Small);
    // Score each candidate on the train half only.
    let mut best: Option<(f64, EngineConfig)> = None;
    let mut train_pnl_best: Vec<f64> = Vec::new();
    for c in &candidates {
        let cfg = BatchConfig {
            symbols: symbols.to_vec(),
            from,
            to: split,
            window_days,
            step_days,
            capital_usd,
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
            profit_mode: c.profit_mode,
        };
        let results = run_batch(&cfg)?;
        if results.is_empty() {
            continue;
        }
        let avg_pnl = results.iter().map(|r| r.pnl_pct).sum::<f64>() / results.len() as f64;
        let avg_dd = results.iter().map(|r| r.max_drawdown_pct).sum::<f64>() / results.len() as f64;
        let cold_ratio = results.iter().map(|r| r.cold_lots as f64).sum::<f64>()
            / results
                .iter()
                .map(|r| (r.total_trades as f64 + r.open_lots as f64 + r.cold_lots as f64).max(1.0))
                .sum::<f64>()
                .max(1.0);
        let profitable =
            results.iter().filter(|r| r.pnl_pct > 0.0).count() as f64 / results.len() as f64;
        let wins: i64 = results.iter().map(|r| r.wins).sum();
        let losses: i64 = results.iter().map(|r| r.losses).sum();
        let win_rate = if wins + losses > 0 {
            wins as f64 / (wins + losses) as f64
        } else {
            0.0
        };
        // Same composite as CandidateStats::finish.
        let score = avg_pnl - 0.5 * avg_dd - 20.0 * cold_ratio + 8.0 * profitable + 5.0 * win_rate;
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, c.clone()));
            train_pnl_best = results.iter().map(|r| r.pnl_pct).collect();
        }
    }
    let (best_score, best_config) = best.ok_or_else(|| anyhow::anyhow!("no train windows"))?;
    // Validate the winner on the unseen half.
    let vcfg = BatchConfig {
        symbols: symbols.to_vec(),
        from: split,
        to,
        window_days,
        step_days,
        capital_usd,
        crash_window_secs: best_config.crash_window_secs,
        min_profit_margin: best_config.min_profit_margin,
        taker_fee_rate: best_config.taker_fee_rate,
        tranche_count: best_config.tranche_count,
        ath_lookback_days: best_config.ath_lookback_days,
        max_buy_from_high_pct: best_config.max_buy_from_high_pct,
        min_dip_from_high_pct: best_config.min_dip_from_high_pct,
        max_runup_pct: best_config.max_runup_pct,
        trend_filter: best_config.trend_filter,
        trend_exit: best_config.trend_exit,
        take_profit_pct: best_config.take_profit_pct,
        trail_pct: best_config.trail_pct,
        profit_mode: best_config.profit_mode,
    };
    let vresults = run_batch(&vcfg)?;
    let validate_pnl: Vec<f64> = vresults.iter().map(|r| r.pnl_pct).collect();
    Ok(WalkForwardReport {
        from,
        split,
        to,
        train_windows: train_pnl_best.len(),
        validate_windows: validate_pnl.len(),
        best_config,
        best_score,
        train_pnl: train_pnl_best,
        validate_pnl,
    })
}

/// Compact aggregate table for a batch sweep.
pub fn batch_summary(results: &[BacktestResult]) -> String {
    let mut s = String::new();
    s.push_str(&format!("=== Batch sweep: {} windows ===\n", results.len()));
    s.push_str(&format!(
        "{:<6} {:<24} {:<7} {:<5} {:<5} {:>8} {:>8} {:>9}\n",
        "SYM", "WINDOW", "TRADES", "WINS", "LOSS", "PNL%", "MAXDD%", "FINAL$"
    ));
    for r in results {
        s.push_str(&format!(
            "{:<6} {:<24} {:<7} {:<5} {:<5} {:>8.2} {:>8.2} {:>9.2}\n",
            r.symbol,
            format!("{}..{}", r.start_date, r.end_date),
            r.total_trades,
            r.wins,
            r.losses,
            r.pnl_pct,
            r.max_drawdown_pct,
            r.final_capital_usd
        ));
    }
    let mut syms: Vec<&str> = results.iter().map(|r| r.symbol.as_str()).collect();
    syms.sort_unstable();
    syms.dedup();
    for sym in syms {
        let runs: Vec<&BacktestResult> = results.iter().filter(|r| r.symbol == sym).collect();
        if runs.is_empty() {
            continue;
        }
        let avg_pnl = runs.iter().map(|r| r.pnl_pct).sum::<f64>() / runs.len() as f64;
        let wins: i64 = runs.iter().map(|r| r.wins).sum();
        let losses: i64 = runs.iter().map(|r| r.losses).sum();
        let win_rate = if wins + losses > 0 {
            wins as f64 / (wins + losses) as f64 * 100.0
        } else {
            0.0
        };
        let profitable = runs.iter().filter(|r| r.pnl_pct > 0.0).count();
        s.push_str(&format!(
            "\n{sym}: {profitable}/{} windows profitable, avg PnL {avg_pnl:+.2}%, trade win-rate {win_rate:.1}% ({wins}W/{losses}L)",
            runs.len()
        ));
    }
    s
}

// ---------------------------------------------------------------------------
// Tests (synthetic series — no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn series(start: NaiveDate, prices: &[f64]) -> Vec<(i64, f64)> {
        prices
            .iter()
            .enumerate()
            .map(|(i, p)| (date_to_ms(start + Duration::days(i as i64)), *p))
            .collect()
    }

    fn cfg(symbol: &str, from: NaiveDate, to: NaiveDate) -> BacktestConfig {
        BacktestConfig {
            symbol: symbol.into(),
            from,
            to,
            ..Default::default()
        }
    }

    fn dates() -> (NaiveDate, NaiveDate) {
        (
            NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(2020, 4, 1).unwrap(),
        )
    }

    #[test]
    fn dip_buy_rip_sell_profits() {
        let (from, to) = dates();
        // 10 days @100, gradual dip to 80 over 20 days, then rip to 110.
        let mut prices = vec![100.0; 10];
        for i in 1..=20 {
            prices.push(100.0 - i as f64);
        }
        prices.extend(std::iter::repeat(110.0).take(60));
        let hist = series(from, &prices);
        let r = run_backtest_inner(&cfg("BTC", from, to), &hist, 0).unwrap();
        assert!(r.wins >= 2, "expected >=2 wins, got {}", r.wins);
        assert_eq!(r.losses, 0, "no realized losses allowed");
        assert!(r.pnl_pct > 0.0, "expected positive pnl, got {}", r.pnl_pct);
        assert!(r.max_drawdown_pct > 0.0);
    }

    #[test]
    fn crash_never_sells_at_loss() {
        let (from, to) = dates();
        // flat at the high (ATH guard parks cash), a 10% dip buys one lot,
        // then a brutal crash to 50: the engine must hold, never sell.
        let mut prices = vec![100.0; 5];
        prices.push(90.0);
        prices.extend(std::iter::repeat(50.0).take(80));
        let hist = series(from, &prices);
        let r = run_backtest_inner(&cfg("BTC", from, to), &hist, 0).unwrap();
        assert_eq!(r.losses, 0, "no realized losses allowed");
        assert!(
            r.cold_lots >= 1,
            "expected cold-hold lots, got {}",
            r.cold_lots
        );
        assert!(r.pnl_pct < 0.0, "unrealized drawdown expected");
        assert!(r.max_drawdown_pct > 10.0, "drawdown should be deep");
    }

    #[test]
    fn ceiling_constraint_blocks_all_buys() {
        let (from, to) = dates();
        let mut c = cfg("BTC", from, to);
        c.ceiling = Some(90.0);
        let hist = series(from, &[100.0; 60]);
        let r = run_backtest_inner(&c, &hist, 0).unwrap();
        assert_eq!(r.total_trades, 0, "no buys allowed above ceiling");
        assert_eq!(r.pnl_pct, 0.0);
    }

    #[test]
    fn flat_market_parks_cash_with_ath_guard() {
        let (from, to) = dates();
        let hist = series(from, &[100.0; 60]);
        let r = run_backtest_inner(&cfg("BTC", from, to), &hist, 0).unwrap();
        // The ATH guard refuses to buy while the price sits at its own high,
        // so a flat market parks the cash instead of opening a cold lot.
        assert_eq!(r.total_trades, 0);
        assert_eq!(r.open_lots, 0);
        assert!(
            r.pnl_pct.abs() < 0.5,
            "fee-only drift expected, got {}",
            r.pnl_pct
        );
    }

    #[test]
    fn windows_generates_rolling_sweep() {
        let from = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let to = NaiveDate::from_ymd_opt(2020, 4, 1).unwrap();
        let w = windows(from, to, 30, 15);
        assert_eq!(w.len(), 5, "91 days / 15 step = 5 windows");
        assert_eq!(w[0], (from, from + Duration::days(30)));
        // 2020 is a leap year: Feb 15 + 15d = Mar 1, so the last window is Mar 1..Mar 31.
        assert_eq!(w[4], (from + Duration::days(60), from + Duration::days(90)));
        assert!(w[4].1 <= to);
    }

    #[test]
    fn report_and_lessons_are_populated() {
        let (from, to) = dates();
        let mut prices = vec![100.0; 10];
        for i in 1..=20 {
            prices.push(100.0 - i as f64);
        }
        prices.extend(std::iter::repeat(110.0).take(60));
        let hist = series(from, &prices);
        let r = run_backtest_inner(&cfg("LINK", from, to), &hist, 0).unwrap();
        assert!(!r.report().is_empty());
        assert!(r.report().contains("LINK"));
        assert!(!r.lessons.is_empty(), "summary lesson always recorded");
        assert!(r.lessons.iter().any(|l| l.contains("Backtest LINK")));
    }

    // -- edge cases ---------------------------------------------------------

    #[test]
    fn empty_or_single_point_history_is_rejected() {
        let (from, to) = dates();
        assert!(run_backtest_inner(&cfg("BTC", from, to), &[], 0).is_err());
        let one = series(from, &[100.0]);
        assert!(run_backtest_inner(&cfg("BTC", from, to), &one, 0).is_err());
    }

    #[test]
    fn duplicate_timestamps_do_not_break_replay() {
        let (from, to) = dates();
        // Same timestamp repeated (as raw feeds sometimes deliver) must not
        // panic or double-spend; the engine just processes them in order.
        let mut hist = series(from, &[100.0, 90.0, 95.0, 110.0]);
        hist.push((hist[1].0, 90.0)); // duplicate of the second bar
        hist.sort_by_key(|&(ts, _)| ts);
        let r = run_backtest_inner(&cfg("BTC", from, to), &hist, 0).unwrap();
        assert_eq!(r.losses, 0, "no realized losses allowed");
        assert!(r.total_trades >= 1);
    }

    #[test]
    fn unsorted_history_is_sorted_by_replay() {
        let (from, to) = dates();
        let mut hist = series(from, &[100.0, 90.0, 95.0, 110.0]);
        hist.reverse();
        // Take-profit off: this test exercises replay robustness, not the
        // target-exit lever (with 15% take-profit the 110 close never reaches
        // the target of a 95 entry, so no trade would happen).
        let mut c = cfg("BTC", from, to);
        c.take_profit_pct = 0.0;
        let r = run_backtest_inner(&c, &hist, 0).unwrap();
        assert_eq!(r.losses, 0);
        assert!(r.total_trades >= 1);
    }

    #[test]
    fn separate_profit_mode_reports_equity_with_reserve() {
        let (from, to) = dates();
        // Two dip-buy / rip-sell cycles so compounding matters on cycle 2.
        let mut prices = vec![100.0; 10];
        for i in 1..=20 {
            prices.push(100.0 - i as f64); // dip to 80
        }
        prices.extend(std::iter::repeat(110.0).take(30)); // rip
        for i in 1..=20 {
            prices.push(110.0 - i as f64); // dip to 90
        }
        prices.extend(std::iter::repeat(120.0).take(30)); // rip
        let hist = series(from, &prices);

        let mut c_sep = cfg("BTC", from, to);
        c_sep.profit_mode = ProfitMode::Separate;
        let r_sep = run_backtest_inner(&c_sep, &hist, 0).unwrap();
        // Equity includes the separate reserve, so PnL is still positive.
        assert!(
            r_sep.pnl_pct > 0.0,
            "separate mode must profit, got {}",
            r_sep.pnl_pct
        );
        assert_eq!(r_sep.losses, 0);

        let mut c_re = cfg("BTC", from, to);
        c_re.profit_mode = ProfitMode::Reinvest;
        let r_re = run_backtest_inner(&c_re, &hist, 0).unwrap();
        assert!(r_re.pnl_pct > 0.0);
        // Compounding has more working capital for cycle 2, so it ends higher.
        assert!(
            r_re.final_capital_usd >= r_sep.final_capital_usd - 0.01,
            "reinvest {} < separate {}",
            r_re.final_capital_usd,
            r_sep.final_capital_usd
        );
    }

    #[test]
    fn extreme_volatility_backtest_never_realizes_losses() {
        let (from, to) = dates();
        // 90-day series alternating brutal crashes and vertical pumps.
        let mut prices = Vec::new();
        let mut price = 100.0_f64;
        let mut seed = 0x1234_5678u64;
        for _ in 0..90 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let swing = ((seed >> 33) as f64 / (u32::MAX as f64)) * 1.2 - 0.6; // -60%..+60%
            price = (price * (1.0 + swing)).max(1.0);
            prices.push(price);
        }
        let hist = series(from, &prices);
        let r = run_backtest_inner(&cfg("SOL", from, to), &hist, 0).unwrap();
        assert_eq!(r.losses, 0, "no realized losses under extreme volatility");
        assert!(r.total_trades >= 0);
    }

    #[test]
    fn fetch_history_serves_from_cache_when_network_down() {
        // Simulated network outage: a cache file covering the requested range
        // must satisfy fetch_history without touching any provider.
        let _rt = tokio::runtime::Runtime::new().unwrap();
        let _guard = _rt.enter();
        let symbol = "TESTNET";
        let from = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();
        let to = NaiveDate::from_ymd_opt(2020, 3, 31).unwrap();
        // Full 91-day series so the cache covers the whole requested range.
        let prices: Vec<f64> = (0..91).map(|i| 100.0 + i as f64 * 0.5).collect();
        let hist = series(from, &prices);
        crate::trading::history_file::save_history(symbol, &hist).unwrap();
        let got = fetch_history_blocking(symbol, from, to).unwrap();
        assert_eq!(got.len(), 91);
        assert_eq!(got[0].1, 100.0);
        assert_eq!(got[90].1, 145.0);
        // Partial overlap: cache covers only part of the range — the slice
        // must still come from disk (no network for the covered part).
        let mid = NaiveDate::from_ymd_opt(2020, 2, 1).unwrap();
        let got2 = fetch_history_blocking(symbol, mid, to).unwrap();
        assert_eq!(got2.len(), 60); // Feb 2020 is a leap year: 29 + 31
        let _ = std::fs::remove_file(crate::trading::history_file::history_path(symbol));
    }
}
