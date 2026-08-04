use std::sync::Arc;
use teloxide::{prelude::*, types::ParseMode};

use crate::{
    config::Config,
    crypto::{ai::prompts::build_ask_prompt, CryptoClient},
    db::{SqliteStore, Store},
};

pub async fn handle_ask_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    sqlite: Arc<crate::db::SqliteStore>,
    args: String,
) -> anyhow::Result<()> {
    let question = args.trim().to_string();

    if question.is_empty() {
        bot.send_message(
            msg.chat.id,
            "💬 <b>AI Chat</b>\n\n\
             Ask anything about your portfolio or the market. Examples:\n\
             • <code>/ask why is ETH dropping?</code>\n\
             • <code>/ask should I buy more SOL now?</code>\n\
             • <code>/ask what is my average BTC entry price?</code>",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);

    let loading = bot
        .send_message(msg.chat.id, "🧠 Thinking…")
        .parse_mode(ParseMode::Html)
        .await?;

    // Call the shared core pipeline
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let bot_clone = bot.clone();
    let chat_id = msg.chat.id;
    let msg_id = loading.id;

    // Spawn a task to update the UI with progress
    let _ui_task = tokio::spawn(async move {
        while let Some(status) = rx.recv().await {
            let _ = bot_clone.edit_message_text(chat_id, msg_id, status).await;
        }
    });

    let reply_context = msg.reply_to_message().and_then(|m| m.text()).map(|s| {
        let chars: Vec<char> = s.chars().collect();
        if chars.len() > 2000 {
            let mut t: String = chars.into_iter().take(2000).collect();
            t.push_str("... (truncated)");
            t
        } else {
            s.to_string()
        }
    });

    let result = execute_ask_pipeline(
        &question,
        user_id,
        store,
        cmc,
        config.clone(),
        sqlite,
        Some(tx),
        reply_context,
    )
    .await;

    let _ = bot.delete_message(msg.chat.id, loading.id).await;

    match result {
        Ok(answer) => {
            if let Err(e) = crate::bot::handlers::ai::send_rich_message_via_api(
                &bot,
                config.clone(),
                msg.chat.id,
                answer,
            )
            .await
            {
                let err_str = e.to_string();
                let safe_err = if err_str.len() > 1000 {
                    format!("{}...", &err_str.chars().take(1000).collect::<String>())
                } else {
                    err_str
                };
                bot.send_message(msg.chat.id, format!("❌ AI failed to respond: {safe_err}"))
                    .await?;
            }
        }
        Err(e) => {
            let err_str = e.to_string();
            let safe_err = if err_str.len() > 1000 {
                format!("{}...", &err_str.chars().take(1000).collect::<String>())
            } else {
                err_str
            };
            bot.send_message(msg.chat.id, format!("❌ AI pipeline failed: {safe_err}"))
                .await?;
        }
    }

    Ok(())
}

/// CLI reproduction of the full `/ask` pipeline (`cargo run -- ask <user|demo> "q"`).
///
/// Enables `CRYPTOGRAPH_DEBUG_PROMPTS` so every stage is printed: the system
/// prompt, every web search the models request (and its results), both
/// generator drafts, the critic, and finally the answer. This is how cases like
/// "AI mixed old discontinued Trezor models with new ones" get reproduced and
/// diagnosed — you can see exactly what data the model was given and where the
/// mix-up entered.
pub async fn run_cli_ask(
    user_id: u64,
    question: &str,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    sqlite: Arc<SqliteStore>,
) -> anyhow::Result<()> {
    std::env::set_var("CRYPTOGRAPH_DEBUG_PROMPTS", "1");

    println!("\n=== /ask PIPELINE REPLAY ===");
    println!("User: {}", user_id);
    println!("Question: {}\n", question);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let progress_task = tokio::spawn(async move {
        while let Some(status) = rx.recv().await {
            println!("⏳ {}", status);
        }
    });

    let result = execute_ask_pipeline(
        question,
        user_id,
        store,
        cmc,
        config,
        sqlite,
        Some(tx),
        None,
    )
    .await;

    drop(progress_task); // tx is dropped with the pipeline; task ends on its own

    match result {
        Ok(answer) => {
            println!("\n=== FINAL ANSWER ===\n{}", answer);
        }
        Err(e) => {
            println!("\n=== PIPELINE FAILED ===\n{:#}", e);
        }
    }
    Ok(())
}

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

    let (fear_greed, global_metrics) = tokio::join!(
        crate::bot::handlers::ai::fetch_fear_and_greed_index(),
        cmc.get_global_metrics(),
    );

    let global_info = global_metrics
        .map(|g| {
            let mut info = format!(
                "Total Market Cap: ${:.0}\n24h Volume: ${:.0}\nBTC Dom: {:.2}%\nETH Dom: {:.2}%",
                g.total_market_cap, g.total_volume_24h, g.btc_dominance, g.eth_dominance
            );
            if let Some(s) = g.stablecoin_mcap {
                info.push_str(&format!("\nStablecoin Mcap: ${:.0}", s));
            }
            if let Some(d) = g.dex_volume_24h {
                info.push_str(&format!("\nDEX Volume 24h: ${:.0}", d));
            }
            info
        })
        .unwrap_or_else(|_| "Market data unavailable.".into());

    let market_data = if symbols.is_empty() {
        format!(
            "Global Market Context:\nFear & Greed Index: {}\n{}",
            fear_greed, global_info
        )
    } else {
        let syms_ref: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
        if let Some(tx) = progress_tx.as_ref() {
            let _ = tx.send("⏳ Fetching deep historical market data... Please wait.".to_string());
        }
        let mut tradfi_syms = Vec::new();
        for s in &symbols {
            if s.ends_with("ON") {
                let base = s.strip_suffix("ON").unwrap();
                if base.len() >= 2 && base.len() <= 5 {
                    tradfi_syms.push(base.to_string());
                }
            }
        }
        let tradfi_refs: Vec<&str> = tradfi_syms.iter().map(|s| s.as_str()).collect();

        let (quotes_res, onchain_res, tradfi_res) = tokio::join!(
            cmc.get_quotes(&syms_ref),
            cmc.get_onchain_metrics(&syms_ref),
            cmc.get_tradfi_prices(&tradfi_refs)
        );

        let quotes_str = match quotes_res {
            Ok(quotes) => {
                let mut parts = Vec::new();
                for q in &quotes {
                    let mut yaml = format!(
                        "- Symbol: {}\n  Price_USD: {:.2}\n  Chg_1h: {:+.2}%\n  Chg_24h: {:+.2}%\n  Chg_7d: {:+.2}%",
                        q.symbol,
                        q.price_usd,
                        q.change_1h.unwrap_or(0.0),
                        q.change_24h.unwrap_or(0.0),
                        q.change_7d.unwrap_or(0.0)
                    );
                    if let Some(c30) = q.change_30d {
                        yaml.push_str(&format!("\n  Chg_30d: {:+.2}%", c30));
                    }
                    if let Some(c90) = q.change_90d {
                        yaml.push_str(&format!("\n  Chg_90d: {:+.2}%", c90));
                    }
                    if let Some(c365) = q.change_365d {
                        yaml.push_str(&format!("\n  Chg_365d: {:+.2}%", c365));
                    }
                    if let Some(c730) = q.change_730d {
                        yaml.push_str(&format!("\n  Chg_730d: {:+.2}%", c730));
                    }
                    if let Some(ath) = q.ath {
                        let disc = q.ath_change_percentage.unwrap_or_else(|| {
                            if ath > 0.0 {
                                (q.price_usd - ath) / ath * 100.0
                            } else {
                                0.0
                            }
                        });
                        yaml.push_str(&format!(
                            "\n  High_1Y: {:.2}\n  Discount_From_Peak: {:+.2}%",
                            ath, disc
                        ));
                    }
                    parts.push(yaml);
                }
                parts.join("\n")
            }
            Err(_) => "Quotes unavailable.".to_string(),
        };

        let mut onchain_str = String::from("ON_CHAIN_METRICS (Pre-fetched via APIs):\n");
        if onchain_res.is_empty() {
            onchain_str.push_str("Unavailable.");
        } else {
            for m in onchain_res {
                let mut row = format!("- {}:", m.symbol);
                if let Some(tvl) = m.tvl {
                    row.push_str(&format!(" TVL: {:.2}B USD |", tvl / 1_000_000_000.0));
                }
                if let Some(act) = m.active_addresses_24h {
                    row.push_str(&format!(" Active_Addresses_24h: {} |", act));
                }
                if let Some(fees) = m.fees_24h_native {
                    row.push_str(&format!(" Fees_24h_Native: {:.2} |", fees));
                }
                if let Some(fr) = m.funding_rate {
                    row.push_str(&format!(" Funding_Rate: {:+.3}% |", fr * 100.0));
                }
                if let Some(oi) = m.open_interest {
                    row.push_str(&format!(
                        " Open_Interest: {:.2}M {}",
                        oi / 1_000_000.0,
                        m.symbol
                    ));
                }
                onchain_str.push_str(&row);
                onchain_str.push('\n');
            }
        }

        let mut tradfi_str = String::new();
        if !tradfi_res.is_empty() {
            tradfi_str.push_str("\nTRADFI RWA ANCHORS (Real-World Asset Prices):\n");
            for quote in tradfi_res {
                let mut yaml = format!(
                    "- Symbol: {}\n  Real_Price_USD: {:.2}",
                    quote.symbol, quote.price
                );
                if let Some(c1d) = quote.change_1d {
                    yaml.push_str(&format!("\n  Real_Chg_1d: {:+.2}%", c1d));
                }
                if let Some(c7d) = quote.change_7d {
                    yaml.push_str(&format!("\n  Real_Chg_7d: {:+.2}%", c7d));
                }
                if let Some(c30d) = quote.change_30d {
                    yaml.push_str(&format!("\n  Real_Chg_30d: {:+.2}%", c30d));
                }
                if let Some(c90d) = quote.change_90d {
                    yaml.push_str(&format!("\n  Real_Chg_90d: {:+.2}%", c90d));
                }
                if let Some(c1y) = quote.change_1y {
                    yaml.push_str(&format!("\n  Real_Chg_1y: {:+.2}%", c1y));
                }
                tradfi_str.push_str(&yaml);
                tradfi_str.push('\n');
            }
        }

        format!(
            "PORTFOLIO:\n{}\n\nCURRENT MARKET DATA:\nQuotes:\n{}\n\n{}\n{}\n\nGlobal Market Context:\nFear & Greed Index: {}\n{}",
            portfolio_context, quotes_str, onchain_str, tradfi_str, fear_greed, global_info
        )
    };

    // Fetch news context
    let mut news_symbols = Vec::new();
    for sym in &symbols {
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

    let mut news_context = if !news_symbols.is_empty() {
        let mut news_parts = Vec::new();
        for (sym, _) in news_symbols.iter().take(fetch_count) {
            if let Ok(Some(news)) = sqlite.get_news_summary(sym).await {
                if !news.contains("Failed to analyze recent news") && !news.trim().is_empty() {
                    news_parts.push(format!("Latest verified news summary for {sym}:\n{news}"));
                }
            }
        }
        news_parts.join("\n")
    } else {
        String::new()
    };

    if news_context.trim().is_empty() || wants_news {
        if let Ok(Some(news)) = sqlite.get_news_summary("general").await {
            if !news.contains("Failed to analyze recent news") && !news.trim().is_empty() {
                news_context = format!("{}\n\nRecent general crypto news:\n{news}", news_context);
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

        if let Some(tx) = progress_tx.as_ref() {
            let _ =
                tx.send("⏳ Analyzing raw news data (MoE pipeline)... Please wait.".to_string());
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
                        format!("{}...", &article.body[..800])
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

                match crate::crypto::ai::pipeline::summarize_news_single_pass(
                    sym,
                    &news_text,
                    &market_data,
                    None,
                    sqlite.clone(),
                )
                .await
                {
                    Ok(verified_summary) => {
                        let cache_key = if sym == "ALL" { "general" } else { sym };
                        sqlite.save_news_summary(cache_key, &verified_summary).await;
                        live_parts.push(format!(
                            "Latest verified news summary for {db_key}:\n{verified_summary}"
                        ));
                    }
                    Err(e) => {
                        sqlite
                            .save_news_summary(db_key, "No recent verified news summary available.")
                            .await;
                        tracing::error!("Single-pass pipeline failed on-demand for {}: {}", sym, e);
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
            if let Some(tx) = progress_tx.as_ref() {
                let _ = tx.send("🧠 Formulating final response...".to_string());
            }
        } else {
            news_context = "No recent news available.".to_string();
        }
    } else if news_context.trim().is_empty() {
        news_context = "No recent news available.".to_string();
    }

    // Sanitize internal MoE critic planner notes (e.g., "*   *Macro/Price Action Section:* ...")
    let news_context = news_context
        .lines()
        .filter(|line| {
            let t = line.trim();
            !(t.starts_with('*') && t.contains("Section:"))
        })
        .collect::<Vec<_>>()
        .join("\n");

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

    // AI memory: the user's standing notes and active goals — the model must
    // always respect them when answering (e.g. fixed investment size, coins on hold).
    let memory_context = store.read(user_id, |p| {
        let memos = p
            .memos
            .iter()
            .map(|m| format!("- memo: {}", m.text))
            .collect::<Vec<_>>()
            .join("\n");
        let goals = p
            .goals
            .iter()
            .filter(|g| g.active && !g.done)
            .map(|g| {
                format!(
                    "- goal #{}: {} (set {})",
                    g.id,
                    g.text,
                    g.created_at.format("%Y-%m-%d")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        [memos, goals]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
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
    let planned = plan_web_search(&client, &config.gemini_api_key, question).await;
    let search_queries: Vec<String> = if !planned.is_empty() {
        planned
    } else {
        // Planner unavailable/failed — fall back to the keyword heuristic so
        // obvious cases ("best wallet", "news") still trigger a search.
        if needs_web_search(&question_lower) {
            vec![question.to_string()]
        } else {
            vec![]
        }
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

    let prompt = build_ask_prompt(
        &question,
        &portfolio_context,
        &market_data,
        &market_intel_context,
        &news_context,
        &web_search_context,
        &current_date,
        reply_context.as_deref(),
        &memory_context,
        &preferred_lang,
    );

    let preferred_model = store.read(user_id, |p| p.settings.ai_model_override.clone());

    let result = crate::crypto::ai::pipeline::call_with_critic(
        &client,
        &config.gemini_api_key,
        &prompt,
        preferred_model.as_deref(),
        false, // is_json = false
        true,  // append_footer = true
        sqlite.clone(),
    )
    .await;
    result
}

/// Fetch the top search-result pages and have a cheap model distill them into
/// a dense digest focused on the user's question. Falls back to raw snippets if
/// page fetching or the summarizer fails (fail-safe — never blocks the answer).
async fn fetch_and_summarize(
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
    let planned = plan_web_search(&client, &gemini_key, query).await;
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
/// we return an empty vec so the caller falls back to the keyword heuristic
/// (or proceeds without search).
async fn plan_web_search(
    client: &reqwest::Client,
    gemini_key: &str,
    question: &str,
) -> Vec<String> {
    let models = crate::crypto::ai::pools::get_available_models(Some("watcher"));
    let prompt = format!(
        "You are a web-search planner for a crypto assistant.\n\
         Today's date: {}.\n\
         Decide whether answering this user question REQUIRES fresh, up-to-date information from \
         the web (current prices, breaking news, hardware-wallet / product comparisons, ratings, \
         reviews, regulations, recent events).\n\
         If it does, output EXACTLY one line: SEARCH: <one concise query>\n\
         If the question is complex and covers SEVERAL distinct topics, you MAY output up to \
         three SEARCH: lines, one per topic.\n\
         IMPORTANT: write the query in ENGLISH always, even if the user writes in Russian or \
         another language — English sources are the most trustworthy for crypto.\n\
         CRITICAL: include the current year ({} or the exact date) in the query when it is a \n\
         \"best/top/review\" type question, so the results are from the current year, not years ago.\n\
         If general knowledge or already-provided market data is enough, output exactly: NO_SEARCH\n\
         User question: \"{}\"",
        chrono::Utc::now().format("%Y-%m-%d"),
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
        "should i",
        "what is",
    ];
    TRIGGERS.iter().any(|t| question_lower.contains(t))
}
