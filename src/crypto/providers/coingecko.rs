use super::RawQuote;
use crate::error::{BotError, Result};
use moka::future::Cache;
use parking_lot::RwLock;
use reqwest::Client;
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::atomic::{AtomicI64, Ordering},
    sync::Arc,
    time::Duration,
};

/// Canonical CoinGecko ids for symbols whose naive "last match wins" lookup
/// resolves to bridged/wrapped copies of the same ticker (e.g. SOL ->
/// "wormhole-bridged-wrapped-sol-sui", whose ATH of $13.4k is a data glitch).
/// Pinning these guarantees the bot always quotes the primary asset.
///
/// Contents: every symbol the bot knows by name (src/bot/handlers/ask.rs)
/// plus the current top-50 coins by market cap — regenerated from the live
/// CoinGecko /coins/markets response, no legacy/dead listings.
const CANONICAL_IDS: &[(&str, &str)] = &[
    // ── Bot's known symbols ────────────────────────────────────────────────
    ("BTC", "bitcoin"),
    ("ETH", "ethereum"),
    ("SOL", "solana"),
    ("LINK", "chainlink"),
    ("ADA", "cardano"),
    ("XRP", "ripple"),
    ("DOT", "polkadot"),
    ("DOGE", "dogecoin"),
    ("SHIB", "shiba-inu"),
    ("AVAX", "avalanche-2"),
    ("MATIC", "matic-network"),
    ("POL", "polygon-ecosystem-token"),
    ("TON", "the-open-network"), // now listed as GRAM, id is stable
    ("TRX", "tron"),
    ("LTC", "litecoin"),
    ("NEAR", "near"),
    ("UNI", "uniswap"),
    ("ATOM", "cosmos"),
    ("XLM", "stellar"),
    ("ALGO", "algorand"),
    ("FTM", "fantom"),
    ("OP", "optimism"),
    ("ARB", "arbitrum"),
    ("SUI", "sui"),
    ("APT", "aptos"),
    // ── Current top-50 majors (live /coins/markets snapshot) ───────────────
    ("BNB", "binancecoin"),
    ("USDT", "tether"),
    ("USDC", "usd-coin"),
    ("DAI", "dai"),
    ("BCH", "bitcoin-cash"),
    ("HBAR", "hedera-hashgraph"),
    ("XMR", "monero"),
    ("ZEC", "zcash"),
    ("LEO", "leo-token"),
    ("CRO", "crypto-com-chain"),
    ("TAO", "bittensor"),
    ("OKB", "okb"),
    ("ONDO", "ondo-finance"),
    ("HYPE", "hyperliquid"),
    // ── Popular tickers with well-known bridged/wrapped name collisions ────
    ("PEPE", "pepe"),
    ("WIF", "dogwifcoin"),
    ("BONK", "bonk"),
    ("FLOKI", "floki"),
    ("INJ", "injective-protocol"),
    ("SEI", "sei-network"),
    ("TIA", "celestia"),
    ("JUP", "jupiter-exchange-solana"),
    ("PYTH", "pyth-network"),
    ("JTO", "jito-governance-token"),
    ("AAVE", "aave"),
    ("LDO", "lido-dao"),
    ("GRT", "the-graph"),
    ("FIL", "filecoin"),
    ("ICP", "internet-computer"),
];

pub struct CoinGeckoProvider {
    http: Client,
    api_key: String,
    /// symbol.upper() → coingecko_id, refreshed every 10 min
    id_map: Arc<RwLock<HashMap<String, String>>>,
    cache: Cache<String, RawQuote>,
    cache_top: Cache<u32, Vec<RawQuote>>,
    ban_until: AtomicI64,
}

