use std::sync::Arc;
use dashmap::DashMap;
use teloxide::{prelude::*, types::ParseMode};

use crate::{
    config::Config,
    crypto::{CryptoClient, ai::prompts::{build_patch_generator_prompt, build_patch_critic_prompt}},
    db::Store,
    models::{Portfolio, PortfolioPatch, PendingPatch, Purchase, PaidCurrency},
    bot::keyboards,
};

/// In-memory store for pending patches awaiting user approval.
/// Key = token (hex-encoded random 8 bytes), Value = PendingPatch.
pub type PatchStore = Arc<DashMap<String, PendingPatch>>;

pub fn new_patch_store() -> PatchStore {
    Arc::new(DashMap::new())
}

async fn call_fallback_models(
    client: &reqwest::Client,
    config: &Config,
    prompt: &str,
    preferred_model: Option<&str>,
) -> anyhow::Result<String> {
    use crate::crypto::ai::providers::call_any_model;

    let mut candidates = Vec::new();
    if let Some(pref) = preferred_model {
        candidates.push(pref.to_string());
    }

    let default_candidates = crate::crypto::ai::pools::get_available_models(Some("main"));

    for c in default_candidates {
        if !candidates.contains(&c) {
            candidates.push(c);
        }
    }

    let mut last_err = anyhow::anyhow!("No AI models configured or reachable");
    for model_id in candidates {
        tracing::info!("Trying AI model: {}", model_id);
        match call_any_model(client, &config.gemini_api_key, &model_id, prompt, None).await {
            Ok(res) => {
                tracing::info!("AI model {} succeeded!", model_id);
                return Ok(res);
            }
            Err(e) => {
                tracing::warn!("AI model {} failed: {}", model_id, e);
                last_err = e;
            }
        }
    }
    Err(last_err)
}

fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Builds a compact JSON snapshot of the portfolio for the AI to parse.
fn build_portfolio_snapshot(portfolio: &Portfolio) -> String {
    let purchases: Vec<serde_json::Value> = portfolio.purchases.iter().map(|p| serde_json::json!({
        "id": p.id,
        "symbol": p.symbol,
        "amount": p.amount,
        "cost_usd": p.cost_usd,
        "source": p.source,
        "notes": p.notes,
    })).collect();

    let holdings: Vec<serde_json::Value> = portfolio.holdings.iter().map(|h| {
        let wallet_name = portfolio.wallets.iter()
            .find(|w| w.id == h.wallet_id)
            .map(|w| w.name.clone())
            .unwrap_or_else(|| format!("wallet#{}", h.wallet_id));
        serde_json::json!({
            "symbol": h.symbol,
            "amount": h.amount,
            "wallet": wallet_name,
            "wallet_id": h.wallet_id,
        })
    }).collect();

    let wallets: Vec<serde_json::Value> = portfolio.wallets.iter().map(|w| serde_json::json!({
        "id": w.id,
        "name": w.name,
        "kind": format!("{:?}", w.kind),
    })).collect();

    serde_json::json!({
        "purchases": purchases,
        "holdings": holdings,
        "wallets": wallets,
    }).to_string()
}

