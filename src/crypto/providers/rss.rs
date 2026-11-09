use super::cryptocompare::NewsArticle;
use crate::error::{BotError, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
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
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static(
                "application/rss+xml, application/xml;q=0.9, text/xml;q=0.8, */*;q=0.5",
            ),
        );

        Self {
            http: Client::builder()
                .default_headers(headers)
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                // Several publishers answer with a 301/308 to their canonical
                // feed URL (Blockworks is the case in point). Without an
                // explicit policy the body of the redirect response was parsed
                // as XML, which is why that source silently returned nothing.
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .unwrap(),
        }
    }

    /// Every feed that currently answers with real XML, fetched in parallel.
    ///
    /// CoinDesk and Bitcoin Magazine were dropped: both reply `403 Forbidden`
    /// to non-browser clients, so they only ever produced warnings. Dead slots
    /// were refilled with outlets that are actually reachable and cover a
    /// different slice of the market, so a single publisher can no longer
    /// dominate the digest.
    fn feeds(&self) -> Vec<(&'static str, &'static str)> {
        vec![
            ("https://cointelegraph.com/rss", "CoinTelegraph"),
            ("https://decrypt.co/feed", "Decrypt"),
            ("https://blockworks.co/feed", "Blockworks"),
            ("https://www.theblock.co/rss.xml", "TheBlock"),
            ("https://cryptoslate.com/feed/", "CryptoSlate"),
            ("https://ambcrypto.com/feed/", "AMBCrypto"),
            ("https://beincrypto.com/feed/", "BeInCrypto"),
            ("https://cryptopotato.com/feed/", "CryptoPotato"),
            ("https://dailyhodl.com/feed/", "TheDailyHodl"),
            ("https://news.bitcoin.com/feed/", "BitcoinCom"),
            ("https://coinjournal.net/feed/", "CoinJournal"),
            ("https://www.newsbtc.com/feed/", "NewsBTC"),
            ("https://cryptobriefing.com/feed/", "CryptoBriefing"),
            ("https://coingape.com/feed/", "CoinGape"),
        ]
    }

    /// Fetch all sources in parallel and merge them, de-duplicated by URL.
    ///
    /// One failing publisher must never empty the whole list, so partial
    /// failures are logged and skipped.
    pub async fn get_all_news(&self) -> Result<Vec<NewsArticle>> {
        let feeds = self.feeds();
        let total_feeds = feeds.len();
        let mut tasks = Vec::with_capacity(feeds.len());
        for (url, name) in feeds {
            tasks.push(async move { (name, self.fetch_rss(url, name).await) });
        }

        let mut merged: Vec<NewsArticle> = Vec::new();
        let mut ok_sources = 0usize;
        for (name, res) in futures_util::future::join_all(tasks).await {
            match res {
                Ok(mut list) => {
                    if !list.is_empty() {
                        ok_sources += 1;
                    }
                    merged.append(&mut list);
                }
                Err(e) => tracing::debug!("RSS feed {name} failed: {e}"),
            }
        }

        if merged.is_empty() {
            return Err(BotError::CmcApi(format!(
                "all {total_feeds} RSS feeds failed"
            )));
        }
        tracing::debug!(
            "Merged {} articles from {ok_sources} RSS feeds",
            merged.len()
        );

        // De-duplicate by URL first (the same story is syndicated with slightly
        // different titles across publishers), then by title.
        let mut seen_urls = std::collections::HashSet::new();
        merged.retain(|a| a.url.is_empty() || seen_urls.insert(a.url.clone()));
        let mut seen_titles = std::collections::HashSet::new();
        merged.retain(|a| seen_titles.insert(a.title.trim().to_lowercase()));
        merged.retain(|a| !a.title.trim().is_empty());
        Ok(merged)
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

        // Most publishers serve RSS 2.0, but Blockworks serves Atom, which the
        // `rss` crate's Channel parser cannot read at all — that source was
        // fetched successfully and then silently discarded.
        let is_atom = xml
            .get(..2048)
            .map(|head| head.contains("<feed") && head.contains("Atom"))
            .unwrap_or(false);
        if is_atom {
            return parse_atom(xml.as_bytes(), source_name);
        }

        let channel = match rss::Channel::read_from(xml.as_bytes()) {
            Ok(c) => c,
            Err(e) => {
                // Wrongly labelled feeds exist too: fall back to Atom once.
                tracing::debug!("RSS parse failed for {source_name} ({e}); trying Atom");
                return parse_atom(xml.as_bytes(), source_name);
            }
        };

        let mut articles = Vec::new();
        for item in channel.items() {
            let body_text = item
                .content()
                .map(str::to_string)
                .unwrap_or_else(|| item.description().unwrap_or_default().to_string());

            articles.push((
                item.pub_date().and_then(parse_feed_date),
                NewsArticle {
                    title: clean_text(item.title().unwrap_or_default()),
                    source: source_name.to_string(),
                    body: clean_body(&body_text),
                    url: item.link().unwrap_or_default().to_string(),
                },
            ));
        }
        Ok(order_recent(articles, 30))
    }
}

