//! Web-search layer for `/ask`: a cheap "watcher" planner decides whether fresh
//! web data is needed and composes keyword queries (never the user's verbatim
//! text), then search dispatch + page fetch + map-reduce summarization turn the
//! results into a dense factual digest. Every model call is bounded (10–25s).

/// Fetch the top search-result pages and have a cheap model distill them into
/// a dense digest focused on the user's question. Falls back to raw snippets if
/// page fetching or the summarizer fails (fail-safe — never blocks the answer).
pub(crate) async fn fetch_and_summarize(
    client: &reqwest::Client,
    gemini_key: &str,
    question: &str,
    results: &[crate::crypto::ai::search::SearchResult],
    progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
    local_only: bool,
) -> String {
    let snippets = crate::crypto::ai::search::format_search_results(results);

    if let Some(tx) = progress_tx {
        let _ = tx.send("\u{1f50d} Reading the found pages...".to_string());
    }
    let pages = fetch_top_pages(client, results, 5, local_only).await;
    if pages.is_empty() {
        return snippets;
    }
    let combined = pages
        .iter()
        .enumerate()
        .map(|(i, (u, t))| format!("--- Source {} ({}) ---\n{}", i + 1, u, t))
        .collect::<Vec<_>>()
        .join("\n\n");

    if let Some(tx) = progress_tx {
        let _ = tx.send("\u{1f9e0} Summarizing the pages...".to_string());
    }
    if std::env::var("CRYPTOGRAPH_DEBUG_NO_SUMMARIZE").unwrap_or_default() == "1" {
        return combined; // debug: show raw fetched pages instead of the digest
    }
    if let Some(digest) = summarize_pages(client, gemini_key, question, &combined).await {
        digest
    } else {
        // Summarizer failed — raw page text is still better than snippets alone.
        combined
    }
}

/// Fetch full text of the top result pages. Tavily Extract runs server-side
/// (their infra, not our rustls client) so Cloudflare-protected sites still
/// yield content; any url Tavily didn't return is tried with a direct fetch.
/// Best-effort: failures are simply skipped.
async fn fetch_top_pages(
    client: &reqwest::Client,
    results: &[crate::crypto::ai::search::SearchResult],
    limit: usize,
    local_only: bool,
) -> Vec<(String, String)> {
    let urls: Vec<String> = results.iter().take(limit).map(|r| r.url.clone()).collect();
    let mut pages: Vec<(String, String)> = Vec::new();
    // Tavily Extract costs API calls — skip it in local-only (test) mode.
    let tavily_key = if local_only {
        String::new()
    } else {
        std::env::var("TAVILY_API_KEY").unwrap_or_default()
    };
    if !tavily_key.is_empty() && !urls.is_empty() {
        pages =
            crate::crypto::ai::search::tavily::tavily_extract(client, &tavily_key, &urls, 8_000)
                .await;
    }
    let got: Vec<String> = pages.iter().map(|(u, _)| u.clone()).collect();
    for u in &urls {
        if got.contains(&u) {
            continue;
        }
        // Free server-side reader (works on Cloudflare sites), then direct fetch.
        if let Some(t) = crate::crypto::ai::search::jina_reader(client, u, 10_000).await {
            pages.push((u.clone(), t));
        } else if let Some(t) = crate::crypto::ai::search::fetch_page_text(client, u, 10_000).await
        {
            pages.push((u.clone(), t));
        }
    }
    pages
}

