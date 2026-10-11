pub mod commands;
pub mod handlers;
pub mod keyboards;
pub mod rich_messages;
pub mod state;

use std::sync::Arc;
use teloxide::{
    dispatching::{
        dialogue::{self, InMemStorage},
        UpdateHandler,
    },
    net::Download,
    prelude::*,
    types::ParseMode,
    utils::command::BotCommands,
};

use crate::{
    access::AccessControl,
    config::Config,
    crypto::{BlockchainClient, CryptoClient},
    db::Store,
    models::{AlertCondition, Period},
};
use commands::Command;
use handlers::nlp::{parse, NlpCommand};
use handlers::{new_patch_store, PatchStore};
use state::State;

type MyDialogue = Dialogue<State, InMemStorage<State>>;

pub async fn start(
    bot: Bot,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    blockchain: Arc<BlockchainClient>,
    acl: Arc<AccessControl>,
    config: Arc<Config>,
    sqlite: Arc<crate::db::SqliteStore>,
) {
    let handler = build_handler();
    let storage = InMemStorage::<State>::new();
    let patch_store: PatchStore = new_patch_store();
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![
            store,
            cmc,
            blockchain,
            acl,
            config,
            storage,
            patch_store,
            sqlite
        ])
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;
}

fn build_handler() -> UpdateHandler<anyhow::Error> {
    use dptree::case;

    let acl_filter = dptree::entry().filter(|update: Update, acl: Arc<AccessControl>| {
        tracing::debug!("Incoming update ID: {}", update.id);
        if let Some(user) = update.user() {
            let allowed = acl.is_allowed(user.id.0);
            tracing::debug!("User ID: {} - Allowed: {}", user.id, allowed);
            allowed
        } else {
            tracing::debug!("Update has no user: {:?}", update);
            false
        }
    });

    let commands = teloxide::filter_command::<Command, _>()
        .branch(case![Command::Start].endpoint(handle_start))
        .branch(case![Command::B].endpoint(handle_start))
        .branch(case![Command::Help].endpoint(handle_help))
        .branch(case![Command::Price(a)].endpoint(handle_price_cmd))
        .branch(case![Command::Top(a)].endpoint(handle_top_cmd))
        .branch(case![Command::Portfolio].endpoint(handle_portfolio_cmd))
        .branch(case![Command::Pnl].endpoint(handle_pnl_cmd))
        .branch(case![Command::Distribution].endpoint(handle_distribution_cmd))
        .branch(case![Command::Add(a)].endpoint(handle_add_cmd))
        .branch(case![Command::Move(a)].endpoint(handle_move_cmd))
        .branch(case![Command::Correct(a)].endpoint(handle_correct_cmd))
        .branch(case![Command::Adjust(a)].endpoint(handle_adjust_cmd))
        .branch(case![Command::Invested(a)].endpoint(handle_invested_cmd))
        .branch(case![Command::Alert(a)].endpoint(handle_alert_cmd))
        .branch(case![Command::Wallet(a)].endpoint(handle_wallet_cmd))
        .branch(case![Command::Watchlist(a)].endpoint(handle_watchlist_cmd))
        .branch(case![Command::Convert(a)].endpoint(handle_convert_cmd))
        .branch(case![Command::Backup].endpoint(handle_backup_cmd))
        .branch(case![Command::Restore].endpoint(handle_restore_cmd))
        .branch(case![Command::Advice].endpoint(handle_advice_cmd))
        .branch(case![Command::Ai(a)].endpoint(handle_ai_cmd))
        .branch(case![Command::Patch(a)].endpoint(handle_patch_cmd_ep))
        .branch(case![Command::Path(a)].endpoint(handle_patch_cmd_ep))
        .branch(case![Command::Ask(a)].endpoint(handle_ask_cmd_ep))
        .branch(case![Command::Memo(a)].endpoint(handle_memo_cmd))
        .branch(case![Command::Goal(a)].endpoint(handle_goal_cmd))
        .branch(case![Command::Plan(a)].endpoint(handle_plan_cmd))
        .branch(case![Command::News(a)].endpoint(handle_news_cmd))
        .branch(case![Command::Trade(a)].endpoint(handle_trade_cmd))
        .branch(case![Command::TradeTask(a)].endpoint(handle_tradetask_cmd))
        .branch(case![Command::Settings].endpoint(handle_settings_cmd))
        .branch(case![Command::Model(a)].endpoint(handle_model_cmd))
        .branch(case![Command::Allow(a)].endpoint(handle_allow_cmd))
        .branch(case![Command::Revoke(a)].endpoint(handle_revoke_cmd))
        .branch(case![Command::Reset].endpoint(handle_reset_cmd))
        .branch(case![Command::Fng].endpoint(handle_fng_cmd))
        .branch(case![Command::Acl].endpoint(handle_acl_cmd))
        .branch(case![Command::Cancel].endpoint(handle_cancel))
        .branch(case![Command::Ver].endpoint(handle_ver));

    let callbacks = Update::filter_callback_query()
        .branch(
            dptree::filter(|q: CallbackQuery| {
                q.data
                    .as_deref()
                    .map(|d| {
                        d == "plan"
                            || d.starts_with("plan:")
                            || d.starts_with("goal:confirm:")
                            || d.starts_with("goal:keep:")
                    })
                    .unwrap_or(false)
            })
            .endpoint(handle_plan_cb),
        )
        .branch(
            dptree::filter(|q: CallbackQuery| {
                q.data.as_deref().unwrap_or("").starts_with("setting")
            })
            .endpoint(handle_settings_cb),
        )
        .branch(dptree::endpoint(handle_callback));
    let documents = Message::filter_document().endpoint(handle_document);
    let text_msgs = Message::filter_text().endpoint(handle_text);

    let main_chain = dialogue::enter::<Update, InMemStorage<State>, State, _>()
        .branch(callbacks)
        .branch(
            Update::filter_message()
                .branch(documents)
                .branch(case![State::Idle].branch(commands).branch(text_msgs))
                .branch(case![State::AiWizard].endpoint(handlers::ai::handle_ai_wizard))
                .branch(case![State::AlertStep1].endpoint(handlers::alert::handle_alert_step1))
                .branch(
                    case![State::AlertStep2 { symbol, condition }]
                        .endpoint(handlers::alert::handle_alert_step2),
                )
                .branch(
                    case![State::AlertStep3 {
                        symbol,
                        condition,
                        threshold,
                        period
                    }]
                    .endpoint(handlers::alert::handle_alert_step3),
                )
                .branch(case![State::WalletStep1].endpoint(handlers::wallet::handle_wallet_step1))
                .branch(
                    case![State::WalletStep2 { name, kind }]
                        .endpoint(handlers::wallet::handle_wallet_step2),
                ),
        );

    acl_filter.branch(main_chain)
}

