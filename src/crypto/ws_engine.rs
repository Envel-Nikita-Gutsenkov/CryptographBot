use futures_util::StreamExt;
use moka::future::Cache;
use serde::Deserialize;
use std::time::Duration;
use tokio::time::sleep;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};

use crate::models::CmcQuote;

const BINANCE_WS_URL: &str = "wss://stream.binance.com:9443/ws/!ticker@arr";

#[derive(Deserialize, Debug)]
struct WsTicker {
    #[serde(rename = "s")]
    symbol: String,
    #[serde(rename = "c")]
    last_price: String,
    #[serde(rename = "P")]
    price_change_percent: String,
    #[serde(rename = "q")]
    quote_volume: String,
}

pub async fn start_binance_ws(cache: Cache<String, CmcQuote>) {
    // Rolling per-symbol price samples (ms, price) kept for ~61 min so we can
    // recompute a real 1h change from WS ticks. Without this, change_1h stayed
    // frozen at whatever the HTTP quote said while the TTL kept being reset.
    let mut hist: std::collections::HashMap<String, std::collections::VecDeque<(i64, f64)>> =
        std::collections::HashMap::new();
    loop {
        info!("Connecting to Binance WebSocket...");
        match connect_async(BINANCE_WS_URL).await {
            Ok((ws_stream, _)) => {
                info!("Binance WebSocket connected");
                let (_, mut read) = ws_stream.split();

                while let Some(msg_result) = read.next().await {
                    match msg_result {
                        Ok(Message::Text(text)) => {
                            if let Ok(tickers) = serde_json::from_str::<Vec<WsTicker>>(&text) {
                                for t in tickers {
                                    if !t.symbol.ends_with("USDT") {
                                        continue;
                                    }
                                    let base_sym = &t.symbol[..t.symbol.len() - 4]; // remove USDT

                                    let price_usd = t.last_price.parse::<f64>().unwrap_or(0.0);
                                    let change_24h =
                                        t.price_change_percent.parse::<f64>().unwrap_or(0.0);
                                    let volume_24h = t.quote_volume.parse::<f64>().unwrap_or(0.0);

                                    if price_usd <= 0.0 {
                                        continue;
                                    }

                                    if let Some(mut q) = cache.get(base_sym).await {
                                        let now_ms = chrono::Utc::now().timestamp_millis();
                                        let entry = hist.entry(base_sym.to_string()).or_default();
                                        // Drop samples older than 61 minutes.
                                        while let Some(&(t, _)) = entry.front() {
                                            if now_ms - t > 61 * 60 * 1000 {
                                                entry.pop_front();
                                            } else {
                                                break;
                                            }
                                        }
                                        // Recompute 1h change from the oldest sample in the
                                        // window once it is at least ~50 min old; otherwise we
                                        // have no honest 1h baseline, so clear it instead of
                                        // serving a frozen value.
                                        q.change_1h = match entry.front() {
                                            Some(&(t0, p0))
                                                if now_ms - t0 >= 50 * 60 * 1000 && p0 > 0.0 =>
                                            {
                                                Some((price_usd / p0 - 1.0) * 100.0)
                                            }
                                            _ => None,
                                        };
                                        entry.push_back((now_ms, price_usd));
                                        q.price_usd = price_usd;
                                        q.change_24h = Some(change_24h);
                                        q.volume_24h = volume_24h;
                                        q.last_updated = chrono::Utc::now();
                                        if q.circulating_supply > 0.0 {
                                            q.market_cap = q.circulating_supply * price_usd;
                                        }

                                        cache.insert(base_sym.to_string(), q).await;
                                    }
                                }
                            }
                        }
                        Ok(Message::Close(_)) => {
                            warn!("Binance WebSocket closed normally");
                            break;
                        }
                        Err(e) => {
                            error!("Binance WebSocket error: {}", e);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                error!("Failed to connect to Binance WS: {}", e);
            }
        }

        warn!("Reconnecting to Binance WS in 5 seconds...");
        sleep(Duration::from_secs(5)).await;
    }
}
