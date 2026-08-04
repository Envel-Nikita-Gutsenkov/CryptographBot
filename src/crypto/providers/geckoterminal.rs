use std::time::Duration;
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use crate::error::{BotError, Result};

pub struct GeckoTerminalProvider {
    http: Client,
    pool_cache: Cache<String, Option<GtPoolRef>>,
}

#[derive(Clone)]
struct GtPoolRef {
    network: String,
    address: String,
}

impl GeckoTerminalProvider {
    pub const NAME: &'static str = "geckoterminal";

    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(10))
                .use_rustls_tls()
                .build()
                .unwrap(),
            pool_cache: Cache::builder()
                .time_to_live(Duration::from_secs(3600))
                .max_capacity(500)
                .build(),
        }
    }

    async fn find_pool(&self, symbol: &str) -> Result<Option<GtPoolRef>> {
        let sym_upper = symbol.to_uppercase();
        if let Some(cached) = self.pool_cache.get(&sym_upper).await {
            return Ok(cached);
        }

        let resp = self.http
            .get("https://api.geckoterminal.com/api/v2/search/pools")
            .query(&[("query", symbol)])
            .send()
            .await?;

        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        let body: GtSearchResp = match resp.json().await {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };

        // Find best matching pool
        let mut best_pool = None;
        let mut max_reserve = 0.0;

        for pool in body.data {
            let reserve = pool.attributes.reserve_in_usd.parse::<f64>().unwrap_or(0.0);
            if reserve > max_reserve {
                max_reserve = reserve;
                let network = pool.relationships.network.data.id;
                let address = pool.attributes.address;
                best_pool = Some(GtPoolRef { network, address });
            }
        }

        self.pool_cache.insert(sym_upper, best_pool.clone()).await;
        Ok(best_pool)
    }

    pub async fn get_history(&self, symbol: &str, days: &str) -> Result<Vec<(i64, f64)>> {
        let pool = self.find_pool(symbol).await?.ok_or_else(|| BotError::SymbolNotFound(symbol.to_string()))?;

        // Determine timeframe based on days
        let timeframe = match days {
            "1d" => "hour",
            "7d" => "hour",
            "30d" => "day",
            "90d" => "day",
            "180d" => "day",
            "365d" => "day",
            _ => "hour",
        };

        // GeckoTerminal limits points, so for 7d with 'hour' it might require pagination, but we just request max limit
        let limit = match timeframe {
            "hour" => {
                let d = days.strip_suffix('d').unwrap_or("1").parse::<u32>().unwrap_or(1);
                (d * 24).min(1000).to_string()
            }
            "day" => days.strip_suffix('d').unwrap_or("30").to_string(),
            _ => "100".to_string(),
        };

        let url = format!("https://api.geckoterminal.com/api/v2/networks/{}/pools/{}/ohlcv/{}", pool.network, pool.address, timeframe);
        
        let resp = self.http
            .get(&url)
            .query(&[("limit", limit)])
            .send()
            .await?;

        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        let body: GtOhlcvResp = resp.json().await.map_err(|e| BotError::CmcApi(e.to_string()))?;
        
        // GeckoTerminal OHLCV format: [timestamp, open, high, low, close, volume]
        let mut points = Vec::new();
        if let Some(meta) = body.data {
            if let Some(ohlcv) = meta.attributes.ohlcv_list {
                for row in ohlcv {
                    if row.len() >= 5 {
                        let ts = row[0] as i64 * 1000; // GT returns seconds, we need ms
                        let close = row[4];
                        points.push((ts, close));
                    }
                }
            }
        }

        // GT returns newest to oldest, we need oldest to newest for charts
        points.reverse();
        
        if points.is_empty() {
            return Err(BotError::SymbolNotFound(symbol.to_string()));
        }

        Ok(points)
    }
}

#[derive(Deserialize)]
struct GtSearchResp {
    data: Vec<GtPool>,
}

#[derive(Deserialize)]
struct GtPool {
    attributes: GtPoolAttributes,
    relationships: GtPoolRelationships,
}

#[derive(Deserialize)]
struct GtPoolAttributes {
    address: String,
    reserve_in_usd: String,
}

#[derive(Deserialize)]
struct GtPoolRelationships {
    network: GtNetworkData,
}

#[derive(Deserialize)]
struct GtNetworkData {
    data: GtNetworkId,
}

#[derive(Deserialize)]
struct GtNetworkId {
    id: String,
}

#[derive(Deserialize)]
struct GtOhlcvResp {
    data: Option<GtOhlcvData>,
}

#[derive(Deserialize)]
struct GtOhlcvData {
    attributes: GtOhlcvAttributes,
}

#[derive(Deserialize)]
struct GtOhlcvAttributes {
    ohlcv_list: Option<Vec<Vec<f64>>>,
}
