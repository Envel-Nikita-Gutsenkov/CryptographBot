use super::RawQuote;
use crate::error::Result;
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct DexScreenerProvider {
    http: Client,
    cache: Cache<String, Option<RawQuote>>,
}

/// Pairs quoted in anything but a USD stable are not comparable to the CEX
/// consensus and were the main source of the 20%+ deviations on BTC/ETH.
const USD_QUOTES: [&str; 8] = [
    "USD", "USDT", "USDC", "DAI", "FDUSD", "BUSD", "TUSD", "USDE",
];

/// Below this the pool price is set by a handful of trades and is useless as a
/// price feed. Filtering it out is what stops dexscreener from being flagged
/// UNHEALTHY for majors on every cycle.
const MIN_LIQUIDITY_USD: f64 = 250_000.0;

impl DexScreenerProvider {
    pub const NAME: &'static str = "dexscreener";

    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                .build()
                .unwrap(),
            // Negative results are cached too, so a symbol with no qualifying
            // pool is not re-searched on every consensus pass.
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(300))
                .max_capacity(500)
                .build(),
        }
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<RawQuote>> {
        let mut out = Vec::new();

        for &sym in symbols {
            let sym_upper = sym.to_uppercase();

            if let Some(cached) = self.cache.get(&sym_upper).await {
                out.extend(cached);
                continue;
            }

            let resp = self
                .http
                .get("https://api.dexscreener.com/latest/dex/search")
                .query(&[("q", sym)])
                .send()
                .await?;

            if resp.status() == 429 {
                continue; // Ignore rate limit for DexScreener, don't fail the whole consensus
            }

            let body: DsSearchResp = match resp.json().await {
                Ok(b) => b,
                Err(_) => continue,
            };

            // Find the deepest USD-quoted pair matching the symbol.
            let mut best_pair: Option<DsPair> = None;
            let mut max_liq = 0.0;

            if let Some(pairs) = body.pairs {
                for pair in pairs {
                    if !pair.base_token.symbol.eq_ignore_ascii_case(sym) {
                        continue;
                    }
                    let quoted_in_usd = pair
                        .quote_token
                        .as_ref()
                        .map(|q| USD_QUOTES.iter().any(|u| q.symbol.eq_ignore_ascii_case(u)))
                        .unwrap_or(false);
                    if !quoted_in_usd {
                        continue;
                    }
                    let liq = pair.liquidity.as_ref().map(|l| l.usd).unwrap_or(0.0);
                    if liq >= MIN_LIQUIDITY_USD && liq > max_liq {
                        max_liq = liq;
                        best_pair = Some(pair);
                    }
                }
            }

            let quote = best_pair.and_then(|pair| {
                let price_usd = pair
                    .price_usd
                    .unwrap_or("0".to_string())
                    .parse::<f64>()
                    .unwrap_or(0.0);
                if price_usd <= 0.0 {
                    return None;
                }
                Some(RawQuote {
                    symbol: pair.base_token.symbol.to_uppercase(),
                    name: pair.base_token.name,
                    price_usd,
                    market_cap: pair.market_cap.or(pair.fdv),
                    volume_24h: pair.volume.as_ref().map(|v| v.h24),
                    change_1h: pair.price_change.as_ref().and_then(|c| c.h1),
                    change_24h: pair.price_change.as_ref().and_then(|c| c.h24),
                    change_7d: None, // DexScreener doesn't provide 7d change
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
                    source: Self::NAME,
                })
            });

            if let Some(q) = &quote {
                out.push(q.clone());
            }
            self.cache.insert(sym_upper, quote).await;
        }

        Ok(out)
    }
}

#[derive(Deserialize)]
struct DsSearchResp {
    pairs: Option<Vec<DsPair>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DsPair {
    base_token: DsToken,
    quote_token: Option<DsQuoteToken>,
    price_usd: Option<String>,
    volume: Option<DsVolume>,
    price_change: Option<DsPriceChange>,
    liquidity: Option<DsLiquidity>,
    market_cap: Option<f64>,
    fdv: Option<f64>,
}

#[derive(Deserialize)]
struct DsToken {
    symbol: String,
    name: String,
}

/// Only the symbol matters for the quote side; keeping the struct minimal means
/// a missing/renamed `name` field can never fail the whole search response.
#[derive(Deserialize)]
struct DsQuoteToken {
    symbol: String,
}

#[derive(Deserialize)]
struct DsVolume {
    h24: f64,
}

#[derive(Deserialize)]
struct DsPriceChange {
    h1: Option<f64>,
    h24: Option<f64>,
}

#[derive(Deserialize)]
struct DsLiquidity {
    usd: f64,
}
