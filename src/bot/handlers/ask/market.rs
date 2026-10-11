//! Market-data context for `/ask`: live quotes / on-chain / tradFi / F&G +
//! global metrics, then the local memory blocks (trajectory, chart, trained
//! strategy, calendar, locality, regime) and the deterministic strategy layer.
//!
//! Every network fetch and every sync DB open is hard-bounded so a slow
//! provider or a locked trading DB can never stall the whole /ask pipeline.

use std::sync::Arc;

use crate::{
    crypto::CryptoClient,
    db::{SqliteStore, Store},
};

/// Build the full `market_data` prompt block for the current question.
///
/// Returns an empty-ish block on total failure — /ask must degrade gracefully,
/// never hang. All network work is wrapped in timeouts (20s F&G+global join,
/// 30s quotes/onchain/tradFi), and the sync trading-DB blocks are bounded
/// inside `market_memory` (spawn_blocking + per-block timeout).
pub(crate) async fn build_market_data(
    question: &str,
    user_id: u64,
    store: &Arc<Store>,
    cmc: &Arc<CryptoClient>,
    sqlite: &Arc<SqliteStore>,
    symbols: &[String],
    portfolio_context: &str,
    progress_tx: Option<&tokio::sync::mpsc::UnboundedSender<String>>,
) -> String {
    // Scenario override: deterministic "what-if" market replaces the live
    // market_data block (same prompt shape, no API calls). When active, we
    // skip every live fetch (fear&greed, global metrics, quotes, onchain,
    // tradfi) so scenario runs are fast and reproducible.
    let scenario_market_data = crate::crypto::ai::scenario::active_scenario()
        .and_then(|s| s.ask_market_data(portfolio_context));

    let mut quote_signals: Vec<crate::crypto::ai::strategy::QuoteSignal> = Vec::new();
    let mut fear_greed = "Unavailable.".to_string();

    let mut market_data = if let Some(md) = scenario_market_data {
        md
    } else {
        // F&G + global metrics are independent; run them together but bound the
        // whole join — the global-metrics fallback chain (CMC -> CoinGecko ->
        // Coinpaprika -> DefiLlama) alone can take ~34s worst case, and a dead
        // provider must not stall the ask.
        let (fg, global_metrics) =
            match tokio::time::timeout(std::time::Duration::from_secs(20), async {
                tokio::join!(
                    crate::bot::handlers::ai::fetch_fear_and_greed_index(),
                    cmc.get_global_metrics(),
                )
            })
            .await
            {
                Ok(v) => v,
                Err(_) => {
                    tracing::warn!(
                        "fear&greed + global metrics timed out after 20s — using empty data"
                    );
                    (
                        "Unavailable.".to_string(),
                        Err(crate::error::BotError::CmcApi("timeout".into())),
                    )
                }
            };
        fear_greed = fg;

        let global_info = global_metrics
            .map(|g| {
                let mut info = format!(
                "Total Market Cap: ${:.0}\n24h Volume: ${:.0}\nBTC Dom: {:.2}%\nETH Dom: {:.2}%",
                g.total_market_cap, g.total_volume_24h, g.btc_dominance, g.eth_dominance
            );
                if let Some(s) = g.stablecoin_mcap {
                    info.push_str(&format!("\nStablecoin Mcap: ${:.0}", s));
                }
                if let Some(d) = g.dex_volume_24h {
                    info.push_str(&format!("\nDEX Volume 24h: ${:.0}", d));
                }
                info
            })
            .unwrap_or_else(|_| "Market data unavailable.".into());

        if symbols.is_empty() {
            format!(
                "Global Market Context:\nFear & Greed Index: {}\n{}",
                fear_greed, global_info
            )
        } else {
            let syms_ref: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
            if let Some(tx) = progress_tx {
                let _ =
                    tx.send("⏳ Fetching deep historical market data... Please wait.".to_string());
            }
            let mut tradfi_syms = Vec::new();
            for s in symbols {
                if s.ends_with("ON") {
                    let base = s.strip_suffix("ON").unwrap();
                    if base.len() >= 2 && base.len() <= 5 {
                        tradfi_syms.push(base.to_string());
                    }
                }
            }
            let tradfi_refs: Vec<&str> = tradfi_syms.iter().map(|s| s.as_str()).collect();

            let (quotes_res, onchain_res, tradfi_res) =
                match tokio::time::timeout(std::time::Duration::from_secs(30), async {
                    tokio::join!(
                        cmc.get_quotes(&syms_ref),
                        cmc.get_onchain_metrics(&syms_ref),
                        cmc.get_tradfi_prices(&tradfi_refs)
                    )
                })
                .await
                {
                    Ok((q, o, t)) => (q, o, t),
                    Err(_) => {
                        tracing::warn!("market-data fetch timed out after 30s — using empty data");
                        (
                            Err(crate::error::BotError::CmcApi("timeout".into())),
                            Vec::new(),
                            Vec::new(),
                        )
                    }
                };

            let quotes_str = match &quotes_res {
                Ok(quotes) => {
                    let mut parts = Vec::new();
                    for q in quotes {
                        quote_signals.push(crate::crypto::ai::strategy::QuoteSignal {
                            symbol: q.symbol.clone(),
                            price: q.price_usd,
                            chg_24h: q.change_24h,
                            chg_7d: q.change_7d,
                        });
                        let mut yaml = format!(
                        "- Symbol: {}\n  Price_USD: {:.2}\n  Chg_1h: {:+.2}%\n  Chg_24h: {:+.2}%\n  Chg_7d: {:+.2}%",
                        q.symbol,
                        q.price_usd,
                        q.change_1h.unwrap_or(0.0),
                        q.change_24h.unwrap_or(0.0),
                        q.change_7d.unwrap_or(0.0)
                    );
                        if let Some(c30) = q.change_30d {
                            yaml.push_str(&format!("\n  Chg_30d: {:+.2}%", c30));
                        }
                        if let Some(c90) = q.change_90d {
                            yaml.push_str(&format!("\n  Chg_90d: {:+.2}%", c90));
                        }
                        if let Some(c365) = q.change_365d {
                            yaml.push_str(&format!("\n  Chg_365d: {:+.2}%", c365));
                        }
                        if let Some(c730) = q.change_730d {
                            yaml.push_str(&format!("\n  Chg_730d: {:+.2}%", c730));
                        }
                        if let Some(ath) = q.ath {
                            let disc = q.ath_change_percentage.unwrap_or_else(|| {
                                if ath > 0.0 {
                                    (q.price_usd - ath) / ath * 100.0
                                } else {
                                    0.0
                                }
                            });
                            yaml.push_str(&format!(
                                "\n  High_1Y: {:.2}\n  Discount_From_Peak: {:+.2}%",
                                ath, disc
                            ));
                        }
                        parts.push(yaml);
                    }
                    parts.join("\n")
                }
                Err(_) => "Quotes unavailable.".to_string(),
            };

            // Exact P&L per position: live quote vs average buy price. This is
            // the number the user actually cares about ("how much am I up?"),
            // computed from the same quotes the model already sees.
            let pnl_block = match &quotes_res {
                Ok(quotes) => store.read(user_id, |p| {
                    if p.holdings.is_empty() && p.purchases.is_empty() {
                        return String::new();
                    }
                    let holdings_by_sym = p.holdings_by_symbol();
                    let mut lines = Vec::new();
                    for (sym, amt) in &holdings_by_sym {
                        let Some(q) =
                            quotes.iter().find(|q| q.symbol.eq_ignore_ascii_case(sym))
                        else {
                            continue;
                        };
                        let Some(avg) = p.avg_buy_price(sym) else {
                            continue;
                        };
                        if avg <= 0.0 {
                            continue;
                        }
                        let pnl_pct = (q.price_usd - avg) / avg * 100.0;
                        let pnl_usd = (q.price_usd - avg) * amt;
                        lines.push(format!(
                            "• {sym}: {amt:.6} @ avg ${avg:.2} → now ${:.2} ({:+.1}%) | unrealized {:+.2} USD",
                            q.price_usd, pnl_pct, pnl_usd
                        ));
                    }
                    if lines.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "PORTFOLIO P&L (live quotes vs avg buy):\n{}",
                            lines.join("\n")
                        )
                    }
                }),
                Err(_) => String::new(),
            };
            let portfolio_section = if pnl_block.is_empty() {
                portfolio_context.to_string()
            } else {
                format!("{}\n\n{}", portfolio_context, pnl_block)
            };

            let mut onchain_str = String::from("ON_CHAIN_METRICS (Pre-fetched via APIs):\n");
            if onchain_res.is_empty() {
                onchain_str.push_str("Unavailable.");
            } else {
                for m in onchain_res {
                    let mut row = format!("- {}:", m.symbol);
                    if let Some(tvl) = m.tvl {
                        row.push_str(&format!(" TVL: {:.2}B USD |", tvl / 1_000_000_000.0));
                    }
                    if let Some(act) = m.active_addresses_24h {
                        row.push_str(&format!(" Active_Addresses_24h: {} |", act));
                    }
                    if let Some(fees) = m.fees_24h_native {
                        row.push_str(&format!(" Fees_24h_Native: {:.2} |", fees));
                    }
                    if let Some(fr) = m.funding_rate {
                        row.push_str(&format!(" Funding_Rate: {:+.3}% |", fr * 100.0));
                    }
                    if let Some(oi) = m.open_interest {
                        row.push_str(&format!(
                            " Open_Interest: {:.2}M {}",
                            oi / 1_000_000.0,
                            m.symbol
                        ));
                    }
                    onchain_str.push_str(&row);
                    onchain_str.push('\n');
                }
            }

            let mut tradfi_str = String::new();
            if !tradfi_res.is_empty() {
                tradfi_str.push_str("\nTRADFI RWA ANCHORS (Real-World Asset Prices):\n");
                for quote in tradfi_res {
                    let mut yaml = format!(
                        "- Symbol: {}\n  Real_Price_USD: {:.2}",
                        quote.symbol, quote.price
                    );
                    if let Some(c1d) = quote.change_1d {
                        yaml.push_str(&format!("\n  Real_Chg_1d: {:+.2}%", c1d));
                    }
                    if let Some(c7d) = quote.change_7d {
                        yaml.push_str(&format!("\n  Real_Chg_7d: {:+.2}%", c7d));
                    }
                    if let Some(c30d) = quote.change_30d {
                        yaml.push_str(&format!("\n  Real_Chg_30d: {:+.2}%", c30d));
                    }
                    if let Some(c90d) = quote.change_90d {
                        yaml.push_str(&format!("\n  Real_Chg_90d: {:+.2}%", c90d));
                    }
                    if let Some(c1y) = quote.change_1y {
                        yaml.push_str(&format!("\n  Real_Chg_1y: {:+.2}%", c1y));
                    }
                    tradfi_str.push_str(&yaml);
                    tradfi_str.push('\n');
                }
            }

            format!(
                "PORTFOLIO:\n{}\n\nCURRENT MARKET DATA:\nQuotes:\n{}\n\n{}\n{}\n\nGlobal Market Context:\nFear & Greed Index: {}\n{}",
                portfolio_section, quotes_str, onchain_str, tradfi_str, fear_greed, global_info
            )
        }
    };

    // Phase B — local market memory: the bot's own price history. The depth
    // follows the question's time horizon (week/month/quarter/year) instead of
    // a fixed 30d, so "how did BTC do this year" gets a year of context.
    // Phase C — deterministic strategy layer: regime rules that stop the model
    // from chasing peaks or panicking at spikes.
    if crate::crypto::ai::scenario::active_scenario().is_none() {
        let depth = question_depth_days(question);
        let traj =
            crate::crypto::ai::market_memory::trajectory_context(sqlite, symbols, depth).await;
        if !traj.is_empty() {
            market_data.push_str(&format!("\n\n{}", traj));
        }
        // Chart view: RSI / SMA / support-resistance / volatility from local
        // OHLCV so the model can reason about charts, not just % deltas.
        let chart =
            crate::crypto::ai::market_memory::chart_context_block(sqlite, symbols, depth).await;
        if !chart.is_empty() {
            market_data.push_str(&format!("\n\n{}", chart));
        }
        // Trained strategy: top configs from historical sweeps, so advice is
        // grounded in what worked for these symbols, not generic platitudes.
        let trained = crate::crypto::ai::market_memory::trained_strategy_block(symbols, 3).await;
        if !trained.is_empty() {
            market_data.push_str(&format!("\n\n{}", trained));
        }
        // Market calendar: halving phase + trained monthly seasonality, so the
        // model knows the cyclical regime ("N days after halving", "October
        // historically +X%") instead of only implicit window memory.
        let calendar = crate::crypto::ai::market_memory::seasonality_context_block(symbols).await;
        if !calendar.is_empty() {
            market_data.push_str(&format!("\n\n{}", calendar));
        }
        // Market locality: deterministic snapshot + engine/ensemble verdict at
        // the latest close — the "is 81k BTC a buy?" answer grounded in the
        // same OHLCV the backtests replay.
        let locality = crate::crypto::ai::market_memory::locality_context_block(symbols).await;
        if !locality.is_empty() {
            market_data.push_str(&format!("\n\n{}", locality));
        }
        // Regime & trained profiles: current regime per symbol + what the best
        // config for that regime historically earned — the "market is EUPHORIA,
        // bot historically earned +127% ETH with d15-tp3.0" grounding.
        let regime_block = crate::crypto::ai::market_memory::regime_context_block(symbols).await;
        if !regime_block.is_empty() {
            market_data.push_str(&format!("\n\n{}", regime_block));
        }
    }
    let strategy_block = if let Some(sc) = crate::crypto::ai::scenario::active_scenario() {
        crate::crypto::ai::strategy::guidance_from_scenario(sc)
    } else {
        crate::crypto::ai::strategy::guidance(
            crate::crypto::ai::strategy::parse_fear_greed(&fear_greed),
            &quote_signals,
        )
    };
    if !strategy_block.is_empty() {
        market_data.push_str(&format!("\n\n{}", strategy_block));
    }

    market_data
}

