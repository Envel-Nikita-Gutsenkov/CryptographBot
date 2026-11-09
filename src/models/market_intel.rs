#![allow(dead_code)]

/// Bitcoin network health metrics from Mempool.space
#[derive(Debug, Clone, Default)]
pub struct BtcNetworkMetrics {
    pub hashrate_eh: f64,
    pub difficulty: f64,
    pub mempool_tx_count: u32,
    pub mempool_vsize_mb: f64,
    pub fee_fastest_sat: u32,
    pub fee_hour_sat: u32,
    pub progress_to_retarget: f64,
    pub estimated_retarget_pct: f64,
    pub top_pool_name: String,
    pub top_pool_share_pct: f64,
}

/// Derivatives and sentiment metrics for a symbol (built from OnChainMetrics)
#[derive(Debug, Clone, Default)]
pub struct DerivativesMetrics {
    pub symbol: String,
    pub funding_rate: f64,
    pub open_interest_native: f64,
    pub bybit_oi_native: f64,
    pub long_short_ratio: f64,
}

/// Risk and liquidity scores from CoinStats API
#[derive(Debug, Clone, Default)]
pub struct CoinStatsRisk {
    pub symbol: String,
    pub liquidity_score: f64,
    pub volatility_score: f64,
    pub risk_score: f64,
}
