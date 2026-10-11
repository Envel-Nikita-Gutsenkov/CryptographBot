//! Technical indicators computed from local OHLCV history.
//!
//! Gives the model a "chart view" instead of dry percentage deltas: RSI,
//! moving averages, support/resistance levels and volatility. All functions
//! are pure and cheap — they run over the last N days of samples the bot
//! already stores locally, so no extra API calls.

/// Simple moving average over the last `period` closes. `None` when there
/// aren't enough samples.
pub fn sma(closes: &[f64], period: usize) -> Option<f64> {
    if closes.len() < period || period == 0 {
        return None;
    }
    let sum: f64 = closes[closes.len() - period..].iter().sum();
    Some(sum / period as f64)
}

/// RSI (Wilder's smoothing) over `period` closes. `None` when there aren't
/// enough samples. Returns 0..100.
pub fn rsi(closes: &[f64], period: usize) -> Option<f64> {
    if closes.len() < period + 1 || period == 0 {
        return None;
    }
    let mut gains = 0.0;
    let mut losses = 0.0;
    // The LAST `period` windows — `.rev()` matters: without it, `.take(period)`
    // grabs the first windows of the series and the RSI freezes at the value
    // from the first days instead of tracking recent price action.
    for w in closes.windows(2).rev().take(period) {
        let diff = w[1] - w[0];
        if diff >= 0.0 {
            gains += diff;
        } else {
            losses -= diff;
        }
    }
    let avg_gain = gains / period as f64;
    let avg_loss = losses / period as f64;
    if avg_loss.abs() < f64::EPSILON {
        return Some(100.0);
    }
    let rs = avg_gain / avg_loss;
    Some(100.0 - 100.0 / (1.0 + rs))
}

/// Highest high and lowest low over the last `period` samples.
pub fn support_resistance(highs: &[f64], lows: &[f64], period: usize) -> Option<(f64, f64)> {
    if highs.len() < period || lows.len() < period || period == 0 {
        return None;
    }
    let hi = highs[highs.len() - period..]
        .iter()
        .cloned()
        .fold(f64::NEG_INFINITY, f64::max);
    let lo = lows[lows.len() - period..]
        .iter()
        .cloned()
        .fold(f64::INFINITY, f64::min);
    Some((lo, hi))
}

/// Annualized volatility (stddev of log returns) over the last `period`
/// samples, expressed as a percentage. `None` when there aren't enough
/// samples or prices are non-positive.
pub fn volatility(closes: &[f64], period: usize) -> Option<f64> {
    if closes.len() < period + 1 || period == 0 {
        return None;
    }
    let mut log_returns = Vec::with_capacity(period);
    for w in closes[closes.len() - period - 1..].windows(2) {
        if w[0] > 0.0 && w[1] > 0.0 {
            log_returns.push((w[1] / w[0]).ln());
        }
    }
    if log_returns.len() < 2 {
        return None;
    }
    let mean = log_returns.iter().sum::<f64>() / log_returns.len() as f64;
    let var = log_returns
        .iter()
        .map(|r| (r - mean) * (r - mean))
        .sum::<f64>()
        / (log_returns.len() - 1) as f64;
    Some(var.sqrt() * 100.0)
}