/// Formats a patch into a human-readable Telegram HTML summary.
fn format_patch_summary(patch: &PortfolioPatch, portfolio: &Portfolio) -> String {
    let mut lines = Vec::new();

    for p in &patch.add_purchases {
        let wallet = p.wallet_name.as_deref().unwrap_or("?");
        let source = p.source.as_deref().unwrap_or("—");
        lines.push(format!(
            "➕ <b>Add</b>: {} {:.6} @ ${:.2} (wallet: {}, source: {})",
            p.symbol, p.amount, p.cost_usd, wallet, source
        ));
    }

    for id in &patch.remove_purchase_ids {
        let info = portfolio.purchases.iter()
            .find(|p| p.id == *id)
            .map(|p| format!("{} {:.6} @ ${:.2}", p.symbol, p.amount, p.cost_usd))
            .unwrap_or_else(|| format!("ID #{id}"));
        lines.push(format!("➖ <b>Remove</b> purchase #{id}: {info}"));
    }

    for adj in &patch.adjust_holdings {
        let wallet = adj.wallet_name.as_deref().unwrap_or("?");
        lines.push(format!(
            "✏️ <b>Adjust balance</b>: {} → {:.6} (wallet: {})",
            adj.symbol, adj.new_amount, wallet
        ));
    }

    for m in &patch.modify_purchases {
        let mut changes = Vec::new();
        if let Some(a) = m.new_amount    { changes.push(format!("amount={:.6}", a)); }
        if let Some(c) = m.new_cost_usd { changes.push(format!("cost=${:.2}", c)); }
        if let Some(ref s) = m.new_source { changes.push(format!("source={}", s)); }
        lines.push(format!("🔧 <b>Modify purchase</b> #{}: {}", m.purchase_id, changes.join(", ")));
    }

    format!(
        "🔄 <b>AI Portfolio Patch</b> — {} change(s)\n\n{}\n\n<i>Reason: \"{}\"</i>",
        patch.change_count(),
        lines.join("\n"),
        patch.description
    )
}

fn parse_date(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .ok()
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|dt| chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc))
        })
}

/// Applies an approved patch to the portfolio.
fn apply_patch(portfolio: &mut Portfolio, patch: &PortfolioPatch) {
    // 1. Remove purchases
    portfolio.purchases.retain(|p| !patch.remove_purchase_ids.contains(&p.id));

    // 2. Modify purchases
    for modify in &patch.modify_purchases {
        if let Some(p) = portfolio.purchases.iter_mut().find(|p| p.id == modify.purchase_id) {
            if let Some(a) = modify.new_amount    { p.amount = a; }
            if let Some(c) = modify.new_cost_usd  { p.cost_usd = c; }
            if let Some(ref s) = modify.new_source { p.source = s.clone(); }
            if let Some(ref n) = modify.new_notes  { p.notes = Some(n.clone()); }
            if let Some(ref d) = modify.new_date   {
                if let Some(dt) = parse_date(d) {
                    p.date = dt;
                }
            }
        }
    }

    // 3. Add purchases + holdings
    for new_p in &patch.add_purchases {
        let id = portfolio.alloc_purchase_id();
        let wallet_id = new_p.wallet_name.as_deref()
            .and_then(|name| portfolio.wallets.iter().find(|w| w.name.eq_ignore_ascii_case(name)))
            .map(|w| w.id)
            .unwrap_or_else(|| portfolio.wallets.first().map(|w| w.id).unwrap_or(1));

        portfolio.purchases.push(Purchase {
            id,
            symbol: new_p.symbol.to_uppercase(),
            amount: new_p.amount,
            cost_usd: new_p.cost_usd,
            paid_amount: new_p.cost_usd,
            paid_currency: PaidCurrency::Usd,
            date: new_p.date.as_deref().and_then(parse_date).unwrap_or_else(chrono::Utc::now),
            source: new_p.source.clone().unwrap_or_else(|| "AI Patch".to_string()),
            notes: new_p.notes.clone(),
            created_at: chrono::Utc::now(),
            corrections: Vec::new(),
        });

        let h = portfolio.holding_mut(wallet_id, &new_p.symbol.to_uppercase());
        h.amount += new_p.amount;
    }

    // 4. Adjust holdings
    for adj in &patch.adjust_holdings {
        let wallet_id = adj.wallet_name.as_deref()
            .and_then(|name| portfolio.wallets.iter().find(|w| w.name.eq_ignore_ascii_case(name)))
            .map(|w| w.id)
            .or_else(|| {
                portfolio.holdings.iter()
                    .find(|h| h.symbol.eq_ignore_ascii_case(&adj.symbol))
                    .map(|h| h.wallet_id)
            })
            .unwrap_or_else(|| portfolio.wallets.first().map(|w| w.id).unwrap_or(1));

        let h = portfolio.holding_mut(wallet_id, &adj.symbol.to_uppercase());
        h.amount = adj.new_amount;
    }
}

