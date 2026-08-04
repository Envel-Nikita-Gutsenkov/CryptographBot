use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{error, info};

use crate::{crypto::CryptoClient, db::SqliteStore};

/// Background worker that every 30 minutes fetches market intelligence data
/// (BTC network metrics, derivatives sentiment, stablecoin flows)
/// and caches an AI-generated summary in SQLite.
pub async fn run_market_intel_worker(sqlite: Arc<SqliteStore>, crypto: Arc<CryptoClient>) {
    info!("Starting Market Intelligence Worker (30-min interval)");

    loop {
        info!("Market Intel Worker: collecting data...");

        let (btc_network, onchain_metrics) = tokio::join!(
            crypto.get_btc_network_metrics(),
            crypto.get_onchain_metrics(&["BTC", "ETH", "SOL", "LINK"]),
        );

        // Format BTC network context
        let network_str = format_btc_network(&btc_network);

        // Format derivatives sentiment
        let derivatives_str = format_derivatives(&onchain_metrics);

        if !network_str.is_empty() {
            let prompt = format!(
                "You are a Bitcoin network health analyst. Based on the following real-time network metrics, \
                write a concise 2-3 sentence summary describing the current state of the Bitcoin network. \
                Focus on: network security (hashrate), transaction demand (mempool), \
                and miner fee pressure. Use relative terms, no raw numbers in final output. \
                CRITICAL: Today's date context matters — be accurate about current conditions.\n\n\
                BTC NETWORK DATA:\n{network_str}"
            );

            match crate::crypto::ai::pipeline::summarize_news_single_pass(
                "BTC_NETWORK",
                "",
                &prompt,
                None,
                sqlite.clone(),
            )
            .await
            {
                Ok(summary) => {
                    sqlite.save_macro_summary("BTC_NETWORK", &summary).await;
                    info!("Saved BTC network summary to cache.");
                }
                Err(e) => {
                    error!("Market intel worker BTC network AI failed: {}", e);
                    // Save raw data as fallback
                    sqlite.save_macro_summary("BTC_NETWORK", &network_str).await;
                }
            }
        }

        if !derivatives_str.is_empty() {
            let prompt = format!(
                "You are a crypto derivatives market analyst. Based on the following futures market data, \
                write a concise 2-3 sentence summary of current market sentiment and risk levels. \
                Describe: are traders leveraged long or short, is OI elevated (systemic liquidation risk), \
                and what does funding rate suggest about market bias. Use relative language only.\n\n\
                DERIVATIVES DATA:\n{derivatives_str}"
            );

            match crate::crypto::ai::pipeline::summarize_news_single_pass(
                "DERIVATIVES",
                "",
                &prompt,
                None,
                sqlite.clone(),
            )
            .await
            {
                Ok(summary) => {
                    sqlite.save_macro_summary("DERIVATIVES", &summary).await;
                    info!("Saved derivatives summary to cache.");
                }
                Err(e) => {
                    error!("Market intel worker derivatives AI failed: {}", e);
                    sqlite.save_macro_summary("DERIVATIVES", &derivatives_str).await;
                }
            }
        }

        // Adaptive sleep interval based on market volatility (checking major pairs BTC and ETH)
        let mut sleep_secs = 20 * 60; // default: 20 minutes
        if let Ok(quotes) = crypto.get_quotes(&["BTC", "ETH"]).await {
            let is_volatile = quotes.iter().any(|q| {
                q.change_1h.unwrap_or(0.0).abs() >= 2.0 || q.change_24h.unwrap_or(0.0).abs() >= 5.0
            });
            if is_volatile {
                sleep_secs = 10 * 60; // 10 minutes in volatile markets
                info!("Market Intel Worker: Volatile market detected! Sleep interval shortened to 10 minutes.");
            }
        }
        sleep(Duration::from_secs(sleep_secs)).await;
    }
}

fn format_btc_network(m: &crate::models::market_intel::BtcNetworkMetrics) -> String {
    if m.hashrate_eh == 0.0 && m.mempool_tx_count == 0 {
        return String::new();
    }
    let mut s = String::new();
    if m.hashrate_eh > 0.0 {
        s.push_str(&format!("Hashrate: {:.1} EH/s\n", m.hashrate_eh));
    }
    if m.difficulty > 0.0 {
        s.push_str(&format!("Difficulty: {:.2}T\n", m.difficulty / 1e12));
    }
    if m.mempool_tx_count > 0 {
        s.push_str(&format!(
            "Mempool: {} txs ({:.1} MB)\n",
            m.mempool_tx_count, m.mempool_vsize_mb
        ));
    }
    if m.fee_fastest_sat > 0 {
        s.push_str(&format!(
            "Fees: fastest={} sat/vB, 1h={} sat/vB\n",
            m.fee_fastest_sat, m.fee_hour_sat
        ));
    }
    if m.progress_to_retarget > 0.0 {
        s.push_str(&format!(
            "Difficulty Retarget: {:.1}% progress, est. change: {:+.2}%\n",
            m.progress_to_retarget, m.estimated_retarget_pct
        ));
    }
    if !m.top_pool_name.is_empty() {
        s.push_str(&format!(
            "Top Mining Pool (1w): {} ({:.1}% share)\n",
            m.top_pool_name, m.top_pool_share_pct
        ));
    }
    s
}

fn format_derivatives(metrics: &[crate::models::onchain::OnChainMetrics]) -> String {
    let mut parts = Vec::new();
    for m in metrics {
        let mut line = format!("{}:", m.symbol);
        if let Some(fr) = m.funding_rate {
            line.push_str(&format!(" Funding={:+.4}%", fr * 100.0));
        }
        if let Some(oi) = m.open_interest {
            line.push_str(&format!(" Binance_OI={:.0}", oi));
        }
        if let Some(bybit_oi) = m.bybit_oi {
            line.push_str(&format!(" Bybit_OI={:.0}", bybit_oi));
        }
        if let Some(ls) = m.long_short_ratio {
            let bias = if ls > 1.1 {
                "bullish"
            } else if ls < 0.9 {
                "bearish"
            } else {
                "neutral"
            };
            line.push_str(&format!(" L/S_Ratio={:.2} ({})", ls, bias));
        }
        parts.push(line);
    }
    parts.join("\n")
}
