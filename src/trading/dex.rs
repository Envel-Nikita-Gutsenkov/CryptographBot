//! DEX execution — legal on-ramp for Belarus P2P withdrawal (see
//! `docs/trading/dex-execution.md`). The bot never touches a CEX: quotes come
//! from aggregators (Jupiter on Solana, 0x/Uniswap on Ethereum), the operator
//! signs in their own wallet UI, and fills are reconciled from the executed
//! price with a slippage check.
//!
//! Flow: quote → guardrail re-check (engine re-runs plan_buy/plan_sell at the
//! quoted price) → operator signs → reconcile from the tx receipt.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Which chain the swap runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chain {
    Solana,
    Ethereum,
}

impl Chain {
    pub fn as_str(&self) -> &'static str {
        match self {
            Chain::Solana => "solana",
            Chain::Ethereum => "ethereum",
        }
    }
}

/// Unified quote from either aggregator.
#[derive(Debug, Clone, PartialEq)]
pub struct SwapQuote {
    pub chain: Chain,
    pub input_symbol: String,
    pub output_symbol: String,
    /// Input amount in input-token units.
    pub input_amount: f64,
    /// Output amount in output-token units.
    pub output_amount: f64,
    /// Price impact in percent (0.0 when the aggregator doesn't report it).
    pub price_impact_pct: f64,
    /// Human-readable route summary (aggregator-provided or synthesized).
    pub route: String,
}

impl SwapQuote {
    /// Implied fill price: output per 1 input unit.
    pub fn implied_price(&self) -> f64 {
        if self.input_amount <= 0.0 {
            0.0
        } else {
            self.output_amount / self.input_amount
        }
    }
}

/// A reconciled fill — what actually executed on-chain.
#[derive(Debug, Clone, PartialEq)]
pub struct FillRecord {
    pub symbol: String,
    pub side: SwapSide,
    pub amount: f64,
    pub executed_price: f64,
    pub quoted_price: f64,
    /// Deviation of the executed price from the quote, in percent.
    pub slippage_pct: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapSide {
    Buy,
    Sell,
}

impl SwapSide {
    pub fn as_str(&self) -> &'static str {
        match self {
            SwapSide::Buy => "buy",
            SwapSide::Sell => "sell",
        }
    }
}

/// Token address mapping for the portfolio symbols on each chain.
/// USDC is the quote token on both chains.
pub fn token_address(chain: Chain, symbol: &str) -> Option<&'static str> {
    match chain {
        Chain::Solana => match symbol.to_uppercase().as_str() {
            "SOL" => Some("So11111111111111111111111111111111111111112"),
            "USDC" => Some("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            _ => None,
        },
        Chain::Ethereum => match symbol.to_uppercase().as_str() {
            "ETH" => Some("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"), // WETH
            "LINK" => Some("0x514910771af9ca656af840dff83e8264ecf986ca"),
            "USDC" => Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
            _ => None,
        },
    }
}

// ---------------------------------------------------------------------------
// Jupiter (Solana) — public quote API, no key required.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JupiterQuoteJson {
    #[serde(default)]
    in_amount: String,
    #[serde(default)]
    out_amount: String,
    #[serde(default)]
    price_impact_pct: String,
    #[serde(default)]
    route_plan: Vec<JupiterRouteStep>,
}

#[derive(Debug, Deserialize)]
struct JupiterRouteStep {
    #[serde(default)]
    #[allow(dead_code)] // parsed from the API response; kept for route detail display
    percent: u8,
}

/// Parse a Jupiter `/v6/quote` response into a unified `SwapQuote`.
pub fn parse_jupiter_quote(
    json: &str,
    input_symbol: &str,
    output_symbol: &str,
) -> Result<SwapQuote> {
    let q: JupiterQuoteJson = serde_json::from_str(json)
        .context("jupiter quote: malformed JSON (is the API rate-limited?)")?;
    let in_amount = q
        .in_amount
        .parse::<f64>()
        .context("jupiter quote: inAmount not numeric")?;
    let out_amount = q
        .out_amount
        .parse::<f64>()
        .context("jupiter quote: outAmount not numeric")?;
    let impact = q.price_impact_pct.parse::<f64>().unwrap_or(0.0);
    let route = if q.route_plan.is_empty() {
        "direct".to_string()
    } else {
        format!("{} route step(s)", q.route_plan.len())
    };
    Ok(SwapQuote {
        chain: Chain::Solana,
        input_symbol: input_symbol.to_uppercase(),
        output_symbol: output_symbol.to_uppercase(),
        input_amount: in_amount,
        output_amount: out_amount,
        price_impact_pct: impact,
        route,
    })
}

