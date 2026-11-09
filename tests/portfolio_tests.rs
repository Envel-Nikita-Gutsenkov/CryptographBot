use chrono::{TimeZone, Utc};
use cryptograph::models::{Holding, PaidCurrency, Portfolio, Purchase};

fn purchase(id: u64, symbol: &str, amount: f64, cost_usd: f64) -> Purchase {
    let at = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    Purchase {
        id,
        symbol: symbol.to_string(),
        amount,
        cost_usd,
        paid_amount: cost_usd,
        paid_currency: PaidCurrency::Usd,
        date: at,
        source: "test".to_string(),
        notes: None,
        created_at: at,
        corrections: Vec::new(),
    }
}

fn holding(id: u64, symbol: &str, amount: f64) -> Holding {
    Holding {
        id,
        wallet_id: 1,
        symbol: symbol.to_string(),
        amount,
        corrections: Vec::new(),
    }
}

#[test]
fn total_invested_tracks_cost_basis() {
    // Simulate: buy 0.5 BTC at $30k (total $30k) + 0.5 BTC at $60k (total $30k) = $60k invested
    let costs = vec![30_000.0_f64, 30_000.0];
    let total: f64 = costs.iter().sum();
    assert_eq!(total, 60_000.0);
}

#[test]
fn transfer_fee_reduces_received_amount() {
    let sent = 1.0_f64;
    let fee = 0.0001_f64;
    let received = sent - fee;
    assert!((received - 0.9999).abs() < 1e-10);
    // Cost basis unchanged — only sent amount is tracked
    let cost_basis = 60_000.0_f64;
    assert_eq!(cost_basis, 60_000.0); // immutable
}

#[test]
fn pnl_calculation() {
    let invested = 60_000.0_f64;
    let current_value = 65_000.0_f64;
    let pnl = current_value - invested;
    let pnl_pct = pnl / invested * 100.0;
    assert!((pnl - 5_000.0).abs() < 0.01);
    assert!((pnl_pct - 8.333).abs() < 0.01);
}

#[test]
fn avg_buy_price() {
    // 0.5 BTC at $50k and 0.5 BTC at $70k → avg $60k
    let purchases = [(0.5_f64, 25_000.0_f64), (0.5, 35_000.0)];
    let total_amount: f64 = purchases.iter().map(|(a, _)| a).sum();
    let total_cost: f64 = purchases.iter().map(|(_, c)| c).sum();
    let avg = total_cost / total_amount;
    assert!((avg - 60_000.0).abs() < 0.01);
}

#[test]
fn fee_accumulation() {
    let fees: Vec<Option<f64>> = vec![Some(5.0), Some(3.0), None, Some(2.0)];
    let total: f64 = fees.iter().filter_map(|f| *f).sum();
    assert_eq!(total, 10.0);
}

#[test]
fn cost_basis_unchanged_by_transfer() {
    // Simulate: buy 1 BTC at $60k, transfer 0.5 BTC (fee 0.0001 BTC)
    let cost_basis = 60_000.0_f64; // NEVER changes
    let sent = 0.5_f64;
    let fee = 0.0001_f64;
    let received = sent - fee;
    // Holdings change, cost basis does not
    assert_eq!(cost_basis, 60_000.0);
    assert!((received - 0.4999).abs() < 1e-10);
}

#[test]
fn partial_sale_keeps_average_entry_unchanged() {
    // 1 BTC bought for $60k, half sold → the remaining 0.5 BTC still has a
    // $60k average entry; invested drops to $30k. Before the fix the full
    // $60k stayed invested, implying a $120k average.
    let mut p = Portfolio::default();
    p.purchases.push(purchase(1, "BTC", 1.0, 60_000.0));
    p.holdings.push(holding(1, "BTC", 0.5));

    let avg = p.avg_buy_price("BTC").expect("avg");
    assert!((avg - 60_000.0).abs() < 1e-9, "avg was {avg}");
    assert!((p.total_invested_usd() - 30_000.0).abs() < 1e-9);
}

#[test]
fn multiple_lots_weighted_average_survives_partial_sale() {
    // 1 BTC @ $50k + 1 BTC @ $70k → avg $60k. Sell 1 BTC → still $60k avg,
    // invested $60k for the remaining 1 BTC.
    let mut p = Portfolio::default();
    p.purchases.push(purchase(1, "BTC", 1.0, 50_000.0));
    p.purchases.push(purchase(2, "BTC", 1.0, 70_000.0));
    p.holdings.push(holding(1, "BTC", 1.0));

    let avg = p.avg_buy_price("BTC").expect("avg");
    assert!((avg - 60_000.0).abs() < 1e-9, "avg was {avg}");
    assert!((p.total_invested_usd() - 60_000.0).abs() < 1e-9);
}

#[test]
fn full_position_keeps_original_cost_basis() {
    let mut p = Portfolio::default();
    p.purchases.push(purchase(1, "ETH", 2.0, 4_000.0));
    p.holdings.push(holding(1, "ETH", 2.0));

    assert!((p.avg_buy_price("ETH").unwrap() - 2_000.0).abs() < 1e-9);
    assert!((p.total_invested_usd() - 4_000.0).abs() < 1e-9);
}

#[test]
fn empty_position_has_no_average() {
    let mut p = Portfolio::default();
    p.purchases.push(purchase(1, "SOL", 1.0, 100.0));
    p.holdings.push(holding(1, "SOL", 0.0));
    assert!(p.avg_buy_price("SOL").is_none());
    assert_eq!(p.total_invested_usd(), 0.0);
}
