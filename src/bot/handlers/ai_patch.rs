use dashmap::DashMap;
use std::sync::Arc;
use teloxide::{prelude::*, types::ParseMode};

use crate::{
    bot::keyboards,
    config::Config,
    crypto::{
        ai::prompts::{build_patch_critic_prompt, build_patch_generator_prompt},
        CryptoClient,
    },
    db::Store,
    format::fmt_usd_signed,
    models::{PaidCurrency, PendingPatch, Portfolio, PortfolioPatch, Purchase},
};

/// In-memory store for pending patches awaiting user approval.
/// Key = token (hex-encoded random 8 bytes), Value = PendingPatch.
pub type PatchStore = Arc<DashMap<String, PendingPatch>>;

pub fn new_patch_store() -> PatchStore {
    Arc::new(DashMap::new())
}

/// Drop every pending patch belonging to `user_id`. Called right before a newer
/// preview is stored, so a user only ever has one confirmable patch and an
/// obsolete "Apply" button cannot mutate a portfolio that already moved on.
pub fn invalidate_pending_for_user(patch_store: &PatchStore, user_id: u64) {
    patch_store.retain(|_, p| p.user_id != user_id);
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
    let purchases: Vec<serde_json::Value> = portfolio
        .purchases
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "symbol": p.symbol,
                "amount": p.amount,
                "cost_usd": p.cost_usd,
                "source": p.source,
                "notes": p.notes,
            })
        })
        .collect();

    let holdings: Vec<serde_json::Value> = portfolio
        .holdings
        .iter()
        .map(|h| {
            let wallet_name = portfolio
                .wallets
                .iter()
                .find(|w| w.id == h.wallet_id)
                .map(|w| w.name.clone())
                .unwrap_or_else(|| format!("wallet#{}", h.wallet_id));
            serde_json::json!({
                "symbol": h.symbol,
                "amount": h.amount,
                "wallet": wallet_name,
                "wallet_id": h.wallet_id,
            })
        })
        .collect();

    let wallets: Vec<serde_json::Value> = portfolio
        .wallets
        .iter()
        .map(|w| {
            serde_json::json!({
                "id": w.id,
                "name": w.name,
                "kind": format!("{:?}", w.kind),
            })
        })
        .collect();

    serde_json::json!({
        "purchases": purchases,
        "holdings": holdings,
        "wallets": wallets,
    })
    .to_string()
}

/// Formats a patch into a human-readable Telegram HTML summary.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `cost_usd` on a purchase is the **total** paid, not a unit price. The old
/// summary printed it as `SYM 4.7 @ $333.33`, which reads as a $333/SOL unit
/// price; label the total explicitly and derive the per-unit figure.
fn unit_price_label(amount: f64, cost_usd: f64) -> String {
    if amount.is_finite() && amount > 0.0 && cost_usd.is_finite() {
        format!("≈ {}/unit", fmt_usd_signed(cost_usd / amount))
    } else {
        "unit price n/a".to_string()
    }
}