/// Map-reduce summarizer. If the combined page text is large, it is split into
/// chunks and each chunk is distilled by a cheap news-tier model IN PARALLEL
/// (map), then a final call merges the chunk digests into one dense summary
/// focused on the question (reduce). Fail-safe: a failed chunk falls back to
/// its raw text, a failed merge joins the chunk digests.
async fn summarize_pages(
    client: &reqwest::Client,
    gemini_key: &str,
    question: &str,
    pages: &str,
) -> Option<String> {
    let models = crate::crypto::ai::pools::get_available_models(Some("news"));
    let alias = models.into_iter().next()?;
    let chunk_size = 5_000;
    let total = pages.chars().count();
    if total <= chunk_size {
        return summarize_chunk(client, gemini_key, &alias, question, pages, 0).await;
    }

    // MAP: split into ~5k-char chunks, keep only chunks sharing keywords with
    // the question (nav menus, cookie notices, tickers have no overlap and are
    // dropped), capped at 6, then summarize each in parallel.
    let raw_chunks: Vec<String> = pages
        .char_indices()
        .step_by(chunk_size)
        .map(|(i, _)| pages.chars().skip(i).take(chunk_size).collect::<String>())
        .collect();
    let q_lower = question.to_lowercase();
    let q_words: std::collections::HashSet<&str> = q_lower.split_whitespace().collect();
    let mut scored: Vec<(usize, usize, String)> = raw_chunks
        .into_iter()
        .enumerate()
        .map(|(idx, c)| {
            let overlap = q_words
                .iter()
                .filter(|w| c.to_lowercase().contains(*w))
                .count();
            (overlap, idx, c)
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let overlapping = scored.iter().filter(|(o, _, _)| *o > 0).count();
    let chunks: Vec<String> = if overlapping >= 2 {
        scored
            .iter()
            .filter(|(o, _, _)| *o > 0)
            .take(6)
            .map(|(_, _, c)| c.clone())
            .collect()
    } else {
        scored.iter().take(6).map(|(_, _, c)| c.clone()).collect()
    };
    let map_futs: Vec<_> = chunks
        .iter()
        .enumerate()
        .map(|(idx, c)| summarize_chunk(client, gemini_key, &alias, question, c, idx))
        .collect();
    let mapped: Vec<Option<String>> = futures::future::join_all(map_futs).await;
    let mut digests: Vec<String> = Vec::new();
    for (idx, m) in mapped.into_iter().enumerate() {
        match m {
            Some(d) => digests.push(d),
            None => digests.push(chunks[idx].clone()), // fallback: raw chunk
        }
    }
    if digests.is_empty() {
        return None;
    }
    let joined = digests.join("\n\n");
    if digests.len() == 1 {
        return Some(joined);
    }

    // REDUCE: one final call merges the chunk digests into the clean summary.
    let reduce_prompt = format!(
        "You are a web research assistant for a crypto investor.\n\
         The user asked: \"{}\"\n\
         Below are digests of separate parts of the same research. Merge them into ONE dense, \
         non-repetitive factual summary — concrete names, prices, dates, specs, ratings, security \
         warnings — without filler. If digests contradict each other, say so explicitly.\n\n\
         {}",
        question, joined
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(25),
        crate::crypto::ai::providers::call_any_model(
            client,
            gemini_key,
            &alias,
            &reduce_prompt,
            None,
        ),
    )
    .await
    .ok()
    .and_then(|r| r.ok())
}

/// Single-chunk summarization with per-call timeout and NO_RELEVANT guard.
async fn summarize_chunk(
    client: &reqwest::Client,
    gemini_key: &str,
    alias: &str,
    question: &str,
    chunk: &str,
    chunk_idx: usize,
) -> Option<String> {
    let prompt = format!(
        "You are a web research assistant for a crypto investor.\n\
         The user asked: \"{}\"\n\
         This is part {} of the research, raw text extracted from web pages just now.\n\
         Extract ALL facts relevant to the question — concrete names, prices, dates, specs, ratings, \
         security warnings. Ignore navigation, ads, cookie notices and boilerplate.\n\
         Output a dense bullet-point digest, noting the source number if visible. \
         If nothing relevant, output exactly: NO_RELEVANT_CONTENT\n\n\
         {}",
        question, chunk_idx + 1, chunk
    );
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(25),
        crate::crypto::ai::providers::call_any_model(client, gemini_key, alias, &prompt, None),
    )
    .await
    .ok()?
    .ok()?;
    let trimmed = resp.trim();
    if trimmed.is_empty() || resp.to_lowercase().contains("no_relevant_content") {
        return None;
    }
    Some(resp)
}

/// Standalone CLI: `cargo run -- search "..."` — shows ONLY the search flow
/// (planner decision → query → snippets → fetched page text). No summarizer,
/// no generator/critic calls, fast and cheap.
pub async fn run_search_cli(query: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .use_rustls_tls()
        .build()?;
    let gemini_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();

    println!("\n== 1. PLANNER (cheap watcher model) ==");
    let planned = plan_web_search(
        &client,
        &gemini_key,
        query,
        &std::collections::HashSet::new(),
    )
    .await;
    let search_queries: Vec<String> = if !planned.is_empty() {
        println!("Decision: SEARCH -> {:?}", planned);
        planned
    } else {
        println!("Decision: NO_SEARCH (or all planner models failed)");
        return Ok(());
    };

    println!("\n== 2. SEARCH DISPATCH ==");
    let budget_mgr = crate::crypto::ai::search::BudgetManager::new(
        crate::crypto::ai::search::SearchBudget::new(),
    );
    let batches: Vec<Vec<crate::crypto::ai::search::SearchResult>> = futures::future::join_all(
        search_queries
            .iter()
            .map(|q| crate::crypto::ai::search::search_dispatch(&client, &budget_mgr, q, true)),
    )
    .await;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut results: Vec<crate::crypto::ai::search::SearchResult> = Vec::new();
    for batch in batches {
        for r in batch {
            if results.len() >= 8 {
                break;
            }
            if seen.insert(r.url.clone()) {
                results.push(r);
            }
        }
    }
    println!(
        "Queries: {:?} (LOCAL search) -> {} merged results\n",
        search_queries,
        results.len()
    );
    for (i, r) in results.iter().enumerate() {
        println!(
            "[{}] {}\n    URL: {}\n    SNIPPET: {}\n",
            i + 1,
            r.title,
            r.url,
            r.snippet
        );
    }

    println!("\n== 3. RAW PAGE FETCH (top 3, Tavily Extract + direct fallback) ==");
    let pages = fetch_top_pages(&client, &results, 5, true).await;
    for (i, (u, t)) in pages.iter().enumerate() {
        println!("--- PAGE {} ({}) ---\n{}\n", i + 1, u, t);
    }
    if pages.is_empty() {
        println!("ALL PAGE FETCHES FAILED — only snippets available.");
    }
    Ok(())
}

/// Cheap "watcher"-tier models decide whether the question needs fresh web
/// data and, if so, what to search for (up to 3 sub-topic queries). Fail-safe:
/// tries up to 2 different planner models; a definitive NO_SEARCH answers
/// immediately, garbage or errors just move to the next model, and if all fail
/// we return an empty vec so the caller falls back to a composed keyword query
/// (or proceeds without search).
pub(crate) async fn plan_web_search(
    client: &reqwest::Client,
    gemini_key: &str,
    question: &str,
    detected: &std::collections::HashSet<String>,
) -> Vec<String> {
    let models = crate::crypto::ai::pools::get_available_models(Some("watcher"));
    let coins = if detected.is_empty() {
        "none".to_string()
    } else {
        let mut v: Vec<&str> = detected.iter().map(|s| s.as_str()).collect();
        v.sort();
        v.join(", ")
    };
    let prompt = format!(
        "You are a web-search planner for a crypto assistant.\n\
         Today's date: {}.\n\
         Coins mentioned by the user (tickers): {}.\n\
         Decide whether answering this user question REQUIRES fresh, up-to-date information from \
         the web (current prices, breaking news, hardware-wallet / product comparisons, ratings, \
         reviews, regulations, recent events).\n\
         If it does, output EXACTLY one line: SEARCH: <one concise query>\n\
         If the question is complex and covers SEVERAL distinct topics, you MAY output up to \
         three SEARCH: lines, one per topic.\n\
         IMPORTANT: write the query in ENGLISH always, even if the user writes in Russian or \
         another language — English sources are the most trustworthy for crypto.\n\
         NEVER copy the user's question verbatim — reformulate it into a concise keyword query \
         (e.g. \"BTC price forecast 2026\", not the user's exact wording).\n\
         If the user mentions specific coins, include their tickers (BTC, ETH, SOL, LINK, ...) \
         in the query.\n\
         CRITICAL: include the current year ({} or the exact date) in the query when it is a \n\
         \"best/top/review\" type question, so the results are from the current year, not years ago.\n\
         If general knowledge or already-provided market data is enough, output exactly: NO_SEARCH\n\
         User question: \"{}\"",
        chrono::Utc::now().format("%Y-%m-%d"),
        coins,
        chrono::Utc::now().format("%Y"),
        question
    );
    for alias in models.into_iter().take(2) {
        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::crypto::ai::providers::call_any_model(client, gemini_key, &alias, &prompt, None),
        )
        .await;
        match resp {
            Ok(Ok(text)) => {
                let lower = text.to_lowercase();
                if lower.contains("no_search") {
                    // Definitive decision — the question can be answered from
                    // context/knowledge; don't burn more planner calls.
                    return vec![];
                }
                let queries = crate::crypto::ai::search::parse_search_requests(&text);
                if !queries.is_empty() {
                    return queries;
                }
                // Unusable output — try the next planner model.
            }
            _ => continue, // error or timeout — try the next planner model
        }
    }
    vec![]
}

