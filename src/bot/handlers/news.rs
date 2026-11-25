//! `/news [SYM] [N]` — a clean, watcher.guru-style crypto headline digest.
//!
//! The old implementation called [`fetch_news_for_symbol`](crate::crypto::ai::fetch_news_for_symbol),
//! which pulled a single Cointelegraph RSS feed, applied a loose keyword filter
//! and finally fell back to a *general* story when the coin had no coverage — so
//! `/news BTC` could answer with one unrelated headline. This version:
//!
//! 1. pulls the merged multi-source feed (CoinDesk, Cointelegraph, The Block,
//!    Decrypt, Bitcoin Magazine, CryptoSlate, …) via
//!    [`CryptoClient::get_news_for_display`], which is strictly per-symbol (no
//!    general-feed fallback), newest-first;
//! 2. renders a compact numbered digest — headline + source + age, no bodies,
//!    no filler — with clickable links, capped well under Telegram's 4096 limit.

use std::sync::Arc;

use chrono::Utc;
use teloxide::{
    prelude::*,
    types::{InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
};

use crate::{
    config::Config,
    crypto::{providers::cryptocompare::NewsArticle, CryptoClient},
    db::Store,
};

/// Default number of headlines in a digest.
const DEFAULT_LIMIT: usize = 10;
/// Upper bound so the message never approaches Telegram's 4096-char limit.
const MAX_LIMIT: usize = 15;
/// Soft UTF-16 budget for the rendered digest. Telegram's hard cap is 4096
/// UTF-16 code units and over-long messages fail *silently*, so we stop adding
/// headlines before we can reach it (the footer + controls take the rest).
const NEWS_BUDGET: usize = 3600;

/// Length in UTF-16 code units, which is what Telegram actually counts.
fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

#[allow(deprecated)]
pub async fn handle_news_cmd(
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

    let russian = is_russian(&store, msg.chat.id.0 as u64);
    let parts: Vec<&str> = args.split_whitespace().collect();
    let (symbol, limit) = parse_news_args(&parts);

    let loading = bot
        .send_message(
            msg.chat.id,
            if russian {
                "📰 Собираю сводку…"
            } else {
                "📰 Fetching headlines…"
            },
        )
        .await?;
    let articles = fetch_digest(&cmc, &symbol).await;
    let _ = bot.delete_message(msg.chat.id, loading.id).await;

    send_digest(&bot, msg.chat.id, &symbol, limit, russian, articles).await
}

#[allow(deprecated)]
pub async fn handle_news_callback(
    bot: Bot,
    chat_id: ChatId,
    symbol: String,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    _config: Arc<Config>,
) -> anyhow::Result<()> {
    let russian = is_russian(&store, chat_id.0 as u64);
    let loading = bot
        .send_message(
            chat_id,
            if russian {
                "📰 Собираю сводку…"
            } else {
                "📰 Fetching headlines…"
            },
        )
        .await?;
    let articles = fetch_digest(&cmc, &symbol).await;
    let _ = bot.delete_message(chat_id, loading.id).await;

    send_digest(&bot, chat_id, &symbol, DEFAULT_LIMIT, russian, articles).await
}

/// Accepts `[SYM] [N]` in either order; a numeric token is the headline cap, an
/// alphabetic token is the symbol. No tokens → general market digest.
fn parse_news_args(parts: &[&str]) -> (String, usize) {
    let mut symbol = "GENERAL".to_string();
    let mut limit = DEFAULT_LIMIT;
    for p in parts {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        if let Ok(n) = p.parse::<usize>() {
            limit = n.clamp(1, MAX_LIMIT);
        } else {
            symbol = p.to_uppercase();
        }
    }
    (symbol, limit)
}

async fn fetch_digest(cmc: &CryptoClient, symbol: &str) -> anyhow::Result<Vec<NewsArticle>> {
    cmc.get_news_for_display(symbol)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
}

async fn send_digest(
    bot: &Bot,
    chat_id: ChatId,
    symbol: &str,
    limit: usize,
    russian: bool,
    result: anyhow::Result<Vec<NewsArticle>>,
) -> anyhow::Result<()> {
    let articles = match result {
        Ok(a) => a,
        Err(e) => {
            bot.send_message(
                chat_id,
                if russian {
                    format!("❌ Не удалось получить новости: {e}")
                } else {
                    format!("❌ Failed to fetch news: {e}")
                },
            )
            .await?;
            return Ok(());
        }
    };

    let text = render_digest(&articles, symbol, limit, russian);
    let keyboard = digest_keyboard(symbol, russian);
    bot.send_message(chat_id, text)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

fn render_digest(articles: &[NewsArticle], symbol: &str, limit: usize, russian: bool) -> String {
    let is_general = symbol.is_empty() || symbol == "GENERAL" || symbol == "ALL";

    let title = if is_general {
        if russian {
            "📰 <b>Крипто-новости</b> · свежее"
        } else {
            "📰 <b>Crypto news</b> · latest"
        }
    } else if russian {
        return render_symbol(articles, symbol, limit, true);
    } else {
        return render_symbol(articles, symbol, limit, false);
    };

    let mut out = String::from(title);
    out.push('\n');

    if articles.is_empty() {
        out.push('\n');
        out.push_str(if russian {
            "Сейчас нет свежих новостей.\nПопробуй позже или укажи монету: <code>/news BTC</code>"
        } else {
            "No fresh headlines right now.\nTry again later or pick a coin: <code>/news BTC</code>"
        });
        return out;
    }

    let shown = append_headlines(&mut out, articles, limit, russian);

    if articles.len() > shown {
        out.push_str(&format!(
            "\n<i>{}</i>\n",
            if russian {
                format!("ещё {} — /news {}", articles.len() - shown, symbol)
            } else {
                format!("{} more — /news {}", articles.len() - shown, symbol)
            }
        ));
    }

    out.push_str(&sources_footer(articles, shown, russian));
    out
}

/// Append up to
fn render_symbol(articles: &[NewsArticle], symbol: &str, limit: usize, russian: bool) -> String {
    let mut out = if russian {
        format!("📰 <b>Новости {}</b>", esc_html(symbol))
    } else {
        format!("📰 <b>{} news</b>", esc_html(symbol))
    };
    out.push('\n');

    if articles.is_empty() {
        out.push('\n');
        out.push_str(&if russian {
            format!(
                "Пока нет свежих новостей именно по {sym}.\nПопробуй общую сводку: <code>/news</code>",
                sym = esc_html(symbol)
            )
        } else {
            format!(
                "No fresh {} headlines right now.\nTry the general digest: <code>/news</code>",
                esc_html(symbol)
            )
        });
        return out;
    }

    out.push_str(&format!(
        "<i>{}</i>\n\n",
        if russian {
            format!("{} заголовков · {}", articles.len().min(limit), "по свежести")
        } else {
            format!("{} headlines · newest first", articles.len().min(limit))
        }
    ));

    let shown = append_headlines(&mut out, articles, limit, russian);

    if articles.len() > shown {
        out.push_str(&format!(
            "\n<i>{}</i>\n",
            if russian {
                format!("ещё {} — /news {} {}", articles.len() - shown, symbol, MAX_LIMIT)
            } else {
                format!("{} more — /news {} {}", articles.len() - shown, symbol, MAX_LIMIT)
            }
        ));
    }

    out.push_str(&sources_footer(articles, shown, russian));
    out
}

/// Append up to `limit` headlines, stopping early if the message would exceed
/// the UTF-16 budget. Returns how many were actually appended.
fn append_headlines(
    out: &mut String,
    articles: &[NewsArticle],
    limit: usize,
    russian: bool,
) -> usize {
    let n = articles.len().min(limit);
    let mut shown = 0usize;
    for (i, a) in articles.iter().take(n).enumerate() {
        let line = format!("{}. {}\n", i + 1, headline_line(a, russian));
        if shown > 0 && utf16_len(out) + utf16_len(&line) > NEWS_BUDGET {
            break;
        }
        out.push_str(&line);
        shown += 1;
    }
    shown
}

/// `[2h] CoinDesk — <a>Headline</a>`
fn headline_line(a: &NewsArticle, russian: bool) -> String {
    let age = a
        .published_at
        .map(|t| human_age(t, russian))
        .unwrap_or_default();
    let age_tag = if age.is_empty() {
        String::new()
    } else {
        format!("<code>{}</code> ", age)
    };
    let source = esc_html(&a.source);
    let title = esc_html(&a.title);
    let linked = if a.url.is_empty() {
        title
    } else {
        format!("<a href=\"{}\">{}</a>", esc_html_attr(&a.url), title)
    };
    format!("{}{} — {}", age_tag, source, linked)
}

fn sources_footer(articles: &[NewsArticle], shown: usize, russian: bool) -> String {
    let mut seen: Vec<String> = Vec::new();
    for a in articles.iter().take(shown) {
        let s = a.source.trim();
        if !s.is_empty() && !seen.iter().any(|x| x.eq_ignore_ascii_case(s)) {
            seen.push(s.to_string());
        }
    }
    if seen.is_empty() {
        return String::new();
    }
    format!(
        "\n<i>{}: {}</i>",
        if russian { "источники" } else { "sources" },
        esc_html(&seen.join(", "))
    )
}

fn digest_keyboard(symbol: &str, russian: bool) -> InlineKeyboardMarkup {
    let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
    rows.push(vec![InlineKeyboardButton::callback(
        if russian { "🔄 Обновить" } else { "🔄 Refresh" },
        format!("news_select:{}", if symbol.is_empty() { "GENERAL" } else { symbol }),
    )]);
    if symbol != "GENERAL" && !symbol.is_empty() {
        rows.push(vec![InlineKeyboardButton::callback(
            if russian {
                "🌐 Общий рынок"
            } else {
                "🌐 General market"
            },
            "news_select:GENERAL".to_string(),
        )]);
    }
    InlineKeyboardMarkup::new(rows)
}

/// Compact, localized "how long ago" label from a publication timestamp.
fn human_age(then: chrono::DateTime<Utc>, russian: bool) -> String {
    let secs = (Utc::now() - then).num_seconds().max(0);
    let (n, unit) = if secs < 60 {
        (secs, "s")
    } else if secs < 3_600 {
        (secs / 60, "m")
    } else if secs < 86_400 {
        (secs / 3_600, "h")
    } else {
        (secs / 86_400, "d")
    };
    if russian {
        let ru = match unit {
            "s" => "с",
            "m" => "м",
            "h" => "ч",
            _ => "д",
        };
        format!("{}{}", n, ru)
    } else {
        format!("{}{}", n, unit)
    }
}

fn esc_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

fn esc_html_attr(s: &str) -> String {
    esc_html(s).replace('"', "&quot;")
}

fn is_russian(store: &Store, user_id: u64) -> bool {
    store
        .read(user_id, |p| p.settings.preferred_language.clone())
        .to_lowercase()
        .starts_with("ru")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn art(title: &str, source: &str, age_mins: i64) -> NewsArticle {
        NewsArticle {
            title: title.to_string(),
            source: source.to_string(),
            body: String::new(),
            url: "https://example.com/x".to_string(),
            published_at: Some(Utc::now() - Duration::minutes(age_mins)),
        }
    }

    #[test]
    fn parse_args_accepts_either_order() {
        assert_eq!(parse_news_args(&["BTC"]).0, "BTC");
        assert_eq!(parse_news_args(&["5"]).1, 5);
        assert_eq!(parse_news_args(&["5", "ETH"]), ("ETH".into(), 5));
        assert_eq!(parse_news_args(&["ETH", "7"]), ("ETH".into(), 7));
        assert_eq!(parse_news_args(&[]), ("GENERAL".into(), DEFAULT_LIMIT));
        // clamp
        assert_eq!(parse_news_args(&["999"]).1, MAX_LIMIT);
    }

    #[test]
    fn digest_renders_numbered_headlines_without_bodies() {
        let arts = vec![art("Bitcoin ETF inflows", "CoinDesk", 120), art("More BTC", "Decrypt", 5)];
        let text = render_symbol(&arts, "BTC", 10, false);
        assert!(text.contains("1."));
        assert!(text.contains("CoinDesk"));
        assert!(text.contains("<a href="));
        assert!(text.contains("2h") || text.contains("1h"));
    }

    #[test]
    fn empty_symbol_news_is_explicit_not_general_fallback() {
        let text = render_symbol(&[], "BTC", 10, false);
        assert!(text.contains("No fresh BTC headlines"));
        assert!(!text.contains("1."));
    }

    #[test]
    fn html_is_escaped() {
        let arts = vec![art("A <script> & \"quote\"", "Src", 1)];
        let text = render_symbol(&arts, "BTC", 10, false);
        assert!(text.contains("&lt;script&gt;"));
        assert!(text.contains("&amp;"));
        assert!(!text.contains("<script>"));
    }

    #[test]
    fn digest_respects_limit() {
        let arts: Vec<_> = (0..20).map(|i| art(&format!("h{i}"), "S", i)).collect();
        let text = render_symbol(&arts, "BTC", 5, false);
        assert!(text.contains("5. "));
        assert!(!text.contains("6. h"));
    }
}