// ── Command endpoints ─────────────────────────────────────────────────────────

async fn handle_start(bot: Bot, msg: Message) -> anyhow::Result<()> {
    bot.send_message(
        msg.chat.id,
        "🤖 *Cryptograph* — your crypto intelligence bot\\.\n\n\
         Use the buttons below or type commands directly\\.",
    )
    .parse_mode(ParseMode::MarkdownV2)
    .reply_markup(keyboards::main_menu())
    .await?;
    Ok(())
}

async fn handle_allow_cmd(
    bot: Bot,
    msg: Message,
    acl: Arc<AccessControl>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_allow(bot, msg, acl, args).await
}

async fn handle_revoke_cmd(
    bot: Bot,
    msg: Message,
    acl: Arc<AccessControl>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_revoke(bot, msg, acl, args).await
}

async fn handle_acl_cmd(bot: Bot, msg: Message, acl: Arc<AccessControl>) -> anyhow::Result<()> {
    handlers::handle_acl_list(bot, msg, acl).await
}

async fn handle_help(bot: Bot, msg: Message) -> anyhow::Result<()> {
    // HTML parse mode: far more forgiving than MarkdownV2 (no need to escape
    // (), [], |, +, ., #, ! …). Only <, > and & must be entity-escaped.
    let help_text = "🤖 <b>Cryptograph Bot Command Reference</b>\n\n\
                     📈 <b>Market Intelligence</b>\n\
                     • <code>/price</code> [SYMS] — Get current quotes (e.g. <code>/price BTC ETH SOL</code>)\n\
                     • <code>/top</code> [N] — Top N coins by 24h volume (e.g. <code>/top 10</code>)\n\
                     • <code>/convert</code> [AMT] [FROM] [TO] — Currency converter (e.g. <code>/convert 100 BYN BTC</code>)\n\n\
                     💼 <b>Portfolio Management</b>\n\
                     • <code>/portfolio</code> — Text overview of assets, values, P&amp;L\n\
                     • <code>/pnl</code> — Send P&amp;L breakdown infographic chart\n\
                     • <code>/distribution</code> — Send asset allocation pie chart\n\
                     • <code>/add</code> [SYM] [AMT] [COST] [WALLET] — Record purchase (e.g. <code>/add BTC 0.01 600 exchange:binance</code>)\n\
                     • <code>/move</code> [SYM] [AMT] <code>from:W</code> <code>to:W</code> [fee:AMT] — Transfer (e.g. <code>/move BTC 0.1 from:binance to:ledger fee:0.0001</code>)\n\
                     • <code>/adjust</code> [W] [SYM] [AMT] — Manually set balance (e.g. <code>/adjust wallet:ledger BTC 0.05</code>)\n\
                     • <code>/correct</code> <code>#ID</code> [field] [value] — Edit purchase record (e.g. <code>/correct #1 amount 0.02</code>)\n\
                     • <code>/history</code> [SYM] — Purchase history (e.g. <code>/history BTC</code>)\n\
                     • <code>/watchlist</code> — Watchlist: <code>add SYM</code> | <code>remove SYM</code> | <code>list</code>\n\
                     • <code>/invested</code> [SYM] [AMT] — Manually set total invested (e.g. <code>/invested BTC 5000</code>)\n\
                     🔔 <b>Price Alerts</b>\n\
                     • <code>/alert list</code> — View all configured price alerts\n\
                     • <code>/alert add</code> [SYM] [COND] [cooldown:Nh] — Add price alert:\n\
                     \u{200b}  ├  <code>BTC &lt; 60000</code> — Below 60k\n\
                     \u{200b}  ├  <code>ETH &gt; 4000</code> — Above 4k\n\
                     \u{200b}  ├  <code>BTC 24h &lt; -5%</code> — 24h drop over 5%\n\
                     \u{200b}  └  (Example: <code>/alert add BTC &gt; 150000 cooldown:6h</code>)\n\
                     • <code>/alert del</code> [ID] — Delete price alert (e.g. <code>/alert del 1</code>)\n\n\
                     👛 <b>Wallets</b>\n\
                     • <code>/wallet list</code> — List all wallets and balances\n\
                     • <code>/wallet add</code> <code>kind:NAME</code> [SYM] [ADDR] — Add wallet (e.g. <code>/wallet add cold:ledger BTC bc1q...</code>)\n\
                     • <code>/wallet refresh</code> — Force balance update from blockchain\n\n\
                     🧠 <b>AI Assistant</b>\n\
                     • <code>/ai</code> [SYM] — Deep market analysis (e.g. <code>/ai BTC</code>)\n\
                     • <code>/ai set</code> [SYMS] — Set multi-asset watchlist (e.g. <code>/ai set BTC ETH SOL</code>)\n\
                     • <code>/patch</code> [text] — AI portfolio corrector (e.g. <code>/patch I have 0.1 BTC not 0.5</code>)\n\
                     • <code>/ask [question]</code> — Ask AI about your portfolio + market (e.g. <code>/ask should I buy SOL?</code>)\n\
                     • <code>/memo [text]</code> — AI memory note (e.g. <code>/memo invest 100 USD at a time</code>); <code>/memo list</code> | <code>/memo del N</code>\n\
                     • <code>/goal [text]</code> — Proactive goal; AI alerts you when market conditions fit (e.g. <code>/goal want to spend 400 USD on crypto this month</code>); <code>/goal list</code> | <code>/goal progress N text</code> | <code>/goal done N</code> | <code>/goal del N</code>\n\
                     • <code>/plan</code> — Plan dashboard menu; <code>/plan status</code> | <code>/plan check</code> (ask the AI about your plan now) | <code>/plan ask question</code>\n\
                     • <code>/advice</code> — Long-term investment intelligence &amp; market sentiment\n\
                     📰 <b>News</b>\n\
                     • <code>/news</code> [SYM] — Recent crypto news (e.g. <code>/news BTC</code>)\n\
                     • <code>/fng</code> — Fear &amp; Greed Index\n\
                     📊 <b>Trading (commander mode)</b>\n\
                     • <code>/trade list</code> — Allocations, constraints, rules, PnL\n\
                     • <code>/trade pnl</code> — Realized + mark-to-market PnL\n\
                     • <code>/trade allocate</code> [SYM] [AMT] — Set monthly budget\n\
                     • <code>/trade rule</code> [add|del] — Trading rules (dip %, ceiling, etc.)\n\
                     • <code>/tradetask list</code> — Standing advisor instructions\n\
                     • <code>/tradetask add</code> [--name N] [--assets SYMS] — Add a task\n\
                     • <code>/tradetask advise</code> — Run advisor now\n\
                     ⚙️ <b>System &amp; Security</b>\n\
                     • <code>/settings</code> — Interactive settings menu (includes /stats)\n\
                     • <code>/model</code> — Select AI model: <code>list</code> | <code>set N</code>\n\
                     • <code>/ver</code> — Show bot build version/hash\n\
                     • <code>/backup</code> — Export JSON backup file\n\
                     • <code>/restore</code> — Restore from JSON backup file (send file after command)\n\
                     • <code>/reset</code> — Admin: reset stats and caches (news, milestones, health); portfolio is kept\n\
                     • <code>/cancel</code> — Cancel current dialog state";

    bot.send_message(msg.chat.id, help_text)
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

async fn handle_price_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);
    handlers::handle_price(bot, msg, user_id, store, cmc, config, args).await
}
async fn handle_top_cmd(
    bot: Bot,
    msg: Message,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);
    let limit = args.trim().parse().unwrap_or(10);
    handlers::handle_top(bot, msg, user_id, cmc, config, limit.to_string()).await
}
async fn handle_portfolio_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    handlers::handle_portfolio(bot, msg, store, cmc, config).await
}
async fn handle_pnl_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    handlers::handle_pnl_chart(bot, msg, store, cmc, config).await
}
async fn handle_distribution_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    handlers::handle_portfolio_chart(bot, msg, store, cmc, config).await
}
async fn handle_add_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_add(bot, msg, store, cmc, config, args).await
}
async fn handle_move_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_move(bot, msg, store, config, args).await
}
async fn handle_correct_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_correct(bot, msg, store, config, args).await
}
async fn handle_adjust_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_adjust(bot, msg, store, config, args).await
}
async fn handle_invested_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_invested_cmd(bot, msg, store, config, args).await
}
async fn handle_backup_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    acl: Arc<AccessControl>,
) -> anyhow::Result<()> {
    handlers::handle_backup(bot, msg, store, acl).await
}
async fn handle_restore_cmd(bot: Bot, msg: Message) -> anyhow::Result<()> {
    bot.send_message(
        msg.chat.id,
        "📤 Send your `.json` or `.cgrp` backup file now\\.",
    )
    .parse_mode(ParseMode::MarkdownV2)
    .await?;
    Ok(())
}
async fn handle_advice_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    handlers::portfolio::handle_advice(bot, msg, store, cmc, config).await
}
async fn handle_ai_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_ai_cmd(bot, msg, store, cmc, config, args).await
}
async fn handle_patch_cmd_ep(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    patch_store: PatchStore,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_patch_cmd(bot, msg, store, cmc, config, patch_store, args).await
}
async fn handle_ask_cmd_ep(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    sqlite: Arc<crate::db::SqliteStore>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_ask_cmd(bot, msg, store, cmc, config, sqlite, args).await
}
async fn handle_memo_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    sqlite: Arc<crate::db::SqliteStore>,
    args: String,
) -> anyhow::Result<()> {
    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);
    handlers::handle_memo_cmd(bot, msg, store, sqlite, user_id, args).await
}
async fn handle_goal_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    sqlite: Arc<crate::db::SqliteStore>,
    args: String,
) -> anyhow::Result<()> {
    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);
    handlers::handle_goal_cmd(bot, msg, store, sqlite, user_id, args).await
}
async fn handle_plan_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    sqlite: Arc<crate::db::SqliteStore>,
    args: String,
) -> anyhow::Result<()> {
    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);
    handlers::handle_plan_cmd(bot, msg, store, cmc, config, sqlite, user_id, args).await
}
async fn handle_news_cmd(
    bot: Bot,
    msg: Message,
    _store: Arc<Store>,
    _cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_news_cmd(bot, msg, config, args).await
}
async fn handle_trade_cmd(
    bot: Bot,
    msg: Message,
    _store: Arc<Store>,
    _cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_trade_cmd(bot, msg, config, args).await
}
async fn handle_tradetask_cmd(
    bot: Bot,
    msg: Message,
    _store: Arc<Store>,
    _cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_tradetask_cmd(bot, msg, config, args).await
}

