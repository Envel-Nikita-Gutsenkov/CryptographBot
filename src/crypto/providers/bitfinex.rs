use super::RawQuote;
use crate::error::{BotError, Result};
use moka::future::Cache;
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

/// Bitfinex spot tickers. Batch endpoint `tickers?symbols=tBTCUSD,tETHUSD`;
/// no key. `tickers` rows are arrays:
/// `[SYMBOL, BID, BID_SIZE, ASK, ASK_SIZE, DAILY_CHANGE, DAILY_CHANGE_REL, LAST, VOLUME, HIGH, LOW]`.
pub struct BitfinexProvider {
    http: Client,
    cache: Cache<String, RawQuote>,
    base_url: String,
}

impl BitfinexProvider {
    pub const NAME: &'static str = "bitfinex";

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
            base_url: "https://api-pub.bitfinex.com".to_string(),
        }
    }

    #[cfg(test)]
    fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self
    }

    fn pair(sym: &str) -> String {
        format!("t{}USD", sym.to_uppercase())
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

        let pairs: Vec<String> = fetch.iter().map(|s| Self::pair(s)).collect();
        let resp = self
            .http
            .get(format!(
                "{}/v2/tickers",
                self.base_url.trim_end_matches('/')
            ))
            .query(&[("symbols", pairs.join(",").as_str())])
            .send()
            .await?;

        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }
        if !resp.status().is_success() {
            return Ok(out);
        }

        let rows: Vec<Value> = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("Bitfinex parse: {e}")))?;

        for row in rows {
            let Some(arr) = row.as_array() else {
                continue;
            };
            let Some(sym_raw) = arr.first().and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(base) = sym_raw
                .strip_prefix('t')
                .and_then(|s| s.strip_suffix("USD"))
                .map(|s| s.to_uppercase())
            else {
                continue;
            };
            if !fetch.contains(&base) {
                continue;
            }
            let num = |i: usize| arr.get(i).and_then(|v| v.as_f64());
            let Some(last) = num(7) else { continue };
            if last <= 0.0 {
                continue;
            }
            let change_24h = num(6).map(|r| r * 100.0);
            let q = RawQuote {
                symbol: base.clone(),
                name: base.clone(),
                price_usd: last,
                market_cap: None,
                volume_24h: num(8).map(|v| v * last),
                change_1h: None,
                change_24h,
                change_7d: None,
                change_30d: None,
                change_90d: None,
                change_365d: None,
                change_730d: None,
                ath: None,
                ath_change_percentage: None,
                high_24h: num(9),
                low_24h: num(10),
                rank: None,
                circulating_supply: None,
                source: Self::NAME,
            };
            self.cache.insert(base, q.clone()).await;
            out.push(q);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_batch_rows() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v2/tickers"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    [
                        "tBTCUSD", 78100.0, 1.0, 78110.0, 1.0, 500.0, 0.0064, 78189.0, 1234.0,
                        79000.0, 76000.0
                    ],
                    [
                        "tETHUSD", 2250.0, 1.0, 2260.0, 1.0, -20.0, -0.0088, 2257.0, 5000.0,
                        2350.0, 2200.0
                    ]
                ])),
            )
            .mount(&server)
            .await;

        let p = BitfinexProvider::new().with_base_url(&server.uri());
        let quotes = p.get_quotes(&["BTC", "ETH"]).await.unwrap();
        assert_eq!(quotes.len(), 2);
        let btc = quotes.iter().find(|q| q.symbol == "BTC").unwrap();
        assert!((btc.price_usd - 78189.0).abs() < 1e-6);
        assert!((btc.change_24h.unwrap() - 0.64).abs() < 1e-6);
        assert_eq!(btc.source, "bitfinex");
    }
}
