//! News context for `/ask`: fresh cached summaries from the knowledge DB, with
//! an on-demand raw-news fallback through the MoE single-pass pipeline when the
//! cache is empty. Bounded: the on-demand summarizer has a 45s timeout per
//! symbol and only the first 2 symbols are fetched live.

use std::sync::Arc;

use crate::{crypto::CryptoClient, db::SqliteStore};

/// Build the news block for the prompt.
///
/// `symbols` are the symbols in scope (portfolio + detected), `portfolio_symbols`
/// are the user's holdings (used when the question names no symbol at all).
pub(crate) async fn build_news_context(
    question_lower: &str,
    symbols: &[String],
    portfolio_symbols: &[String],
    sqlite: &Arc<SqliteStore>,
    cmc: &Arc<CryptoClient>,
    market_data: &str,
    progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
) -> String {
    let mut news_symbols = Vec::new();
    for sym in symbols {
        let sym_lower = sym.to_lowercase();
        let name_mapping = [
            ("BTC", "bitcoin"),
            ("ETH", "ethereum"),
            ("SOL", "solana"),
            ("LINK", "chainlink"),
            ("ADA", "cardano"),
            ("XRP", "ripple"),
            ("DOT", "polkadot"),
            ("DOGE", "dogecoin"),
            ("TON", "toncoin"),
        ];
        let name = name_mapping
            .iter()
            .find(|(s, _)| s == sym)
            .map(|(_, n)| *n)
            .unwrap_or(&sym_lower);
        if question_lower.contains(&sym_lower) || question_lower.contains(name) {
            news_symbols.push((sym.to_string(), name.to_string()));
        }
    }

    let wants_news = question_lower.contains("news")
        || question_lower.contains("новости")
        || question_lower.contains("новости?");
    let fetch_count = if wants_news { 5 } else { 2 };

    if news_symbols.is_empty() && !portfolio_symbols.is_empty() {
        for sym in portfolio_symbols.iter().take(fetch_count) {
            news_symbols.push((sym.clone(), sym.clone()));
        }
    }

    // Scenario override: deterministic news block for tests. When active, we
    // skip the news DB reads and any live on-demand news fetch entirely.
    let news_context = if let Some(n) =
        crate::crypto::ai::scenario::active_scenario().and_then(|s| s.news_context())
    {
        n
    } else {
        let mut news_context = if !news_symbols.is_empty() {
            let mut news_parts = Vec::new();
            for (sym, _) in news_symbols.iter().take(fetch_count) {
                if let Ok(Some((news, ts))) = sqlite.get_news_summary_with_ts(sym).await {
                    if !news.contains("Failed to analyze recent news")
                        && !news.trim().is_empty()
                        && SqliteStore::news_summary_is_fresh(&ts, 24)
                    {
                        news_parts.push(format!("Latest verified news summary for {sym}:\n{news}"));
                    }
                }
            }
            news_parts.join("\n")
        } else {
            String::new()
        };

        if news_context.trim().is_empty() || wants_news {
            if let Ok(Some((news, ts))) = sqlite.get_news_summary_with_ts("general").await {
                if !news.contains("Failed to analyze recent news")
                    && !news.trim().is_empty()
                    && SqliteStore::news_summary_is_fresh(&ts, 24)
                {
                    news_context =
                        format!("{}\n\nRecent general crypto news:\n{news}", news_context);
                }
            }
        }

        // Fallback: If DB has no cached news summaries at all, fetch live raw news on-demand
        if news_context.trim().is_empty() {
            let fetch_list = if news_symbols.is_empty() {
                vec![("ALL".to_string(), "general".to_string())]
            } else {
                news_symbols.clone()
            };

            if let Some(tx) = progress_tx {
                let _ = tx
                    .send("⏳ Analyzing raw news data (MoE pipeline)... Please wait.".to_string());
            }

            let mut live_parts = Vec::new();
            for (sym, db_key) in fetch_list.iter().take(2) {
                if let Ok(articles) = cmc.get_news(sym).await {
                    if articles.is_empty() {
                        sqlite
                            .save_news_summary(db_key, "No recent news detected.")
                            .await;
                        continue;
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

                    match tokio::time::timeout(
                        std::time::Duration::from_secs(45),
                        crate::crypto::ai::pipeline::summarize_news_single_pass(
                            sym,
                            &news_text,
                            market_data,
                            None,
                            sqlite.clone(),
                        ),
                    )
                    .await
                    {
                        Ok(Ok(verified_summary)) => {
                            let cache_key = if sym == "ALL" { "general" } else { sym };
                            sqlite.save_news_summary(cache_key, &verified_summary).await;
                            live_parts.push(format!(
                                "Latest verified news summary for {db_key}:\n{verified_summary}"
                            ));
                        }
                        Ok(Err(e)) => {
                            sqlite
                                .save_news_summary(
                                    db_key,
                                    "No recent verified news summary available.",
                                )
                                .await;
                            tracing::error!(
                                "Single-pass pipeline failed on-demand for {}: {}",
                                sym,
                                e
                            );
                        }
                        Err(_) => {
                            sqlite
                                .save_news_summary(
                                    db_key,
                                    "No recent verified news summary available.",
                                )
                                .await;
                            tracing::error!(
                                "Single-pass pipeline timed out on-demand for {} after 45s",
                                sym
                            );
                        }
                    };
                } else {
                    sqlite
                        .save_news_summary(db_key, "No recent news available.")
                        .await;
                }
            }

            if !live_parts.is_empty() {
                news_context = live_parts.join("\n\n");
                if let Some(tx) = progress_tx {
                    let _ = tx.send("🧠 Formulating final response...".to_string());
                }
            } else {
                news_context = "No recent news available.".to_string();
            }
        }

        news_context
    };

    // Sanitize internal MoE critic planner notes (e.g., "*   *Macro/Price Action Section:* ...")
    news_context
        .lines()
        .filter(|line| {
            let t = line.trim();
            !(t.starts_with('*') && t.contains("Section:"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}
