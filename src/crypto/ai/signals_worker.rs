//! Background worker that refreshes the free crowd-signal sources.
//!
//! It is deliberately AI-free: CMC headlines, Binance announcements, trending
//! searches and community votes are stored raw in `market_signals` with a short
//! TTL. The news worker folds them into its existing summary pass (no extra AI
//! call) and `/ask` reads them straight from the DB when a question is about
//! what the market is saying or feeling.

use std::sync::Arc;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use crate::{crypto::CryptoClient, db::SqliteStore, db::Store};

/// 30 minutes is enough for headlines and trending; these endpoints change on
/// the scale of hours, not seconds.
const SCAN_INTERVAL_SECS: u64 = 30 * 60;
/// Startup jitter so this does not compete with the news/macro workers.
const FIRST_SCAN_DELAY_SECS: u64 = 90;
/// Raw crowd data has a short useful life; the AI summary is what persists.
const SIGNAL_TTL_DAYS: i64 = 3;
/// CoinGecko sentiment costs one request per coin — keep the fan-out small.
const MAX_SENTIMENT_SYMBOLS: usize = 6;

pub async fn run(store: Arc<Store>, sqlite: Arc<SqliteStore>, crypto: Arc<CryptoClient>) {
    info!("Starting market signals worker (30-min interval)");
    sleep(Duration::from_secs(FIRST_SCAN_DELAY_SECS)).await;
    loop {
        if let Err(e) = scan_once(&store, &sqlite, &crypto).await {
            warn!("Signals worker scan failed: {e}");
        }
        sleep(Duration::from_secs(SCAN_INTERVAL_SECS)).await;
    }
}

async fn scan_once(
    store: &Store,
    sqlite: &SqliteStore,
    crypto: &CryptoClient,
) -> anyhow::Result<()> {
    let mut symbols: Vec<String> = Vec::new();
    for user_id in store.all_user_ids() {
        let watched = store.read(user_id, |p| {
            let mut syms: Vec<String> = p.holdings_by_symbol().into_keys().collect();
            syms.extend(p.settings.default_watchlist.clone());
            syms
        });
        for sym in watched {
            let upper = sym.to_uppercase();
            if !symbols.contains(&upper) {
                symbols.push(upper);
            }
        }
    }
    symbols.sort();

    let sentiment_symbols: Vec<&str> = symbols
        .iter()
        .take(MAX_SENTIMENT_SYMBOLS)
        .map(|s| s.as_str())
        .collect();

    let signals = crypto.get_market_signals(&sentiment_symbols).await;
    if signals.is_empty() {
        info!("Signals worker: no signals returned this cycle");
        return Ok(());
    }
    let count = signals.len();
    let saved = sqlite.save_market_signals(signals).await?;
    info!("Signals worker: stored {saved}/{count} signals");

    let removed = sqlite.cleanup_market_signals(SIGNAL_TTL_DAYS).await?;
    if removed > 0 {
        info!("Signals worker: pruned {removed} stale signals");
    }
    Ok(())
}