async fn handle_fng_cmd(
    bot: Bot,
    msg: Message,
    _store: Arc<Store>,
    cmc: Arc<CryptoClient>,
) -> anyhow::Result<()> {
    handlers::handle_fng_cmd(bot, msg, cmc).await
}
async fn handle_model_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_model_cmd(bot, msg, store, config, args).await
}
async fn handle_settings_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
) -> anyhow::Result<()> {
    handlers::handle_settings(bot, msg, store, cmc, config).await
}
async fn handle_watchlist_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_watchlist(bot, msg, store, config, args).await
}
async fn handle_convert_cmd(
    bot: Bot,
    msg: Message,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    handlers::handle_convert(bot, msg, cmc, config, args).await
}
async fn handle_cancel(bot: Bot, msg: Message, dialogue: MyDialogue) -> anyhow::Result<()> {
    dialogue.exit().await?;
    bot.send_message(msg.chat.id, "❌ Cancelled")
        .reply_markup(keyboards::main_menu())
        .await?;
    Ok(())
}

async fn handle_ver(bot: Bot, msg: Message) -> anyhow::Result<()> {
    let hash = include_str!("version.txt").trim();
    let display_hash = if hash.is_empty() { "unknown" } else { hash };
    bot.send_message(msg.chat.id, format!("🤖 Build Hash: `{}`", display_hash))
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
    Ok(())
}

