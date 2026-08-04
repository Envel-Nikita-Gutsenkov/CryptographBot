//! Price consensus: parallel fetch from all sources, outlier detection via MAD,
//! weighted average of surviving prices.

use super::providers::RawQuote;
use crate::models::CmcQuote;

/// Threshold: exclude a source if its price deviates > this % from the median.
const OUTLIER_THRESHOLD_PCT: f64 = 5.0;

#[derive(Debug)]
pub struct ConsensusResult {
    pub quote: CmcQuote,
    /// 0.0..1.0 — higher = more sources agreed
    pub confidence: f64,
    pub sources_used: Vec<&'static str>,
    /// (source_name, price, deviation_pct)
    pub outliers: Vec<(&'static str, f64, f64)>,
}

/// Build consensus CmcQuote from quotes produced by multiple providers for the same symbol.
/// Returns None only if all quotes have zero price.
pub fn build_consensus(quotes: Vec<RawQuote>) -> Option<ConsensusResult> {
    if quotes.is_empty() {
        return None;
    }
    if quotes.len() == 1 {
        let q = &quotes[0];
        if q.price_usd == 0.0 {
            return None;
        }
        let source = q.source;
        let mut canonical = q.clone().into_canonical();
        // Same ATH guard as the multi-source path: drop implausible peaks and
        // recompute the discount so a provider's bogus value never propagates.
        if let Some(a) = canonical.ath {
            if a > 0.0 && a <= canonical.price_usd * 100.0 {
                canonical.ath_change_percentage = Some((canonical.price_usd - a) / a * 100.0);
            } else {
                canonical.ath = None;
                canonical.ath_change_percentage = None;
            }
        }
        return Some(ConsensusResult {
            quote: canonical,
            confidence: 0.33,
            sources_used: vec![source],
            outliers: vec![],
        });
    }

    let prices: Vec<f64> = quotes
        .iter()
        .map(|q| q.price_usd)
        .filter(|&p| p > 0.0)
        .collect();
    if prices.is_empty() {
        return None;
    }

    let min_price = prices.iter().copied().fold(f64::INFINITY, f64::min);
    let max_price = prices.iter().copied().fold(0.0_f64, f64::max);
    let spread_pct = if min_price > 0.0 {
        (max_price - min_price) / min_price * 100.0
    } else {
        0.0
    };

    let median = median_f64(&prices);

    // Partition: valid vs outlier
    let mut valid_quotes: Vec<&RawQuote> = Vec::new();
    let mut outliers: Vec<(&'static str, f64, f64)> = Vec::new();

    for q in &quotes {
        if q.price_usd <= 0.0 {
            continue;
        }
        let dev = (q.price_usd - median).abs() / median * 100.0;
        if dev > OUTLIER_THRESHOLD_PCT {
            outliers.push((q.source, q.price_usd, dev));
        } else {
            valid_quotes.push(q);
        }
    }

    // If ALL are outliers (pathological), fall back to the median of originals
    let fallback_to_all = valid_quotes.is_empty();
    if fallback_to_all {
        valid_quotes = quotes.iter().collect();
        outliers.clear();
    }

    let consensus_price = if fallback_to_all {
        // Mean is skewed by whatever pushed everyone off the median (e.g. a
        // wrong-pair DEX quote); the median is the robust fallback.
        median
    } else {
        valid_quotes.iter().map(|q| q.price_usd).sum::<f64>() / valid_quotes.len() as f64
    };

    // Sanitize ATH values before picking the peak: a single provider can poison
    // the ATH with an implausible value (e.g. a bridged token sharing the same
    // symbol whose ATH is a data glitch). Same policy as the peak_2y milestone
    // cap in client.rs: anything above 100x the consensus price is garbage.
    // The discount is recomputed from the surviving peak so a provider's bogus
    // ath_change_percentage never propagates either. With 3+ sources the median
    // is used so one over-reporting provider can't inflate the peak.
    let consensus_ath = {
        let mut aths: Vec<f64> = valid_quotes
            .iter()
            .filter_map(|q| q.ath)
            .filter(|a| *a > 0.0 && *a <= consensus_price * 100.0)
            .collect();
        if aths.is_empty() {
            None
        } else if aths.len() >= 3 {
            aths.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let n = aths.len();
            Some(if n % 2 == 0 {
                (aths[n / 2 - 1] + aths[n / 2]) / 2.0
            } else {
                aths[n / 2]
            })
        } else {
            aths.into_iter().max_by(|a, b| a.partial_cmp(b).unwrap())
        }
    };
    let consensus_ath_change_pct = match (consensus_ath, consensus_price) {
        (Some(a), p) if a > 0.0 && p > 0.0 => Some((p - a) / a * 100.0),
        _ => None,
    };

    // Pick the "richest" valid quote as base for non-price fields
    let base = valid_quotes
        .iter()
        .max_by_key(|q| {
            let mut score = 0u8;
            if q.change_1h.is_some() {
                score += 1;
            }
            if q.change_7d.is_some() {
                score += 1;
            }
            if q.change_30d.is_some() {
                score += 1;
            }
            if q.change_90d.is_some() {
                score += 1;
            }
            score
        })
        .unwrap();

    // Consensus 24h change (average of valid sources that provide it)
    let change_24h_vals: Vec<f64> = valid_quotes.iter().filter_map(|q| q.change_24h).collect();
    let consensus_24h = if change_24h_vals.is_empty() {
        base.change_24h
    } else {
        Some(change_24h_vals.iter().sum::<f64>() / change_24h_vals.len() as f64)
    };

    let confidence = match valid_quotes.len() {
        3 => 1.0,
        2 => 0.67,
        1 => 0.33,
        _ => 0.1,
    };

    let sources_used: Vec<&'static str> = valid_quotes.iter().map(|q| q.source).collect();

    let mut quote = CmcQuote {
        symbol: base.symbol.clone(),
        name: base.name.clone(),
        price_usd: consensus_price,
        market_cap: valid_quotes
            .iter()
            .filter_map(|q| q.market_cap)
            .max_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap_or(0.0),
        volume_24h: valid_quotes
            .iter()
            .filter_map(|q| q.volume_24h)
            .fold(0.0_f64, f64::max),
        change_1h: valid_quotes.iter().filter_map(|q| q.change_1h).next(),
        change_24h: consensus_24h,
        change_7d: valid_quotes.iter().filter_map(|q| q.change_7d).next(),
        change_30d: valid_quotes.iter().filter_map(|q| q.change_30d).next(),
        change_90d: valid_quotes.iter().filter_map(|q| q.change_90d).next(),
        change_365d: valid_quotes.iter().filter_map(|q| q.change_365d).next(),
        change_730d: valid_quotes.iter().filter_map(|q| q.change_730d).next(),
        ath: consensus_ath,
        ath_change_percentage: consensus_ath_change_pct,
        high_24h: valid_quotes
            .iter()
            .filter_map(|q| q.high_24h)
            .max_by(|a, b| a.partial_cmp(b).unwrap()),
        low_24h: valid_quotes
            .iter()
            .filter_map(|q| q.low_24h)
            .min_by(|a, b| a.partial_cmp(b).unwrap()),
        rank: valid_quotes
            .iter()
            .filter_map(|q| q.rank)
            .min()
            .unwrap_or(0),
        circulating_supply: valid_quotes
            .iter()
            .filter_map(|q| q.circulating_supply)
            .max_by(|a, b| a.partial_cmp(b).unwrap())
            .unwrap_or(0.0),
        last_updated: chrono::Utc::now(),
        anomaly_warning: None,
    };

    if spread_pct > 5.0 {
        quote.anomaly_warning = Some(format!(
            "Extreme API price divergence detected: {:.1}% spread between providers.",
            spread_pct
        ));
    } else if quotes.len() >= 3 && (valid_quotes.len() as f64 / quotes.len() as f64) <= 0.5 {
        quote.anomaly_warning = Some(format!(
            "Multiple APIs returning bad data. Excluded {} outliers.",
            quotes.len() - valid_quotes.len()
        ));
    }

    Some(ConsensusResult {
        quote,
        confidence,
        sources_used,
        outliers,
    })
}

fn median_f64(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = sorted.len();
    if n % 2 == 0 {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    } else {
        sorted[n / 2]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(sym: &str, price: f64, src: &'static str) -> RawQuote {
        RawQuote {
            symbol: sym.into(),
            name: sym.into(),
            price_usd: price,
            market_cap: None,
            volume_24h: None,
            change_1h: None,
            change_24h: Some(-1.0),
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
            source: src,
        }
    }

    #[test]
    fn three_sources_agree() {
        let quotes = vec![
            raw("BTC", 65000.0, "a"),
            raw("BTC", 65100.0, "b"),
            raw("BTC", 64900.0, "c"),
        ];
        let r = build_consensus(quotes).unwrap();
        assert!((r.quote.price_usd - 65000.0).abs() < 100.0);
        assert_eq!(r.confidence, 1.0);
        assert!(r.outliers.is_empty());
    }

    #[test]
    fn outlier_excluded() {
        let quotes = vec![
            raw("BTC", 65000.0, "a"),
            raw("BTC", 65100.0, "b"),
            raw("BTC", 100000.0, "c"),
        ];
        let r = build_consensus(quotes).unwrap();
        assert!(r.quote.price_usd < 70000.0, "outlier should be excluded");
        assert_eq!(r.outliers.len(), 1);
        assert_eq!(r.outliers[0].0, "c");
    }

    #[test]
    fn single_source_low_confidence() {
        let quotes = vec![raw("ETH", 3000.0, "a")];
        let r = build_consensus(quotes).unwrap();
        assert_eq!(r.confidence, 0.33);
    }

    #[test]
    fn garbage_ath_poisoned_discount_sanitized() {
        // Regression: a provider with the same symbol (e.g. bridged SOL with a
        // glitched $13.4k ATH vs $73 price) used to win via max() and produce a
        // nonsensical -99% discount. The 100x cap must drop it and the discount
        // must be recomputed from the surviving real peak.
        let mut bad = raw("SOL", 73.21, "coingecko");
        bad.ath = Some(13458.71);
        bad.ath_change_percentage = Some(-99.46);
        let mut real = raw("SOL", 73.50, "binance");
        real.ath = Some(293.31);
        let mut ok = raw("SOL", 72.80, "bybit");
        ok.ath = Some(260.0);
        let r = build_consensus(vec![bad, real, ok]).unwrap();
        assert_eq!(r.quote.ath, Some(293.31));
        let exp = (r.quote.price_usd - 293.31) / 293.31 * 100.0;
        let got = r.quote.ath_change_percentage.unwrap();
        assert!(
            (got - exp).abs() < 0.01,
            "discount recomputed: {got} vs {exp}"
        );
    }

    #[test]
    fn ath_median_resists_one_inflated_provider() {
        // Two honest ATHs (~294) plus one glitch within the 100x cap (5000):
        // with 3 sources the median must win instead of max().
        let mut a = raw("SOL", 73.0, "a");
        a.ath = Some(293.31);
        let mut b = raw("SOL", 73.4, "b");
        b.ath = Some(294.16);
        let mut c = raw("SOL", 73.1, "c");
        c.ath = Some(5000.0); // passes the 100x cap, still bogus
        let r = build_consensus(vec![a, b, c]).unwrap();
        let ath = r.quote.ath.unwrap();
        assert!(
            (ath - 294.16).abs() < 1.0,
            "median should resist inflated ATH, got {ath}"
        );
    }

    #[test]
    fn all_outliers_fallback_uses_median_not_mean() {
        // Every source deviates >5% from the median (5050), so the pathological
        // fallback kicks in. It must return the median (5050), not the mean
        // (5025.25) which is skewed toward the outlier cluster.
        let quotes = vec![
            raw("BTC", 1.0, "a"),
            raw("BTC", 100.0, "b"),
            raw("BTC", 10000.0, "c"),
            raw("BTC", 10000.0, "d"),
        ];
        let r = build_consensus(quotes).unwrap();
        assert!(
            (r.quote.price_usd - 5050.0).abs() < 0.01,
            "fallback must use median, got {}",
            r.quote.price_usd
        );
    }
}
