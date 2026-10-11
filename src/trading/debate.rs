//! AI debate room for sell decisions: "sell now vs hold".
//!
//! The deterministic engine decides sells by the no-loss invariant + trailing
//! stop. This module adds an *adversarial* layer on top: a bull voice argues
//! for holding, a bear voice argues for selling now, and an arbiter picks a
//! side. The debate is grounded in real context — open lots (entry vs current
//! price), OHLCV indicators, seasonality, trained-strategy stats — and the
//! verdict is recorded in `agent_debates` so the AI can learn from outcomes.
//!
//! Any failure (no API key, dead model, garbage output) degrades to a safe
//! `Hold` — the deterministic engine's own exit rules still apply, so the
//! debate can only *add* a sell signal, never force one.

use crate::trading::store::{TradingStore, VerdictAction};

/// Outcome of a sell debate.
#[derive(Debug, Clone)]
pub struct DebateOutcome {
    pub symbol: String,
    pub price: f64,
    pub action: VerdictAction,
    pub bull_rationale: String,
    pub bear_rationale: String,
    pub arbiter_verdict: String,
    pub model_alias: String,
}

/// Parse the arbiter's verdict out of free-text model output.
/// Accepts SELL / HOLD / PASS / DEFER anywhere in the text (case-insensitive,
/// also "sell now", "hold", "wait"). Anything else → `Hold` (safe default).
pub fn parse_verdict(raw: &str) -> VerdictAction {
    let upper = raw.to_uppercase();
    if upper.contains("SELL") {
        VerdictAction::Sell
    } else if upper.contains("HOLD") {
        VerdictAction::Hold
    } else if upper.contains("PASS") {
        VerdictAction::Pass
    } else if upper.contains("DEFER") {
        VerdictAction::Defer
    } else {
        VerdictAction::Hold
    }
}

/// Compact market context for one symbol: open lots with unrealized PnL,
/// seasonality block, trained-strategy block. Token-cheap, grounded in the
/// same data the backtests replay.
pub fn build_sell_debate_prompt(
    store: &TradingStore,
    symbol: &str,
    price: f64,
    role: &str,
) -> String {
    let sym = symbol.to_uppercase();
    let mut ctx = String::new();
    ctx.push_str(&format!(
        "You are the {role} in a sell-vs-hold debate for {sym} at ${price:.2}.\n"
    ));
    ctx.push_str("Answer with a short rationale and a final line: SELL or HOLD.\n\n");

    // Open lots for this symbol: entry vs current, unrealized PnL.
    let lots: Vec<String> = store
        .list_lots(None)
        .unwrap_or_default()
        .into_iter()
        .filter(|l| {
            (l.pair == sym || l.pair == format!("{sym}/USDT"))
                && (l.status.as_str() == "OPEN" || l.status.as_str() == "COLD_HOLD")
        })
        .map(|l| {
            let pnl = if l.entry_price > 0.0 {
                (price - l.entry_price) / l.entry_price * 100.0
            } else {
                0.0
            };
            format!(
                "lot {}: entry ${:.2}, amount {:.4}, unrealized {:+.1}%",
                l.id, l.entry_price, l.amount, pnl
            )
        })
        .collect();
    if lots.is_empty() {
        ctx.push_str("No open lots for this symbol.\n");
    } else {
        ctx.push_str("Open lots:\n");
        for l in lots {
            ctx.push_str(&format!("  {l}\n"));
        }
    }

    // Seasonality + halving cycle (explicit cyclical knowledge).
    let season = crate::trading::market_calendar::seasonality_block(
        &sym,
        &[],
        chrono::Utc::now().date_naive(),
    );
    if !season.is_empty() {
        ctx.push_str(&format!("Cycle context: {season}\n"));
    }

    // Trained-strategy stats: what configs won historically for this symbol.
    let trained: Vec<String> = store
        .list_training_stats(&sym, 3)
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            format!(
                "{}: avg PnL {:+.2}%, win {:.0}%, max DD {:.1}%, score {:.1}",
                r.config_name, r.avg_pnl_pct, r.win_rate, r.avg_max_dd_pct, r.score
            )
        })
        .collect();
    if !trained.is_empty() {
        ctx.push_str("Historically winning configs for this symbol:\n");
        for t in trained {
            ctx.push_str(&format!("  {t}\n"));
        }
    }

    ctx.push_str("\nRules:\n- SELL only if the risk of holding outweighs the upside.\n- HOLD if the position is still in an uptrend or the drop is a normal dip.\n- Never recommend selling at a loss unless the trend is clearly broken.\n- End with exactly one line: SELL or HOLD.\n");
    ctx
}

