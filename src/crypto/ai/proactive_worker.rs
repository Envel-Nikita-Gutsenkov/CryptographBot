//! Proactive Goal/Memo worker.
//!
//! Periodically scans the market and evaluates every user's active goals and
//! memory notes with the strongest available models, using a double-checked
//! pipeline:
//!   1. two powerful generators race to produce drafts (role "main"),
//!   2. a cheap model reviews/merges the drafts against ground truth (role "news"),
//!   3. a powerful critic fact-checks the merged draft (role "critic").
//!
//! The result is a strict JSON list of per-goal actions (alert / wait / done /
//! stale). Alerts are sent to Telegram with a per-goal cooldown; done/stale
//! goals are deactivated AND the user is notified with the reason. Everything
//! the model needs (portfolio, quotes, milestones, news, market intel, goals,
//! memos, date) is pre-fetched and handed to it "on a platter".
//!
//! The core logic lives in [`run_scan`] and is shared between the live bot
//! loop ([`run`]) and the transparent CLI test (`cargo run -- goal <user_id>`),
//! which prints every pipeline stage so the decision process is inspectable.

use std::sync::Arc;

use chrono::{DateTime, Timelike, Utc};
use teloxide::prelude::*;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use crate::{
    config::Config,
    crypto::{
        ai::{pipeline, pools, prompts, providers},
        CryptoClient,
    },
    db::{SqliteStore, Store},
    models::Goal,
};

const DEFAULT_SCAN_HOURS: f64 = 6.0;
const MIN_SCAN_HOURS: f64 = 2.0;
/// Default minimum gap between proactive alerts for the same goal
/// (overridable via `PROACTIVE_ALERT_COOLDOWN_HOURS`).
const GOAL_ALERT_COOLDOWN_HOURS: i64 = 24;
/// How long to suppress re-proposing a goal after the user explicitly declined
/// ("keep active"). Much longer than the normal alert cooldown so the bot
/// doesn't nag the same goal every day.
const GOAL_DECLINE_SUPPRESS_HOURS: i64 = 24 * 7;
/// How many previously sent alert messages to feed back into the prompt.
const MAX_ALERT_MEMORY: usize = 5;
/// Max symbols included in the market snapshot (token/API economy).
const MAX_SNAPSHOT_SYMBOLS: usize = 12;

pub async fn run(
    bot: Bot,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    sqlite: Arc<SqliteStore>,
    config: Arc<Config>,
) {
    info!("Starting Proactive Goal/Memo Worker");

    // Let the rest of the stack boot first, then start scanning.
    sleep(Duration::from_secs(45)).await;

    loop {
        info!("Proactive worker tick");
        let started = Utc::now();

        for user_id in store.all_user_ids() {
            let has_something =
                store.read(user_id, |p| p.goals.iter().any(|g| g.active && !g.done));
            if !has_something {
                continue; // token economy: only scan users with live goals
            }
            if let Err(e) = scan_user(&bot, &store, &cmc, &sqlite, config.clone(), user_id).await {
                warn!("Proactive scan failed for user {}: {:#}", user_id, e);
            }
        }

        let next = next_interval(&cmc).await;
        let elapsed = (Utc::now() - started).num_seconds().max(0) as u64;
        let secs = next.as_secs().saturating_sub(elapsed).max(60);
        info!("Proactive worker sleeping for {}s", secs);
        sleep(Duration::from_secs(secs)).await;
    }
}

/// Adaptive scan interval: env `PROACTIVE_SCAN_HOURS` (default 6h), halved in
/// volatile markets, never below 2h.
async fn next_interval(cmc: &CryptoClient) -> Duration {
    let base_hours = std::env::var("PROACTIVE_SCAN_HOURS")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|h| *h >= 1.0)
        .unwrap_or(DEFAULT_SCAN_HOURS);
    let mut hours = base_hours;
    if let Ok(quotes) = cmc.get_quotes(&["BTC", "ETH"]).await {
        let volatile = quotes.iter().any(|q| {
            q.change_1h.unwrap_or(0.0).abs() >= 2.0 || q.change_24h.unwrap_or(0.0).abs() >= 5.0
        });
        if volatile {
            hours = (hours * 0.5).max(MIN_SCAN_HOURS);
            info!(
                "Proactive worker: volatile market, interval shortened to {}h",
                hours
            );
        }
    }
    Duration::from_secs_f64(hours * 3600.0)
}

/// Live path: quiet-hours gate + shared scan + Telegram delivery.
async fn scan_user(
    bot: &Bot,
    store: &Store,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    config: Arc<Config>,
    user_id: u64,
) -> anyhow::Result<()> {
    // Respect quiet hours (skip scanning entirely so no alerts fire).
    let (quiet_start, quiet_end) = store.read(user_id, |p| {
        (
            p.settings.alert_quiet_hours_start,
            p.settings.alert_quiet_hours_end,
        )
    });
    if let (Some(s), Some(e)) = (quiet_start, quiet_end) {
        let hour = chrono::Local::now().hour() as u8;
        if if s < e {
            hour >= s && hour < e
        } else {
            hour >= s || hour < e
        } {
            return Ok(());
        }
    }

    let outcome = run_scan(store, cmc, sqlite, user_id, true, true).await?;

    if !outcome.messages.is_empty() {
        let header = format!(
            "🤖 **Smart alert for your goals** ({})",
            outcome.current_date
        );
        let text = format!("{}\n\n{}", header, outcome.messages.join("\n\n"));
        if let Err(e) = crate::bot::handlers::ai::send_rich_message_via_api(
            bot,
            config.clone(),
            teloxide::types::ChatId(user_id as i64),
            text,
        )
        .await
        {
            warn!("Proactive alert send failed for user {}: {}", user_id, e);
        }
    }

    // Proposals require explicit user confirmation — each is sent as its own
    // message with a Confirm/Keep inline keyboard. The AI can never deactivate
    // a goal without the user pressing Confirm.
    for (goal_id, msg) in &outcome.proposals {
        let chat_id = teloxide::types::ChatId(user_id as i64);
        if let Err(e) = crate::bot::handlers::ai::send_rich_message_via_api_with_markup(
            bot,
            chat_id,
            msg.clone(),
            crate::bot::keyboards::goal_propose_keyboard(*goal_id),
        )
        .await
        {
            warn!("Proactive proposal send failed for user {}: {}", user_id, e);
        }
    }
    Ok(())
}

