use std::time::Duration;
use reqwest::Client;
use serde::Deserialize;
use super::cryptocompare::NewsArticle;
use crate::error::{BotError, Result};

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
        self.fetch_rss("https://cointelegraph.com/rss", "CoinTelegraph").await
    }

    pub async fn get_news_coindesk(&self) -> Result<Vec<NewsArticle>> {
        self.fetch_rss("https://www.coindesk.com/arc/outboundfeeds/rss/", "CoinDesk").await
    }

    pub async fn get_news_decrypt(&self) -> Result<Vec<NewsArticle>> {
        self.fetch_rss("https://decrypt.co/feed", "Decrypt").await
    }

    pub async fn get_news_blockworks(&self) -> Result<Vec<NewsArticle>> {
        self.fetch_rss("https://blockworks.co/feed", "Blockworks").await
    }

    pub async fn get_news_theblock(&self) -> Result<Vec<NewsArticle>> {
        self.fetch_rss("https://www.theblock.co/rss.xml", "TheBlock").await
    }

    pub async fn get_news_bitcoinmagazine(&self) -> Result<Vec<NewsArticle>> {
        self.fetch_rss("https://bitcoinmagazine.com/.rss/full/", "BitcoinMagazine").await
    }

    async fn fetch_rss(&self, feed_url: &str, source_name: &'static str) -> Result<Vec<NewsArticle>> {
        let url = format!("https://api.rss2json.com/v1/api.json?rss_url={}", feed_url);
        let resp = self.http.get(&url).send().await?;
        if !resp.status().is_success() {
            return Err(BotError::CmcApi(format!("RSS fetch status failed: {}", resp.status())));
        }

        let body: Rss2JsonResp = resp.json().await.map_err(|e| BotError::CmcApi(e.to_string()))?;
        if body.status != "ok" {
            return Err(BotError::CmcApi("RSS2JSON returned non-ok status".to_string()));
        }

        let mut articles = Vec::new();
        for item in body.items.into_iter().take(30) {
            let mut body_text = item.content.unwrap_or_else(|| item.description.unwrap_or_default());
            body_text = strip_html(&body_text);
            if body_text.chars().count() > 800 {
                body_text = body_text.chars().take(797).collect::<String>();
                body_text.push_str("...");
            }

            articles.push(NewsArticle {
                title: item.title.trim().to_string(),
                source: source_name.to_string(),
                body: body_text.trim().to_string(),
                url: item.link.unwrap_or_default(),
            });
        }
        Ok(articles)
    }
}

#[derive(Deserialize)]
struct Rss2JsonResp {
    status: String,
    items: Vec<Rss2JsonItem>,
}

#[derive(Deserialize)]
struct Rss2JsonItem {
    title: String,
    description: Option<String>,
    content: Option<String>,
    link: Option<String>,
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