fn parse_atom(xml: &[u8], source_name: &'static str) -> Result<Vec<NewsArticle>> {
    let feed = atom_syndication::Feed::read_from(xml)
        .map_err(|e| BotError::CmcApi(format!("Atom parse failed: {e}")))?;

    let mut articles = Vec::new();
    for entry in feed.entries() {
        let title = clean_text(entry.title().as_str());
        let body_text = entry
            .content()
            .and_then(|c| c.value().map(str::to_string))
            .or_else(|| entry.summary().map(|s| s.as_str().to_string()))
            .unwrap_or_default();
        let url = entry
            .links()
            .iter()
            .find(|l| l.rel() == "alternate" || l.rel().is_empty())
            .or_else(|| entry.links().first())
            .map(|l| l.href().to_string())
            .unwrap_or_default();

        articles.push((
            Some(entry.updated().with_timezone(&Utc)),
            NewsArticle {
                title,
                source: source_name.to_string(),
                body: clean_body(&body_text),
                url,
            },
        ));
    }
    Ok(order_recent(articles, 30))
}

/// Parse the timestamp formats actually used by RSS 2.0 and Atom feeds.
fn parse_feed_date(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc2822(raw)
        .ok()
        .or_else(|| DateTime::parse_from_rfc3339(raw).ok())
        .map(|d| d.with_timezone(&Utc))
        // Publishers routinely ship a wrong weekday or an unusual RFC 2822
        // variant; parse the date/time without the weekday before giving up.
        .or_else(|| {
            let rest = raw.split_once(", ").map(|(_, r)| r).unwrap_or(raw);
            let normalized = rest
                .replace(" GMT", " +0000")
                .replace(" UTC", " +0000")
                .replace(" UT", " +0000");
            DateTime::parse_from_str(&normalized, "%d %b %Y %H:%M:%S %z")
                .ok()
                .map(|d| d.with_timezone(&Utc))
        })
        .or_else(|| {
            NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
                .ok()
                .map(|n| n.and_utc())
        })
        .or_else(|| {
            NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|n| n.and_utc())
        })
}

