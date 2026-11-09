use std::sync::Arc;
use teloxide::{dispatching::dialogue::InMemStorage, prelude::*, types::ParseMode};
use tracing::info;

use crate::{
    bot::state::State,
    config::Config,
    crypto::{
        ai::{fetch_news_for_symbol, GeminiClient},
        CryptoClient,
    },
    db::Store,
};

type MyDialogue = Dialogue<State, InMemStorage<State>>;

pub async fn handle_ai_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    if !config.is_admin(msg.chat.id.0 as u64) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }

    let trimmed = args.trim();
    if trimmed.is_empty() {
        return perform_multi_ai_analysis(bot, msg.chat.id, cmc, config, store).await;
    }

    if trimmed.to_lowercase().starts_with("set ") {
        let symbols: Vec<String> = trimmed[4..]
            .split_whitespace()
            .map(|s| s.to_uppercase())
            .collect();

        if symbols.is_empty() {
            bot.send_message(msg.chat.id, "Usage: `/ai set BTC ETH SOL`")
                .await?;
            return Ok(());
        }

        let user_id = msg.chat.id.0 as u64;
        store.write(user_id, |u| u.settings.ai_multi_assets = symbols.clone());
        crate::bot::handlers::portfolio::auto_backup(&bot, &store, user_id).await;
        bot.send_message(
            msg.chat.id,
            format!("✅ AI multi-asset list updated: {}", symbols.join(", ")),
        )
        .await?;
        return Ok(());
    }

    // Free-text input (spaces, punctuation, non-ASCII) is a question, not a
    // symbol — route it to /ask instead of failing with "Symbol ... not found".
    let looks_like_symbol = !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !looks_like_symbol {
        bot.send_message(
            msg.chat.id,
            "💬 <b>AI Chat</b>\n\n`/ai` analyzes a single coin by ticker (e.g. <code>/ai BTC</code>).\nFor questions, use <code>/ask your question</code> — it has full portfolio + market context.",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    let symbol = trimmed.to_uppercase();
    perform_ai_analysis(bot, msg.chat.id, symbol, cmc, config, store).await
}

pub async fn handle_ai_callback(
    bot: Bot,
    chat_id: ChatId,
    symbol: String,
    _period: String,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    store: Arc<Store>,
) -> anyhow::Result<()> {
    perform_ai_analysis(bot, chat_id, symbol, cmc, config, store).await
}

pub async fn handle_ai_portfolio_callback(
    bot: Bot,
    msg: Message,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    store: Arc<Store>,
) -> anyhow::Result<()> {
    perform_portfolio_ai_analysis(bot, msg.chat.id, cmc, config, store).await
}

/// Sanitize AI-generated text for Telegram: strip codeblock wrappers and
/// replace `$` with `USD` (avoids breaking Telegram's LaTeX math parser).
fn sanitize_ai_text(text: &mut String) {
    // 1. Remove markdown codeblock wrappers if the AI wrapped the entire response
    if text.starts_with("```markdown") {
        *text = text.trim_start_matches("```markdown").to_string();
    } else if text.starts_with("```") {
        *text = text.trim_start_matches("```").to_string();
    }
    if text.ends_with("```") {
        *text = text.trim_end_matches("```").to_string();
    }
    *text = text.trim().to_string();

    // 2. Replace $ with USD to avoid breaking Telegram's LaTeX math parser
    *text = text.replace("$", "USD ");
}

#[allow(deprecated)]
pub async fn send_rich_message_via_api(
    bot: &Bot,
    config: Arc<Config>,
    chat_id: ChatId,
    mut text: String,
) -> anyhow::Result<()> {
    sanitize_ai_text(&mut text);

    // 3. Send using standard Teloxide MarkdownV1
    // MarkdownV1 is much more forgiving than MarkdownV2
    let chars: Vec<char> = text.chars().collect();
    let mut current_idx = 0;
    while current_idx < chars.len() {
        let mut end_idx = current_idx + 30000;
        if end_idx >= chars.len() {
            end_idx = chars.len();
        } else {
            // Try to break at a double newline first to avoid breaking markdown blocks/tables
            let mut found_split = false;

            // Look for double newline in the last 1000 chars of the chunk
            let search_start = current_idx.max(end_idx.saturating_sub(1000));
            for i in (search_start..end_idx - 1).rev() {
                if chars[i] == '\n' && chars[i + 1] == '\n' {
                    end_idx = i;
                    found_split = true;
                    break;
                }
            }

            if !found_split {
                // Fallback to single newline
                for i in (current_idx..end_idx).rev() {
                    if chars[i] == '\n' {
                        end_idx = i;
                        break;
                    }
                }
            }
        }

        let chunk: String = chars[current_idx..end_idx].iter().collect();

        let res =
            crate::bot::rich_messages::send_rich_message_markdown(&config, chat_id.0, &chunk).await;
        if let Err(e) = res {
            tracing::warn!(
                "Rich message chunk send failed: {}. Retrying as plain text...",
                e
            );
            if let Err(plain_e) = bot.send_message(chat_id, chunk).await {
                tracing::error!("Plain text chunk send failed too: {}", plain_e);
            }
        }

        current_idx = end_idx;
        while current_idx < chars.len() && chars[current_idx] == '\n' {
            current_idx += 1;
        }
    }

    Ok(())
}

/// Send a sanitized AI message with an inline keyboard (used for goal
/// proposals that need user confirmation). Proposals are short single messages,
/// so this uses the standard Bot API path (the rich-message endpoint has no
/// reply-markup support).
pub async fn send_rich_message_via_api_with_markup(
    bot: &Bot,
    chat_id: ChatId,
    mut text: String,
    markup: teloxide::types::InlineKeyboardMarkup,
) -> anyhow::Result<()> {
    sanitize_ai_text(&mut text);
    bot.send_message(chat_id, text).reply_markup(markup).await?;
    Ok(())
}

#[allow(deprecated)]
async fn perform_multi_ai_analysis(
    bot: Bot,
    chat_id: ChatId,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    store: Arc<Store>,
) -> anyhow::Result<()> {
    let user_id = chat_id.0 as u64;
    let ai_client = GeminiClient::new(&config.gemini_api_key);
    if !ai_client.is_configured() {
        bot.send_message(chat_id, "❌ API KEY not configured.")
            .await?;
        return Ok(());
    }

    let symbols = store.read(user_id, |p| p.settings.ai_multi_assets.clone());
    if symbols.is_empty() {
        bot.send_message(chat_id, "❌ Multi-asset list is empty. Use /ai set BTC ETH")
            .await?;
        return Ok(());
    }

    let loading_msg = bot
        .send_message(
            chat_id,
            format!(
                "🧠 Analyzing multiple assets: <b>{}</b>…",
                symbols.join(", ")
            ),
        )
        .parse_mode(ParseMode::Html)
        .await?;

    let symbols_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = match cmc.get_quotes(&symbols_refs).await {
        Ok(q) if !q.is_empty() => q,
        _ => {
            let _ = bot.delete_message(chat_id, loading_msg.id).await;
            bot.send_message(chat_id, "❌ Failed to fetch market data.")
                .await?;
            return Ok(());
        }
    };

    let mut assets_data_str = String::new();
    let fmt = |v: Option<f64>| v.map(|x| format!("{:.2}%", x)).unwrap_or("N/A".into());

    for quote in quotes {
        let (c30, rsi_14) = tokio::join!(
            calculate_historical_change(&cmc, &quote.symbol, "30"),
            calculate_rsi(&cmc, &quote.symbol, "14"),
        );
        let rsi_str = rsi_14.map(|r| format!("{:.1}", r)).unwrap_or("N/A".into());
        assets_data_str.push_str(&format!(
            "[{}] Price: ${:.2} | 24h: {:.2}% | 7d: {:.2}% | 30d: {} | RSI: {} | MCap: ${:.0}\n",
            quote.symbol,
            quote.price_usd,
            quote.change_24h.unwrap_or(0.0),
            quote.change_7d.unwrap_or(0.0),
            fmt(c30),
            rsi_str,
            quote.market_cap
        ));
    }

    let preferred_model = store.read(user_id, |p| p.settings.ai_model_override.clone());
    let portfolio_context = build_portfolio_context(&store, user_id, &cmc).await;
    let pref_ref = preferred_model.as_deref();
    let port_ref = portfolio_context.as_deref();

    match ai_client
        .generate_multi_analysis(&assets_data_str, pref_ref, port_ref, cmc.sqlite().unwrap())
        .await
    {
        Ok(analysis) => {
            let _ = bot.delete_message(chat_id, loading_msg.id).await;
            let _ = send_rich_message_via_api(&bot, config.clone(), chat_id, analysis).await;
        }
        Err(e) => {
            let _ = bot.delete_message(chat_id, loading_msg.id).await;
            bot.send_message(
                chat_id,
                format!(
                    "❌ AI analysis failed: {}",
                    crate::format::redact_secrets(&e.to_string())
                ),
            )
            .await?;
        }
    }
    Ok(())
}

async fn perform_portfolio_ai_analysis(
    bot: Bot,
    chat_id: ChatId,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    store: Arc<Store>,
) -> anyhow::Result<()> {
    let user_id = chat_id.0 as u64;
    let ai_client = GeminiClient::new(&config.gemini_api_key);
    if !ai_client.is_configured() {
        bot.send_message(chat_id, "❌ API KEY not configured.")
            .await?;
        return Ok(());
    }

    let symbols: Vec<String> =
        store.read(user_id, |p| p.holdings_by_symbol().into_keys().collect());

    if symbols.is_empty() {
        bot.send_message(chat_id, "❌ Your portfolio is empty. Add assets first!")
            .await?;
        return Ok(());
    }

    let loading_msg = bot
        .send_message(
            chat_id,
            format!(
                "🧠 Analyzing your portfolio assets: <b>{}</b>…",
                symbols.join(", ")
            ),
        )
        .parse_mode(ParseMode::Html)
        .await?;

    let symbols_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let quotes = match cmc.get_quotes(&symbols_refs).await {
        Ok(q) if !q.is_empty() => q,
        _ => {
            let _ = bot.delete_message(chat_id, loading_msg.id).await;
            bot.send_message(chat_id, "❌ Failed to fetch market data.")
                .await?;
            return Ok(());
        }
    };

    let mut assets_data_str = String::new();
    let fmt = |v: Option<f64>| v.map(|x| format!("{:.2}%", x)).unwrap_or("N/A".into());

    for quote in quotes {
        let (c30, rsi_14) = tokio::join!(
            calculate_historical_change(&cmc, &quote.symbol, "30"),
            calculate_rsi(&cmc, &quote.symbol, "14"),
        );
        let rsi_str = rsi_14.map(|r| format!("{:.1}", r)).unwrap_or("N/A".into());
        assets_data_str.push_str(&format!(
            "[{}] Price: ${:.2} | 24h: {:.2}% | 7d: {:.2}% | 30d: {} | RSI: {} | MCap: ${:.0}\n",
            quote.symbol,
            quote.price_usd,
            quote.change_24h.unwrap_or(0.0),
            quote.change_7d.unwrap_or(0.0),
            fmt(c30),
            rsi_str,
            quote.market_cap
        ));
    }

    let preferred_model = store.read(user_id, |p| p.settings.ai_model_override.clone());
    let portfolio_context = build_portfolio_context(&store, user_id, &cmc).await;
    let pref_ref = preferred_model.as_deref();
    let port_ref = portfolio_context.as_deref();

    match ai_client
        .generate_multi_analysis(&assets_data_str, pref_ref, port_ref, cmc.sqlite().unwrap())
        .await
    {
        Ok(analysis) => {
            let _ = bot.delete_message(chat_id, loading_msg.id).await;
            let _ = send_rich_message_via_api(&bot, config.clone(), chat_id, analysis).await;
        }
        Err(e) => {
            let _ = bot.delete_message(chat_id, loading_msg.id).await;
            bot.send_message(
                chat_id,
                format!(
                    "❌ AI portfolio analysis failed: {}",
                    crate::format::redact_secrets(&e.to_string())
                ),
            )
            .await?;
        }
    }
    Ok(())
}

async fn perform_ai_analysis(
    bot: Bot,
    chat_id: ChatId,
    symbol: String,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    store: Arc<Store>,
) -> anyhow::Result<()> {
    let user_id = chat_id.0 as u64;
    let ai_client = GeminiClient::new(&config.gemini_api_key);
    if !ai_client.is_configured() {
        bot.send_message(chat_id, "❌ GEMINI_API_KEY not configured.")
            .await?;
        return Ok(());
    }

    let preferred_model = store.read(user_id, |p| p.settings.ai_model_override.clone());
    let portfolio_context = build_portfolio_context(&store, user_id, &cmc).await;

    let loading_msg = bot
        .send_message(chat_id, format!("🧠 Analyzing <b>{}</b>…", symbol))
        .parse_mode(ParseMode::Html)
        .await?;

    let quotes = match cmc.get_quotes(&[&symbol]).await {
        Ok(q) if !q.is_empty() => q,
        Ok(_) => {
            let _ = bot.delete_message(chat_id, loading_msg.id).await;
            bot.send_message(chat_id, format!("❌ Symbol {} not found.", symbol))
                .await?;
            return Ok(());
        }
        Err(e) => {
            let _ = bot.delete_message(chat_id, loading_msg.id).await;
            bot.send_message(
                chat_id,
                format!(
                    "❌ CMC error: {}",
                    crate::format::redact_secrets(&e.to_string())
                ),
            )
            .await?;
            return Ok(());
        }
    };
    let quote = &quotes[0];

    let (c30, c90, c180, c365, rsi_14, global_res, fear_greed, news) = tokio::join!(
        calculate_historical_change(&cmc, &symbol, "30"),
        calculate_historical_change(&cmc, &symbol, "90"),
        calculate_historical_change(&cmc, &symbol, "180"),
        calculate_historical_change(&cmc, &symbol, "365"),
        calculate_rsi(&cmc, &symbol, "14"),
        cmc.get_global_metrics(),
        fetch_fear_and_greed_index(),
        fetch_news_for_symbol(&symbol, &quote.name, 15),
    );

    let news = news.unwrap_or_else(|e| {
        info!("News fetch: {e}");
        "No recent news.".into()
    });

    let global_info = global_res
        .map(|g| {
            format!(
                "Total Market Cap: ${:.0}\n24h Volume: ${:.0}\nBTC Dom: {:.2}%\nETH Dom: {:.2}%",
                g.total_market_cap, g.total_volume_24h, g.btc_dominance, g.eth_dominance
            )
        })
        .unwrap_or_else(|_| "Unavailable".into());

    let fmt = |v: Option<f64>| v.map(|x| format!("{:.2}%", x)).unwrap_or("N/A".into());

    // Derived signals
    let vol_mcap_ratio = if quote.market_cap > 0.0 {
        format!("{:.2}%", quote.volume_24h / quote.market_cap * 100.0)
    } else {
        "N/A".into()
    };

    let momentum = match quote.change_7d.unwrap_or(0.0) {
        x if x > 15.0 => "Strong Uptrend",
        x if x > 5.0 => "Moderate Uptrend",
        x if x > -5.0 => "Sideways",
        x if x > -15.0 => "Moderate Downtrend",
        _ => "Strong Downtrend",
    };

    let rsi_str = rsi_14.map(|r| format!("{:.1}", r)).unwrap_or("N/A".into());

    let market_data = format!(
        "Coin: {} ({})\nPrice: ${:.2}\nRank: #{}\n\n\
         Price Changes:\n1h: {:.2}%  24h: {:.2}%  7d: {:.2}%\n30d: {}  90d: {}  180d: {}  365d: {}\n\n\
         Market Stats:\n24h Volume: ${:.0}\nMarket Cap: ${:.0}\nVol/MCap Ratio: {}\n\
         Circulating Supply: {:.0} {}\n\n\
         Technical Indicators:\nMomentum (7d): {}\nRSI (14d approx): {}\n\n\
         Market Context:\nFear & Greed Index: {}\n{}",
        quote.name, quote.symbol, quote.price_usd, quote.rank,
        quote.change_1h.unwrap_or(0.0), quote.change_24h.unwrap_or(0.0), quote.change_7d.unwrap_or(0.0),
        fmt(c30), fmt(c90), fmt(c180), fmt(c365),
        quote.volume_24h, quote.market_cap, vol_mcap_ratio,
        quote.circulating_supply, quote.symbol, momentum, rsi_str,
        fear_greed, global_info
    );

    let pref_ref = preferred_model.as_deref();
    let port_ref = portfolio_context.as_deref();

    match ai_client
        .generate_analysis(
            &symbol,
            &market_data,
            &news,
            pref_ref,
            port_ref,
            cmc.sqlite().unwrap(),
        )
        .await
    {
        Ok(analysis) => {
            let _ = bot.delete_message(chat_id, loading_msg.id).await;
            let _ = send_rich_message_via_api(&bot, config.clone(), chat_id, analysis).await;
        }
        Err(e) => {
            let _ = bot.delete_message(chat_id, loading_msg.id).await;
            bot.send_message(
                chat_id,
                format!(
                    "❌ AI analysis failed: {}",
                    crate::format::redact_secrets(&e.to_string())
                ),
            )
            .await?;
        }
    }
    Ok(())
}

pub async fn run_cli_analysis(
    symbol: &str,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    let ai_client = GeminiClient::new(&config.gemini_api_key);
    if !ai_client.is_configured() {
        anyhow::bail!("GEMINI_API_KEY not configured");
    }
    std::env::set_var("DEBUG_PROMPT", "true");

    println!("🧠 Fetching market data for {symbol}…");
    let quotes = cmc.get_quotes(&[symbol]).await?;
    if quotes.is_empty() {
        anyhow::bail!("Symbol {symbol} not found");
    }
    let quote = &quotes[0];

    println!("📰 Fetching news…");
    let news = fetch_news_for_symbol(symbol, &quote.name, 15)
        .await
        .unwrap_or_default();

    println!("📊 Fetching metrics…");
    let (c30, c90, c180, c365, rsi_14, global_res, fear_greed) = tokio::join!(
        calculate_historical_change(&cmc, symbol, "30"),
        calculate_historical_change(&cmc, symbol, "90"),
        calculate_historical_change(&cmc, symbol, "180"),
        calculate_historical_change(&cmc, symbol, "365"),
        calculate_rsi(&cmc, symbol, "14"),
        cmc.get_global_metrics(),
        fetch_fear_and_greed_index(),
    );

    let fmt = |v: Option<f64>| v.map(|x| format!("{:.2}%", x)).unwrap_or("N/A".into());
    let global_info = global_res
        .map(|g| {
            format!(
                "Market Cap: ${:.0}  BTC Dom: {:.2}%",
                g.total_market_cap, g.btc_dominance
            )
        })
        .unwrap_or_default();

    let rsi_str = rsi_14.map(|r| format!("{:.1}", r)).unwrap_or("N/A".into());
    let market_data = format!(
        "Coin: {} ({})\nPrice: ${:.2}\n1h: {:.2}%  24h: {:.2}%  7d: {:.2}%\n30d: {}  90d: {}  180d: {}  365d: {}\n\
         Vol: ${:.0}  MCap: ${:.0}\nRSI(14d): {}  F&G: {}\n{}",
        quote.name, quote.symbol, quote.price_usd,
        quote.change_1h.unwrap_or(0.0), quote.change_24h.unwrap_or(0.0), quote.change_7d.unwrap_or(0.0),
        fmt(c30), fmt(c90), fmt(c180), fmt(c365),
        quote.volume_24h, quote.market_cap, rsi_str, fear_greed, global_info
    );

    println!("🤖 Calling AI…");
    let analysis = ai_client
        .generate_analysis(
            symbol,
            &market_data,
            &news,
            None,
            None,
            cmc.sqlite().unwrap(),
        )
        .await?;
    println!("\n=== AI ANALYSIS: {symbol} ===\n{analysis}");
    Ok(())
}

async fn build_portfolio_context(
    store: &Arc<Store>,
    user_id: u64,
    cmc: &Arc<CryptoClient>,
) -> Option<String> {
    let holdings = store.read(user_id, |p| p.holdings_by_symbol());
    if holdings.is_empty() {
        return None;
    }

    let total: f64 = holdings.values().sum();
    if total == 0.0 {
        return None;
    }

    let syms: Vec<&str> = holdings.keys().map(|s| s.as_str()).collect();
    let quotes = cmc.get_quotes(&syms).await.unwrap_or_default();
    let prices: std::collections::HashMap<_, _> = quotes
        .iter()
        .map(|q| (q.symbol.clone(), q.price_usd))
        .collect();

    // Compute value-based allocation
    let mut values: Vec<(String, f64)> = holdings
        .iter()
        .map(|(sym, &amt)| {
            let price = prices.get(sym).copied().unwrap_or(1.0);
            (sym.clone(), amt * price)
        })
        .collect();
    let total_val: f64 = values.iter().map(|(_, v)| v).sum();
    if total_val == 0.0 {
        return None;
    }

    values.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let parts: Vec<String> = values
        .iter()
        .map(|(sym, val)| format!("{}: {:.1}%", sym, val / total_val * 100.0))
        .collect();
    Some(parts.join(", "))
}

pub async fn fetch_fear_and_greed_index() -> String {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .use_rustls_tls()
        .build()
    {
        Ok(c) => c,
        Err(_) => return "Unknown".into(),
    };
    if let Ok(resp) = client
        .get("https://api.alternative.me/fng/?limit=1")
        .send()
        .await
    {
        if let Ok(val) = resp.json::<serde_json::Value>().await {
            if let Some(arr) = val["data"].as_array() {
                if let Some(item) = arr.first() {
                    let score = item["value"].as_str().unwrap_or("?");
                    let label = item["value_classification"].as_str().unwrap_or("?");
                    return format!("{score} ({label})");
                }
            }
        }
    }
    "Unknown".into()
}

async fn calculate_historical_change(cmc: &CryptoClient, symbol: &str, days: &str) -> Option<f64> {
    let points = cmc.get_history(symbol, days).await.ok()?;
    if points.len() < 2 {
        return None;
    }
    let start = points.first()?.1;
    let end = points.last()?.1;
    if start == 0.0 {
        return None;
    }
    Some((end - start) / start * 100.0)
}

async fn calculate_rsi(cmc: &CryptoClient, symbol: &str, days: &str) -> Option<f64> {
    let points = cmc.get_history(symbol, days).await.ok()?;
    if points.len() < 2 {
        return None;
    }

    let mut gains = 0.0;
    let mut losses = 0.0;
    let mut count = 0;

    for w in points.windows(2) {
        let diff = w[1].1 - w[0].1;
        if diff > 0.0 {
            gains += diff;
        } else {
            losses += diff.abs();
        }
        count += 1;
    }

    if count == 0 {
        return None;
    }
    let avg_gain = gains / count as f64;
    let avg_loss = losses / count as f64;

    if avg_loss == 0.0 {
        return Some(100.0);
    }
    let rs = avg_gain / avg_loss;
    Some(100.0 - (100.0 / (1.0 + rs)))
}

#[allow(deprecated)]
pub async fn handle_ai_wizard(
    bot: Bot,
    msg: Message,
    dialogue: MyDialogue,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    let text = msg.text().unwrap_or("").trim().to_uppercase();
    if text.is_empty() {
        bot.send_message(
            msg.chat.id,
            "Please enter a valid coin symbol (e.g. `SOL`):",
        )
        .reply_markup(crate::bot::keyboards::cancel_only())
        .await?;
        return Ok(());
    }
    dialogue.exit().await.map_err(|e| anyhow::anyhow!(e))?;
    perform_ai_analysis(bot, msg.chat.id, text, cmc, config, store).await
}
