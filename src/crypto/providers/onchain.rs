use std::time::Duration;
use reqwest::Client;
use serde::Deserialize;
use crate::models::onchain::OnChainMetrics;

pub struct OnChainProvider {
    http: Client,
}

#[derive(Deserialize)]
struct CoinMetricsResp {
    data: Vec<CoinMetricsData>,
}

#[derive(Deserialize)]
struct CoinMetricsData {
    asset: String,
    #[serde(rename = "AdrActCnt")]
    adr_act_cnt: Option<String>,
    #[serde(rename = "FeeTotNtv")]
    fee_tot_ntv: Option<String>,
}

#[derive(Deserialize)]
struct BinanceFundingResp {
    #[allow(dead_code)]
    symbol: String,
    #[serde(rename = "fundingRate")]
    funding_rate: String,
}

#[derive(Deserialize)]
struct BinanceOpenInterestResp {
    #[allow(dead_code)]
    symbol: String,
    #[serde(rename = "openInterest")]
    open_interest: String,
}

#[derive(Deserialize)]
struct BinanceLongShortResp {
    #[serde(rename = "longShortRatio")]
    long_short_ratio: String,
}

#[derive(Deserialize)]
struct BybitOiResp {
    result: BybitOiResult,
}

#[derive(Deserialize)]
struct BybitOiResult {
    list: Vec<BybitOiItem>,
}

#[derive(Deserialize)]
struct BybitOiItem {
    #[serde(rename = "openInterest")]
    open_interest: String,
}

impl OnChainProvider {
    #[allow(dead_code)]
    pub const NAME: &'static str = "onchain";

    pub fn new() -> Self {
        Self {
            http: Client::builder().timeout(Duration::from_secs(10)).use_rustls_tls().build().unwrap(),
        }
    }

