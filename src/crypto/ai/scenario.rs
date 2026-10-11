//! Deterministic scenario overrides for pipeline testing.
//!
//! A scenario file (JSON) can replace the live market data, news and macro
//! intel that the /ask and proactive pipelines feed to the models. This lets
//! us run hard "what-if" cases ("BTC crashed 20%", "everything pumped 30%",
//! "Fear & Greed = 85") against the REAL pipeline — same prompts, same model
//! chain, same action parsing — without touching live APIs.
//!
//! Activation: the scenario runner (`cargo run -- scenario <file.json>`)
//! calls [`ScenarioMarket::activate`]; the pipelines check [`active`] and, if
//! present, use the override instead of fetching live data.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

static ACTIVE: OnceLock<ScenarioMarket> = OnceLock::new();
static FROM_ENV: OnceLock<Option<ScenarioMarket>> = OnceLock::new();

/// Structured quote for one symbol. Fields mirror what the live pipelines
/// format into the prompt (price, changes, ATH distance).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScenarioQuote {
    pub price: f64,
    #[serde(default)]
    pub chg_1h: Option<f64>,
    #[serde(default)]
    pub chg_24h: Option<f64>,
    #[serde(default)]
    pub chg_7d: Option<f64>,
    #[serde(default)]
    pub chg_30d: Option<f64>,
    #[serde(default)]
    pub chg_90d: Option<f64>,
    #[serde(default)]
    pub chg_365d: Option<f64>,
    #[serde(default)]
    pub ath: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScenarioMarket {
    #[serde(default)]
    pub fear_greed: Option<String>,
    #[serde(default)]
    pub global: Option<String>,
    #[serde(default)]
    pub quotes: HashMap<String, ScenarioQuote>,
    #[serde(default)]
    pub news: Option<String>,
    #[serde(default)]
    pub intel: Option<String>,
}

impl ScenarioMarket {
    pub fn load(path: &str) -> Option<ScenarioMarket> {
        let contents = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&contents).ok()
    }

    pub fn load_from_env() -> Option<ScenarioMarket> {
        let path = std::env::var("CRYPTOGRAPH_SCENARIO").ok()?;
        Self::load(&path)
    }

    /// Activate this override for the whole process (scenario runner).
    pub fn activate(self) {
        let _ = ACTIVE.set(self);
    }

    fn has_market(&self) -> bool {
        !self.quotes.is_empty() || self.global.is_some() || self.fear_greed.is_some()
    }

    /// Full market snapshot for the proactive goal scan — same shape as
    /// `build_market_snapshot` (GLOBAL MARKET / Fear & Greed / per-symbol lines).
    pub fn market_snapshot(&self, symbols: &[String]) -> Option<String> {
        if !self.has_market() {
            return None;
        }
        let mut lines = Vec::new();
        let global = self.global.as_deref().unwrap_or(
            "Total Mcap: unavailable | 24h Vol: unavailable | BTC Dom: -- | ETH Dom: --",
        );
        lines.push(format!("GLOBAL MARKET:\n{}", global));
        lines.push(format!(
            "Fear & Greed: {}",
            self.fear_greed.as_deref().unwrap_or("Unavailable.")
        ));
        for sym in symbols {
            if let Some(q) = self.quotes.get(sym) {
                lines.push(format_quote_line(sym, q));
            }
        }
        // Phase C — deterministic strategy layer for the scenario regime.
        let strat = crate::crypto::ai::strategy::guidance_from_scenario(self);
        if !strat.is_empty() {
            lines.push(strat);
        }
        Some(lines.join("\n"))
    }

    /// Market data block for the /ask pipeline — same shape as the live
    /// `CURRENT MARKET DATA` section (PORTFOLIO / Quotes YAML / global).
    pub fn ask_market_data(&self, portfolio_context: &str) -> Option<String> {
        if !self.has_market() {
            return None;
        }
        let mut quotes_str = String::new();
        let mut syms: Vec<&String> = self.quotes.keys().collect();
        syms.sort();
        for sym in syms {
            let q = &self.quotes[sym];
            quotes_str.push_str(&format_quote_yaml(sym, q));
        }
        if quotes_str.is_empty() {
            quotes_str.push_str("- Symbol: none\n  Price_USD: 0.00\n");
        }
        Some(format!(
            "PORTFOLIO:\n{}\n\nCURRENT MARKET DATA (SCENARIO OVERRIDE):\nQuotes:\n{}\n\nGlobal Market Context:\nFear & Greed Index: {}\n{}",
            portfolio_context,
            quotes_str,
            self.fear_greed.as_deref().unwrap_or("Unavailable."),
            self.global.as_deref().unwrap_or("Unavailable.")
        ))
    }

    pub fn news_context(&self) -> Option<String> {
        self.news.clone()
    }

    pub fn intel_context(&self) -> Option<String> {
        self.intel.clone()
    }
}

/// The active override, if any: first an explicitly activated scenario
/// (`cargo run -- scenario`), then `CRYPTOGRAPH_SCENARIO=<file>` from the
/// environment (cached after the first read).
pub fn active_scenario() -> Option<&'static ScenarioMarket> {
    if let Some(s) = ACTIVE.get() {
        return Some(s);
    }
    FROM_ENV.get_or_init(ScenarioMarket::load_from_env).as_ref()
}

