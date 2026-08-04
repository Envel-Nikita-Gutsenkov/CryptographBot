use std::time::Duration;
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use super::RawQuote;
use crate::error::{BotError, Result};

pub struct CoinbaseProvider {
    http: Client,
    cache: Cache<String, RawQuote>,
}

impl CoinbaseProvider {
    pub const NAME: &'static str = "coinbase";

    pub fn new() -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("CryptographBot/1.0"),
        );

        Self {
            http: Client::builder()
                .default_headers(headers)
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                .build()
                .unwrap(),
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
        let pair = format!("{}-USD", symbol);
        let resp = self.http
            .get(format!("https://api.exchange.coinbase.com/products/{}/ticker", pair))
            .send()
            .await?;

        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        if !resp.status().is_success() {
            return Ok(None);
        }

        let ticker: CoinbaseTicker = resp.json().await.map_err(|e| BotError::CmcApi(format!("Coinbase parse: {e}")))?;
        let price_usd: f64 = ticker.price.parse().unwrap_or(0.0);
        let volume_24h: f64 = ticker.volume.parse().unwrap_or(0.0) * price_usd;

        Ok(Some(RawQuote {
            symbol: symbol.to_uppercase(),
            name: symbol.to_uppercase(),
            price_usd,
            market_cap: None,
            volume_24h: Some(volume_24h),
            change_1h: None,
            change_24h: None,
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
        Err(BotError::CmcApi("Coinbase does not support get_top".into()))
    }
}

#[derive(Deserialize)]
struct CoinbaseTicker {
    price: String,
    volume: String,
}
