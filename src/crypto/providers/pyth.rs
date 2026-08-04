use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;

use super::RawQuote;
use crate::error::{BotError, Result};

/// Pyth Network (Hermes API) — free, no API key, CEX+DEX aggregated prices.
/// Feed ids for the main symbols are pinned; unknown symbols are resolved
/// once via /v2/price_feeds and cached, so a batch call stays a single HTTP
/// request.
pub struct PythProvider {
    http: Client,
    /// SYMBOL (upper) -> feed id (64-hex). Pinned defaults + lazily resolved.
    feed_ids: Mutex<HashMap<String, String>>,
}

/// Verified on 2026-08-04 via hermes.pyth.network (BTC/ETH/SOL/BNB/LINK).
const KNOWN_FEEDS: &[(&str, &str)] = &[
    (
        "BTC",
        "e62df6c8b4a85fe1a67db44dc12de5db330f7ac66b72dc658afedf0f4a415b43",
    ),
    (
        "ETH",
        "ff61491a931112ddf1bd8147cd1b641375f79f5825126d665480874634fd0ace",
    ),
    (
        "SOL",
        "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d",
    ),
    (
        "BNB",
        "2f95862b045670cd22bee3114c39763a4a08beeb663b145d283c31d7d1101c4f",
    ),
    (
        "LINK",
        "8ac0c70fff57e9aefdf5edf44b51d62c2d433653cbb2cf5cc06bb115af04d221",
    ),
];

#[derive(Deserialize)]
struct PythResp {
    parsed: Vec<PythPrice>,
}

#[derive(Deserialize)]
struct PythPrice {
    id: String,
    price: PythVal,
}

#[derive(Deserialize)]
struct PythVal {
    price: i64,
    expo: i32,
}

#[derive(Deserialize)]
struct FeedAttributes {
    base: String,
    quote_currency: String,
}

#[derive(Deserialize)]
struct Feed {
    id: String,
    attributes: FeedAttributes,
}

impl PythProvider {
    pub const NAME: &'static str = "pyth";

    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                .build()
                .unwrap(),
            feed_ids: Mutex::new(
                KNOWN_FEEDS
                    .iter()
                    .map(|(s, id)| (s.to_string(), id.to_string()))
                    .collect(),
            ),
        }
    }

    async fn resolve_feed_id(&self, symbol: &str) -> Option<String> {
        let key = symbol.to_uppercase();
        if let Some(id) = self.feed_ids.lock().unwrap().get(&key).cloned() {
            return Some(id);
        }
        let url = format!(
            "https://hermes.pyth.network/v2/price_feeds?query={}&asset_type=crypto",
            symbol
        );
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", "cryptograph/1.0")
            .send()
            .await
            .ok()?;
        let feeds: Vec<Feed> = resp.json().await.ok()?;
        let id = feeds
            .into_iter()
            .find(|f| {
                f.attributes.base.eq_ignore_ascii_case(symbol)
                    && f.attributes.quote_currency == "USD"
            })?
            .id;
        self.feed_ids.lock().unwrap().insert(key, id.clone());
        Some(id)
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<RawQuote>> {
        let mut id_to_sym: HashMap<String, String> = HashMap::new();
        for &s in symbols {
            if let Some(id) = self.resolve_feed_id(s).await {
                id_to_sym.insert(id, s.to_uppercase());
            }
        }
        if id_to_sym.is_empty() {
            return Ok(vec![]);
        }

        let ids: Vec<&String> = id_to_sym.keys().collect();
        let url = "https://hermes.pyth.network/v2/updates/price/latest?".to_string()
            + &ids
                .iter()
                .map(|id| format!("ids[]={}", id))
                .collect::<Vec<_>>()
                .join("&");
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", "cryptograph/1.0")
            .send()
            .await?;
        let body: PythResp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(e.to_string()))?;

        let mut out = Vec::with_capacity(body.parsed.len());
        for p in body.parsed {
            if let Some(sym) = id_to_sym.get(&p.id) {
                let price = p.price.price as f64 * 10f64.powi(p.price.expo);
                if price > 0.0 && price.is_finite() {
                    out.push(RawQuote {
                        symbol: sym.clone(),
                        name: sym.clone(),
                        price_usd: price,
                        market_cap: None,
                        volume_24h: None,
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
                    });
                }
            }
        }
        Ok(out)
    }
}