/// `/reset` — admin-only: wipe non-critical data (news summaries, macro cache,
/// price milestones, model health, stats, quote meta, local history) while
/// KEEPING portfolios, wallets, goals, memos and the paid-usage budget.
async fn handle_reset_cmd(
    bot: Bot,
    msg: Message,
    acl: Arc<AccessControl>,
    sqlite: Arc<crate::db::SqliteStore>,
) -> anyhow::Result<()> {
    let user_id = msg.chat.id.0 as u64;
    if !acl.is_allowed(user_id) {
        bot.send_message(msg.chat.id, "⛔ Unauthorized").await?;
        return Ok(());
    }
    match sqlite.wipe_knowledge() {
        Ok(n) => {
            // Drain in-memory metric counters so pending deltas don't get
            // re-flushed into the freshly wiped tables on the next 5-min tick.
            crate::crypto::ai::metrics::take_deltas();
            crate::crypto::ai::metrics::take_search_deltas();
            crate::bot::handlers::settings::update_stats_cache(&sqlite).await;
            bot.send_message(
                msg.chat.id,
                format!(
                    "🧹 *Knowledge base reset:* `{n}` rows removed from cache/stat tables\\.\n\
                     ✅ Kept: portfolio, wallets, goals, memos, paid\\-usage budget\\.",
                ),
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        }
        Err(e) => {
            bot.send_message(msg.chat.id, format!("❌ Reset failed: {e}"))
                .await?;
        }
    }
    Ok(())
}

async fn handle_alert_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    args: String,
) -> anyhow::Result<()> {
    let parts: Vec<&str> = args.splitn(2, ' ').collect();
    match parts.as_slice() {
        ["list"] | [] => handlers::handle_alert_list(bot, msg, store, config).await,
        ["reset" | "default" | "restore_defaults"] => {
            handlers::handle_alert_reset(bot, msg, store, config).await
        }
        ["add", rest] => {
            handlers::handle_alert_add_cmd(bot, msg, store, config, cmc, rest.to_string()).await
        }
        ["del", id] | ["delete", id] => {
            let id = id.trim_start_matches('#').parse().unwrap_or(0);
            handlers::handle_alert_delete(bot, msg, store, config, id).await
        }
        _ => handlers::handle_alert_list(bot, msg, store, config).await,
    }
}

async fn handle_wallet_cmd(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    config: Arc<Config>,
    blockchain: Arc<BlockchainClient>,
    args: String,
) -> anyhow::Result<()> {
    let parts: Vec<&str> = args.splitn(2, ' ').collect();
    match parts.as_slice() {
        ["add", rest] => {
            handlers::handle_wallet_add(bot, msg, store, config, rest.to_string()).await
        }
        ["list"] | [] => handlers::handle_wallet_list(bot, msg, store, config).await,
        ["refresh"] => {
            handlers::handle_wallet_refresh(bot, msg, store, config, blockchain, None).await
        }
        ["del" | "delete", id] => {
            let id = id.trim_start_matches('#').parse().unwrap_or(0);
            handlers::handle_wallet_delete(bot, msg, store, config, id).await
        }
        _ => handlers::handle_wallet_list(bot, msg, store, config).await,
    }
}

// ── Callback query handler ─────────────────────────────────────────────────────

