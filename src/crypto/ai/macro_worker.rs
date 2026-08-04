use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{error, info, warn};

use crate::{
    crypto::CryptoClient,
    db::SqliteStore,
};

/// Runs a background worker that periodically fetches global macro history
/// and runs it through a lightweight AI agent to cache the current macro state.
pub async fn run_macro_worker(
    sqlite: Arc<SqliteStore>,
    crypto: Arc<CryptoClient>,
) {
    info!("Starting AI Macro Worker");

    loop {
        info!("Running Macro Analysis pipeline");

        // 1. Fetch Global Metrics
        let m_data = crypto.get_global_metrics().await;
        let global_str = m_data
        .map(|g| {
            let mut info = format!(
                "Global Market Context:\nTotal Market Cap: ${:.0}\n24h Volume: ${:.0}\nBTC Dom: {:.2}%\nETH Dom: {:.2}%",
                g.total_market_cap, g.total_volume_24h, g.btc_dominance, g.eth_dominance
            );
            if let Some(s) = g.stablecoin_mcap { info.push_str(&format!("\nStablecoin Mcap: ${:.0}", s)); }
            if let Some(d) = g.dex_volume_24h { info.push_str(&format!("\nDEX Volume 24h: ${:.0}", d)); }
            info
        })
        .unwrap_or_else(|_| "Global Market Context: Unavailable".into());

        // 2. Fetch 1-year history for BTC and ETH
        let mut history_context = String::new();
        for sym in ["BTC", "ETH"] {
            match crypto.get_history(sym, "365").await {
                Ok(hist) => {
                    if !hist.is_empty() {
                        let current = hist.last().unwrap().1;
                        let max = hist.iter().map(|(_, p)| *p).fold(0.0, f64::max);
                        let min = hist.iter().map(|(_, p)| *p).fold(f64::MAX, f64::min);
                        let ath_drop = if max > 0.0 { (current - max) / max * 100.0 } else { 0.0 };
                        
                        history_context.push_str(&format!(
                            "\n{} 1-Year Range: Min ${:.2} - Max ${:.2} (Current: ${:.2}, {:.2}% from peak)",
                            sym, min, max, current, ath_drop
                        ));
                    }
                }
                Err(e) => {
                    warn!("Failed to fetch 1-year history for {}: {}", sym, e);
                }
            }
        }

        // 3. Construct prompt
        let prompt_base = format!(
            "You are a macro-economic cryptocurrency analyst. \
            Based on the following 1-year price history and current global metrics, \
            write a concise 1-2 paragraph summary of the current market cycle stage (bull/bear/accumulation) and general sentiment.\n\
            CRITICAL: Do NOT include specific prices (e.g. $60,000) or specific percentage drops in your final output, as these will become stale quickly. Speak in relative terms only (e.g. 'nearing historical highs', 'deep discount', 'in a consolidation phase').\n\n\
            {}\n{}",
            global_str, history_context
        );

        // 4. Run through single-pass lightweight LLM (borrowing the summarize_news_single_pass logic)
        let verified_summary = match crate::crypto::ai::pipeline::summarize_news_single_pass("GLOBAL", "", &prompt_base, None, sqlite.clone()).await {
            Ok(s) => s,
            Err(e) => {
                error!("Macro pipeline failed: {}", e);
                "Macro analysis currently unavailable due to AI failure.".to_string()
            }
        };

        let clean_macro = verified_summary.replace('$', "");
        // 5. Save to DB
        sqlite.save_macro_summary("GLOBAL", &clean_macro).await;
        info!("Saved new macro summary to database.");

        // Adaptive sleep interval based on market volatility (checking major pairs BTC and ETH)
        let mut sleep_secs = 12 * 3600; // default: 12 hours
        if let Ok(quotes) = crypto.get_quotes(&["BTC", "ETH"]).await {
            let is_volatile = quotes.iter().any(|q| {
                q.change_1h.unwrap_or(0.0).abs() >= 2.0 || q.change_24h.unwrap_or(0.0).abs() >= 5.0
            });
            if is_volatile {
                sleep_secs = 4 * 3600; // 4 hours in volatile markets
                info!("Macro Worker: Volatile market detected! Sleep interval shortened to 4 hours.");
            }
        }
        sleep(Duration::from_secs(sleep_secs)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dotenvy::dotenv;

    #[tokio::test]
    async fn test_macro_worker_real() {
        dotenv().ok();
        let cmc_key = std::env::var("CMC_API_KEY").unwrap_or_default();
        let cg_key = std::env::var("COINGECKO_API_KEY").unwrap_or_default();
        let cc_key = std::env::var("COINCAP_API_KEY").unwrap_or_default();
        let cryptocompare_key = std::env::var("CRYPTOCOMPARE_API_KEY").unwrap_or_default();
        
        if cmc_key.is_empty() || cg_key.is_empty() {
            println!("Skipping test: Missing API keys (CMC/CoinGecko needed)");
            return;
        }
        if std::env::var("GEMINI_API_KEY").unwrap_or_default().is_empty() && std::env::var("GROQ_API_KEY").unwrap_or_default().is_empty() {
            println!("Skipping test: Missing API keys for AI Models");
            return;
        }

        println!("Initializing CryptoClient...");
        let crypto = Arc::new(CryptoClient::new(&cmc_key, &cg_key, &cc_key, &cryptocompare_key, None));

        println!("Fetching Global Metrics...");
        let m_data = crypto.get_global_metrics().await;
        let global_str = m_data.map(|m| format!(
            "Total Market Cap: ${:.0}\n24h Volume: ${:.0}\nBTC Dom: {:.2}%\nETH Dom: {:.2}%",
            m.total_market_cap, m.total_volume_24h, m.btc_dominance, m.eth_dominance
        )).unwrap_or_else(|_| "Global Market Context: Unavailable".into());
        println!("{}\n", global_str);

        println!("Fetching 1-year history for BTC and ETH...");
        let mut history_context = String::new();
        for sym in ["BTC", "ETH"] {
            match crypto.get_history(sym, "365").await {
                Ok(hist) => {
                    if !hist.is_empty() {
                        let current = hist.last().unwrap().1;
                        let max = hist.iter().map(|(_, p)| *p).fold(0.0, f64::max);
                        let min = hist.iter().map(|(_, p)| *p).fold(f64::MAX, f64::min);
                        let ath_drop = if max > 0.0 { (current - max) / max * 100.0 } else { 0.0 };
                        
                        history_context.push_str(&format!(
                            "{} 1-Year Range: Min ${:.2} - Max ${:.2} (Current: ${:.2}, {:.2}% from peak)\n",
                            sym, min, max, current, ath_drop
                        ));
                    }
                }
                Err(e) => println!("Failed to fetch history for {}: {}", sym, e),
            }
        }
        println!("{}\n", history_context);

        let prompt_base = format!(
            "You are a macro-economic cryptocurrency analyst. \
            Based on the following 1-year price history and current global metrics, \
            write a concise 1-2 paragraph summary of the current market cycle stage (bull/bear/accumulation), \
            proximity to ATH (All-Time Highs for the year), and general sentiment.\n\n\
            {}\n{}",
            global_str, history_context
        );

        println!("--- RUNNING AI MACRO PIPELINE ---");
        let test_db = std::sync::Arc::new(crate::db::sqlite::SqliteStore::new("test_macro.db").unwrap());
        let verified_summary = crate::crypto::ai::pipeline::summarize_news_single_pass("GLOBAL", "", &prompt_base, None, test_db).await.expect("AI pipeline failed");
        println!("\n=== FINAL MACRO SUMMARY ===\n{}\n=============================\n", verified_summary);
    }
}

