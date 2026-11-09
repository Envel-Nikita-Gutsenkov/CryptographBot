//! Deterministic strategy layer (Phase C).
//!
//! Classifies the market regime from fear/greed + recent price moves and emits
//! operating rules the model must follow. This is what stops the bot from
//! panicking at a spike, chasing a peak, or advising "buy now" in euphoria —
//! the exact failure modes the user reported from the August rally.

use crate::crypto::ai::scenario::ScenarioMarket;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    /// Fear dominates, prices are falling — dip-buying within budget is allowed.
    Accumulate,
    /// Greed dominates, prices are overheated — never chase, wait for pullback.
    Euphoria,
    /// Neutral conditions — steady DCA only.
    Hold,
}

/// One symbol's recent price action, enough for regime classification.
#[derive(Debug, Clone)]
pub struct QuoteSignal {
    pub symbol: String,
    pub price: f64,
    pub chg_24h: Option<f64>,
    pub chg_7d: Option<f64>,
}

/// Parse the numeric part of a fear&greed string ("34 (Fear)" → 34).
pub fn parse_fear_greed(s: &str) -> Option<f64> {
    let token = s
        .split(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .next()?
        .trim();
    if token.is_empty() {
        return None;
    }
    token.parse::<f64>().ok()
}

/// Deterministic regime classification. Fear&greed dominates when available;
/// price action is the fallback (and the tie-breaker inside greed zones).
pub fn classify(fear_greed: Option<f64>, signals: &[QuoteSignal]) -> Regime {
    let max_24h = signals
        .iter()
        .filter_map(|s| s.chg_24h)
        .fold(0.0_f64, f64::max);
    let min_24h = signals
        .iter()
        .filter_map(|s| s.chg_24h)
        .fold(0.0_f64, f64::min);
    // 7-day action catches slower, sustained moves that a single 24h tick
    // misses (e.g. a week-long grind up or a gradual bleed down).
    let max_7d = signals
        .iter()
        .filter_map(|s| s.chg_7d)
        .fold(0.0_f64, f64::max);
    let min_7d = signals
        .iter()
        .filter_map(|s| s.chg_7d)
        .fold(0.0_f64, f64::min);
    match fear_greed {
        Some(fg) if fg <= 30.0 => Regime::Accumulate,
        Some(fg) if fg >= 70.0 => {
            if max_24h >= 5.0 || max_7d >= 10.0 {
                Regime::Euphoria
            } else {
                Regime::Hold
            }
        }
        _ => {
            // No fear/greed signal (or neutral): fall back to price action.
            if min_24h <= -8.0 || min_7d <= -15.0 {
                Regime::Accumulate
            } else if max_24h >= 10.0 || max_7d >= 20.0 {
                Regime::Euphoria
            } else {
                Regime::Hold
            }
        }
    }
}

/// Full prompt block: regime label + operating rules the model must follow.
pub fn guidance(fear_greed: Option<f64>, signals: &[QuoteSignal]) -> String {
    let regime = classify(fear_greed, signals);
    let fg = fear_greed
        .map(|v| format!("{v:.0}"))
        .unwrap_or_else(|| "n/a".to_string());

    let regime_line = match regime {
        Regime::Accumulate => "ACCUMULATE — fear dominates, prices are falling",
        Regime::Euphoria => "EUPHORIA — greed dominates, prices are overheated",
        Regime::Hold => "HOLD — neutral conditions",
    };
    // Name the strongest 24h mover so the model anchors its advice to the
    // symbol that is actually driving the regime, not a generic "the market".
    let mover = signals
        .iter()
        .filter_map(|s| s.chg_24h.map(|c| (c, s)))
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(c, s)| {
            format!(
                " | biggest 24h mover: {} at {:.2} USD ({:+.1}%)",
                s.symbol, s.price, c
            )
        })
        .unwrap_or_default();
    let rules = match regime {
        Regime::Accumulate => {
            "Operating rules: buying the dip is allowed WITHIN the user's stated monthly budget; \
             prefer limit orders at support levels; keep a reserve for further drops; never panic-sell \
             a long-term position on a dip."
        }
        Regime::Euphoria => {
            "Operating rules: DO NOT chase prices at the peak. New buys are NOT recommended until a \
             pullback of at least 5-10%. If the user holds a large profit (>30% above avg entry), \
             discussing partial profit-taking is acceptable — but never advise selling at a loss."
        }
        Regime::Hold => {
            "Operating rules: continue DCA within the user's budget; only buy dips of 3-5%; \
             no aggressive moves in either direction."
        }
    };
    format!(
        "MARKET REGIME (deterministic strategy layer): {}{}\nFear & Greed: {}\n{}",
        regime_line, mover, fg, rules
    )
}

/// Strategy block for an active scenario (deterministic what-if runs).
pub fn guidance_from_scenario(sc: &ScenarioMarket) -> String {
    let fg = sc.fear_greed.as_deref().and_then(parse_fear_greed);
    let signals: Vec<QuoteSignal> = sc
        .quotes
        .iter()
        .map(|(sym, q)| QuoteSignal {
            symbol: sym.clone(),
            price: q.price,
            chg_24h: q.chg_24h,
            chg_7d: q.chg_7d,
        })
        .collect();
    guidance(fg, &signals)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sig(sym: &str, chg_24h: f64) -> QuoteSignal {
        QuoteSignal {
            symbol: sym.into(),
            price: 100.0,
            chg_24h: Some(chg_24h),
            chg_7d: None,
        }
    }

    #[test]
    fn parses_fear_greed_number() {
        assert_eq!(parse_fear_greed("34 (Fear)"), Some(34.0));
        assert_eq!(parse_fear_greed("72 (Greed)"), Some(72.0));
        assert_eq!(parse_fear_greed("Unavailable."), None);
        assert_eq!(parse_fear_greed(""), None);
    }

    #[test]
    fn extreme_fear_is_accumulate_even_with_flat_prices() {
        assert_eq!(classify(Some(15.0), &[sig("BTC", 0.0)]), Regime::Accumulate);
    }

    #[test]
    fn greed_with_pump_is_euphoria() {
        assert_eq!(
            classify(Some(72.0), &[sig("ETH", 18.0), sig("SOL", 11.0)]),
            Regime::Euphoria
        );
    }

    #[test]
    fn greed_without_pump_is_hold() {
        assert_eq!(classify(Some(72.0), &[sig("BTC", 1.0)]), Regime::Hold);
    }

    #[test]
    fn no_fear_greed_falls_back_to_price_action() {
        assert_eq!(classify(None, &[sig("BTC", -12.0)]), Regime::Accumulate);
        assert_eq!(classify(None, &[sig("BTC", 12.0)]), Regime::Euphoria);
        assert_eq!(classify(None, &[sig("BTC", 2.0)]), Regime::Hold);
    }

    #[test]
    fn guidance_mentions_regime_and_rules() {
        let g = guidance(Some(72.0), &[sig("ETH", 18.0)]);
        assert!(g.contains("EUPHORIA"));
        assert!(g.contains("DO NOT chase"));
        let g2 = guidance(Some(20.0), &[sig("ETH", -18.0)]);
        assert!(g2.contains("ACCUMULATE"));
        assert!(g2.contains("buying the dip is allowed"));
    }
}