/// Plan dashboard callbacks (`/plan` menu buttons) — kept separate from
/// [`handle_callback`] so it stays within the injectable dependency count.
async fn handle_plan_cb(
    bot: Bot,
    q: CallbackQuery,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    sqlite: Arc<crate::db::SqliteStore>,
) -> anyhow::Result<()> {
    let _ = bot.answer_callback_query(&q.id).await;
    let data = match q.data {
        Some(d) => d,
        None => return Ok(()),
    };
    let msg = match q.message {
        Some(m) => m,
        None => return Ok(()),
    };
    let user_id = q.from.id.0;
    use keyboards::cb;
    match data.as_str() {
        cb::PLAN => {
            bot.send_message(
                msg.chat.id,
                "🎯 <b>Plan dashboard</b>\n\n\
                 • <b>Goals</b> — proactive goals with progress & AI reasoning\n\
                 • <b>Memos</b> — AI memory notes\n\
                 • <b>Plan Status</b> — full timeline of every goal\n\
                 • <b>AI Check</b> — ask the AI right now about the state of your plan\n\
                 • <b>Help</b> — full command reference",
            )
            .parse_mode(ParseMode::Html)
            .reply_markup(keyboards::plan_menu())
            .await?;
        }
        cb::PLAN_GOALS => {
            handlers::handle_goal_cmd(bot, msg, store, sqlite, user_id, "list".into()).await?
        }
        cb::PLAN_MEMOS => {
            handlers::handle_memo_cmd(bot, msg, store, sqlite, user_id, "list".into()).await?
        }
        cb::PLAN_STATUS => {
            handlers::handle_plan_cmd(
                bot,
                msg,
                store,
                cmc,
                config,
                sqlite,
                user_id,
                "status".into(),
            )
            .await?
        }
        cb::PLAN_CHECK => {
            handlers::handle_plan_cmd(
                bot,
                msg,
                store,
                cmc,
                config,
                sqlite,
                user_id,
                "check".into(),
            )
            .await?
        }
        cb::PLAN_HELP => {
            handle_help(bot, msg).await?;
        }
        // User confirmed an AI proposal to deactivate a goal. The AI can only
        // *propose*; deactivation happens exclusively here, on user action.
        d if d.starts_with("goal:confirm:") => {
            let goal_id: u64 = d
                .strip_prefix("goal:confirm:")
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            let found = store.write(user_id, |p| {
                if let Some(g) = p.goals.iter_mut().find(|g| g.id == goal_id) {
                    g.active = false;
                    g.progress_notes.push(crate::models::ProgressNote {
                        at: chrono::Utc::now(),
                        text: "user confirmed deactivation (AI proposal)".into(),
                    });
                    true
                } else {
                    false
                }
            });
            if found {
                crate::bot::handlers::memo_goal::persist_goals(&store, &sqlite, user_id).await;
                bot.send_message(
                    msg.chat.id,
                    format!("⏸ Goal #{} deactivated (you confirmed).", goal_id),
                )
                .await?;
            } else {
                bot.send_message(msg.chat.id, format!("❌ Goal #{} not found.", goal_id))
                    .await?;
            }
        }
        // User declined the AI proposal — the goal stays active, and the
        // cooldown is reset so the AI doesn't re-propose it next scan.
        d if d.starts_with("goal:keep:") => {
            let goal_id: u64 = d
                .strip_prefix("goal:keep:")
                .unwrap_or("0")
                .parse()
                .unwrap_or(0);
            let found = store.write(user_id, |p| {
                if let Some(g) = p.goals.iter_mut().find(|g| g.id == goal_id) {
                    g.progress_notes.push(crate::models::ProgressNote {
                        at: chrono::Utc::now(),
                        text: "user chose to KEEP this goal active (AI proposal declined)".into(),
                    });
                    g.last_alert_at = Some(chrono::Utc::now());
                    g.last_declined_at = Some(chrono::Utc::now());
                    true
                } else {
                    false
                }
            });
            if found {
                crate::bot::handlers::memo_goal::persist_goals(&store, &sqlite, user_id).await;
                bot.send_message(msg.chat.id, format!("🔒 Goal #{} stays active.", goal_id))
                    .await?;
            } else {
                bot.send_message(msg.chat.id, format!("❌ Goal #{} not found.", goal_id))
                    .await?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn handle_callback(
    bot: Bot,
    q: CallbackQuery,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    blockchain: Arc<BlockchainClient>,
    acl: Arc<AccessControl>,
    config: Arc<Config>,
    patch_store: PatchStore,
    dialogue: MyDialogue,
) -> anyhow::Result<()> {
    tracing::debug!("handle_callback: user_id={}, data={:?}", q.from.id, q.data);

    // answer_callback_query — best-effort, don't crash if it fails
    let _ = bot.answer_callback_query(&q.id).await;

    let user_id = q.from.id.0;
    let data = match q.data {
        Some(d) => d,
        None => return Ok(()),
    };
    let msg = match q.message {
        Some(m) => m,
        None => return Ok(()),
    };
    let chat_id = msg.chat.id;
    let bot_notify = bot.clone();

    use keyboards::cb;
    let res: anyhow::Result<()> = async {
        match data.as_str() {
            cb::PRICE => handlers::handle_price(bot.clone(), msg.clone(), user_id, store.clone(), cmc.clone(), config.clone(), String::new()).await?,
            cb::TOP10 => handlers::handle_top(bot.clone(), msg.clone(), user_id, cmc.clone(), config.clone(), "10".into()).await?,
            cb::TOP20 => handlers::handle_top(bot.clone(), msg.clone(), user_id, cmc.clone(), config.clone(), "20".into()).await?,
            cb::TOP50 => handlers::handle_top(bot.clone(), msg.clone(), user_id, cmc.clone(), config.clone(), "50".into()).await?,
            cb::PORTFOLIO => handlers::handle_portfolio(bot.clone(), msg.clone(), store.clone(), cmc.clone(), config.clone()).await?,
            cb::AI_PORTFOLIO => handlers::handle_ai_portfolio_callback(bot.clone(), msg.clone(), cmc.clone(), config.clone(), store.clone()).await?,
            cb::PORTFOLIO_CHART => handlers::handle_portfolio_chart(bot.clone(), msg.clone(), store.clone(), cmc.clone(), config.clone()).await?,
            cb::PURCHASE_HISTORY => handlers::handle_purchase_history(bot.clone(), msg.clone(), store.clone(), config.clone()).await?,
            cb::PNL => handlers::handle_pnl_chart(bot.clone(), msg.clone(), store.clone(), cmc.clone(), config.clone()).await?,
            cb::PNL_CHART => handlers::handle_pnl_chart(bot.clone(), msg.clone(), store.clone(), cmc.clone(), config.clone()).await?,
            cb::DISTRIBUTION => handlers::handle_portfolio_chart(bot.clone(), msg.clone(), store.clone(), cmc.clone(), config.clone()).await?,
            cb::TRANSFERS => {
                let user_id = msg.chat.id.0 as u64;
                let list = store.read(user_id, |p| {
                    p.transfers.iter().map(|t| {
                        format!("• {}: {:.4} {} -> wallet #{}", t.date.format("%d.%m"), t.amount_sent, t.symbol, t.to_wallet_id.unwrap_or(0))
                    }).collect::<Vec<_>>().join("\n")
                });
                let reply = if list.is_empty() { "📋 No transfers recorded\\.".to_string() } else { format!("📋 *Transfer History*:\n\n{}", crate::format::esc(&list)) };
                bot.send_message(msg.chat.id, reply).parse_mode(ParseMode::MarkdownV2).await?;
            }
            cb::REFRESH => {
                handlers::handle_portfolio(bot.clone(), msg.clone(), store.clone(), cmc.clone(), config.clone()).await?;
            }
            "top:chart" => handlers::price::handle_top_chart(bot.clone(), msg.clone(), user_id, cmc.clone(), config.clone(), 10).await?,
            cb::ALERTS => handlers::handle_alert_list(bot.clone(), msg.clone(), store.clone(), config.clone()).await?,
            cb::ALERT_ADD => {
                dialogue.update(State::AlertStep1).await.map_err(|e| anyhow::anyhow!(e))?;
                bot.send_message(msg.chat.id,
                    "🔔 *Alert Wizard* \\(Step 1 of 3\\)\n\n\
                     Enter the cryptocurrency symbol \\(e\\.g\\. `BTC`, `ETH`, `SOL`\\):")
                    .parse_mode(ParseMode::MarkdownV2)
                    .reply_markup(keyboards::cancel_only())
                    .await?;
            }
            cb::BACKUP => handlers::handle_backup(bot.clone(), msg.clone(), store.clone(), acl.clone()).await?,
            cb::WALLETS => handlers::handle_wallet_list(bot.clone(), msg.clone(), store.clone(), config.clone()).await?,
            cb::WALLET_ADD => {
                dialogue.update(State::WalletStep1).await.map_err(|e| anyhow::anyhow!(e))?;
                bot.send_message(msg.chat.id,
                    "👛 *Wallet Wizard* \\(Step 1 of 2\\)\n\n\
                     Select the type of wallet you want to add:")
                    .parse_mode(ParseMode::MarkdownV2)
                    .reply_markup(keyboards::wallet_kind_picker())
                    .await?;
            }
            cb::SETTINGS => handlers::handle_settings(bot.clone(), msg.clone(), store.clone(), cmc.clone(), config.clone()).await?,
            cb::CONVERT => {
                bot.send_message(msg.chat.id,
                    "💱 *Currency Converter*\n\n\
                     Please use the `/convert` command with arguments\\.\n\
                     *Format*: `/convert AMOUNT FROM TO`\n\
                     *Example*: `/convert 100 BYN BTC`")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            _ if data.starts_with("alert:del:") => {
                let id = data["alert:del:".len()..].parse().unwrap_or(0);
                handlers::handle_alert_delete(bot.clone(), msg.clone(), store.clone(), config.clone(), id).await?;
            }
            _ if data.starts_with("alert:toggle:") => {
                let id = data["alert:toggle:".len()..].parse().unwrap_or(0);
                handlers::handle_alert_toggle(bot.clone(), msg.clone(), store.clone(), config.clone(), id).await?;
            }
            _ if data.starts_with("wallet:del:") => {
                let id = data["wallet:del:".len()..].parse().unwrap_or(0);
                handlers::handle_wallet_delete(bot.clone(), msg.clone(), store.clone(), config.clone(), id).await?;
            }
            _ if data.starts_with("wallet:refresh:") => {
                let id = data["wallet:refresh:".len()..].parse().ok();
                handlers::handle_wallet_refresh(bot.clone(), msg.clone(), store.clone(), config.clone(), blockchain.clone(), id).await?;
            }
            _ if data.starts_with("alert:cond:") => {
                let cond_str = data.strip_prefix("alert:cond:").unwrap();
                let condition = match cond_str {
                    "below" => AlertCondition::PriceBelow,
                    "above" => AlertCondition::PriceAbove,
                    "drop" => AlertCondition::DropPercent,
                    "rise" => AlertCondition::RisePercent,
                    _ => return Ok(()),
                };
                if let Some(State::AlertStep2 { symbol, .. }) = dialogue.get().await.map_err(|e| anyhow::anyhow!(e))? {
                    if condition == AlertCondition::DropPercent || condition == AlertCondition::RisePercent {
                        dialogue.update(State::AlertStep2 { symbol: symbol.clone(), condition }).await.map_err(|e| anyhow::anyhow!(e))?;
                        bot.send_message(msg.chat.id,
                            "🔔 *Alert Wizard* \\(Step 2 of 3\\)\n\n\
                             Select the comparison time period:")
                            .parse_mode(ParseMode::MarkdownV2)
                            .reply_markup(keyboards::alert_period_picker())
                            .await?;
                    } else {
                        dialogue.update(State::AlertStep3 { symbol: symbol.clone(), condition, threshold: 0.0, period: None }).await.map_err(|e| anyhow::anyhow!(e))?;
                        bot.send_message(msg.chat.id,
                            "🔔 *Alert Wizard* \\(Step 3 of 3\\)\n\n\
                             Enter the threshold price in USD \\(e\\.g\\. `150000`, `2500`\\):")
                            .parse_mode(ParseMode::MarkdownV2)
                            .reply_markup(keyboards::cancel_only())
                            .await?;
                    }
                }
            }
            _ if data.starts_with("alert:period:") => {
                let p_str = data.strip_prefix("alert:period:").unwrap();
                let period = match p_str {
                    "1h" => Some(Period::OneHour),
                    "4h" => Some(Period::FourHours),
                    "24h" => Some(Period::OneDay),
                    "7d" => Some(Period::SevenDays),
                    "30d" => Some(Period::ThirtyDays),
                    "90d" => Some(Period::NinetyDays),
                    _ => None,
                };
                if let Some(State::AlertStep2 { symbol, condition }) = dialogue.get().await.map_err(|e| anyhow::anyhow!(e))? {
                    dialogue.update(State::AlertStep3 { symbol: symbol.clone(), condition, threshold: 0.0, period }).await.map_err(|e| anyhow::anyhow!(e))?;
                    bot.send_message(msg.chat.id,
                        "🔔 *Alert Wizard* \\(Step 3 of 3\\)\n\n\
                         Enter the threshold percentage change \\(e\\.g\\. `5`, `10` for 10%\\):\n\n\
                         _Do not enter the minus or percent signs, just the number\\._")
                        .parse_mode(ParseMode::MarkdownV2)
                        .reply_markup(keyboards::cancel_only())
                        .await?;
                }
            }
            _ if data.starts_with("wallet:kind:") => {
                use crate::models::WalletKind;
                let kind_str = data.strip_prefix("wallet:kind:").unwrap();
                let kind = match kind_str {
                    "cold" => WalletKind::Cold,
                    "hot" => WalletKind::Hot,
                    "exchange" => WalletKind::Exchange,
                    _ => return Ok(()),
                };
                if let Some(State::WalletStep1) = dialogue.get().await.map_err(|e| anyhow::anyhow!(e))? {
                    dialogue.update(State::WalletStep2 { name: String::new(), kind }).await.map_err(|e| anyhow::anyhow!(e))?;
                    bot.send_message(msg.chat.id,
                        "👛 *Wallet Wizard* \\(Step 2 of 2\\)\n\n\
                         Enter the wallet name and optionally blockchain symbol/address separated by spaces\\.\n\n\
                         *Format options*:\n\
                         • `Name` \\(simple non\\-monitored\\)\n\
                         • `Name SYM ADDRESS` \\(auto\\-monitored\\, e\\.g\\. `MyLedger BTC bc1q...`\\)")
                        .parse_mode(ParseMode::MarkdownV2)
                        .reply_markup(keyboards::cancel_only())
                        .await?;
                }
            }
            cb::CANCEL => {
                dialogue.exit().await.map_err(|e| anyhow::anyhow!(e))?;
                bot.send_message(msg.chat.id, "❌ Cancelled")
                    .reply_markup(keyboards::main_menu()).await?;
            }
            "menu" => {
                bot.send_message(msg.chat.id, "🏠 Main menu")
                    .reply_markup(keyboards::main_menu()).await?;
            }
            d if d.starts_with("chart:") => {
                let parts: Vec<&str> = d.split(':').collect();
                if parts.len() == 3 {
                    let symbol = parts[1];
                    let period = parts[2];
                    let has_photo = msg.photo().is_some();
                    match cmc.get_history(symbol, period).await {
                        Ok(points) => {
                            match crate::crypto::charts::ChartRenderer::historical_price_chart(symbol, &points, period).await {
                                Ok(chart_bytes) => {
                                    let input_file = teloxide::types::InputFile::memory(chart_bytes);
                                    if has_photo {
                                        let media = teloxide::types::InputMedia::Photo(teloxide::types::InputMediaPhoto::new(input_file));
                                        bot.edit_message_media(msg.chat.id, msg.id, media)
                                            .reply_markup(keyboards::chart_period_menu(symbol, period))
                                            .await?;
                                    } else {
                                        bot.send_photo(msg.chat.id, input_file)
                                            .reply_markup(keyboards::chart_period_menu(symbol, period))
                                            .await?;
                                    }
                                }
                                Err(e) => {
                                    bot.send_message(msg.chat.id, format!("Error rendering chart: {e}")).await?;
                                }
                            }
                        }
                        Err(e) => {
                            bot.send_message(msg.chat.id, format!("Error fetching history: {e}")).await?;
                        }
                    }
                }
            }
            d if d.starts_with("ai_analysis:") => {
                let parts: Vec<&str> = d.split(':').collect();
                if parts.len() == 3 {
                    let symbol = parts[1].to_string();
                    let period = parts[2].to_string();
                    handlers::handle_ai_callback(bot.clone(), msg.chat.id, symbol, period, cmc.clone(), config.clone(), store.clone()).await?;
                }
            }
            "menu:ai" => {
                dialogue.update(State::AiWizard).await.map_err(|e| anyhow::anyhow!(e))?;
                bot.send_message(msg.chat.id,
                    "🧠 *AI Market Analyzer*\n\n\
                     Select a coin below, or type any coin symbol \\(e\\.g\\. `SOL`, `ADA`\\) directly in the chat:")
                    .parse_mode(ParseMode::MarkdownV2)
                    .reply_markup(keyboards::ai_coin_picker())
                    .await?;
            }
            "menu:news" => {
                bot.send_message(msg.chat.id,
                    "📰 *Crypto News Feed*\n\n\
                     Select a coin below to see recent news, or type `/news SYM` \\(e\\.g\\. `/news ADA`\\) for any other coin:")
                    .parse_mode(ParseMode::MarkdownV2)
                    .reply_markup(keyboards::news_coin_picker())
                    .await?;
            }
            d if d.starts_with("ai_select:") => {
                let symbol = d.strip_prefix("ai_select:").unwrap().to_string();
                dialogue.exit().await.map_err(|e| anyhow::anyhow!(e))?;
                handlers::handle_ai_callback(bot.clone(), msg.chat.id, symbol, "7d".to_string(), cmc.clone(), config.clone(), store.clone()).await?;
            }
            d if d.starts_with("news_select:") => {
                let symbol = d.strip_prefix("news_select:").unwrap().to_string();
                handlers::handle_news_callback(bot.clone(), msg.chat.id, symbol, config.clone()).await?;
            }
            d if d.starts_with("patch:confirm:") => {
                let token = d.strip_prefix("patch:confirm:").unwrap_or("");
                handlers::handle_patch_confirm(bot.clone(), msg.chat.id, user_id, token, store.clone(), patch_store.clone()).await?;
            }
            d if d.starts_with("patch:cancel:") => {
                let token = d.strip_prefix("patch:cancel:").unwrap_or("");
                handlers::handle_patch_cancel(bot.clone(), msg.chat.id, token, patch_store.clone()).await?;
            }
            _ => {}
        }
        Ok(())
    }.await;

    fn is_ignorable_telegram_error(err: &anyhow::Error) -> bool {
        let err_str = err.to_string();
        err_str.contains("message is not modified")
            || err_str.contains("canceled by new editMessageMedia request")
    }

    match &res {
        Ok(_) => {}
        Err(e) => {
            if !is_ignorable_telegram_error(e) {
                tracing::error!("Callback handler error (user={}): {:?}", user_id, e);
                let _ = bot_notify
                    .send_message(chat_id, format!("⚠️ An error occurred: {e}"))
                    .await;
            } else {
                tracing::info!(
                    "Ignored harmless Telegram callback error (user={}): {}",
                    user_id,
                    e
                );
            }
        }
    }

    // Always return Ok — never let handler errors kill the dispatcher
    Ok(())
}

async fn handle_settings_cb(
    bot: Bot,
    q: CallbackQuery,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    config: Arc<Config>,
    sqlite: Arc<crate::db::SqliteStore>,
) -> anyhow::Result<()> {
    let _ = bot.answer_callback_query(&q.id).await;
    let data = match q.data {
        Some(d) => d,
        None => return Ok(()),
    };
    let msg = match q.message {
        Some(m) => m,
        None => return Ok(()),
    };
    let user_id = q.from.id.0;

    if data == "settings:watchlist" {
        bot.send_message(
            msg.chat.id,
            "Use `/watchlist add BTC` or `/watchlist remove BTC` to edit watchlist\\.",
        )
        .parse_mode(ParseMode::MarkdownV2)
        .await?;
    } else {
        handlers::settings::handle_setting_toggle(
            bot, msg, user_id, store, cmc, config, sqlite, &data,
        )
        .await?;
    }
    Ok(())
}

// ── Document handler ──────────────────────────────────────────────────────────

async fn handle_document(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    acl: Arc<AccessControl>,
) -> anyhow::Result<()> {
    handlers::handle_restore_document(bot, msg, store, acl).await
}

// ── Free-text NLP handler ─────────────────────────────────────────────────────

async fn handle_text(
    bot: Bot,
    msg: Message,
    store: Arc<Store>,
    cmc: Arc<CryptoClient>,
    acl: Arc<AccessControl>,
    config: Arc<Config>,
    sqlite: Arc<crate::db::SqliteStore>,
) -> anyhow::Result<()> {
    // Check for message replies or forwards for /restore command
    let text = msg.text().unwrap_or("");
    let is_restore_cmd = text.trim() == "/restore";

    // 1. Check if the message is a reply containing a document, or the user sent /restore as a reply to a document
    if let Some(reply) = msg.reply_to_message() {
        if let Some(doc) = reply.document() {
            if is_restore_cmd
                || reply.caption().unwrap_or("").contains("backup")
                || doc
                    .file_name
                    .as_ref()
                    .map(|f| f.ends_with(".json") || f.ends_with(".cgrp"))
                    .unwrap_or(false)
            {
                let file = bot.get_file(&doc.file.id).await?;
                let mut buf = Vec::new();
                bot.download_file(&file.path, &mut buf).await?;
                let user_id = msg.chat.id.0 as u64;
                match store.decode_backup(&buf) {
                    Ok(portfolio) => {
                        let info = format!(
                            "📦 *Backup loaded from reply document:*\n\
                             {} purchases  \\|  {} wallets\n\
                             {} alerts  \\|  {} transfers\n\n\
                             ✅ Portfolio restored\\.",
                            portfolio.purchases.len(),
                            portfolio.wallets.len(),
                            portfolio.alerts.len(),
                            portfolio.transfers.len(),
                        );
                        store.restore(user_id, portfolio);
                        bot.send_message(msg.chat.id, info)
                            .parse_mode(ParseMode::MarkdownV2)
                            .await?;
                        handlers::backup::auto_backup_plain(&bot, &store, user_id).await;
                        return Ok(());
                    }
                    Err(e) => {
                        bot.send_message(
                            msg.chat.id,
                            format!("❌ Invalid backup in replied file: {e}"),
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }
        }
    }

    // Determine target text content: use text of the replied message if /restore is sent as a reply
    let target_text = if is_restore_cmd {
        if let Some(reply) = msg.reply_to_message() {
            reply.text().unwrap_or(text)
        } else {
            text
        }
    } else {
        text
    };

    if target_text.starts_with("backup")
        || target_text.contains("```")
        || (target_text.trim().starts_with('{') && target_text.trim().ends_with('}'))
    {
        let json_content = if let Some(start_idx) = target_text.find("```") {
            let rest = &target_text[start_idx + 3..];
            if let Some(end_idx) = rest.find("```") {
                rest[..end_idx].trim().to_string()
            } else {
                target_text.to_string()
            }
        } else {
            target_text.trim().to_string()
        };

        if !json_content.is_empty() {
            let user_id = msg.chat.id.0 as u64;
            match store.decode_backup(json_content.as_bytes()) {
                Ok(portfolio) => {
                    let info = format!(
                        "📦 *Text Backup loaded:*\n\
                         {} purchases  \\|  {} wallets\n\
                         {} alerts  \\|  {} transfers\n\n\
                         ✅ Portfolio restored\\.",
                        portfolio.purchases.len(),
                        portfolio.wallets.len(),
                        portfolio.alerts.len(),
                        portfolio.transfers.len(),
                    );
                    store.restore(user_id, portfolio);
                    bot.send_message(msg.chat.id, info)
                        .parse_mode(ParseMode::MarkdownV2)
                        .await?;
                    handlers::backup::auto_backup_plain(&bot, &store, user_id).await;
                    return Ok(());
                }
                Err(e) => {
                    if target_text.starts_with("backup")
                        || target_text.contains("```")
                        || is_restore_cmd
                    {
                        bot.send_message(msg.chat.id, format!("❌ Invalid text backup: {e}"))
                            .await?;
                        return Ok(());
                    }
                }
            }
        }
    }

    let user_id = msg.from().map(|u| u.id.0).unwrap_or(msg.chat.id.0 as u64);
    match parse(text) {
        Some(NlpCommand::AddPurchase {
            symbol,
            amount,
            cost_usd,
            wallet,
        }) => {
            let args = format!(
                "{symbol} {amount} {cost_usd} {}",
                wallet.unwrap_or_default()
            );
            handlers::handle_add(bot, msg, store, cmc, config, args).await?;
        }
        Some(NlpCommand::Price(syms)) => {
            let args = syms.join(" ");
            handlers::handle_price(bot, msg, user_id, store, cmc, config, args).await?;
        }
        Some(NlpCommand::Move {
            symbol,
            amount,
            from,
            to,
            fee,
        }) => {
            let fee_part = fee.map(|f| format!(" fee:{f}")).unwrap_or_default();
            let args = format!("{symbol} {amount} from:{from} to:{to}{fee_part}");
            handlers::handle_move(bot, msg, store, config, args).await?;
        }
        Some(NlpCommand::AlertAdd(args)) => {
            handlers::handle_alert_add_cmd(bot, msg, store, config, cmc, args).await?;
        }
        Some(NlpCommand::Convert { amount, from, to }) => {
            handlers::handle_convert(bot, msg, cmc, config, format!("{amount} {from} {to}"))
                .await?;
        }
        Some(NlpCommand::Backup) => {
            handlers::handle_backup(bot, msg, store, acl).await?;
        }
        Some(NlpCommand::Portfolio) => {
            handlers::handle_portfolio(bot, msg, store, cmc, config).await?;
        }
        Some(NlpCommand::Top(n)) => {
            handlers::handle_top(bot, msg, user_id, cmc, config, n.to_string()).await?;
        }
        Some(NlpCommand::Help) => {
            bot.send_message(msg.chat.id, Command::descriptions().to_string())
                .await?;
        }
        None => {
            if text.trim().starts_with('/') {
                bot.send_message(msg.chat.id, "❌ Unknown command or parsing error. Make sure there are no typos (e.g. accidental Cyrillic letters). If you're stuck in a dialog, press /cancel.")
                    .await?;
            } else if let Some(reply) = msg.reply_to_message() {
                // Plain-text reply to a bot message → treat it as a follow-up
                // /ask question. The replied message becomes the dialogue
                // context, so "и че когда откат" on top of the bot's analysis
                // continues the conversation instead of being silently ignored.
                let replied_by_bot = reply.from().map(|u| u.is_bot).unwrap_or(false);
                if replied_by_bot && reply.text().is_some() {
                    return handlers::handle_ask_cmd(
                        bot,
                        msg.clone(),
                        store,
                        cmc,
                        config,
                        sqlite,
                        text.to_string(),
                    )
                    .await;
                }
            }
        }
    }
    Ok(())
}
