use super::RawQuote;
use crate::error::{BotError, Result};
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct BinanceProvider {
    http: Client,
    cache: Cache<String, RawQuote>,
    // Prod default, injected in tests via with_base_url
    base_url: String,
}

impl BinanceProvider {
    pub const NAME: &'static str = "binance";

    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                // Pin the socket family to IPv4: api.binance.com is served via
                // CloudFront (AAAA present, and on hosts with a broken v6 route
                // ~20% of attempts stalled past the timeout while IPv4 was
                // always instant). CoinGecko/BYbit etc. were verified working
                // on both stacks, so this stays Binance-specific.
                .local_address(Some(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)))
                .build()
                .unwrap(),
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(30))
                .max_capacity(200)
                .build(),
            base_url: "https://api.binance.com".to_string(),
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
            let sym_upper = s.to_uppercase();
            match self.cache.get(&sym_upper).await {
                Some(q) => out.push(q),
                // Dedupe: callers may repeat symbols (watchlist + holdings),
                // and the batch 24hr endpoint rejects any duplicate with
                // code -1151, failing the whole batch.
                None if !fetch.contains(&sym_upper) => fetch.push(sym_upper),
                None => {}
            }
        }
        if fetch.is_empty() {
            return Ok(out);
        }

        // Map symbols to Binance USDT pairs, e.g. BTC -> BTCUSDT
        let pairs: Vec<String> = fetch.iter().map(|s| format!("{}USDT", s)).collect();

        // Serialize to JSON array for request
        let symbols_json = match serde_json::to_string(&pairs) {
            Ok(j) => j,
            Err(e) => {
                return Err(BotError::CmcApi(format!(
                    "Binance json serialize error: {e}"
                )))
            }
        };

        let resp = self
            .http
            .get(format!(
                "{}/api/v3/ticker/24hr",
                self.base_url.trim_end_matches('/')
            ))
            .query(&[("symbols", &symbols_json)])
            .send()
            .await?;

        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(BotError::CmcApi(format!("Binance API error: {}", err_text)));
        }

        let tickers: Vec<BinanceTicker> = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("Binance parse: {e}")))?;

        for t in tickers {
            // Convert symbol back to original base symbol, e.g. BTCUSDT -> BTC
            let base_symbol = t
                .symbol
                .strip_suffix("USDT")
                .unwrap_or(&t.symbol)
                .to_uppercase();
            let price_usd: f64 = t.last_price.parse().unwrap_or(0.0);
            let volume_24h: Option<f64> = t.quote_volume.parse().ok();
            let change_24h: Option<f64> = t.price_change_percent.parse().ok();

            let q = RawQuote {
                symbol: base_symbol.clone(),
                name: base_symbol.clone(),
                price_usd,
                market_cap: None,
                volume_24h,
                change_1h: None,
                change_24h,
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
            };

            self.cache.insert(base_symbol, q.clone()).await;
            out.push(q);
        }

        Ok(out)
    }

    #[allow(dead_code)]
    pub async fn get_top(&self, _limit: u32) -> Result<Vec<RawQuote>> {
        // Exchanges don't have a reliable ranking endpoint of top marketcap coins
        Err(BotError::CmcApi("Binance does not support get_top".into()))
    }

    pub async fn get_history(&self, symbol: &str, days: u32) -> Result<Vec<(i64, f64)>> {
        let pair = format!("{}USDT", symbol.to_uppercase());

        let (interval, limit) = match days {
            1 => ("5m", 288),
            7 => ("1h", 168),
            30 => ("4h", 180),
            90 => ("12h", 180),
            _ => ("1d", days),
        };

        let resp = self
            .http
            .get(format!(
                "{}/api/v3/klines",
                self.base_url.trim_end_matches('/')
            ))
            .query(&[
                ("symbol", pair.as_str()),
                ("interval", interval),
                ("limit", limit.to_string().as_str()),
            ])
            .send()
            .await?;

        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        if !resp.status().is_success() {
            let err_text = resp.text().await.unwrap_or_default();
            return Err(BotError::CmcApi(format!("Binance API error: {}", err_text)));
        }

        let klines: Vec<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("Binance parse: {e}")))?;

        let mut history = Vec::new();
        for k in klines {
            // kline format: [Open time, Open, High, Low, Close, Volume, Close time, Quote asset volume, Number of trades, Taker buy base asset volume, Taker buy quote asset volume, Ignore]
            if let (Some(open_time), Some(close)) = (k[0].as_i64(), k[4].as_str()) {
                if let Ok(price) = close.parse::<f64>() {
                    history.push((open_time, price));
                }
            }
        }

        if history.is_empty() {
            return Err(BotError::CmcApi("Empty history from Binance".into()));
        }

        Ok(history)
    }
}

#[derive(Deserialize)]
struct BinanceTicker {
    symbol: String,
    #[serde(rename = "priceChangePercent")]
    price_change_percent: String,
    #[serde(rename = "lastPrice")]
    last_price: String,
    #[serde(rename = "quoteVolume")]
    quote_volume: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Callers may pass the same symbol twice (watchlist + holdings overlap).
    /// The batch 24hr endpoint rejects duplicates with code -1151, so the
    /// provider must send each symbol once. If the dedupe regresses, the
    /// request becomes `["BTCUSDT","BTCUSDT"]`, no mock matches it and the
    /// call fails.
    #[tokio::test]
    async fn dedupes_symbols_in_batch_request() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::query_param("symbols", "[\"BTCUSDT\"]"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {
                        "symbol": "BTCUSDT",
                        "lastPrice": "50000.00",
                        "priceChangePercent": "1.0",
                        "quoteVolume": "1000.0"
                    }
                ])),
            )
            .mount(&server)
            .await;

        let p = BinanceProvider::new().with_base_url(&server.uri());
        let quotes = p
            .get_quotes(&["BTC", "BTC"])
            .await
            .expect("deduped call ok");
        assert_eq!(quotes.len(), 1);
        assert_eq!(quotes[0].symbol, "BTC");
        assert_eq!(quotes[0].price_usd, 50000.0);
    }
}
