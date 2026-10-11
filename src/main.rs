mod access;
mod alerts;
mod bot;
mod config;
mod crypto;
mod db;
mod error;
mod format;
mod models;
mod security;
mod supervisor;
mod trading;
mod watchdog;

use std::sync::Arc;
use tracing_subscriber::EnvFilter;

/// Synthetic user id used by `cargo run -- goal demo` (never collides with real users).
const DEMO_USER_ID: u64 = 999_000_001;

/// In-memory demo portfolio for `cargo run -- goal demo` / `ask demo`: a few
/// sample holdings, memos and goals so the CLI pipelines are testable offline.
fn demo_portfolio() -> crate::models::Portfolio {
    use crate::models::{Goal, Memo, Portfolio};
    use chrono::{Datelike, Timelike, Utc};

    let now = Utc::now();
    let end_of_month = now.with_day(28).unwrap_or(now).with_hour(23).unwrap_or(now);

    let mut p = Portfolio::default();
    // Generic sample holdings so the "2x goal" logic is testable.
    p.next_memo_id = 1;
    p.next_goal_id = 1;
    p.manual_cost_basis.insert("BTC".into(), 30_000.0);
    p.manual_cost_basis.insert("ETH".into(), 6_000.0);
    p.manual_cost_basis.insert("SOL".into(), 1_500.0);
    for (symbol, amount) in [("BTC", 0.5), ("ETH", 2.0), ("SOL", 10.0)] {
        let id = p.alloc_holding_id();
        p.holdings.push(crate::models::Holding {
            id,
            wallet_id: 1,
            symbol: symbol.to_string(),
            amount,
            corrections: vec![],
        });
    }
    p.memos = vec![
        Memo {
            id: 1,
            text: "Always invest 100 USD per entry".into(),
            created_at: now,
        },
        Memo {
            id: 2,
            text: "BTC is on hold; watch altcoins for entries".into(),
            created_at: now,
        },
    ];
    p.goals = vec![
        Goal {
            id: 1,
            text: "Spend 400 USD on crypto this month".into(),
            created_at: now,
            expires_at: Some(end_of_month),
            active: true,
            last_alert_at: None,
            last_declined_at: None,
            alert_count: 0,
            done: false,
            progress_notes: vec![crate::models::ProgressNote {
                at: now,
                text: "Bought 100 USD of ETH".into(),
            }],
            ai_summary: Some(
                "wait — spent 100 of 400, looking for a strong dip to continue".into(),
            ),
            sent_alerts: Vec::new(),
        },
        Goal {
            id: 2,
            text: "Alert me on top discounts so I don't raise my average entry".into(),
            created_at: now,
            expires_at: None,
            active: true,
            last_alert_at: None,
            last_declined_at: None,
            alert_count: 0,
            done: false,
            progress_notes: Vec::new(),
            ai_summary: None,
            sent_alerts: Vec::new(),
        },
        Goal {
            id: 3,
            text: "Alert me when my assets reach 2x".into(),
            created_at: now,
            expires_at: None,
            active: true,
            last_alert_at: None,
            last_declined_at: None,
            alert_count: 0,
            done: false,
            progress_notes: Vec::new(),
            ai_summary: None,
            sent_alerts: Vec::new(),
        },
    ];
    p
}

fn demo_store() -> Arc<db::Store> {
    let store = db::Store::new();
    store.restore(DEMO_USER_ID, demo_portfolio());
    Arc::new(store)
}

fn main() -> anyhow::Result<()> {
    // All CLI paths run via block_on, which executes on the CALLING thread —
    // and on Windows that thread starts with only a 1MB stack. The deep async
    // call chains (e.g. tokio::join! over 13 price providers, web search)
    // overflow it, so the whole app runs on a dedicated thread with a large
    // stack; the runtime workers get their own 8MB stacks on top.
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| -> anyhow::Result<()> {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(8 * 1024 * 1024)
                .build()?;
            rt.block_on(main_inner())
        })
        .unwrap()
        .join()
        .unwrap()
}