// ── Shared scan core (bot + CLI) ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GoalAction {
    pub goal_id: u64,
    pub action: String,
    pub message: String,
    pub reason: String,
}

/// Full trace of one goal-check pipeline run. The CLI prints stages live via
/// the `run_scan_with` callback; the struct is kept for debugging/verbose use.
#[allow(dead_code)]
pub struct PipelineTrace {
    pub prompt: String,
    pub gen_a_model: String,
    pub draft_a: String,
    pub gen_b_model: String,
    pub draft_b: String,
    pub reviewer_model: String,
    pub reviewer_output: String,
    pub critic_model: String,
    pub critic_output: String,
}

/// Result of a scan: parsed actions, notifications to deliver, state changes
/// applied to the store, and the full pipeline trace.
pub struct ScanOutcome {
    pub current_date: String,
    pub actions: Vec<GoalAction>,
    /// Notifications that should be delivered to the user.
    pub messages: Vec<String>,
    /// (goal_id, message) proposals that need user confirmation (Confirm/Keep
    /// inline keyboard). Sent separately from plain alerts.
    pub proposals: Vec<(u64, String)>,
    /// (goal_id, goal_text, new_status) for goals whose state changed.
    pub changed: Vec<(u64, String, String)>,
    /// Full pipeline trace (kept for debugging; the CLI prints stages live).
    #[allow(dead_code)]
    pub trace: PipelineTrace,
}

pub async fn run_scan(
    store: &Store,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    user_id: u64,
    apply: bool,
    persist: bool,
) -> anyhow::Result<ScanOutcome> {
    run_scan_with(store, cmc, sqlite, user_id, apply, persist, &|_, _| {}).await
}

