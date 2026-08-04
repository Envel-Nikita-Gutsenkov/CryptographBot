//! Model catalog validator: compares models.json entries against each provider's
//! live /models endpoint and reports which IDs are MISSING (dead weight that causes
//! 0% win-rate entries in /stats).
//!
//! Run: cargo test --test model_catalog_check -- --nocapture
use dotenvy::dotenv;
use reqwest::Client;
use std::collections::HashSet;

struct ProviderSpec {
    name: &'static str,
    url: fn(&str) -> String,
    key_env: &'static str,
    extract: fn(&serde_json::Value) -> Vec<String>,
}

fn providers() -> Vec<ProviderSpec> {
    vec![
        ProviderSpec {
            name: "Gemini",
            url: |k| format!("https://generativelanguage.googleapis.com/v1beta/models?key={k}"),
            key_env: "GEMINI_API_KEY",
            extract: |v| {
                v["models"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| {
                                m["name"]
                                    .as_str()
                                    .map(|s| s.trim_start_matches("models/").to_string())
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            },
        },
        ProviderSpec {
            name: "Groq",
            url: |_| "https://api.groq.com/openai/v1/models".into(),
            key_env: "GROQ_API_KEY",
            extract: |v| {
                v["data"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            },
        },
        ProviderSpec {
            name: "Mistral",
            url: |_| "https://api.mistral.ai/v1/models".into(),
            key_env: "MISTRAL_API_KEY",
            extract: |v| {
                v["data"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            },
        },
        ProviderSpec {
            name: "Nvidia",
            url: |_| "https://integrate.api.nvidia.com/v1/models".into(),
            key_env: "NVIDIA_API_KEY",
            extract: |v| {
                v["data"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            },
        },
        ProviderSpec {
            name: "OpenRouter",
            url: |_| "https://openrouter.ai/api/v1/models".into(),
            key_env: "",
            extract: |v| {
                v["data"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            },
        },
        ProviderSpec {
            name: "Cohere",
            url: |_| "https://api.cohere.com/v1/models".into(),
            key_env: "COHERE_API_KEY",
            extract: |v| {
                v["models"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            },
        },
        ProviderSpec {
            name: "Cerebras",
            url: |_| "https://api.cerebras.ai/v1/models".into(),
            key_env: "CEREBRAS_API_KEY",
            extract: |v| {
                v["data"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            },
        },
        ProviderSpec {
            name: "Cloudflare",
            url: |_| {
                format!(
                "https://api.cloudflare.com/client/v4/accounts/{}/ai/models/search?per_page=500",
                std::env::var("CLOUDFLARE_ACCOUNT_ID").unwrap_or_default()
            )
            },
            key_env: "CLOUDFLARE_API_TOKEN",
            extract: |v| {
                v["result"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|m| m["name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default()
            },
        },
    ]
}

#[tokio::test]
async fn validate_model_catalog() {
    dotenv().ok();
    let client = Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .use_rustls_tls()
        .build()
        .unwrap();

    let catalog: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string("models.json").unwrap()).unwrap();

    for spec in providers() {
        let key = std::env::var(spec.key_env).unwrap_or_default();
        if spec.key_env != "" && key.is_empty() {
            println!("[{}] SKIP (no {} set)", spec.name, spec.key_env);
            continue;
        }

        let mut req = client.get((spec.url)(&key));
        if !key.is_empty() {
            req = req.header("Authorization", format!("Bearer {key}"));
        }
        let real: HashSet<String> = match req.send().await {
            Ok(r) if r.status().is_success() => match r.json::<serde_json::Value>().await {
                Ok(v) => (spec.extract)(&v).into_iter().collect(),
                Err(_) => {
                    println!("[{}] SKIP (parse error)", spec.name);
                    continue;
                }
            },
            Ok(r) => {
                println!("[{}] SKIP (HTTP {})", spec.name, r.status());
                continue;
            }
            Err(e) => {
                println!("[{}] SKIP (network: {})", spec.name, e);
                continue;
            }
        };
        println!("[{}] provider exposes {} models", spec.name, real.len());

        let entries: Vec<(String, String)> = catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| {
                m["provider"]
                    .as_str()
                    .unwrap_or("")
                    .eq_ignore_ascii_case(spec.name)
            })
            .map(|m| {
                (
                    m["alias"].as_str().unwrap_or("?").to_string(),
                    m["id"].as_str().unwrap_or("?").to_string(),
                )
            })
            .collect();

        for (alias, id) in entries {
            let found = real.contains(&id) || real.iter().any(|r| r.ends_with(&format!("/{id}")));
            println!(
                "  {} {:?} ({alias})",
                if found { "✅" } else { "❌ MISSING" },
                id
            );
        }
    }
}
