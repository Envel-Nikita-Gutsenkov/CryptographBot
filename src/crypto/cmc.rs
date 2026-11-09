use std::{sync::Arc, time::Duration};

use governor::{Quota, RateLimiter, state::NotKeyed, clock::DefaultClock, middleware::NoOpMiddleware};
use moka::future::Cache;
use nonzero_ext::nonzero;
use reqwest::Client;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{
    error::{BotError, Result},
    models::{CmcQuote, GlobalMetrics},
};

const BASE_URL: &str = "https://pro-api.coinmarketcap.com";
const CACHE_TTL_PRICE: Duration = Duration::from_secs(60);
const CACHE_TTL_TOP: Duration = Duration::from_secs(300);
const CACHE_TTL_GLOBAL: Duration = Duration::from_secs(300);
const CACHE_TTL_BYN: Duration = Duration::from_secs(86_400);

type Limiter = RateLimiter<NotKeyed, governor::state::InMemoryState, DefaultClock, NoOpMiddleware>;

pub struct CmcClient {
    http: Client,
    api_key: String,
    rate_limiter: Arc<Limiter>,
    cache_price: Cache<String, CmcQuote>,
    cache_top: Cache<u32, Vec<CmcQuote>>,
    cache_global: Cache<(), GlobalMetrics>,
    cache_byn: Cache<(), f64>,
}

impl CmcClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .use_rustls_tls()
            .build()
            .expect("HTTP client init");

        // 30 calls/minute sustained (well under 333/day hard limit)
        let rate_limiter = Arc::new(RateLimiter::direct(
            Quota::per_minute(nonzero!(30u32)),
        ));

        let cache_price = Cache::builder()
            .time_to_live(CACHE_TTL_PRICE)
            .max_capacity(200)
            .build();

        let cache_top = Cache::builder()
            .time_to_live(CACHE_TTL_TOP)
            .max_capacity(10)
            .build();

        let cache_global = Cache::builder()
            .time_to_live(CACHE_TTL_GLOBAL)
            .max_capacity(1)
            .build();

        let cache_byn = Cache::builder()
            .time_to_live(CACHE_TTL_BYN)
            .max_capacity(1)
            .build();

        Self {
            http,
            api_key: api_key.into(),
            rate_limiter,
            cache_price,
            cache_top,
            cache_global,
            cache_byn,
        }
    }

    async fn throttle(&self) -> Result<()> {
        self.rate_limiter
            .until_ready()
            .await;
        Ok(())
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<CmcQuote>> {
        let mut results = Vec::new();
        let mut to_fetch: Vec<&str> = Vec::new();

        for &sym in symbols {
            if let Some(cached) = self.cache_price.get(&sym.to_uppercase()).await {
                results.push(cached);
            } else {
                to_fetch.push(sym);
            }
        }

        if !to_fetch.is_empty() {
            self.throttle().await?;
            let sym_str = to_fetch.join(",");
            debug!("CMC fetch quotes: {sym_str}");

            let resp = self
                .http
                .get(format!("{BASE_URL}/v1/cryptocurrency/quotes/latest"))
                .header("X-CMC_PRO_API_KEY", &self.api_key)
                .query(&[("symbol", &sym_str), ("convert", &"USD".to_string())])
                .send()
                .await?;

            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                return Err(BotError::RateLimited);
            }

            let body: CmcQuoteResponse = resp.json().await.map_err(|e| {
                BotError::CmcApi(format!("parse error: {e}"))
            })?;

            for (sym, entry) in body.data {
                let quote = parse_quote(entry)?;
                self.cache_price.insert(sym.to_uppercase(), quote.clone()).await;
                results.push(quote);
            }
        }

        Ok(results)
    }

    pub async fn get_top(&self, limit: u32) -> Result<Vec<CmcQuote>> {
        if let Some(cached) = self.cache_top.get(&limit).await {
            return Ok(cached);
        }

        self.throttle().await?;
        debug!("CMC fetch top {limit}");

        let resp = self
            .http
            .get(format!("{BASE_URL}/v1/cryptocurrency/listings/latest"))
            .header("X-CMC_PRO_API_KEY", &self.api_key)
            .query(&[
                ("limit", limit.to_string()),
                ("sort", "volume_24h".to_string()),
                ("convert", "USD".to_string()),
            ])
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(BotError::RateLimited);
        }

        let body: CmcListingResponse = resp.json().await.map_err(|e| {
            BotError::CmcApi(format!("parse error: {e}"))
        })?;

        let quotes: Result<Vec<CmcQuote>> = body.data.into_iter().map(parse_listing).collect();
        let quotes = quotes?;
        self.cache_top.insert(limit, quotes.clone()).await;
        Ok(quotes)
    }

    pub async fn get_global_metrics(&self) -> Result<GlobalMetrics> {
        if let Some(cached) = self.cache_global.get(&()).await {
            return Ok(cached);
        }

        self.throttle().await?;

        let resp = self
            .http
            .get(format!("{BASE_URL}/v1/global-metrics/quotes/latest"))
            .header("X-CMC_PRO_API_KEY", &self.api_key)
            .query(&[("convert", "USD")])
            .send()
            .await?;

        let body: serde_json::Value = resp.json().await.map_err(|e| {
            BotError::CmcApi(e.to_string())
        })?;

        let d = &body["data"];
        let metrics = GlobalMetrics {
            total_market_cap: d["quote"]["USD"]["total_market_cap"].as_f64().unwrap_or(0.0),
            total_volume_24h: d["quote"]["USD"]["total_volume_24h"].as_f64().unwrap_or(0.0),
            btc_dominance: d["btc_dominance"].as_f64().unwrap_or(0.0),
            eth_dominance: d["eth_dominance"].as_f64().unwrap_or(0.0),
            active_cryptocurrencies: d["active_cryptocurrencies"].as_u64().unwrap_or(0) as u32,
        };

        self.cache_global.insert((), metrics.clone()).await;
        Ok(metrics)
    }

    /// Get live BYN/USD rate from NBRB (Belarus National Bank).
    pub async fn get_byn_rate(&self) -> Result<f64> {
        if let Some(cached) = self.cache_byn.get(&()).await {
            return Ok(cached);
        }

        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let url = format!("https://api.nbrb.by/exrates/rates/431?ondate={today}");

        let resp = self.http.get(&url).send().await?;
        let body: serde_json::Value = resp.json().await.map_err(|e| {
            BotError::CmcApi(format!("NBRB parse: {e}"))
        })?;

        let rate = body["Cur_OfficialRate"].as_f64().unwrap_or(0.0);
        if rate == 0.0 {
            warn!("NBRB returned zero rate, fallback to 3.25");
            return Ok(3.25);
        }

        self.cache_byn.insert((), rate).await;
        Ok(rate)
    }

    /// Convert amount of sym to USD using live rate.
    pub async fn to_usd(&self, amount: f64, symbol: &str) -> Result<f64> {
        if symbol == "USD" {
            return Ok(amount);
        }
        if symbol == "BYN" {
            let rate = self.get_byn_rate().await?;
            return Ok(amount / rate);
        }
        let quotes = self.get_quotes(&[symbol]).await?;
        quotes
            .first()
            .map(|q| q.price_usd * amount)
            .ok_or_else(|| BotError::SymbolNotFound(symbol.to_string()))
    }
}