/// Fetch a Jupiter quote. `amount` is in the input token's smallest unit
/// (lamports for SOL, 1e6 units for USDC). `slippage_bps` defaults to 100
/// (1%).
pub async fn jupiter_quote(
    input_symbol: &str,
    output_symbol: &str,
    amount: u64,
    slippage_bps: u16,
) -> Result<SwapQuote> {
    let input_mint = token_address(Chain::Solana, input_symbol)
        .with_context(|| format!("no Solana token mapping for {input_symbol}"))?;
    let output_mint = token_address(Chain::Solana, output_symbol)
        .with_context(|| format!("no Solana token mapping for {output_symbol}"))?;
    let url = format!(
        "https://quote-api.jup.ag/v6/quote?inputMint={input_mint}&outputMint={output_mint}\
         &amount={amount}&slippageBps={slippage_bps}"
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .use_rustls_tls()
        .build()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .context("jupiter quote: request failed")?;
    if !resp.status().is_success() {
        bail!("jupiter quote: HTTP {}", resp.status());
    }
    let text = resp
        .text()
        .await
        .context("jupiter quote: read body failed")?;
    parse_jupiter_quote(&text, input_symbol, output_symbol)
}

// ---------------------------------------------------------------------------
// 0x Swap API (Ethereum) — aggregates Uniswap v2/v3 + friends.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ZeroExQuoteJson {
    #[serde(default)]
    sell_amount: String,
    #[serde(default)]
    buy_amount: String,
    #[serde(default)]
    #[allow(dead_code)] // parsed from the API response; kept for price-impact detail
    price: String,
    #[serde(default)]
    sources: Vec<serde_json::Value>,
}

/// Parse a 0x `/swap/v1/quote` response into a unified `SwapQuote`.
pub fn parse_zeroex_quote(
    json: &str,
    input_symbol: &str,
    output_symbol: &str,
) -> Result<SwapQuote> {
    let q: ZeroExQuoteJson = serde_json::from_str(json)
        .context("0x quote: malformed JSON (is the API rate-limited?)")?;
    let sell = q
        .sell_amount
        .parse::<f64>()
        .context("0x quote: sellAmount not numeric")?;
    let buy = q
        .buy_amount
        .parse::<f64>()
        .context("0x quote: buyAmount not numeric")?;
    let route = if q.sources.is_empty() {
        "uniswap".to_string()
    } else {
        format!("{} source(s)", q.sources.len())
    };
    Ok(SwapQuote {
        chain: Chain::Ethereum,
        input_symbol: input_symbol.to_uppercase(),
        output_symbol: output_symbol.to_uppercase(),
        input_amount: sell,
        output_amount: buy,
        // 0x doesn't report price impact; the caller compares implied price
        // against the engine's expected price instead.
        price_impact_pct: 0.0,
        route,
    })
}

/// Fetch a 0x quote. `amount` is in the input token's smallest unit (1e18 for
/// ETH/LINK, 1e6 for USDC). Requires the optional `ZEROEX_API_KEY` env var;
/// without it the public rate limit applies.
pub async fn zeroex_quote(
    input_symbol: &str,
    output_symbol: &str,
    amount: u64,
) -> Result<SwapQuote> {
    let sell_token = token_address(Chain::Ethereum, input_symbol)
        .with_context(|| format!("no Ethereum token mapping for {input_symbol}"))?;
    let buy_token = token_address(Chain::Ethereum, output_symbol)
        .with_context(|| format!("no Ethereum token mapping for {output_symbol}"))?;
    let url = format!(
        "https://api.0x.org/swap/v1/quote?buyToken={buy_token}&sellToken={sell_token}\
         &sellAmount={amount}"
    );
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .use_rustls_tls()
        .build()?;
    let mut req = client.get(&url);
    if let Ok(key) = std::env::var("ZEROEX_API_KEY") {
        if !key.is_empty() && !key.starts_with("xxxxxx") {
            req = req.header("0x-api-key", key);
        }
    }
    let resp = req.send().await.context("0x quote: request failed")?;
    if !resp.status().is_success() {
        bail!("0x quote: HTTP {}", resp.status());
    }
    let text = resp.text().await.context("0x quote: read body failed")?;
    parse_zeroex_quote(&text, input_symbol, output_symbol)
}

// ---------------------------------------------------------------------------
// Blocking entry point + manual sign mode + fill reconciliation.
// ---------------------------------------------------------------------------