impl CoinGeckoProvider {
    pub const NAME: &'static str = "coingecko";

    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                .build()
                .unwrap(),
            api_key: api_key.into(),
            id_map: Arc::new(RwLock::new(HashMap::new())),
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(60))
                .max_capacity(200)
                .build(),
            cache_top: Cache::builder()
                .time_to_live(Duration::from_secs(300))
                .max_capacity(10)
                .build(),
            ban_until: AtomicI64::new(0),
        }
    }

    async fn ensure_id_map(&self) -> Result<()> {
        if chrono::Utc::now().timestamp() < self.ban_until.load(Ordering::Relaxed) {
            return Err(BotError::RateLimited);
        }
        if !self.id_map.read().is_empty() {
            return Ok(());
        }

        // Build the whole map off-lock (parking_lot guards are !Send, so they
        // must never be held across an .await), then swap it in atomically.
        let mut built: HashMap<String, String> = HashMap::new();

        // 1) Pinned canonical ids — highest priority, never overwritten.
        for (sym, id) in CANONICAL_IDS {
            built.insert((*sym).to_string(), (*id).to_string());
        }

        // 2) Top-250 coins by market cap: the primary asset of a symbol has the
        //    largest cap, so first-wins here beats the alphabetical order of
        //    the full list. Only fills symbols not already pinned above.
        if let Ok(markets) = self.fetch_markets(250, 1).await {
            for m in markets {
                built.entry(m.symbol.to_uppercase()).or_insert(m.id);
            }
        }

        // 3) Full coin list: best-effort fill for long-tail symbols. First
        //    match wins — pinned/top-250 entries are never overwritten.
        let resp = self
            .http
            .get("https://api.coingecko.com/api/v3/coins/list")
            .query(&[("x_cg_demo_api_key", &self.api_key)])
            .send()
            .await?;
        if resp.status() == 429 {
            self.ban_until
                .store(chrono::Utc::now().timestamp() + 600, Ordering::Relaxed);
            return Err(BotError::RateLimited);
        }

        #[derive(Deserialize)]
        struct CgListElem {
            id: String,
            symbol: String,
        }

        let list: Vec<CgListElem> = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("cg list parse: {e}")))?;
        for m in list {
            built.entry(m.symbol.to_uppercase()).or_insert(m.id);
        }

        let mut map = self.id_map.write();
        if map.is_empty() {
            *map = built;
        }
        Ok(())
    }

    async fn fetch_markets(&self, per_page: u32, page: u32) -> Result<Vec<CgMarket>> {
        let resp = self
            .http
            .get("https://api.coingecko.com/api/v3/coins/markets")
            .query(&[
                ("vs_currency", "usd"),
                ("order", "market_cap_desc"),
                ("per_page", &per_page.to_string()),
                ("page", &page.to_string()),
                ("price_change_percentage", "1h,24h,7d,30d,1y"),
                ("x_cg_demo_api_key", &self.api_key),
            ])
            .send()
            .await?;
        if resp.status() == 429 {
            self.ban_until
                .store(chrono::Utc::now().timestamp() + 600, Ordering::Relaxed);
            return Err(BotError::RateLimited);
        }
        resp.json::<Vec<CgMarket>>()
            .await
            .map_err(|e| BotError::CmcApi(format!("cg parse: {e}")))
    }

    pub async fn get_id_for_symbol(&self, symbol: &str) -> Option<String> {
        if self.ensure_id_map().await.is_err() {
            return None;
        }
        self.id_map.read().get(&symbol.to_uppercase()).cloned()
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<RawQuote>> {
        if chrono::Utc::now().timestamp() < self.ban_until.load(Ordering::Relaxed) {
            return Err(BotError::RateLimited);
        }
        self.ensure_id_map().await?;
        let mut out = Vec::new();
        let mut missing = Vec::new();
        for &sym in symbols {
            let key = sym.to_uppercase();
            if let Some(c) = self.cache.get(&key).await {
                out.push(c);
            } else {
                missing.push(key);
            }
        }
        if missing.is_empty() {
            return Ok(out);
        }

        let mut ids_needed: Vec<String> = Vec::new();
        {
            let map = self.id_map.read();
            for key in missing {
                if let Some(id) = map.get(&key) {
                    ids_needed.push(id.clone());
                }
            }
        }
        if ids_needed.is_empty() {
            return Ok(out);
        }
        let resp = self
            .http
            .get("https://api.coingecko.com/api/v3/coins/markets")
            .query(&[
                ("vs_currency", "usd"),
                ("ids", ids_needed.join(",").as_str()),
                ("price_change_percentage", "1h,24h,7d,30d,1y"),
                ("x_cg_demo_api_key", self.api_key.as_str()),
            ])
            .send()
            .await?;
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }
        let markets: Vec<CgMarket> = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(e.to_string()))?;
        for m in markets {
            let q = market_to_raw(m);
            self.cache.insert(q.symbol.clone(), q.clone()).await;
            out.push(q);
        }
        Ok(out)
    }

    pub async fn get_top(&self, limit: u32) -> Result<Vec<RawQuote>> {
        if let Some(c) = self.cache_top.get(&limit).await {
            return Ok(c);
        }
        let markets = self.fetch_markets(limit.min(250), 1).await?;
        let quotes: Vec<RawQuote> = markets.into_iter().map(market_to_raw).collect();
        self.cache_top.insert(limit, quotes.clone()).await;
        Ok(quotes)
    }

    pub async fn get_history(&self, symbol: &str, days: &str) -> Result<Vec<(i64, f64)>> {
        if chrono::Utc::now().timestamp() < self.ban_until.load(Ordering::Relaxed) {
            return Err(BotError::RateLimited);
        }
        self.ensure_id_map().await?;
        let cg_id = {
            let map = self.id_map.read();
            map.get(&symbol.to_uppercase()).cloned()
        }
        .ok_or_else(|| BotError::SymbolNotFound(symbol.to_string()))?;

        let url = format!("https://api.coingecko.com/api/v3/coins/{cg_id}/market_chart");
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("vs_currency", "usd"),
                ("days", days),
                ("x_cg_demo_api_key", self.api_key.as_str()),
            ])
            .send()
            .await?;

        if resp.status() == 429 {
            self.ban_until
                .store(chrono::Utc::now().timestamp() + 600, Ordering::Relaxed);
            return Err(BotError::RateLimited);
        }

        #[derive(Deserialize)]
        struct CgChartResp {
            prices: Vec<Vec<f64>>,
        }

        let body: CgChartResp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(e.to_string()))?;
        let points = body
            .prices
            .into_iter()
            .filter_map(|p| {
                if p.len() == 2 {
                    Some((p[0] as i64, p[1]))
                } else {
                    None
                }
            })
            .collect();
        Ok(points)
    }

    pub async fn get_global(&self) -> Result<crate::models::GlobalMetrics> {
        if chrono::Utc::now().timestamp() < self.ban_until.load(Ordering::Relaxed) {
            return Err(BotError::RateLimited);
        }
        let resp = self
            .http
            .get("https://api.coingecko.com/api/v3/global")
            .query(&[("x_cg_demo_api_key", &self.api_key)])
            .send()
            .await?;

        if resp.status() == 429 {
            self.ban_until
                .store(chrono::Utc::now().timestamp() + 600, Ordering::Relaxed);
            return Err(BotError::RateLimited);
        }

        let body: CgGlobalResp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("cg global parse: {e}")))?;
        let data = body.data;
        let total_market_cap = data.total_market_cap.get("usd").copied().unwrap_or(0.0);
        let total_volume_24h = data.total_volume.get("usd").copied().unwrap_or(0.0);
        let btc_dominance = data
            .market_cap_percentage
            .get("btc")
            .copied()
            .unwrap_or(0.0);
        let eth_dominance = data
            .market_cap_percentage
            .get("eth")
            .copied()
            .unwrap_or(0.0);

        Ok(crate::models::GlobalMetrics {
            total_market_cap,
            total_volume_24h,
            btc_dominance,
            eth_dominance,
            active_cryptocurrencies: data.active_cryptocurrencies,
            stablecoin_mcap: None,
            dex_volume_24h: None,
        })
    }
}

