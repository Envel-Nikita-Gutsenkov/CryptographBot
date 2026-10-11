use cryptograph::config::Config;
use cryptograph::crypto::ai;
use cryptograph::crypto::client::CryptoClient;
use cryptograph::db::sqlite::SqliteStore;
use cryptograph::format;
use std::io::{self, Write};
use std::sync::Arc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .without_time()
        .init();

    // Enable prompt debugging explicitly for the CLI
    std::env::set_var("CRYPTOGRAPH_DEBUG_PROMPTS", "1");

    let config = Config::from_env().expect("Failed to load config");
    let sqlite_store_opt = SqliteStore::new("crypto.db").ok().map(Arc::new);

    let client = Arc::new(CryptoClient::new(
        &config.cmc_api_key,
        &config.coingecko_api_key,
        &config.coincap_api_key,
        &config.cryptocompare_api_key,
        sqlite_store_opt.clone(),
    ));

    let gemini_client = ai::client::GeminiClient::new(&config.gemini_api_key);

    // Run the REPL / one-shot commands on a worker task. The main thread's
    // stack is too small for the deeply nested provider futures (e.g.
    // get_quotes → fetch_consensus polls reqwest futures that need >512KB),
    // so polling them on the main thread overflows the stack.
    let result = tokio::task::spawn(async move {
        run_main(&client, &gemini_client, &config, sqlite_store_opt).await
    })
    .await;

    match result {
        Ok(res) => res,
        Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
    }
}