/// Shared scan core with a stage callback (`run_scan` uses a no-op). The CLI
/// uses it to print each pipeline stage live as it completes.
///
/// `apply=true` mutates goal state (alerts cooldowns, done/stale, AI memory);
/// `persist=true` writes the changes + cache to SQLite. `/plan check` runs with
/// `apply=false, persist=false` — pure report, nothing changed.
pub async fn run_scan_with<F: Fn(&str, &str)>(
    store: &Store,
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    user_id: u64,
    apply: bool,
    persist: bool,
    on_stage: &F,
) -> anyhow::Result<ScanOutcome> {
    let (goals, memos) = store.read(user_id, |p| (p.goals.clone(), p.memos.clone()));
    let active_goals: Vec<Goal> = goals
        .iter()
        .filter(|g| g.active && !g.done)
        .cloned()
        .collect();
    if active_goals.is_empty() {
        return Err(anyhow::anyhow!(
            "User {} has no active goals — nothing to scan",
            user_id
        ));
    }

    let current_date = Utc::now().format("%Y-%m-%d").to_string();

    // ── 1. Prepare context "on a platter" ────────────────────────────────────
    let portfolio_context = build_portfolio_context(store, user_id);
    let market_snapshot = build_market_snapshot(cmc, sqlite, store, user_id).await;
    let (news_context, intel_context) = build_aux_context(sqlite, store, user_id).await;

    let goals_text = active_goals
        .iter()
        .map(|g| {
            let created = g.created_at.format("%Y-%m-%d");
            let expires = g
                .expires_at
                .map(|e| format!(", expires_at: {}", e.format("%Y-%m-%d")))
                .unwrap_or_default();
            let mut block = format!(
                "- goal_id {}: \"{}\" (created_at: {}{})",
                g.id, g.text, created, expires
            );
            // Temporal memory: what already happened + last AI reasoning.
            if !g.progress_notes.is_empty() {
                let notes: Vec<String> = g
                    .progress_notes
                    .iter()
                    .rev()
                    .take(6)
                    .map(|n| format!("    - {}: {}", n.at.format("%Y-%m-%d %H:%M"), n.text))
                    .collect();
                block.push_str(&format!("\n  progress_so_far:\n{}", notes.join("\n")));
            }
            // What we already told the user about this goal (last 5 alerts) —
            // the model must not repeat these unless conditions changed.
            if !g.sent_alerts.is_empty() {
                let prev: Vec<String> = g
                    .sent_alerts
                    .iter()
                    .rev()
                    .take(MAX_ALERT_MEMORY)
                    .map(|a| format!("    - {}: {}", a.at.format("%Y-%m-%d %H:%M"), a.text))
                    .collect();
                block.push_str(&format!(
                    "\n  previous_alerts_sent (do NOT repeat):\n{}",
                    prev.join("\n")
                ));
            }
            if let Some(sum) = &g.ai_summary {
                let short: String = sum.chars().take(300).collect();
                block.push_str(&format!("\n  last_ai_reasoning: {}", short));
            }
            block
        })
        .collect::<Vec<_>>()
        .join("\n");
    let memos_text = if memos.is_empty() {
        "(none)".to_string()
    } else {
        memos
            .iter()
            .map(|m| format!("- \"{}\"", m.text))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let previous = sqlite.load_proactive_cache(user_id).await.unwrap_or(None);
    let preferred_lang = store.read(user_id, |p| p.settings.preferred_language.clone());

    // ── 2. Double-checked pipeline ───────────────────────────────────────────
    let prompt = prompts::build_proactive_prompt(
        &current_date,
        &portfolio_context,
        &market_snapshot,
        &intel_context,
        &news_context,
        &goals_text,
        &memos_text,
        previous.as_ref().map(|(_, a)| a.as_str()),
        &preferred_lang,
    );
    on_stage(
        "progress",
        "Context ready (portfolio, quotes, milestones, news, market intel).",
    );
    on_stage("prompt", &prompt);

    let trace = run_goal_pipeline_traced_with(&prompt, Arc::new(sqlite.clone()), on_stage).await?;
    let actions = parse_goal_actions(&trace.critic_output, &active_goals)?;
    if actions.is_empty() {
        warn!(
            "Proactive scan: no actionable goals parsed for user {}",
            user_id
        );
        // Still cache the analysis for continuity.
        if persist {
            let (_, analysis) = previous.unwrap_or_default();
            sqlite
                .save_proactive_cache(user_id, &market_snapshot, &analysis)
                .await;
        }
        return Ok(ScanOutcome {
            current_date,
            actions: vec![],
            messages: vec![],
            proposals: vec![],
            changed: vec![],
            trace,
        });
    }

    // ── 3. Apply actions ─────────────────────────────────────────────────────
    let now = Utc::now();
    let mut messages: Vec<String> = Vec::new();
    let mut proposals: Vec<(u64, String)> = Vec::new();
    let mut changed: Vec<(u64, String, String)> = Vec::new();

    if apply {
        for action in &actions {
            let applied = store.write(user_id, |p| {
                let Some(goal) = p.goals.iter_mut().find(|g| g.id == action.goal_id) else {
                    return None;
                };
                let text = goal.text.clone();
                apply_action(goal, action, now).map(|(msg, status)| (text, msg, status))
            });
            if let Some((text, msg, status)) = applied {
                if status == "propose" {
                    // Proposals need user confirmation — delivered separately
                    // with a Confirm/Keep keyboard, not as a plain alert.
                    proposals.push((action.goal_id, msg));
                } else {
                    messages.push(msg);
                }
                changed.push((action.goal_id, text, status));
            }
        }

        // Temporal memory: persist the AI's reasoning for every goal (including
        // "wait") so the next check remembers what the model already decided.
        let summary_updated = store.write(user_id, |p| {
            let mut any = false;
            for g in p.goals.iter_mut() {
                if let Some(a) = actions.iter().find(|a| a.goal_id == g.id) {
                    let summary = if !a.reason.trim().is_empty() {
                        format!("{} — {}", a.action, a.reason.trim())
                    } else {
                        a.action.clone()
                    };
                    if g.ai_summary.as_deref() != Some(summary.as_str()) {
                        g.ai_summary = Some(summary);
                        any = true;
                    }
                }
            }
            any
        });
        if summary_updated {
            changed.push((0, "(ai_summary)".into(), "updated".into()));
        }
    } else {
        // Report mode (/plan check): show what WOULD be sent, change nothing.
        for action in &actions {
            if let Some(goal) = store.read(user_id, |p| {
                p.goals.iter().find(|g| g.id == action.goal_id).cloned()
            }) {
                if let Some(msg) = action_message(&goal, action) {
                    messages.push(msg);
                }
            }
        }
    }

    if !changed.is_empty() && persist {
        crate::bot::handlers::memo_goal::persist_goals(store, sqlite, user_id).await;
    }

    // ── 4. Cache continuity ──────────────────────────────────────────────────
    if persist {
        sqlite
            .save_proactive_cache(user_id, &market_snapshot, &trace.critic_output)
            .await;
    }

    Ok(ScanOutcome {
        current_date,
        actions,
        messages,
        proposals,
        changed,
        trace,
    })
}

/// Pure decision application: mutates the goal and returns
/// `Some((notification, new_status))` when something user-visible happened.
fn apply_action(
    goal: &mut Goal,
    action: &GoalAction,
    now: DateTime<Utc>,
) -> Option<(String, String)> {
    match action.action.as_str() {
        "alert" => {
            if !cooldown_ok(goal, now) {
                return None;
            }
            goal.last_alert_at = Some(now);
            goal.alert_count += 1;
            let msg = action.message.trim();
            if msg.is_empty() {
                None
            } else {
                // Remember what we said so the next scan doesn't repeat it.
                remember_alert(goal, now, msg);
                Some((
                    format!("🎯 Goal #{}: {}", goal.id, msg),
                    "active".to_string(),
                ))
            }
        }
        "done" => {
            goal.done = true;
            goal.active = false;
            // Temporal memory is only kept for active goals — clean up so we
            // don't store junk about finished ones.
            goal.ai_summary = None;
            goal.progress_notes.clear();
            goal.sent_alerts.clear();
            action_message(goal, action).map(|m| (m, "done".to_string()))
        }
        "propose" => {
            // Same cooldown discipline as alerts: don't re-propose the same
            // goal every scan. "Keep active" resets last_alert_at so the AI
            // knows the user already declined.
            if !cooldown_ok(goal, now) || decline_suppressed(goal, now) {
                return None;
            }
            goal.last_alert_at = Some(now);
            goal.alert_count += 1;
            let note = pick_note(action, "the goal may no longer be relevant.");
            remember_alert(goal, now, &note);
            Some((
                format!("💡 Goal #{}: {}", goal.id, note),
                "propose".to_string(),
            ))
        }
        "stale" => {
            // Guardrail: AI is never allowed to unilaterally deactivate user goals.
            // If the model still returns "stale", log a warning and treat it as a no-op / wait.
            warn!(
                "AI returned 'stale' for goal #{} ({}) — suppressed by guardrail (goals cannot be auto-deactivated)",
                goal.id, goal.text
            );
            None
        }
        _ => None, // "wait"
    }
}

/// True when the per-goal alert/propose cooldown (env
/// `PROACTIVE_ALERT_COOLDOWN_HOURS`, default 24h) has elapsed.
fn cooldown_ok(goal: &Goal, now: DateTime<Utc>) -> bool {
    let cooldown = std::env::var("PROACTIVE_ALERT_COOLDOWN_HOURS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|h| *h >= 1)
        .unwrap_or(GOAL_ALERT_COOLDOWN_HOURS);
    goal.last_alert_at
        .map(|t| now.signed_duration_since(t).num_hours() >= cooldown)
        .unwrap_or(true)
}

/// True when the user recently declined an AI proposal to deactivate this goal
/// ("keep active"). Suppresses re-proposing the same goal for a week so the bot
/// doesn't nag the user every day about a goal they already said to keep.
fn decline_suppressed(goal: &Goal, now: DateTime<Utc>) -> bool {
    goal.last_declined_at
        .map(|t| now.signed_duration_since(t).num_hours() < GOAL_DECLINE_SUPPRESS_HOURS)
        .unwrap_or(false)
}

/// Push a sent message into the goal's alert memory (capped at MAX_ALERT_MEMORY).
fn remember_alert(goal: &mut Goal, now: DateTime<Utc>, msg: &str) {
    goal.sent_alerts.push(crate::models::SentAlert {
        at: now,
        text: msg.to_string(),
    });
    if goal.sent_alerts.len() > MAX_ALERT_MEMORY {
        let excess = goal.sent_alerts.len() - MAX_ALERT_MEMORY;
        goal.sent_alerts.drain(..excess);
    }
}

/// Pure notification text for an action (no mutation) — used both by
/// `apply_action` and by report mode (`/plan check`).
fn action_message(goal: &Goal, action: &GoalAction) -> Option<String> {
    match action.action.as_str() {
        "alert" => {
            let msg = action.message.trim();
            if msg.is_empty() {
                None
            } else {
                Some(format!("🎯 Goal #{}: {}", goal.id, msg))
            }
        }
        "done" => {
            let note = pick_note(action, "goal condition met.");
            Some(format!(
                "🎯 Goal #{} \"{}\" — DONE.\n{}",
                goal.id, goal.text, note
            ))
        }
        "propose" => {
            let note = pick_note(action, "the goal may no longer be relevant.");
            Some(format!(
                "💡 Goal #{} \"{}\" — AI suggests reconsidering.\n{}\n\nConfirm to deactivate, or keep it active.",
                goal.id, goal.text, note
            ))
        }
        _ => None, // "wait"
    }
}

fn pick_note(action: &GoalAction, fallback: &str) -> String {
    let m = action.message.trim();
    let r = action.reason.trim();
    if !m.is_empty() {
        m.to_string()
    } else if !r.is_empty() {
        r.to_string()
    } else {
        fallback.to_string()
    }
}

async fn run_goal_pipeline_traced_with<F: Fn(&str, &str)>(
    prompt: &str,
    sqlite: Arc<SqliteStore>,
    on_stage: &F,
) -> anyhow::Result<PipelineTrace> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(90))
        .use_rustls_tls()
        .build()?;
    let gemini_key = std::env::var("GEMINI_API_KEY").unwrap_or_default();
    let search_budget = crate::crypto::ai::search::SearchBudget::new();
    let budget_mgr = Arc::new(crate::crypto::ai::search::BudgetManager::new(search_budget));

    // Stage 1: two powerful generators race. Hard 240s cap so a bad model can
    // never hang the whole scan (each individual call is capped by its client).
    on_stage(
        "progress",
        "Stage 1/3: racing 2 powerful generators (pool: main)...",
    );
    let raced = tokio::time::timeout(
        Duration::from_secs(240),
        pipeline::race_dynamic_generators(
            client.clone(),
            gemini_key.clone(),
            prompt.to_string(),
            None,
            sqlite.clone(),
            budget_mgr.clone(),
        ),
    )
    .await;
    let (draft_a, draft_b, gen_a, gen_b) = match raced {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(anyhow::anyhow!(
                "Generators timed out after 240s (models too slow/rate-limited)"
            ))
        }
    };
    on_stage("progress", "Stage 1/3 done — 2 drafts ready.");
    on_stage("draft_a", &format!("[{}]\n{}", gen_a, draft_a));
    on_stage("draft_b", &format!("[{}]\n{}", gen_b, draft_b));

    // Stage 2: cheap reviewer merges + verifies against ground truth.
    on_stage(
        "progress",
        "Stage 2/3: cheap reviewer merging drafts (pool: news, 90s/call)...",
    );
    let reviewer_prompt = prompts::build_proactive_reviewer_prompt(prompt, &draft_a, &draft_b);
    let reviewers = pools::get_available_models(Some("news"));
    let mut reviewer_model = String::new();
    let mut merged = String::new();
    let mut last_err = anyhow::anyhow!("No reviewer models available");
    for reviewer in reviewers {
        let call = tokio::time::timeout(
            Duration::from_secs(90),
            providers::call_any_model_with_search(
                &client,
                &gemini_key,
                &reviewer,
                &reviewer_prompt,
                Some(sqlite.clone()),
                Some(&budget_mgr),
            ),
        )
        .await;
        match call {
            Ok(Ok(res)) => {
                pools::record_successful_model(&reviewer);
                reviewer_model = reviewer;
                merged = res;
                break;
            }
            Ok(Err(e)) => {
                warn!("Proactive reviewer {} failed: {}", reviewer, e);
                last_err = e;
            }
            Err(_) => {
                warn!("Proactive reviewer {} timed out", reviewer);
                last_err = anyhow::anyhow!("reviewer {} timed out", reviewer);
            }
        }
    }
    if merged.is_empty() {
        // Reviewer failed entirely — fall back to the better draft.
        warn!(
            "Proactive reviewer failed ({}), falling back to raw drafts",
            last_err
        );
        reviewer_model = "(reviewer failed, using raw draft)".to_string();
        merged = if draft_a.len() >= draft_b.len() {
            draft_a.clone()
        } else {
            draft_b.clone()
        };
    }
    on_stage("reviewer", &format!("[{}]\n{}", reviewer_model, merged));

    // Stage 3: powerful critic fact-checks the merged draft.
    on_stage(
        "progress",
        "Stage 3/3: powerful critic fact-checking (pool: critic, 90s/call)...",
    );
    let critic_prompt = prompts::build_proactive_critic_prompt(prompt, &merged);
    let critics = pools::get_available_models(Some("critic"));
    let mut critic_model = String::new();
    let mut final_res = String::new();
    let mut last_err = anyhow::anyhow!("No critic models available");
    for critic in critics {
        let call = tokio::time::timeout(
            Duration::from_secs(90),
            providers::call_any_model_with_search(
                &client,
                &gemini_key,
                &critic,
                &critic_prompt,
                Some(sqlite.clone()),
                Some(&budget_mgr),
            ),
        )
        .await;
        match call {
            Ok(Ok(res)) => {
                pools::record_successful_model(&critic);
                critic_model = critic;
                final_res = res;
                break;
            }
            Ok(Err(e)) => {
                warn!("Proactive critic {} failed: {}", critic, e);
                last_err = e;
            }
            Err(_) => {
                warn!("Proactive critic {} timed out", critic);
                last_err = anyhow::anyhow!("critic {} timed out", critic);
            }
        }
    }
    if final_res.is_empty() {
        warn!("Proactive critic failed ({}), using merged draft", last_err);
        critic_model = "(critic failed, using merged draft)".to_string();
        final_res = merged.clone();
    }
    on_stage("critic", &format!("[{}]\n{}", critic_model, final_res));

    // Stage 4: agentic reflection — a cheap reviewer re-checks the critic's
    // JSON verdict (goal coverage, no stale, numbers vs snapshot, no FOMO,
    // complete sentences) before it is finalized. Bounded to one call; on any
    // failure the critic's output is kept unchanged.
    on_stage(
        "progress",
        "Stage 4/4: cheap reflection reviewer re-checking the verdict (pool: watcher, 30s/call)...",
    );
    if let Some(reflected) = reflect_verdict(&client, &gemini_key, prompt, &final_res).await {
        on_stage(
            "reflection",
            &format!("[reflection reviewer]\n{}", reflected),
        );
        final_res = reflected;
    } else {
        on_stage(
            "reflection",
            "(reflection skipped or failed — keeping critic verdict)",
        );
    }

    Ok(PipelineTrace {
        prompt: prompt.to_string(),
        gen_a_model: gen_a,
        draft_a,
        gen_b_model: gen_b,
        draft_b,
        reviewer_model,
        reviewer_output: merged,
        critic_model,
        critic_output: final_res,
    })
}