#[derive(Deserialize)]
struct CgGlobalResp {
    data: CgGlobalData,
}

#[derive(Deserialize)]
struct CgGlobalData {
    active_cryptocurrencies: u32,
    total_market_cap: HashMap<String, f64>,
    total_volume: HashMap<String, f64>,
    market_cap_percentage: HashMap<String, f64>,
}

#[derive(Deserialize)]
struct CgMarket {
    #[allow(dead_code)]
    id: String,
    symbol: String,
    name: String,
    current_price: Option<f64>,
    market_cap: Option<f64>,
    total_volume: Option<f64>,
    price_change_percentage_1h_in_currency: Option<f64>,
    price_change_percentage_24h: Option<f64>,
    price_change_percentage_7d_in_currency: Option<f64>,
    price_change_percentage_30d_in_currency: Option<f64>,
    price_change_percentage_1y_in_currency: Option<f64>,
    ath: Option<f64>,
    ath_change_percentage: Option<f64>,
    high_24h: Option<f64>,
    low_24h: Option<f64>,
    market_cap_rank: Option<u32>,
    circulating_supply: Option<f64>,
}

fn market_to_raw(m: CgMarket) -> RawQuote {
    RawQuote {
        symbol: m.symbol.to_uppercase(),
        name: m.name,
        price_usd: m.current_price.unwrap_or(0.0),
        market_cap: m.market_cap,
        volume_24h: m.total_volume,
        change_1h: m.price_change_percentage_1h_in_currency,
        change_24h: m.price_change_percentage_24h,
        change_7d: m.price_change_percentage_7d_in_currency,
        change_30d: m.price_change_percentage_30d_in_currency,
        change_90d: None,
        change_365d: m.price_change_percentage_1y_in_currency,
        change_730d: None,
        ath: m.ath,
        ath_change_percentage: m.ath_change_percentage,
        high_24h: m.high_24h,
        low_24h: m.low_24h,
        rank: m.market_cap_rank,
        circulating_supply: m.circulating_supply,
        source: CoinGeckoProvider::NAME,
    }
}