/// Blocking quote for a buy (input = USDC) or sell (input = SYMBOL).
/// `amount` is in the input token's smallest unit.
pub fn quote_blocking(
    chain: Chain,
    symbol: &str,
    side: SwapSide,
    amount: u64,
) -> Result<SwapQuote> {
    let (input, output) = match side {
        SwapSide::Buy => ("USDC", symbol),
        SwapSide::Sell => (symbol, "USDC"),
    };
    match chain {
        Chain::Solana => futures::executor::block_on(jupiter_quote(input, output, amount, 100)),
        Chain::Ethereum => futures::executor::block_on(zeroex_quote(input, output, amount)),
    }
}

/// Manual sign mode: render the swap payload the operator confirms in their
/// wallet UI (Phantom / MetaMask). The bot never holds the private key.
pub fn build_swap_payload(quote: &SwapQuote) -> String {
    format!(
        "Confirm swap on {}:\n  {} {} -> {} {}\n  implied price: 1 {} = {:.8} {}\n  price impact: {:.2}%\n  route: {}\n  Sign in your wallet UI (Phantom/MetaMask).",
        quote.chain.as_str(),
        quote.input_amount,
        quote.input_symbol,
        quote.output_amount,
        quote.output_symbol,
        quote.input_symbol,
        quote.implied_price(),
        quote.output_symbol,
        quote.price_impact_pct,
        quote.route,
    )
}