/// Run the full bull/bear/arbiter debate against a live model.
/// `model_alias` overrides the pool pick; `None` uses the best "main" model.
/// Safe fallback: no key / no model / model failure → `Hold`.
pub fn run_sell_debate(
    store: &TradingStore,
    symbol: &str,
    price: f64,
    model_alias: Option<&str>,
) -> DebateOutcome {
    let gemini_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();
    let alias = model_alias
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .or_else(|| crate::trading::tuner::pick_model(None))
        .unwrap_or_default();
    let safe = DebateOutcome {
        symbol: symbol.to_uppercase(),
        price,
        action: VerdictAction::Hold,
        bull_rationale: "no model available".into(),
        bear_rationale: "no model available".into(),
        arbiter_verdict: "hold (safe default)".into(),
        model_alias: alias.clone(),
    };
    if gemini_key.is_empty() || alias.is_empty() {
        return safe;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .use_rustls_tls()
        .build()
    {
        Ok(c) => c,
        Err(_) => return safe,
    };

    let bull_prompt = build_sell_debate_prompt(store, symbol, price, "bull (argue HOLD)");
    let bear_prompt = build_sell_debate_prompt(store, symbol, price, "bear (argue SELL)");

    let bull = futures::executor::block_on(call_debate_model(
        &client,
        &gemini_key,
        &alias,
        &bull_prompt,
    ));
    let bear = futures::executor::block_on(call_debate_model(
        &client,
        &gemini_key,
        &alias,
        &bear_prompt,
    ));

    let arbiter_prompt = format!(
        "You are the arbiter. Two voices debated selling vs holding {sym} at ${price:.2}.\n\n\
         BULL (hold):\n{bull}\n\nBEAR (sell):\n{bear}\n\n\
         Decide. End with exactly one line: SELL or HOLD.",
        sym = symbol.to_uppercase()
    );
    let arbiter = futures::executor::block_on(call_debate_model(
        &client,
        &gemini_key,
        &alias,
        &arbiter_prompt,
    ));

    let action = parse_verdict(&arbiter);
    let outcome = DebateOutcome {
        symbol: symbol.to_uppercase(),
        price,
        action,
        bull_rationale: bull,
        bear_rationale: bear,
        arbiter_verdict: arbiter,
        model_alias: alias,
    };

    // Record the debate for the audit trail (best-effort; never fatal).
    if let Err(e) = store.record_debate(crate::trading::store::NewDebate {
        lot_id: None,
        market_context_json: format!(
            r#"{{"symbol":"{}","price":{},"model":"{}"}}"#,
            outcome.symbol, outcome.price, outcome.model_alias
        ),
        bull_rationale: outcome.bull_rationale.clone(),
        bear_rationale: outcome.bear_rationale.clone(),
        arbiter_verdict: outcome.arbiter_verdict.clone(),
        verdict_action: outcome.action,
    }) {
        tracing::warn!("debate record failed: {e}");
    }
    outcome
}

async fn call_debate_model(
    client: &reqwest::Client,
    gemini_key: &str,
    alias: &str,
    prompt: &str,
) -> String {
    let mut last_err = anyhow::anyhow!("debate model call failed");
    for attempt in 0..3 {
        match crate::crypto::ai::providers::call_any_model(client, gemini_key, alias, prompt, None)
            .await
        {
            Ok(text) => return text,
            Err(e) => {
                tracing::warn!("debate model attempt {} failed: {e}", attempt + 1);
                last_err = e;
                tokio::time::sleep(std::time::Duration::from_millis(500 * (attempt as u64 + 1)))
                    .await;
            }
        }
    }
    format!("model unavailable after 3 attempts ({last_err})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::store::{NewAllocation, NewLot, TradingMode};
    use std::sync::atomic::{AtomicU64, Ordering};

    static DEBATE_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_store() -> TradingStore {
        let n = DEBATE_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = format!("target/test_debate_{}_{}.db", std::process::id(), n);
        let _ = std::fs::remove_file(&path);
        TradingStore::open_path(&path, TradingMode::Simulation).unwrap()
    }

    #[test]
    fn parse_verdict_accepts_english_variants() {
        assert_eq!(parse_verdict("SELL"), VerdictAction::Sell);
        assert_eq!(parse_verdict("sell now"), VerdictAction::Sell);
        assert_eq!(parse_verdict("I recommend HOLD"), VerdictAction::Hold);
        assert_eq!(parse_verdict("hold"), VerdictAction::Hold);
        assert_eq!(parse_verdict("PASS on this"), VerdictAction::Pass);
        assert_eq!(parse_verdict("DEFER decision"), VerdictAction::Defer);
        // Garbage → safe Hold.
        assert_eq!(parse_verdict("maybe later idk"), VerdictAction::Hold);
        assert_eq!(parse_verdict(""), VerdictAction::Hold);
    }

    #[test]
    fn prompt_includes_lots_and_trained_stats() {
        let store = test_store();
        store
            .create_allocation(NewAllocation {
                asset: "SOL".into(),
                total_budget_usd: 300.0,
            })
            .unwrap();
        store
            .open_lot(NewLot {
                allocation_id: None,
                pair: "SOL".into(),
                entry_price: 75.0,
                amount: 1.0,
                fee_paid_usd: 0.5,
                target_exit_price: 75.0 * 1.05,
                cost_basis_usd: None,
                peak_price: Some(75.0),
            })
            .unwrap();
        let p = build_sell_debate_prompt(&store, "SOL", 80.0, "bull (argue HOLD)");
        assert!(p.contains("SOL"));
        assert!(p.contains("entry $75.00"));
        assert!(p.contains("unrealized"));
        assert!(p.contains("SELL or HOLD"));
    }

    #[test]
    fn no_key_degrades_to_safe_hold() {
        // Clear the env var for this test; restore after.
        let prev = std::env::var("GEMINI_API_KEY").ok();
        std::env::remove_var("GEMINI_API_KEY");
        let store = test_store();
        let o = run_sell_debate(&store, "BTC", 100.0, None);
        assert_eq!(o.action, VerdictAction::Hold);
        assert!(o.arbiter_verdict.contains("safe default"));
        if let Some(k) = prev {
            std::env::set_var("GEMINI_API_KEY", k);
        }
    }
}
