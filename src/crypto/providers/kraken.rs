use super::RawQuote;
use crate::error::{BotError, Result};
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

/// Kraken spot tickers. Batch endpoint (`pair=XBTUSDT,ETHUSDT`) so one request
/// covers the whole watchlist; no key, 1 req/s public allowance is plenty.
pub struct KrakenProvider {
    http: Client,
    cache: Cache<String, RawQuote>,
    base_url: String,
}

impl KrakenProvider {
    pub const NAME: &'static str = "kraken";

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
            base_url: "https://api.kraken.com".to_string(),
        }
    }

    #[cfg(test)]
    fn with_base_url(mut self, url: &str) -> Self {
        self.base_url = url.to_string();
        self
    }

    /// Kraken quotes bitcoin as XBT.
    fn pair_symbol(sym: &str) -> String {
        if sym == "BTC" {
            "XBT".to_string()
        } else {
            sym.to_string()
        }
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

        let pairs: Vec<String> = fetch
            .iter()
            .map(|s| format!("{}USDT", Self::pair_symbol(s)))
            .collect();
        let resp = self
            .http
            .get(format!(
                "{}/0/public/Ticker",
                self.base_url.trim_end_matches('/')
            ))
            .query(&[("pair", pairs.join(","))])
            .send()
            .await?;

        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }
        if !resp.status().is_success() {
            return Ok(out);
        }

        let body: KrakenResp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("Kraken parse: {e}")))?;
        if !body.error.is_empty() {
            // Unknown pair for one symbol makes Kraken return an error for the
            // batch; treat as "nothing matched" rather than a hard failure.
            return Ok(out);
        }

        for sym in fetch {
            let base = Self::pair_symbol(&sym);
            let Some(t) = body
                .result
                .iter()
                .find_map(|(k, v)| (k.contains(&base) && k.contains("USDT")).then_some(v))
            else {
                continue;
            };
            let Some(last) = t.c.first().and_then(|v| v.parse::<f64>().ok()) else {
                continue;
            };
            if last <= 0.0 {
                continue;
            }
            let open = t.o.parse::<f64>().ok().filter(|v| *v > 0.0);
            let change_24h = open.map(|o| (last / o - 1.0) * 100.0);
            let volume =
                t.v.get(1)
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|base_vol| base_vol * last);
            let q = RawQuote {
                symbol: sym.clone(),
                name: sym.clone(),
                price_usd: last,
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
                high_24h: t.h.first().and_then(|v| v.parse::<f64>().ok()),
                low_24h: t.l.first().and_then(|v| v.parse::<f64>().ok()),
                rank: None,
                circulating_supply: None,
                source: Self::NAME,
            };
            self.cache.insert(sym, q.clone()).await;
            out.push(q);
        }
        Ok(out)
    }
}

#[derive(Deserialize)]
struct KrakenResp {
    error: Vec<String>,
    result: HashMap<String, KrakenTicker>,
}

#[derive(Deserialize)]
struct KrakenTicker {
    /// [last trade price, last trade volume]
    #[serde(rename = "c")]
    c: Vec<String>,
    /// Today's opening price.
    #[serde(rename = "o")]
    o: String,
    #[serde(rename = "h")]
    h: Vec<String>,
    #[serde(rename = "l")]
    l: Vec<String>,
    #[serde(rename = "v")]
    v: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_batch_and_maps_xbt() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/0/public/Ticker"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "error": [],
                "result": {
                    "XBTUSDT": {"c": ["78090.8", "0.1"], "o": "77000.0", "h": ["79000.0", "1"], "l": ["76000.0", "1"], "v": ["100.0", "7800000.0"]},
                    "ETHUSDT": {"c": ["2257.0", "1.0"], "o": "2300.0", "h": ["2350.0", "1"], "l": ["2200.0", "1"], "v": ["1000.0", "2257000.0"]}
                }
            })))
            .mount(&server)
            .await;

        let p = KrakenProvider::new().with_base_url(&server.uri());
        let quotes = p.get_quotes(&["BTC", "ETH"]).await.unwrap();
        assert_eq!(quotes.len(), 2);
        let btc = quotes.iter().find(|q| q.symbol == "BTC").unwrap();
        assert!((btc.price_usd - 78090.8).abs() < 1e-6);
        assert!(btc.change_24h.unwrap() > 0.0);
        assert_eq!(btc.source, "kraken");
    }
}