/// Pick the history depth (days) that matches the question's time horizon.
fn question_depth_days(question: &str) -> u32 {
    let q = question.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| q.contains(n));
    if has(&[
        "год", "года", "году", "year", "1y", "365", "12 month", "12m", "twelve", "annual",
        "52 week",
    ]) {
        365
    } else if has(&[
        "полгода",
        "пол-года",
        "6 month",
        "6m",
        "half year",
        "180",
        "26 week",
        "six month",
    ]) {
        180
    } else if has(&[
        "квартал",
        "quarter",
        "90",
        "3 month",
        "3m",
        "13 week",
        "three month",
    ]) {
        90
    } else if has(&[
        "месяц",
        "месяца",
        "месяцев",
        "мес",
        "month",
        "30",
        "4 week",
        "30d",
        "30 day",
    ]) {
        30
    } else if has(&["недел", "week", "7", "7d", "7 day"]) {
        7
    } else {
        30
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_depth_follows_time_horizon() {
        assert_eq!(question_depth_days("как биток за год"), 365);
        assert_eq!(question_depth_days("eth last 6 months"), 180);
        assert_eq!(question_depth_days("sol за квартал"), 90);
        assert_eq!(question_depth_days("link за месяц"), 30);
        assert_eq!(question_depth_days("btc last week"), 7);
        assert_eq!(question_depth_days("should I buy more SOL now"), 30);
    }
}
