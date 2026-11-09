use super::RawQuote;
use crate::error::{BotError, Result};
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

/// OKX spot tickers. No API key, generous public rate limits, one request per
/// symbol (`instId=BTC-USDT`) — verified live at ~300ms.
pub struct OkxProvider {
    http: Client,
    cache: Cache<String, RawQuote>,
    base_url: String,
}

impl OkxProvider {
    pub const NAME: &'static str = "okx";

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
            base_url: "https://www.okx.com".to_string(),
        }
    }

    #[cfg(test)]
    fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<RawQuote>> {
        let mut out = Vec::new();
        let mut fetch = Vec::new();
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
        let inst = format!("{}-USDT", symbol);
        let resp = self
            .http
            .get(format!(
                "{}/api/v5/market/ticker",
                self.base_url.trim_end_matches('/')
            ))
            .query(&[("instId", inst.as_str())])
            .send()
            .await?;

        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }
        if !resp.status().is_success() {
            return Ok(None);
        }

        let body: OkxResp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("OKX parse: {e}")))?;
        if body.code != "0" {
            return Ok(None);
        }
        let Some(t) = body.data.first() else {
            return Ok(None);
        };
        let price: f64 = t.last.parse().unwrap_or(0.0);
        if price <= 0.0 {
            return Ok(None);
        }
        let open: f64 = t.open_24h.parse().unwrap_or(0.0);
        let change_24h = if open > 0.0 {
            Some((price / open - 1.0) * 100.0)
        } else {
            None
        };
        // `volCcy24h` is the quote-currency (USDT) turnover.
        let volume = t.vol_ccy_24h.parse::<f64>().ok();
        let high = t.high_24h.parse::<f64>().ok();
        let low = t.low_24h.parse::<f64>().ok();

        Ok(Some(RawQuote {
            symbol: symbol.to_uppercase(),
            name: symbol.to_uppercase(),
            price_usd: price,
            market_cap: None,
            volume_24h: volume,
            change_1h: None,
            change_24h,
            change_7d: None,
            change_30d: None,
            change_90d: None,
            change_365d: None,
            change_730d: None,
            ath: None,
            ath_change_percentage: None,
            high_24h: high,
            low_24h: low,
            rank: None,
            circulating_supply: None,
            source: Self::NAME,
        }))
    }
}

#[derive(Deserialize)]
struct OkxResp {
    code: String,
    data: Vec<OkxTicker>,
}

#[derive(Deserialize)]
struct OkxTicker {
    last: String,
    #[serde(rename = "open24h")]
    open_24h: String,
    #[serde(rename = "high24h")]
    high_24h: String,
    #[serde(rename = "low24h")]
    low_24h: String,
    #[serde(rename = "volCcy24h")]
    vol_ccy_24h: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_ticker() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v5/market/ticker"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "code": "0",
                    "data": [{
                        "instId": "BTC-USDT",
                        "last": "78092.3",
                        "open24h": "77000",
                        "high24h": "79000",
                        "low24h": "76000",
                        "volCcy24h": "1234567"
                    }]
                })),
            )
            .mount(&server)
            .await;

        let p = OkxProvider::new().with_base_url(&server.uri());
        let quotes = p.get_quotes(&["BTC"]).await.unwrap();
        assert_eq!(quotes.len(), 1);
        assert_eq!(quotes[0].source, "okx");
        assert!((quotes[0].price_usd - 78092.3).abs() < 1e-6);
        assert!(quotes[0].change_24h.unwrap() > 0.0);
    }
}
