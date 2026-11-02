use crate::models::onchain::OnChainMetrics;
use reqwest::Client;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::Duration;

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
    #[serde(rename = "fundingRate")]
    funding_rate: String,
}

#[derive(Deserialize)]
struct BinanceOiHistResp {
    #[serde(rename = "sumOpenInterest")]
    sum_open_interest: String,
    timestamp: i64,
}

#[derive(Deserialize)]
struct BinanceLongShortResp {
    #[serde(rename = "longShortRatio")]
    long_short_ratio: String,
}

#[derive(Deserialize)]
struct BinanceTakerResp {
    #[serde(rename = "buySellRatio")]
    buy_sell_ratio: String,
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
            http: Client::builder()
                .timeout(Duration::from_secs(10))
                .use_rustls_tls()
                .build()
                .unwrap(),
        }
    }

    pub async fn get_metrics(&self, symbols: &[&str]) -> Vec<OnChainMetrics> {
        if symbols.is_empty() {
            return Vec::new();
        }

        // Dedupe (watchlists overlap) and keep the original case for the output.
        let mut uniq: Vec<String> = Vec::new();
        for &s in symbols {
            let u = s.to_uppercase();
            if !uniq.iter().any(|e| e == &u) {
                uniq.push(u);
            }
        }

        // Every family of requests runs concurrently: the old code did five
        // sequential HTTP calls per symbol, which is why the caller had a 20s
        // timeout band-aid around this function.
        let (activity, derivatives, tvl) = tokio::join!(
            self.fetch_activity(&uniq),
            self.fetch_derivatives(&uniq),
            self.fetch_tvl(&uniq),
        );

        let mut out = Vec::with_capacity(uniq.len());
        for sym in &uniq {
            let mut m = OnChainMetrics::empty(sym);
            if let Some((acts, fees)) = activity.get(sym) {
                m.active_addresses_24h = *acts;
                m.fees_24h_native = *fees;
            }
            if let Some(d) = derivatives.get(sym) {
                m.funding_rate = d.funding_rate;
                m.funding_rate_avg_8h = d.funding_avg;
                m.open_interest = d.open_interest;
                m.oi_change_24h_pct = d.oi_change_24h;
                m.oi_change_7d_pct = d.oi_change_7d;
                m.long_short_ratio = d.long_short_ratio;
                m.global_long_short_ratio = d.global_long_short_ratio;
                m.taker_buy_sell_ratio = d.taker_buy_sell;
                m.bybit_oi = d.bybit_oi;
            }
            if let Some(t) = tvl.get(sym) {
                m.tvl = Some(*t);
            }
            out.push(m);
        }
        out
    }

    async fn fetch_activity(
        &self,
        symbols: &[String],
    ) -> HashMap<String, (Option<u64>, Option<f64>)> {
        let mut map = HashMap::new();
        if symbols.is_empty() {
            return map;
        }
        let assets_lower: Vec<String> = symbols.iter().map(|s| s.to_lowercase()).collect();
        let url = format!(
            "https://community-api.coinmetrics.io/v4/timeseries/asset-metrics?assets={}&metrics=AdrActCnt,FeeTotNtv&frequency=1d&limit_per_asset=1",
            assets_lower.join(",")
        );
        if let Ok(resp) = self.http.get(&url).send().await {
            if let Ok(body) = resp.json::<CoinMetricsResp>().await {
                for data in body.data {
                    let acts = data.adr_act_cnt.and_then(|v| v.parse().ok());
                    let fees = data.fee_tot_ntv.and_then(|v| v.parse().ok());
                    map.insert(data.asset.to_uppercase(), (acts, fees));
                }
            }
        }
        map
    }

    async fn fetch_derivatives(&self, symbols: &[String]) -> HashMap<String, DerivativesData> {
        let futures: Vec<_> = symbols
            .iter()
            .map(|s| self.fetch_derivatives_one(s))
            .collect();
        let results = futures::future::join_all(futures).await;
        results.into_iter().collect()
    }

    async fn fetch_derivatives_one(&self, symbol: &str) -> (String, DerivativesData) {
        let pair = format!("{}USDT", symbol);

        // All five calls run in parallel for this symbol.
        let fr_url = format!(
            "https://fapi.binance.com/fapi/v1/fundingRate?symbol={}&limit=3",
            pair
        );
        let oi_url = format!(
            "https://fapi.binance.com/futures/data/openInterestHist?symbol={}&period=1h&limit=168",
            pair
        );
        let ls_url = format!(
            "https://fapi.binance.com/futures/data/globalLongShortAccountRatio?symbol={}&period=1h&limit=1",
            pair
        );
        let taker_url = format!(
            "https://fapi.binance.com/futures/data/takerlongshortRatio?symbol={}&period=1h&limit=1",
            pair
        );
        let bybit_url = format!(
            "https://api.bybit.com/v5/market/open-interest?category=linear&symbol={}USDT&intervalTime=5min&limit=1",
            symbol
        );
        let (fr, oi_hist, ls_global, taker, bybit) = tokio::join!(
            self.get_json::<Vec<BinanceFundingResp>>(&fr_url),
            self.get_json::<Vec<BinanceOiHistResp>>(&oi_url),
            self.get_json::<Vec<BinanceLongShortResp>>(&ls_url),
            self.get_json::<Vec<BinanceTakerResp>>(&taker_url),
            self.get_json::<BybitOiResp>(&bybit_url),
        );

        let mut d = DerivativesData::default();

        if let Some(rows) = fr.as_ref() {
            let rates: Vec<f64> = rows
                .iter()
                .filter_map(|r| r.funding_rate.parse::<f64>().ok())
                .collect();
            d.funding_rate = rates.last().copied();
            if !rates.is_empty() {
                d.funding_avg = Some(rates.iter().sum::<f64>() / rates.len() as f64);
            }
        }

        if let Some(rows) = oi_hist.as_ref() {
            // Ascending by time. Latest is the instant OI, 24 rows back = 24h ago,
            // 168 rows back = 7d ago (1h buckets).
            if let Some(last) = rows.last() {
                d.open_interest = last.sum_open_interest.parse::<f64>().ok();
                let last_v = d.open_interest;
                let at = |back: usize| -> Option<f64> {
                    let idx = rows.len().checked_sub(back)?;
                    rows.get(idx)
                        .and_then(|r| r.sum_open_interest.parse::<f64>().ok())
                };
                if let (Some(now), Some(prev)) = (last_v, at(25)) {
                    if prev > 0.0 {
                        d.oi_change_24h = Some((now / prev - 1.0) * 100.0);
                    }
                }
                if let (Some(now), Some(prev)) = (last_v, at(169)) {
                    if prev > 0.0 {
                        d.oi_change_7d = Some((now / prev - 1.0) * 100.0);
                    }
                }
                let _ = last.timestamp;
            }
        }

        if let Some(rows) = ls_global.as_ref() {
            if let Some(first) = rows.first() {
                d.global_long_short_ratio = first.long_short_ratio.parse::<f64>().ok();
            }
        }

        if let Some(rows) = taker.as_ref() {
            if let Some(first) = rows.first() {
                d.taker_buy_sell = first.buy_sell_ratio.parse::<f64>().ok();
            }
        }

        if let Some(body) = bybit.as_ref() {
            if let Some(item) = body.result.list.first() {
                d.bybit_oi = item.open_interest.parse::<f64>().ok();
            }
        }

        (symbol.to_string(), d)
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(&self, url: &str) -> Option<T> {
        let resp = self.http.get(url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<T>().await.ok()
    }

    async fn fetch_tvl(&self, symbols: &[String]) -> HashMap<String, f64> {
        let mut map = HashMap::new();
        if let Ok(resp) = self.http.get("https://api.llama.fi/v2/chains").send().await {
            if let Ok(body) = resp.json::<Vec<serde_json::Value>>().await {
                for chain in body {
                    if let Some(token) = chain.get("tokenSymbol").and_then(|t| t.as_str()) {
                        let sym = token.to_uppercase();
                        if symbols.contains(&sym) {
                            if let Some(tvl) = chain.get("tvl").and_then(|t| t.as_f64()) {
                                map.insert(sym, tvl);
                            }
                        }
                    }
                }
            }
        }

        // Ondo Finance RWA TVL, assigned to the ONDO token.
        let has_ondo = symbols
            .iter()
            .any(|s| s == "ONDO" || s == "USDY" || s == "OUSG");
        if has_ondo {
            if let Ok(resp) = self
                .http
                .get("https://api.llama.fi/protocol/ondo-finance")
                .send()
                .await
            {
                if let Ok(body) = resp.json::<serde_json::Value>().await {
                    if let Some(tvl) = body
                        .get("tvl")
                        .and_then(|t| t.as_array())
                        .and_then(|a| a.last())
                        .and_then(|l| l.get("totalLiquidityUSD"))
                        .and_then(|l| l.as_f64())
                    {
                        map.insert("ONDO".to_string(), tvl);
                    }
                }
            }
        }
        map
    }
}

#[derive(Debug, Default)]
struct DerivativesData {
    funding_rate: Option<f64>,
    funding_avg: Option<f64>,
    open_interest: Option<f64>,
    oi_change_24h: Option<f64>,
    oi_change_7d: Option<f64>,
    long_short_ratio: Option<f64>,
    global_long_short_ratio: Option<f64>,
    taker_buy_sell: Option<f64>,
    bybit_oi: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_onchain_and_mempool() {
        let provider = OnChainProvider::new();
        let metrics = provider.get_metrics(&["BTC", "ETH"]).await;
        assert!(
            !metrics.is_empty(),
            "Should return metrics for requested symbols"
        );
        println!("OnChain metrics fetched successfully: {:?}", metrics.len());
        for m in &metrics {
            println!(
                "Symbol: {}, funding {:?}, OI 24h {:?}%, LS {:?}",
                m.symbol, m.funding_rate, m.oi_change_24h_pct, m.long_short_ratio
            );
        }
    }

    #[tokio::test]
    async fn dedupes_symbols() {
        let provider = OnChainProvider::new();
        let metrics = provider.get_metrics(&["BTC", "btc"]).await;
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].symbol, "BTC");
    }

    #[tokio::test]
    async fn empty_input_returns_nothing() {
        let provider = OnChainProvider::new();
        assert!(provider.get_metrics(&[]).await.is_empty());
    }
}