async fn main_inner() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 && (args[1] == "analyze" || args[1] == "--analyze") {
        let config = Arc::new(config::Config::from_env()?);
        if let Some(parent) = std::path::Path::new(&config.sqlite_db).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        db::SqliteStore::initialize_schema(&config.sqlite_db)?;
        let sqlite = Arc::new(db::SqliteStore::new(&config.sqlite_db)?);
        crate::crypto::ai::deepseek_budget::DeepSeekBudget::instance().init(Some(sqlite.clone()));
        let cmc = Arc::new(crypto::CryptoClient::new(
            &config.cmc_api_key,
            &config.coingecko_api_key,
            &config.coincap_api_key,
            &config.cryptocompare_api_key,
            Some(sqlite.clone()),
        ));
        let symbol = args[2].to_uppercase();
        bot::handlers::run_cli_analysis(&symbol, cmc, config).await?;
        return Ok(());
    }

    // `cargo run -- goal <user_id|demo> [-v]` — transparently run one proactive
    // goal scan and print every pipeline stage (prompt → drafts → reviewer →
    // critic → actions → notifications), so decisions like "goal is done/stale"
    // can be verified and debugged without the Telegram bot.
    if args.len() > 2 && (args[1] == "goal" || args[1] == "--goal") {
        let verbose = args.iter().any(|a| a == "-v" || a == "--verbose");

        let config = Arc::new(config::Config::from_env()?);
        if let Some(parent) = std::path::Path::new(&config.sqlite_db).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        db::SqliteStore::initialize_schema(&config.sqlite_db)?;

        // `demo`: synthetic user with sample goals/memos — inspect the decision
        // process without touching the real database.
        if args[2].eq_ignore_ascii_case("demo") {
            let store = demo_store();
            let sqlite = Arc::new(db::SqliteStore::new(&config.sqlite_db)?);
            crate::crypto::ai::deepseek_budget::DeepSeekBudget::instance()
                .init(Some(sqlite.clone()));
            let cmc = Arc::new(crypto::CryptoClient::new(
                &config.cmc_api_key,
                &config.coingecko_api_key,
                &config.coincap_api_key,
                &config.cryptocompare_api_key,
                Some(sqlite.clone()),
            ));
            crypto::ai::proactive_worker::run_cli_scan(
                DEMO_USER_ID,
                store,
                cmc,
                sqlite,
                verbose,
                false,
            )
            .await?;
            return Ok(());
        }

        let user_id: u64 = args[2]
            .trim()
            .parse()
            .map_err(|_| anyhow::anyhow!("Usage: cargo run -- goal <user_id|demo> [-v]"))?;

        let sqlite = Arc::new(db::SqliteStore::new(&config.sqlite_db)?);
        crate::crypto::ai::deepseek_budget::DeepSeekBudget::instance().init(Some(sqlite.clone()));

        let store = Arc::new(db::Store::new());
        for (uid, data) in db::SqliteStore::load_all_users(&config.sqlite_db).unwrap_or_default() {
            if let Ok(p) = store.decode_backup(&data) {
                store.restore(uid, p);
            }
        }
        for (uid, list) in db::SqliteStore::load_user_memos(&config.sqlite_db).unwrap_or_default() {
            store.write(uid, |p| p.memos = list);
        }
        for (uid, list) in db::SqliteStore::load_user_goals(&config.sqlite_db).unwrap_or_default() {
            store.write(uid, |p| p.goals = list);
        }

        let cmc = Arc::new(crypto::CryptoClient::new(
            &config.cmc_api_key,
            &config.coingecko_api_key,
            &config.coincap_api_key,
            &config.cryptocompare_api_key,
            Some(sqlite.clone()),
        ));

        crypto::ai::proactive_worker::run_cli_scan(user_id, store, cmc, sqlite, verbose, true)
            .await?;
        return Ok(());
    }

    // `cargo run -- trade <subcommand> ...` — the trading module CLI (1:1 with
    // the Telegram /trade commands). See docs/trading/TRADING_MODULE_PLAN.md §6.
    if args.len() > 1 && (args[1] == "trade" || args[1] == "--trade") {
        return trading::cli::run(&args[2..]);
    }

    // `cargo run -- models <refresh|list|rank|stats|calibrate> ...` — generalized
    // lazy model ranking across all providers (OpenRouter free-tier discovery +
    // AI-judged benchmarking + per-model statistics). See src/crypto/ai/model_rank.rs.
    if args.len() > 2 && (args[1] == "models" || args[1] == "--models") {
        let config = Arc::new(config::Config::from_env()?);
        if let Some(parent) = std::path::Path::new(&config.sqlite_db).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        db::SqliteStore::initialize_schema(&config.sqlite_db)?;
        let sqlite = Arc::new(db::SqliteStore::new(&config.sqlite_db)?);
        crate::crypto::ai::deepseek_budget::DeepSeekBudget::instance().init(Some(sqlite.clone()));
        crate::crypto::ai::openrouter_budget::OpenRouterBudget::instance()
            .init(Some(sqlite.clone()));

        let sub = args[2].to_lowercase();
        let client = reqwest::Client::new();
        match sub.as_str() {
            "refresh" => {
                let report = crate::crypto::ai::model_rank::discover(&client, &sqlite).await?;
                println!(
                    "Model registry refreshed: {} total, {} new",
                    report.total, report.new
                );
                for (provider, id, _alias, display, ctx, fp, tested, tfp, bc) in
                    sqlite.get_model_registry()?
                {
                    let tested_s = tested.unwrap_or_else(|| "never".to_string());
                    println!(
                        "  [{provider}] {id}  |  {display}  |  ctx {ctx}  |  bench {bc}  |  tested {tested_s}  |  fp {fp}{}",
                        if tfp.is_empty() { String::new() } else { format!(" (tested {tfp})") }
                    );
                }
            }
            "list" => {
                let rows = sqlite.get_model_registry()?;
                if rows.is_empty() {
                    println!("No discovered models. Run `cargo run -- models refresh` first.");
                }
                for (provider, id, alias, display, ctx, _fp, tested, _tfp, bc) in rows {
                    let tested_s = tested.unwrap_or_else(|| "never".to_string());
                    println!(
                        "  [{provider}] {id}  |  alias {alias}  |  {display}  |  ctx {ctx}  |  bench {bc}  |  tested {tested_s}"
                    );
                }
            }
            "rank" => {
                let report = crate::crypto::ai::model_rank::rank(&client, &sqlite).await?;
                println!(
                    "Ranked {} models ({} skipped: rate-limit/cooldown/budget)",
                    report.tested.len(),
                    report.skipped_tested
                );
                for r in &report.tested {
                    let status = if r.ok { "OK " } else { "ERR" };
                    let rate = if r.skipped_rate_limit {
                        " (rate-limited, retry later)"
                    } else {
                        ""
                    };
                    println!(
                        "  [{status}] {}/{} role={} q={:.1} latency={}ms (reason: {}) {}{}",
                        r.provider,
                        r.model_id,
                        r.role,
                        r.quality,
                        r.latency_ms,
                        r.reason,
                        r.error.as_deref().unwrap_or(""),
                        rate
                    );
                }
                println!(
                    "Judge baseline: {:.1} (reliable: {})",
                    report.judge_baseline, report.judge_reliable
                );
                println!(
                    "OpenRouter budget remaining: {} (bench stops below {} reserve)",
                    report.budget_remaining,
                    crate::crypto::ai::model_rank::MIN_OPENROUTER_RESERVE
                );
                println!(
                    "\nAssigned roles (written to {}):",
                    crate::crypto::ai::model_rank::RANK_FILE
                );
                for m in &report.assigned {
                    println!("  prio {} {} roles={:?}", m.priority, m.id, m.roles);
                }
            }
            "stats" => {
                let days: u32 = args.get(3).and_then(|d| d.parse().ok()).unwrap_or(14);
                let rows = crate::crypto::ai::model_rank::stats(&sqlite, days)?;
                if rows.is_empty() {
                    println!("No stats yet for the last {days} days.");
                }
                println!("Model stats (last {days} days):");
                for (provider, model, calls, errors, avg_lat, avg_q) in rows {
                    let err_pct = if calls > 0 {
                        errors as f64 * 100.0 / calls as f64
                    } else {
                        0.0
                    };
                    println!(
                        "  [{provider}] {model}  calls={calls} err={err_pct:.0}% avg_latency={avg_lat}ms avg_quality={avg_q:.1}"
                    );
                }
            }
            "calibrate" => {
                let baseline = crate::crypto::ai::model_rank::calibrate(&client, &sqlite).await;
                println!(
                    "Judge baseline: {baseline:.1} (min reliable: {})",
                    crate::crypto::ai::model_rank::JUDGE_MIN_BASELINE
                );
            }
            other => {
                anyhow::bail!(
                    "Unknown models subcommand '{other}'. Use: refresh | list | rank | stats [days] | calibrate"
                );
            }
        }
        return Ok(());
    }

    // `cargo run -- wipe-knowledge` — reset the bot's knowledge base: wipes
    // news/macro/milestone/health/stats caches but keeps users, goals, memos
    // and the paid-usage budget. Use after poisoned-data incidents.
    if args.len() > 1 && (args[1] == "wipe-knowledge" || args[1] == "--wipe-knowledge") {
        let config = Arc::new(config::Config::from_env()?);
        let sqlite = db::SqliteStore::new(&config.sqlite_db)?;
        match sqlite.wipe_knowledge() {
            Ok(n) => println!("Knowledge base reset: {n} rows deleted from cache tables.\nUsers, goals, memos and paid-usage budget were KEPT."),
            Err(e) => println!("Failed to reset knowledge base: {e}"),
        }
        return Ok(());
    }

    // `cargo run -- search "question"` — inspect ONLY the web-search flow:
    // planner decision → query → search results → fetched page text. No big
    // models, no summarizer — fast way to check WHAT we search and get.
    if args.len() > 2 && (args[1] == "search" || args[1] == "--search") {
        let query = args[2..].join(" ");
        bot::handlers::ask::run_search_cli(&query).await?;
        return Ok(());
    }

    // `cargo run -- scenario <file.json> ["question"]` — deterministic
    // what-if test: activate a synthetic market state (crash / pump / euphoria)
    // and replay BOTH the /ask pipeline and the proactive goal scan against the
    // REAL pipeline code, without touching live market APIs. The scenario file
    // replaces market data, news and macro intel inside the prompts.
    if args.len() > 2 && (args[1] == "scenario" || args[1] == "--scenario") {
        let path = &args[2];
        // `--fast`: force cheap watcher-tier models so scenario runs complete in
        // seconds. The deterministic strategy layer still applies (no-FOMO,
        // no-panic rules), so the what-if verdict stays meaningful.
        if args.iter().any(|a| a == "--fast") {
            std::env::set_var("CRYPTOGRAPH_FAST_MODELS", "1");
        }
        let scenario = crate::crypto::ai::scenario::ScenarioMarket::load(path)
            .ok_or_else(|| anyhow::anyhow!("Failed to load scenario from {path}"))?;
        let question = if args.len() > 3 {
            args[3..].join(" ")
        } else {
            "Стоит ли покупать крипту сейчас?".to_string()
        };

        let config = Arc::new(config::Config::from_env()?);
        if let Some(parent) = std::path::Path::new(&config.sqlite_db).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        db::SqliteStore::initialize_schema(&config.sqlite_db)?;
        let sqlite = Arc::new(db::SqliteStore::new(&config.sqlite_db)?);
        crate::crypto::ai::deepseek_budget::DeepSeekBudget::instance().init(Some(sqlite.clone()));

        let store = demo_store();
        let cmc = Arc::new(crypto::CryptoClient::new(
            &config.cmc_api_key,
            &config.coingecko_api_key,
            &config.coincap_api_key,
            &config.cryptocompare_api_key,
            Some(sqlite.clone()),
        ));

        scenario.activate();
        println!("\n=== SCENARIO: {path} ===");
        println!("Market override active — live price/news APIs are bypassed.\n");

        bot::handlers::ask::run_cli_ask(
            DEMO_USER_ID,
            &question,
            store.clone(),
            cmc.clone(),
            config.clone(),
            sqlite.clone(),
        )
        .await?;
        crypto::ai::proactive_worker::run_cli_scan(DEMO_USER_ID, store, cmc, sqlite, true, false)
            .await?;
        return Ok(());
    }

    // `cargo run -- news <summary|history|articles|cleanup> ...` — inspect the
    // news layer: current verified summary, summary history, raw stored
    // articles (extended results) and the retention sweep. The AI prompt only
    // ever sees the summarized form; these commands expose the full store.
    if args.len() > 2 && (args[1] == "news" || args[1] == "--news") {
        let config = Arc::new(config::Config::from_env()?);
        if let Some(parent) = std::path::Path::new(&config.sqlite_db).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        db::SqliteStore::initialize_schema(&config.sqlite_db)?;
        let sqlite = Arc::new(db::SqliteStore::new(&config.sqlite_db)?);

        let sub = args[2].to_lowercase();
        match sub.as_str() {
            "summary" => {
                let sym = args.get(3).map(|s| s.as_str()).unwrap_or("general");
                match sqlite.get_news_summary_with_ts(sym).await? {
                    Some((summary, ts)) => {
                        let fresh = db::SqliteStore::news_summary_is_fresh(&ts, 24);
                        println!("=== {sym} summary (updated {ts}, fresh={fresh}) ===\n{summary}");
                    }
                    None => println!("No cached summary for {sym}."),
                }
            }
            "history" => {
                let sym = args.get(3).map(|s| s.as_str()).unwrap_or("general");
                let limit = args.get(4).and_then(|s| s.parse::<i64>().ok()).unwrap_or(5);
                let rows = sqlite.list_news_summary_history(sym, limit).await?;
                if rows.is_empty() {
                    println!("No summary history for {sym}.");
                }
                for (i, (summary, ts)) in rows.iter().enumerate() {
                    let head: String = summary.chars().take(200).collect();
                    println!(
                        "--- [{i}] {ts} ---\n{head}{}",
                        if summary.len() > 200 { "..." } else { "" }
                    );
                }
            }
            "articles" => {
                let sym = args.get(3).map(|s| s.as_str()).unwrap_or("general");
                let days = args.get(4).and_then(|s| s.parse::<i64>().ok()).unwrap_or(3);
                let rows = sqlite.get_news_articles(sym, days).await?;
                if rows.is_empty() {
                    println!("No raw articles for {sym} in the last {days} days.");
                }
                for (source, title, body, url, ts) in &rows {
                    let head: String = body.chars().take(160).collect();
                    println!("[{ts}] ({source}) {title}\n  {head}{url}\n");
                }
            }
            "cleanup" => {
                let deleted = sqlite.cleanup_news(7, 30).await?;
                println!("Retention sweep deleted {deleted} rows (articles >7d, history >30d).");
            }
            "run-once" => {
                // One full news cycle without the bot loop: fetch + summarize +
                // persist for every watched symbol, then the retention sweep.
                crate::crypto::ai::deepseek_budget::DeepSeekBudget::instance()
                    .init(Some(sqlite.clone()));
                let store = Arc::new(db::Store::new());
                for (uid, data) in
                    db::SqliteStore::load_all_users(&config.sqlite_db).unwrap_or_default()
                {
                    if let Ok(p) = store.decode_backup(&data) {
                        store.restore(uid, p);
                    }
                }
                for (uid, list) in
                    db::SqliteStore::load_user_memos(&config.sqlite_db).unwrap_or_default()
                {
                    store.write(uid, |p| p.memos = list);
                }
                for (uid, list) in
                    db::SqliteStore::load_user_goals(&config.sqlite_db).unwrap_or_default()
                {
                    store.write(uid, |p| p.goals = list);
                }
                let crypto = Arc::new(crypto::CryptoClient::new(
                    &config.cmc_api_key,
                    &config.coingecko_api_key,
                    &config.coincap_api_key,
                    &config.cryptocompare_api_key,
                    Some(sqlite.clone()),
                ));
                let symbols =
                    crate::crypto::ai::news_worker::run_news_cycle(store, sqlite.clone(), crypto)
                        .await;
                println!(
                    "News cycle done: {} symbols processed (BTC, ETH, SOL, LINK, GENERAL...).",
                    symbols.len()
                );
                for s in &symbols {
                    match sqlite.get_news_summary_with_ts(s).await? {
                        Some((summary, ts)) => {
                            let fresh = db::SqliteStore::news_summary_is_fresh(&ts, 24);
                            let head: String = summary.chars().take(120).collect();
                            println!("  {s}: updated {ts}, fresh={fresh} — {head}...");
                        }
                        None => println!("  {s}: no summary (no news found)"),
                    }
                }
            }
            other => anyhow::bail!(
                "unknown news subcommand '{other}' (summary|history|articles|cleanup|run-once)"
            ),
        }
        return Ok(());
    }

    // `cargo run -- ask <user_id|demo> "question"` — replay the full /ask
    // pipeline in the CLI with debug output: system prompt, every web search
    // the models request (+ results), both generator drafts, the critic and
    // the final answer. Use it to reproduce "AI mixed X with Y" cases.
    if args.len() > 2 && (args[1] == "ask" || args[1] == "--ask") {
        let config = Arc::new(config::Config::from_env()?);
        if let Some(parent) = std::path::Path::new(&config.sqlite_db).parent() {
            if !parent.as_os_str().is_empty() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        db::SqliteStore::initialize_schema(&config.sqlite_db)?;
        let sqlite = Arc::new(db::SqliteStore::new(&config.sqlite_db)?);
        crate::crypto::ai::deepseek_budget::DeepSeekBudget::instance().init(Some(sqlite.clone()));

        let (user_id, question) = if args[2].eq_ignore_ascii_case("demo") {
            (DEMO_USER_ID, args[3..].join(" "))
        } else if let Ok(uid) = args[2].trim().parse::<u64>() {
            (uid, args[3..].join(" "))
        } else {
            (DEMO_USER_ID, args[2..].join(" "))
        };
        if question.trim().is_empty() {
            anyhow::bail!("Usage: cargo run -- ask <user_id|demo> \"question\"");
        }

        let store = Arc::new(db::Store::new());
        if user_id == DEMO_USER_ID {
            // Demo user (synthetic portfolio, memos and goals — same as `goal demo`).
            let p = demo_portfolio();
            store.restore(DEMO_USER_ID, p);
        } else {
            for (uid, data) in
                db::SqliteStore::load_all_users(&config.sqlite_db).unwrap_or_default()
            {
                if let Ok(p) = store.decode_backup(&data) {
                    store.restore(uid, p);
                }
            }
            for (uid, list) in
                db::SqliteStore::load_user_memos(&config.sqlite_db).unwrap_or_default()
            {
                store.write(uid, |p| p.memos = list);
            }
            for (uid, list) in
                db::SqliteStore::load_user_goals(&config.sqlite_db).unwrap_or_default()
            {
                store.write(uid, |p| p.goals = list);
            }
        }

        let cmc = Arc::new(crypto::CryptoClient::new(
            &config.cmc_api_key,
            &config.coingecko_api_key,
            &config.coincap_api_key,
            &config.cryptocompare_api_key,
            Some(sqlite.clone()),
        ));

        bot::handlers::ask::run_cli_ask(user_id, &question, store, cmc, config, sqlite).await?;
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config = Arc::new(config::Config::from_env()?);
    tracing::info!("Starting Cryptograph bot");
    tracing::info!("SQLite database path: {}", config.sqlite_db);

    let store = Arc::new(db::Store::new());

    // Ensure parent directory exists (important for Docker volume mounts)
    if let Some(parent) = std::path::Path::new(&config.sqlite_db).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!("Could not create SQLite dir {:?}: {}", parent, e);
            }
        }
    }

    // Initialize SQLite schema before loading data
    if let Err(e) = db::SqliteStore::initialize_schema(&config.sqlite_db) {
        tracing::error!("Failed to initialize SQLite schema: {}", e);
    }

    // Attempt to load existing users from SQLite
    let users_data = db::SqliteStore::load_all_users(&config.sqlite_db).unwrap_or_else(|e| {
        tracing::warn!("Failed to load users from SQLite: {}", e);
        vec![]
    });
    for (uid, data) in users_data {
        if let Ok(portfolio) = store.decode_backup(&data) {
            store.restore(uid, portfolio);
        } else {
            tracing::warn!("Failed to decode portfolio for user {} from SQLite", uid);
        }
    }
    tracing::info!("Restored {} users from SQLite", store.all_user_ids().len());

    // Restore memos/goals from their dedicated tables — they survive even if the
    // generic users table is ever rebuilt (goals/memos are user data, not cache).
    if let Ok(memos) = db::SqliteStore::load_user_memos(&config.sqlite_db) {
        for (uid, list) in memos {
            store.write(uid, |p| p.memos = list);
        }
    }
    if let Ok(goals) = db::SqliteStore::load_user_goals(&config.sqlite_db) {
        for (uid, list) in goals {
            store.write(uid, |p| p.goals = list);
        }
    }

    let sqlite = Arc::new(db::SqliteStore::new(&config.sqlite_db)?);

    // Self-heal stats on upgrade: drop rows for models that no longer exist
    // in models.json (e.g. removed Vercel/dead models) so stale rows don't
    // accumulate in /stats forever.
    {
        let valid: Vec<String> = crate::crypto::ai::providers::available_models()
            .iter()
            .map(|m| format!("{:?}/{}", m.provider, m.id))
            .collect();
        match sqlite.prune_stale_metrics(&valid) {
            Ok(n) if n > 0 => tracing::info!("Pruned {n} stale metrics rows on upgrade"),
            _ => {}
        }
    }

    // Wire the paid DeepSeek budget to SQLite (falls back to RAM-only on DB failure).
    crate::crypto::ai::deepseek_budget::DeepSeekBudget::instance().init(Some(sqlite.clone()));

    // Wire the OpenRouter daily budget (300 requests/day free tier) to SQLite.
    crate::crypto::ai::openrouter_budget::OpenRouterBudget::instance().init(Some(sqlite.clone()));

    let acl = Arc::new(access::AccessControl::new(config.allowed_admin_ids.clone()));

    let cmc = Arc::new(crypto::CryptoClient::new(
        &config.cmc_api_key,
        &config.coingecko_api_key,
        &config.coincap_api_key,
        &config.cryptocompare_api_key,
        Some(Arc::clone(&sqlite)),
    ));

    let blockchain = Arc::new(crypto::BlockchainClient::new(
        &config.helius_api_key,
        &config.ankr_rpc_url,
    ));
    let bot = teloxide::Bot::new(&config.telegram_token);

    // Watchdog: RSS limit + hang detection (process-level guardian, not restarted)
    let max_rss = std::env::var("MAX_RSS_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512);
    let hang_secs = std::env::var("HANG_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    let watchdog = watchdog::Watchdog::new(max_rss, hang_secs);
    let pinger = watchdog.pinger();
    tokio::spawn(watchdog.run());

    // Heartbeat — proves the async runtime is alive to the watchdog
    supervisor::spawn_infallible("heartbeat", move || {
        let p = pinger.clone();
        async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                ticker.tick().await;
                p.ping();
            }
        }
    });

    // Alert engine — supervised: auto-restarts on panic or error
    {
        let bot2 = bot.clone();
        let store2 = Arc::clone(&store);
        let cmc2 = Arc::clone(&cmc);
        let cfg2 = Arc::clone(&config);
        supervisor::spawn("alert-engine", move || {
            let b = bot2.clone();
            let s = Arc::clone(&store2);
            let c = Arc::clone(&cmc2);
            let cfg = Arc::clone(&cfg2);
            async move {
                alerts::run_alert_engine(b, s, c, cfg).await;
                Ok(())
            }
        });
    }

    // AI market watcher — checks every 6h, notifies on significant events
    let bot_watcher = bot.clone();
    let store_watcher = Arc::clone(&store);
    let cmc_watcher = Arc::clone(&cmc);
    let sqlite_watcher = Arc::clone(&sqlite);
    let config_watcher = Arc::clone(&config);
    supervisor::spawn_infallible("ai-watcher", move || {
        let bot2 = bot_watcher.clone();
        let store2 = Arc::clone(&store_watcher);
        let cmc2 = Arc::clone(&cmc_watcher);
        let sqlite2 = Arc::clone(&sqlite_watcher);
        let config2 = Arc::clone(&config_watcher);
        async move {
            crypto::ai::watcher::run(bot2, store2, cmc2, sqlite2, config2).await;
        }
    });

    // 5) News worker (Dual-Agent Summarizer Pipeline)
    let store_news = Arc::clone(&store);
    let sqlite_news = Arc::clone(&sqlite);
    let cmc_news = Arc::clone(&cmc);
    supervisor::spawn_infallible("news-worker", move || {
        let store2 = Arc::clone(&store_news);
        let sqlite2 = Arc::clone(&sqlite_news);
        let cmc2 = Arc::clone(&cmc_news);
        async move {
            crypto::ai::news_worker::run_news_worker(store2, sqlite2, cmc2).await;
        }
    });

    // 6) Macro worker (Global AI Summary)
    let sqlite_macro = Arc::clone(&sqlite);
    let cmc_macro = Arc::clone(&cmc);
    supervisor::spawn_infallible("macro-worker", move || {
        let sqlite2 = Arc::clone(&sqlite_macro);
        let cmc2 = Arc::clone(&cmc_macro);
        async move {
            crypto::ai::macro_worker::run_macro_worker(sqlite2, cmc2).await;
        }
    });

    // 7) Market Intelligence worker (BTC network, derivatives, stablecoin flows — every 30min)
    let sqlite_intel = Arc::clone(&sqlite);
    let cmc_intel = Arc::clone(&cmc);
    supervisor::spawn_infallible("market-intel-worker", move || {
        let sqlite2 = Arc::clone(&sqlite_intel);
        let cmc2 = Arc::clone(&cmc_intel);
        async move {
            crypto::ai::market_intel_worker::run_market_intel_worker(sqlite2, cmc2).await;
        }
    });

    // 8) Proactive Goal/Memo worker — scans market on schedule, alerts when a
    //    user's goal conditions match (double-checked: 2 gens → reviewer → critic).
    let store_pro = Arc::clone(&store);
    let sqlite_pro = Arc::clone(&sqlite);
    let cmc_pro = Arc::clone(&cmc);
    let bot_pro = bot.clone();
    let config_pro = Arc::clone(&config);
    supervisor::spawn_infallible("proactive-worker", move || {
        let b = bot_pro.clone();
        let s = Arc::clone(&store_pro);
        let c = Arc::clone(&cmc_pro);
        let sq = Arc::clone(&sqlite_pro);
        let cfg = Arc::clone(&config_pro);
        async move {
            crypto::ai::proactive_worker::run(b, s, c, sq, cfg).await;
        }
    });

    // 9) Model ranker — refreshes the registry (all providers + OpenRouter free
    //    tier) and runs ONE lazy benchmark batch per day: at most 3 models, one
    //    role each, 30-day cooldown, fingerprint change detection, and a hard
    //    stop when the OpenRouter budget drops below the reserve. Writes
    //    models.openrouter.json (merged into the pools by load_all_models).
    let sqlite_rank = Arc::clone(&sqlite);
    supervisor::spawn_infallible("model-ranker", move || {
        let sq = Arc::clone(&sqlite_rank);
        async move {
            let client = reqwest::Client::new();
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(86_400));
            loop {
                ticker.tick().await;
                match crate::crypto::ai::model_rank::discover(&client, &sq).await {
                    Ok(r) => {
                        tracing::info!("Model registry refresh: {} total, {} new", r.total, r.new)
                    }
                    Err(e) => tracing::warn!("Model registry refresh failed: {}", e),
                }
                match crate::crypto::ai::model_rank::rank(&client, &sq).await {
                    Ok(r) => tracing::info!(
                        "Model ranker: tested {}, skipped {}, assigned {}, judge baseline {:.1}",
                        r.tested.len(),
                        r.skipped_tested,
                        r.assigned.len(),
                        r.judge_baseline
                    ),
                    Err(e) => tracing::warn!("Model ranker failed: {}", e),
                }
            }
        }
    });

    // Periodic price pre-fetch — supervised: auto-restarts on panic or error
    {
        let store2 = Arc::clone(&store);
        let cmc2 = Arc::clone(&cmc);
        let sqlite_prefetch = Arc::clone(&sqlite);
        supervisor::spawn_infallible("price-prefetch", move || {
            let store3 = Arc::clone(&store2);
            let cmc3 = Arc::clone(&cmc2);
            let sq = Arc::clone(&sqlite_prefetch);
            async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(600));
                // Daily OHLC accumulator: open = first sample of the UTC day,
                // high/low = running extremes, close = latest sample. Feeds the
                // local OHLCV table so chart indicators see real candles.
                let mut daily_ohlc: std::collections::HashMap<String, (String, f64, f64, f64)> =
                    std::collections::HashMap::new();
                loop {
                    ticker.tick().await;
                    tracing::info!("Starting periodic crypto price pre-fetch...");

                    let active_symbols = store3.get_all_active_symbols();
                    let sym_refs: Vec<&str> = active_symbols.iter().map(|s| s.as_str()).collect();
                    if !sym_refs.is_empty() {
                        match cmc3.get_quotes(&sym_refs).await {
                            Ok(q) => {
                                tracing::info!("Pre-fetched {} quotes successfully", q.len());
                                for quote in q.iter() {
                                    // Guard against bad samples: a price that jumps
                                    // >50% from the last logged sample (within 24h) is
                                    // almost always a provider glitch, not a real move.
                                    // Logging it would poison local-history charts with
                                    // fake x2 spikes.
                                    let mut ok = true;
                                    if let Ok(h) = sq.get_price_history(&quote.symbol, 1).await {
                                        if let Some((_, prev)) = h.last() {
                                            if *prev > 0.0 {
                                                let dev = (quote.price_usd - *prev).abs() / *prev;
                                                if dev > 0.5 {
                                                    ok = false;
                                                    tracing::warn!(
                                                        "Skipping suspicious price for {}: {:.2} vs prev {:.2} ({}% jump)",
                                                        quote.symbol, quote.price_usd, prev, dev * 100.0
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    if ok {
                                        let today =
                                            chrono::Utc::now().format("%Y-%m-%d").to_string();
                                        let e = daily_ohlc
                                            .entry(quote.symbol.clone())
                                            .or_insert_with(|| {
                                                (
                                                    today.clone(),
                                                    quote.price_usd,
                                                    quote.price_usd,
                                                    quote.price_usd,
                                                )
                                            });
                                        if e.0 != today {
                                            *e = (
                                                today.clone(),
                                                quote.price_usd,
                                                quote.price_usd,
                                                quote.price_usd,
                                            );
                                        }
                                        e.2 = e.2.max(quote.price_usd); // high
                                        e.3 = e.3.min(quote.price_usd); // low
                                        sq.log_ohlcv(&quote.symbol, quote.price_usd, e.1, e.2, e.3)
                                            .await;
                                    }
                                }
                            }
                            Err(e) => tracing::warn!("Failed to pre-fetch quotes: {}", e),
                        }
                    }

                    for limit in [10u32, 20, 50] {
                        match cmc3.get_top(limit).await {
                            Ok(t) => tracing::info!("Pre-fetched top {} successfully", t.len()),
                            Err(e) => tracing::warn!("Failed to pre-fetch top {}: {}", limit, e),
                        }
                    }
                }
            }
        });
    }

    // Milestone warming worker: fills price_milestones for symbols missing data,
    // then refreshes daily so AI always has concrete historical price anchors.
    {
        let cmc_milestones = Arc::clone(&cmc);
        let store_milestones = Arc::clone(&store);
        supervisor::spawn_infallible("milestone-warmer", move || {
            let cmc2 = Arc::clone(&cmc_milestones);
            let store2 = Arc::clone(&store_milestones);
            async move {
                // At startup: warm BTC, ETH plus all user portfolio symbols
                let core_symbols = vec![
                    "BTC".to_string(),
                    "ETH".to_string(),
                    "SOL".to_string(),
                    "XRP".to_string(),
                ];
                let user_symbols = store2.get_all_active_symbols();
                let all_symbols: Vec<String> = core_symbols
                    .into_iter()
                    .chain(user_symbols.into_iter())
                    .collect::<std::collections::HashSet<String>>()
                    .into_iter()
                    .collect();
                tracing::info!(
                    "Warming price milestones for {} symbols...",
                    all_symbols.len()
                );
                cmc2.warm_milestones(&all_symbols).await;

                // Then refresh every 24h to keep milestones current
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(86_400));
                loop {
                    ticker.tick().await;
                    let user_syms = store2.get_all_active_symbols();
                    tracing::info!(
                        "Daily milestone refresh for {} user symbols",
                        user_syms.len()
                    );
                    cmc2.warm_milestones(&user_syms).await;
                }
            }
        });
    }

    // Periodic SQLite Flusher
    {
        let store_flush = Arc::clone(&store);
        let sqlite_flush = Arc::clone(&sqlite);
        let cmc_flush = Arc::clone(&cmc);
        supervisor::spawn_infallible("sqlite-flusher", move || {
            let s = Arc::clone(&store_flush);
            let sq = Arc::clone(&sqlite_flush);
            let c = Arc::clone(&cmc_flush);
            async move {
                // Update cache immediately on startup
                crate::bot::handlers::settings::update_stats_cache(&sq).await;

                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300)); // 5 minutes
                loop {
                    ticker.tick().await;
                    for uid in s.all_user_ids() {
                        if let Ok(data) = s.export_backup(uid) {
                            sq.save_user(uid, data).await;
                        }
                        // Keep the dedicated memos/goals tables in sync with the
                        // store (they survive even if the users table is rebuilt).
                        let (memos, goals) = s.read(uid, |p| (p.memos.clone(), p.goals.clone()));
                        sq.save_user_memos(uid, &memos).await;
                        sq.save_user_goals(uid, &goals).await;
                    }

                    // Flush AI and Data Source stats
                    let mut deltas = crypto::ai::metrics::take_deltas();
                    deltas.extend(c.take_health_deltas());
                    deltas.extend(crypto::ai::metrics::take_search_deltas());

                    if !deltas.is_empty() {
                        sq.flush_daily_metrics(deltas).await;
                    }

                    // Update the memory cache AFTER flushing to SQLite
                    crate::bot::handlers::settings::update_stats_cache(&sq).await;

                    // Periodically trigger background cleanup of old records
                    sq.cleanup_old_events();
                }
            }
        });
    }

    // Bot dispatcher — blocks until Ctrl+C
    bot::start(bot, store, cmc, blockchain, acl, config, sqlite).await;
    Ok(())
}
