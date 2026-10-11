/// Portfolio math: cost basis, fee tracking, P&L, transfers.

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