/// Per-symbol line in the proactive snapshot, e.g.
/// `- BTC: 60120.00 USD | 1h: -1.2% | 24h: -18.5% | 7d: -25.0% | peak: 126173.00 USD (-52.3% from peak)`
fn format_quote_line(sym: &str, q: &ScenarioQuote) -> String {
    let mut yaml = format!(
        "- {}: {:.2} USD | 1h: {:+.2}% | 24h: {:+.2}% | 7d: {:+.2}%",
        sym,
        q.price,
        q.chg_1h.unwrap_or(0.0),
        q.chg_24h.unwrap_or(0.0),
        q.chg_7d.unwrap_or(0.0)
    );
    if let Some(c30) = q.chg_30d {
        yaml.push_str(&format!(" | 30d: {:+.2}%", c30));
    }
    if let Some(c90) = q.chg_90d {
        yaml.push_str(&format!(" | 90d: {:+.2}%", c90));
    }
    if let Some(c365) = q.chg_365d {
        yaml.push_str(&format!(" | 365d: {:+.2}%", c365));
    }
    if let Some(ath) = q.ath {
        let disc = if ath > 0.0 {
            (q.price - ath) / ath * 100.0
        } else {
            0.0
        };
        yaml.push_str(&format!(
            " | peak: {:.2} USD ({:+.2}% from peak)",
            ath, disc
        ));
    }
    yaml
}

/// Per-symbol YAML block in the /ask market data, e.g.
/// `- Symbol: BTC\n  Price_USD: 60120.00\n  Chg_24h: -18.50%\n  High_1Y: 126173.00\n  Discount_From_Peak: -52.35%`
fn format_quote_yaml(sym: &str, q: &ScenarioQuote) -> String {
    let mut yaml = format!(
        "- Symbol: {}\n  Price_USD: {:.2}\n  Chg_1h: {:+.2}%\n  Chg_24h: {:+.2}%\n  Chg_7d: {:+.2}%",
        sym,
        q.price,
        q.chg_1h.unwrap_or(0.0),
        q.chg_24h.unwrap_or(0.0),
        q.chg_7d.unwrap_or(0.0)
    );
    if let Some(c30) = q.chg_30d {
        yaml.push_str(&format!("\n  Chg_30d: {:+.2}%", c30));
    }
    if let Some(c90) = q.chg_90d {
        yaml.push_str(&format!("\n  Chg_90d: {:+.2}%", c90));
    }
    if let Some(c365) = q.chg_365d {
        yaml.push_str(&format!("\n  Chg_365d: {:+.2}%", c365));
    }
    if let Some(ath) = q.ath {
        let disc = if ath > 0.0 {
            (q.price - ath) / ath * 100.0
        } else {
            0.0
        };
        yaml.push_str(&format!(
            "\n  High_1Y: {:.2}\n  Discount_From_Peak: {:+.2}%",
            ath, disc
        ));
    }
    yaml
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crash_market() -> ScenarioMarket {
        ScenarioMarket {
            fear_greed: Some("15 (Extreme Fear)".into()),
            global: Some("Total Mcap: 2.1T USD | 24h Vol: 180B USD | BTC Dom: 55.2%".into()),
            quotes: HashMap::from([
                (
                    "BTC".into(),
                    ScenarioQuote {
                        price: 60_120.0,
                        chg_24h: Some(-18.5),
                        chg_7d: Some(-25.0),
                        chg_30d: Some(-12.0),
                        ath: Some(126_173.0),
                        ..Default::default()
                    },
                ),
                (
                    "ETH".into(),
                    ScenarioQuote {
                        price: 1_650.0,
                        chg_24h: Some(-22.0),
                        chg_7d: Some(-28.0),
                        ath: Some(4_800.0),
                        ..Default::default()
                    },
                ),
            ]),
            news: Some("Liquidations cascade as leveraged longs unwind.".into()),
            intel: None,
        }
    }

    #[test]
    fn snapshot_contains_symbols_and_fear_greed() {
        let m = crash_market();
        let snap = m
            .market_snapshot(&["BTC".into(), "ETH".into(), "SOL".into()])
            .unwrap();
        assert!(snap.contains("GLOBAL MARKET:"));
        assert!(snap.contains("Fear & Greed: 15 (Extreme Fear)"));
        assert!(snap.contains("- BTC: 60120.00 USD"));
        assert!(snap.contains("- ETH: 1650.00 USD"));
        // SOL has no override quote — must not appear.
        assert!(!snap.contains("SOL:"));
        // ATH discount is computed.
        assert!(snap.contains("peak: 126173.00 USD (-52.35% from peak)"));
    }

    #[test]
    fn ask_market_data_has_yaml_shape() {
        let m = crash_market();
        let md = m.ask_market_data("Total invested: 1620.69 USD").unwrap();
        assert!(md.contains("CURRENT MARKET DATA (SCENARIO OVERRIDE)"));
        assert!(md.contains("- Symbol: BTC"));
        assert!(md.contains("Price_USD: 60120.00"));
        assert!(md.contains("Chg_24h: -18.50%"));
        assert!(md.contains("Discount_From_Peak: -52.35%"));
    }

    #[test]
    fn empty_market_returns_none() {
        let m = ScenarioMarket::default();
        assert!(m.market_snapshot(&["BTC".into()]).is_none());
        assert!(m.ask_market_data("x").is_none());
    }

    #[test]
    fn news_and_intel_overrides() {
        let m = crash_market();
        assert_eq!(
            m.news_context().as_deref(),
            Some("Liquidations cascade as leveraged longs unwind.")
        );
        assert!(m.intel_context().is_none());
    }
}
