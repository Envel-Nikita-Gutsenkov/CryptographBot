use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, warn};

use crate::{crypto::CryptoClient, db::SqliteStore, db::Store};

/// Runs one full news cycle: fetch + summarize + persist for every watched
/// symbol, then the retention sweep. Returns the list of symbols processed.
pub async fn run_news_cycle(
    store: Arc<Store>,
    sqlite: Arc<SqliteStore>,
    crypto: Arc<CryptoClient>,
) -> Vec<String> {
    // Find all unique symbols currently watched by all users
    let mut symbols: Vec<String> = Vec::new();
    for user_id in store.all_user_ids() {
        let user_symbols = store.read(user_id, |p| {
            let mut syms: std::collections::HashSet<String> =
                p.holdings_by_symbol().into_keys().collect();
            syms.extend(p.settings.default_watchlist.clone());
            syms
        });
        for sym in user_symbols {
            let upper = sym.to_uppercase();
            if !symbols.contains(&upper) {
                symbols.push(upper);
            }
        }
    }

    // Always ensure GENERAL market news is processed
    if !symbols.contains(&"GENERAL".to_string()) {
        symbols.push("GENERAL".to_string());
    }

    info!(
        "News worker found {} active symbols to process",
        symbols.len()
    );

    for symbol_str in &symbols {
        let symbol = symbol_str;
        info!("Processing news pipeline for {}", symbol);

        // 1. Fetch raw news
        let articles = match crypto.get_news(&symbol).await {
            Ok(a) => a,
            Err(e) => {
                warn!("Failed to fetch news for {}: {}", symbol, e);
                continue;
            }
        };

        if articles.is_empty() {
            debug!("No news found for {}", symbol);
            continue;
        }

        // Persist raw articles for on-demand "extended results". The AI
        // prompt only ever sees the summarized form below.
        if let Err(e) = sqlite.save_news_articles(symbol, &articles).await {
            warn!("Failed to store raw news articles for {}: {}", symbol, e);
        }

        let mut news_text = String::new();
        let mut char_count = 0;
        let max_chars = 60_000;
        for (i, article) in articles.iter().enumerate() {
            let body_snippet = if article.body.len() > 800 {
                crate::format::truncate_chars(&article.body, 800)
            } else {
                article.body.clone()
            };
            let entry = format!("{}. {}\n{}\n\n", i + 1, article.title, body_snippet);
            if char_count + entry.len() > max_chars {
                break;
            }
            news_text.push_str(&entry);
            char_count += entry.len();
        }

        // Get previous summary from DB
        let previous_summary_opt = sqlite.get_news_summary(&symbol).await.unwrap_or(None);

        // Generate market data
        let m_data = crypto.get_global_metrics().await;
        let m_str = m_data.map(|m| format!("Global Market Context:\nFear & Greed Index: 50\nTotal Market Cap: {}\n24h Volume: {}\nBTC Dom: {}%\nETH Dom: {}%", m.total_market_cap, m.total_volume_24h, m.btc_dominance, m.eth_dominance)).unwrap_or_default();

        // Run Single Pass Pipeline
        let verified_summary = match crate::crypto::ai::pipeline::summarize_news_single_pass(
            symbol,
            &news_text,
            &m_str,
            previous_summary_opt.as_deref(),
            sqlite.clone(),
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                error!("Single-pass pipeline failed for {}: {}", symbol, e);
                // Fallback to previous summary if pipeline totally fails
                previous_summary_opt
                    .unwrap_or_else(|| "No recent verified news summary available.".to_string())
            }
        };

        // 4. Save to DB (current + history)
        sqlite.save_news_summary(&symbol, &verified_summary).await;
        sqlite
            .save_news_summary_history(&symbol, &verified_summary)
            .await;
        info!("Saved verified news summary for {}", symbol);

        // Sleep a bit between symbols to avoid rate limits
        sleep(Duration::from_secs(10)).await;
    }

    // Retention sweep once per full cycle: raw articles 7 days,
    // summary history 30 days. Keeps the DB small while preserving
    // enough history for "what changed over time" questions.
    if let Err(e) = sqlite.cleanup_news(7, 30).await {
        warn!("News retention sweep failed: {}", e);
    }

    symbols
}

/// Runs a background worker that periodically fetches news for all symbols
/// and runs them through a dual-agent pipeline (Summarizer -> Controller)
/// before saving the verified summary to the database.
pub async fn run_news_worker(
    store: Arc<Store>,
    sqlite: Arc<SqliteStore>,
    crypto: Arc<CryptoClient>,
) {
    info!("Starting AI News Worker (Dual-Agent Pipeline)");

    loop {
        let symbols = run_news_cycle(store.clone(), sqlite.clone(), crypto.clone()).await;

        // Adaptive sleep interval based on market volatility of watched assets
        let mut sleep_secs = 2 * 3600; // default: 2 hours
        let sym_refs: Vec<&str> = symbols
            .iter()
            .map(|s| s.as_str())
            .filter(|&s| s != "GENERAL")
            .collect();
        if !sym_refs.is_empty() {
            if let Ok(quotes) = crypto.get_quotes(&sym_refs).await {
                let is_volatile = quotes.iter().any(|q| {
                    q.change_1h.unwrap_or(0.0).abs() >= 2.0
                        || q.change_24h.unwrap_or(0.0).abs() >= 5.0
                });
                if is_volatile {
                    sleep_secs = 1 * 3600; // 1 hour
                    info!("AI News Worker: Volatile market detected! Sleep interval shortened to 1 hour.");
                }
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
    async fn test_moe_pipeline_real() {
        dotenv().ok();
        let api_key = std::env::var("CRYPTOCOMPARE_API_KEY").unwrap_or_default();
        if api_key.is_empty() {
            println!("Skipping test: no CRYPTOCOMPARE_API_KEY");
            return;
        }
        if std::env::var("GEMINI_API_KEY")
            .unwrap_or_default()
            .is_empty()
            || std::env::var("GROQ_API_KEY").unwrap_or_default().is_empty()
        {
            println!("Skipping test: Missing API keys for MoE (Gemini/Groq needed)");
            return;
        }

        // Fetch real news
        let crypto = CryptoClient::new("", "", "", &api_key, None);
        let articles = crypto.get_news("BTC").await.expect("Failed to get news");
        let mut news_text = String::new();
        for (i, article) in articles.iter().enumerate().take(5) {
            let body_snippet = if article.body.len() > 800 {
                crate::format::truncate_chars(&article.body, 800)
            } else {
                article.body.clone()
            };
            news_text.push_str(&format!(
                "{}. [{}] {}\n{}\n\n",
                i + 1,
                article.source,
                article.title,
                body_snippet
            ));
        }

        let prompt_base = format!(
            "You are an expert cryptocurrency analyst analyzing news for BTC.\n\
            Extract the most important facts and sentiment. Pay special attention to any regulatory news, partnerships, hacks, or massive inflows/outflows.\n\
            News Data:\n{}", news_text
        );

        println!("--- RUNNING MOE PIPELINE FOR BTC ---");
        let test_db =
            std::sync::Arc::new(crate::db::sqlite::SqliteStore::new("test_news.db").unwrap());
        let final_summary: String = crate::crypto::ai::pipeline::generate_moe_pipeline(
            &prompt_base,
            Some("No previous summary. Everything is fine."),
            test_db,
        )
        .await
        .expect("MoE failed");

        println!(
            "\n=== JUDGE'S FINAL VERDICT ===\n{}\n=============================\n",
            final_summary
        );
    }
}
