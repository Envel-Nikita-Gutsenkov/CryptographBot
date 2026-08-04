use std::time::Duration;
use reqwest::Client;
use serde::Deserialize;

use crate::error::{BotError, Result};

pub struct YFinanceProvider {
    http: Client,
}

#[derive(Debug, Clone)]
pub struct TradFiQuote {
    pub symbol: String,
    pub price: f64,
    pub change_1d: Option<f64>,
    pub change_7d: Option<f64>,
    pub change_30d: Option<f64>,
    pub change_90d: Option<f64>,
    pub change_1y: Option<f64>,
}

#[derive(Deserialize)]
struct YfChartResp {
    chart: YfChart,
}

#[derive(Deserialize)]
struct YfChart {
    result: Option<Vec<YfResult>>,
}

#[derive(Deserialize)]
struct YfResult {
    meta: YfMeta,
    indicators: YfIndicators,
}

#[derive(Deserialize)]
struct YfMeta {
    #[serde(rename = "regularMarketPrice")]
    regular_market_price: f64,
}

#[derive(Deserialize)]
struct YfIndicators {
    quote: Vec<YfQuote>,
}

#[derive(Deserialize)]
struct YfQuote {
    close: Vec<Option<f64>>,
}

impl YFinanceProvider {
    #[allow(dead_code)]
    pub const NAME: &'static str = "yfinance";

    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(5))
                .user_agent("Mozilla/5.0")
                .use_rustls_tls()
                .build()
                .unwrap(),
        }
    }

    pub async fn get_quote(&self, symbol: &str) -> Result<TradFiQuote> {
        let url = format!("https://query1.finance.yahoo.com/v8/finance/chart/{}?range=1y&interval=1d", symbol.to_uppercase());
        let resp = self.http.get(&url).send().await?;
        
        if resp.status() == 404 {
            return Err(BotError::SymbolNotFound(symbol.to_string()));
        }
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        let body_str = resp.text().await.map_err(|e| BotError::CmcApi(e.to_string()))?;
        let body: YfChartResp = serde_json::from_str(&body_str)
            .map_err(|e| BotError::CmcApi(format!("yf json parse error: {}", e)))?;
        
        let mut r = body.chart.result
            .and_then(|mut arr| arr.pop())
            .ok_or_else(|| BotError::SymbolNotFound(symbol.to_string()))?;

        let price = r.meta.regular_market_price;
        let mut quote = TradFiQuote {
            symbol: symbol.to_uppercase(),
            price,
            change_1d: None,
            change_7d: None,
            change_30d: None,
            change_90d: None,
            change_1y: None,
        };

        if let Some(q) = r.indicators.quote.pop() {
            let closes: Vec<f64> = q.close.into_iter().filter_map(|x| x).collect();
            let n = closes.len();
            if n > 0 {
                let current = closes[n - 1];
                let calc_pct = |old: f64| -> f64 { ((current - old) / old) * 100.0 };

                if n >= 2 { quote.change_1d = Some(calc_pct(closes[n - 2])); }
                if n >= 6 { quote.change_7d = Some(calc_pct(closes[n - 6])); } // ~5 trading days = 1 week
                if n >= 21 { quote.change_30d = Some(calc_pct(closes[n - 21])); } // ~21 trading days = 1 month
                if n >= 63 { quote.change_90d = Some(calc_pct(closes[n - 63])); } // ~63 trading days = 3 months
                if n >= 250 { quote.change_1y = Some(calc_pct(closes[0])); } // ~252 trading days = 1 year
            }
        }

        Ok(quote)
    }
}
