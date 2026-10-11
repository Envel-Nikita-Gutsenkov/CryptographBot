use chrono::Utc;
use moka::future::Cache;
use std::sync::OnceLock;
use std::time::Duration;

use super::pipeline::call_with_critic;
use super::prompts::{
    build_full_prompt, build_multi_prompt, build_watcher_prompt, parse_watcher_response,
    WatcherResult, WatcherSeverity,
};

static ANALYSIS_CACHE: OnceLock<Cache<String, String>> = OnceLock::new();

fn get_analysis_cache() -> Cache<String, String> {
    ANALYSIS_CACHE
        .get_or_init(|| {
            Cache::builder()
                .time_to_live(Duration::from_secs(3600)) // 1 hour TTL
                .max_capacity(500)
                .build()
        })
        .clone()
}

pub struct GeminiClient {
    api_key: String,
}

impl GeminiClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.api_key.is_empty()
    }

    fn build_http_client(timeout_secs: u64) -> anyhow::Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_secs))
            .use_rustls_tls()
            .build()?)
    }

    pub async fn generate_analysis(
        &self,
        symbol: &str,
        market_data: &str,
        news_summary: &str,
        preferred_model: Option<&str>,
        portfolio_context: Option<&str>,
        db: std::sync::Arc<crate::db::sqlite::SqliteStore>,
    ) -> anyhow::Result<String> {
        let cache = get_analysis_cache();
        let cache_key = format!(
            "single_{}_{}_{}_{}",
            symbol,
            preferred_model.unwrap_or(""),
            portfolio_context.is_some(),
            news_summary.len()
        );

        if let Some(text) = cache.get(&cache_key).await {
            return Ok(text);
        }

        let portfolio_section = portfolio_context
            .map(|ctx| {
                format!("\n\nUser Portfolio Allocation (reference only, no exact amounts):\n{ctx}")
            })
            .unwrap_or_default();

        let prompt = build_full_prompt(symbol, market_data, news_summary, &portfolio_section);

        if std::env::var("DEBUG_PROMPT")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false)
        {
            println!("\n=== [DEBUG] PROMPT ===\n{prompt}\n=====================\n");
        }

        let client = Self::build_http_client(45)?; // Extended timeout for critic loop
        let text = call_with_critic(
            &client,
            &self.api_key,
            &prompt,
            preferred_model,
            false,
            true,
            db,
        )
        .await?;

        cache.insert(cache_key, text.clone()).await;
        Ok(text)
    }

    pub async fn generate_multi_analysis(
        &self,
        assets_data: &str,
        preferred_model: Option<&str>,
        portfolio_context: Option<&str>,
        db: std::sync::Arc<crate::db::sqlite::SqliteStore>,
    ) -> anyhow::Result<String> {
        let port_section = if let Some(p) = portfolio_context {
            format!("\nUser's Portfolio Context:\n{p}")
        } else {
            "".to_string()
        };
        let prompt = build_multi_prompt(assets_data, &port_section);

        let cache = get_analysis_cache();
        let cache_key = format!("multi_{}_{}", Utc::now().format("%Y%m%d_%H"), prompt.len());

        if let Some(text) = cache.get(&cache_key).await {
            return Ok(text);
        }

        let client = Self::build_http_client(45)?;
        let text = call_with_critic(
            &client,
            &self.api_key,
            &prompt,
            preferred_model,
            false,
            true,
            db,
        )
        .await?;

        cache.insert(cache_key, text.clone()).await;
        Ok(text)
    }

    pub async fn watcher_analysis(
        &self,
        symbols_data: &str,
        previous_analysis: Option<&str>,
        user_context: Option<&str>,
        db: std::sync::Arc<crate::db::sqlite::SqliteStore>,
    ) -> anyhow::Result<WatcherResult> {
        let current_date = Utc::now().format("%Y-%m-%d").to_string();
        let prompt = build_watcher_prompt(symbols_data, previous_analysis, &current_date, user_context);
        let client = Self::build_http_client(45)?;

        let raw =
            match call_with_critic(&client, &self.api_key, &prompt, None, true, false, db).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Watcher analysis critic loop failed: {e}");
                    return Ok(WatcherResult {
                        severity: WatcherSeverity::None,
                        headline: String::new(),
                        detail: String::new(),
                    });
                }
            };

        Ok(parse_watcher_response(&raw))
    }

    #[allow(dead_code)]
    pub async fn generate_alert_comment(
        &self,
        quote: &crate::models::CmcQuote,
        condition: &str,
        threshold: f64,
        label: Option<&str>,
        db: std::sync::Arc<crate::db::sqlite::SqliteStore>,
    ) -> anyhow::Result<String> {
        let label_str = label.map(|l| format!("(Note: {})", l)).unwrap_or_default();
        let prompt = format!(
            "You are an active crypto assistant. A price alert has just fired for {symbol}.\n\
             Alert Details: condition is {condition}, threshold is {threshold}, current price is {price}.\n\
             User custom note: {label_str}\n\n\
             Market Context for {symbol}:\n\
             - 1h change: {change_1h:.2}%\n\
             - 24h change: {change_24h:.2}%\n\
             - 7d change: {change_7d:.2}%\n\
             - 30d change: {change_30d:.2}%\n\
             - 24h volume: ${volume_24h:.0}\n\n\
             Write a very short, professional, and direct comment/action recommendation for the user in English.\n\
             Focus on explaining the significance of the move (e.g. if it's a massive crash or a minor correction) and what they should consider doing (e.g. buy the dip, take profit, hold, or prepare to sell).\n\
             Keep it under 2 sentences. Do NOT use any formatting like markdown asterisks, bold tags, or italic tags. Keep it plain text.",
            symbol = quote.symbol,
            condition = condition,
            threshold = threshold,
            price = quote.price_usd,
            label_str = label_str,
            change_1h = quote.change_1h.unwrap_or(0.0),
            change_24h = quote.change_24h.unwrap_or(0.0),
            change_7d = quote.change_7d.unwrap_or(0.0),
            change_30d = quote.change_30d.unwrap_or(0.0),
            volume_24h = quote.volume_24h,
        );
        let client = Self::build_http_client(20)?;
        let text =
            call_with_critic(&client, &self.api_key, &prompt, None, false, false, db).await?;
        Ok(text.trim().to_string())
    }
}
