use crate::models::market_intel::CoinStatsRisk;
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

#[allow(dead_code)] // Reserved for future risk-scoring integration in Market Intel Worker
pub struct CoinStatsProvider {
    http: Client,
    api_key: String,
    risk_cache: Cache<String, CoinStatsRisk>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct CoinStatsCoin {
    symbol: String,
    #[serde(rename = "liquidityScore", default)]
    liquidity_score: f64,
    #[serde(rename = "volatilityScore", default)]
    volatility_score: f64,
    #[serde(rename = "riskScore", default)]
    risk_score: f64,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct CoinStatsListResp {
    result: Vec<CoinStatsCoin>,
}

#[allow(dead_code)]
impl CoinStatsProvider {
    pub fn new() -> Self {
        let api_key = std::env::var("COINSTATS_API_KEY").unwrap_or_default();
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(10))
                .use_rustls_tls()
                .build()
                .unwrap(),
            api_key,
            risk_cache: Cache::builder()
                .time_to_live(Duration::from_secs(5 * 60))
                .max_capacity(200)
                .build(),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// Fetch risk/liquidity scores for a list of symbols.
    pub async fn get_risk_scores(&self, symbols: &[&str]) -> Vec<CoinStatsRisk> {
        if !self.is_configured() {
            return vec![];
        }

        let mut result = Vec::new();
        let mut missing: Vec<&str> = Vec::new();

        for &sym in symbols {
            let key = sym.to_uppercase();
            if let Some(cached) = self.risk_cache.get(&key).await {
                result.push(cached);
            } else {
                missing.push(sym);
            }
        }

        if missing.is_empty() {
            return result;
        }

        // CoinStats free tier: fetch top 100 to find what we need
        let url = "https://api.coinstats.app/v1/coins?limit=100&currency=USD";
        match self
            .http
            .get(url)
            .header("X-API-KEY", &self.api_key)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(body) = resp.json::<CoinStatsListResp>().await {
                    for coin in body.result {
                        let sym_upper = coin.symbol.to_uppercase();
                        let risk = CoinStatsRisk {
                            symbol: sym_upper.clone(),
                            liquidity_score: coin.liquidity_score,
                            volatility_score: coin.volatility_score,
                            risk_score: coin.risk_score,
                        };
                        self.risk_cache
                            .insert(sym_upper.clone(), risk.clone())
                            .await;
                        if missing.iter().any(|s| s.to_uppercase() == sym_upper) {
                            result.push(risk);
                        }
                    }
                }
            }
            Ok(resp) => {
                tracing::warn!("CoinStats API error: {}", resp.status());
            }
            Err(e) => {
                tracing::warn!("CoinStats request failed: {}", e);
            }
        }

        result
    }
}