/// Reconcile a fill from the executed price. Fails if the executed price
/// deviates from the quote by more than `max_slippage_pct` (default 2%).
pub fn reconcile_fill(
    quote: &SwapQuote,
    side: SwapSide,
    executed_price: f64,
    max_slippage_pct: f64,
) -> Result<FillRecord> {
    // The operator reports the symbol's price in quote-token (USDC) units.
    // For a buy the symbol is the output (USDC per symbol = input/output);
    // for a sell it is the input (USDC per symbol = output/input).
    let quoted = match side {
        SwapSide::Buy => quote.input_amount / quote.output_amount,
        SwapSide::Sell => quote.output_amount / quote.input_amount,
    };
    if !quoted.is_finite() || quoted <= 0.0 || executed_price <= 0.0 {
        bail!("reconcile: prices must be positive (quoted {quoted}, executed {executed_price})");
    }
    // For a buy the executed price must not exceed the quote; for a sell it
    // must not be below it. Slippage is always measured as a positive %.
    let slippage = match side {
        SwapSide::Buy => (executed_price - quoted) / quoted * 100.0,
        SwapSide::Sell => (quoted - executed_price) / quoted * 100.0,
    };
    if slippage > max_slippage_pct {
        bail!(
            "reconcile: executed price deviates {slippage:.2}% from quote \
             (max {max_slippage_pct:.2}%) — refusing to record the fill"
        );
    }
    Ok(FillRecord {
        symbol: quote.output_symbol.clone(),
        side,
        amount: quote.output_amount,
        executed_price,
        quoted_price: quoted,
        slippage_pct: slippage.max(0.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jupiter_parse_maps_fields() {
        let json = r#"{
            "inputMint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "inAmount": "1000000",
            "outputMint": "So11111111111111111111111111111111111111112",
            "outAmount": "50000000",
            "otherAmountThreshold": "49900000",
            "swapMode": "ExactIn",
            "slippageBps": 100,
            "priceImpactPct": "0.42",
            "routePlan": [{"swapInfo": {"ammKey": "x"}, "percent": 100}]
        }"#;
        let q = parse_jupiter_quote(json, "USDC", "SOL").unwrap();
        assert_eq!(q.chain, Chain::Solana);
        assert_eq!(q.input_symbol, "USDC");
        assert_eq!(q.output_symbol, "SOL");
        assert_eq!(q.input_amount, 1_000_000.0);
        assert_eq!(q.output_amount, 50_000_000.0);
        assert!((q.price_impact_pct - 0.42).abs() < 1e-9);
        assert!(q.route.contains("1 route step"));
        // 1 USDC = 50 SOL (in smallest units — the caller scales by decimals).
        assert!((q.implied_price() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn jupiter_parse_handles_empty_route_and_bad_impact() {
        let json =
            r#"{"inAmount":"1000","outAmount":"2000","priceImpactPct":"n/a","routePlan":[]}"#;
        let q = parse_jupiter_quote(json, "USDC", "SOL").unwrap();
        assert_eq!(q.route, "direct");
        assert_eq!(q.price_impact_pct, 0.0);
    }

    #[test]
    fn zeroex_parse_maps_fields() {
        let json = r#"{
            "price": "0.001",
            "guaranteedPrice": "0.00099",
            "to": "0xdef1c0ded9bec7f1a1670819833240f027b25eff",
            "data": "0x",
            "value": "0",
            "gas": "150000",
            "estimatedGas": "120000",
            "gasPrice": "1000000000",
            "protocolFee": "0",
            "minimumProtocolFee": "0",
            "buyAmount": "1000000000000000000",
            "sellAmount": "1000000000000000000000",
            "sources": [{"name": "Uniswap_V3", "proportion": "1"}],
            "allowanceTarget": "0xdef1c0ded9bec7f1a1670819833240f027b25eff",
            "sellTokenAddress": "0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2",
            "buyTokenAddress": "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        }"#;
        let q = parse_zeroex_quote(json, "ETH", "USDC").unwrap();
        assert_eq!(q.chain, Chain::Ethereum);
        assert_eq!(q.input_symbol, "ETH");
        assert_eq!(q.output_symbol, "USDC");
        assert_eq!(q.input_amount, 1e21);
        assert_eq!(q.output_amount, 1e18);
        assert!(q.route.contains("1 source"));
    }

    #[test]
    fn token_addresses_cover_portfolio() {
        assert!(token_address(Chain::Solana, "SOL").is_some());
        assert!(token_address(Chain::Solana, "USDC").is_some());
        assert!(token_address(Chain::Ethereum, "ETH").is_some());
        assert!(token_address(Chain::Ethereum, "LINK").is_some());
        assert!(token_address(Chain::Ethereum, "USDC").is_some());
        assert!(token_address(Chain::Solana, "BTC").is_none());
        assert!(token_address(Chain::Ethereum, "SOL").is_none());
    }

    #[test]
    fn payload_is_operator_facing() {
        let q = SwapQuote {
            chain: Chain::Solana,
            input_symbol: "USDC".into(),
            output_symbol: "SOL".into(),
            input_amount: 100.0,
            output_amount: 2.5,
            price_impact_pct: 0.5,
            route: "2 route steps".into(),
        };
        let p = build_swap_payload(&q);
        assert!(p.contains("solana"));
        assert!(p.contains("100 USDC"));
        assert!(p.contains("2.5 SOL"));
        assert!(p.contains("Phantom/MetaMask"));
    }

    #[test]
    fn reconcile_accepts_within_slippage_rejects_beyond() {
        let q = SwapQuote {
            chain: Chain::Ethereum,
            input_symbol: "USDC".into(),
            output_symbol: "ETH".into(),
            input_amount: 1000.0,
            output_amount: 0.5,
            price_impact_pct: 0.0,
            route: "uniswap".into(),
        };
        // Quoted 1 ETH = 2000 USDC. Executed at 2020 (buy) -> 1% slippage, ok.
        let f = reconcile_fill(&q, SwapSide::Buy, 2020.0, 2.0).unwrap();
        assert_eq!(f.symbol, "ETH");
        assert!((f.slippage_pct - 1.0).abs() < 1e-9);
        // Executed at 2100 (buy) -> 5% slippage, rejected.
        assert!(reconcile_fill(&q, SwapSide::Buy, 2100.0, 2.0).is_err());
        // Sell side: the quote is inverted (input ETH, output USDC).
        let sell_q = SwapQuote {
            chain: Chain::Ethereum,
            input_symbol: "ETH".into(),
            output_symbol: "USDC".into(),
            input_amount: 0.5,
            output_amount: 1000.0,
            price_impact_pct: 0.0,
            route: "uniswap".into(),
        };
        // Executed below quote by 1% is ok.
        let f = reconcile_fill(&sell_q, SwapSide::Sell, 1980.0, 2.0).unwrap();
        assert!((f.slippage_pct - 1.0).abs() < 1e-9);
        // Sell executed above quote -> negative slippage, clamped to 0.
        let f = reconcile_fill(&sell_q, SwapSide::Sell, 2100.0, 2.0).unwrap();
        assert_eq!(f.slippage_pct, 0.0);
    }

    #[test]
    fn reconcile_rejects_zero_prices() {
        let q = SwapQuote {
            chain: Chain::Solana,
            input_symbol: "USDC".into(),
            output_symbol: "SOL".into(),
            input_amount: 100.0,
            output_amount: 0.0,
            price_impact_pct: 0.0,
            route: "direct".into(),
        };
        assert!(reconcile_fill(&q, SwapSide::Buy, 1.0, 2.0).is_err());
    }
}