/// Build a compact "chart view" text block for one symbol from OHLCV samples.
/// Returns `None` when there isn't enough data to say anything meaningful.
pub fn chart_context(
    symbol: &str,
    ohlcv: &[(i64, Option<f64>, Option<f64>, Option<f64>, f64)],
) -> Option<String> {
    if ohlcv.len() < 14 {
        return None;
    }
    let closes: Vec<f64> = ohlcv.iter().map(|(_, _, _, _, c)| *c).collect();
    let highs: Vec<f64> = ohlcv
        .iter()
        .map(|(_, _, h, _, _)| h.unwrap_or(*ohlcv.last().map(|(_, _, _, _, c)| c).unwrap_or(&0.0)))
        .collect();
    let lows: Vec<f64> = ohlcv
        .iter()
        .map(|(_, _, _, l, _)| l.unwrap_or(*ohlcv.last().map(|(_, _, _, _, c)| c).unwrap_or(&0.0)))
        .collect();

    let last = closes.last().copied().unwrap_or(0.0);
    let first = closes.first().copied().unwrap_or(0.0);
    let span_pct = if first > 0.0 {
        (last - first) / first * 100.0
    } else {
        0.0
    };

    let rsi_14 = rsi(&closes, 14);
    let sma_20 = sma(&closes, 20);
    let sma_50 = sma(&closes, 50);
    let sr = support_resistance(&highs, &lows, 20);
    let vol = volatility(&closes, 20);

    let mut out = format!(
        "- {}: {:.2} → {:.2} ({:+.1}% over window) | RSI(14): {}",
        symbol,
        first,
        last,
        span_pct,
        rsi_14
            .map(|r| format!("{:.1}", r))
            .unwrap_or_else(|| "N/A".into())
    );
    if let Some(s) = sma_20 {
        out.push_str(&format!(" | SMA20: {:.2}", s));
    }
    if let Some(s) = sma_50 {
        out.push_str(&format!(" | SMA50: {:.2}", s));
    }
    if let Some((lo, hi)) = sr {
        out.push_str(&format!(" | S/R(20): {:.2}–{:.2}", lo, hi));
    }
    if let Some(v) = vol {
        out.push_str(&format!(" | vol: {:.1}%", v));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closes(prices: &[f64]) -> Vec<f64> {
        prices.to_vec()
    }

    #[test]
    fn sma_basic() {
        assert_eq!(sma(&closes(&[1.0, 2.0, 3.0, 4.0]), 2), Some(3.5));
        assert_eq!(sma(&closes(&[1.0, 2.0]), 3), None);
        assert_eq!(sma(&closes(&[]), 2), None);
    }

    #[test]
    fn rsi_flat_is_100() {
        assert_eq!(rsi(&closes(&[10.0; 20]), 14), Some(100.0));
    }

    #[test]
    fn rsi_all_down_is_0() {
        let down: Vec<f64> = (0..20).map(|i| 100.0 - i as f64).collect();
        assert_eq!(rsi(&closes(&down), 14), Some(0.0));
    }

    #[test]
    fn rsi_insufficient_data() {
        assert_eq!(rsi(&closes(&[1.0, 2.0]), 14), None);
    }

    #[test]
    fn rsi_tracks_recent_windows_not_the_series_start() {
        // Regression: the old implementation took the FIRST `period` windows,
        // freezing RSI at the value from the first days. A long flat start
        // followed by a slide must read as oversold, not 100.
        let mut series = vec![100.0; 30];
        series.extend((0..20).map(|i| 100.0 - i as f64));
        let r = rsi(&closes(&series), 14).unwrap();
        assert!(r <= 35.0, "expected oversold RSI, got {r}");
    }

    #[test]
    fn support_resistance_basic() {
        let highs = vec![10.0, 12.0, 11.0, 15.0];
        let lows = vec![8.0, 9.0, 7.0, 10.0];
        assert_eq!(support_resistance(&highs, &lows, 4), Some((7.0, 15.0)));
        assert_eq!(support_resistance(&highs, &lows, 10), None);
    }

    #[test]
    fn volatility_flat_is_zero() {
        assert_eq!(volatility(&closes(&[100.0; 20]), 14), Some(0.0));
    }

    #[test]
    fn chart_context_needs_14_samples() {
        let short: Vec<(i64, Option<f64>, Option<f64>, Option<f64>, f64)> = (0..10)
            .map(|i| (i, Some(100.0), Some(100.0), Some(100.0), 100.0))
            .collect();
        assert!(chart_context("BTC", &short).is_none());
    }

    #[test]
    fn chart_context_includes_indicators() {
        let ohlcv: Vec<(i64, Option<f64>, Option<f64>, Option<f64>, f64)> = (0..30)
            .map(|i| {
                let p = 100.0 + i as f64;
                (i, Some(p - 1.0), Some(p + 1.0), Some(p - 2.0), p)
            })
            .collect();
        let s = chart_context("BTC", &ohlcv).unwrap();
        assert!(s.contains("BTC:"));
        assert!(s.contains("RSI(14):"));
        assert!(s.contains("SMA20:"));
        assert!(s.contains("S/R(20):"));
    }
}
