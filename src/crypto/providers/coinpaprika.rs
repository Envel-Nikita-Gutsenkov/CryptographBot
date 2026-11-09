use super::RawQuote;
use crate::error::{BotError, Result};
use moka::future::Cache;
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

pub struct CoinPaprikaProvider {
    http: Client,
    cache: Cache<(), Vec<RawQuote>>,
}

impl CoinPaprikaProvider {
    pub const NAME: &'static str = "coinpaprika";

    pub fn new() -> Self {
        Self {
            http: Client::builder()
                .timeout(Duration::from_secs(8))
                .use_rustls_tls()
                .build()
                .unwrap(),
            cache: Cache::builder()
                .time_to_live(Duration::from_secs(45))
                .max_capacity(1)
                .build(),
        }
    }

    pub async fn get_quotes(&self, symbols: &[&str]) -> Result<Vec<RawQuote>> {
        let all_quotes = self.get_all_tickers().await?;
        let mut out = Vec::new();

        for &sym in symbols {
            let sym_upper = sym.to_uppercase();
            // Symbols collide (e.g. SOL -> sol-solana AND sol-binance-peg-sol):
            // pick the primary asset — the match with the best (lowest) rank.
            let best = all_quotes
                .iter()
                .filter(|q| q.symbol == sym_upper)
                .min_by_key(|q| q.rank.unwrap_or(u32::MAX));
            if let Some(q) = best {
                out.push(q.clone());
            }
        }

        Ok(out)
    }

    async fn get_all_tickers(&self) -> Result<Vec<RawQuote>> {
        if let Some(cached) = self.cache.get(&()).await {
            return Ok(cached);
        }

        let resp = self
            .http
            .get("https://api.coinpaprika.com/v1/tickers")
            .header("User-Agent", "CryptographBot/1.0")
            .send()
            .await?;
        if resp.status() == 429 {
            return Err(BotError::RateLimited);
        }

        let body: Vec<PaprikaTicker> = resp
            .json()
            .await
            .map_err(|e| BotError::CmcApi(e.to_string()))?;

        let mut quotes = Vec::with_capacity(body.len());
        for t in body {
            let rank = t.rank.unwrap_or(0);
            if rank == 0 || t.quotes.usd.price == 0.0 {
                continue;
            } // filter useless ones

            quotes.push(RawQuote {
                symbol: t.symbol.to_uppercase(),
                name: t.name,
                price_usd: t.quotes.usd.price,
                market_cap: Some(t.quotes.usd.market_cap),
                volume_24h: Some(t.quotes.usd.volume_24h),
                change_1h: Some(t.quotes.usd.percent_change_1h),
                change_24h: Some(t.quotes.usd.percent_change_24h),
                change_7d: Some(t.quotes.usd.percent_change_7d),
                change_30d: Some(t.quotes.usd.percent_change_30d),
                change_90d: None,
                change_365d: None,
                change_730d: None,
                ath: t.quotes.usd.ath_price,
                ath_change_percentage: t.quotes.usd.percent_from_price_ath,
                high_24h: None,
                low_24h: None,
                rank: Some(rank),
                circulating_supply: t.circulating_supply,
                source: Self::NAME,
            });
        }

        self.cache.insert((), quotes.clone()).await;
        Ok(quotes)
    }
}

#[derive(Deserialize)]
struct PaprikaTicker {
    symbol: String,
    name: String,
    rank: Option<u32>,
    circulating_supply: Option<f64>,
    quotes: PaprikaQuotes,
}

#[derive(Deserialize)]
struct PaprikaQuotes {
    #[serde(rename = "USD")]
    usd: PaprikaUsdQuote,
}

#[derive(Deserialize)]
struct PaprikaUsdQuote {
    price: f64,
    volume_24h: f64,
    market_cap: f64,
    percent_change_1h: f64,
    percent_change_24h: f64,
    percent_change_7d: f64,
    percent_change_30d: f64,
    #[serde(default)]
    ath_price: Option<f64>,
    #[serde(default)]
    percent_from_price_ath: Option<f64>,
}