fn format_patch_summary(patch: &PortfolioPatch, portfolio: &Portfolio) -> String {
    let mut lines = Vec::new();

    for p in &patch.add_purchases {
        let wallet = p.wallet_name.as_deref().unwrap_or("?");
        let source = p.source.as_deref().unwrap_or("—");
        lines.push(format!(
            "➕ <b>Add</b>: {} {} for {} ({}) (wallet: {}, source: {})",
            html_escape(&p.symbol),
            p.amount,
            fmt_usd_signed(p.cost_usd),
            unit_price_label(p.amount, p.cost_usd),
            html_escape(wallet),
            html_escape(source)
        ));
    }

    for id in &patch.remove_purchase_ids {
        let info = portfolio
            .purchases
            .iter()
            .find(|p| p.id == *id)
            .map(|p| {
                format!(
                    "{} {} for {} ({})",
                    html_escape(&p.symbol),
                    p.amount,
                    fmt_usd_signed(p.cost_usd),
                    unit_price_label(p.amount, p.cost_usd)
                )
            })
            .unwrap_or_else(|| format!("ID #{id}"));
        lines.push(format!("➖ <b>Remove</b> purchase #{id}: {info}"));
    }

    for adj in &patch.adjust_holdings {
        let wallet = adj.wallet_name.as_deref().unwrap_or("?");
        lines.push(format!(
            "✏️ <b>Adjust balance</b>: {} → {:.6} (wallet: {})",
            html_escape(&adj.symbol),
            adj.new_amount,
            html_escape(wallet)
        ));
    }

    for m in &patch.modify_purchases {
        let mut changes = Vec::new();
        if let Some(a) = m.new_amount {
            changes.push(format!("amount={:.6}", a));
        }
        if let Some(c) = m.new_cost_usd {
            changes.push(format!("cost=${:.2}", c));
        }
        if let Some(ref s) = m.new_source {
            changes.push(format!("source={}", html_escape(s)));
        }
        lines.push(format!(
            "🔧 <b>Modify purchase</b> #{}: {}",
            m.purchase_id,
            changes.join(", ")
        ));
    }

    format!(
        "🔄 <b>AI Portfolio Patch</b> — {} change(s)\n\n{}\n\n<i>Reason: \"{}\"</i>",
        patch.change_count(),
        lines.join("\n"),
        html_escape(&patch.description)
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
                .map(|dt| {
                    chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc)
                })
        })
}

