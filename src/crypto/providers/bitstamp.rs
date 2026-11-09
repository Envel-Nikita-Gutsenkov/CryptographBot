use super::RawQuote;
use crate::error::{BotError, Result};
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

/// Bitstamp spot tickers. One request per symbol (`/api/v2/ticker/btcusdt/`),
/// no key, very fast (~35ms from the prod host).
pub struct BitstampProvider {
    http: Client,
    cache: Cache<String, RawQuote>,
    base_url: String,
}

impl BitstampProvider {
    pub const NAME: &'static str = "bitstamp";

    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                .build()
                .unwrap(),
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(30))
                .max_capacity(200)
                .build(),
            base_url: "https://www.bitstamp.net".to_string(),
        }
    }

    #[cfg(test)]
    fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<RawQuote>> {
        let mut out = Vec::new();
        let mut fetch: Vec<String> = Vec::new();
        for &s in symbols {
            let sym = s.to_uppercase();
            match self.cache.get(&sym).await {
                Some(q) => out.push(q),
                None if !fetch.contains(&sym) => fetch.push(sym),
                None => {}
            }
        }
        if fetch.is_empty() {
            return Ok(out);
        }

        let futures: Vec<_> = fetch.iter().map(|s| self.fetch_one(s)).collect();
        for res in futures::future::join_all(futures).await {
            if let Ok(Some(q)) = res {
                self.cache.insert(q.symbol.clone(), q.clone()).await;
                out.push(q);
            }
        }
        Ok(out)
    }

    async fn fetch_one(&self, symbol: &str) -> Result<Option<RawQuote>> {
        let pair = format!("{}usdt", symbol.to_lowercase());
        let resp = self
            .http
            .get(format!(
                "{}/api/v2/ticker/{}/",
                self.base_url.trim_end_matches('/'),
                pair
            ))
            .send()
            .await?;

        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }
        if !resp.status().is_success() {
            return Ok(None);
        }

        let t: BitstampTicker = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("Bitstamp parse: {e}")))?;
        let price: f64 = t.last.parse().unwrap_or(0.0);
        if price <= 0.0 {
            return Ok(None);
        }
        let open: f64 = t.open.parse().unwrap_or(0.0);
        let change_24h = if open > 0.0 {
            Some((price / open - 1.0) * 100.0)
        } else {
            None
        };

        Ok(Some(RawQuote {
            symbol: symbol.to_uppercase(),
            name: symbol.to_uppercase(),
            price_usd: price,
            market_cap: None,
            volume_24h: t.volume.parse::<f64>().ok().map(|v| v * price),
            change_1h: None,
            change_24h,
            change_7d: None,
            change_30d: None,
            change_90d: None,
            change_365d: None,
            change_730d: None,
            ath: None,
            ath_change_percentage: None,
            high_24h: t.high.parse::<f64>().ok(),
            low_24h: t.low.parse::<f64>().ok(),
            rank: None,
            circulating_supply: None,
            source: Self::NAME,
        }))
    }
}

#[derive(Deserialize)]
struct BitstampTicker {
    last: String,
    open: String,
    high: String,
    low: String,
    volume: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_ticker() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/ticker/btcusdt/"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "last": "78107.64",
                    "open": "77000.0",
                    "high": "79000.0",
                    "low": "76000.0",
                    "volume": "100.5"
                })),
            )
            .mount(&server)
            .await;

        let p = BitstampProvider::new().with_base_url(&server.uri());
        let quotes = p.get_quotes(&["BTC"]).await.unwrap();
        assert_eq!(quotes.len(), 1);
        assert!((quotes[0].price_usd - 78107.64).abs() < 1e-6);
        assert!(quotes[0].change_24h.unwrap() > 0.0);
        assert_eq!(quotes[0].source, "bitstamp");
    }
}