/// Questions that need up-to-date information: product picks, comparisons,
/// news, prices, ratings, reviews. Portfolio-only questions (e.g. "my
/// balance") don't match and skip the web search.
fn needs_web_search(question_lower: &str) -> bool {
    const TRIGGERS: &[&str] = &[
        // ru
        "кошелек",
        "кошелька",
        "кошельке",
        "кошельки",
        "лучш",
        "топ",
        "топов",
        "сравн",
        "обзор",
        "рейтинг",
        "новост",
        "цена",
        "цене",
        "цену",
        "стоимост",
        "сегодня",
        "случил",
        "произошл",
        "стоит ли",
        "какой сейчас",
        "какая сейчас",
        "актуал",
        "свеж",
        "последн",
        "рекоменд",
        "прогноз",
        "ожид",
        "вероятн",
        // en
        "wallet",
        "best",
        "top ",
        "compare",
        "review",
        "rating",
        "news",
        "price",
        "today",
        "current",
        "latest",
        "happen",
        "recommend",
        "forecast",
        "should i",
        "what is",
    ];
    TRIGGERS.iter().any(|t| question_lower.contains(t))
}

/// Compose search queries from detected symbols + question topic when the
/// planner model is unavailable or echoed the raw user question. Never sends
/// the user's verbatim text to a search engine.
pub(crate) fn fallback_search_queries(
    question_lower: &str,
    detected: &std::collections::HashSet<String>,
) -> Vec<String> {
    if !needs_web_search(question_lower) {
        return vec![];
    }
    let syms = if detected.is_empty() {
        "crypto".to_string()
    } else {
        let mut v: Vec<&str> = detected.iter().map(|s| s.as_str()).collect();
        v.sort();
        v.join(" ")
    };
    let topic = if question_lower.contains("новост")
        || question_lower.contains("news")
        || question_lower.contains("случил")
        || question_lower.contains("произошл")
        || question_lower.contains("happen")
    {
        "news"
    } else if question_lower.contains("прогноз")
        || question_lower.contains("forecast")
        || question_lower.contains("ожид")
        || question_lower.contains("вероятн")
    {
        "price forecast"
    } else if question_lower.contains("кошелек")
        || question_lower.contains("wallet")
        || question_lower.contains("лучш")
        || question_lower.contains("best")
        || question_lower.contains("топ")
        || question_lower.contains("top")
        || question_lower.contains("сравн")
        || question_lower.contains("compare")
        || question_lower.contains("обзор")
        || question_lower.contains("review")
    {
        "best"
    } else {
        "price"
    };
    let year = chrono::Utc::now().format("%Y");
    vec![format!("{syms} {topic} {year}")]
}

