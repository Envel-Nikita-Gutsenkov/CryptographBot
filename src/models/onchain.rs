use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnChainMetrics {
    pub symbol: String,
    
    // DeFi specific (ETH, SOL, LINK, etc) from DefiLlama
    pub tvl: Option<f64>,
    
    // Activity (CoinMetrics)
    pub active_addresses_24h: Option<u64>,
    pub fees_24h_native: Option<f64>,
    
    // Derivatives / Heat (Binance)
    pub funding_rate: Option<f64>,
    pub open_interest: Option<f64>,
    // Binance Futures long/short account ratio (>1.0 = more longs)
    pub long_short_ratio: Option<f64>,
    // Bybit Open Interest (native coin)
    pub bybit_oi: Option<f64>,
}

impl OnChainMetrics {
    pub fn empty(symbol: &str) -> Self {
        Self {
            symbol: symbol.to_string(),
            tvl: None,
            active_addresses_24h: None,
            fees_24h_native: None,
            funding_rate: None,
            open_interest: None,
            long_short_ratio: None,
            bybit_oi: None,
        }
    }
}
