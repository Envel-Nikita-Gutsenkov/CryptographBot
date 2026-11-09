use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

use super::RawQuote;
use crate::error::{BotError, Result};

pub struct DefiLlamaProvider {
    http: Client,
}

#[derive(Deserialize)]
struct DlPriceResp {
    coins: HashMap<String, DlCoinData>,
}

#[derive(Deserialize)]
struct DlCoinData {
    price: f64,
    symbol: String,
}

#[derive(Deserialize)]
struct DlChartResp {
    coins: HashMap<String, DlChartData>,
}

#[derive(Deserialize)]
struct DlChartData {
    prices: Vec<DlPricePoint>,
}

#[derive(Deserialize)]
struct DlPricePoint {
    timestamp: i64,
    price: f64,
}

impl DefiLlamaProvider {
    pub const NAME: &'static str = "defillama";

    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(10))
                .use_rustls_tls()
                .build()
                .unwrap(),
        }
    }

    /// Queries `https://coins.llama.fi/prices/current/{coingecko:id,...}`
    pub async fn get_quotes(&self, symbol_id_pairs: &[(&str, &str)]) -> Result<Vec<RawQuote>> {
        if symbol_id_pairs.is_empty() {
            return Ok(vec![]);
        }

        let coins_param = symbol_id_pairs
            .iter()
            .map(|(_, id)| format!("coingecko:{}", id))
            .collect::<Vec<_>>()
            .join(",");

        let url = format!("https://coins.llama.fi/prices/current/{}", coins_param);

        let resp = self.http.get(&url).send().await?;
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        let body: DlPriceResp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(e.to_string()))?;

        let mut out = Vec::new();
        for (symbol, id) in symbol_id_pairs {
            let key = format!("coingecko:{}", id);
            if let Some(coin) = body.coins.get(&key) {
                out.push(RawQuote {
                    symbol: symbol.to_string().to_uppercase(), // Keep original requested symbol
                    name: coin.symbol.clone(),
                    price_usd: coin.price,
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

        Ok(out)
    }

    /// Queries `https://coins.llama.fi/chart/{coingecko:id}?start={timestamp}&span={days}`
    pub async fn get_history(&self, cg_id: &str, days: &str) -> Result<Vec<(i64, f64)>> {
        let days_num = days
            .strip_suffix('d')
            .unwrap_or("30")
            .parse::<i64>()
            .unwrap_or(30);
        let start_ts = chrono::Utc::now().timestamp() - (days_num * 86400);

        let url = format!(
            "https://coins.llama.fi/chart/coingecko:{}?start={}&span={}d&period=1h",
            cg_id, start_ts, days_num
        );

        let resp = self.http.get(&url).send().await?;
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        let body: DlChartResp = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(e.to_string()))?;

        let key = format!("coingecko:{}", cg_id);
        let data = body
            .coins
            .get(&key)
            .ok_or_else(|| BotError::SymbolNotFound(cg_id.to_string()))?;

        let mut points = Vec::new();
        for pt in &data.prices {
            points.push((pt.timestamp * 1000, pt.price));
        }

        if points.is_empty() {
            return Err(BotError::SymbolNotFound(cg_id.to_string()));
        }

        Ok(points)
    }

    /// Fetches global DeFi metrics (Stablecoin Market Cap and DEX Volume)
    pub async fn get_global_metrics(&self) -> (Option<f64>, Option<f64>) {
        let mut stablecoin_mcap = None;
        let mut dex_volume = None;

        // 1. Fetch Stablecoin Market Cap
        if let Ok(resp) = self
            .http
            .get("https://stablecoins.llama.fi/stablecoincharts/all")
            .send()
            .await
        {
            if let Ok(mut body) = resp.json::<Vec<serde_json::Value>>().await {
                if let Some(last) = body.pop() {
                    if let Some(circulating) = last
                        .get("totalCirculatingUSD")
                        .and_then(|t| t.get("peggedUSD"))
                        .and_then(|v| v.as_f64())
                    {
                        stablecoin_mcap = Some(circulating);
                    }
                }
            }
        }

        // 2. Fetch DEX Volume (24h)
        let dex_url = "https://api.llama.fi/overview/dexs?excludeTotalDataChart=true&excludeTotalDataChartBreakdown=true&dataType=dailyVolume";
        if let Ok(resp) = self.http.get(dex_url).send().await {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                if let Some(total) = body.get("total24h").and_then(|v| v.as_f64()) {
                    dex_volume = Some(total);
                }
            }
        }

        (stablecoin_mcap, dex_volume)
    }
}