/// Phase D — agentic reflection for the goal scan. Runs ONE cheap watcher-tier
/// call that re-checks the critic's JSON verdict and returns a corrected JSON.
/// Returns `None` when the critic output is not parseable JSON, the reviewer
/// fails/times out, or the corrected output is unusable — the caller keeps the
/// critic's verdict in those cases.
async fn reflect_verdict(
    client: &reqwest::Client,
    gemini_key: &str,
    prompt: &str,
    critic_json: &str,
) -> Option<String> {
    // Only reflect when the critic actually produced a JSON object to check.
    if !critic_json.trim_start().starts_with('{') {
        return None;
    }
    let reflection_prompt = prompts::build_proactive_reflection_prompt(prompt, critic_json);
    let models = pools::get_available_models(Some("watcher"));
    for alias in models.into_iter().take(2) {
        let call = tokio::time::timeout(
            Duration::from_secs(30),
            providers::call_any_model(client, gemini_key, &alias, &reflection_prompt, None),
        )
        .await;
        let Ok(Ok(text)) = call else {
            continue;
        };
        // Accept only a clean JSON object (no markdown fences, no commentary).
        let Some(candidate) = extract_json_object(&text) else {
            continue;
        };
        let v: serde_json::Value = serde_json::from_str(&candidate).ok()?;
        if v.get("goals").and_then(|g| g.as_array()).is_none() {
            continue;
        }
        pools::record_successful_model(&alias);
        return Some(candidate);
    }
    None
}

