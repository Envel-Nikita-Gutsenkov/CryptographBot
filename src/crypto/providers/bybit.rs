use std::time::Duration;
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use super::RawQuote;
use crate::error::{BotError, Result};

pub struct BybitProvider {
    http: Client,
    cache: Cache<String, RawQuote>,
}

impl BybitProvider {
    pub const NAME: &'static str = "bybit";

    pub fn new() -> Self {
        Self {
            http: Client::builder().timeout(Duration::from_secs(8)).use_rustls_tls().build().unwrap(),
            cache: Cache::builder().time_to_live(Duration::from_secs(30)).max_capacity(200).build(),
        }
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<RawQuote>> {
        let mut out = Vec::new();
        let mut fetch = Vec::new();
        for &s in symbols {
            let sym_upper = s.to_uppercase();
            match self.cache.get(&sym_upper).await {
                Some(q) => out.push(q),
                None => fetch.push(sym_upper),
            }
        }
        if fetch.is_empty() {
            return Ok(out);
        }

        // Query each missing pair in parallel
        let futures: Vec<_> = fetch.iter().map(|sym| self.fetch_one(sym)).collect();
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
        let pair = format!("{}USDT", symbol);
        let resp = self.http
            .get("https://api.bybit.com/v5/market/tickers")
            .query(&[("category", "spot"), ("symbol", &pair)])
            .send()
            .await?;

        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        if !resp.status().is_success() {
            return Ok(None);
        }

        let body: BybitResp = resp.json().await.map_err(|e| BotError::CmcApi(format!("Bybit parse: {e}")))?;
        if body.ret_code != 0 || body.result.list.is_empty() {
            return Ok(None);
        }

        let t = &body.result.list[0];
        let price_usd: f64 = t.last_price.parse().unwrap_or(0.0);
        let volume_24h: f64 = t.volume24h.parse().unwrap_or(0.0) * price_usd; // volume24h in base asset
        let change_24h: f64 = t.price_24h_pcnt.parse::<f64>().unwrap_or(0.0) * 100.0;

        Ok(Some(RawQuote {
            symbol: symbol.to_uppercase(),
            name: symbol.to_uppercase(),
            price_usd,
            market_cap: None,
            volume_24h: Some(volume_24h),
            change_1h: None,
            change_24h: Some(change_24h),
            change_7d: None,
            change_30d: None,
            change_90d: None,
            change_365d: None,
            change_730d: None,
            ath: None,
            ath_change_percentage: None,
            high_24h: None,
            low_24h: None,
            rank: None,
            circulating_supply: None,
            source: Self::NAME,
        }))
    }

    #[allow(dead_code)]
    pub async fn get_top(&self, _limit: u32) -> Result<Vec<RawQuote>> {
        Err(BotError::CmcApi("Bybit does not support get_top".into()))
    }
}

#[derive(Deserialize)]
struct BybitResp {
    #[serde(rename = "retCode")]
    ret_code: i32,
    result: BybitResult,
}

#[derive(Deserialize)]
struct BybitResult {
    list: Vec<BybitTicker>,
}

#[derive(Deserialize)]
struct BybitTicker {
    #[serde(rename = "lastPrice")]
    last_price: String,
    volume24h: String,
    #[serde(rename = "price24hPcnt")]
    price_24h_pcnt: String,
}
