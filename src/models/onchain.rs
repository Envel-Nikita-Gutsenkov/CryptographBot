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
    /// Mean funding of the last three 8h receipts (~24h).
    pub funding_rate_avg_8h: Option<f64>,
    pub open_interest: Option<f64>,
    /// Open interest change over the last 24h, in percent.
    pub oi_change_24h_pct: Option<f64>,
    /// Open interest change over the last 7d, in percent.
    pub oi_change_7d_pct: Option<f64>,
    // Binance Futures top-trader long/short account ratio (>1.0 = more longs)
    pub long_short_ratio: Option<f64>,
    /// Binance Futures global (all accounts) long/short ratio.
    pub global_long_short_ratio: Option<f64>,
    /// Binance Futures taker buy/sell volume ratio (>1.0 = aggressive buying).
    pub taker_buy_sell_ratio: Option<f64>,
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
            funding_rate_avg_8h: None,
            open_interest: None,
            oi_change_24h_pct: None,
            oi_change_7d_pct: None,
            long_short_ratio: None,
            global_long_short_ratio: None,
            taker_buy_sell_ratio: None,
            bybit_oi: None,
        }
    }

    /// True when at least one derivatives input is present.
    pub fn has_derivatives(&self) -> bool {
        self.funding_rate.is_some()
            || self.open_interest.is_some()
            || self.oi_change_24h_pct.is_some()
            || self.long_short_ratio.is_some()
    }
}

/// What the futures tape says about the current move: are longs being washed
/// out (bottom forming) or are fresh shorts piling on (still falling)?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DerivativesRead {
    /// Price down + OI down: longs closed out, leverage flushed — bottom is near.
    LongsFlushed,
    /// Price down + OI up: new shorts opening — the fall can continue.
    ShortsPiling,
    /// Price up + OI up: fresh longs funding the rally.
    TrendUp,
    /// Deeply negative funding: shorts crowded, squeeze risk to the upside.
    SqueezeRisk,
    Neutral,
}

impl DerivativesRead {
    pub fn verdict(self, russian: bool) -> &'static str {
        match (self, russian) {
            (DerivativesRead::LongsFlushed, true) => "лонги вымыты, дно близко",
            (DerivativesRead::LongsFlushed, false) => "longs flushed, bottom is near",
            (DerivativesRead::ShortsPiling, true) => "шорты набирают — рано ловить нож",
            (DerivativesRead::ShortsPiling, false) => {
                "shorts piling in — too early to catch the knife"
            }
            (DerivativesRead::TrendUp, true) => "рост на новых лонгах",
            (DerivativesRead::TrendUp, false) => "rally fuelled by fresh longs",
            (DerivativesRead::SqueezeRisk, true) => "перепроданные шорты, риск сквиза вверх",
            (DerivativesRead::SqueezeRisk, false) => {
                "shorts overcrowded, squeeze risk to the upside"
            }
            (DerivativesRead::Neutral, true) => "нейтрально",
            (DerivativesRead::Neutral, false) => "neutral",
        }
    }
}

/// Compact derivatives snapshot used by `/sniper`.
#[derive(Debug, Clone, Copy)]
pub struct DerivativesSignal {
    pub funding_avg_pct: Option<f64>,
    pub oi_change_24h_pct: Option<f64>,
    pub oi_change_7d_pct: Option<f64>,
    pub long_short_ratio: Option<f64>,
    pub taker_buy_sell: Option<f64>,
    pub price_change_24h_pct: f64,
    pub read: DerivativesRead,
}

impl DerivativesSignal {
    /// Classify the tape from the price move, OI change and funding.
    pub fn classify(
        price_change_24h_pct: f64,
        oi_change_24h_pct: Option<f64>,
        funding_avg_pct: Option<f64>,
    ) -> DerivativesRead {
        let price_down = price_change_24h_pct <= -1.0;
        let price_up = price_change_24h_pct >= 1.0;
        if let Some(oi) = oi_change_24h_pct {
            if price_down && oi <= -3.0 {
                return DerivativesRead::LongsFlushed;
            }
            if price_down && oi >= 3.0 {
                return DerivativesRead::ShortsPiling;
            }
            if price_up && oi >= 3.0 {
                return DerivativesRead::TrendUp;
            }
        }
        if funding_avg_pct.map(|f| f <= -0.015).unwrap_or(false) {
            return DerivativesRead::SqueezeRisk;
        }
        DerivativesRead::Neutral
    }

    /// Build a signal from the metrics, given the 24h price move (computed by
    /// the caller from its own price history).
    pub fn from_metrics(m: &OnChainMetrics, price_change_24h_pct: f64) -> Option<Self> {
        if !m.has_derivatives() {
            return None;
        }
        let funding_avg_pct = m.funding_rate_avg_8h.or(m.funding_rate);
        let read = Self::classify(price_change_24h_pct, m.oi_change_24h_pct, funding_avg_pct);
        Some(Self {
            funding_avg_pct,
            oi_change_24h_pct: m.oi_change_24h_pct,
            oi_change_7d_pct: m.oi_change_7d_pct,
            long_short_ratio: m.global_long_short_ratio.or(m.long_short_ratio),
            taker_buy_sell: m.taker_buy_sell_ratio,
            price_change_24h_pct,
            read,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_down_oi_down_is_long_flush() {
        assert_eq!(
            DerivativesSignal::classify(-4.0, Some(-6.0), Some(0.01)),
            DerivativesRead::LongsFlushed
        );
    }

    #[test]
    fn price_down_oi_up_is_shorts_piling() {
        assert_eq!(
            DerivativesSignal::classify(-4.0, Some(5.0), Some(0.01)),
            DerivativesRead::ShortsPiling
        );
    }

    #[test]
    fn price_up_oi_up_is_trend() {
        assert_eq!(
            DerivativesSignal::classify(3.0, Some(4.0), None),
            DerivativesRead::TrendUp
        );
    }

    #[test]
    fn deeply_negative_funding_flags_squeeze() {
        assert_eq!(
            DerivativesSignal::classify(0.2, Some(1.0), Some(-0.03)),
            DerivativesRead::SqueezeRisk
        );
    }

    #[test]
    fn empty_metrics_produce_no_signal() {
        let m = OnChainMetrics::empty("BTC");
        assert!(DerivativesSignal::from_metrics(&m, 1.0).is_none());
    }

    #[test]
    fn signal_picks_average_funding_over_latest() {
        let mut m = OnChainMetrics::empty("BTC");
        m.funding_rate = Some(0.05);
        m.funding_rate_avg_8h = Some(-0.01);
        let s = DerivativesSignal::from_metrics(&m, -2.0).unwrap();
        assert_eq!(s.funding_avg_pct, Some(-0.01));
    }
}
