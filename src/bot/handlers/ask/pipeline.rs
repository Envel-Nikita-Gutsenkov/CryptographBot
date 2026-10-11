//! `execute_ask_pipeline` — the /ask orchestration: detect symbols, build the
//! portfolio + market + news + memory context, run the web-search planner,
//! call the generator/critic, and persist the Q/A pair for multi-turn dialogue.
//!
//! Heavy lifting lives in the sibling modules (`market`, `news`, `search`,
//! `reflect`) so each file stays under 600 lines. Every stage is bounded by a
//! timeout; the whole pipeline is additionally wrapped in a 180s cap by the
//! entry points in `mod.rs`.

use std::sync::Arc;

use crate::{
    config::Config,
    crypto::{ai::prompts::build_ask_prompt, CryptoClient},
    db::Store,
};

use super::{
    format_ask_history,
    market::build_market_data,
    news::build_news_context,
    reflect::{needs_reflection, reflect_and_refine},
    search::{fallback_search_queries, fetch_and_summarize, is_echo_query, plan_web_search},
};

pub async fn execute_ask_pipeline(
    question: &str,
    user_id: u64,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    sqlite: Arc<crate::db::SqliteStore>,
    progress_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    reply_context: Option<String>,
) -> anyhow::Result<String> {
    // Extract mentioned symbols from the question
    let name_to_sym = [
        ("bitcoin", "BTC"),
        ("ethereum", "ETH"),
        ("ether", "ETH"),
        ("solana", "SOL"),
        ("sol", "SOL"),
        ("chainlink", "LINK"),
        ("link", "LINK"),
        ("cardano", "ADA"),
        ("ada", "ADA"),
        ("ripple", "XRP"),
        ("xrp", "XRP"),
        ("polkadot", "DOT"),
        ("dot", "DOT"),
        ("dogecoin", "DOGE"),
        ("doge", "DOGE"),
        ("shiba", "SHIB"),
        ("shib", "SHIB"),
        ("avalanche", "AVAX"),
        ("avax", "AVAX"),
        ("polygon", "MATIC"),
        ("matic", "MATIC"),
        ("toncoin", "TON"),
        ("ton", "TON"),
        ("tron", "TRX"),
        ("trx", "TRX"),
        ("litecoin", "LTC"),
        ("ltc", "LTC"),
        ("near", "NEAR"),
        ("uniswap", "UNI"),
        ("uni", "UNI"),
        ("cosmos", "ATOM"),
        ("atom", "ATOM"),
        ("stellar", "XLM"),
        ("xlm", "XLM"),
        ("algorand", "ALGO"),
        ("algo", "ALGO"),
        ("fantom", "FTM"),
        ("ftm", "FTM"),
        ("optimism", "OP"),
        ("op", "OP"),
        ("arbitrum", "ARB"),
        ("arb", "ARB"),
        ("sui", "SUI"),
        ("aptos", "APT"),
        ("apt", "APT"),
    ];

    let question_lower = question.to_lowercase();
    let mut detected = std::collections::HashSet::new();
    let words: Vec<String> = question
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    let is_sym = |word: &str| {
        word.len() >= 2
            && word.len() <= 10
            && word
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
    };

    let is_ondo = |word: &str| {
        if word.len() >= 5 && word.len() <= 7 && word.ends_with("on") {
            let base = &word[..word.len() - 2];
            base.chars().all(|c| c.is_ascii_uppercase())
        } else {
            false
        }
    };

    for word in &words {
        let word_lower = word.to_lowercase();
        for &(name, sym) in &name_to_sym {
            if word_lower == name || word_lower == sym.to_lowercase() {
                detected.insert(sym.to_string());
            }
        }
        if is_sym(word) || is_ondo(word) {
            detected.insert(word.to_uppercase());
        }
    }

    // Build portfolio context (detailed for /ask)
    let portfolio_context = store.read(user_id, |p| {
        if p.holdings.is_empty() && p.purchases.is_empty() {
            return "Portfolio is empty.".to_string();
        }
        let holdings_by_sym = p.holdings_by_symbol();
        let mut lines = vec![format!("Total invested: ${:.2}", p.total_invested_usd())];
        for (sym, amt) in &holdings_by_sym {
            let avg = p
                .avg_buy_price(sym)
                .map(|a| format!("avg buy: ${:.2}", a))
                .unwrap_or_default();
            lines.push(format!("• {sym}: {amt:.6} ({avg})"));
        }
        lines.join("\n")
    });

    // Fetch live market data for portfolio symbols + detected symbols
    let portfolio_symbols: Vec<String> =
        store.read(user_id, |p| p.holdings_by_symbol().into_keys().collect());
    let mut symbols = portfolio_symbols.clone();
    for sym in detected.clone() {
        if !symbols.contains(&sym) {
            symbols.push(sym);
        }
    }
    for sym in [
        "BTC".to_string(),
        "ETH".to_string(),
        "SOL".to_string(),
        "LINK".to_string(),
    ] {
        if !symbols.contains(&sym) {
            symbols.push(sym);
        }
    }

    // Live market data + local memory blocks + deterministic strategy layer.
    // All network fetches and sync DB opens inside are hard-bounded, so this
    // can degrade to a thin block but never hang the pipeline.
    let market_data = build_market_data(
        question,
        user_id,
        &store,
        &cmc,
        &sqlite,
        &symbols,
        &portfolio_context,
        progress_tx.as_ref(),
    )
    .await;

    // News context: cached summaries first, on-demand MoE fallback.
    let news_context = build_news_context(
        &question_lower,
        &symbols,
        &portfolio_symbols,
        &sqlite,
        &cmc,
        &market_data,
        progress_tx.as_ref(),
    )
    .await;

    let current_date = chrono::Utc::now().format("%Y-%m-%d").to_string();

    // Load cached market intelligence from SQLite — TTL: 1h (refreshed every 30min by worker)
    let mut market_intel_parts: Vec<String> = Vec::new();
    if let Ok(Some(btc_net)) = sqlite.get_macro_summary("BTC_NETWORK", 1).await {
        market_intel_parts.push(format!("BTC Network Health:\n{btc_net}"));
    }
    if let Ok(Some(deriv)) = sqlite.get_macro_summary("DERIVATIVES", 1).await {
        market_intel_parts.push(format!("Derivatives Sentiment:\n{deriv}"));
    }
    if let Ok(Some(global)) = sqlite.get_macro_summary("GLOBAL", 25).await {
        market_intel_parts.push(format!("Macro Cycle Context:\n{global}"));
    }
    let market_intel_context = market_intel_parts.join("\n\n");

    // Scenario override: deterministic macro/derivatives intel for tests.
    let market_intel_context = if let Some(i) =
        crate::crypto::ai::scenario::active_scenario().and_then(|s| s.intel_context())
    {
        i
    } else {
        market_intel_context
    };

    // AI memory: the user's standing notes and active goals — the model must
    // always respect them when answering (e.g. fixed investment size, coins on
    // hold). Phase E: explicitly separate the user's OWN decisions from the
    // AI's past recommendations so the model never claims the user decided
    // something it merely suggested (the "вы решили не докупать BTC" bug).
    let memory_context = store.read(user_id, |p| {
        let memos = p
            .memos
            .iter()
            .map(|m| format!("- USER INSTRUCTION (binding): {}", m.text))
            .collect::<Vec<_>>()
            .join("\n");
        let goals = p
            .goals
            .iter()
            .filter(|g| g.active && !g.done)
            .map(|g| {
                format!(
                    "- ACTIVE GOAL #{}: {} (set {})",
                    g.id,
                    g.text,
                    g.created_at.format("%Y-%m-%d")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut parts = Vec::new();
        if !memos.is_empty() {
            parts.push(format!(
                "USER'S OWN DECISIONS AND INSTRUCTIONS (binding — always follow them):\n{}",
                memos
            ));
        }
        if !goals.is_empty() {
            parts.push(format!("USER'S ACTIVE GOALS:\n{}", goals));
        }
        parts.push(
            "IMPORTANT: AI's own past recommendations are NOT user decisions. Only the memos and \
             goals above reflect the user's actual choices. Never claim the user decided something \
             unless it is written here."
                .to_string(),
        );
        parts.join("\n\n")
    });
    let preferred_lang = store.read(user_id, |p| p.settings.preferred_language.clone());

    // Cheap planner model decides IF a web search is needed and WHAT to
    // search. Only then do we pay for the real search and pass the results to
    // the big model — and only if something was actually found.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .use_rustls_tls()
        .build()?;
    let mut web_search_context = String::new();
    // Prod /ask keeps the full provider chain (Tavily -> Exa -> Local); set
    // CRYPTOGRAPH_LOCAL_SEARCH=1 to force local-only (used for tests).
    let local_only = std::env::var("CRYPTOGRAPH_LOCAL_SEARCH").unwrap_or_default() == "1";
    // Scenario runs are deterministic: skip the web-search planner and live
    // search dispatch entirely (the scenario JSON already carries news/intel).
    if crate::crypto::ai::scenario::active_scenario().is_none() {
        let planned = plan_web_search(&client, &config.gemini_api_key, question, &detected).await;
        // Sanity-filter planner output: weak planner models sometimes echo the
        // user's question verbatim instead of composing an English query.
        // Non-ASCII (Cyrillic) text and near-verbatim repeats are echoes — drop
        // them.
        let planned: Vec<String> = planned
            .into_iter()
            .filter(|q| !is_echo_query(q, question))
            .collect();
        let search_queries: Vec<String> = if !planned.is_empty() {
            planned
        } else {
            // Planner unavailable/failed/echoed — compose a keyword query from the
            // detected symbols + topic instead of searching the raw user text.
            fallback_search_queries(&question_lower, &detected)
        };
        if !search_queries.is_empty() {
            if let Some(tx) = progress_tx.as_ref() {
                let _ = tx.send(format!(
                    "\u{1f310} Searching: {}",
                    search_queries.join(" | ")
                ));
            }
            let budget_mgr = crate::crypto::ai::search::BudgetManager::new(
                crate::crypto::ai::search::SearchBudget::new(),
            );
            // Complex questions may spawn several sub-topic queries — search them
            // all in parallel and merge (dedupe by url, cap at 8 results).
            let batches: Vec<Vec<crate::crypto::ai::search::SearchResult>> =
                futures::future::join_all(search_queries.iter().map(|q| {
                    crate::crypto::ai::search::search_dispatch(&client, &budget_mgr, q, local_only)
                }))
                .await;
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut merged: Vec<crate::crypto::ai::search::SearchResult> = Vec::new();
            for batch in batches {
                for r in batch {
                    if merged.len() >= 8 {
                        break;
                    }
                    if seen.insert(r.url.clone()) {
                        merged.push(r);
                    }
                }
            }
            if !merged.is_empty() {
                tracing::info!(
                    "Auto web search for ask: {} queries -> {} merged results",
                    search_queries.len(),
                    merged.len()
                );
                // Deep dive: snippets alone are too thin, so fetch the top pages
                // and have a cheap summarizer distill them into a factual digest.
                // Falls back to plain snippets if fetching/summarizing fails.
                web_search_context = fetch_and_summarize(
                    &client,
                    &config.gemini_api_key,
                    question,
                    &merged,
                    progress_tx.as_ref(),
                    local_only,
                )
                .await;
            }
        }
    }

    let ask_history = sqlite.load_ask_history(user_id).await.unwrap_or_default();
    let ask_history_str = format_ask_history(&ask_history);

    let prompt = build_ask_prompt(
        &question,
        &portfolio_context,
        &market_data,
        &market_intel_context,
        &news_context,
        &web_search_context,
        &current_date,
        reply_context.as_deref(),
        (!ask_history_str.is_empty()).then_some(ask_history_str.as_str()),
        &memory_context,
        &preferred_lang,
    );

    let preferred_model = store.read(user_id, |p| p.settings.ai_model_override.clone());

    if let Some(tx) = progress_tx.as_ref() {
        let _ = tx.send("🧠 AI is analyzing the market data...".to_string());
    }

    let mut final_answer = match tokio::time::timeout(
        std::time::Duration::from_secs(150),
        crate::crypto::ai::pipeline::call_with_critic(
            &client,
            &config.gemini_api_key,
            &prompt,
            preferred_model.as_deref(),
            false, // is_json = false
            true,  // append_footer = true
            sqlite.clone(),
        ),
    )
    .await
    {
        Ok(res) => res,
        Err(_) => Err(anyhow::anyhow!(
            "AI pipeline timed out after 150s — please try again."
        )),
    };

    // Phase D — agentic reflection. After the first answer, a cheap reviewer
    // checks whether a specific fact is still missing (the "single-iteration"
    // complaint). If a gap is found, we run ONE extra targeted search and
    // regenerate the answer with the new facts. Bounded to one iteration.
    if crate::crypto::ai::scenario::active_scenario().is_none()
        && std::env::var("CRYPTOGRAPH_NO_REFLECT").unwrap_or_default() != "1"
        && needs_reflection(&question_lower)
    {
        if let Ok(first) = &final_answer {
            if let Some(refined) = reflect_and_refine(
                &client,
                &config.gemini_api_key,
                question,
                &detected,
                &prompt,
                first,
                progress_tx.as_ref(),
                local_only,
                preferred_model.as_deref(),
                sqlite.clone(),
            )
            .await
            {
                final_answer = Ok(refined);
            }
        }
    }

    // Persist the Q/A pair so replying to a previous answer continues the
    // dialogue (multi-turn memory, bounded to the last 12 exchanges).
    if let Ok(answer) = &final_answer {
        let mut history = sqlite.load_ask_history(user_id).await.unwrap_or_default();
        history.push((question.to_string(), answer.clone()));
        if history.len() > 12 {
            let excess = history.len() - 12;
            history.drain(..excess);
        }
        sqlite.save_ask_history(user_id, &history).await;
    }

    final_answer
}
