//! Free, keyless "market sentiment" sources: CoinMarketCap headlines and
//! trending searches, Binance exchange announcements, and CoinGecko community
//! votes (fetched through [`CoinGeckoProvider`]).
//!
//! These endpoints are public web APIs, not the keyed `pro-api`. They are
//! treated as best-effort: one failing source must never take the others down,
//! so each fetch returns its own `Result` and the worker logs and skips.

use crate::error::{BotError, Result};
use crate::models::MarketSignal;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

/// Binance announcement catalogs that can move a holding: new listings,
/// latest exchange news, delistings and maintenance/upgrades.
const RELEVANT_CATALOGS: &[i64] = &[48, 49, 157, 161];

pub struct SignalsProvider {
    http: Client,
}

impl SignalsProvider {
    pub fn new() -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                 (KHTML, like Gecko) Chrome/120.0 Safari/537.36",
            ),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        Self {
            http: Client::builder()
                .default_headers(headers)
                .timeout(Duration::from_secs(10))
                .use_rustls_tls()
                .build()
                .unwrap(),
        }
    }

    /// CoinMarketCap's public headlines feed. Market-wide, no key required;
    /// each item carries the assets it mentions, but the tickers are not in the
    /// payload, so items are stored as `GENERAL` and matched to a coin by name
    /// later (same word-boundary matching as the RSS feed).
    pub async fn market_news(&self, limit: u32) -> Result<Vec<MarketSignal>> {
        #[derive(Deserialize)]
        struct Resp {
            data: Vec<Item>,
        }
        #[derive(Deserialize)]
        struct Item {
            slug: Option<String>,
            #[serde(rename = "createdAt", default)]
            created_at: Option<String>,
            meta: Option<Meta>,
        }
        #[derive(Deserialize)]
        struct Meta {
            title: Option<String>,
            subtitle: Option<String>,
            content: Option<String>,
        }

        let resp = self
            .http
            .get("https://api.coinmarketcap.com/content/v3/news")
            .query(&[("pageSize", limit.to_string())])
            .send()
            .await?;
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }
        if !resp.status().is_success() {
            return Err(BotError::CmcApi(format!(
                "CMC content news HTTP {}",
                resp.status()
            )));
        }
        let body: Resp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("CMC content news parse: {e}")))?;

        let mut out = Vec::new();
        for item in body.data {
            let Some(meta) = item.meta else { continue };
            let Some(title) = meta.title.filter(|t| !t.trim().is_empty()) else {
                continue;
            };
            let raw = meta
                .content
                .filter(|c| !c.trim().is_empty())
                .or(meta.subtitle)
                .unwrap_or_default();
            let body = crate::crypto::providers::rss::strip_html(&raw)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let slug = item.slug.unwrap_or_default();
            let url = if slug.is_empty() {
                String::new()
            } else {
                format!("https://coinmarketcap.com/headlines/news/{slug}/")
            };
            let mut signal =
                MarketSignal::new(MarketSignal::KIND_NEWS, "coinmarketcap", "GENERAL", title)
                    .with_body(crate::format::truncate_chars(&body, 700))
                    .with_url(url);
            if let Some(at) = item.created_at {
                signal = signal.with_published_at(at);
            }
            out.push(signal);
        }
        Ok(out)
    }

    /// What the crowd is searching for right now. Position is the score, so a
    /// rising rank is visible without any AI pass.
    pub async fn trending(&self) -> Result<Vec<MarketSignal>> {
        #[derive(Deserialize)]
        struct Resp {
            data: Data,
        }
        #[derive(Deserialize)]
        struct Data {
            #[serde(rename = "cryptoTopSearchRanks", default)]
            ranks: Vec<Item>,
        }
        #[derive(Deserialize)]
        struct Item {
            symbol: Option<String>,
            name: Option<String>,
        }

        let resp = self
            .http
            .get("https://api.coinmarketcap.com/data-api/v3/topsearch/rank")
            .send()
            .await?;
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }
        if !resp.status().is_success() {
            return Err(BotError::CmcApi(format!(
                "CMC topsearch HTTP {}",
                resp.status()
            )));
        }
        let body: Resp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("CMC topsearch parse: {e}")))?;

        Ok(body
            .data
            .ranks
            .into_iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let symbol = item.symbol.filter(|s| !s.trim().is_empty())?;
                let name = item.name.unwrap_or_else(|| symbol.clone());
                Some(
                    MarketSignal::new(MarketSignal::KIND_TRENDING, "coinmarketcap", &symbol, name)
                        .with_score((i + 1) as f64),
                )
            })
            .collect())
    }

    /// Binance's public CMS feed: new listings, delistings, maintenance and
    /// exchange news. Stored as `GENERAL` and matched by name, like the
    /// headlines.
    pub async fn binance_announcements(&self, limit: u32) -> Result<Vec<MarketSignal>> {
        #[derive(Deserialize)]
        struct Resp {
            data: Option<Data>,
        }
        #[derive(Deserialize)]
        struct Data {
            #[serde(default)]
            catalogs: Vec<Catalog>,
        }
        #[derive(Deserialize)]
        struct Catalog {
            #[serde(rename = "catalogId", default)]
            catalog_id: Option<i64>,
            #[serde(rename = "catalogName", default)]
            catalog_name: Option<String>,
            #[serde(default)]
            articles: Vec<Article>,
        }
        #[derive(Deserialize)]
        struct Article {
            #[serde(default)]
            code: Option<String>,
            #[serde(default)]
            title: Option<String>,
            #[serde(rename = "releaseDate", default)]
            release_date: Option<i64>,
        }

        let resp = self
            .http
            .get(
                "https://www.binance.com/bapi/composite/v1/public/cms/article/list/query\
                 ?type=1&pageNo=1&pageSize=20",
            )
            .send()
            .await?;
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }
        if !resp.status().is_success() {
            return Err(BotError::CmcApi(format!(
                "Binance announcements HTTP {}",
                resp.status()
            )));
        }
        let body: Resp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(format!("Binance announcements parse: {e}")))?;

        let mut out = Vec::new();
        for catalog in body.data.map(|d| d.catalogs).unwrap_or_default() {
            if !catalog
                .catalog_id
                .map(|id| RELEVANT_CATALOGS.contains(&id))
                .unwrap_or(false)
            {
                continue;
            }
            let category = catalog.catalog_name.unwrap_or_default();
            for article in catalog.articles.into_iter().take(limit as usize) {
                let Some(title) = article.title.filter(|t| !t.trim().is_empty()) else {
                    continue;
                };
                let code = article.code.unwrap_or_default();
                let url = if code.is_empty() {
                    String::new()
                } else {
                    format!("https://www.binance.com/en/support/announcement/{code}")
                };
                let labelled = if category.is_empty() {
                    title
                } else {
                    format!("[{category}] {title}")
                };
                let mut signal = MarketSignal::new(
                    MarketSignal::KIND_ANNOUNCEMENT,
                    "binance",
                    "GENERAL",
                    labelled,
                )
                .with_url(url);
                if let Some(ms) = article.release_date {
                    if let Some(dt) = chrono::DateTime::from_timestamp_millis(ms) {
                        signal = signal.with_published_at(dt.to_rfc3339());
                    }
                }
                out.push(signal);
            }
        }
        Ok(out)
    }
}

impl Default for SignalsProvider {
    fn default() -> Self {
        Self::new()
    }
}
