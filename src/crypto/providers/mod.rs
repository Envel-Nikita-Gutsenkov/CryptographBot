pub mod alternative_me;
pub mod binance;
pub mod bybit;
pub mod cmc;
pub mod coinbase;
pub mod coincap;
pub mod coingecko;
pub mod coinpaprika;
pub mod coinstats;
pub mod cryptocompare;
pub mod defillama;
pub mod dexscreener;
pub mod geckoterminal;
pub mod mempool;
pub mod onchain;
pub mod pyth;
pub mod rss;
pub mod yfinance;
/// Unified quote type returned by all providers.
/// Fields are Optional because not all sources provide all data.
#[derive(Debug, Clone)]
pub struct RawQuote {
    pub symbol: String,
    pub name: String,
    pub price_usd: f64,
    pub market_cap: Option<f64>,
    pub volume_24h: Option<f64>,
    pub change_1h: Option<f64>,
    pub change_24h: Option<f64>,
    pub change_7d: Option<f64>,
    pub change_30d: Option<f64>,
    pub change_90d: Option<f64>,
    pub change_365d: Option<f64>,
    pub change_730d: Option<f64>,
    pub ath: Option<f64>,
    pub ath_change_percentage: Option<f64>,
    pub high_24h: Option<f64>,
    pub low_24h: Option<f64>,
    pub rank: Option<u32>,
    pub circulating_supply: Option<f64>,
    /// Which provider produced this quote (for diagnostics).
    pub source: &'static str,
}

impl RawQuote {
    /// Convert to canonical CmcQuote, filling missing fields with 0.
    pub fn into_canonical(self) -> crate::models::CmcQuote {
        crate::models::CmcQuote {
            symbol: self.symbol,
            name: self.name,
            price_usd: self.price_usd,
            market_cap: self.market_cap.unwrap_or(0.0),
            volume_24h: self.volume_24h.unwrap_or(0.0),
            change_1h: self.change_1h,
            change_24h: self.change_24h,
            change_7d: self.change_7d,
            change_30d: self.change_30d,
            change_90d: self.change_90d,
            change_365d: self.change_365d,
            change_730d: self.change_730d,
            ath: self.ath,
            ath_change_percentage: self.ath_change_percentage,
            high_24h: self.high_24h,
            low_24h: self.low_24h,
            rank: self.rank.unwrap_or(0),
            circulating_supply: self.circulating_supply.unwrap_or(0.0),
            last_updated: chrono::Utc::now(),
            anomaly_warning: None,
        }
    }
}

/// Per-source fetch outcome, including latency for health tracking.
#[derive(Debug)]
#[allow(dead_code)]
pub struct SourceFetch {
    pub source: &'static str,
    pub quotes: Result<Vec<RawQuote>, String>,
    pub latency_ms: u64,
}
