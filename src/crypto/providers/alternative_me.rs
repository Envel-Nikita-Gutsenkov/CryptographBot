use super::RawQuote;
use crate::error::{BotError, Result};
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct AlternativeMeProvider {
    http: Client,
    cache: Cache<(), Vec<RawQuote>>,
}

impl AlternativeMeProvider {
    pub const NAME: &'static str = "alternative_me";

    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                .build()
                .unwrap(),
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(60)) // They update every 5 mins, 60s is safe
                .max_capacity(1)
                .build(),
        }
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<RawQuote>> {
        let all_quotes = self.get_all_tickers().await?;
        let mut out = Vec::new();

        for &sym in symbols {
            let sym_upper = sym.to_uppercase();
            if let Some(q) = all_quotes.iter().find(|q| q.symbol == sym_upper) {
                out.push(q.clone());
            }
        }

        Ok(out)
    }

    async fn get_all_tickers(&self) -> Result<Vec<RawQuote>> {
        if let Some(cached) = self.cache.get(&()).await {
            return Ok(cached);
        }

        let resp = self
            .http
            .get("https://api.alternative.me/v2/ticker/?limit=0")
            .header("User-Agent", "CryptographBot/1.0")
            .send()
            .await?;
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        let body: AltResp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(e.to_string()))?;

        let mut quotes = Vec::with_capacity(body.data.len());
        for (_, t) in body.data {
            let price = t.quotes.usd.price.unwrap_or(0.0);
            if price == 0.0 {
                continue;
            } // filter out dead data

            quotes.push(RawQuote {
                symbol: t.symbol.to_uppercase(),
                name: t.name,
                price_usd: price,
                market_cap: t.quotes.usd.market_cap,
                volume_24h: t.quotes.usd.volume_24h,
                change_1h: t.quotes.usd.percentage_change_1h,
                change_24h: t.quotes.usd.percentage_change_24h,
                change_7d: t.quotes.usd.percentage_change_7d,
                change_30d: None,
                change_90d: None,
                change_365d: None,
                change_730d: None,
                ath: None,
                ath_change_percentage: None,
                high_24h: None,
                low_24h: None,
                rank: t.rank,
                circulating_supply: t.circulating_supply,
                source: Self::NAME,
            });
        }

        self.cache.insert((), quotes.clone()).await;
        Ok(quotes)
    }

    /// Fetches the latest Fear and Greed Index
    pub async fn get_fng(&self) -> Result<FearAndGreed> {
        let resp = self
            .http
            .get("https://api.alternative.me/fng/?limit=1")
            .header("User-Agent", "CryptographBot/1.0")
            .send()
            .await?;
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        let body: FngResp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(e.to_string()))?;
        body.data
            .into_iter()
            .next()
            .ok_or_else(|| BotError::CmcApi("Empty FNG data".to_string()))
    }
}

#[derive(Deserialize)]
struct AltResp {
    data: std::collections::HashMap<String, AltTicker>,
}

#[derive(Deserialize)]
struct AltTicker {
    symbol: String,
    name: String,
    rank: Option<u32>,
    circulating_supply: Option<f64>,
    quotes: AltQuotes,
}

#[derive(Deserialize)]
struct AltQuotes {
    #[serde(rename = "USD")]
    usd: AltUsdQuote,
}

#[derive(Deserialize)]
struct AltUsdQuote {
    price: Option<f64>,
    volume_24h: Option<f64>,
    market_cap: Option<f64>,
    percentage_change_1h: Option<f64>,
    percentage_change_24h: Option<f64>,
    percentage_change_7d: Option<f64>,
}

#[derive(Deserialize)]
struct FngResp {
    data: Vec<FearAndGreed>,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct FearAndGreed {
    pub value: String,
    pub value_classification: String,
    pub timestamp: String,
    pub time_until_update: Option<String>,
}
