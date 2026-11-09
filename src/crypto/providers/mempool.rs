use crate::models::market_intel::BtcNetworkMetrics;
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct MempoolProvider {
    http: Client,
    cache: Cache<&'static str, BtcNetworkMetrics>,
}

#[derive(Deserialize)]
struct DifficultyAdjustment {
    #[serde(rename = "progressPercent")]
    progress_percent: f64,
    #[serde(rename = "difficultyChange")]
    difficulty_change: f64,
}

#[derive(Deserialize)]
struct HashrateResp {
    #[serde(rename = "currentHashrate")]
    current_hashrate: f64,
    #[serde(rename = "currentDifficulty")]
    current_difficulty: f64,
}

#[derive(Deserialize)]
struct MempoolResp {
    count: u32,
    #[serde(rename = "vsize")]
    vsize: u64,
}

#[derive(Deserialize)]
struct FeesResp {
    #[serde(rename = "fastestFee")]
    fastest_fee: u32,
    #[serde(rename = "hourFee")]
    hour_fee: u32,
}

#[derive(Deserialize)]
struct MiningPoolsResp {
    pools: Vec<MiningPool>,
    #[serde(rename = "blockCount")]
    block_count: Option<u64>,
}

#[derive(Deserialize)]
struct MiningPool {
    name: String,
    #[serde(rename = "blockCount")]
    block_count: u64,
}

impl MempoolProvider {
    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(10))
                .use_rustls_tls()
                .build()
                .unwrap(),
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(15 * 60))
                .max_capacity(1)
                .build(),
        }
    }

    /// Fetches Bitcoin network health metrics, cached for 15 minutes.
    pub async fn get_btc_network_metrics(&self) -> BtcNetworkMetrics {
        if let Some(cached) = self.cache.get(&"btc_network").await {
            return cached;
        }

        let mut metrics = BtcNetworkMetrics::default();

        // 1. Difficulty adjustment progress
        if let Ok(resp) = self
            .http
            .get("https://mempool.space/api/v1/difficulty-adjustment")
            .send()
            .await
        {
            if let Ok(da) = resp.json::<DifficultyAdjustment>().await {
                metrics.progress_to_retarget = da.progress_percent;
                metrics.estimated_retarget_pct = da.difficulty_change;
            }
        }

        // 2. Current hashrate & difficulty
        if let Ok(resp) = self
            .http
            .get("https://mempool.space/api/v1/mining/hashrate/3d")
            .send()
            .await
        {
            if let Ok(hr) = resp.json::<HashrateResp>().await {
                // Convert H/s to EH/s
                metrics.hashrate_eh = hr.current_hashrate / 1e18;
                metrics.difficulty = hr.current_difficulty;
            }
        }

        // 3. Mempool stats
        if let Ok(resp) = self
            .http
            .get("https://mempool.space/api/mempool")
            .send()
            .await
        {
            if let Ok(mp) = resp.json::<MempoolResp>().await {
                metrics.mempool_tx_count = mp.count;
                // vsize is in vBytes, convert to MB
                metrics.mempool_vsize_mb = mp.vsize as f64 / 1_000_000.0;
            }
        }

        // 4. Recommended fees
        if let Ok(resp) = self
            .http
            .get("https://mempool.space/api/v1/fees/recommended")
            .send()
            .await
        {
            if let Ok(fees) = resp.json::<FeesResp>().await {
                metrics.fee_fastest_sat = fees.fastest_fee;
                metrics.fee_hour_sat = fees.hour_fee;
            }
        }

        // 5. Top mining pool (last 1 week)
        if let Ok(resp) = self
            .http
            .get("https://mempool.space/api/v1/mining/pools/1w")
            .send()
            .await
        {
            if let Ok(pools_resp) = resp.json::<MiningPoolsResp>().await {
                if let Some(top) = pools_resp.pools.first() {
                    metrics.top_pool_name = top.name.clone();
                    if let Some(total) = pools_resp.block_count {
                        if total > 0 {
                            metrics.top_pool_share_pct =
                                top.block_count as f64 / total as f64 * 100.0;
                        }
                    }
                }
            }
        }

        self.cache.insert("btc_network", metrics.clone()).await;
        metrics
    }
}