/// Extract the first balanced JSON object from a model response, tolerating
/// markdown fences and stray prose around it. Returns `None` when there is no
/// `{...}` pair or the braces are unbalanced.
pub fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..start + i + 1].to_string());
                }
            }
            '"' => in_string = true,
            _ => {}
        }
    }
    None
}

/// CLI: run one real scan for a user and print every pipeline stage, the parsed
/// actions, the notifications that WOULD be sent, and the state changes applied.
pub async fn run_cli_scan(
    user_id: u64,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    sqlite: Arc<SqliteStore>,
    verbose: bool,
    persist: bool,
) -> anyhow::Result<()> {
    let (goals, memos) = store.read(user_id, |p| (p.goals.clone(), p.memos.clone()));

    println!("\n=== PROACTIVE GOAL SCAN (CLI) ===");
    println!(
        "User: {}{}",
        user_id,
        if persist {
            ""
        } else {
            " (demo — changes NOT persisted)"
        }
    );

    println!("\n--- GOALS ({}) ---", goals.len());
    for g in &goals {
        let status = if g.done {
            "done"
        } else if !g.active {
            "inactive"
        } else {
            "active"
        };
        let exp = g
            .expires_at
            .map(|e| e.format("%Y-%m-%d").to_string())
            .unwrap_or_else(|| "none".into());
        println!(
            "#{} [{}] status={} expires={} alerts={} \"{}\"",
            g.id,
            g.created_at.format("%Y-%m-%d"),
            status,
            exp,
            g.alert_count,
            g.text
        );
    }

    println!("\n--- MEMOS ({}) ---", memos.len());
    for m in &memos {
        println!("#{} \"{}\"", m.id, m.text);
    }

    if goals.iter().all(|g| !g.active || g.done) {
        println!("\n(No active goals — scan would be skipped in the live bot.)");
        return Ok(());
    }

    println!("\nFetching market data + running AI pipeline (2 gens → reviewer → critic)...");
    use std::io::Write;
    let _ = std::io::stdout().flush();

    // Live stage-by-stage output: each callback fires as the stage completes.
    let outcome = run_scan_with(
        &store,
        &cmc,
        &sqlite,
        user_id,
        persist, // apply: real user => yes, demo => no
        persist,
        &|stage, content| {
            let limit = if verbose { usize::MAX } else { 4000 };
            match stage {
                "progress" => println!("\n⏳ {}", content),
                "prompt" => println!(
                    "\n--- PROMPT (fed to the 2 generators) ---\n{}",
                    truncate(content, limit)
                ),
                "draft_a" => println!("\n--- GENERATOR DRAFT A ---\n{}", truncate(content, limit)),
                "draft_b" => println!("\n--- GENERATOR DRAFT B ---\n{}", truncate(content, limit)),
                "reviewer" => println!(
                    "\n--- REVIEWER (merge + verify) ---\n{}",
                    truncate(content, limit)
                ),
                "critic" => println!(
                    "\n--- CRITIC (fact-check) ---\n{}",
                    truncate(content, limit)
                ),
                _ => println!("\n--- {} ---\n{}", stage, truncate(content, limit)),
            }
            // Flush so the output is visible even while later stages are still running.
            let _ = std::io::stdout().flush();
        },
    )
    .await?;

    println!("\n--- PARSED ACTIONS ---");
    if outcome.actions.is_empty() {
        println!("(none — model didn't return parseable per-goal actions)");
    }
    for a in &outcome.actions {
        println!("goal_id {}: {}", a.goal_id, a.action);
        if !a.message.is_empty() {
            println!("  message: {}", a.message);
        }
        if !a.reason.is_empty() {
            println!("  reason: {}", a.reason);
        }
    }

    println!("\n--- WOULD NOTIFY ({} msg) ---", outcome.messages.len());
    for m in &outcome.messages {
        println!("{}", m);
    }

    println!(
        "\n--- PROPOSALS NEEDING USER CONFIRMATION ({} msg) ---",
        outcome.proposals.len()
    );
    for (goal_id, m) in &outcome.proposals {
        println!("goal #{}: {}", goal_id, m);
    }

    println!(
        "\n--- STATE CHANGES APPLIED ({}) ---",
        outcome.changed.len()
    );
    for (id, text, status) in &outcome.changed {
        println!("#{} \"{}\" -> {}", id, text, status);
    }
    println!("\n=== END SCAN ===");
    Ok(())
}