async fn run_main(
    client: &Arc<CryptoClient>,
    gemini_client: &ai::client::GeminiClient,
    config: &Config,
    sqlite_store_opt: Option<Arc<SqliteStore>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // One-shot mode: `cryptograph-cli price BTC ETH SOL LINK` runs the command
    // and exits. No args → interactive REPL.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        let input = args.join(" ");
        let parts: Vec<&str> = input.splitn(2, ' ').collect();
        let cmd = parts[0];
        let arg = if parts.len() > 1 { parts[1] } else { "" };
        let cmd_args: Vec<&str> = arg.split_whitespace().collect();
        run_command(
            cmd,
            arg,
            &cmd_args,
            client,
            gemini_client,
            config,
            sqlite_store_opt.as_ref(),
        )
        .await?;
        return Ok(());
    }

    println!("========================================");
    println!(" Cryptograph CLI REPL (AI Sandbox)");
    println!(" Type 'help' for commands.");
    println!("========================================");

    loop {
        print!("> ");
        io::stdout().flush()?;

        let mut input = String::new();
        let bytes_read = io::stdin().read_line(&mut input).unwrap_or(0);
        if bytes_read == 0 {
            break;
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        let parts: Vec<&str> = input.splitn(2, ' ').collect();
        let cmd = parts[0];
        let arg = if parts.len() > 1 { parts[1] } else { "" };
        let args: Vec<&str> = arg.split_whitespace().collect();

        run_command(
            cmd,
            arg,
            &args,
            client,
            gemini_client,
            config,
            sqlite_store_opt.as_ref(),
        )
        .await?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_command(
    cmd: &str,
    arg: &str,
    args: &[&str],
    client: &Arc<CryptoClient>,
    gemini_client: &ai::client::GeminiClient,
    config: &Config,
    sqlite_store_opt: Option<&Arc<SqliteStore>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match cmd {
        "help" => {
            println!("Commands:");
            println!("  price <SYM>          - Formatted quote card");
            println!("  global               - Formatted global metrics");
            println!("  fng                  - Formatted Fear & Greed index");
            println!("  news <SYM>           - List news articles");
            println!("  market <SYM>         - AI Market Summary");
            println!("  ask <TEXT>           - AI Assistant query (like /ask)");
            println!("  ai_analysis <SYM>    - Full single coin analysis");
            println!("  ai_compare <S1> <S2> - Multi-coin comparison");
            println!("  ai_watcher <S1> <S2> - Real-time watcher analysis");
            println!("  ai_news <SYM>        - Map-Reduce news pipeline");
            println!("  exit / quit          - Exit the REPL");
        }
        "price" => {
            if args.is_empty() {
                println!("Usage: price <SYMBOL>");
                return Ok(());
            }
            match client.get_quotes(&args).await {
                Ok(quotes) => {
                    if quotes.is_empty() {
                        println!("No quotes found.");
                    } else {
                        println!("\n{}\n", format::fmt_quote_card(&quotes[0]));
                    }
                }
                Err(e) => println!("Error: {}", e),
            }
        }
        "global" => match client.get_global_metrics().await {
            Ok(g) => println!("\n{}\n", format::fmt_global_metrics(&g)),
            Err(e) => println!("Error: {}", e),
        },
        "fng" => match client.get_fng().await {
            Ok(f) => println!("\n{}\n", format::format_fng(&f)),
            Err(e) => println!("Error: {}", e),
        },
        "news" => {
            if args.is_empty() {
                println!("Usage: news <SYMBOL>");
                return Ok(());
            }
            match client.get_news(args[0]).await {
                Ok(news) => {
                    if news.is_empty() {
                        println!("No news found for {}", args[0]);
                    } else {
                        println!("\nNews for {}:", args[0].to_uppercase());
                        for n in news {
                            println!("- [{}] {}", n.source, n.title);
                        }
                        println!();
                    }
                }
                Err(e) => println!("Error: {}", e),
            }
        }
        "market" => {
            if args.is_empty() {
                println!("Usage: market <SYMBOL>");
                return Ok(());
            }
            let sym = args[0];
            println!("Gathering market data for {}...", sym);

            let quotes = client.get_quotes(&[sym]).await.unwrap_or_default();
            if quotes.is_empty() {
                println!("Could not fetch quotes.");
                return Ok(());
            }
            let q = &quotes[0];
            let news = client.get_news(sym).await.unwrap_or_default();
            let fng = client.get_fng().await.ok();

            let mut data_str = String::new();
            data_str.push_str(&format!("Price: ${}\n", q.price_usd));
            data_str.push_str(&format!(
                "1h: {:?} 24h: {:?} 7d: {:?} 30d: {:?}\n",
                q.change_1h, q.change_24h, q.change_7d, q.change_30d
            ));
            data_str.push_str(&format!(
                "Vol: ${} MktCap: ${}\n",
                q.volume_24h, q.market_cap
            ));

            if let Some(f) = fng {
                data_str.push_str(&format!("FnG: {} ({})\n", f.value, f.value_classification));
            }
            if !news.is_empty() {
                data_str.push_str("Recent News:\n");
                for n in news.iter().take(3) {
                    data_str.push_str(&format!("- {}\n", n.title));
                }
            }

            println!("Direct market data (AI skipped):");
            println!("\n--- DATA ---\n{}\n", data_str);
        }
        "ask" => {
            if arg.is_empty() {
                println!("Usage: ask <YOUR QUESTION>");
                return Ok(());
            }
            println!("Gathering context and executing ask pipeline...");

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let _ui_task = tokio::spawn(async move {
                while let Some(status) = rx.recv().await {
                    println!(">> [AI Status] {}", status);
                }
            });

            let fake_user_id = 0;
            let store = Arc::new(cryptograph::db::Store::new());
            let client = client.clone();
            let config = Arc::new(config.clone());
            let question = arg.to_string();
            let sqlite_clone = sqlite_store_opt.unwrap().clone();

            // Spawn on a new task to get a fresh stack (avoids stack overflow from deeply nested async)
            let result = tokio::task::spawn(async move {
                cryptograph::bot::handlers::ask::execute_ask_pipeline(
                    &question,
                    fake_user_id,
                    store,
                    client,
                    config,
                    sqlite_clone,
                    Some(tx),
                    None,
                )
                .await
            })
            .await;

            match result {
                Ok(Ok(response)) => {
                    println!("\n=== [AI RESPONSE] ===\n{}\n===================", response);
                }
                Ok(Err(e)) => {
                    eprintln!("\n=== [AI ERROR] ===\n{}\n==================", e);
                }
                Err(e) => {
                    eprintln!("\n=== [TASK PANIC] ===\n{}\n==================", e);
                }
            }
        }
        "ai_analysis" => {
            if args.is_empty() {
                println!("Usage: ai_analysis <SYMBOL>");
                return Ok(());
            }
            let sym = args[0];
            println!("Gathering data for ai_analysis of {}...", sym);
            let quotes = client.get_quotes(&[sym]).await.unwrap_or_default();
            if quotes.is_empty() {
                println!("No quotes found.");
                return Ok(());
            }
            let news = client.get_news(sym).await.unwrap_or_default();
            let mut news_summary = String::new();
            for n in news.iter().take(5) {
                news_summary.push_str(&format!("- [{}] {}\n", n.source, n.title));
            }

            let q = &quotes[0];
            let market_data = format!("Price: ${}\n24h: {:?}\n", q.price_usd, q.change_24h);

            match gemini_client
                .generate_analysis(
                    sym,
                    &market_data,
                    &news_summary,
                    None,
                    None,
                    sqlite_store_opt.unwrap().clone(),
                )
                .await
            {
                Ok(res) => println!("\n--- AI RESPONSE ---\n{}\n", res),
                Err(e) => println!("Error: {}", e),
            }
        }
        "ai_compare" => {
            if args.is_empty() {
                println!("Usage: ai_compare <SYM1> <SYM2> ...");
                return Ok(());
            }
            println!("Gathering data for ai_watcher of {:?}...", args);
            let quotes = client.get_quotes(&args).await.unwrap_or_default();
            if quotes.is_empty() {
                println!("No quotes found.");
                return Ok(());
            }
            let mut assets_data = String::new();
            for q in quotes {
                assets_data.push_str(&format!(
                    "{}: ${} (24h: {:?})\n",
                    q.symbol, q.price_usd, q.change_24h
                ));
            }

            match gemini_client
                .generate_multi_analysis(
                    &assets_data,
                    None,
                    None,
                    sqlite_store_opt.unwrap().clone(),
                )
                .await
            {
                Ok(res) => println!("\n--- AI RESPONSE ---\n{}\n", res),
                Err(e) => println!("Error: {}", e),
            }
        }
        "ai_watcher" => {
            if args.is_empty() {
                println!("Usage: ai_watcher <SYM1> <SYM2> ...");
                return Ok(());
            }
            println!("Gathering data for ai_watcher of {:?}...", args);
            let quotes = client.get_quotes(&args).await.unwrap_or_default();
            if quotes.is_empty() {
                println!("No quotes found.");
                return Ok(());
            }
            let mut symbols_data = String::new();
            for q in quotes {
                symbols_data.push_str(&format!(
                    "{}: ${} (24h: {:?})\n",
                    q.symbol, q.price_usd, q.change_24h
                ));
            }

            match gemini_client
                .watcher_analysis(&symbols_data, None, None, sqlite_store_opt.unwrap().clone())
                .await
            {
                Ok(res) => println!(
                    "\n--- AI RESPONSE ---\nSeverity: {:?}\nHeadline: {}\nDetail: {}\n",
                    res.severity, res.headline, res.detail
                ),
                Err(e) => println!("Error: {}", e),
            }
        }
        "ai_news" => {
            if args.is_empty() {
                println!("Usage: ai_news <SYMBOL>");
                return Ok(());
            }
            let sym = args[0];
            println!("Gathering news for ai_news of {}...", sym);
            let articles = client.get_news(sym).await.unwrap_or_default();
            if articles.is_empty() {
                println!("No news found.");
                return Ok(());
            }

            let mut news_text = String::new();
            let mut char_count = 0;
            let max_chars = 60_000;
            for (i, a) in articles.iter().enumerate() {
                let text = if a.body.len() > 800 {
                    crate::format::truncate_chars(&a.body, 800)
                } else {
                    a.body.clone()
                };
                let entry = format!("{}. {}\n{}\n\n", i + 1, a.title, text);
                if char_count + entry.len() > max_chars {
                    break;
                }
                news_text.push_str(&entry);
                char_count += entry.len();
            }

            let market_data = "Global Market Context: (Mock for CLI test)".to_string();

            match ai::pipeline::summarize_news_single_pass(
                sym,
                &news_text,
                &market_data,
                None,
                sqlite_store_opt.unwrap().clone(),
            )
            .await
            {
                Ok(res) => println!("\n--- AI RESPONSE ---\n{}\n", res),
                Err(e) => println!("Error: {}", e),
            }
        }
        "exit" | "quit" => return Ok(()),
        _ => println!("Unknown command. Type 'help' for options."),
    }

    Ok(())
}
