use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;
use tracing::warn;

use crate::error::Result;

pub const NAME: &str = "cryptocompare";

pub struct CryptoCompareProvider {
    http: Client,
    api_key: String,
}

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct NewsArticle {
    pub title: String,
    pub source: String,
    pub body: String,
    pub url: String,
}

impl CryptoCompareProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(8))
            .use_rustls_tls()
            .build()
            .unwrap();
        Self {
            http,
            api_key: api_key.into(),
        }
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<super::RawQuote>> {
        if symbols.is_empty() { return Ok(Vec::new()); }
        let fsyms = symbols.join(",");
        let url = format!("https://min-api.cryptocompare.com/data/pricemultifull?fsyms={}&tsyms=USD", fsyms);
        let mut req = self.http.get(&url);
        if !self.api_key.is_empty() && !self.api_key.starts_with("xxxxxx") && self.api_key != "dummy" {
            req = req.header("authorization", format!("Apikey {}", self.api_key));
        }
        let resp: serde_json::Value = req.send().await?.json().await?;
        
        let mut quotes = Vec::new();
        if let Some(raw) = resp.get("RAW").and_then(|r| r.as_object()) {
            for (sym, data) in raw {
                if let Some(usd) = data.get("USD") {
                    let price_usd = usd.get("PRICE").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    if price_usd <= 0.0 { continue; }
                    
                    quotes.push(super::RawQuote {
                        symbol: sym.to_uppercase(),
                        name: sym.to_uppercase(), // CryptoCompare doesn't give full name here
                        price_usd,
                        market_cap: usd.get("MKTCAP").and_then(|v| v.as_f64()),
                        volume_24h: usd.get("TOTALVOLUME24HTO").and_then(|v| v.as_f64()),
                        change_1h: usd.get("CHANGEPCTHOUR").and_then(|v| v.as_f64()),
                        change_24h: usd.get("CHANGEPCT24HOUR").and_then(|v| v.as_f64()),
                        change_7d: None,
                        change_30d: None,
                        change_90d: None,
                        change_365d: None,
                        change_730d: None,
                        ath: None,
                        ath_change_percentage: None,
                        high_24h: usd.get("HIGH24HOUR").and_then(|v| v.as_f64()),
                        low_24h: usd.get("LOW24HOUR").and_then(|v| v.as_f64()),
                        rank: None,
                        circulating_supply: usd.get("SUPPLY").and_then(|v| v.as_f64()),
                        source: NAME,
                    });
                }
            }
        }
        Ok(quotes)
    }

    /// Fetches the latest 3-5 news articles for the given symbol to keep the context optimized.
    pub async fn get_news(&self, symbol: &str) -> Result<Vec<NewsArticle>> {
        if self.api_key.is_empty() || self.api_key.starts_with("xxxxxx") || self.api_key == "dummy" {
            return Ok(Vec::new());
        }

        let sym_upper = symbol.to_uppercase();
        let url = if sym_upper == "ALL" || sym_upper == "GENERAL" || sym_upper.is_empty() {
            "https://min-api.cryptocompare.com/data/v2/news/?lang=EN".to_string()
        } else {
            format!(
                "https://min-api.cryptocompare.com/data/v2/news/?categories={}&lang=EN",
                sym_upper
            )
        };

        let req = self.http.get(&url).header("authorization", format!("Apikey {}", self.api_key));

        let resp: serde_json::Value = req.send().await?.json().await?;

        if let Some(err) = resp.get("Response").and_then(|r| r.as_str()) {
            if err == "Error" {
                warn!("CryptoCompare news error: {:?}", resp.get("Message"));
                return Ok(Vec::new());
            }
        }

        let mut articles = Vec::new();
        if let Some(data) = resp.get("Data").and_then(|d| d.as_array()) {
            for item in data.iter().take(50) {
                let title = item["title"].as_str().unwrap_or("").to_string();
                let source = item["source"].as_str().unwrap_or("").to_string();
                let mut body = item["body"].as_str().unwrap_or("").to_string();
                
                // Truncate body if it's too long, safely handling multi-byte UTF-8 characters
                if body.chars().count() > 800 {
                    body = body.chars().take(797).collect::<String>();
                    body.push_str("...");
                }
                
                articles.push(NewsArticle { title, source, body, url: String::new() });
            }
        }

        Ok(articles)
    }

    pub async fn get_history(&self, symbol: &str, days: u32) -> Result<Vec<(i64, f64)>> {
        if self.api_key.is_empty() || self.api_key.starts_with("xxxxxx") {
            return Err(crate::error::BotError::CmcApi("CryptoCompare API key missing".into()));
        }

        let (endpoint, limit) = match days {
            1 => ("histominute", 1440),
            7 => ("histohour", 168),
            _ => ("histoday", days),
        };

        let url = format!(
            "https://min-api.cryptocompare.com/data/v2/{}?fsym={}&tsym=USD&limit={}",
            endpoint,
            symbol.to_uppercase(),
            limit
        );

        let req = self.http.get(&url).header("authorization", format!("Apikey {}", self.api_key));
        let resp: serde_json::Value = req.send().await?.json().await?;

        if let Some(err) = resp.get("Response").and_then(|r| r.as_str()) {
            if err == "Error" {
                return Err(crate::error::BotError::CmcApi(format!("CryptoCompare API error: {:?}", resp.get("Message"))));
            }
        }

        let mut history = Vec::new();
        if let Some(data) = resp.get("Data").and_then(|d| d.get("Data")).and_then(|d| d.as_array()) {
            for item in data {
                if let (Some(time), Some(close)) = (item.get("time").and_then(|t| t.as_i64()), item.get("close").and_then(|c| c.as_f64())) {
                    // CryptoCompare returns time in seconds, typically we use milliseconds for charting
                    history.push((time * 1000, close));
                }
            }
        }

        if history.is_empty() {
            return Err(crate::error::BotError::CmcApi("Empty history from CryptoCompare".into()));
        }

        Ok(history)
    }
}
