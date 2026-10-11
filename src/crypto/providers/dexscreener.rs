use super::RawQuote;
use crate::error::Result;
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct DexScreenerProvider {
    http: Client,
    cache: Cache<String, RawQuote>,
}

impl DexScreenerProvider {
    pub const NAME: &'static str = "dexscreener";

    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                .build()
                .unwrap(),
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(60))
                .max_capacity(500)
                .build(),
        }
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<RawQuote>> {
        let mut out = Vec::new();

        for &sym in symbols {
            let sym_upper = sym.to_uppercase();

            if let Some(q) = self.cache.get(&sym_upper).await {
                out.push(q);
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

            // Find the pair with highest liquidity matching the symbol
            let mut best_pair: Option<DsPair> = None;
            let mut max_liq = 0.0;

            if let Some(pairs) = body.pairs {
                for pair in pairs {
                    if pair.base_token.symbol.eq_ignore_ascii_case(sym) {
                        let liq = pair.liquidity.as_ref().map(|l| l.usd).unwrap_or(0.0);
                        if liq > max_liq {
                            max_liq = liq;
                            best_pair = Some(pair);
                        }
                    }
                }
            }

            if let Some(pair) = best_pair {
                let price_usd = pair
                    .price_usd
                    .unwrap_or("0".to_string())
                    .parse::<f64>()
                    .unwrap_or(0.0);
                if price_usd > 0.0 {
                    let q = RawQuote {
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
                    };
                    self.cache.insert(sym_upper.clone(), q.clone()).await;
                    out.push(q);
                }
            }
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