    pub async fn get_metrics(&self, symbols: &[&str]) -> Vec<OnChainMetrics> {
        let mut results = Vec::new();
        if symbols.is_empty() {
            return results;
        }

        // 1. Fetch from CoinMetrics
        let assets_lower: Vec<String> = symbols.iter().map(|s| s.to_lowercase()).collect();
        let cm_url = format!(
            "https://community-api.coinmetrics.io/v4/timeseries/asset-metrics?assets={}&metrics=AdrActCnt,FeeTotNtv&frequency=1d&limit_per_asset=1",
            assets_lower.join(",")
        );

        let mut metrics_map = std::collections::HashMap::new();
        for &sym in symbols {
            metrics_map.insert(sym.to_uppercase(), OnChainMetrics::empty(sym));
        }

        if let Ok(resp) = self.http.get(&cm_url).send().await {
            if let Ok(body) = resp.json::<CoinMetricsResp>().await {
                for data in body.data {
                    let sym_upper = data.asset.to_uppercase();
                    if let Some(metric) = metrics_map.get_mut(&sym_upper) {
                        if let Some(act) = data.adr_act_cnt {
                            metric.active_addresses_24h = act.parse().ok();
                        }
                        if let Some(fee) = data.fee_tot_ntv {
                            metric.fees_24h_native = fee.parse().ok();
                        }
                    }
                }
            }
        }

        // 2. Fetch from Binance for Futures (Funding Rate, Open Interest)
        for &sym in symbols {
            let sym_upper = sym.to_uppercase();
            let pair = format!("{}USDT", sym_upper);
            
            // Funding Rate
            let fr_url = format!("https://fapi.binance.com/fapi/v1/fundingRate?symbol={}&limit=1", pair);
            if let Ok(resp) = self.http.get(&fr_url).send().await {
                if let Ok(body) = resp.json::<Vec<BinanceFundingResp>>().await {
                    if let Some(first) = body.first() {
                        if let Some(metric) = metrics_map.get_mut(&sym_upper) {
                            metric.funding_rate = first.funding_rate.parse().ok();
                        }
                    }
                }
            }
            
            // Open Interest
            let oi_url = format!("https://fapi.binance.com/fapi/v1/openInterest?symbol={}", pair);
            if let Ok(resp) = self.http.get(&oi_url).send().await {
                if let Ok(body) = resp.json::<BinanceOpenInterestResp>().await {
                    if let Some(metric) = metrics_map.get_mut(&sym_upper) {
                        metric.open_interest = body.open_interest.parse().ok();
                    }
                }
            }

            // Long/Short Account Ratio (top traders, 5m period)
            let ls_url = format!(
                "https://fapi.binance.com/futures/data/topLongShortAccountRatio?symbol={}&period=5m&limit=1",
                pair
            );
            if let Ok(resp) = self.http.get(&ls_url).send().await {
                if let Ok(body) = resp.json::<Vec<BinanceLongShortResp>>().await {
                    if let Some(first) = body.first() {
                        if let Some(metric) = metrics_map.get_mut(&sym_upper) {
                            metric.long_short_ratio = first.long_short_ratio.parse().ok();
                        }
                    }
                }
            }

            // Bybit Open Interest
            let bybit_oi_url = format!(
                "https://api.bybit.com/v5/market/open-interest?category=linear&symbol={}USDT&intervalTime=5min&limit=1",
                sym_upper
            );
            if let Ok(resp) = self.http.get(&bybit_oi_url).send().await {
                if let Ok(body) = resp.json::<BybitOiResp>().await {
                    if let Some(item) = body.result.list.first() {
                        if let Some(metric) = metrics_map.get_mut(&sym_upper) {
                            metric.bybit_oi = item.open_interest.parse().ok();
                        }
                    }
                }
            }
        }

        // 3. Fetch from DefiLlama for TVL (ETH, SOL, etc)
        if let Ok(resp) = self.http.get("https://api.llama.fi/v2/chains").send().await {
            if let Ok(body) = resp.json::<Vec<serde_json::Value>>().await {
                for chain in body {
                    if let Some(token) = chain.get("tokenSymbol").and_then(|t| t.as_str()) {
                        let sym_upper = token.to_uppercase();
                        if let Some(metric) = metrics_map.get_mut(&sym_upper) {
                            if let Some(tvl) = chain.get("tvl").and_then(|t| t.as_f64()) {
                                metric.tvl = Some(tvl);
                            }
                        }
                    }
                }
            }
        }

        // 4. Fetch Ondo Finance Protocol TVL (RWA specific)
        let has_ondo_products = symbols.iter().any(|s| {
            let u = s.to_uppercase();
            u == "ONDO" || u == "USDY" || u == "OUSG" || u.ends_with("ON")
        });

        if has_ondo_products {
            if let Ok(resp) = self.http.get("https://api.llama.fi/protocol/ondo-finance").send().await {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(tvl_array) = body.get("tvl").and_then(|t| t.as_array()) {
                        if let Some(last_tvl) = tvl_array.last() {
                            if let Some(liquidity) = last_tvl.get("totalLiquidityUSD").and_then(|l| l.as_f64()) {
                                // Assign the TVL to the primary ONDO token (or create if missing)
                                let metric = metrics_map.entry("ONDO".to_string()).or_insert_with(|| OnChainMetrics::empty("ONDO"));
                                metric.tvl = Some(liquidity);
                            }
                        }
                    }
                }
            }
        }

        for (_, metric) in metrics_map {
            results.push(metric);
        }

        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_onchain_and_mempool() {
        let provider = OnChainProvider::new();
        let metrics = provider.get_metrics(&["BTC", "ETH"]).await;
        assert!(!metrics.is_empty(), "Should return metrics for requested symbols");
        println!("OnChain metrics fetched successfully: {:?}", metrics.len());
        for m in &metrics {
            println!("Symbol: {}, LS Ratio: {:?}, Bybit OI: {:?}", m.symbol, m.long_short_ratio, m.bybit_oi);
        }
    }
}
