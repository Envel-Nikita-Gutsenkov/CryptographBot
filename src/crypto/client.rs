//! Unified crypto client: parallel fetch from all providers, consensus price,
//! health tracking, shared L1 cache.

use moka::future::Cache;
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;
use tracing::{debug, warn};

use super::{
    consensus::{build_consensus, ConsensusResult},
    health::HealthTracker,
    providers::{
        alternative_me::{AlternativeMeProvider, FearAndGreed},
        binance::BinanceProvider,
        bitfinex::BitfinexProvider,
        bitstamp::BitstampProvider,
        bybit::BybitProvider,
        cmc::CmcProvider,
        coinbase::CoinbaseProvider,
        coincap::CoinCapProvider,
        coingecko::CoinGeckoProvider,
        coinpaprika::CoinPaprikaProvider,
        cryptocompare::{CryptoCompareProvider, NewsArticle},
        kraken::KrakenProvider,
        okx::OkxProvider,
        rss::RssProvider,
        signals::SignalsProvider,
        RawQuote,
    },
};
use crate::crypto::providers::coinstats::CoinStatsProvider;
use crate::crypto::providers::defillama::DefiLlamaProvider;
use crate::crypto::providers::dexscreener::DexScreenerProvider;
use crate::crypto::providers::geckoterminal::GeckoTerminalProvider;
use crate::crypto::providers::mempool::MempoolProvider;
use crate::crypto::providers::yfinance::YFinanceProvider;
use crate::{
    db::SqliteStore,
    error::{BotError, Result},
    models::{CmcQuote, GlobalMetrics},
};

const PROVIDER_TIMEOUT: Duration = Duration::from_secs(6);
const HISTORY_TIMEOUT: Duration = Duration::from_secs(30);
const L1_TTL: Duration = Duration::from_secs(180);
const TOP_TTL: Duration = Duration::from_secs(300);
const BYN_TTL: Duration = Duration::from_secs(86_400);
/// Headlines move on the scale of hours, not minutes: re-reading the same 8
/// feeds every symbol every few minutes only burns bandwidth and rate limits.
const NEWS_TTL: Duration = Duration::from_secs(600);
/// How long to stop asking CryptoCompare for news after it rate-limits us.
const NEWS_CC_COOLDOWN: Duration = Duration::from_secs(1_800);

struct CryptoClientInner {
    cmc: CmcProvider,
    coingecko: CoinGeckoProvider,
    coincap: CoinCapProvider,
    coinpaprika: CoinPaprikaProvider,
    alternative_me: AlternativeMeProvider,
    binance: BinanceProvider,
    bitfinex: BitfinexProvider,
    bitstamp: BitstampProvider,
    bybit: BybitProvider,
    okx: OkxProvider,
    kraken: KrakenProvider,
    coinbase: CoinbaseProvider,
    cryptocompare: CryptoCompareProvider,
    defillama: DefiLlamaProvider,
    dexscreener: DexScreenerProvider,
    geckoterminal: GeckoTerminalProvider,
    onchain: crate::crypto::providers::onchain::OnChainProvider,
    yfinance: YFinanceProvider,
    rss: RssProvider,
    signals: SignalsProvider,
    mempool: MempoolProvider,
    #[allow(dead_code)] // Reserved for future risk-scoring integration in Market Intel Worker
    coinstats: CoinStatsProvider,
    health: Arc<HealthTracker>,
    cache: Cache<String, CmcQuote>,
    cache_meta: Cache<String, CmcQuote>,
    cache_onchain: Cache<String, crate::models::onchain::OnChainMetrics>,
    cache_top: Cache<u32, Vec<CmcQuote>>,
    cache_global: Cache<(), GlobalMetrics>,
    cache_byn: Cache<(), f64>,
    /// Merged RSS feed shared by every caller. The feeds are market-wide, not
    /// per coin, so fetching them once per symbol meant 8 HTTP requests per
    /// symbol per cycle for byte-identical content.
    cache_news: Cache<(), Vec<NewsArticle>>,
    /// Set when CryptoCompare news fails (it is almost always rate limited), so
    /// the following symbols go straight to RSS instead of each burning a
    /// request on a guaranteed 429.
    news_cc_cooldown: Cache<(), ()>,
    http: reqwest::Client,
    sqlite: Option<Arc<SqliteStore>>,
}

pub struct CryptoClient {
    inner: Arc<CryptoClientInner>,
}

impl CryptoClient {
    pub fn new(
        cmc_key: &str,
        cg_key: &str,
        cc_key: &str,
        cryptocompare_key: &str,
        sqlite: Option<Arc<SqliteStore>>,
    ) -> Self {
        let health = Arc::new(HealthTracker::new(&[
            CmcProvider::NAME,
            CoinGeckoProvider::NAME,
            CoinCapProvider::NAME,
            CoinPaprikaProvider::NAME,
            AlternativeMeProvider::NAME,
            BinanceProvider::NAME,
            BybitProvider::NAME,
            CoinbaseProvider::NAME,
            OkxProvider::NAME,
            KrakenProvider::NAME,
            BitfinexProvider::NAME,
            BitstampProvider::NAME,
            crate::crypto::providers::cryptocompare::NAME,
            DefiLlamaProvider::NAME,
            DexScreenerProvider::NAME,
            GeckoTerminalProvider::NAME,
            RssProvider::NAME,
        ]));
        let cache: Cache<String, CmcQuote> = Cache::builder()
            .time_to_live(L1_TTL)
            .max_capacity(500)
            .build();
        let ws_cache = cache.clone();
        tokio::spawn(async move {
            crate::crypto::ws_engine::start_binance_ws(ws_cache).await;
        });

        let inner = CryptoClientInner {
            cmc: CmcProvider::new(cmc_key),
            coingecko: CoinGeckoProvider::new(cg_key),
            coincap: CoinCapProvider::new(cc_key),
            coinpaprika: CoinPaprikaProvider::new(),
            alternative_me: AlternativeMeProvider::new(),
            binance: BinanceProvider::new(),
            bitfinex: BitfinexProvider::new(),
            bitstamp: BitstampProvider::new(),
            bybit: BybitProvider::new(),
            okx: OkxProvider::new(),
            kraken: KrakenProvider::new(),
            coinbase: CoinbaseProvider::new(),
            cryptocompare: CryptoCompareProvider::new(cryptocompare_key),
            defillama: DefiLlamaProvider::new(),
            dexscreener: DexScreenerProvider::new(),
            geckoterminal: GeckoTerminalProvider::new(),
            onchain: crate::crypto::providers::onchain::OnChainProvider::new(),
            yfinance: YFinanceProvider::new(),
            rss: RssProvider::new(),
            signals: SignalsProvider::new(),
            mempool: MempoolProvider::new(),
            coinstats: CoinStatsProvider::new(),
            health,
            cache,
            cache_meta: Cache::builder()
                .time_to_live(Duration::from_secs(86400))
                .max_capacity(5000)
                .build(),
            cache_onchain: Cache::builder()
                .time_to_live(Duration::from_secs(3600))
                .max_capacity(100)
                .build(),
            cache_top: Cache::builder()
                .time_to_live(TOP_TTL)
                .max_capacity(10)
                .build(),
            cache_global: Cache::builder()
                .time_to_live(TOP_TTL)
                .max_capacity(1)
                .build(),
            cache_byn: Cache::builder()
                .time_to_live(BYN_TTL)
                .max_capacity(1)
                .build(),
            cache_news: Cache::builder()
                .time_to_live(NEWS_TTL)
                .max_capacity(4)
                .build(),
            news_cc_cooldown: Cache::builder()
                .time_to_live(NEWS_CC_COOLDOWN)
                .max_capacity(4)
                .build(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                .build()
                .unwrap(),
            sqlite,
        };
        Self {
            inner: Arc::new(inner),
        }
    }

    pub async fn get_onchain_metrics(
        &self,
        symbols: &[&str],
    ) -> Vec<crate::models::onchain::OnChainMetrics> {
        let mut out = Vec::new();
        let mut missing = Vec::new();
        for &sym in symbols {
            let key = sym.to_uppercase();
            match self.inner.cache_onchain.get(&key).await {
                Some(m) => out.push(m),
                None => missing.push(sym.to_string()),
            }
        }

        if missing.is_empty() {
            return out;
        }

        let missing_refs: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
        // Bound the whole on-chain fetch: it makes sequential per-symbol HTTP
        // calls (CoinMetrics + 2× Binance each) that can stack to minutes when
        // providers are slow/blocked — the /ask pipeline awaits this.
        let new_metrics = match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            self.inner.onchain.get_metrics(&missing_refs),
        )
        .await
        {
            Ok(m) => m,
            Err(_) => {
                tracing::warn!("on-chain metrics fetch timed out after 20s");
                Vec::new()
            }
        };

        for m in new_metrics {
            self.inner
                .cache_onchain
                .insert(m.symbol.clone(), m.clone())
                .await;
            out.push(m);
        }

        out
    }

