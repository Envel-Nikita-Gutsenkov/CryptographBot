use std::time::Duration;
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use governor::{Quota, RateLimiter, state::NotKeyed, clock::DefaultClock, middleware::NoOpMiddleware};
use nonzero_ext::nonzero;
use super::RawQuote;
use crate::error::{BotError, Result};

type Limiter = RateLimiter<NotKeyed, governor::state::InMemoryState, DefaultClock, NoOpMiddleware>;

pub struct CmcProvider {
    http: Client,
    api_key: String,
    limiter: Limiter,
    cache: Cache<String, RawQuote>,
    cache_top: Cache<u32, Vec<RawQuote>>,
}

impl CmcProvider {
    pub const NAME: &'static str = "coinmarketcap";

    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: Client::builder().timeout(Duration::from_secs(8)).use_rustls_tls().build().unwrap(),
            api_key: api_key.into(),
            limiter: RateLimiter::direct(Quota::per_minute(nonzero!(20u32))),
            cache: Cache::builder().time_to_live(Duration::from_secs(300)).max_capacity(200).build(),
            cache_top: Cache::builder().time_to_live(Duration::from_secs(300)).max_capacity(10).build(),
        }
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<RawQuote>> {
        let mut out = Vec::new();
        let mut fetch: Vec<&str> = Vec::new();
        for &s in symbols {
            match self.cache.get(&s.to_uppercase()).await {
                Some(q) => out.push(q),
                None => fetch.push(s),
            }
        }
        if fetch.is_empty() { return Ok(out); }
        self.limiter.until_ready().await;
        let resp = self.http
            .get("https://pro-api.coinmarketcap.com/v1/cryptocurrency/quotes/latest")
            .header("X-CMC_PRO_API_KEY", &self.api_key)
            .query(&[("symbol", fetch.join(",")), ("convert", "USD".into())])
            .send().await?;
        if resp.status() == 429 { return Err(BotError::RateLimited); }
        let body: CmcResp = resp.json().await.map_err(|e| BotError::CmcApi(e.to_string()))?;
        for (sym, e) in body.data {
            let q = to_raw(e);
            self.cache.insert(sym.to_uppercase(), q.clone()).await;
            out.push(q);
        }
        Ok(out)
    }

    pub async fn get_top(&self, limit: u32) -> Result<Vec<RawQuote>> {
        if let Some(c) = self.cache_top.get(&limit).await { return Ok(c); }
        self.limiter.until_ready().await;
        let resp = self.http
            .get("https://pro-api.coinmarketcap.com/v1/cryptocurrency/listings/latest")
            .header("X-CMC_PRO_API_KEY", &self.api_key)
            .query(&[("limit", limit.to_string()), ("sort", "volume_24h".into()), ("convert", "USD".into())])
            .send().await?;
        if resp.status() == 429 { return Err(BotError::RateLimited); }
        let body: CmcListResp = resp.json().await.map_err(|e| BotError::CmcApi(e.to_string()))?;
        let quotes: Vec<RawQuote> = body.data.into_iter().map(to_listing).collect();
        self.cache_top.insert(limit, quotes.clone()).await;
        Ok(quotes)
    }

    pub async fn get_global(&self) -> Result<crate::models::GlobalMetrics> {
        self.limiter.until_ready().await;
        let resp = self.http
            .get("https://pro-api.coinmarketcap.com/v1/global-metrics/quotes/latest")
            .header("X-CMC_PRO_API_KEY", &self.api_key)
            .query(&[("convert", "USD")])
            .send().await?;
            
        if !resp.status().is_success() {
            return Err(BotError::CmcApi(format!("CMC global API error: {}", resp.status())));
        }
        
        let v: serde_json::Value = resp.json().await.map_err(|e| BotError::CmcApi(e.to_string()))?;
        
        if let Some(status) = v.get("status") {
            if let Some(err_code) = status.get("error_code").and_then(|c| c.as_u64()) {
                if err_code != 0 {
                    return Err(BotError::CmcApi(format!("CMC global error code: {}", err_code)));
                }
            }
        }
        
        let d = &v["data"];
        if d.is_null() {
            return Err(BotError::CmcApi("CMC global data is null".into()));
        }
        
        Ok(crate::models::GlobalMetrics {
            total_market_cap: d["quote"]["USD"]["total_market_cap"].as_f64().unwrap_or(0.0),
            total_volume_24h: d["quote"]["USD"]["total_volume_24h"].as_f64().unwrap_or(0.0),
            btc_dominance: d["btc_dominance"].as_f64().unwrap_or(0.0),
            eth_dominance: d["eth_dominance"].as_f64().unwrap_or(0.0),
            active_cryptocurrencies: d["active_cryptocurrencies"].as_u64().unwrap_or(0) as u32,
            stablecoin_mcap: None,
            dex_volume_24h: None,
        })
    }
}

#[derive(Deserialize)] struct CmcResp { data: std::collections::HashMap<String, CmcEntry> }
#[derive(Deserialize)] struct CmcListResp { data: Vec<CmcListEntry> }
#[derive(Deserialize)] struct CmcEntry { name: String, symbol: String, cmc_rank: Option<u32>, circulating_supply: Option<f64>, quote: CmcQData }
#[derive(Deserialize)] struct CmcListEntry { name: String, symbol: String, cmc_rank: Option<u32>, circulating_supply: Option<f64>, quote: CmcQData }
#[derive(Deserialize)] struct CmcQData { #[serde(rename = "USD")] usd: CmcUsd }
#[derive(Deserialize)] struct CmcUsd { price: f64, market_cap: Option<f64>, volume_24h: Option<f64>, percent_change_1h: Option<f64>, percent_change_24h: Option<f64>, percent_change_7d: Option<f64>, percent_change_30d: Option<f64>, percent_change_90d: Option<f64>, percent_change_365d: Option<f64> }

fn to_raw(e: CmcEntry) -> RawQuote {
    RawQuote { symbol: e.symbol, name: e.name, price_usd: e.quote.usd.price, market_cap: e.quote.usd.market_cap, volume_24h: e.quote.usd.volume_24h, change_1h: e.quote.usd.percent_change_1h, change_24h: e.quote.usd.percent_change_24h, change_7d: e.quote.usd.percent_change_7d, change_30d: e.quote.usd.percent_change_30d, change_90d: e.quote.usd.percent_change_90d, change_365d: e.quote.usd.percent_change_365d, change_730d: None, ath: None, ath_change_percentage: None, high_24h: None, low_24h: None, rank: e.cmc_rank, circulating_supply: e.circulating_supply, source: CmcProvider::NAME }
}

fn to_listing(e: CmcListEntry) -> RawQuote {
    RawQuote { symbol: e.symbol, name: e.name, price_usd: e.quote.usd.price, market_cap: e.quote.usd.market_cap, volume_24h: e.quote.usd.volume_24h, change_1h: e.quote.usd.percent_change_1h, change_24h: e.quote.usd.percent_change_24h, change_7d: e.quote.usd.percent_change_7d, change_30d: e.quote.usd.percent_change_30d, change_90d: e.quote.usd.percent_change_90d, change_365d: e.quote.usd.percent_change_365d, change_730d: None, ath: None, ath_change_percentage: None, high_24h: None, low_24h: None, rank: e.cmc_rank, circulating_supply: e.circulating_supply, source: CmcProvider::NAME }
}