pub async fn handle_patch_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    patch_store: PatchStore,
    args: String,
) -> anyhow::Result<()> {
    tracing::info!("handle_patch_cmd: user={}, args={:?}", msg.chat.id, args);
    let _ = cmc; // may be used for context enrichment in future
    let user_request = args.trim().to_string();

    if user_request.is_empty() {
        bot.send_message(msg.chat.id,
            "🔄 <b>AI Portfolio Corrector</b>\n\n\
             Describe what to change in your portfolio in any language. Examples:\n\
             • <code>/patch I have 0.1 BTC not 0.5, fix it</code>\n\
             • <code>/patch here is my exchange balance: BTC 0.3, ETH 2.1, SOL 15</code>\n\
             • <code>/patch delete purchase #5 and add BTC 0.05 for $5000</code>")
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);

    let (portfolio_snapshot, portfolio_clone) = store.read(user_id, |p| {
        (build_portfolio_snapshot(p), p.clone())
    });

    if portfolio_clone.purchases.is_empty() && portfolio_clone.holdings.is_empty() {
        bot.send_message(msg.chat.id,
            "❌ Your portfolio is empty. Add assets first with /add.")
            .await?;
        return Ok(());
    }

    let loading = bot.send_message(msg.chat.id, "🧠 Analyzing your request…")
        .parse_mode(ParseMode::Html)
        .await?;

    let preferred_model = store.read(user_id, |p| p.settings.ai_model_override.clone());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .use_rustls_tls()
        .build()?;



    // We will try up to 3 times to get a valid, approved patch
    let mut draft = String::new();
    let mut last_error = String::new();
    let mut final_patch = String::new();
    let mut approved = false;

    for attempt in 1..=3 {
        let gen_prompt = if attempt == 1 {
            build_patch_generator_prompt(&user_request, &portfolio_snapshot)
        } else {
            format!(
                "Your previous attempt to generate a portfolio patch failed.\n\
                 The Critic or Parser provided this feedback:\n{}\n\n\
                 Please fix the issues and generate the patch again. \
                 Return ONLY the valid JSON object.\n\n\
                 USER REQUEST:\n\"{}\"\n\n\
                 CURRENT PORTFOLIO:\n{}",
                last_error, user_request, portfolio_snapshot
            )
        };

        // Writer: generate the patch
        draft = match call_fallback_models(&client, &config, &gen_prompt, preferred_model.as_deref()).await {
            Ok(d) => d,
            Err(e) => {
                last_error = e.to_string();
                continue;
            }
        };

        // Critic: validate the patch against the portfolio
        let critic_prompt = build_patch_critic_prompt(&user_request, &portfolio_snapshot, &draft);
        let validated = match call_fallback_models(&client, &config, &critic_prompt, preferred_model.as_deref()).await {
            Ok(v) => v,
            Err(_) => draft.clone(), // fallback to Writer draft if Critic fails
        };

        // Extract the JSON object reliably by finding the first '{' and last '}'
        let start_idx = validated.find('{').unwrap_or(0);
        let end_idx = validated.rfind('}').map(|i| i + 1).unwrap_or(validated.len());
        let clean = if start_idx < end_idx {
            &validated[start_idx..end_idx]
        } else {
            validated.trim()
        };

        // Try to parse the critic's response to check if approved
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(clean) {
            let is_approved = v["approved"].as_bool().unwrap_or(true);
            if is_approved {
                // Try to get improved_text, or just use the whole draft if it's not the critic format
                if let Some(imp) = v.get("improved_text") {
                    if imp.is_string() {
                        final_patch = imp.as_str().unwrap_or("{}").to_string();
                    } else {
                        final_patch = serde_json::to_string(imp).unwrap_or_else(|_| "{}".to_string());
                    }
                } else {
                    final_patch = clean.to_string();
                }
                approved = true;
                break;
            } else {
                last_error = v["critique"].as_str().unwrap_or("Critic rejected the draft without explanation.").to_string();
                tracing::info!("Patch attempt {} rejected by Critic: {}", attempt, last_error);
            }
        } else {
            last_error = "Failed to parse the JSON output correctly. Make sure you return ONLY valid JSON without markdown formatting.".to_string();
            tracing::warn!("Patch attempt {} failed parsing. Raw: {}", attempt, clean);
        }
    }

    if !approved && final_patch.is_empty() {
        // Fallback to the last draft if all attempts failed
        final_patch = draft;
    }
    
    // Extract the final JSON object one last time in case the fallback draft has markdown
    let start_idx = final_patch.find('{').unwrap_or(0);
    let end_idx = final_patch.rfind('}').map(|i| i + 1).unwrap_or(final_patch.len());
    let clean = if start_idx < end_idx {
        &final_patch[start_idx..end_idx]
    } else {
        final_patch.trim()
    };

    let patch: PortfolioPatch = match serde_json::from_str::<serde_json::Value>(clean) {
        Ok(v) => {
            // Check if it's the Critic audit format: {"approved":..., "improved_text":...}
            if let Some(imp) = v.get("improved_text") {
                if imp.is_string() {
                    serde_json::from_str(imp.as_str().unwrap_or("{}")).unwrap_or_else(|_| serde_json::from_value(v.clone()).unwrap_or_default())
                } else {
                    serde_json::from_value(imp.clone()).unwrap_or_else(|_| serde_json::from_value(v.clone()).unwrap_or_default())
                }
            } else {
                serde_json::from_value(v).unwrap_or_default()
            }
        }
        Err(e) => {
            let _ = bot.delete_message(msg.chat.id, loading.id).await;
            tracing::warn!("Failed to parse Critic outer JSON: {e}\nRaw: {clean}");
            bot.send_message(msg.chat.id,
                "❌ AI returned an invalid patch structure. Try rephrasing your request.")
                .await?;
            return Ok(());
        }
    };

    if patch.is_empty() {
        let _ = bot.delete_message(msg.chat.id, loading.id).await;
        bot.send_message(msg.chat.id,
            format!("ℹ️ AI found nothing to change.\n\n<i>\"{}\"</i>", patch.description))
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    let summary = format_patch_summary(&patch, &portfolio_clone);
    let token = generate_token();

    patch_store.insert(token.clone(), PendingPatch::new(user_id, patch));

    let _ = bot.delete_message(msg.chat.id, loading.id).await;
    bot.send_message(msg.chat.id, &summary)
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboards::patch_confirm_menu(&token))
        .await?;

    Ok(())
}