fn parse_goal_actions(json: &str, active_goals: &[Goal]) -> anyhow::Result<Vec<GoalAction>> {
    let start = json
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("No JSON object in model output"))?;
    let end = json.rfind('}').map(|i| i + 1).unwrap_or(json.len());
    let body = &json[start..end];
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| anyhow::anyhow!("Model output is not valid JSON: {}", e))?;

    let goals_arr = v
        .get("goals")
        .and_then(|g| g.as_array())
        .cloned()
        .unwrap_or_default();

    // Index active goals by id so we never act on a goal the user deleted mid-scan.
    let active_ids: std::collections::HashSet<u64> = active_goals.iter().map(|g| g.id).collect();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();

    for g in goals_arr {
        let goal_id = g.get("goal_id").and_then(|x| x.as_u64()).unwrap_or(0);
        if goal_id == 0 || !active_ids.contains(&goal_id) || !seen.insert(goal_id) {
            continue;
        }
        let action = g
            .get("action")
            .and_then(|a| a.as_str())
            .unwrap_or("wait")
            .to_lowercase();
        let message = g
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let reason = g
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("")
            .to_string();
        if matches!(action.as_str(), "alert" | "done" | "propose" | "wait") {
            out.push(GoalAction {
                goal_id,
                action,
                message,
                reason,
            });
        }
    }
    Ok(out)
}

fn truncate(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max).collect();
        format!("{}... (truncated, total {} chars)", head, count)
    }
}

// ── Context builders ──────────────────────────────────────────────────────────

fn build_portfolio_context(store: &Store, user_id: u64) -> String {
    store.read(user_id, |p| {
        if p.holdings.is_empty() && p.purchases.is_empty() {
            return "Portfolio is empty.".to_string();
        }
        let holdings = p.holdings_by_symbol();
        let mut lines = vec![format!("Total invested: {:.2} USD", p.total_invested_usd())];
        for (sym, amt) in &holdings {
            let avg = p
                .avg_buy_price(sym)
                .map(|a| format!(", avg buy: {:.2} USD", a))
                .unwrap_or_default();
            lines.push(format!("- {}: {:.6}{}", sym, amt, avg));
        }
        lines.join("\n")
    })
}

async fn build_market_snapshot(
    cmc: &CryptoClient,
    sqlite: &SqliteStore,
    store: &Store,
    user_id: u64,
) -> String {
    let mut symbols: Vec<String> = store.read(user_id, |p| {
        let mut syms: Vec<String> = p
            .holdings_by_symbol()
            .into_keys()
            .chain(p.settings.default_watchlist.clone())
            .collect();
        syms.sort();
        syms.dedup();
        syms
    });
    for core in ["BTC", "ETH", "SOL"] {
        if !symbols.iter().any(|s| s == core) {
            symbols.push(core.to_string());
        }
    }
    symbols.truncate(MAX_SNAPSHOT_SYMBOLS);

    // Scenario override: deterministic "what-if" market for tests — replaces
    // the live snapshot entirely (same prompt shape, no API calls).
    if let Some(snap) =
        crate::crypto::ai::scenario::active_scenario().and_then(|s| s.market_snapshot(&symbols))
    {
        return snap;
    }

    let (fear_greed, global) = tokio::join!(
        crate::bot::handlers::ai::fetch_fear_and_greed_index(),
        cmc.get_global_metrics(),
    );

    let mut global_str = String::from("GLOBAL MARKET:\n");
    match global {
        Ok(g) => {
            global_str.push_str(&format!(
                "Total Mcap: {:.0} USD | 24h Vol: {:.0} USD | BTC Dom: {:.2}% | ETH Dom: {:.2}%",
                g.total_market_cap, g.total_volume_24h, g.btc_dominance, g.eth_dominance
            ));
            if let Some(s) = g.stablecoin_mcap {
                global_str.push_str(&format!("\nStablecoin Mcap: {:.0} USD", s));
            }
            if let Some(d) = g.dex_volume_24h {
                global_str.push_str(&format!("\nDEX Volume 24h: {:.0} USD", d));
            }
        }
        Err(e) => {
            warn!("Proactive: global metrics failed: {}", e);
            global_str.push_str("Unavailable.");
        }
    }
    global_str.push_str(&format!("\nFear & Greed: {}", fear_greed));

    let sym_refs: Vec<&str> = symbols.iter().map(|s| s.as_str()).collect();
    let mut lines = vec![global_str];
    let mut quote_signals: Vec<crate::crypto::ai::strategy::QuoteSignal> = Vec::new();
    if let Ok(quotes) = cmc.get_quotes(&sym_refs).await {
        for q in quotes {
            quote_signals.push(crate::crypto::ai::strategy::QuoteSignal {
                symbol: q.symbol.clone(),
                price: q.price_usd,
                chg_24h: q.change_24h,
                chg_7d: q.change_7d,
            });
            let mut yaml = format!(
                "- {}: {:.2} USD | 1h: {:+.2}% | 24h: {:+.2}% | 7d: {:+.2}%",
                q.symbol,
                q.price_usd,
                q.change_1h.unwrap_or(0.0),
                q.change_24h.unwrap_or(0.0),
                q.change_7d.unwrap_or(0.0)
            );
            if let Some(c30) = q.change_30d {
                yaml.push_str(&format!(" | 30d: {:+.2}%", c30));
            }
            if let Some(c90) = q.change_90d {
                yaml.push_str(&format!(" | 90d: {:+.2}%", c90));
            }
            if let Some(c365) = q.change_365d {
                yaml.push_str(&format!(" | 365d: {:+.2}%", c365));
            }
            if q.rank > 0 {
                yaml.push_str(&format!(" | rank: #{}", q.rank));
            }
            if q.market_cap > 0.0 {
                let cap = if q.market_cap >= 1e9 {
                    format!("{:.1}B", q.market_cap / 1e9)
                } else {
                    format!("{:.1}M", q.market_cap / 1e6)
                };
                yaml.push_str(&format!(" | mcap: {}", cap));
            }
            if let Some(ath) = q.ath {
                let disc = q.ath_change_percentage.unwrap_or_else(|| {
                    if ath > 0.0 {
                        (q.price_usd - ath) / ath * 100.0
                    } else {
                        0.0
                    }
                });
                yaml.push_str(&format!(
                    " | peak: {:.2} USD ({:+.2}% from peak)",
                    ath, disc
                ));
            }
            lines.push(yaml);
        }
    }

    // Milestones (support/resistance anchors) from the cache — cheap, no extra API.
    for sym in symbols.iter().take(6) {
        if let Ok(milestones) = sqlite.get_price_milestones(sym).await {
            if !milestones.is_empty() {
                let ms: Vec<String> = milestones
                    .iter()
                    .map(|(name, price, _)| format!("{}: {:.2} USD", name, price))
                    .collect();
                lines.push(format!("- {} milestones: {}", sym, ms.join(", ")));
            }
        }
    }

    // Phase B — local market memory: price trajectory from the bot's own
    // history (30d), so the goal scan sees the trend, not just the snapshot.
    let traj = crate::crypto::ai::market_memory::trajectory_context(sqlite, &symbols, 30).await;
    if !traj.is_empty() {
        lines.push(traj);
    }
    // Chart view: RSI / SMA / support-resistance / volatility from local OHLCV.
    let chart = crate::crypto::ai::market_memory::chart_context_block(sqlite, &symbols, 30).await;
    if !chart.is_empty() {
        lines.push(chart);
    }
    // Trained strategy: top configs from historical sweeps for these symbols.
    let trained = crate::crypto::ai::market_memory::trained_strategy_block(&symbols, 3).await;
    if !trained.is_empty() {
        lines.push(trained);
    }
    // Market calendar: halving phase + trained monthly seasonality.
    let calendar = crate::crypto::ai::market_memory::seasonality_context_block(&symbols).await;
    if !calendar.is_empty() {
        lines.push(calendar);
    }

    // Phase C — deterministic strategy layer: regime rules for the scan.
    let strat = crate::crypto::ai::strategy::guidance(
        crate::crypto::ai::strategy::parse_fear_greed(&fear_greed),
        &quote_signals,
    );
    if !strat.is_empty() {
        lines.push(strat);
    }

    lines.join("\n")
}