// ---- Raw deserialization types ----

#[derive(Deserialize)]
struct CmcQuoteResponse {
    data: std::collections::HashMap<String, CmcQuoteEntry>,
}

#[derive(Deserialize)]
struct CmcQuoteEntry {
    name: String,
    symbol: String,
    cmc_rank: Option<u32>,
    circulating_supply: Option<f64>,
    quote: CmcQuoteData,
}

#[derive(Deserialize)]
struct CmcQuoteData {
    #[serde(rename = "USD")]
    usd: UsdQuote,
}

#[derive(Deserialize)]
struct UsdQuote {
    price: f64,
    market_cap: Option<f64>,
    volume_24h: Option<f64>,
    percent_change_1h: Option<f64>,
    percent_change_24h: Option<f64>,
    percent_change_7d: Option<f64>,
    percent_change_30d: Option<f64>,
    percent_change_90d: Option<f64>,
}

fn parse_quote(e: CmcQuoteEntry) -> Result<CmcQuote> {
    let q = &e.quote.usd;
    Ok(CmcQuote {
        symbol: e.symbol,
        name: e.name,
        price_usd: q.price,
        market_cap: q.market_cap.unwrap_or(0.0),
        volume_24h: q.volume_24h.unwrap_or(0.0),
        change_1h: q.percent_change_1h,
        change_24h: q.percent_change_24h,
        change_7d: q.percent_change_7d,
        change_30d: q.percent_change_30d,
        change_90d: q.percent_change_90d,
        rank: e.cmc_rank.unwrap_or(0),
        circulating_supply: e.circulating_supply.unwrap_or(0.0),
    })
}

#[derive(Deserialize)]
struct CmcListingResponse {
    data: Vec<CmcListingEntry>,
}

#[derive(Deserialize)]
struct CmcListingEntry {
    name: String,
    symbol: String,
    cmc_rank: Option<u32>,
    circulating_supply: Option<f64>,
    quote: CmcQuoteData,
}

fn parse_listing(e: CmcListingEntry) -> Result<CmcQuote> {
    let q = &e.quote.usd;
    Ok(CmcQuote {
        symbol: e.symbol,
        name: e.name,
        price_usd: q.price,
        market_cap: q.market_cap.unwrap_or(0.0),
        volume_24h: q.volume_24h.unwrap_or(0.0),
        change_1h: q.percent_change_1h,
        change_24h: q.percent_change_24h,
        change_7d: q.percent_change_7d,
        change_30d: q.percent_change_30d,
        change_90d: q.percent_change_90d,
        rank: e.cmc_rank.unwrap_or(0),
        circulating_supply: e.circulating_supply.unwrap_or(0.0),
    })
}
