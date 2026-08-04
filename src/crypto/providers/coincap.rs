use super::RawQuote;
use crate::error::{BotError, Result};
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct CoinCapProvider {
    http: Client,
    api_key: String,
    cache: Cache<String, RawQuote>,
    cache_top: Cache<u32, Vec<RawQuote>>,
    cache_ath: Cache<String, Option<f64>>,
}

impl CoinCapProvider {
    pub const NAME: &'static str = "coincap";

    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                .build()
                .unwrap(),
            api_key: api_key.into(),
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(30))
                .max_capacity(200)
                .build(),
            cache_top: Cache::builder()
                .time_to_live(Duration::from_secs(60))
                .max_capacity(10)
                .build(),
            cache_ath: Cache::builder()
                .time_to_live(Duration::from_secs(6 * 3600)) // ATH changes rarely
                .max_capacity(100)
                .build(),
        }
    }

    fn auth(&self) -> String {
        format!("Bearer {}", self.api_key)
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
        if fetch.is_empty() {
            return Ok(out);
        }

        // CoinCap search per symbol (no bulk symbol lookup, but search is fast)
        let futures: Vec<_> = fetch.iter().map(|&sym| self.fetch_one(sym)).collect();
        let results = futures::future::join_all(futures).await;
        for res in results {
            if let Ok(Some(q)) = res {
                self.cache.insert(q.symbol.clone(), q.clone()).await;
                out.push(q);
            }
        }
        Ok(out)
    }

    async fn fetch_one(&self, symbol: &str) -> Result<Option<RawQuote>> {
        let mut req = self
            .http
            .get("https://api.coincap.io/v2/assets")
            .query(&[("search", symbol), ("limit", "1")]);
        if !self.api_key.is_empty() && self.api_key != "dummy" {
            req = req.header("Authorization", self.auth());
        }
        let resp = req.send().await?;
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }
        let body: CcResp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(e.to_string()))?;
        Ok(body
            .data
            .into_iter()
            .find(|a| a.symbol.eq_ignore_ascii_case(symbol))
            .map(asset_to_raw))
    }

    pub async fn get_top(&self, limit: u32) -> Result<Vec<RawQuote>> {
        if let Some(c) = self.cache_top.get(&limit).await {
            return Ok(c);
        }
        let mut req = self
            .http
            .get("https://api.coincap.io/v2/assets")
            .query(&[("limit", limit.to_string())]);
        if !self.api_key.is_empty() && self.api_key != "dummy" {
            req = req.header("Authorization", self.auth());
        }
        let resp = req.send().await?;
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }
        let body: CcResp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(e.to_string()))?;
        let quotes: Vec<RawQuote> = body.data.into_iter().map(asset_to_raw).collect();
        self.cache_top.insert(limit, quotes.clone()).await;
        Ok(quotes)
    }

    /// All-time-high computed from CoinCap's own daily price history (free, no
    /// key, third independent vendor besides CoinGecko/CoinPaprika).
    /// Cached 6h. Returns None when the symbol is unknown or the API fails.
    pub async fn get_ath(&self, symbol: &str) -> Option<f64> {
        let key = format!("ath:{}", symbol.to_uppercase());
        if let Some(c) = self.cache_ath.get(&key).await {
            return c;
        }
        // Resolve symbol -> CoinCap asset id (exact symbol match wins).
        let mut req = self
            .http
            .get("https://api.coincap.io/v2/assets")
            .query(&[("search", symbol), ("limit", "10")]);
        if !self.api_key.is_empty() && self.api_key != "dummy" {
            req = req.header("Authorization", self.auth());
        }
        let resp = req.send().await.ok()?;
        if resp.status() == 429 {
            return None;
        }
        let body: CcResp = resp.json().await.ok()?;
        let id = body
            .data
            .into_iter()
            .find(|a| a.symbol.eq_ignore_ascii_case(symbol))
            .map(|a| a.id)?;

        // Daily history -> max price.
        let mut hreq = self
            .http
            .get(format!("https://api.coincap.io/v2/assets/{}/history", id))
            .query(&[("interval", "d1"), ("limit", "2000")]);
        if !self.api_key.is_empty() && self.api_key != "dummy" {
            hreq = hreq.header("Authorization", self.auth());
        }
        let hresp = hreq.send().await.ok()?;
        if hresp.status() == 429 {
            return None;
        }
        #[derive(Deserialize)]
        struct CcHistory {
            data: Vec<CcPoint>,
        }
        #[derive(Deserialize)]
        struct CcPoint {
            #[serde(rename = "priceUsd")]
            price: Option<serde_json::Value>,
        }
        let hist: CcHistory = hresp.json().await.ok()?;
        let ath = hist
            .data
            .iter()
            .filter_map(|p| parse_f64(&p.price))
            .fold(0.0_f64, f64::max);
        let result = if ath > 0.0 { Some(ath) } else { None };
        self.cache_ath.insert(key, result).await;
        result
    }
}

#[derive(Deserialize)]
struct CcResp {
    data: Vec<CcAsset>,
}
#[derive(Deserialize)]
#[allow(dead_code)]
struct CcAsset {
    id: String,
    rank: Option<serde_json::Value>,
    symbol: String,
    name: String,
    #[serde(rename = "priceUsd")]
    price_usd: Option<serde_json::Value>,
    #[serde(rename = "marketCapUsd")]
    market_cap_usd: Option<serde_json::Value>,
    #[serde(rename = "volumeUsd24Hr")]
    volume_24h: Option<serde_json::Value>,
    #[serde(rename = "changePercent24Hr")]
    change_24h: Option<serde_json::Value>,
    supply: Option<serde_json::Value>,
}

fn parse_f64(v: &Option<serde_json::Value>) -> Option<f64> {
    v.as_ref()
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse().ok())
}

fn asset_to_raw(a: CcAsset) -> RawQuote {
    RawQuote {
        symbol: a.symbol.to_uppercase(),
        name: a.name,
        price_usd: parse_f64(&a.price_usd).unwrap_or(0.0),
        market_cap: parse_f64(&a.market_cap_usd),
        volume_24h: parse_f64(&a.volume_24h),
        change_1h: None,
        change_24h: parse_f64(&a.change_24h),
        change_7d: None,
        change_30d: None,
        change_90d: None,
        change_365d: None,
        change_730d: None,
        ath: None,
        ath_change_percentage: None,
        high_24h: None,
        low_24h: None,
        rank: parse_f64(&a.rank).map(|r| r as u32),
        circulating_supply: parse_f64(&a.supply),
        source: CoinCapProvider::NAME,
    }
}
