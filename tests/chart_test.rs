use cryptograph::crypto::charts;

#[tokio::test]
async fn test_generate_chart() {
    let base_time = 1717750000000i64; // millisecond timestamp
    let points = vec![
        (base_time, 60000.0),
        (base_time + 3600_000, 61000.0),
        (base_time + 7200_000, 59500.0),
        (base_time + 10800_000, 63000.0),
        (base_time + 14400_000, 62307.41),
    ];

    println!("Generating chart...");
    let chart_bytes = charts::ChartRenderer::historical_price_chart("BTC", &points, "1d")
        .await
        .unwrap();

    let output_path = "chart_test_output.png";
    std::fs::write(output_path, chart_bytes).unwrap();
    println!("Chart saved to {}", output_path);
}
