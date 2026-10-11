use super::cryptocompare::NewsArticle;
use crate::error::{BotError, Result};
use reqwest::Client;
use std::time::Duration;

pub struct RssProvider {
    http: Client,
}

impl RssProvider {
    pub const NAME: &'static str = "rss";

    pub fn new() -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("Mozilla/5.0 (Windows NT 10.0; Win64; x64)"),
        );

        Self {
            http: Client::builder()
                .default_headers(headers)
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                .build()
                .unwrap(),
        }
    }

    pub async fn get_news_cointelegraph(&self) -> Result<Vec<NewsArticle>> {
        self.fetch_rss("https://cointelegraph.com/rss", "CoinTelegraph")
            .await
    }

    pub async fn get_news_coindesk(&self) -> Result<Vec<NewsArticle>> {
        self.fetch_rss(
            "https://www.coindesk.com/arc/outboundfeeds/rss/",
            "CoinDesk",
        )
        .await
    }

    pub async fn get_news_decrypt(&self) -> Result<Vec<NewsArticle>> {
        self.fetch_rss("https://decrypt.co/feed", "Decrypt").await
    }

    pub async fn get_news_blockworks(&self) -> Result<Vec<NewsArticle>> {
        self.fetch_rss("https://blockworks.co/feed", "Blockworks")
            .await
    }

    pub async fn get_news_theblock(&self) -> Result<Vec<NewsArticle>> {
        self.fetch_rss("https://www.theblock.co/rss.xml", "TheBlock")
            .await
    }

    pub async fn get_news_bitcoinmagazine(&self) -> Result<Vec<NewsArticle>> {
        self.fetch_rss("https://bitcoinmagazine.com/.rss/full/", "BitcoinMagazine")
            .await
    }

    /// Fetches the feed XML directly and parses it with the `rss` crate.
    ///
    /// Previously this proxied through api.rss2json.com, whose free tier is
    /// rate-limited and requires an API key — a single point of failure for
    /// the whole news fallback chain. Direct parsing removes that dependency.
    async fn fetch_rss(
        &self,
        feed_url: &str,
        source_name: &'static str,
    ) -> Result<Vec<NewsArticle>> {
        let resp = self.http.get(feed_url).send().await?;
        if !resp.status().is_success() {
            return Err(BotError::CmcApi(format!(
                "RSS fetch status failed: {}",
                resp.status()
            )));
        }

        let xml = resp
            .text()
            .await
            .map_err(|e| BotError::CmcApi(e.to_string()))?;
        let channel = rss::Channel::read_from(xml.as_bytes())
            .map_err(|e| BotError::CmcApi(format!("RSS parse failed: {e}")))?;

        let mut articles = Vec::new();
        for item in channel.items().iter().take(30) {
            let mut body_text = item
                .content()
                .map(str::to_string)
                .unwrap_or_else(|| item.description().unwrap_or_default().to_string());
            body_text = strip_html(&body_text);
            if body_text.chars().count() > 800 {
                body_text = body_text.chars().take(797).collect::<String>();
                body_text.push_str("...");
            }

            articles.push(NewsArticle {
                title: item.title().unwrap_or_default().trim().to_string(),
                source: source_name.to_string(),
                body: body_text.trim().to_string(),
                url: item.link().unwrap_or_default().to_string(),
            });
        }
        Ok(articles)
    }
}

fn strip_html(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            out.push(c);
        }
    }
    out
}