/// True when a planner query is a useless echo of the user's question:
/// non-ASCII (Cyrillic) text, or a near-verbatim repeat of the question
/// itself. The planner must reformulate into a keyword query, not google the
/// question.
pub(crate) fn is_echo_query(query: &str, question: &str) -> bool {
    if !query.chars().all(|c| c.is_ascii() || c == ' ') {
        return true;
    }
    let norm = |s: &str| -> Vec<String> {
        s.to_lowercase()
            .split_whitespace()
            .map(|w| w.to_string())
            .collect()
    };
    let q = norm(query);
    let u = norm(question);
    if q.is_empty() {
        return true;
    }
    if q == u {
        return true;
    }
    // Near-verbatim: the query contains every word of the question (planner
    // just appended words), or the question contains every word of the query
    // with enough overlap (planner just dropped words). A real reformulation
    // (e.g. "best crypto to buy September 2026" vs the question "what is the
    // best crypto to buy in september") keeps enough new words to pass.
    let overlap = q.iter().filter(|w| u.contains(w)).count();
    let q_in_u = q.iter().all(|w| u.contains(w));
    let u_in_q = u.iter().all(|w| q.contains(w));
    q_in_u || (u_in_q && overlap >= 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(detected: &[&str]) -> std::collections::HashSet<String> {
        detected.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn fallback_uses_detected_symbols_and_topic() {
        let q = "ну что братан как там по крипте в сентябре, прогноз по битку";
        let queries = fallback_search_queries(q, &set(&["BTC"]));
        assert_eq!(queries.len(), 1);
        let year = chrono::Utc::now().format("%Y").to_string();
        assert_eq!(queries[0], format!("BTC price forecast {year}"));
    }

    #[test]
    fn fallback_never_echoes_raw_question() {
        let q = "какие сейчас новости по эфиру";
        let queries = fallback_search_queries(q, &set(&["ETH"]));
        assert_eq!(
            queries,
            vec![format!("ETH news {}", chrono::Utc::now().format("%Y"))]
        );
    }

    #[test]
    fn fallback_empty_when_no_search_needed() {
        let q = "покажи мой портфель";
        assert!(fallback_search_queries(q, &set(&[])).is_empty());
    }

    #[test]
    fn fallback_defaults_to_crypto_price() {
        let q = "какая сейчас цена на биток";
        let queries = fallback_search_queries(q, &set(&[]));
        assert_eq!(
            queries,
            vec![format!("crypto price {}", chrono::Utc::now().format("%Y"))]
        );
    }

    #[test]
    fn planner_echo_filter_drops_cyrillic() {
        // Mirrors the filter applied to plan_web_search output in
        // execute_ask_pipeline: non-ASCII (Cyrillic) queries are echoes.
        let question = "какие сейчас новости по эфиру";
        let planned = vec![
            "ну что братан как по крипте".to_string(),
            "BTC price forecast 2026".to_string(),
        ];
        let filtered: Vec<String> = planned
            .into_iter()
            .filter(|q| !is_echo_query(q, question))
            .collect();
        assert_eq!(filtered, vec!["BTC price forecast 2026".to_string()]);
    }

    #[test]
    fn planner_echo_filter_drops_verbatim_english_question() {
        // Even an English echo of the user's question is useless — the planner
        // must reformulate into a keyword query, not google the question.
        let question = "what is the best crypto to buy in september";
        let planned = vec![
            question.to_string(),
            "best crypto to buy September 2026".to_string(),
        ];
        let filtered: Vec<String> = planned
            .into_iter()
            .filter(|q| !is_echo_query(q, question))
            .collect();
        assert_eq!(
            filtered,
            vec!["best crypto to buy September 2026".to_string()]
        );
    }
}