/// Newest first, undated entries last. Feeds are not guaranteed to be
/// chronological, and the old code simply took the first 30 items as served —
/// a stale or reordered feed could push today's headlines out entirely.
fn order_recent(
    mut items: Vec<(Option<DateTime<Utc>>, NewsArticle)>,
    limit: usize,
) -> Vec<NewsArticle> {
    items.sort_by(|a, b| match (&a.0, &b.0) {
        (Some(x), Some(y)) => y.cmp(x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    items.truncate(limit);
    items.into_iter().map(|(_, article)| article).collect()
}

/// Strip markup and cap the body at 800 chars (the news prompt only ever uses
/// the first 800 anyway).
fn clean_body(raw: &str) -> String {
    let mut body = clean_text(raw);
    if body.chars().count() > 800 {
        body = body.chars().take(797).collect::<String>();
        body.push_str("...");
    }
    body.trim().to_string()
}

/// Strip markup, decode the XML/HTML entities that leak into feed titles and
/// bodies (`&amp;`, `&quot;`, `&#8217;`, …), and trim.
fn clean_text(raw: &str) -> String {
    decode_entities(&strip_html(raw)).trim().to_string()
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let semi = tail.find(';').filter(|p| *p <= 12);
        let decoded = semi.and_then(|semi| {
            let ent = &tail[1..semi];
            let ch = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => ent.strip_prefix('#').and_then(|num| {
                    let code = match num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => num.parse::<u32>().ok(),
                    };
                    code.and_then(char::from_u32)
                }),
            };
            ch.map(|ch| (ch, semi))
        });
        if let Some((ch, semi)) = decoded {
            out.push(ch);
            rest = &tail[semi + 1..];
        } else {
            out.push('&');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    out
}

pub(crate) fn strip_html(s: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::{decode_entities, order_recent, parse_atom, parse_feed_date, strip_html};
    use crate::crypto::providers::cryptocompare::NewsArticle;

    #[test]
    fn html_tags_are_stripped_from_descriptions() {
        assert_eq!(
            strip_html("<p>BTC <b>drops</b> 5%</p>").trim(),
            "BTC drops 5%"
        );
    }

    #[test]
    fn entities_are_decoded_in_titles_and_bodies() {
        // Feed titles routinely carry escaped markup (`&amp;`) and typographic
        // entities (`&#8217;`). They used to reach the AI prompt and the
        // Telegram message verbatim.
        assert_eq!(
            decode_entities("Bitcoin &amp; Ethereum &quot;rally&quot;"),
            "Bitcoin & Ethereum \"rally\""
        );
        assert_eq!(decode_entities("it&#8217;s up &lt;5%&gt;"), "it’s up <5%>");
        assert_eq!(decode_entities("&#x41;&#66;"), "AB");
        // Known bug of the naive scanner: a bare ampersand must survive.
        assert_eq!(decode_entities("A & B"), "A & B");
        assert_eq!(decode_entities("&unknown;"), "&unknown;");
    }

    #[test]
    fn atom_feeds_are_parsed() {
        // Trimmed real-world shape of the Blockworks feed: Atom, not RSS.
        let xml = br#"<?xml version="1.0" encoding="utf-8"?>
<feed xmlns="http://www.w3.org/2005/Atom">
  <title>Test</title>
  <entry>
    <title>ETF inflows hit a record</title>
    <link rel="alternate" href="https://example.com/a"/>
    <id>https://example.com/a</id>
    <updated>2026-11-02T10:00:00Z</updated>
    <summary>Bitcoin ETFs took in $1B.</summary>
  </entry>
</feed>"#;

        let articles = parse_atom(xml, "Blockworks").expect("atom parse");
        assert_eq!(articles.len(), 1);
        assert_eq!(articles[0].title, "ETF inflows hit a record");
        assert_eq!(articles[0].source, "Blockworks");
        assert_eq!(articles[0].url, "https://example.com/a");
        assert!(articles[0].body.contains("$1B"));
    }

    #[test]
    fn feed_dates_are_parsed_in_both_formats() {
        assert!(parse_feed_date("Wed, 08 Nov 2026 20:44:00 GMT").is_some());
        assert!(parse_feed_date("2026-11-08T20:44:00Z").is_some());
        assert!(parse_feed_date("2026-11-08T20:44:00+00:00").is_some());
        assert!(parse_feed_date("2026-11-08T20:44:00").is_some());
        assert!(parse_feed_date("").is_none());
        assert!(parse_feed_date("not a date").is_none());
    }

    #[test]
    fn feed_items_are_sorted_newest_first_and_undated_last() {
        let art = |title: &str| NewsArticle {
            title: title.to_string(),
            source: "test".to_string(),
            body: String::new(),
            url: String::new(),
        };
        let d = |s: &str| parse_feed_date(s);
        let items = vec![
            (d("Wed, 01 Jan 2025 00:00:00 GMT"), art("old")),
            (None, art("undated")),
            (d("Wed, 08 Nov 2026 00:00:00 GMT"), art("new")),
        ];
        let out = order_recent(items, 30);
        assert_eq!(out[0].title, "new");
        assert_eq!(out[1].title, "old");
        assert_eq!(out[2].title, "undated");
    }
}