    /// Fetch Bitcoin network health metrics (hashrate, mempool, fees, difficulty).
    pub async fn get_btc_network_metrics(&self) -> crate::models::market_intel::BtcNetworkMetrics {
        self.inner.mempool.get_btc_network_metrics().await
    }

    /// Fetch CoinStats risk/liquidity scores for given symbols.
    #[allow(dead_code)] // Reserved for future risk-scoring integration in Market Intel Worker
    pub async fn get_coin_risk_scores(
        &self,
        symbols: &[&str],
    ) -> Vec<crate::models::market_intel::CoinStatsRisk> {
        self.inner.coinstats.get_risk_scores(symbols).await
    }

    pub async fn get_tradfi_prices(
        &self,
        symbols: &[&str],
    ) -> Vec<crate::crypto::providers::yfinance::TradFiQuote> {
        let mut out = Vec::new();
        let mut tasks = Vec::new();
        for &sym in symbols {
            let inner = Arc::clone(&self.inner);
            let s = sym.to_string();
            tasks.push(tokio::spawn(async move {
                inner.yfinance.get_quote(&s).await.ok()
            }));
        }
        for res in futures::future::join_all(tasks).await {
            if let Ok(Some(quote)) = res {
                out.push(quote);
            }
        }
        out
    }

    pub fn sqlite(&self) -> Option<std::sync::Arc<SqliteStore>> {
        self.inner.sqlite.clone()
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<CmcQuote>> {
        let mut out = Vec::new();
        let mut missing = Vec::new();
        for &sym in symbols {
            let key = sym.to_uppercase();
            match self.inner.cache.get(&key).await {
                Some(q) => out.push(q),
                None => missing.push(sym.to_string()),
            }
        }

        if missing.is_empty() {
            // Full cache hit: no network fan-out. Price freshness is maintained
            // by the scheduled workers (price-prefetch / alert-engine) and by
            // the Binance WS stream, which rewrites cache entries on every
            // tick. Spawning a background consensus here caused a ~6x
            // amplification of provider fan-out (~1300 sched/day).
            return Ok(out);
        }

        let missing_refs: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
        let (mut consensus_quotes, needs_refresh) = self.inner.fetch_consensus(&missing_refs).await;

        if !needs_refresh.is_empty() {
            // Milestone refresh fetches 730d history per symbol — expensive and
            // unbounded (provider fallback chain). Run it detached so the caller
            // (e.g. the /ask pipeline) never blocks on it; the SQLite backfill
            // below just uses whatever milestones already exist.
            for (sym, price) in needs_refresh {
                let inner = Arc::clone(&self.inner);
                tokio::spawn(async move {
                    inner.refresh_price_milestones(&sym, price).await;
                });
            }

            for quote in &mut consensus_quotes {
                let sym = &quote.symbol;
                let current_price = quote.price_usd;
                if current_price > 0.0 {
                    if let Some(sqlite) = &self.inner.sqlite {
                        if let Ok(milestones) = sqlite.get_price_milestones(sym).await {
                            for (milestone, price, _) in &milestones {
                                if price <= &0.0 {
                                    continue;
                                }
                                let pct = (current_price - price) / price * 100.0;
                                match milestone.as_str() {
                                    "price_1d_ago" if quote.change_24h.unwrap_or(0.0) == 0.0 => {
                                        quote.change_24h = Some(pct)
                                    }
                                    "price_7d_ago" if quote.change_7d.unwrap_or(0.0) == 0.0 => {
                                        quote.change_7d = Some(pct)
                                    }
                                    "price_30d_ago" if quote.change_30d.unwrap_or(0.0) == 0.0 => {
                                        quote.change_30d = Some(pct)
                                    }
                                    "price_90d_ago" if quote.change_90d.unwrap_or(0.0) == 0.0 => {
                                        quote.change_90d = Some(pct)
                                    }
                                    "price_365d_ago" if quote.change_365d.is_none() => {
                                        quote.change_365d = Some(pct)
                                    }
                                    "price_730d_ago" if quote.change_730d.is_none() => {
                                        quote.change_730d = Some(pct)
                                    }
                                    "peak_2y" => {
                                        // Sanitize: ignore implausible 2y peaks (poisoned
                                        // history) — cap at 100x the current price.
                                        let mut p = *price;
                                        if p > current_price * 100.0 {
                                            p = current_price;
                                        }
                                        let peak =
                                            p.max(quote.ath.unwrap_or(0.0)).max(current_price);
                                        quote.ath = Some(peak);
                                        if peak > 0.0 {
                                            quote.ath_change_percentage =
                                                Some((current_price - peak) / peak * 100.0);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            }
        }

        for q in consensus_quotes {
            // Canonical cache key: `get_quotes` looks up `sym.to_uppercase()`,
            // so storing the raw provider casing (`btc`) missed every lookup and
            // could even let two spellings of one asset shadow each other.
            self.inner
                .cache
                .insert(q.symbol.to_uppercase(), q.clone())
                .await;
            out.push(q);
        }
        Ok(out)
    }

    /// Warm price milestones for a list of symbols at startup.
    /// Schedules background refresh for each symbol that lacks complete milestone data.
    pub async fn warm_milestones(&self, symbols: &[String]) {
        let sqlite = match &self.inner.sqlite {
            Some(s) => s.clone(),
            None => return,
        };
        // Check which symbols need refresh in one batch query
        let missing = match sqlite.get_symbols_without_milestones().await {
            Ok(m) => m,
            Err(_) => return,
        };
        // Also trigger for explicitly requested symbols not in DB yet
        let need: std::collections::HashSet<String> =
            missing.into_iter().chain(symbols.iter().cloned()).collect();
        for sym in need {
            self.inner.spawn_milestone_refresh(sym, 0.0);
        }
    }

    pub async fn get_top(&self, limit: u32) -> Result<Vec<CmcQuote>> {
        if let Some(c) = self.inner.cache_top.get(&limit).await {
            // Serve from cache; the scheduled price-prefetch worker refreshes
            // this on its own cadence, so no background spawn-on-hit is needed.
            return Ok(c);
        }
        self.inner.fetch_top_direct(limit).await
    }

    pub async fn get_global_metrics(&self) -> Result<GlobalMetrics> {
        if let Some(c) = self.inner.cache_global.get(&()).await {
            return Ok(c);
        }
        let mut m = match self.inner.cmc.get_global().await {
            Ok(metrics) => metrics,
            Err(e) => {
                tracing::warn!(
                    "CMC global metrics failed ({}), falling back to CoinGecko",
                    e
                );
                match self.inner.coingecko.get_global().await {
                    Ok(metrics) => metrics,
                    Err(e2) => {
                        tracing::warn!(
                            "CoinGecko global metrics failed ({}), falling back to Coinpaprika",
                            e2
                        );
                        match self.fetch_global_paprika().await {
                            Ok(metrics) => metrics,
                            Err(e3) => {
                                tracing::error!(
                                    "Coinpaprika global metrics failed ({}), no providers left.",
                                    e3
                                );
                                return Err(crate::error::BotError::Internal(anyhow::anyhow!(
                                    "All global metrics providers failed"
                                )));
                            }
                        }
                    }
                }
            }
        };

        // Inject DefiLlama Metrics
        let (stablecoin_mcap, dex_volume_24h) = self.inner.defillama.get_global_metrics().await;
        m.stablecoin_mcap = stablecoin_mcap;
        m.dex_volume_24h = dex_volume_24h;

        self.inner.cache_global.insert((), m.clone()).await;
        Ok(m)
    }

    async fn fetch_global_paprika(&self) -> Result<GlobalMetrics> {
        let resp = self
            .inner
            .http
            .get("https://api.coinpaprika.com/v1/global")
            .header("User-Agent", "CryptographBot/1.0")
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(BotError::CmcApi(format!(
                "Coinpaprika API error: {}",
                resp.status()
            )));
        }
        let body: PaprikaGlobal = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("Coinpaprika parse: {e}")))?;

        let eth_resp = self
            .inner
            .http
            .get("https://api.coinpaprika.com/v1/tickers/eth-ethereum")
            .header("User-Agent", "CryptographBot/1.0")
            .send()
            .await;

        let mut eth_dom = 0.0;
        if let Ok(resp) = eth_resp {
            if let Ok(v) = resp.json::<serde_json::Value>().await {
                if let Some(mcap) = v["quotes"]["USD"]["market_cap"].as_f64() {
                    if body.market_cap_usd > 0.0 {
                        eth_dom = (mcap / body.market_cap_usd) * 100.0;
                    }
                }
            }
        }

        Ok(GlobalMetrics {
            total_market_cap: body.market_cap_usd,
            total_volume_24h: body.volume_24h_usd,
            btc_dominance: body.bitcoin_dominance_percentage,
            eth_dominance: eth_dom,
            active_cryptocurrencies: body.cryptocurrencies_number,
            stablecoin_mcap: None,
            dex_volume_24h: None,
        })
    }

    pub async fn get_byn_rate(&self) -> Result<f64> {
        if let Some(r) = self.inner.cache_byn.get(&()).await {
            return Ok(r);
        }
        let today = chrono::Utc::now().format("%Y-%m-%d");
        let url = format!("https://api.nbrb.by/exrates/rates/431?ondate={today}");
        let v: serde_json::Value = self
            .inner
            .http
            .get(&url)
            .send()
            .await?
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("NBRB: {e}")))?;
        let rate = v["Cur_OfficialRate"].as_f64().unwrap_or(3.25);
        self.inner.cache_byn.insert((), rate).await;
        Ok(rate)
    }

    pub async fn to_usd(&self, amount: f64, symbol: &str) -> Result<f64> {
        match symbol {
            "USD" => Ok(amount),
            "BYN" => Ok(amount / self.get_byn_rate().await?),
            sym => {
                let quotes = self.get_quotes(&[sym]).await?;
                quotes
                    .first()
                    .map(|q| q.price_usd * amount)
                    .ok_or_else(|| BotError::SymbolNotFound(sym.into()))
            }
        }
    }

    pub fn take_health_deltas(&self) -> Vec<crate::models::MetricDelta> {
        self.inner.health.take_deltas()
    }

    pub async fn get_history(&self, symbol: &str, period: &str) -> Result<Vec<(i64, f64)>> {
        self.inner.get_history(symbol, period).await
    }

    /// News headlines relevant to `symbol` ("GENERAL" means the whole market).
    ///
    /// The RSS sources are market-wide, not per coin, so the merged feed is
    /// fetched once per [`NEWS_TTL`] and shared by every caller; each symbol then
    /// keeps only the articles that actually mention it. CryptoCompare is tried
    /// first, but one failure parks it for [`NEWS_CC_COOLDOWN`] — it is almost
    /// always rate-limited, and previously every symbol burned a request on a
    /// guaranteed error before falling back anyway.
    pub async fn get_news(&self, symbol: &str) -> Result<Vec<NewsArticle>> {
        if self.inner.news_cc_cooldown.get(&()).await.is_none() {
            match self.inner.cryptocompare.get_news(symbol).await {
                Ok(news) if !news.is_empty() => return Ok(news),
                Ok(_) => {
                    // The provider reports a rate limit as an empty success with
                    // only a `warn!` inside, so an empty result is the signal to
                    // stop asking: otherwise every symbol in the cycle burned a
                    // request on the same guaranteed-empty response.
                    debug!(
                        "CryptoCompare news empty for {symbol}; parking it for {}s",
                        NEWS_CC_COOLDOWN.as_secs()
                    );
                    self.inner.news_cc_cooldown.insert((), ()).await;
                }
                Err(e) => {
                    warn!(
                        "CryptoCompare news failed ({e}); skipping it for {}s",
                        NEWS_CC_COOLDOWN.as_secs()
                    );
                    self.inner.news_cc_cooldown.insert((), ()).await;
                }
            }
        }

        let all = match self.inner.cache_news.get(&()).await {
            Some(cached) => cached,
            None => {
                let fresh = self.inner.rss.get_all_news().await?;
                tracing::info!("RSS news refreshed: {} articles", fresh.len());
                self.inner.cache_news.insert((), fresh.clone()).await;
                fresh
            }
        };

        Ok(select_for_symbol(all, symbol))
    }

    pub async fn get_fng(&self) -> Result<FearAndGreed> {
        self.inner.alternative_me.get_fng().await
    }

    /// Free crowd signals: CMC headlines and trending searches, Binance
    /// announcements, and CoinGecko community votes. Every source is
    /// best-effort — a failing one is logged and skipped, and the caller gets
    /// whatever the others returned.
    pub async fn get_market_signals(
        &self,
        sentiment_symbols: &[&str],
    ) -> Vec<crate::models::MarketSignal> {
        use crate::models::MarketSignal;

        let (news, trending, announcements) = tokio::join!(
            self.inner.signals.market_news(20),
            self.inner.signals.trending(),
            self.inner.signals.binance_announcements(20),
        );

        let mut out = Vec::new();
        for (name, res) in [
            ("CMC headlines", news),
            ("CMC trending", trending),
            ("Binance announcements", announcements),
        ] {
            match res {
                Ok(mut v) => out.append(&mut v),
                Err(e) => debug!("Signal source {name} failed: {e}"),
            }
        }

        for (sym, up, down) in self.inner.coingecko.get_sentiment(sentiment_symbols).await {
            out.push(
                MarketSignal::new(
                    MarketSignal::KIND_SENTIMENT,
                    "coingecko",
                    &sym,
                    format!("Community sentiment: {up:.0}% bullish / {down:.0}% bearish"),
                )
                .with_score(up)
                .with_url(format!("coingecko://sentiment/{sym}")),
            );
        }

        out
    }
}

impl CryptoClientInner {
    /// Sanity check for a history series: it must be non-empty and, when we know
    /// the current price of the symbol (from the quote cache), its median must
    /// be in the same ballpark (0.01x..100x). Rejects providers that return
    /// empty arrays as "success" or garbage prices for a wrong token/id.
    fn is_sane_history(ref_price: Option<f64>, pts: &[(i64, f64)]) -> bool {
        if pts.is_empty() {
            return false;
        }
        if let Some(rp) = ref_price {
            if rp > 0.0 {
                let mut prices: Vec<f64> = pts.iter().map(|(_, p)| *p).collect();
                prices.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let med = prices[prices.len() / 2];
                if med <= 0.0 {
                    return false;
                }
                let ratio = med / rp;
                if ratio > 100.0 || ratio < 0.01 {
                    return false;
                }
            }
        }
        true
    }

    pub async fn get_history(&self, symbol: &str, period: &str) -> Result<Vec<(i64, f64)>> {
        // Global bound: the provider fallback chain (3 parallel + DefiLlama +
        // GeckoTerminal + SQLite) is sequential and was previously unbounded,
        // which let a slow provider hang callers like the /ask pipeline.
        match tokio::time::timeout(HISTORY_TIMEOUT, self.get_history_inner(symbol, period)).await {
            Ok(r) => r,
            Err(_) => Err(BotError::CmcApi(format!(
                "history fetch for {symbol} timed out after {}s",
                HISTORY_TIMEOUT.as_secs()
            ))),
        }
    }

    async fn get_history_inner(&self, symbol: &str, period: &str) -> Result<Vec<(i64, f64)>> {
        let days = match period {
            "1d" | "1" => 1,
            "7d" | "7" => 7,
            "30d" | "30" => 30,
            "90d" | "90" => 90,
            "180d" | "180" => 180,
            "365d" | "365" | "1y" => 365,
            s => s
                .strip_suffix('d')
                .and_then(|x| x.parse::<u32>().ok())
                .unwrap_or_else(|| s.parse::<u32>().unwrap_or(1)),
        };

        // Reference price (consensus-validated) to reject implausible series.
        let ref_price = self
            .cache
            .get(&symbol.to_uppercase())
            .await
            .map(|q| q.price_usd);
        let days_str = days.to_string();

        // 1. Fetch the three main history providers IN PARALLEL (health-gated)
        //    and let them agree: a single deviant series is dropped, and a
        //    consistently failing provider gets temporarily banned by the
        //    health tracker (same fail-resistance as the quote consensus).
        let cg = self.fetch_history_guarded(
            CoinGeckoProvider::NAME,
            self.coingecko.get_history(symbol, &days_str),
        );
        let bi = self.fetch_history_guarded(
            BinanceProvider::NAME,
            self.binance.get_history(symbol, days),
        );
        let cc = self.history_from_cc(symbol, days);
        let (cg, bi, cc) = tokio::join!(cg, bi, cc);

        let mut candidates: Vec<Vec<(i64, f64)>> = Vec::new();
        for h in [cg, bi, cc].into_iter().flatten() {
            candidates.push(h);
        }
        if let Some(picked) = Self::pick_history_consensus(ref_price, &candidates) {
            return Ok(crate::crypto::charts::sanitize_history(&picked));
        }

        // 2. Fallback chain (single providers, still sanity-checked).
        if let Some(cg_id) = self.coingecko.get_id_for_symbol(symbol).await {
            if self.health.should_probe(DefiLlamaProvider::NAME) {
                match self.defillama.get_history(&cg_id, &days.to_string()).await {
                    Ok(h) if Self::is_sane_history(ref_price, &h) => {
                        return Ok(crate::crypto::charts::sanitize_history(&h))
                    }
                    Ok(_) => tracing::warn!(
                        "DefiLlama history for {} empty/implausible, falling back to GeckoTerminal",
                        symbol
                    ),
                    Err(e) => {
                        self.health
                            .record_err(DefiLlamaProvider::NAME, &e.to_string());
                        tracing::warn!(
                            "DefiLlama history failed ({}), falling back to GeckoTerminal",
                            e
                        );
                    }
                }
            }
        }

        match self
            .geckoterminal
            .get_history(symbol, &days.to_string())
            .await
        {
            Ok(h) if Self::is_sane_history(ref_price, &h) => {
                return Ok(crate::crypto::charts::sanitize_history(&h))
            }
            Ok(_) => tracing::warn!(
                "GeckoTerminal history for {} empty/implausible, falling back to local SQLite",
                symbol
            ),
            Err(e) => tracing::warn!(
                "GeckoTerminal history failed ({}), falling back to local SQLite",
                e
            ),
        }

        // 3. Last resort: local SQLite samples.
        if let Some(sqlite) = &self.sqlite {
            if let Ok(history) = sqlite.get_price_history(symbol, days).await {
                if Self::is_sane_history(ref_price, &history) {
                    return Ok(crate::crypto::charts::sanitize_history(&history));
                }
            }
        }

        // 4. On-disk daily cache (`data/history/{SYM}.csv`, shipped with the
        // repo). Guarantees BTC/ETH/LINK/SOL still have a usable series when
        // every provider is down or rate-limited — this is what produced
        // "No provider returned usable history for BTC/SOL" in the chat.
        if let Some(cached) = crate::trading::history_file::load_history(symbol) {
            let cutoff = chrono::Utc::now().timestamp_millis() - days as i64 * 86_400_000;
            let slice: Vec<(i64, f64)> =
                cached.into_iter().filter(|(ts, _)| *ts >= cutoff).collect();
            if slice.len() >= 2 {
                let age_days = chrono::Utc::now()
                    .timestamp_millis()
                    .saturating_sub(slice[slice.len() - 1].0)
                    / 86_400_000;
                tracing::warn!(
                    "History for {} served from the on-disk daily cache ({} candles, last close {}d old)",
                    symbol,
                    slice.len(),
                    age_days
                );
                return Ok(slice);
            }
        }

        Err(crate::error::BotError::CmcApi(format!(
            "No provider returned usable history for {}",
            symbol
        )))
    }

    /// Health-gated history fetch: skips providers that are temporarily banned
    /// and records failures so a consistently bad provider is excluded.
    /// CryptoCompare history gate: without a configured API key the provider
    /// always fails with 401, so skip it (without an error record) entirely.
    async fn history_from_cc(&self, symbol: &str, days: u32) -> Option<Vec<(i64, f64)>> {
        if !self.cryptocompare.is_configured() {
            return None;
        }
        self.fetch_history_guarded(
            crate::crypto::providers::cryptocompare::NAME,
            self.cryptocompare.get_history(symbol, days),
        )
        .await
    }

    async fn fetch_history_guarded<F>(&self, name: &'static str, fut: F) -> Option<Vec<(i64, f64)>>
    where
        F: std::future::Future<Output = crate::error::Result<Vec<(i64, f64)>>>,
    {
        if !self.health.should_probe(name) {
            return None;
        }
        match fut.await {
            Ok(h) if !h.is_empty() => {
                self.health.record_ok(name, 0.0);
                Some(h)
            }
            Ok(_) => None,
            Err(e) => {
                self.health.record_err(name, &e.to_string());
                None
            }
        }
    }

    /// Provider agreement: drop series whose median deviates >25% from the
    /// median-of-medians (the outlier provider), reject everything if the agreed
    /// level is implausible vs the known current price, and return the accepted
    /// series with the most points.
    fn pick_history_consensus(
        ref_price: Option<f64>,
        candidates: &[Vec<(i64, f64)>],
    ) -> Option<Vec<(i64, f64)>> {
        if candidates.is_empty() {
            return None;
        }
        let median = |prices: &[(i64, f64)]| -> f64 {
            let mut v: Vec<f64> = prices.iter().map(|(_, p)| *p).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            if v.is_empty() {
                0.0
            } else if v.len() % 2 == 0 {
                (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
            } else {
                v[v.len() / 2]
            }
        };
        let medians: Vec<f64> = candidates.iter().map(|c| median(c)).collect();
        let mut sorted = medians.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let anchor = if sorted.len() % 2 == 0 {
            (sorted[sorted.len() / 2 - 1] + sorted[sorted.len() / 2]) / 2.0
        } else {
            sorted[sorted.len() / 2]
        };
        if anchor <= 0.0 {
            return None;
        }
        // Absolute sanity: the agreed level must be in the same ballpark as the
        // consensus current price (catches ALL providers agreeing on a wrong token).
        if let Some(rp) = ref_price {
            if rp > 0.0 {
                let ratio = anchor / rp;
                if ratio > 20.0 || ratio < 0.05 {
                    return None;
                }
            }
        }
        let mut best: Option<&Vec<(i64, f64)>> = None;
        for (i, c) in candidates.iter().enumerate() {
            if (medians[i] - anchor).abs() / anchor > 0.25 {
                continue;
            }
            if best.map(|b| b.len() < c.len()).unwrap_or(true) {
                best = Some(c);
            }
        }
        best.cloned()
    }

    async fn fetch_consensus(&self, symbols: &[&str]) -> (Vec<CmcQuote>, Vec<(String, f64)>) {
        let (
            r_cmc,
            r_cg,
            r_cc,
            r_cp,
            r_alt,
            r_ccp,
            r_bi,
            r_by,
            r_cb,
            r_dl,
            r_ds,
            r_gt,
            r_okx,
            r_kr,
            r_bfx,
            r_bstp,
        ) = tokio::join!(
            self.fetch_from_cmc(symbols),
            self.fetch_from_cg(symbols),
            self.fetch_from_cc(symbols),
            self.fetch_from_cp(symbols),
            self.fetch_from_alt(symbols),
            self.fetch_from_ccp(symbols),
            self.fetch_from_bi(symbols),
            self.fetch_from_by(symbols),
            self.fetch_from_cb(symbols),
            self.fetch_from_dl(symbols),
            self.fetch_from_ds(symbols),
            self.fetch_from_gt(symbols),
            self.fetch_from_okx(symbols),
            self.fetch_from_kraken(symbols),
            self.fetch_from_bfx(symbols),
            self.fetch_from_bstp(symbols),
        );

        let mut by_sym: std::collections::HashMap<String, Vec<RawQuote>> =
            std::collections::HashMap::new();
        for batch in [
            r_cmc, r_cg, r_cc, r_cp, r_alt, r_ccp, r_bi, r_by, r_cb, r_dl, r_ds, r_gt, r_okx, r_kr,
            r_bfx, r_bstp,
        ]
        .into_iter()
        .flatten()
        {
            for q in batch {
                by_sym.entry(q.symbol.clone()).or_default().push(q);
            }
        }

        let mut results = Vec::new();
        let mut needs_refresh: Vec<(String, f64)> = Vec::new();
        for (sym, quotes) in by_sym {
            if let Some(ConsensusResult {
                mut quote,
                sources_used,
                outliers,
                confidence,
            }) = build_consensus(quotes)
            {
                if !outliers.is_empty() {
                    // One line per symbol per cycle, not per outlier, and at debug
                    // level: a DEX quote disagreeing with CEX consensus is normal,
                    // not an incident worth a WARN every 10 minutes.
                    let detail = outliers
                        .iter()
                        .map(|(s, p, d)| format!("{s} ${p:.6} ({d:.1}%)"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    debug!(
                        "Symbol {sym}: {} outlier quote(s): {detail}",
                        outliers.len()
                    );
                }

                // Use SQLite metadata backfill
                if quote.rank == 0 || quote.circulating_supply == 0.0 {
                    if let Some(sq) = &self.sqlite {
                        if let Ok(Some(meta)) = sq.get_quote_meta(&sym).await {
                            if quote.rank == 0 {
                                quote.rank = meta.rank;
                            }
                            if quote.circulating_supply == 0.0 {
                                quote.circulating_supply = meta.circulating_supply;
                            }
                        }
                    }
                }

                // Backfill missing percentage changes from SQLite milestones (fast, no HTTP)
                let current_price = quote.price_usd;
                if current_price > 0.0 {
                    if let Some(sqlite) = &self.sqlite {
                        if let Ok(milestones) = sqlite.get_price_milestones(&sym).await {
                            for (milestone, price, _recorded_at) in &milestones {
                                if price <= &0.0 {
                                    continue;
                                }
                                let pct = (current_price - price) / price * 100.0;
                                match milestone.as_str() {
                                    "price_1d_ago" if quote.change_24h.unwrap_or(0.0) == 0.0 => {
                                        quote.change_24h = Some(pct);
                                    }
                                    "price_7d_ago" if quote.change_7d.unwrap_or(0.0) == 0.0 => {
                                        quote.change_7d = Some(pct);
                                    }
                                    "price_30d_ago" if quote.change_30d.unwrap_or(0.0) == 0.0 => {
                                        quote.change_30d = Some(pct);
                                    }
                                    "price_90d_ago" if quote.change_90d.unwrap_or(0.0) == 0.0 => {
                                        quote.change_90d = Some(pct);
                                    }
                                    "price_365d_ago" if quote.change_365d.is_none() => {
                                        quote.change_365d = Some(pct);
                                    }
                                    "price_730d_ago" if quote.change_730d.is_none() => {
                                        quote.change_730d = Some(pct);
                                    }
                                    "peak_2y" => {
                                        // Sanitize: ignore implausible 2y peaks (poisoned
                                        // history) — cap at 100x the current price.
                                        let mut p = *price;
                                        if p > current_price * 100.0 {
                                            p = current_price;
                                        }
                                        let peak =
                                            p.max(quote.ath.unwrap_or(0.0)).max(current_price);
                                        quote.ath = Some(peak);
                                        if peak > 0.0 {
                                            quote.ath_change_percentage =
                                                Some((current_price - peak) / peak * 100.0);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    // Third independent ATH opinion (free, no key): CoinCap's
                    // own daily-history peak — used when the direct ATH fields
                    // (CoinGecko/CoinPaprika) are missing. Same 100x cap.
                    if quote.ath.is_none() && CoinCapProvider::is_available() {
                        if let Some(peak) = self.coincap.get_ath(&sym).await {
                            if peak <= current_price * 100.0 && peak >= current_price {
                                quote.ath = Some(peak);
                                quote.ath_change_percentage =
                                    Some((current_price - peak) / peak * 100.0);
                            }
                        }
                    }

                    // Trigger background milestone refresh if needed
                    let milestone_needs_refresh = quote.change_365d.is_none()
                        || quote.change_730d.is_none()
                        || quote.ath.is_none();
                    if milestone_needs_refresh {
                        needs_refresh.push((sym.clone(), current_price));
                    }
                }

                // If rich metadata is missing (because CMC/CoinGecko failed), salvage it from cache_meta
                if quote.rank == 0 {
                    let mut found_meta = None;
                    if let Some(meta) = self.cache_meta.get(&sym).await {
                        found_meta = Some(meta);
                    } else if let Some(sqlite) = &self.sqlite {
                        if let Ok(Some(meta)) = sqlite.get_quote_meta(&sym).await {
                            self.cache_meta.insert(sym.clone(), meta.clone()).await;
                            found_meta = Some(meta);
                        }
                    }

                    if let Some(meta) = found_meta {
                        quote.rank = meta.rank;
                        quote.circulating_supply = meta.circulating_supply;
                        if quote.circulating_supply > 0.0 {
                            quote.market_cap = quote.circulating_supply * quote.price_usd;
                        }
                    }
                } else {
                    self.cache_meta.insert(sym.clone(), quote.clone()).await;
                    if let Some(sqlite) = &self.sqlite {
                        sqlite.save_quote_meta(&quote).await;
                    }
                }

                for src in &sources_used {
                    self.health.record_ok(src, 0.0);
                }
                for (src, _, dev) in &outliers {
                    // Deviation is data, not an outage: update the rolling metric
                    // but never mark the source unhealthy because of it.
                    self.health.record_deviation(src, *dev);
                }
                debug!(
                    "Consensus {sym}: ${:.2} confidence={confidence:.2} sources={:?}",
                    quote.price_usd, sources_used
                );
                results.push(quote);
            }
        }
        (results, needs_refresh)
    }

    /// Spawn a background task to refresh milestones. Safe to call multiple times (idempotent).
    fn spawn_milestone_refresh(self: &Arc<Self>, sym: String, current_price: f64) {
        let inner = Arc::clone(self);
        tokio::spawn(async move {
            inner.refresh_price_milestones(&sym, current_price).await;
        });
    }

    /// Pick the 2-year peak from history, folding in the live price when it is
    /// known. `current_price == 0.0` means "not known" (startup warm-up), and
    /// the 100x sanity cap must not be applied in that case.
    fn two_year_peak(hist: &[(i64, f64)], current_price: f64) -> Option<(f64, i64)> {
        let max_point = hist
            .iter()
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;
        let mut peak = max_point.1;
        if current_price > 0.0 {
            peak = peak.max(current_price);
            if peak > current_price * 100.0 {
                peak = current_price;
            }
        }
        (peak > 0.0).then_some((peak, max_point.0))
    }

    /// Fetch 730-day history and persist all price milestones to SQLite.
    /// Runs in the background — never blocks the main consensus path.
    async fn refresh_price_milestones(&self, sym: &str, current_price: f64) {
        let sqlite = match &self.sqlite {
            Some(s) => s,
            None => return,
        };

        let hist = match self.get_history(sym, "730d").await {
            Ok(h) if !h.is_empty() => h,
            _ => return,
        };

        let now_ms = chrono::Utc::now().timestamp_millis();

        let day_ms = 24 * 3600 * 1000_i64;
        let milestones = [
            ("price_1d_ago", 1),
            ("price_7d_ago", 7),
            ("price_30d_ago", 30),
            ("price_90d_ago", 90),
            ("price_365d_ago", 365),
            ("price_730d_ago", 730),
        ];

        for (name, days) in &milestones {
            let target_ms = now_ms - (days * day_ms);
            let window = day_ms * 3;
            // Find the data point closest to the target age
            if let Some(closest) = hist
                .iter()
                .filter(|(ts, _)| (ts - target_ms).abs() < window)
                .min_by_key(|(ts, _)| (ts - target_ms).abs())
            {
                let recorded_at = chrono::DateTime::from_timestamp_millis(closest.0)
                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                sqlite
                    .save_price_milestone(sym, name, closest.1, &recorded_at)
                    .await;
            }
        }

        // Compute 2-year peak price. `current_price` can legitimately be 0.0
        // (the startup warm-up has no quote in hand), so the comparison against
        // it must be skipped rather than applied blindly — `peak > 0.0 * 100`
        // is always true and used to overwrite every watchlist peak with zero.
        if let Some((peak, peak_ts)) = Self::two_year_peak(&hist, current_price) {
            let recorded_at = chrono::DateTime::from_timestamp_millis(peak_ts)
                .map(|dt| dt.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| "unknown".to_string());
            sqlite
                .save_price_milestone(sym, "peak_2y", peak, &recorded_at)
                .await;
        }
    }

    async fn fetch_from_cmc(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(CmcProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.cmc.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health.record_err(CmcProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(CmcProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_cg(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(CoinGeckoProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.coingecko.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health
                    .record_err(CoinGeckoProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(CoinGeckoProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_cc(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        // api.coincap.io is a dead domain — don't spend a probe on it.
        if !CoinCapProvider::is_available() {
            return None;
        }
        if !self.health.should_probe(CoinCapProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.coincap.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health
                    .record_err(CoinCapProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(CoinCapProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_cp(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(CoinPaprikaProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.coinpaprika.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health
                    .record_err(CoinPaprikaProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(CoinPaprikaProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_alt(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(AlternativeMeProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.alternative_me.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health
                    .record_err(AlternativeMeProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health
                    .record_err(AlternativeMeProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_ccp(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.cryptocompare.is_configured()
            || !self
                .health
                .should_probe(crate::crypto::providers::cryptocompare::NAME)
        {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.cryptocompare.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health.record_err(
                    crate::crypto::providers::cryptocompare::NAME,
                    &err.to_string(),
                );
                None
            }
            Err(_) => {
                self.health
                    .record_err(crate::crypto::providers::cryptocompare::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_bi(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(BinanceProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.binance.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                // Low rate (~several per day): surfacing the real reason lets us
                // diagnose provider dips from logs instead of only counters.
                warn!("Binance quotes failed: {err}");
                self.health
                    .record_err(BinanceProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(BinanceProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_by(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(BybitProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.bybit.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health
                    .record_err(BybitProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(BybitProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_cb(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(CoinbaseProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.coinbase.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health
                    .record_err(CoinbaseProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(CoinbaseProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_okx(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(OkxProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.okx.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health.record_err(OkxProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(OkxProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_kraken(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(KrakenProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.kraken.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health
                    .record_err(KrakenProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(KrakenProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_bfx(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(BitfinexProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.bitfinex.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health
                    .record_err(BitfinexProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(BitfinexProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_bstp(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(BitstampProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.bitstamp.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health
                    .record_err(BitstampProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(BitstampProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_top_direct(&self, limit: u32) -> Result<Vec<CmcQuote>> {
        let raw: Vec<RawQuote> = match timeout(PROVIDER_TIMEOUT, self.cmc.get_top(limit)).await {
            Ok(Ok(v)) => {
                self.health.record_ok(CmcProvider::NAME, 0.0);
                v
            }
            _ => match timeout(PROVIDER_TIMEOUT, self.coingecko.get_top(limit)).await {
                Ok(Ok(v)) => {
                    self.health.record_ok(CoinGeckoProvider::NAME, 0.0);
                    v
                }
                _ => match timeout(PROVIDER_TIMEOUT, self.coincap.get_top(limit)).await {
                    Ok(Ok(v)) => v,
                    Ok(Err(e)) => return Err(e),
                    Err(_) => return Err(BotError::CmcApi("all providers timed out".into())),
                },
            },
        };
        let quotes: Vec<CmcQuote> = raw
            .into_iter()
            .map(|r: RawQuote| r.into_canonical())
            .collect();
        self.cache_top.insert(limit, quotes.clone()).await;
        Ok(quotes)
    }

    async fn fetch_from_dl(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(DefiLlamaProvider::NAME) {
            return None;
        }

        let mut pairs = Vec::new();
        for &s in symbols {
            if let Some(id) = self.coingecko.get_id_for_symbol(s).await {
                pairs.push((s, id));
            }
        }

        if pairs.is_empty() {
            return None;
        }

        let ref_pairs: Vec<(&str, &str)> = pairs.iter().map(|(s, id)| (*s, id.as_str())).collect();

        match timeout(PROVIDER_TIMEOUT, self.defillama.get_quotes(&ref_pairs)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health
                    .record_err(DefiLlamaProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(DefiLlamaProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_ds(&self, symbols: &[&str]) -> Option<Vec<RawQuote>> {
        if !self.health.should_probe(DexScreenerProvider::NAME) {
            return None;
        }
        match timeout(PROVIDER_TIMEOUT, self.dexscreener.get_quotes(symbols)).await {
            Ok(Ok(q)) => Some(q),
            Ok(Err(e)) => {
                let err: BotError = e;
                self.health
                    .record_err(DexScreenerProvider::NAME, &err.to_string());
                None
            }
            Err(_) => {
                self.health.record_err(DexScreenerProvider::NAME, "timeout");
                None
            }
        }
    }

    async fn fetch_from_gt(&self, _symbols: &[&str]) -> Option<Vec<RawQuote>> {
        // GeckoTerminal doesn't do batched get_quotes right now, used only for history
        None
    }
}

/// Per-coin aliases used to pull a symbol's own stories out of a market-wide
/// feed. The ticker alone is not enough: publishers write "ether", "solana"
/// and "Chainlink" far more often than "ETH"/"SOL"/"LINK".
///
/// Returns `None` for symbols we have no aliases for — those keep the full feed
/// instead of getting an empty list.
pub(crate) fn aliases_for(symbol: &str) -> Option<&'static [&'static str]> {
    Some(match symbol {
        "BTC" => &["btc", "bitcoin"],
        "ETH" => &["eth", "ethereum", "ether", "vitalik"],
        "SOL" => &["sol", "solana"],
        "LINK" => &["link", "chainlink"],
        "BNB" => &["bnb", "binance coin", "bnb chain", "bsc"],
        "XRP" => &["xrp", "ripple"],
        "ADA" => &["ada", "cardano"],
        "DOGE" => &["doge", "dogecoin"],
        "AVAX" => &["avax", "avalanche"],
        "DOT" => &["dot", "polkadot"],
        "MATIC" | "POL" => &["matic", "polygon"],
        "TON" => &["toncoin", "the open network"],
        "TRX" => &["trx", "tron"],
        "LTC" => &["ltc", "litecoin"],
        "SHIB" => &["shib", "shiba"],
        "UNI" => &["uni", "uniswap"],
        "ATOM" => &["atom", "cosmos"],
        "NEAR" => &["near protocol", "nearprot"],
        "ARB" => &["arb", "arbitrum"],
        "INJ" => &["inj", "injective"],
        "SUI" => &["sui"],
        "APT" => &["apt", "aptos"],
        "SEI" => &["sei"],
        "TIA" => &["tia", "celestia"],
        "OP" => &["optimism"],
        "RNDR" | "RENDER" => &["rndr", "render token", "render network"],
        "PEPE" => &["pepe"],
        _ => return None,
    })
}

/// Keeps only the articles that mention `symbol`, falling back to the newest
/// slice of the feed when a known coin simply has no coverage in this batch
/// (better a market digest than an empty summary).
fn select_for_symbol(articles: Vec<NewsArticle>, symbol: &str) -> Vec<NewsArticle> {
    let symbol = symbol.trim().to_uppercase();
    if symbol == "GENERAL" {
        return articles;
    }
    let Some(aliases) = aliases_for(&symbol) else {
        return articles;
    };

    let mut needles: Vec<String> = aliases.iter().map(|a| a.to_string()).collect();
    if symbol.len() > 1 {
        needles.push(symbol.to_lowercase());
    }

    let selected: Vec<NewsArticle> = articles
        .iter()
        .filter(|a| {
            let title = a.title.to_lowercase();
            let body = a.body.to_lowercase();
            needles
                .iter()
                .any(|n| contains_word_lower(&title, n) || contains_word_lower(&body, n))
        })
        .cloned()
        .collect();

    if selected.is_empty() {
        // No coin-specific coverage right now: hand back the freshest few
        // market stories so the summary still has something to chew on.
        return articles.into_iter().take(20).collect();
    }
    selected
}

/// Case-insensitive whole-word containment. `haystack` must already be
/// lowercased. Boundary checks stop "sol" from matching "solution" and "link"
/// from matching "linking".
pub(crate) fn contains_word_lower(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    let mut start = 0usize;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let end = abs + needle.len();
        let before_ok = haystack[..abs]
            .chars()
            .next_back()
            .map_or(true, |c| !c.is_alphanumeric());
        let after_ok = haystack[end..]
            .chars()
            .next()
            .map_or(true, |c| !c.is_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
        start = end;
        if start >= haystack.len() {
            break;
        }
    }
    false
}

#[derive(serde::Deserialize)]
struct PaprikaGlobal {
    market_cap_usd: f64,
    volume_24h_usd: f64,
    bitcoin_dominance_percentage: f64,
    cryptocurrencies_number: u32,
}

#[cfg(test)]
mod debug_tests {
    use super::*;

    #[test]
    fn news_filter_picks_coin_specific_articles() {
        let art = |title: &str, body: &str| NewsArticle {
            title: title.to_string(),
            source: "test".to_string(),
            body: body.to_string(),
            url: format!("https://example.com/{}", title.len()),
        };
        let feed = vec![
            art("Bitcoin ETF inflows hit record", "BTC keeps climbing"),
            art("Solana outage resolved", "validators restarted"),
            art("Ethereum upgrade ships", "the ether roadmap continues"),
        ];

        let btc = select_for_symbol(feed.clone(), "BTC");
        assert_eq!(btc.len(), 1);
        assert!(btc[0].title.contains("Bitcoin"));

        let eth = select_for_symbol(feed.clone(), "ETH");
        assert_eq!(eth.len(), 1);
        assert!(eth[0].title.contains("Ethereum"));

        // "GENERAL" and unknown tickers keep the whole feed.
        assert_eq!(select_for_symbol(feed.clone(), "GENERAL").len(), 3);
        assert_eq!(select_for_symbol(feed.clone(), "ZZZ").len(), 3);
    }

    #[test]
    fn news_filter_respects_word_boundaries() {
        assert!(contains_word_lower("bitcoin drops", "btc") == false);
        assert!(contains_word_lower("btc drops 5%", "btc"));
        assert!(!contains_word_lower("a solution for scaling", "sol"));
        assert!(contains_word_lower("sol rallies", "sol"));
        assert!(!contains_word_lower("linking chains", "link"));
        assert!(contains_word_lower("link surges", "link"));
    }

    #[test]
    fn news_filter_falls_back_when_coin_has_no_coverage() {
        let feed: Vec<NewsArticle> = (0..30)
            .map(|i| NewsArticle {
                title: format!("Market story {i}"),
                source: "test".to_string(),
                body: String::new(),
                url: format!("https://example.com/{i}"),
            })
            .collect();
        assert_eq!(select_for_symbol(feed, "LINK").len(), 20);
    }

    #[test]
    fn sane_history_rejects_empty() {
        assert!(!CryptoClientInner::is_sane_history(None, &[]));
    }

    #[test]
    fn sane_history_rejects_implausible_prices() {
        // Reference ETH price ~1860; a provider returning ~0.000016 is garbage.
        let garbage: Vec<(i64, f64)> = (0..10).map(|i| (i * 1000, 0.000016)).collect();
        assert!(!CryptoClientInner::is_sane_history(Some(1860.0), &garbage));
    }

    #[test]
    fn sane_history_accepts_plausible_prices() {
        let ok: Vec<(i64, f64)> = (0..10)
            .map(|i| (i * 1000, 1800.0 + i as f64 * 10.0))
            .collect();
        assert!(CryptoClientInner::is_sane_history(Some(1860.0), &ok));
    }

    #[test]
    fn sane_history_no_reference_accepts() {
        let pts: Vec<(i64, f64)> = vec![(1, 1.0), (2, 2.0)];
        assert!(CryptoClientInner::is_sane_history(None, &pts));
    }

    #[test]
    fn history_consensus_drops_deviant_provider() {
        // Two providers agree on ~1860, one returns garbage (10x off).
        let a: Vec<(i64, f64)> = (0..100).map(|i| (i * 1000, 1860.0)).collect();
        let b: Vec<(i64, f64)> = (0..50).map(|i| (i * 1000, 1870.0)).collect();
        let bad: Vec<(i64, f64)> = (0..200).map(|i| (i * 1000, 18600.0)).collect();
        let picked =
            CryptoClientInner::pick_history_consensus(Some(1860.0), &[a.clone(), b.clone(), bad]);
        assert!(picked.is_some());
        let picked = picked.unwrap();
        // The garbage provider (most points) must NOT win; the agreed level wins.
        assert!(picked.iter().all(|(_, p)| *p < 1900.0));
    }

    #[test]
    fn history_consensus_rejects_all_garbage() {
        // All providers agree but on an implausible level vs the current price.
        let a: Vec<(i64, f64)> = (0..10).map(|i| (i * 1000, 0.000016)).collect();
        let b: Vec<(i64, f64)> = (0..10).map(|i| (i * 1000, 0.000017)).collect();
        assert!(CryptoClientInner::pick_history_consensus(Some(1860.0), &[a, b]).is_none());
    }

    #[test]
    fn history_consensus_single_candidate_ok() {
        let a: Vec<(i64, f64)> = (0..10).map(|i| (i * 1000, 1860.0)).collect();
        assert!(CryptoClientInner::pick_history_consensus(Some(1860.0), &[a]).is_some());
    }

    #[test]
    fn history_consensus_empty_candidates() {
        assert!(CryptoClientInner::pick_history_consensus(None, &[]).is_none());
    }

    #[test]
    fn two_year_peak_keeps_history_peak_during_warm_up() {
        // The startup warm-up passes current_price = 0.0; the peak must still be
        // the real history maximum instead of being clamped to zero.
        let hist = vec![(1_000, 50_000.0), (2_000, 126_000.0), (3_000, 75_000.0)];
        let (peak, ts) = CryptoClientInner::two_year_peak(&hist, 0.0).unwrap();
        assert_eq!(peak, 126_000.0);
        assert_eq!(ts, 2_000);
    }

    #[test]
    fn two_year_peak_folds_in_live_price_and_caps_garbage() {
        let hist = vec![(1_000, 10.0), (2_000, 20.0)];
        // Live price above history wins.
        assert_eq!(
            CryptoClientInner::two_year_peak(&hist, 30.0).unwrap().0,
            30.0
        );
        // A history peak far above the live price is poisoned data.
        let poisoned = vec![(1_000, 10.0), (2_000, 5_000.0)];
        assert_eq!(
            CryptoClientInner::two_year_peak(&poisoned, 30.0).unwrap().0,
            30.0
        );
    }

    #[test]
    fn two_year_peak_rejects_empty_or_zero_history() {
        assert!(CryptoClientInner::two_year_peak(&[], 0.0).is_none());
        assert!(CryptoClientInner::two_year_peak(&[(1, 0.0)], 0.0).is_none());
    }
}