async fn build_aux_context(sqlite: &SqliteStore, store: &Store, user_id: u64) -> (String, String) {
    // News: cached summaries for portfolio symbols + general.
    let symbols = store.read(user_id, |p| {
        let mut syms: Vec<String> = p.holdings_by_symbol().into_keys().collect();
        syms.sort();
        syms.dedup();
        syms
    });
    let mut news_parts: Vec<String> = Vec::new();
    for sym in symbols.iter().take(3) {
        if let Ok(Some(news)) = sqlite.get_news_summary(sym).await {
            if !news.trim().is_empty() && !news.contains("Failed to analyze") {
                news_parts.push(format!("{}: {}", sym, news));
            }
        }
    }
    if let Ok(Some(general)) = sqlite.get_news_summary("general").await {
        if !general.trim().is_empty() && !general.contains("Failed to analyze") {
            news_parts.push(format!("GENERAL: {}", general));
        }
    }
    let news_context = if news_parts.is_empty() {
        "(no cached news)".to_string()
    } else {
        news_parts.join("\n\n")
    };

    // Market intel cache (refreshed every 30 min by the dedicated worker).
    let mut intel_parts: Vec<String> = Vec::new();
    if let Ok(Some(b)) = sqlite.get_macro_summary("BTC_NETWORK", 2).await {
        intel_parts.push(format!("BTC Network:\n{}", b));
    }
    if let Ok(Some(d)) = sqlite.get_macro_summary("DERIVATIVES", 2).await {
        intel_parts.push(format!("Derivatives:\n{}", d));
    }
    if let Ok(Some(g)) = sqlite.get_macro_summary("GLOBAL", 12).await {
        intel_parts.push(format!("Macro Cycle:\n{}", g));
    }
    (news_context, intel_parts.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Goal;
    use chrono::Duration as ChronoDuration;

    fn goal(id: u64) -> Goal {
        Goal {
            id,
            text: format!("goal {}", id),
            created_at: Utc::now(),
            expires_at: None,
            active: true,
            last_alert_at: None,
            last_declined_at: None,
            alert_count: 0,
            done: false,
            progress_notes: Vec::new(),
            ai_summary: None,
            sent_alerts: Vec::new(),
        }
    }

    #[test]
    fn parse_actions_ok() {
        let json = r#"{
            "market_read": "dip",
            "goals": [
                {"goal_id": 1, "action": "alert", "message": "ETH dipped to support, good DCA point", "reason": "x"},
                {"goal_id": 2, "action": "wait", "message": "", "reason": "no signal"},
                {"goal_id": 3, "action": "done", "message": "", "reason": "x2 reached"},
                {"goal_id": 4, "action": "propose", "message": "", "reason": "goal outdated"}
            ]
        }"#;
        let goals = vec![goal(1), goal(2), goal(3), goal(4)];
        let actions = parse_goal_actions(json, &goals).unwrap();
        assert_eq!(actions.len(), 4);
        assert_eq!(actions[0].goal_id, 1);
        assert_eq!(actions[0].action, "alert");
        assert_eq!(actions[2].reason, "x2 reached");
        assert_eq!(actions[3].action, "propose");
    }

    #[test]
    fn parse_actions_skips_unknown_goal_ids() {
        let json = r#"{"goals":[{"goal_id": 99, "action": "alert", "message": "fake"}]}"#;
        let goals: Vec<Goal> = vec![];
        let actions = parse_goal_actions(json, &goals).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn parse_actions_skips_stale() {
        // The AI is never allowed to deactivate goals: "stale" is dropped by
        // the parser so it can never reach apply_action or the report mode.
        let json = r#"{"goals":[{"goal_id": 1, "action": "stale", "message": "", "reason": "no longer relevant"}]}"#;
        let goals = vec![goal(1)];
        let actions = parse_goal_actions(json, &goals).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn parse_bad_json_errors() {
        assert!(parse_goal_actions("not json at all", &[]).is_err());
    }

    #[test]
    fn apply_done_notifies_with_reason() {
        let mut g = goal(1);
        let a = GoalAction {
            goal_id: 1,
            action: "done".into(),
            message: String::new(),
            reason: "BTC x2 target hit at 200k".into(),
        };
        let (msg, status) = apply_action(&mut g, &a, Utc::now()).unwrap();
        assert_eq!(status, "done");
        assert!(g.done && !g.active);
        assert!(msg.contains("DONE"));
        assert!(msg.contains("BTC x2 target hit at 200k"));
        // Temporal memory is cleaned up for finished goals (no junk kept).
        assert!(g.ai_summary.is_none());
        assert!(g.progress_notes.is_empty());
    }

    #[test]
    fn apply_stale_is_suppressed() {
        // Guardrail: the AI can never deactivate a goal. "stale" is a no-op
        // that keeps the goal fully active (the parser also drops it).
        let mut g = goal(1);
        let a = GoalAction {
            goal_id: 1,
            action: "stale".into(),
            message: String::new(),
            reason: "goal expired, superseded by memo #2".into(),
        };
        assert!(apply_action(&mut g, &a, Utc::now()).is_none());
        assert!(g.active);
        assert!(!g.done);
        assert!(g.progress_notes.is_empty());
    }

    #[test]
    fn apply_propose_notifies_with_reason() {
        let mut g = goal(1);
        let a = GoalAction {
            goal_id: 1,
            action: "propose".into(),
            message: String::new(),
            reason: "BTC is far above the 62-63k target; consider pausing this goal".into(),
        };
        let (msg, status) = apply_action(&mut g, &a, Utc::now()).unwrap();
        assert_eq!(status, "propose");
        assert!(g.active); // proposal never mutates goal state
        assert!(msg.contains("💡"));
        assert!(msg.contains("62-63k"));
        // The proposal is remembered so the AI doesn't repeat it next scan.
        assert_eq!(g.sent_alerts.len(), 1);
        assert_eq!(g.alert_count, 1);
    }

    #[test]
    fn apply_propose_honors_cooldown() {
        let mut g = goal(1);
        let a = GoalAction {
            goal_id: 1,
            action: "propose".into(),
            message: "reconsider".into(),
            reason: String::new(),
        };
        let now = Utc::now();
        assert!(apply_action(&mut g, &a, now).is_some());
        // Second proposal immediately after — suppressed by cooldown.
        assert!(apply_action(&mut g, &a, now).is_none());
        // After 24h+ it may propose again.
        let later = now + ChronoDuration::hours(25);
        assert!(apply_action(&mut g, &a, later).is_some());
        assert_eq!(g.alert_count, 2);
    }

    #[test]
    fn apply_propose_suppressed_after_user_decline() {
        let mut g = goal(1);
        // User clicked "keep active" — the bot records the decline.
        g.last_declined_at = Some(Utc::now());
        let a = GoalAction {
            goal_id: 1,
            action: "propose".into(),
            message: "reconsider".into(),
            reason: String::new(),
        };
        // Even after the normal 24h cooldown, the decline suppresses re-proposing.
        let later = Utc::now() + ChronoDuration::hours(25);
        assert!(apply_action(&mut g, &a, later).is_none());
        // After the suppression window (7 days) it may propose again.
        let much_later = Utc::now() + ChronoDuration::hours(24 * 7 + 1);
        assert!(apply_action(&mut g, &a, much_later).is_some());
    }

    #[test]
    fn apply_alert_honors_cooldown() {
        let mut g = goal(1);
        let a = GoalAction {
            goal_id: 1,
            action: "alert".into(),
            message: "buy the dip".into(),
            reason: String::new(),
        };
        let now = Utc::now();
        assert!(apply_action(&mut g, &a, now).is_some());
        assert_eq!(g.alert_count, 1);
        // Second alert immediately after — suppressed by cooldown.
        assert!(apply_action(&mut g, &a, now).is_none());
        // Still suppressed before the 24h cooldown elapses.
        let early = now + ChronoDuration::hours(20);
        assert!(apply_action(&mut g, &a, early).is_none());
        // After 24h+ it fires again.
        let later = now + ChronoDuration::hours(25);
        assert!(apply_action(&mut g, &a, later).is_some());
        assert_eq!(g.alert_count, 2);
    }

    #[test]
    fn alert_message_is_remembered_for_prompt() {
        let mut g = goal(1);
        let a = GoalAction {
            goal_id: 1,
            action: "alert".into(),
            message: "ETH dipped to support".into(),
            reason: String::new(),
        };
        apply_action(&mut g, &a, Utc::now()).unwrap();
        assert_eq!(g.sent_alerts.len(), 1);
        assert_eq!(g.sent_alerts[0].text, "ETH dipped to support");
    }

    #[test]
    fn extract_json_object_handles_fences_and_prose() {
        let with_fences = "```json\n{\"goals\": [{\"goal_id\": 1, \"action\": \"wait\"}]}\n```";
        let got = extract_json_object(with_fences).unwrap();
        assert!(got.starts_with('{'));
        assert!(serde_json::from_str::<serde_json::Value>(&got).is_ok());

        let with_prose = "Here is the verdict: {\"goals\": []} hope that helps.";
        assert_eq!(extract_json_object(with_prose).unwrap(), "{\"goals\": []}");

        // Braces inside strings must not break the balance.
        let tricky = "{\"message\": \"price { 60-63k } zone\", \"goals\": []}";
        let got2 = extract_json_object(tricky).unwrap();
        assert!(serde_json::from_str::<serde_json::Value>(&got2).is_ok());

        assert!(extract_json_object("no json here").is_none());
        assert!(extract_json_object("{\"unclosed\": ").is_none());
    }

    #[test]
    fn reflection_prompt_forbids_stale_and_fomo() {
        let p = prompts::build_proactive_reflection_prompt(
            "CURRENT MARKET SNAPSHOT: BTC 78k +23% 7d (vertical pump)",
            "{\"goals\": [{\"goal_id\": 1, \"action\": \"alert\", \"message\": \"buy now at the top\"}]}",
        );
        assert!(p.contains("stale"));
        assert!(p.contains("ANTI-FOMO"));
        assert!(p.contains("vertical pump"));
        assert!(p.contains("complete sentences"));
    }
}
