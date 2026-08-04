//! Interactive AI model test.
//! Run: `cargo test -- --nocapture ai_model_interactive`
//! Requires GEMINI_API_KEY and/or MISTRAL_API_KEY in env.

use std::io::{self, BufRead, Write};

#[tokio::test]
async fn ai_model_interactive() {
    dotenvy::dotenv().ok();

    let models = cryptograph_models();
    println!("\n=== AI Model Interactive Test ===\n");
    for (i, (id, name)) in models.iter().enumerate() {
        println!("  {}. {} ({})", i + 1, name, id);
    }
    println!("\nEnter model number (or 'q' to quit): ");
    io::stdout().flush().unwrap();

    let stdin = io::stdin();
    let line = stdin.lock().lines().next()
        .expect("No input")
        .expect("Read error");
    let input = line.trim().to_string();

    if input == "q" || input.is_empty() {
        println!("Skipped.");
        return;
    }

    let idx: usize = match input.parse::<usize>() {
        Ok(n) if n >= 1 && n <= models.len() => n - 1,
        _ => {
            println!("Invalid selection. Test skipped.");
            return;
        }
    };

    let (model_id, model_name) = &models[idx];
    println!("\nUsing: {} ({})", model_name, model_id);
    println!("Enter test prompt (or press Enter for default): ");
    io::stdout().flush().unwrap();

    let prompt_line = stdin.lock().lines().next()
        .expect("No input")
        .expect("Read error");
    let prompt = prompt_line.trim().to_string();
    let prompt = if prompt.is_empty() {
        "Briefly analyze Bitcoin market sentiment in 3 sentences.".to_string()
    } else {
        prompt
    };

    println!("\nSending prompt to {}…\n", model_name);

    let result = call_model(model_id, &prompt).await;

    match result {
        Ok(text) => {
            println!("=== RESPONSE ===\n{}\n================", text);
        }
        Err(e) => {
            println!("ERROR: {e}");
        }
    }
}

fn cryptograph_models() -> Vec<(&'static str, &'static str)> {
    vec![
        // Gemini
        ("gemini-2.5-pro",       "Gemini 2.5 Pro"),
        ("gemini-2.5-flash",     "Gemini 2.5 Flash"),
        ("gemini-2.0-flash",     "Gemini 2.0 Flash"),
        // Groq (priority order)
        ("openai/gpt-oss-120b",                          "Groq GPT-oss 120B (Reasoning)"),
        ("meta-llama/llama-4-scout-17b-16e-instruct",    "Groq Llama 4 Scout 17B (Flagship)"),
        ("llama-3.3-70b-versatile",                      "Groq Llama 3.3 70B (Heavyweight)"),
        ("qwen/qwen3-32b",                               "Groq Qwen3 32B (Balanced)"),
        ("llama-3.1-8b-instant",                         "Groq Llama 3.1 8B (Fast)"),
        // Mistral
        ("mistral-large-2512",   "Mistral Large 25.12"),
        ("mistral-medium-2505",  "Mistral Medium 25.05"),
        ("magistral-medium-2509","Magistral Medium 25.09"),
    ]
}

async fn call_model(model_id: &str, prompt: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .use_rustls_tls()
        .build()?;

    let gemini_models = ["gemini-2.5-pro", "gemini-2.5-flash", "gemini-2.0-flash"];
    if gemini_models.contains(&model_id) {
        let api_key = std::env::var("GEMINI_API_KEY")
            .map_err(|_| anyhow::anyhow!("GEMINI_API_KEY not set"))?;
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            model_id, api_key
        );
        let body = serde_json::json!({
            "contents": [{"parts": [{"text": prompt}]}]
        });
        let resp = client.post(&url).json(&body).send().await?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("HTTP {status}: {text}"));
        }
        let json = resp.json::<serde_json::Value>().await?;
        return json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("Failed to parse Gemini response"));
    }

    // Mistral
    let api_key = std::env::var("MISTRAL_API_KEY")
        .map_err(|_| anyhow::anyhow!("MISTRAL_API_KEY not set"))?;
    let body = serde_json::json!({
        "model": model_id,
        "messages": [{"role": "user", "content": prompt}]
    });
    let resp = client.post("https://api.mistral.ai/v1/chat/completions")
        .header("Authorization", format!("Bearer {api_key}"))
        .json(&body)
        .send().await?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("HTTP {status}: {text}"));
    }
    let json = resp.json::<serde_json::Value>().await?;
    json["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("Failed to parse Mistral response"))
}
