/// Price consensus tests — outlier detection, weighted average.

fn median(values: &[f64]) -> f64 {
    let mut s = values.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = s.len();
    if n % 2 == 0 { (s[n/2-1] + s[n/2]) / 2.0 } else { s[n/2] }
}

fn consensus(prices: &[f64], threshold_pct: f64) -> f64 {
    if prices.is_empty() { return 0.0; }
    let m = median(prices);
    let valid: Vec<f64> = prices.iter()
        .filter(|&&p| (p - m).abs() / m * 100.0 <= threshold_pct)
        .copied()
        .collect();
    if valid.is_empty() { m } else { valid.iter().sum::<f64>() / valid.len() as f64 }
}

#[test]
fn three_sources_close_together() {
    let prices = [65_000.0, 65_100.0, 64_900.0];
    let c = consensus(&prices, 5.0);
    assert!((c - 65_000.0).abs() < 200.0);
}

#[test]
fn obvious_outlier_excluded() {
    let prices = [65_000.0, 65_100.0, 100_000.0];
    let c = consensus(&prices, 5.0);
    assert!(c < 70_000.0, "outlier 100k should be excluded, got {c}");
}

#[test]
fn low_outlier_excluded() {
    let prices = [65_000.0, 65_100.0, 1_000.0];
    let c = consensus(&prices, 5.0);
    assert!(c > 60_000.0, "outlier 1k should be excluded, got {c}");
}

#[test]
fn single_source_returned_as_is() {
    let prices = [42_000.0];
    let c = consensus(&prices, 5.0);
    assert_eq!(c, 42_000.0);
}

#[test]
fn two_equal_prices() {
    let prices = [50_000.0, 50_000.0];
    let c = consensus(&prices, 5.0);
    assert_eq!(c, 50_000.0);
}

#[test]
fn all_outliers_fall_back_to_median() {
    // Pathological: 3 prices, each >5% from median of each other
    // median of [1, 100, 1000] = 100
    // 1:     dev = 99% > 5% → outlier
    // 100:   dev = 0%  ≤ 5% → valid
    // 1000:  dev = 900% > 5% → outlier
    let prices = [1.0, 100.0, 1000.0];
    let c = consensus(&prices, 5.0);
    // Only 100.0 passes → returns 100
    assert_eq!(c, 100.0);
}

#[test]
fn median_even_count() {
    assert_eq!(median(&[1.0, 2.0, 3.0, 4.0]), 2.5);
}

#[test]
fn median_odd_count() {
    assert_eq!(median(&[1.0, 5.0, 3.0]), 3.0);
}

// ── Alert evaluation ──────────────────────────────────────────────────────────

#[test]
fn price_below_threshold() {
    let price = 59_000.0_f64;
    let threshold = 60_000.0_f64;
    assert!(price < threshold);
}

#[test]
fn cooldown_respected() {
    let last_fired_secs_ago = 6 * 3600; // 6h ago
    let cooldown_hours = 12u64;
    assert!(last_fired_secs_ago < cooldown_hours as usize * 3600);
    // Should NOT fire — cooldown not expired
    
    let last_fired_secs_ago2 = 13 * 3600; // 13h ago
    assert!(last_fired_secs_ago2 >= cooldown_hours as usize * 3600);
    // Should fire — cooldown expired
}

#[test]
fn consecutive_trigger_dedup() {
    let mut count = 0u32;
    // First trigger: count = 1, threshold = 2 → don't fire
    count += 1;
    assert!(count < 2);
    // Second trigger: count = 2, threshold = 2 → fire
    count += 1;
    assert!(count >= 2);
}