/// Applies an approved patch to the portfolio.
fn apply_patch(portfolio: &mut Portfolio, patch: &PortfolioPatch) {
    // 1. Remove purchases
    portfolio
        .purchases
        .retain(|p| !patch.remove_purchase_ids.contains(&p.id));

    // 2. Modify purchases
    for modify in &patch.modify_purchases {
        if let Some(p) = portfolio
            .purchases
            .iter_mut()
            .find(|p| p.id == modify.purchase_id)
        {
            if let Some(a) = modify.new_amount {
                p.amount = a;
            }
            if let Some(c) = modify.new_cost_usd {
                p.cost_usd = c;
            }
            if let Some(ref s) = modify.new_source {
                p.source = s.clone();
            }
            if let Some(ref n) = modify.new_notes {
                p.notes = Some(n.clone());
            }
            if let Some(ref d) = modify.new_date {
                if let Some(dt) = parse_date(d) {
                    p.date = dt;
                }
            }
        }
    }

    // 3. Add purchases + holdings
    for new_p in &patch.add_purchases {
        let id = portfolio.alloc_purchase_id();
        let wallet_id = new_p
            .wallet_name
            .as_deref()
            .and_then(|name| {
                portfolio
                    .wallets
                    .iter()
                    .find(|w| w.name.eq_ignore_ascii_case(name))
            })
            .map(|w| w.id)
            .unwrap_or_else(|| portfolio.wallets.first().map(|w| w.id).unwrap_or(1));

        portfolio.purchases.push(Purchase {
            id,
            symbol: new_p.symbol.to_uppercase(),
            amount: new_p.amount,
            cost_usd: new_p.cost_usd,
            paid_amount: new_p.cost_usd,
            paid_currency: PaidCurrency::Usd,
            date: new_p
                .date
                .as_deref()
                .and_then(parse_date)
                .unwrap_or_else(chrono::Utc::now),
            source: new_p
                .source
                .clone()
                .unwrap_or_else(|| "AI Patch".to_string()),
            notes: new_p.notes.clone(),
            created_at: chrono::Utc::now(),
            corrections: Vec::new(),
        });

        let h = portfolio.holding_mut(wallet_id, &new_p.symbol.to_uppercase());
        h.amount += new_p.amount;
    }

    // 4. Adjust holdings
    for adj in &patch.adjust_holdings {
        let wallet_id = adj
            .wallet_name
            .as_deref()
            .and_then(|name| {
                portfolio
                    .wallets
                    .iter()
                    .find(|w| w.name.eq_ignore_ascii_case(name))
            })
            .map(|w| w.id)
            .or_else(|| {
                portfolio
                    .holdings
                    .iter()
                    .find(|h| h.symbol.eq_ignore_ascii_case(&adj.symbol))
                    .map(|h| h.wallet_id)
            })
            .unwrap_or_else(|| portfolio.wallets.first().map(|w| w.id).unwrap_or(1));

        let h = portfolio.holding_mut(wallet_id, &adj.symbol.to_uppercase());
        let old = h.amount;
        if (old - adj.new_amount).abs() > f64::EPSILON {
            h.corrections.push(crate::models::CorrectionEntry {
                at: chrono::Utc::now(),
                field: "amount".into(),
                old_value: old.to_string(),
                new_value: adj.new_amount.to_string(),
                reason: "AI portfolio patch (adjust)".into(),
            });
        }
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
        bot.send_message(
            msg.chat.id,
            "🔄 <b>AI Portfolio Corrector</b>\n\n\
             Describe what to change in your portfolio in any language. Examples:\n\
             • <code>/patch I have 0.1 BTC not 0.5, fix it</code>\n\
             • <code>/patch here is my exchange balance: BTC 0.3, ETH 2.1, SOL 15</code>\n\
             • <code>/patch delete purchase #5 and add BTC 0.05 for $5000</code>",
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);

    let (portfolio_snapshot, portfolio_clone) =
        store.read(user_id, |p| (build_portfolio_snapshot(p), p.clone()));

    if portfolio_clone.purchases.is_empty() && portfolio_clone.holdings.is_empty() {
        bot.send_message(
            msg.chat.id,
            "❌ Your portfolio is empty. Add assets first with /add.",
        )
        .await?;
        return Ok(());
    }

    let loading = bot
        .send_message(msg.chat.id, "🧠 Analyzing your request…")
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
        draft =
            match call_fallback_models(&client, &config, &gen_prompt, preferred_model.as_deref())
                .await
            {
                Ok(d) => d,
                Err(e) => {
                    last_error = e.to_string();
                    continue;
                }
            };

        // Critic: validate the patch against the portfolio
        let critic_prompt = build_patch_critic_prompt(&user_request, &portfolio_snapshot, &draft);
        let validated = match call_fallback_models(
            &client,
            &config,
            &critic_prompt,
            preferred_model.as_deref(),
        )
        .await
        {
            Ok(v) => v,
            Err(_) => draft.clone(), // fallback to Writer draft if Critic fails
        };

        // Extract the JSON object reliably by finding the first '{' and last '}'
        let start_idx = validated.find('{').unwrap_or(0);
        let end_idx = validated
            .rfind('}')
            .map(|i| i + 1)
            .unwrap_or(validated.len());
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
                        final_patch =
                            serde_json::to_string(imp).unwrap_or_else(|_| "{}".to_string());
                    }
                } else {
                    final_patch = clean.to_string();
                }
                approved = true;
                break;
            } else {
                last_error = v["critique"]
                    .as_str()
                    .unwrap_or("Critic rejected the draft without explanation.")
                    .to_string();
                tracing::info!(
                    "Patch attempt {} rejected by Critic: {}",
                    attempt,
                    last_error
                );
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
    let end_idx = final_patch
        .rfind('}')
        .map(|i| i + 1)
        .unwrap_or(final_patch.len());
    let clean = if start_idx < end_idx {
        &final_patch[start_idx..end_idx]
    } else {
        final_patch.trim()
    };

    let mut patch: PortfolioPatch = match serde_json::from_str::<serde_json::Value>(clean) {
        Ok(v) => {
            // Check if it's the Critic audit format: {"approved":..., "improved_text":...}
            if let Some(imp) = v.get("improved_text") {
                if imp.is_string() {
                    serde_json::from_str(imp.as_str().unwrap_or("{}"))
                        .unwrap_or_else(|_| serde_json::from_value(v.clone()).unwrap_or_default())
                } else {
                    serde_json::from_value(imp.clone())
                        .unwrap_or_else(|_| serde_json::from_value(v.clone()).unwrap_or_default())
                }
            } else {
                serde_json::from_value(v).unwrap_or_default()
            }
        }
        Err(e) => {
            let _ = bot.delete_message(msg.chat.id, loading.id).await;
            tracing::warn!("Failed to parse Critic outer JSON: {e}\nRaw: {clean}");
            bot.send_message(
                msg.chat.id,
                "❌ AI returned an invalid patch structure. Try rephrasing your request.",
            )
            .await?;
            return Ok(());
        }
    };

    let dropped = patch.sanitize();
    if dropped > 0 {
        tracing::info!("patch: dropped {dropped} degenerate record(s)");
    }

    if patch.is_empty() {
        let _ = bot.delete_message(msg.chat.id, loading.id).await;
        let extra = if dropped > 0 {
            "\n\n⚠️ Some records were rejected: unknown/zero ticker, amount or USD cost.\nIf you converted from another currency, state the rate (e.g. <code>1$ = 3 BYN</code>) so the cost can be filled in."
        } else {
            ""
        };
        bot.send_message(
            msg.chat.id,
            format!("ℹ️ AI found nothing valid to change.{}\n\n<i>\"{}\"</i>", extra,
                html_escape(&patch.description)
            ),
        )
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(());
    }

    let summary = format_patch_summary(&patch, &portfolio_clone);
    let token = generate_token();

    // A newer preview supersedes any older one for this user: drop stale
    // entries so an obsolete "Apply" button can never be confirmed after
    // the portfolio has moved on.
    invalidate_pending_for_user(&patch_store, user_id);
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
            bot.send_message(
                chat_id,
                "⏰ Patch expired or already applied (10-minute timeout).",
            )
            .await?;
            return Ok(());
        }
    };

    if pending.user_id != user_id {
        bot.send_message(chat_id, "⛔ This patch belongs to another user.")
            .await?;
        return Ok(());
    }

    if pending.is_expired() {
        bot.send_message(chat_id, "⏰ Patch expired (10-minute timeout).")
            .await?;
        return Ok(());
    }

    let count = pending.patch.change_count();
    store.write(user_id, |p| apply_patch(p, &pending.patch));

    // Trigger backup
    crate::bot::handlers::portfolio::auto_backup_silent(&bot, &store, user_id).await;

    bot.send_message(
        chat_id,
        format!(
            "✅ Patch applied: <b>{count}</b> change(s) written to your portfolio.\n\
                 Backup updated automatically."
        ),
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn patch_with_desc(desc: &str) -> PortfolioPatch {
        PortfolioPatch {
            description: desc.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn patch_summary_labels_cost_as_total_not_unit_price() {
        // Real Nov-2026 case: 1000 BYN at 3 BYN/USD -> $333.33 total for
        // 4.728187 SOL (~$70.51/SOL). The old `@ $333.33` read as a unit price.
        let patch = PortfolioPatch {
            add_purchases: vec![add("SOL", 4.728_187, 333.33)],
            description: "convert".into(),
            ..Default::default()
        };
        let s = format_patch_summary(&patch, &Portfolio::default());
        assert!(!s.contains("@ $333.33"), "unit-price-looking output: {s}");
        assert!(s.contains("for $333.33"), "total must be explicit: {s}");
        assert!(s.contains("$70.50/unit"), "per-unit must be derived: {s}");
    }

    #[test]
    fn patch_summary_handles_zero_amount_without_dividing() {
        let mut patch = PortfolioPatch::default();
        patch.add_purchases.push(add("BTC", 0.0, 100.0));
        let s = format_patch_summary(&patch, &Portfolio::default());
        assert!(s.contains("unit price n/a"), "must not divide by zero: {s}");
        assert!(!s.contains("inf"), "must not print inf: {s}");
    }

    #[test]
    fn newer_preview_invalidates_older_pending_for_same_user() {
        let store = new_patch_store();
        store.insert("old".into(), PendingPatch::new(7, patch_with_desc("old")));
        store.insert("keep".into(), PendingPatch::new(9, patch_with_desc("other")));

        invalidate_pending_for_user(&store, 7);
        store.insert("new".into(), PendingPatch::new(7, patch_with_desc("new")));

        assert!(store.get("old").is_none(), "stale patch must be dropped");
        assert!(store.get("new").is_some(), "fresh patch must be kept");
        assert!(store.get("keep").is_some(), "other users are untouched");
        assert_eq!(store.len(), 2);
    }

    fn holding_amount(p: &Portfolio, symbol: &str) -> f64 {
        p.holdings
            .iter()
            .filter(|h| h.symbol.eq_ignore_ascii_case(symbol))
            .map(|h| h.amount)
            .sum()
    }

    fn add(symbol: &str, amount: f64, cost_usd: f64) -> crate::models::NewPurchase {
        crate::models::NewPurchase {
            symbol: symbol.into(),
            amount,
            cost_usd,
            wallet_name: None,
            source: None,
            notes: None,
            date: None,
        }
    }

    #[test]
    fn apply_patch_add_increments_holding_and_records_purchase() {
        let mut p = Portfolio::default();
        let patch = PortfolioPatch {
            add_purchases: vec![add("ETH", 0.05, 100.0)],
            description: "buy".into(),
            ..Default::default()
        };
        apply_patch(&mut p, &patch);
        assert!((holding_amount(&p, "ETH") - 0.05).abs() < 1e-12);
        assert_eq!(p.purchases.len(), 1);
        assert_eq!(p.purchases[0].symbol, "ETH");
    }

    #[test]
    fn apply_patch_add_then_adjust_same_symbol_sets_absolute() {
        // The dominant AI pattern (seen in real logs): "add N of X" together
        // with "set X to <final>". The explicit absolute adjust must win, so
        // the holding is the final number, not old+N+final (no double count).
        let mut p = Portfolio::default();
        let patch0 = PortfolioPatch {
            add_purchases: vec![add("ETH", 0.104439, 200.0)],
            description: "seed".into(),
            ..Default::default()
        };
        apply_patch(&mut p, &patch0);

        let patch = PortfolioPatch {
            add_purchases: vec![add("ETH", 0.055088, 100.0)],
            adjust_holdings: vec![crate::models::HoldingAdjust {
                symbol: "ETH".into(),
                wallet_name: None,
                new_amount: 0.159527,
            }],
            description: "add + reconcile".into(),
            ..Default::default()
        };
        apply_patch(&mut p, &patch);

        assert!(
            (holding_amount(&p, "ETH") - 0.159527).abs() < 1e-12,
            "absolute adjust must win, got {}",
            holding_amount(&p, "ETH")
        );
        // The purchase record is still persisted for cost-basis/audit.
        assert_eq!(p.purchases.len(), 2);
        // Because the add made the holding exactly the adjust target, there is
        // no net correction to record (old == new after the add).
        let corrections: usize = p.holdings.iter().map(|h| h.corrections.len()).sum();
        assert_eq!(corrections, 0);
    }

    #[test]
    fn apply_patch_adjust_only_overwrites_holding() {
        let mut p = Portfolio::default();
        let seed = PortfolioPatch {
            add_purchases: vec![add("SOL", 2.0, 300.0)],
            description: "seed".into(),
            ..Default::default()
        };
        apply_patch(&mut p, &seed);
        let patch = PortfolioPatch {
            adjust_holdings: vec![crate::models::HoldingAdjust {
                symbol: "SOL".into(),
                wallet_name: None,
                new_amount: 1.5,
            }],
            description: "reconcile".into(),
            ..Default::default()
        };
        apply_patch(&mut p, &patch);
        assert!((holding_amount(&p, "SOL") - 1.5).abs() < 1e-12);
        // A real change (2.0 → 1.5) is auditable as a correction.
        let corrections: usize = p.holdings.iter().map(|h| h.corrections.len()).sum();
        assert_eq!(corrections, 1);
    }
}