pub async fn handle_patch_confirm(
    bot: Bot,
    chat_id: ChatId,
    user_id: u64,
    token: &str,
    store: Arc<Store>,
    patch_store: PatchStore,
) -> anyhow::Result<()> {
    let pending = match patch_store.remove(token) {
        Some((_, p)) => p,
        None => {
            bot.send_message(chat_id, "⏰ Patch expired or already applied (10-minute timeout).")
                .await?;
            return Ok(());
        }
    };

    if pending.user_id != user_id {
        bot.send_message(chat_id, "⛔ This patch belongs to another user.").await?;
        return Ok(());
    }

    if pending.is_expired() {
        bot.send_message(chat_id, "⏰ Patch expired (10-minute timeout).").await?;
        return Ok(());
    }

    let count = pending.patch.change_count();
    store.write(user_id, |p| apply_patch(p, &pending.patch));

    // Trigger backup
    crate::bot::handlers::portfolio::auto_backup_silent(&bot, &store, user_id).await;

    bot.send_message(chat_id,
        format!("✅ Patch applied: <b>{count}</b> change(s) written to your portfolio.\n\
                 Backup updated automatically."))
        .parse_mode(ParseMode::Html)
        .reply_markup(keyboards::main_menu())
        .await?;

    Ok(())
}

pub async fn handle_patch_cancel(
    bot: Bot,
    chat_id: ChatId,
    token: &str,
    patch_store: PatchStore,
) -> anyhow::Result<()> {
    patch_store.remove(token);
    bot.send_message(chat_id, "❌ Patch cancelled. No changes were made.")
        .reply_markup(keyboards::main_menu())
        .await?;
    Ok(())
}


