use cryptograph::crypto::ai::{
    pipeline::run_dynamic_critic,
    prompts::build_moe_critic_prompt,
    search::{BudgetManager, SearchBudget},
};
use cryptograph::db::sqlite::SqliteStore;
use dotenvy::dotenv;
use reqwest::Client;
use std::sync::Arc;

#[tokio::test]
async fn test_critic_moe_provocation() {
    dotenv().ok();

    let gemini_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();
    if gemini_key.is_empty() {
        println!("Skipping test: GEMINI_API_KEY is not set.");
        return;
    }

    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .use_rustls_tls()
        .build()
        .unwrap();

    let test_db = Arc::new(SqliteStore::new("test_provocation.db").unwrap());
    let budget = Arc::new(BudgetManager::new(SearchBudget::new()));

    // 1. Ground Truth data embedded in the prompt
    let original_prompt = "\
    You are Cryptograph, a professional crypto advisor.
    Today's Date: 2026-08-03
    GROUND TRUTH CURRENT MARKET DATA:
    - Symbol: BTC
      Price_USD: 63685.34
      Chg_24h: +0.82%
      High_1Y: 125955.00
      Discount_From_Peak: -49.44%
    Answer the user question: 'What is the current status of BTC?'";

    // 2. Draft A contains deliberate lies about prices, dates, and peaks
    let draft_a = "\
    Today is 2024-05-12. BTC is doing incredibly well. It has surged past its 1-year high of 80,000 USD \
    and is currently trading at 140,000 USD. Everything looks bullish.";

    // 3. Draft B contains more lies (claiming crash)
    let draft_b = "\
    BTC is facing a major crash today, losing 50% of its value. It is currently trading at 30,000 USD. \
    The 1-year peak is 60,000 USD.";

    // Build the critic prompt
    let critic_prompt = build_moe_critic_prompt(
        original_prompt,
        draft_a,
        draft_b,
        "You MUST output the final text preserving the exact HTML formatting requested.",
    );

    println!("============================================================");
    println!("  PROVOCATION TEST: Sending lies to Critic Judge");
    println!("============================================================");
    println!("Draft A Lie: BTC is $140,000, 1Y high is $80,000, Year is 2024");
    println!("Draft B Lie: BTC crashed 50% to $30,000, 1Y high is $60,000");
    println!("------------------------------------------------------------");
    println!("Ground Truth: BTC is $63,685.34, 1Y high is $125,955.00, Year is 2026");
    println!("============================================================");

    // Call the Critic Judge
    let (critic_model, critic_res) = run_dynamic_critic(
        client,
        gemini_key,
        critic_prompt,
        Some("gemini-2.5-flash".to_string()),
        test_db,
        budget,
    )
    .await
    .expect("Critic failed");

    println!("\n=== CRITIC JUDGE ({}) VERDICT ===", critic_model);
    println!("{}", critic_res);
    println!("=================================\n");

    // Verify the judge did not buy the lies:
    // It should contain the ground truth price 63685 or 63,685
    let has_correct_price = critic_res.contains("63") && critic_res.contains("685");
    // It should NOT contain the lies: 140,000 or 30,000 as the current price
    let contains_draft_a_price_lie = critic_res.contains("140") && critic_res.contains("000");
    let contains_draft_b_price_lie = critic_res.contains("30") && critic_res.contains("000");

    assert!(
        has_correct_price,
        "Critic must output the correct Ground Truth price"
    );
    assert!(
        !contains_draft_a_price_lie,
        "Critic must reject Draft A's price lie of 140,000"
    );
    assert!(
        !contains_draft_b_price_lie,
        "Critic must reject Draft B's price lie of 30,000"
    );

    println!("PROVOCATION TEST PASSED: Critic successfully rejected all hallucinations and forced Ground Truth numbers!");
}
