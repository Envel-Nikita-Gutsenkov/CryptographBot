#[derive(Debug, Clone)]
pub struct CmcQuote {
    pub symbol: String,
    pub name: String,
    pub price_usd: f64,
    pub market_cap: f64,
    pub volume_24h: f64,
    pub change_1h: Option<f64>,
    pub change_24h: Option<f64>,
    pub change_7d: Option<f64>,
    pub change_30d: Option<f64>,
    pub change_90d: Option<f64>,
    pub change_365d: Option<f64>,
    pub change_730d: Option<f64>,
    pub ath: Option<f64>,
    pub ath_change_percentage: Option<f64>,
    #[allow(dead_code)]
    pub high_24h: Option<f64>,
    #[allow(dead_code)]
    pub low_24h: Option<f64>,
    pub rank: u32,
    pub circulating_supply: f64,
    pub last_updated: chrono::DateTime<chrono::Utc>,
    pub anomaly_warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GlobalMetrics {
    pub total_market_cap: f64,
    pub total_volume_24h: f64,
    pub btc_dominance: f64,
    pub eth_dominance: f64,
    pub active_cryptocurrencies: u32,
    pub stablecoin_mcap: Option<f64>,
    pub dex_volume_24h: Option<f64>,
}
