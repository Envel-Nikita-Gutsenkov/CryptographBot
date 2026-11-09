//! Market-regime classifier: a deterministic read of *which* market we are in
//! right now (bull / bear / euphoria / sideways / dip), derived from the same
//! OHLCV series the backtests replay.
//!
//! Used in two places:
//! * **Training** — every sweep window is tagged with its regime, so the grid
//!   picks the best config *per regime* (deep-dip entries win in DIP, tight
//!   margins win in SIDEWAYS, wide trails win in BULL).
//! * **Runtime** — the bot classifies the current regime and auto-selects the
//!   best profile for it, so it self-adapts without user commands.

use crate::crypto::ai::indicators::{rsi, sma};

/// Coarse market regime. Classification order matters (first match wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MarketRegime {
    /// Near the all-time high with hot momentum — chasing tops is exactly what
    /// the ATH guard exists to prevent. Park cash.
    Euphoria,
    /// Meaningful drawdown from the recent mean with weak momentum — the
    /// accumulator's entry zone (buy the fear).
    Dip,
    /// Trend structure down (SMA20 < SMA50, price < SMA200). Park cash.
    Bear,
    /// Trend structure up (SMA20 > SMA50, price > SMA200). Accumulate.
    Bull,
    /// Everything else — no strong signal. Normal accumulation.
    Sideways,
}

impl MarketRegime {
    pub fn as_str(&self) -> &'static str {
        match self {
            MarketRegime::Euphoria => "EUPHORIA",
            MarketRegime::Dip => "DIP",
            MarketRegime::Bear => "BEAR",
            MarketRegime::Bull => "BULL",
            MarketRegime::Sideways => "SIDEWAYS",
        }
    }

    #[allow(dead_code)] // planned CLI/Telegram regime filter surface
    pub fn parse(s: &str) -> Option<MarketRegime> {
        match s.to_uppercase().as_str() {
            "EUPHORIA" => Some(MarketRegime::Euphoria),
            "DIP" => Some(MarketRegime::Dip),
            "BEAR" => Some(MarketRegime::Bear),
            "BULL" => Some(MarketRegime::Bull),
            "SIDEWAYS" => Some(MarketRegime::Sideways),
            _ => None,
        }
    }
}

/// Classify a window of closes (the last close = the state at window end).
/// Needs ~20 closes for the SMA20; shorter series → Sideways.
///
/// Rule order matters — first match wins:
/// 1. **Dip** — price ≥10% below the SMA50 but still within 20% of the
///    SMA200: a crash inside a range/bull (mean-reversion buy zone). A full
///    bear collapse (price far below the 200-day mean) is NOT a dip.
/// 2. **Euphoria** — near the ATH with hot momentum (RSI overbought AND price
///    running ≥3% above the SMA20). A flat series reads RSI=100, so the
///    momentum margin keeps flat-at-the-top out of Euphoria.
/// 3. **Bear** — trend structure down (SMA20 < SMA50, price < SMA200).
/// 4. **Bull** — trend structure up (SMA20 > SMA50, price > SMA200).
/// 5. **Sideways** — everything else.
pub fn classify_closes(closes: &[f64]) -> MarketRegime {
    if closes.len() < 20 {
        return MarketRegime::Sideways;
    }
    let price = closes[closes.len() - 1];
    let sma20 = sma(closes, 20).unwrap_or(price);
    let sma50 = sma(closes, 50).unwrap_or(price);
    let sma200 = sma(closes, 200).unwrap_or(price);
    let rsi14 = rsi(closes, 14).unwrap_or(50.0);
    let ath = closes.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let ath_dist = if ath.is_finite() && ath > 0.0 {
        (price - ath) / ath * 100.0
    } else {
        0.0
    };

    // Dip: deep below the recent mean, but not a full bear collapse.
    if price < sma50 * 0.90 && price >= sma200 * 0.80 {
        return MarketRegime::Dip;
    }
    // Euphoria: at/near the ATH with hot momentum.
    if ath_dist >= -5.0 && rsi14 >= 65.0 && price > sma20 * 1.03 {
        return MarketRegime::Euphoria;
    }
    // Bear: trend structure down.
    if sma20 < sma50 && price < sma200 {
        return MarketRegime::Bear;
    }
    // Bull: trend structure up.
    if sma20 > sma50 && price > sma200 {
        return MarketRegime::Bull;
    }
    MarketRegime::Sideways
}

/// Classify from a (ts_ms, close) history series.
pub fn classify_history(history: &[(i64, f64)]) -> MarketRegime {
    let closes: Vec<f64> = history.iter().map(|&(_, c)| c).collect();
    classify_closes(&closes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closes(prices: &[f64]) -> Vec<f64> {
        prices.to_vec()
    }

    #[test]
    fn euphoria_near_ath_with_hot_rsi() {
        // 250 days flat 100, then a run to 120 (new ATH) with RSI hot.
        let mut p = vec![100.0; 250];
        p.extend((0..30).map(|i| 100.0 + i as f64 * 20.0 / 29.0));
        assert_eq!(classify_closes(&closes(&p)), MarketRegime::Euphoria);
    }

    #[test]
    fn dip_below_sma50_with_weak_rsi() {
        // 250 days flat 100, then a 20% drop.
        let mut p = vec![100.0; 250];
        p.extend(vec![80.0; 20]);
        assert_eq!(classify_closes(&closes(&p)), MarketRegime::Dip);
    }

    #[test]
    fn bear_trend_structure_down() {
        // Steady decline: SMA20 < SMA50, price < SMA200.
        let p: Vec<f64> = (0..250).map(|i| 200.0 - i as f64 * 0.5).collect();
        assert_eq!(classify_closes(&closes(&p)), MarketRegime::Bear);
    }

    #[test]
    fn bull_trend_structure_up() {
        let p: Vec<f64> = (0..250).map(|i| 50.0 + i as f64 * 0.5).collect();
        assert_eq!(classify_closes(&closes(&p)), MarketRegime::Bull);
    }

    #[test]
    fn sideways_flat_market() {
        let p = vec![100.0; 250];
        assert_eq!(classify_closes(&closes(&p)), MarketRegime::Sideways);
    }

    #[test]
    fn short_history_falls_back_to_sideways() {
        assert_eq!(classify_closes(&[1.0, 2.0, 3.0]), MarketRegime::Sideways);
        assert_eq!(classify_history(&[]), MarketRegime::Sideways);
    }

    #[test]
    fn parse_roundtrip() {
        for r in [
            MarketRegime::Euphoria,
            MarketRegime::Dip,
            MarketRegime::Bear,
            MarketRegime::Bull,
            MarketRegime::Sideways,
        ] {
            assert_eq!(MarketRegime::parse(r.as_str()), Some(r));
        }
        assert_eq!(MarketRegime::parse("nonsense"), None);
    }
}
