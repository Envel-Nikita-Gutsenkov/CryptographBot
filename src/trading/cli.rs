//! Headless CLI for the trading module — 1:1 with the Telegram `/trade`
//! commands (plan §6). Every subcommand talks to the SAME store code the bot
//! uses; nothing here is a mock.
//!
//! Command tree:
//!   trade mode <live|sim>            — print which engine is active
//!   trade allocate <SYM> <USD>       — create a capital pool
//!   trade rule <SYM> <TYPE> <VALUE>  — add an operator constraint
//!   trade tick <SYM> <PRICE>         — run one engine cycle (plan only)
//!   trade buy <SYM> <PRICE>          — force plan+execute a buy
//!   trade sell <LOT_ID> <PRICE>      — force plan+execute a sell
//!   trade status [--asset SYM]       — counts + open lots + free cash
//!   trade export [--format csv|json] — dump ledger to stdout
//!   trade backup <dest.db>           — hot VACUUM INTO backup
//!   trade backtest ...               — replay real history (plan §7)
//!   trade lessons [--limit N]        — strategy lessons the AI learned
//!   trade backtests [--limit N]      — recorded backtest runs
//!   trade tune [--dry-run]           — AI self-tuning of EngineConfig (plan §7.4)
//!   trade config [--set F V]         — show config history / manual override
//!   trade pause <alloc-id> | resume <alloc-id>
//!   trade help                       — usage

use anyhow::{bail, Result};
use chrono::NaiveDate;
use std::io::Write;

use super::advisor::{
    decide, grid_summary, report, simulate_grid, simulate_task, BuySignal, TaskGridConfig,
    TaskSimConfig,
};
use super::backtest::{BacktestConfig, BatchConfig, DEFAULT_CRASH_WINDOW_DAILY_SECS};
use super::engine::{
    EngineConfig, MarketTick, ProfitMode, TickOutcome, TradingEngine, TrendFilter,
};
use super::nlp::{self, ParsedTradeReport};
use super::store::{
    AllocationStatus, LotStatus, NewAllocation, NewConstraint, NewRecommendation, NewTradeTask,
    RecommendationStatus, RuleType, Side, TaskExecution, TaskStatus, TradingMode, TradingStore,
};
use super::tuner::check_tuner_bounds;

const USAGE: &str = r#"trade — trading module CLI (1:1 with Telegram /trade)

USAGE:
  cargo run -- trade <subcommand> [args]

SUBCOMMANDS:
  mode <live|sim>                     Show the active engine (default: sim)
  allocate <SYMBOL> <USD>             Create a capital pool for SYMBOL
  deallocate <ID>                     Delete a capital pool
  rule <SYMBOL> <TYPE> <VALUE>        Add an operator constraint
                                      TYPE: ceiling|floor|drawdown|pause-buy|pause-sell
  rule del <ID>                       Delete an operator constraint
  tick <SYMBOL> <PRICE>               Run one engine cycle (plans only)
                                      [--bid B] [--ask A] [--execute]
  buy <SYMBOL> <PRICE>                Force plan + execute a buy
  sell <LOT_ID> <PRICE>               Force plan + execute a sell
  status [--asset SYMBOL]             Show counts, open lots, free cash
  export [--format csv|json]          Dump the full ledger to stdout
  backup <dest.db>                    Hot backup of the active DB (VACUUM INTO)
  seed [--dest path]                  Refresh the bundled seed DB used to
                                      pre-load trained profiles + lessons on
                                      first run (default: data/trading_sim_seed.db)
  backtest --symbol SYM --from DATE --to DATE [--capital USD]
                                      Replay real history through the engine
                                      [--ceiling P] [--floor P] [--margin M]
                                      [--fee F] [--tranches N] [--crash-window S]
                                      [--max-runup P] [--trend-filter OFF|SMA50|GOLDEN_CROSS]
                                      [--trend-exit] [--take-profit P] [--trail P]
                                      [--profit-mode REINVEST|SEPARATE|PARTIAL:0.5]
                                      [--walk-forward]
  backtest --batch --symbols A,B,C --from DATE --to DATE
                                      Rolling-window sweep (hundreds of runs)
                                      [--window-days N] [--step-days N]
                                      [--max-runup P] [--trend-filter OFF|SMA50|GOLDEN_CROSS]
                                      [--trend-exit] [--take-profit P] [--trail P]
                                      [--profit-mode REINVEST|SEPARATE|PARTIAL:0.5]
  lessons [--limit N]                 Show strategy lessons the AI learned
  backtests [--limit N]               Show recorded backtest runs
  tune [--dry-run] [--model ALIAS]    AI self-tuning: model proposes EngineConfig
                                      changes from backtests + lessons; the
                                      deterministic gate rejects garbage
                                      [--limit N]
  config [--set FIELD VALUE]          Show current config + tuning history, or
                                      apply a manual override (validated)
                                      FIELD: min_profit_margin|taker_fee_rate|
                                             tranche_count|min_tranche_fraction|
                                             max_spread_pct|crash_drop_pct|
                                             crash_window_secs|ath_lookback_days|
                                             max_buy_from_high_pct|max_runup_pct|
                                             trend_filter|trend_exit|take_profit_pct|profit_mode
  pause <alloc-id>                    Pause an allocation
  resume <alloc-id>                   Resume an allocation
  recommend buy <SYMBOL> <PRICE>      Plan a buy and record it as PENDING advice
                                      [--mode live|sim]
  recommend sell <LOT_ID> <PRICE>     Plan a sell (no-loss checked) and record it
                                      [--mode live|sim]
  recommend list [--limit N]          Show recommendations
                                      [--status PENDING|EXECUTED|REJECTED|EXPIRED]
                                      [--mode live|sim]
  recommend execute <REC_ID> <PRICE>  Operator executed the trade at PRICE; the bot
                                      re-checks every guardrail at PRICE, mirrors
                                      the fill in its ledger, and records outcome
                                      [--note "..."] [--mode live|sim]
  recommend reject <REC_ID>           Operator declined the advice
                                      [--note "..."] [--mode live|sim]
  recommend expire <REC_ID>           Mark stale PENDING advice as expired
                                      [--mode live|sim]
  recommend parse "<free text>"       Understand plain-language trade reports:
                                      "купил ETH за 1800", "sold 0.5 SOL at 75",
                                      "продал LINK, получилось 10.2". Matches a
                                      PENDING advice for the same symbol or
                                      records the operator's manual trade with
                                      full guardrail re-checking
                                      [--mode live|sim]
  strategies                          List the strategy catalog (builtin + trained)
  strategy apply <NAME>               Apply a strategy profile (sets engine config
                                      + default constraints)
                                      [--mode live|sim]
  strategy train                      Sweep candidate configs over historical windows
                                      and persist the best as trained profiles
                                      [--symbols A,B,C] [--from DATE] [--to DATE]
                                      [--window-days N] [--step-days N]
                                      [--capital USD] [--top N] [--grid-full]
                                      [--grid-conviction]
                                      [--threads N] [--force] [--mode live|sim]
  strategy delete <NAME>              Delete a stored (trained/manual) profile
  strategy regimes                     Per-regime winners from the last training
                                      sweep (the configs the bot auto-selects)
  regime <SYMBOL>                     Which market regime is SYMBOL in right now,
                                      and which config the bot auto-selected
                                      [--mode live|sim]
  fee [--buy P] [--sell P]            Show exchange fees for the active mode, or
                                      set them (percentages, e.g. --buy 5 --sell 2.5)
  dex quote <SYM> <AMT> [--side buy|sell]
                                      DEX quote (Jupiter/0x) at the actual
                                      fill price before signing
                                      [--chain solana|ethereum]
  dex sign <SYM> <AMT> [--side buy|sell]
                                      Manual sign mode: render the swap payload
                                      for the operator's wallet UI
  dex reconcile <SYM> <AMT> <PRICE>   Reconcile a fill from the executed price
                                      [--side buy|sell] [--max-slippage P]
  task add --name NAME --assets A,B,C Create a standing trade task (advisor mode)
                                      [--budget USD] [--alerts N] [--tranches N]
                                      [--margin M] [--execution manual|auto]
                                      [--mode live|sim]
  task list                           List trade tasks (month spend, alerts today)
  task pause <id> | resume <id>       Pause / resume a trade task
  task advise --symbol SYM --price P  Run the advisor for a task against the
                                      latest ~30 days of real history
                                      [--task ID] [--signal any|dip|rsi|sma]
                                      [--mode live|sim]
  task sim --from DATE --to DATE      Replay one task config over real history
                                      [--symbols A,B,C] [--budget USD] [--alerts N]
                                      [--tranches N] [--margin M] [--buy-fee P]
                                      [--sell-fee P] [--signal any|dip|rsi|sma]
                                      [--ceiling P]
  task grid --from DATE --to DATE     Sweep the parameter grid over real history
                                      [--symbols A,B,C] [--budget USD] [--alerts N]
                                      [--ceiling P]
  experiment run                      Livepatch sweep: one config (or inline
                                      overrides) over windows x capitals with a
                                      buy-and-hold benchmark, persisted so the
                                      next run can be compared
                                      [--label NAME] [--symbols A,B,C]
                                      [--from DATE] [--to DATE]
                                      [--window-days N] [--step-days N]
                                      [--capitals 20,50,200] [--config NAME]
                                      [--threads N]
                                      Inline overrides (same fields as `config`):
                                      [--margin M] [--fee F] [--tranches N]
                                      [--crash-drop P] [--crash-window S]
                                      [--ath-days N] [--max-buy-from-high P]
                                      [--max-runup P] [--trend-filter OFF|SMA50|GOLDEN_CROSS]
                                      [--trend-exit] [--take-profit P] [--trail P]
                                      [--profit-mode REINVEST|SEPARATE|PARTIAL:0.5]
  experiment list [--limit N]         Recent experiments, newest first, with
                                      score delta vs the previous run
  experiment clear                    Drop all experiment rows
  faketrade <BUDGET> auto             Virtual trading: replay a monthly-budget
                                      strategy over real history without ever
                                      bothering the user (no alerts). Results
                                      land in the fake account + log
                                      [--symbols A,B,C] [--from DATE] [--to DATE]
  faketrade clear | stop              Reset / stop the fake account
  fakestats [--limit N]               Fake account: spent / earned / fees /
                                      balances per symbol + recent virtual trades
  pnl                                 Aggregate PnL: realized (executions) +
                                      mark-to-market of open/cold lots at the
                                      latest price; sell-equivalent + net %
                                      [--price BTC=81000] (override closes)
  debate <SYMBOL>                      AI sell-vs-hold debate (bull/bear/arbiter)
                                      for an open position; safe HOLD fallback
                                      [--price P] [--model ALIAS]
  analyze <SYMBOL>                     Deterministic market-state snapshot: trend,
                                      RSI, ATH distance, support/resistance, vol,
                                      halving phase, seasonality + what the engine
                                      and the top-3 trained ensemble would do
                                      [--price P] [--mode live|sim]
  regime <SYMBOL>                      Market regime + auto-selected config
                                      [--mode live|sim]
  help                                This help

EXAMPLES:
  cargo run -- trade mode sim
  cargo run -- trade allocate SOL 50
  cargo run -- trade rule BTC ceiling 65000
  cargo run -- trade tick SOL 75 --execute
  cargo run -- trade status
  cargo run -- trade backup trading_sim_backup.db
  cargo run -- trade backtest --symbol BTC --from 2020-01-01 --to 2026-08-01 --capital 1000
  cargo run -- trade backtest --batch --symbols BTC,ETH,SOL,LINK --from 2020-01-01 --to 2026-08-01 --window-days 90 --step-days 30
  cargo run -- trade lessons --limit 10
  cargo run -- trade tune --dry-run
  cargo run -- trade config
  cargo run -- trade config --set min_profit_margin 0.02
  cargo run -- trade config --set profit_mode SEPARATE
  cargo run -- trade strategies
  cargo run -- trade strategy apply Win-Win-Accumulator
  cargo run -- trade strategy train --symbols BTC,ETH,SOL,LINK --from 2020-01-01 --to 2026-08-01
  cargo run -- trade fee --buy 5 --sell 2.5
  cargo run -- trade task add --name "Monthly DCA" --assets BTC,ETH,SOL,LINK --budget 200 --alerts 1
  cargo run -- trade task advise --symbol ETH --price 1800
  cargo run -- trade task sim --from 2020-01-01 --to 2026-09-05 --buy-fee 5 --sell-fee 2.5
  cargo run -- trade task grid --from 2020-01-01 --to 2026-09-05 --budget 200 --alerts 1
  cargo run -- trade experiment run --label tp30 --symbols BTC,ETH,SOL,LINK --from 2019-01-01 --to 2026-09-05 --capitals 20,50,200 --config Trained-1-m0.050-t2-REINVEST-c10 --take-profit 0.30
  cargo run -- trade experiment list --limit 5
  cargo run -- trade recommend buy ETH 1800
  cargo run -- trade recommend sell <LOT_ID> 1900
  cargo run -- trade recommend list --status PENDING
  cargo run -- trade recommend execute <REC_ID> 1795 --note "filled on DEX"
  cargo run -- trade recommend parse "ок я купил ETH за 1800"
  cargo run -- trade recommend parse "продал SOL, получилось 85.5"
"#;

/// Parse `--mode` / `-m` from args; default to simulation (safe).
fn mode_from_args(args: &[String]) -> Result<TradingMode> {
    for (i, a) in args.iter().enumerate() {
        if a == "--mode" || a == "-m" {
            if let Some(v) = args.get(i + 1) {
                if let Some(m) = TradingMode::parse(v) {
                    return Ok(m);
                }
                bail!("unknown mode '{v}' (expected live or sim)");
            }
        }
    }
    Ok(TradingMode::Simulation)
}

fn flag_str<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn flag_f64(args: &[String], name: &str) -> Option<f64> {
    flag_str(args, name).and_then(|s| s.parse().ok())
}

fn flag_i64(args: &[String], name: &str) -> Option<i64> {
    flag_str(args, name).and_then(|s| s.parse().ok())
}

fn flag_u32(args: &[String], name: &str) -> Option<u32> {
    flag_str(args, name).and_then(|s| s.parse().ok())
}

/// Parse a boolean flag: `--flag` alone means true, `--flag true|false|on|off`
/// also works. Missing flag = false.
fn flag_bool(args: &[String], name: &str) -> bool {
    match flag_str(args, name) {
        None => args.iter().any(|a| a == name),
        Some(v) => matches!(v.to_lowercase().as_str(), "true" | "1" | "on" | "yes"),
    }
}

/// Parse `--profit-mode` (defaults to REINVEST — the classic compounding
/// behavior; invalid values fall back silently to the default).
fn flag_profit_mode(args: &[String]) -> ProfitMode {
    flag_str(args, "--profit-mode")
        .and_then(ProfitMode::parse)
        .unwrap_or(ProfitMode::Reinvest)
}

fn parse_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|e| anyhow::anyhow!("invalid date '{s}' (expected YYYY-MM-DD): {e}"))
}

/// Cached closes for regime classification: ~400 days (SMA200 + RSI14 need
/// the long lookback). Reads `data/history/{SYMBOL}.csv` first — offline and
/// fast; an empty result is fine (the auto-selector falls back to runner).
fn regime_history(symbol: &str) -> Vec<(i64, f64)> {
    let today = chrono::Local::now().date_naive();
    super::backtest::fetch_history_blocking(symbol, today - chrono::Duration::days(400), today)
        .unwrap_or_default()
}

/// The engine config the bot should trade with right now: an explicit user
/// choice (manual override / applied strategy) wins, otherwise the best
/// trained profile for the current market regime of `symbol`.
fn active_config(store: &TradingStore, symbol: &str) -> Result<EngineConfig> {
    super::strategies::auto_engine_config(store, symbol, &regime_history(symbol))
}

pub fn run(args: &[String]) -> Result<()> {
    if args.is_empty() || args[0] == "help" || args[0] == "--help" || args[0] == "-h" {
        print!("{USAGE}");
        return Ok(());
    }

    let cmd = args[0].as_str();
    let rest = &args[1..];

    match cmd {
        "mode" => {
            let mode = mode_from_args(rest)?;
            println!("Active trading engine: {}", mode.as_str());
            println!("DB file: {}", mode.db_file());
            Ok(())
        }
        "allocate" => {
            if rest.len() < 2 {
                bail!("usage: trade allocate <SYMBOL> <USD>");
            }
            let symbol = rest[0].to_uppercase();
            let usd: f64 = rest[1]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid USD amount '{}'", rest[1]))?;
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let a = store.create_allocation(NewAllocation {
                asset: symbol.clone(),
                total_budget_usd: usd,
            })?;
            println!(
                "Allocation created [{}]: {} pool of {:.2} USD (id: {})",
                mode.as_str(),
                symbol,
                usd,
                a.id
            );
            Ok(())
        }
        "deallocate" => {
            if rest.is_empty() {
                bail!("usage: trade deallocate <ID>");
            }
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            store.delete_allocation(&rest[0])?;
            println!("Allocation {} deleted [{}].", rest[0], mode.as_str());
            Ok(())
        }
        "rule" => {
            if rest.get(0).map(|s| s.eq_ignore_ascii_case("del")) == Some(true) {
                if rest.len() < 2 {
                    bail!("usage: trade rule del <ID>");
                }
                let mode = mode_from_args(rest)?;
                let store = TradingStore::open(mode)?;
                store.delete_constraint(&rest[1])?;
                println!("Constraint {} deleted [{}].", rest[1], mode.as_str());
                return Ok(());
            }
            if rest.len() < 3 {
                bail!("usage: trade rule <SYMBOL> <TYPE> <VALUE>");
            }
            let symbol = rest[0].to_uppercase();
            let rule_type = parse_rule_type(&rest[1])?;
            let value: f64 = rest[2]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid threshold '{}'", rest[2]))?;
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let c = store.add_constraint(NewConstraint {
                asset: symbol.clone(),
                rule_type,
                threshold_value: value,
                raw_prompt: format!("CLI: {} {} {}", symbol, rest[1], value),
                expires_at: None,
            })?;
            println!(
                "Constraint added [{}]: {} {} {} (id: {})",
                mode.as_str(),
                symbol,
                rule_type.as_str(),
                value,
                c.id
            );
            Ok(())
        }
        "constraint" => {
            if rest.len() < 2 || rest[0] != "parse" {
                bail!("usage: trade constraint parse \"<free text>\"");
            }
            let text = rest[1..].join(" ");
            let parsed = nlp::parse_constraint(&text)
                .ok_or_else(|| anyhow::anyhow!("could not parse a constraint from: {text}"))?;
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let c = store.add_constraint(NewConstraint {
                asset: parsed.symbol.clone(),
                rule_type: parsed.rule_type,
                threshold_value: parsed.threshold_value,
                raw_prompt: parsed.note.clone(),
                expires_at: None,
            })?;
            println!(
                "Constraint added [{}]: {} {} {} (id: {})\n  raw: {}",
                mode.as_str(),
                parsed.symbol,
                parsed.rule_type.as_str(),
                parsed.threshold_value,
                c.id,
                parsed.note
            );
            Ok(())
        }
        "tick" => {
            if rest.len() < 2 {
                bail!("usage: trade tick <SYMBOL> <PRICE> [--bid B] [--ask A] [--execute]");
            }
            let symbol = rest[0].to_uppercase();
            let price: f64 = rest[1]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid price '{}'", rest[1]))?;
            let bid = flag_value(rest, "--bid").unwrap_or(price);
            let ask = flag_value(rest, "--ask").unwrap_or(price);
            let execute = rest.iter().any(|a| a == "--execute");
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let config = active_config(&store, &symbol)?;
            let mut engine = TradingEngine::new(config)?;
            let tick = MarketTick::new(&symbol, price, bid, ask, chrono::Utc::now().timestamp());
            let outcome = if execute {
                engine.apply_tick(&store, &tick)?
            } else {
                engine.on_tick(&store, &tick)?
            };
            print_outcome(&outcome, execute);
            Ok(())
        }
        "buy" => {
            if rest.len() < 2 {
                bail!("usage: trade buy <SYMBOL> <PRICE> [--mode live|sim]");
            }
            let symbol = rest[0].to_uppercase();
            let price: f64 = rest[1]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid price '{}'", rest[1]))?;
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let config = active_config(&store, &symbol)?;
            let engine = TradingEngine::new(config)?;
            match engine.plan_buy(&store, &symbol, price, price, price)? {
                super::engine::Decision::Proceed(plan) => {
                    let lot = engine.execute_buy(&store, &plan)?;
                    println!(
                        "BOUGHT {} {} @ {:.4} (cost {:.2}, target exit {:.4}, lot {})",
                        plan.symbol,
                        plan.amount,
                        plan.entry_price,
                        plan.cost_basis_usd,
                        plan.target_exit_price,
                        lot.id
                    );
                }
                super::engine::Decision::Skip(reason) => {
                    println!("SKIP: {reason}");
                }
            }
            Ok(())
        }
        "sell" => {
            if rest.len() < 2 {
                bail!("usage: trade sell <LOT_ID> <PRICE> [--mode live|sim]");
            }
            let lot_id = rest[0].clone();
            let price: f64 = rest[1]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid price '{}'", rest[1]))?;
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let symbol = store
                .get_lot(&lot_id)?
                .ok_or_else(|| anyhow::anyhow!("lot {lot_id} not found"))?
                .pair;
            let config = active_config(&store, &symbol)?;
            let engine = TradingEngine::new(config)?;
            match engine.plan_sell(&store, &lot_id, price)? {
                super::engine::Decision::Proceed(plan) => {
                    let ex = engine.execute_sell(&store, &plan)?;
                    println!(
                        "SOLD lot {} @ {:.4} net {:.2} (profit {:.2}, fee {:.2})",
                        plan.lot_id, plan.exit_price, ex.net_usd, plan.profit_usd, plan.fee_usd
                    );
                }
                super::engine::Decision::Skip(reason) => {
                    println!("SKIP: {reason}");
                }
            }
            Ok(())
        }
        "status" => {
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let counts = store.counts()?;
            println!(
                "=== Trading status [{}] ({}) ===",
                mode.as_str(),
                mode.db_file()
            );
            println!(
                "Constraints: {} | Allocations: {} | Debates: {} | Executions: {}",
                counts.constraints, counts.allocations, counts.debates, counts.executions
            );
            println!(
                "Lots — open: {} | cold-hold: {} | closed: {}",
                counts.open_lots, counts.cold_lots, counts.closed_lots
            );
            println!(
                "Backtests: {} | Lessons: {} | Config history: {}",
                counts.backtests, counts.lessons, counts.engine_configs
            );
            println!(
                "Recommendations: {} (pending/executed/rejected/expired)",
                counts.recommendations
            );

            // Active engine config (latest persisted, else defaults).
            let config = store
                .latest_engine_config()?
                .map(|r| r.to_engine_config())
                .unwrap_or_else(EngineConfig::runner);
            println!("\nEngine config (active):");
            println!(
                "  margin {:.4} | fee {:.4} | tranches {} | min-tranche {:.4} | spread {:.2}% | crash {:.1}% / {}s | profit {}",
                config.min_profit_margin,
                config.taker_fee_rate,
                config.tranche_count,
                config.min_tranche_fraction,
                config.max_spread_pct,
                config.crash_drop_pct,
                config.crash_window_secs,
                config.profit_mode.as_str()
            );
            // Auto-adaptation state: with regime profiles the bot switches
            // config per market regime unless the operator applied one.
            let regime_rows = store.list_regime_profiles("")?;
            let explicit = store
                .latest_engine_config()?
                .map(|r| r.source == "manual" || r.source.starts_with("strategy:"))
                .unwrap_or(false);
            if explicit {
                println!("  auto-adapt: OFF (explicit config applied — `trade strategy apply` or `trade config --set` wins)");
            } else if regime_rows.is_empty() {
                println!("  auto-adapt: OFF (no regime profiles — run `trade strategy train --force` to enable)");
            } else {
                println!("  auto-adapt: ON ({} regime profiles — bot picks the best config for the current regime)", regime_rows.len());
            }

            let asset_filter = rest
                .iter()
                .position(|a| a == "--asset")
                .and_then(|i| rest.get(i + 1));
            let constraints = store.list_constraints(asset_filter.map(|s| s.as_str()), true)?;
            if !constraints.is_empty() {
                println!("\nActive constraints:");
                for c in constraints {
                    println!(
                        "  [{}] {} {} {}",
                        c.id,
                        c.asset,
                        c.rule_type.as_str(),
                        c.threshold_value
                    );
                }
            }

            let allocations = store.list_allocations(None)?;
            if !allocations.is_empty() {
                println!("\nAllocations:");
                for a in allocations {
                    println!(
                        "  [{}] {} — total {:.2} | deployed {:.2} | available {:.2} | reserve {:.2} | profit {:.2} | {}",
                        a.id,
                        a.asset,
                        a.total_budget_usd,
                        a.deployed_usd,
                        a.available_usd,
                        a.profit_reserve_usd,
                        a.total_profit_usd,
                        a.status.as_str()
                    );
                }
            }

            let open = store.list_lots(Some(super::store::LotStatus::Open))?;
            let cold = store.list_lots(Some(super::store::LotStatus::ColdHold))?;
            if !open.is_empty() || !cold.is_empty() {
                println!("\nActive lots:");
                for lot in open.iter().chain(cold.iter()) {
                    println!(
                        "  [{}] {} entry {:.4} amount {:.6} cost {:.2} target {:.4} ({})",
                        lot.id,
                        lot.pair,
                        lot.entry_price,
                        lot.amount,
                        lot.cost_basis_usd,
                        lot.target_exit_price,
                        lot.status.as_str()
                    );
                }
            }

            // Recent backtests, lessons and tuning history (the "memory" the
            // model learns from).
            let backtests = store.list_backtests(5)?;
            if !backtests.is_empty() {
                println!("\nRecent backtests:");
                for b in backtests {
                    println!(
                        "  [{}] {} {}..{} trades={} pnl={:+.2}% dd={:.2}%",
                        b.id,
                        b.symbol,
                        b.start_date,
                        b.end_date,
                        b.total_trades,
                        b.pnl_pct,
                        b.max_drawdown_pct
                    );
                }
            }
            let lessons = store.list_lessons(5)?;
            if !lessons.is_empty() {
                println!("\nRecent lessons:");
                for l in lessons {
                    println!("  [{}] {}: {}", l.id, l.symbol, l.lesson);
                }
            }
            let configs = store.list_engine_configs(5)?;
            if !configs.is_empty() {
                println!("\nConfig history:");
                for c in configs {
                    println!(
                        "  [{}] {} margin {:.4} fee {:.4} tranches {} ({}: {})",
                        c.id,
                        c.created_at,
                        c.min_profit_margin,
                        c.taker_fee_rate,
                        c.tranche_count,
                        c.source,
                        c.rationale
                    );
                }
            }
            Ok(())
        }
        "export" => {
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let fmt = rest
                .iter()
                .position(|a| a == "--format")
                .and_then(|i| rest.get(i + 1))
                .map(|s| s.as_str())
                .unwrap_or("json");
            match fmt {
                "json" => export_json(&store),
                "csv" => export_csv(&store),
                other => bail!("unknown format '{other}' (expected csv or json)"),
            }
        }
        "backup" => {
            if rest.is_empty() {
                bail!("usage: trade backup <dest.db>");
            }
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let dest = rest[0].clone();
            store.backup_to(&dest)?;
            println!("Backup written: {dest}");
            Ok(())
        }
        "seed" => {
            // Refresh the bundled seed DB (data/trading_sim_seed.db) from the
            // current sim DB — ships trained profiles + lessons + training
            // stats so a fresh install never starts from a blank slate.
            // Runtime state (lots, executions, backtest runs) is excluded so
            // the bundled file stays small (target 1–5 MB).
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let dest = flag_str(rest, "--dest")
                .map(|s| s.to_string())
                .unwrap_or_else(|| super::store::SEED_DB_FILE.to_string());
            let size = store.export_seed(&dest)?;
            println!(
                "Seed written: {dest} ({:.2} KB — profiles + lessons + training stats only)",
                size as f64 / 1024.0
            );
            Ok(())
        }
        "history" => {
            // trade history status [--symbols A,B,C]
            // trade history refresh [--symbols A,B,C] [--from DATE] [--to DATE]
            // Local OHLCV cache in data/history/{SYMBOL}.csv — backtests,
            // sim sweeps and training read it instead of hitting the network.
            let symbols: Vec<String> = flag_str(rest, "--symbols")
                .unwrap_or("BTC,ETH,SOL,LINK")
                .split(',')
                .map(|s| s.trim().to_uppercase())
                .filter(|s| !s.is_empty())
                .collect();
            match rest.first().map(|s| s.as_str()) {
                Some("refresh") => {
                    let from = parse_date(flag_str(rest, "--from").unwrap_or("2019-01-01"))?;
                    let to = parse_date(flag_str(rest, "--to").unwrap_or("2026-09-05"))?;
                    for sym in &symbols {
                        let h = super::backtest::fetch_history_blocking(sym, from, to)?;
                        println!("{sym}: {} candles cached ({from}..{to})", h.len());
                    }
                }
                _ => {
                    for sym in &symbols {
                        match super::history_file::cached_range(sym) {
                            Some((f, l)) => {
                                let fd = chrono::DateTime::from_timestamp(f / 1000, 0)
                                    .map(|d| d.date_naive().to_string())
                                    .unwrap_or_else(|| "?".into());
                                let ld = chrono::DateTime::from_timestamp(l / 1000, 0)
                                    .map(|d| d.date_naive().to_string())
                                    .unwrap_or_else(|| "?".into());
                                let n = super::history_file::load_history(sym)
                                    .map(|h| h.len())
                                    .unwrap_or(0);
                                println!("{sym}: {n} candles cached ({fd}..{ld})");
                            }
                            None => println!("{sym}: no cache (run `trade history refresh`)"),
                        }
                    }
                }
            }
            Ok(())
        }
        "backtest" => {
            if rest.iter().any(|a| a == "--batch") {
                let symbols: Vec<String> = flag_str(rest, "--symbols")
                    .unwrap_or("BTC,ETH,SOL,LINK")
                    .split(',')
                    .map(|s| s.trim().to_uppercase())
                    .filter(|s| !s.is_empty())
                    .collect();
                let from = parse_date(flag_str(rest, "--from").unwrap_or("2020-01-01"))?;
                let to = parse_date(flag_str(rest, "--to").unwrap_or("2026-08-01"))?;
                let cfg = BatchConfig {
                    symbols,
                    from,
                    to,
                    window_days: flag_i64(rest, "--window-days").unwrap_or(90),
                    step_days: flag_i64(rest, "--step-days").unwrap_or(30),
                    capital_usd: flag_f64(rest, "--capital").unwrap_or(1000.0),
                    crash_window_secs: flag_i64(rest, "--crash-window")
                        .unwrap_or(DEFAULT_CRASH_WINDOW_DAILY_SECS),
                    min_profit_margin: flag_f64(rest, "--margin")
                        .unwrap_or(super::engine::DEFAULT_MIN_PROFIT_MARGIN),
                    taker_fee_rate: flag_f64(rest, "--fee")
                        .unwrap_or(super::engine::DEFAULT_TAKER_FEE_RATE),
                    tranche_count: flag_u32(rest, "--tranches")
                        .unwrap_or(super::engine::DEFAULT_TRANCHE_COUNT),
                    ath_lookback_days: flag_i64(rest, "--ath-lookback")
                        .unwrap_or(super::engine::DEFAULT_ATH_LOOKBACK_DAYS),
                    max_buy_from_high_pct: flag_f64(rest, "--max-buy-from-high")
                        .unwrap_or(super::engine::DEFAULT_MAX_BUY_FROM_HIGH_PCT),
                    min_dip_from_high_pct: flag_f64(rest, "--min-dip").unwrap_or(0.0),
                    max_runup_pct: flag_f64(rest, "--max-runup")
                        .unwrap_or(super::engine::DEFAULT_MAX_RUNUP_PCT),
                    trend_filter: flag_str(rest, "--trend-filter")
                        .and_then(|s| TrendFilter::parse(s))
                        .unwrap_or(TrendFilter::Off),
                    trend_exit: flag_bool(rest, "--trend-exit"),
                    take_profit_pct: flag_f64(rest, "--take-profit")
                        .unwrap_or(super::engine::DEFAULT_TAKE_PROFIT_PCT),
                    trail_pct: flag_f64(rest, "--trail").unwrap_or(0.0),
                    profit_mode: flag_profit_mode(rest),
                };
                let results = super::backtest::run_batch(&cfg)?;
                print!("{}", super::backtest::batch_summary(&results));
                Ok(())
            } else {
                let symbol = flag_str(rest, "--symbol").unwrap_or("BTC").to_uppercase();
                let from = parse_date(flag_str(rest, "--from").unwrap_or("2020-01-01"))?;
                let to = parse_date(flag_str(rest, "--to").unwrap_or("2026-08-01"))?;
                let cfg = BacktestConfig {
                    symbol,
                    from,
                    to,
                    capital_usd: flag_f64(rest, "--capital").unwrap_or(1000.0),
                    crash_window_secs: flag_i64(rest, "--crash-window")
                        .unwrap_or(DEFAULT_CRASH_WINDOW_DAILY_SECS),
                    min_profit_margin: flag_f64(rest, "--margin")
                        .unwrap_or(super::engine::DEFAULT_MIN_PROFIT_MARGIN),
                    taker_fee_rate: flag_f64(rest, "--fee")
                        .unwrap_or(super::engine::DEFAULT_TAKER_FEE_RATE),
                    tranche_count: flag_u32(rest, "--tranches")
                        .unwrap_or(super::engine::DEFAULT_TRANCHE_COUNT),
                    ath_lookback_days: flag_i64(rest, "--ath-lookback")
                        .unwrap_or(super::engine::DEFAULT_ATH_LOOKBACK_DAYS),
                    max_buy_from_high_pct: flag_f64(rest, "--max-buy-from-high")
                        .unwrap_or(super::engine::DEFAULT_MAX_BUY_FROM_HIGH_PCT),
                    min_dip_from_high_pct: flag_f64(rest, "--min-dip").unwrap_or(0.0),
                    max_runup_pct: flag_f64(rest, "--max-runup")
                        .unwrap_or(super::engine::DEFAULT_MAX_RUNUP_PCT),
                    trend_filter: flag_str(rest, "--trend-filter")
                        .and_then(|s| TrendFilter::parse(s))
                        .unwrap_or(TrendFilter::Off),
                    trend_exit: flag_bool(rest, "--trend-exit"),
                    take_profit_pct: flag_f64(rest, "--take-profit")
                        .unwrap_or(super::engine::DEFAULT_TAKE_PROFIT_PCT),
                    trail_pct: flag_f64(rest, "--trail").unwrap_or(0.0),
                    ceiling: flag_f64(rest, "--ceiling"),
                    floor: flag_f64(rest, "--floor"),
                    profit_mode: flag_profit_mode(rest),
                };
                let result = super::backtest::run_backtest(&cfg)?;
                print!("{}", result.report());
                if rest.iter().any(|a| a == "--walk-forward") {
                    let mid = cfg.from + chrono::Duration::days((cfg.to - cfg.from).num_days() / 2);
                    let wf = super::backtest::walk_forward(
                        &[cfg.symbol.clone()],
                        cfg.from,
                        cfg.to,
                        mid,
                        180,
                        60,
                        cfg.capital_usd,
                    )?;
                    print!("{}", wf.report());
                }
                Ok(())
            }
        }
        "lessons" => {
            let store = TradingStore::open(TradingMode::Simulation)?;
            let limit = flag_i64(rest, "--limit").unwrap_or(20);
            for l in store.list_lessons(limit)? {
                println!("[{}] {}: {}", l.id, l.symbol, l.lesson);
            }
            Ok(())
        }
        "backtests" => {
            let store = TradingStore::open(TradingMode::Simulation)?;
            let limit = flag_i64(rest, "--limit").unwrap_or(20);
            for b in store.list_backtests(limit)? {
                println!(
                    "[{}] {} {}..{} trades={} pnl={:+.2}% dd={:.2}% final=${:.2}",
                    b.id,
                    b.symbol,
                    b.start_date,
                    b.end_date,
                    b.total_trades,
                    b.pnl_pct,
                    b.max_drawdown_pct,
                    b.final_capital_usd
                );
            }
            Ok(())
        }
        "tune" => {
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let dry_run = rest.iter().any(|a| a == "--dry-run");
            let model = flag_str(rest, "--model");
            let limit = flag_i64(rest, "--limit").unwrap_or(10);
            let out = super::tuner::run_tuner(&store, dry_run, model, limit)?;
            println!("=== AI tune [{}] ===", mode.as_str());
            println!("Model: {}", out.source);
            println!("Status: {}", out.note);
            println!("Rationale: {}", out.rationale);
            println!("Resulting config:");
            println!(
                "  margin {:.4} | fee {:.4} | tranches {} | min-tranche {:.4} | spread {:.2}% | crash {:.1}% / {}s | ath {}/{}% runup {}% | trend {} exit {} tp {:.2} trail {:.2} | profit {}",
                out.config.min_profit_margin,
                out.config.taker_fee_rate,
                out.config.tranche_count,
                out.config.min_tranche_fraction,
                out.config.max_spread_pct,
                out.config.crash_drop_pct,
                out.config.crash_window_secs,
                out.config.ath_lookback_days,
                out.config.max_buy_from_high_pct,
                out.config.max_runup_pct,
                out.config.trend_filter.as_str(),
                out.config.trend_exit,
                out.config.take_profit_pct,
                out.config.trail_pct,
                out.config.profit_mode.as_str()
            );
            Ok(())
        }
        "config" => {
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            if let Some(pos) = rest.iter().position(|a| a == "--set") {
                if rest.len() < pos + 3 {
                    bail!("usage: trade config --set <FIELD> <VALUE>");
                }
                let field = rest[pos + 1].clone();
                let value = rest[pos + 2].clone();
                let current = store
                    .latest_engine_config()?
                    .map(|r| r.to_engine_config())
                    .unwrap_or_else(EngineConfig::runner);
                let next = set_config_field(&current, &field, &value)?;
                store.record_engine_config(super::store::NewEngineConfig {
                    min_profit_margin: next.min_profit_margin,
                    taker_fee_rate: next.taker_fee_rate,
                    tranche_count: next.tranche_count,
                    min_tranche_fraction: next.min_tranche_fraction,
                    max_spread_pct: next.max_spread_pct,
                    crash_drop_pct: next.crash_drop_pct,
                    crash_window_secs: next.crash_window_secs,
                    ath_lookback_days: next.ath_lookback_days,
                    max_buy_from_high_pct: next.max_buy_from_high_pct,
                    min_dip_from_high_pct: next.min_dip_from_high_pct,
                    max_runup_pct: next.max_runup_pct,
                    trend_filter: next.trend_filter.as_str().to_string(),
                    trend_exit: next.trend_exit,
                    take_profit_pct: next.take_profit_pct,
                    trail_pct: next.trail_pct,
                    profit_mode: next.profit_mode.as_str(),
                    rationale: format!("manual override: {field} = {value}"),
                    source: "manual".to_string(),
                })?;
                println!("Config updated [{}]: {field} = {value}", mode.as_str());
                return Ok(());
            }
            let current = store
                .latest_engine_config()?
                .map(|r| r.to_engine_config())
                .unwrap_or_else(EngineConfig::runner);
            println!("=== Engine config [{}] ===", mode.as_str());
            println!(
                "Active: margin {:.4} | fee {:.4} | tranches {} | min-tranche {:.4} | spread {:.2}% | crash {:.1}% / {}s | ath {}/{}% dip {}% runup {}% | trend {} exit {} tp {:.2} trail {:.2} | profit {}",
                current.min_profit_margin,
                current.taker_fee_rate,
                current.tranche_count,
                current.min_tranche_fraction,
                current.max_spread_pct,
                current.crash_drop_pct,
                current.crash_window_secs,
                current.ath_lookback_days,
                current.max_buy_from_high_pct,
                current.min_dip_from_high_pct,
                current.max_runup_pct,
                current.trend_filter.as_str(),
                current.trend_exit,
                current.take_profit_pct,
                current.trail_pct,
                current.profit_mode.as_str()
            );
            let history = store.list_engine_configs(20)?;
            if history.is_empty() {
                println!("History: (none — engine runs on defaults)");
            } else {
                println!("\nHistory (newest first):");
                for c in history {
                    println!(
                        "  [{}] {} margin {:.4} fee {:.4} tranches {} min-tranche {:.4} spread {:.2}% crash {:.1}% / {}s ath {}/{}% runup {}% trend {} exit {} tp {:.2} trail {:.2} profit {} — {} ({})",
                        c.id,
                        c.created_at,
                        c.min_profit_margin,
                        c.taker_fee_rate,
                        c.tranche_count,
                        c.min_tranche_fraction,
                        c.max_spread_pct,
                        c.crash_drop_pct,
                        c.crash_window_secs,
                        c.ath_lookback_days,
                        c.max_buy_from_high_pct,
                        c.max_runup_pct,
                        c.trend_filter,
                        c.trend_exit,
                        c.take_profit_pct,
                        c.trail_pct,
                        c.profit_mode,
                        c.rationale,
                        c.source
                    );
                }
            }
            Ok(())
        }
        "strategies" => {
            let store = TradingStore::open(TradingMode::Simulation)?;
            println!("=== Strategy catalog ===");
            println!("BUILT-IN (ships with the binary):");
            for p in super::strategies::builtin_profiles() {
                println!("  {} — {}", p.name, p.description);
                println!(
                    "    margin {:.4} | fee {:.4} | tranches {} | spread {:.2}% | crash {:.1}% / {}s | ath {}/{}% runup {}% | trend {} exit {} tp {:.2} | profit {}",
                    p.engine_config.min_profit_margin,
                    p.engine_config.taker_fee_rate,
                    p.engine_config.tranche_count,
                    p.engine_config.max_spread_pct,
                    p.engine_config.crash_drop_pct,
                    p.engine_config.crash_window_secs,
                    p.engine_config.ath_lookback_days,
                    p.engine_config.max_buy_from_high_pct,
                    p.engine_config.max_runup_pct,
                    p.engine_config.trend_filter.as_str(),
                    p.engine_config.trend_exit,
                    p.engine_config.take_profit_pct,
                    p.engine_config.profit_mode.as_str()
                );
            }
            let stored = store.list_strategy_profiles()?;
            if !stored.is_empty() {
                println!("\nSTORED (apply with: trade strategy apply <NAME>):");
                for r in stored {
                    let score = r
                        .score
                        .map(|s| format!("{s:.2}"))
                        .unwrap_or_else(|| "-".to_string());
                    println!(
                        "  {} — {} (source: {}, score: {})",
                        r.name, r.description, r.source, score
                    );
                }
            }
            Ok(())
        }
        "strategy" => {
            if rest.is_empty() {
                bail!("usage: trade strategy <apply|train|delete> ...");
            }
            match rest[0].as_str() {
                "apply" => {
                    if rest.len() < 2 {
                        bail!("usage: trade strategy apply <NAME> [--mode live|sim]");
                    }
                    let name = rest[1].clone();
                    let mode = mode_from_args(rest)?;
                    let store = TradingStore::open(mode)?;
                    let profile = super::strategies::builtin_profiles()
                        .into_iter()
                        .find(|p| p.name == name)
                        .or_else(|| {
                            store
                                .get_strategy_profile(&name)
                                .ok()
                                .flatten()
                                .and_then(|r| {
                                    super::strategies::StrategyProfile::from_record(&r).ok()
                                })
                        });
                    match profile {
                        Some(p) => {
                            super::strategies::apply_profile(&store, &p)?;
                            println!("Strategy '{}' applied [{}]", p.name, mode.as_str());
                            println!(
                                "  engine config: margin {:.4} | fee {:.4} | tranches {} | spread {:.2}% | crash {:.1}% / {}s | ath {}/{}% runup {}% | profit {}",
                                p.engine_config.min_profit_margin,
                                p.engine_config.taker_fee_rate,
                                p.engine_config.tranche_count,
                                p.engine_config.max_spread_pct,
                                p.engine_config.crash_drop_pct,
                                p.engine_config.crash_window_secs,
                                p.engine_config.ath_lookback_days,
                                p.engine_config.max_buy_from_high_pct,
                                p.engine_config.max_runup_pct,
                                p.engine_config.profit_mode.as_str()
                            );
                            if !p.default_constraints.is_empty() {
                                println!("  default constraints:");
                                for c in &p.default_constraints {
                                    println!(
                                        "    {} {} {}",
                                        c.asset,
                                        c.rule_type.as_str(),
                                        c.threshold_value
                                    );
                                }
                            }
                        }
                        None => bail!("unknown strategy '{name}' (see `trade strategies`)"),
                    }
                    Ok(())
                }
                "train" => {
                    let mode = mode_from_args(rest)?;
                    let store = TradingStore::open(mode)?;
                    let symbols: Vec<String> = flag_str(rest, "--symbols")
                        .unwrap_or("BTC,ETH,SOL,LINK")
                        .split(',')
                        .map(|s| s.trim().to_uppercase())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let from = parse_date(flag_str(rest, "--from").unwrap_or("2020-01-01"))?;
                    let to = parse_date(flag_str(rest, "--to").unwrap_or("2026-08-01"))?;
                    let grid = if rest.iter().any(|a| a == "--grid-full") {
                        super::strategies::GridSize::Full
                    } else {
                        super::strategies::GridSize::Small
                    };
                    let candidates = if rest.iter().any(|a| a == "--grid-conviction") {
                        super::strategies::conviction_configs()
                    } else {
                        super::strategies::candidate_configs(grid)
                    };
                    let cfg = super::strategies::TrainingConfig {
                        symbols,
                        from,
                        to,
                        window_days: flag_i64(rest, "--window-days").unwrap_or(180),
                        step_days: flag_i64(rest, "--step-days").unwrap_or(60),
                        capital_usd: flag_f64(rest, "--capital").unwrap_or(1000.0),
                        top_n: flag_i64(rest, "--top").unwrap_or(4) as usize,
                        force: rest.iter().any(|a| a == "--force"),
                        threads: flag_u32(rest, "--threads").unwrap_or_else(|| {
                            std::thread::available_parallelism()
                                .map(|n| n.get())
                                .unwrap_or(4) as u32
                        }) as usize,
                    };
                    println!(
                        "Training {} candidate configs on {} symbols ({}..{}), window {}d/step {}d...",
                        candidates.len(),
                        cfg.symbols.len(),
                        from,
                        to,
                        cfg.window_days,
                        cfg.step_days
                    );
                    let summary = super::strategies::train(
                        &store,
                        &cfg,
                        &candidates,
                        &super::backtest::fetch_history_blocking,
                    )?;
                    if summary.skipped {
                        println!(
                            "Skipped: trained profiles already exist (use --force to retrain)."
                        );
                        return Ok(());
                    }
                    println!(
                        "Done: {} runs across {} candidates, {} global + {} per-symbol profiles persisted.",
                        summary.runs,
                        summary.candidates,
                        summary.trained.len(),
                        summary.per_symbol_trained.len()
                    );
                    for t in &summary.trained {
                        println!(
                            "  {} — score {:.2} | avg PnL {:+.2}% | max DD {:.2}% | win-rate {:.0}% | profitable {:.0}% windows | cold-lots {:.0}%",
                            t.profile.name,
                            t.score,
                            t.avg_pnl_pct,
                            t.avg_max_dd_pct,
                            t.win_rate * 100.0,
                            t.profitable_ratio * 100.0,
                            t.cold_ratio * 100.0
                        );
                    }
                    if !summary.per_symbol_trained.is_empty() {
                        println!("Per-symbol best configs:");
                        for t in &summary.per_symbol_trained {
                            println!(
                                "  {} — score {:.2} | avg PnL {:+.2}% | win-rate {:.0}% | cold-lots {:.0}%",
                                t.profile.name,
                                t.score,
                                t.avg_pnl_pct,
                                t.win_rate * 100.0,
                                t.cold_ratio * 100.0
                            );
                        }
                    }
                    println!(
                        "Full per-symbol sweep grid ({} rows) saved to the sim DB.\nRun `trade seed` to bundle profiles + lessons + stats into data/trading_sim_seed.db.",
                        summary.candidates * cfg.symbols.len()
                    );
                    Ok(())
                }
                "delete" => {
                    if rest.len() < 2 {
                        bail!("usage: trade strategy delete <NAME>");
                    }
                    let store = TradingStore::open(mode_from_args(rest)?)?;
                    if store.delete_strategy_profile(&rest[1])? {
                        println!("Deleted strategy profile '{}'", rest[1]);
                    } else {
                        println!(
                            "No stored profile named '{}' (builtin profiles cannot be deleted)",
                            rest[1]
                        );
                    }
                    Ok(())
                }
                "regimes" => {
                    // Per-regime winners from the last training sweep: the
                    // configs the bot auto-selects when no explicit choice is set.
                    let store = TradingStore::open(mode_from_args(rest)?)?;
                    let rows = store.list_regime_profiles("")?;
                    if rows.is_empty() {
                        println!(
                            "No regime profiles — run `trade strategy train --force` to build them."
                        );
                        return Ok(());
                    }
                    for r in rows {
                        let cfg = serde_json::from_str::<EngineConfig>(&r.engine_config_json)
                            .unwrap_or_else(|_| EngineConfig::runner());
                        println!(
                            "{} {} -> {} (score {:.1}, avg PnL {:+.1}%, {} runs) | margin {:.3}, tranches {}, tp {:.2}, trail {:.2}, {}",
                            r.symbol,
                            r.regime,
                            r.config_name,
                            r.score,
                            r.avg_pnl_pct,
                            r.runs,
                            cfg.min_profit_margin,
                            cfg.tranche_count,
                            cfg.take_profit_pct,
                            cfg.trail_pct,
                            cfg.profit_mode.as_str()
                        );
                    }
                    Ok(())
                }
                other => {
                    bail!("unknown strategy subcommand '{other}' (apply|train|delete|regimes)")
                }
            }
        }
        "recommend" => {
            if rest.is_empty() {
                bail!("usage: trade recommend <buy|sell|list|execute|reject|expire> ...");
            }
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            // Engine is built per branch: the auto-adaptive config depends on
            // the symbol (regime classification), which is only known there.
            let make_engine = |sym: &str| -> Result<TradingEngine> {
                Ok(TradingEngine::new(active_config(&store, sym)?)?)
            };
            match rest[0].as_str() {
                "buy" => {
                    if rest.len() < 3 {
                        bail!("usage: trade recommend buy <SYMBOL> <PRICE> [--mode live|sim]");
                    }
                    let symbol = rest[1].to_uppercase();
                    let price: f64 = rest[2]
                        .parse()
                        .map_err(|_| anyhow::anyhow!("invalid price '{}'", rest[2]))?;
                    let engine = make_engine(&symbol)?;
                    match engine.plan_buy(&store, &symbol, price, price, price)? {
                        super::engine::Decision::Proceed(plan) => {
                            let rec = store.record_recommendation(NewRecommendation {
                                side: Side::Buy,
                                symbol: plan.symbol.clone(),
                                lot_id: None,
                                recommended_price: plan.entry_price,
                                amount: plan.amount,
                                value_usd: plan.tranche_usd,
                                target_exit_price: Some(plan.target_exit_price),
                                rationale: plan.rationale.clone(),
                            })?;
                            println!("RECOMMENDATION {} [{}]", rec.id, mode.as_str());
                            println!(
                                "  BUY {} {} @ {:.4} (tranche {:.2} USD, target exit {:.4})",
                                plan.symbol,
                                plan.amount,
                                plan.entry_price,
                                plan.tranche_usd,
                                plan.target_exit_price
                            );
                            println!("  rationale: {}", plan.rationale);
                            println!(
                                "  Execute: trade recommend execute {} <ACTUAL_PRICE> [--note \"...\"]",
                                rec.id
                            );
                        }
                        super::engine::Decision::Skip(reason) => {
                            println!("NO ADVICE: {reason}");
                        }
                    }
                    Ok(())
                }
                "sell" => {
                    if rest.len() < 3 {
                        bail!("usage: trade recommend sell <LOT_ID> <PRICE> [--mode live|sim]");
                    }
                    let lot_id = rest[1].clone();
                    let price: f64 = rest[2]
                        .parse()
                        .map_err(|_| anyhow::anyhow!("invalid price '{}'", rest[2]))?;
                    let symbol = store
                        .get_lot(&lot_id)?
                        .ok_or_else(|| anyhow::anyhow!("lot {lot_id} not found"))?
                        .pair;
                    let engine = make_engine(&symbol)?;
                    match engine.plan_sell(&store, &lot_id, price)? {
                        super::engine::Decision::Proceed(plan) => {
                            let rec = store.record_recommendation(NewRecommendation {
                                side: Side::Sell,
                                symbol: symbol.clone(),
                                lot_id: Some(plan.lot_id.clone()),
                                recommended_price: plan.exit_price,
                                amount: plan.amount,
                                value_usd: plan.net_usd,
                                target_exit_price: None,
                                rationale: plan.rationale.clone(),
                            })?;
                            println!("RECOMMENDATION {} [{}]", rec.id, mode.as_str());
                            println!(
                                "  SELL {} {} @ {:.4} (net {:.2}, profit {:.2})",
                                symbol, plan.amount, plan.exit_price, plan.net_usd, plan.profit_usd
                            );
                            println!("  rationale: {}", plan.rationale);
                            println!(
                                "  Execute: trade recommend execute {} <ACTUAL_PRICE> [--note \"...\"]",
                                rec.id
                            );
                        }
                        super::engine::Decision::Skip(reason) => {
                            println!("NO ADVICE: {reason}");
                        }
                    }
                    Ok(())
                }
                "list" => {
                    let limit = flag_i64(rest, "--limit").unwrap_or(20);
                    let status = flag_str(rest, "--status").and_then(RecommendationStatus::parse);
                    let recs = store.list_recommendations(limit, status)?;
                    if recs.is_empty() {
                        println!("No recommendations.");
                        return Ok(());
                    }
                    for rec in recs {
                        println!(
                            "[{}] {} {} {} @ {:.4} ({} USD) — {}",
                            rec.id,
                            rec.status.as_str(),
                            rec.side.as_str(),
                            rec.symbol,
                            rec.recommended_price,
                            rec.value_usd,
                            rec.rationale
                        );
                        if let Some(exit) = rec.target_exit_price {
                            println!("    target exit: {exit:.4}");
                        }
                        if let Some(ap) = rec.actual_price {
                            println!("    filled @ {ap:.4}");
                        }
                        if let Some(n) = &rec.user_note {
                            println!("    note: {n}");
                        }
                    }
                    Ok(())
                }
                "execute" => {
                    if rest.len() < 3 {
                        bail!("usage: trade recommend execute <REC_ID> <ACTUAL_PRICE> [--note \"...\"] [--mode live|sim]");
                    }
                    let rec_id = rest[1].clone();
                    let actual_price: f64 = rest[2]
                        .parse()
                        .map_err(|_| anyhow::anyhow!("invalid price '{}", rest[2]))?;
                    let note = flag_str(rest, "--note").map(|s| s.to_string());
                    let rec = store
                        .get_recommendation(&rec_id)?
                        .ok_or_else(|| anyhow::anyhow!("recommendation {rec_id} not found"))?;
                    let engine = make_engine(&rec.symbol)?;
                    execute_recommendation(&store, &engine, &rec_id, actual_price, note)?;
                    Ok(())
                }
                "parse" => {
                    // trade recommend parse "<free text>" [--mode live|sim]
                    let text = free_text_args(rest)?;
                    let parsed = nlp::parse_trade_report(&text).ok_or_else(|| {
                        anyhow::anyhow!(
                            "could not understand \"{text}\" — say e.g. \"купил ETH за 1800\" or \"sold 0.5 SOL at 75\""
                        )
                    })?;
                    println!(
                        "UNDERSTOOD: {} {} @ {:.4} ({})",
                        parsed.side.as_str(),
                        parsed.symbol,
                        parsed.price,
                        parsed
                            .amount
                            .map(|a| format!("{a} coins"))
                            .unwrap_or_else(|| "amount unknown — tranche sized by engine".into())
                    );
                    // Prefer matching a PENDING advice so the audit trail links
                    // advice → outcome; otherwise record the manual trade.
                    let pending = store
                        .list_recommendations(100, Some(RecommendationStatus::Pending))?
                        .into_iter()
                        .find(|r| r.side == parsed.side && r.symbol == parsed.symbol);
                    match pending {
                        Some(rec) => {
                            let engine = make_engine(&rec.symbol)?;
                            execute_recommendation(
                                &store,
                                &engine,
                                &rec.id,
                                parsed.price,
                                Some(parsed.note),
                            )?
                        }
                        None => {
                            let engine = make_engine(&parsed.symbol)?;
                            record_manual_trade(&store, &engine, &parsed)?
                        }
                    }
                    Ok(())
                }
                "reject" => {
                    if rest.len() < 2 {
                        bail!("usage: trade recommend reject <REC_ID> [--note \"...\"]");
                    }
                    let note = flag_str(rest, "--note").map(|s| s.to_string());
                    let rec = store.mark_recommendation_rejected(&rest[1], note)?;
                    println!("REJECTED recommendation {} ({})", rec.id, rec.symbol);
                    Ok(())
                }
                "expire" => {
                    if rest.len() < 2 {
                        bail!("usage: trade recommend expire <REC_ID>");
                    }
                    let rec = store.mark_recommendation_expired(&rest[1])?;
                    println!("EXPIRED recommendation {} ({})", rec.id, rec.symbol);
                    Ok(())
                }
                other => bail!(
                    "unknown recommend subcommand '{other}' (buy|sell|list|execute|reject|expire|parse)"
                ),
            }
        }
        "fee" => {
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            if rest.iter().any(|a| a == "--buy") || rest.iter().any(|a| a == "--sell") {
                let buy = flag_f64(rest, "--buy").unwrap_or(0.0);
                let sell = flag_f64(rest, "--sell").unwrap_or(0.0);
                let f = store.set_fee_config(buy, sell)?;
                println!(
                    "Exchange fees set [{}]: buy {:.2}% | sell {:.2}%",
                    mode.as_str(),
                    f.buy_fee_pct,
                    f.sell_fee_pct
                );
            } else {
                let f = store.get_fee_config()?;
                println!(
                    "Exchange fees [{}]: buy {:.2}% | sell {:.2}% (updated {})",
                    mode.as_str(),
                    f.buy_fee_pct,
                    f.sell_fee_pct,
                    f.updated_at
                );
            }
            Ok(())
        }
        "dex" => {
            if rest.is_empty() {
                bail!("usage: trade dex <quote|sign|reconcile> ...");
            }
            let chain = if rest.iter().any(|a| a == "--chain") {
                match flag_str(rest, "--chain")
                    .unwrap_or("solana")
                    .to_lowercase()
                    .as_str()
                {
                    "solana" => super::dex::Chain::Solana,
                    "ethereum" | "eth" => super::dex::Chain::Ethereum,
                    other => bail!("unknown chain '{other}' (solana|ethereum)"),
                }
            } else {
                super::dex::Chain::Solana
            };
            match rest[0].as_str() {
                "quote" => {
                    if rest.len() < 3 {
                        bail!("usage: trade dex quote <SYMBOL> <AMOUNT> [--side buy|sell] [--chain solana|ethereum]");
                    }
                    let symbol = rest[1].to_uppercase();
                    let amount: f64 = rest[2].parse()?;
                    let side = match flag_str(rest, "--side")
                        .unwrap_or("buy")
                        .to_lowercase()
                        .as_str()
                    {
                        "buy" => super::dex::SwapSide::Buy,
                        "sell" => super::dex::SwapSide::Sell,
                        other => bail!("unknown side '{other}' (buy|sell)"),
                    };
                    let (input, output) = match side {
                        super::dex::SwapSide::Buy => ("USDC", symbol.as_str()),
                        super::dex::SwapSide::Sell => (symbol.as_str(), "USDC"),
                    };
                    // Convert the human amount to the token's smallest unit.
                    let decimals = if input == "USDC" { 6 } else { 9 };
                    let raw = (amount * 10f64.powi(decimals)) as u64;
                    let quote = super::dex::quote_blocking(chain, &symbol, side, raw)?;
                    println!("=== DEX quote ({}) ===", chain.as_str());
                    println!(
                        "  {} {input} -> {} {output}",
                        quote.input_amount, quote.output_amount
                    );
                    println!(
                        "  implied price: 1 {input} = {:.8} {output}",
                        quote.implied_price()
                    );
                    println!(
                        "  price impact: {:.2}% | route: {}",
                        quote.price_impact_pct, quote.route
                    );
                    println!(
                        "  sign with: trade dex sign {} {} --side {}",
                        symbol,
                        amount,
                        side.as_str()
                    );
                    Ok(())
                }
                "sign" => {
                    if rest.len() < 3 {
                        bail!("usage: trade dex sign <SYMBOL> <AMOUNT> [--side buy|sell] [--chain solana|ethereum]");
                    }
                    let symbol = rest[1].to_uppercase();
                    let amount: f64 = rest[2].parse()?;
                    let side = match flag_str(rest, "--side")
                        .unwrap_or("buy")
                        .to_lowercase()
                        .as_str()
                    {
                        "buy" => super::dex::SwapSide::Buy,
                        "sell" => super::dex::SwapSide::Sell,
                        other => bail!("unknown side '{other}' (buy|sell)"),
                    };
                    let (input, _) = match side {
                        super::dex::SwapSide::Buy => ("USDC", symbol.as_str()),
                        super::dex::SwapSide::Sell => (symbol.as_str(), "USDC"),
                    };
                    let decimals = if input == "USDC" { 6 } else { 9 };
                    let raw = (amount * 10f64.powi(decimals)) as u64;
                    let quote = super::dex::quote_blocking(chain, &symbol, side, raw)?;
                    println!("{}", super::dex::build_swap_payload(&quote));
                    Ok(())
                }
                "reconcile" => {
                    if rest.len() < 4 {
                        bail!("usage: trade dex reconcile <SYMBOL> <AMOUNT> <PRICE> [--side buy|sell] [--max-slippage P]");
                    }
                    let symbol = rest[1].to_uppercase();
                    let amount: f64 = rest[2].parse()?;
                    let price: f64 = rest[3].parse()?;
                    let side = match flag_str(rest, "--side")
                        .unwrap_or("buy")
                        .to_lowercase()
                        .as_str()
                    {
                        "buy" => super::dex::SwapSide::Buy,
                        "sell" => super::dex::SwapSide::Sell,
                        other => bail!("unknown side '{other}' (buy|sell)"),
                    };
                    let max_slippage = flag_f64(rest, "--max-slippage").unwrap_or(2.0);
                    // Re-quote at the executed amount to compare prices.
                    let (input, _) = match side {
                        super::dex::SwapSide::Buy => ("USDC", symbol.as_str()),
                        super::dex::SwapSide::Sell => (symbol.as_str(), "USDC"),
                    };
                    let decimals = if input == "USDC" { 6 } else { 9 };
                    let raw = (amount * 10f64.powi(decimals)) as u64;
                    let quote = super::dex::quote_blocking(chain, &symbol, side, raw)?;
                    let fill = super::dex::reconcile_fill(&quote, side, price, max_slippage)?;
                    println!(
                        "Fill reconciled: {:.6} {} @ {:.8} (quoted {:.8}, slippage {:.2}%)",
                        fill.amount,
                        fill.symbol,
                        fill.executed_price,
                        fill.quoted_price,
                        fill.slippage_pct
                    );
                    Ok(())
                }
                other => bail!("unknown dex subcommand '{other}' (quote|sign|reconcile)"),
            }
        }
        "task" => {
            if rest.is_empty() {
                bail!("usage: trade task <add|list|pause|resume|advise|sim|grid> ...");
            }
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            match rest[0].as_str() {
                "add" => {
                    let name = flag_str(rest, "--name")
                        .ok_or_else(|| anyhow::anyhow!("--name is required"))?;
                    let assets = flag_str(rest, "--assets")
                        .unwrap_or("BTC,ETH,SOL,LINK")
                        .to_uppercase();
                    let t = store.create_task(NewTradeTask {
                        name: name.to_string(),
                        assets,
                        execution: flag_str(rest, "--execution")
                            .and_then(TaskExecution::parse)
                            .unwrap_or(TaskExecution::Manual),
                        monthly_budget_usd: flag_f64(rest, "--budget").unwrap_or(200.0),
                        max_alerts_per_day: flag_i64(rest, "--alerts").unwrap_or(1),
                        tranche_count: flag_u32(rest, "--tranches")
                            .unwrap_or(super::advisor::DEFAULT_TASK_TRANCHES),
                        min_profit_margin: flag_f64(rest, "--margin")
                            .unwrap_or(super::advisor::DEFAULT_TASK_MARGIN),
                    })?;
                    println!(
                        "Task created [{}]: {} (id: {})",
                        mode.as_str(),
                        t.name,
                        t.id
                    );
                    println!(
                        "  assets {} | budget ${:.0}/mo | {} alert/day | {} tranches | margin {:.1}% | {}",
                        t.assets,
                        t.monthly_budget_usd,
                        t.max_alerts_per_day,
                        t.tranche_count,
                        t.min_profit_margin * 100.0,
                        t.execution.as_str()
                    );
                    Ok(())
                }
                "list" => {
                    let tasks = store.list_tasks()?;
                    if tasks.is_empty() {
                        println!("No trade tasks [{}].", mode.as_str());
                        return Ok(());
                    }
                    for t in tasks {
                        println!(
                            "[{}] {} — {} | budget ${:.0}/mo | spent ${:.2} | {}/{} alerts today | {} tranches | margin {:.1}% | {}",
                            t.id,
                            t.name,
                            t.status.as_str(),
                            t.monthly_budget_usd,
                            t.month_spend_usd,
                            t.alerts_today,
                            t.max_alerts_per_day,
                            t.tranche_count,
                            t.min_profit_margin * 100.0,
                            t.execution.as_str()
                        );
                    }
                    Ok(())
                }
                "pause" | "resume" => {
                    if rest.len() < 2 {
                        bail!("usage: trade task {cmd} <TASK_ID>");
                    }
                    let status = if rest[0] == "pause" {
                        TaskStatus::Paused
                    } else {
                        TaskStatus::Active
                    };
                    let t = store.set_task_status(&rest[1], status)?;
                    println!("Task {} -> {} ({})", t.id, t.status.as_str(), t.name);
                    Ok(())
                }
                "advise" => {
                    let symbol = flag_str(rest, "--symbol")
                        .ok_or_else(|| anyhow::anyhow!("--symbol is required"))?
                        .to_uppercase();
                    let price = flag_f64(rest, "--price")
                        .ok_or_else(|| anyhow::anyhow!("--price is required"))?;
                    let signal = flag_str(rest, "--signal")
                        .and_then(BuySignal::parse)
                        .unwrap_or(BuySignal::Any);
                    let task = match flag_str(rest, "--task") {
                        Some(id) => store.get_task(id)?,
                        None => store
                            .list_tasks()?
                            .into_iter()
                            .find(|t| t.status == TaskStatus::Active)
                            .ok_or_else(|| {
                                anyhow::anyhow!(
                                    "no active task — create one with `trade task add` or pass --task <ID>"
                                )
                            })?,
                    };
                    // Recent closes: last 30 days of real history (same source
                    // the bot's backtests and training use).
                    let to = chrono::Utc::now().date_naive();
                    let from = to - chrono::Duration::days(30);
                    let hist = super::backtest::fetch_history_blocking(&symbol, from, to)?;
                    let closes: Vec<f64> = hist.iter().map(|&(_, p)| p).collect();
                    let fees = store.get_fee_config()?;
                    match decide(&task, &fees, &store, &symbol, price, &closes, signal)? {
                        Some(advice) => {
                            let spend = if advice.side == Side::Buy {
                                advice.value_usd
                            } else {
                                0.0
                            };
                            let updated = store.bump_task_alert(&task.id, spend)?;
                            println!(
                                "ADVICE [{}] {} — {} {} @ {:.4}",
                                task.id,
                                task.name,
                                advice.side.as_str(),
                                advice.symbol,
                                advice.price
                            );
                            println!(
                                "  amount {:.6} | value ${:.2} | target exit {:?}",
                                advice.amount, advice.value_usd, advice.target_exit_price
                            );
                            println!("  rationale: {}", advice.rationale);
                            println!(
                                "  task now: spent ${:.2}/mo | {}/{} alerts today",
                                updated.month_spend_usd,
                                updated.alerts_today,
                                updated.max_alerts_per_day
                            );
                            // Locality snapshot: trend/RSI/ATH/halving context so
                            // the operator sees *where* the market stands before
                            // acting ("81k BTC after a 59k summer is not a dip").
                            let loc_hist = super::backtest::fetch_history_blocking(
                                &symbol,
                                to - chrono::Duration::days(400),
                                to,
                            )?;
                            let state = super::analyze::market_state(&symbol, price, &loc_hist);
                            println!("  locality: {}", super::analyze::render(&state));
                            // AI debate room on top of the deterministic advice:
                            // for a SELL signal, bull/bear/arbiter confirm or
                            // override before the operator acts. Safe HOLD
                            // fallback when no model is available.
                            if advice.side == Side::Sell {
                                let outcome =
                                    super::debate::run_sell_debate(&store, &symbol, price, None);
                                println!(
                                    "  AI debate: {} — {}",
                                    outcome.action.as_str(),
                                    outcome.arbiter_verdict
                                );
                            }
                        }
                        None => {
                            println!(
                                "NO ADVICE: task paused, budget exhausted, alert cap hit, ceiling reached, or no buy signal."
                            );
                        }
                    }
                    Ok(())
                }
                "sim" => {
                    let cfg = TaskSimConfig {
                        symbols: flag_str(rest, "--symbols")
                            .unwrap_or("BTC,ETH,SOL,LINK")
                            .split(',')
                            .map(|s| s.trim().to_uppercase())
                            .filter(|s| !s.is_empty())
                            .collect(),
                        from: parse_date(flag_str(rest, "--from").unwrap_or("2020-01-01"))?,
                        to: parse_date(flag_str(rest, "--to").unwrap_or("2026-09-05"))?,
                        monthly_budget_usd: flag_f64(rest, "--budget").unwrap_or(200.0),
                        max_alerts_per_day: flag_i64(rest, "--alerts").unwrap_or(1),
                        tranche_count: flag_u32(rest, "--tranches")
                            .unwrap_or(super::advisor::DEFAULT_TASK_TRANCHES),
                        min_profit_margin: flag_f64(rest, "--margin")
                            .unwrap_or(super::advisor::DEFAULT_TASK_MARGIN),
                        buy_fee_pct: flag_f64(rest, "--buy-fee").unwrap_or(0.0),
                        sell_fee_pct: flag_f64(rest, "--sell-fee").unwrap_or(0.0),
                        signal: flag_str(rest, "--signal")
                            .and_then(BuySignal::parse)
                            .unwrap_or(BuySignal::Any),
                        ceiling: flag_f64(rest, "--ceiling"),
                    };
                    let mut histories = std::collections::HashMap::new();
                    for sym in &cfg.symbols {
                        let h = super::backtest::fetch_history_blocking(sym, cfg.from, cfg.to)?;
                        histories.insert(sym.clone(), h);
                    }
                    let r = simulate_task(&cfg, &histories)?;
                    print!("{}", report(&r));
                    store.record_task_sim(&super::store::NewTaskSimRun {
                        label: flag_str(rest, "--label").unwrap_or("task-sim").to_string(),
                        symbols: cfg.symbols.join(","),
                        start_date: cfg.from.to_string(),
                        end_date: cfg.to.to_string(),
                        monthly_budget_usd: cfg.monthly_budget_usd,
                        buy_fee_pct: cfg.buy_fee_pct,
                        sell_fee_pct: cfg.sell_fee_pct,
                        tranche_count: cfg.tranche_count,
                        min_profit_margin: cfg.min_profit_margin,
                        max_alerts_per_day: cfg.max_alerts_per_day,
                        signal: cfg.signal.as_str().to_string(),
                        total_buys: r.total_buys,
                        total_sells: r.total_sells,
                        realized_profit_usd: r.realized_profit_usd,
                        unrealized_profit_usd: r.unrealized_profit_usd,
                        fees_paid_usd: r.fees_paid_usd,
                        open_lots: r.open_lots,
                        total_deployed_usd: r.total_deployed_usd,
                        win_rate: r.win_rate,
                        total_pnl_pct: r.total_pnl_pct,
                        score: r.score,
                    })?;
                    println!("Persisted to task_sim_runs.");
                    Ok(())
                }
                "grid" => {
                    let cfg = TaskGridConfig {
                        symbols: flag_str(rest, "--symbols")
                            .unwrap_or("BTC,ETH,SOL,LINK")
                            .split(',')
                            .map(|s| s.trim().to_uppercase())
                            .filter(|s| !s.is_empty())
                            .collect(),
                        from: parse_date(flag_str(rest, "--from").unwrap_or("2020-01-01"))?,
                        to: parse_date(flag_str(rest, "--to").unwrap_or("2026-09-05"))?,
                        monthly_budget_usd: flag_f64(rest, "--budget").unwrap_or(200.0),
                        max_alerts_per_day: flag_i64(rest, "--alerts").unwrap_or(1),
                        tranche_counts: vec![2, 3, 4, 6],
                        margins: vec![0.02, 0.03, 0.05, 0.08],
                        buy_fees: vec![0.0, 2.5, 5.0],
                        sell_fees: vec![0.0, 1.25, 2.5],
                        signals: vec![
                            BuySignal::Any,
                            BuySignal::DipOnly,
                            BuySignal::RsiOnly,
                            BuySignal::SmaOnly,
                        ],
                        ceiling: flag_f64(rest, "--ceiling"),
                    };
                    let results = simulate_grid(&cfg)?;
                    print!("{}", grid_summary(&results));
                    for r in &results {
                        store.record_task_sim(&super::store::NewTaskSimRun {
                            label: format!("grid-{}", r.label),
                            symbols: cfg.symbols.join(","),
                            start_date: cfg.from.to_string(),
                            end_date: cfg.to.to_string(),
                            monthly_budget_usd: cfg.monthly_budget_usd,
                            buy_fee_pct: r.buy_fee_pct,
                            sell_fee_pct: r.sell_fee_pct,
                            tranche_count: r.tranche_count,
                            min_profit_margin: r.min_profit_margin,
                            max_alerts_per_day: cfg.max_alerts_per_day,
                            signal: r.signal.clone(),
                            total_buys: r.total_buys,
                            total_sells: r.total_sells,
                            realized_profit_usd: r.realized_profit_usd,
                            unrealized_profit_usd: r.unrealized_profit_usd,
                            fees_paid_usd: r.fees_paid_usd,
                            open_lots: r.open_lots,
                            total_deployed_usd: r.total_deployed_usd,
                            win_rate: r.win_rate,
                            total_pnl_pct: r.total_pnl_pct,
                            score: r.score,
                        })?;
                    }
                    println!("Persisted {} runs to task_sim_runs.", results.len());
                    Ok(())
                }
                other => bail!(
                    "unknown task subcommand '{other}' (add|list|pause|resume|advise|sim|grid)"
                ),
            }
        }
        "experiment" => {
            if rest.is_empty() {
                bail!("usage: trade experiment <run|list|clear> ...");
            }
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            match rest[0].as_str() {
                "run" => {
                    let symbols: Vec<String> = flag_str(rest, "--symbols")
                        .unwrap_or("BTC,ETH,SOL,LINK")
                        .split(',')
                        .map(|s| s.trim().to_uppercase())
                        .filter(|s| !s.is_empty())
                        .collect();
                    let from = parse_date(flag_str(rest, "--from").unwrap_or("2019-01-01"))?;
                    let to = parse_date(flag_str(rest, "--to").unwrap_or("2026-09-05"))?;
                    let capitals: Vec<f64> = flag_str(rest, "--capitals")
                        .unwrap_or("20,50,200")
                        .split(',')
                        .filter_map(|s| s.trim().parse().ok())
                        .collect();
                    let label = flag_str(rest, "--label")
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("exp-{}", chrono::Utc::now().timestamp()));

                    // Base config: --config NAME (stored/builtin profile) or the
                    // active engine config, then inline overrides on top.
                    let mut base = match flag_str(rest, "--config") {
                        Some(name) => {
                            let profile = super::strategies::builtin_profiles()
                                .into_iter()
                                .find(|p| p.name == name)
                                .or_else(|| {
                                    store
                                        .get_strategy_profile(&name)
                                        .ok()
                                        .flatten()
                                        .and_then(|r| {
                                            super::strategies::StrategyProfile::from_record(&r).ok()
                                        })
                                });
                            match profile {
                                Some(p) => p.engine_config,
                                None => bail!("unknown --config '{name}' (see `trade strategies`)"),
                            }
                        }
                        None => store
                            .latest_engine_config()?
                            .map(|r| r.to_engine_config())
                            .unwrap_or_else(EngineConfig::runner),
                    };
                    // Inline overrides reuse the same validated field setter as
                    // `trade config --set`, so a typo can never persist garbage.
                    for (flag, field) in [
                        ("--margin", "min_profit_margin"),
                        ("--fee", "taker_fee_rate"),
                        ("--tranches", "tranche_count"),
                        ("--crash-drop", "crash_drop_pct"),
                        ("--crash-window", "crash_window_secs"),
                        ("--ath-days", "ath_lookback_days"),
                        ("--max-buy-from-high", "max_buy_from_high_pct"),
                        ("--min-dip", "min_dip_from_high_pct"),
                        ("--max-runup", "max_runup_pct"),
                        ("--trend-filter", "trend_filter"),
                        ("--trend-exit", "trend_exit"),
                        ("--take-profit", "take_profit_pct"),
                        ("--trail", "trail_pct"),
                        ("--profit-mode", "profit_mode"),
                    ] {
                        if let Some(v) = flag_str(rest, flag) {
                            base = set_config_field(&base, field, &v)?;
                        }
                    }

                    let cfg = super::experiment::ExperimentConfig {
                        label: label.clone(),
                        symbols,
                        from,
                        to,
                        window_days: flag_i64(rest, "--window-days").unwrap_or(180),
                        step_days: flag_i64(rest, "--step-days").unwrap_or(60),
                        capitals,
                        configs: vec![base],
                        threads: flag_u32(rest, "--threads").unwrap_or_else(|| {
                            std::thread::available_parallelism()
                                .map(|n| n.get())
                                .unwrap_or(4) as u32
                        }) as usize,
                    };
                    let results = super::experiment::run_experiment(
                        &store,
                        &cfg,
                        &super::backtest::fetch_history_blocking,
                    )?;
                    // Compare against the previous persisted run (any label).
                    let prev = store.list_experiments(1)?;
                    println!("\nExperiment '{}' [{}]:", label, mode.as_str());
                    for r in &results {
                        println!(
                            "  {} — score {:.2} | avg PnL {:+.2}% | max DD {:.2}% | win-rate {:.0}% | profitable {:.0}% windows | cold {:.0}% | beats B&H {}/{} windows | avg vs B&H {:+.2}pp | runs {}",
                            r.config_name,
                            r.score,
                            r.avg_pnl_pct,
                            r.avg_max_dd_pct,
                            r.win_rate * 100.0,
                            r.profitable_ratio * 100.0,
                            r.cold_ratio * 100.0,
                            r.windows_beaten_bh,
                            r.windows_total,
                            r.avg_pnl_vs_bh,
                            r.runs
                        );
                    }
                    if let Some(p) = prev.first() {
                        let delta = results.first().map(|r| r.score - p.score).unwrap_or(0.0);
                        println!(
                            "  vs previous '{}': score delta {:+.2} (prev {:.2})",
                            p.label, delta, p.score
                        );
                    }
                    Ok(())
                }
                "list" => {
                    let limit = flag_i64(rest, "--limit").unwrap_or(10);
                    let rows = store.list_experiments(limit)?;
                    if rows.is_empty() {
                        println!("No experiments yet — run `trade experiment run ...`.");
                        return Ok(());
                    }
                    println!("Recent experiments [{}]:", mode.as_str());
                    for (i, r) in rows.iter().enumerate() {
                        let delta = if i + 1 < rows.len() {
                            r.score - rows[i + 1].score
                        } else {
                            0.0
                        };
                        println!(
                            "  [{}] {} — {} | {} | {}..{} | w{} s{} | caps {} | runs {} | score {:.2} ({:+.2} vs prev) | avg PnL {:+.2}% | DD {:.2}% | B&H {}/{} | vs B&H {:+.2}pp | {}",
                            r.id,
                            r.label,
                            r.config_name,
                            r.symbols,
                            r.from_date,
                            r.to_date,
                            r.window_days,
                            r.step_days,
                            r.capitals,
                            r.runs,
                            r.score,
                            delta,
                            r.avg_pnl_pct,
                            r.avg_max_dd_pct,
                            r.windows_beaten_bh,
                            r.windows_total,
                            r.avg_pnl_vs_bh,
                            r.created_at
                        );
                    }
                    Ok(())
                }
                "clear" => {
                    store.clear_experiments()?;
                    println!("All experiment rows dropped [{}].", mode.as_str());
                    Ok(())
                }
                other => bail!("unknown experiment subcommand '{other}' (run|list|clear)"),
            }
        }
        "faketrade" => {
            // faketrade <BUDGET> auto [--symbols A,B,C] [--from DATE] [--to DATE]
            // faketrade clear | stop
            if rest.is_empty() {
                bail!("usage: trade faketrade <BUDGET> auto [--symbols A,B,C] [--from DATE] [--to DATE] | clear | stop");
            }
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            match rest[0].as_str() {
                "clear" => {
                    store.clear_fake_trades()?;
                    println!(
                        "Fake account reset (all virtual trades dropped) [{}].",
                        mode.as_str()
                    );
                    Ok(())
                }
                "stop" => {
                    store.clear_fake_trades()?;
                    println!(
                        "Fake trading stopped and account reset [{}].",
                        mode.as_str()
                    );
                    Ok(())
                }
                budget_str => {
                    let budget: f64 = budget_str
                        .parse()
                        .map_err(|_| anyhow::anyhow!("invalid budget '{budget_str}'"))?;
                    if rest.get(1).map(|s| s.as_str()) != Some("auto") {
                        bail!("usage: trade faketrade <BUDGET> auto [--symbols A,B,C] [--from DATE] [--to DATE]");
                    }
                    let symbols = flag_str(rest, "--symbols")
                        .unwrap_or("BTC,ETH,SOL,LINK")
                        .split(',')
                        .map(|s| s.trim().to_uppercase())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>();
                    let from = parse_date(flag_str(rest, "--from").unwrap_or("2020-01-01"))?;
                    let to = parse_date(flag_str(rest, "--to").unwrap_or("2026-09-05"))?;
                    // Reuse the advisor task simulator: pure in-memory replay,
                    // respects fees + monthly budget, never emits user alerts.
                    let cfg = super::advisor::TaskSimConfig {
                        symbols: symbols.clone(),
                        from,
                        to,
                        monthly_budget_usd: budget,
                        max_alerts_per_day: i64::MAX,
                        tranche_count: super::advisor::DEFAULT_TASK_TRANCHES,
                        min_profit_margin: super::advisor::DEFAULT_TASK_MARGIN,
                        buy_fee_pct: 0.0,
                        sell_fee_pct: 0.0,
                        signal: super::advisor::BuySignal::Any,
                        ceiling: None,
                    };
                    let mut histories = std::collections::HashMap::new();
                    for sym in &symbols {
                        let h = super::backtest::fetch_history_blocking(sym, from, to)?;
                        if h.is_empty() {
                            bail!("no history for {sym}");
                        }
                        histories.insert(sym.clone(), h);
                    }
                    let r = super::advisor::simulate_task(&cfg, &histories)?;
                    // Persist the aggregate result as virtual fills so
                    // `fakestats` and the log work across invocations.
                    store.record_fake_trade(&super::store::NewFakeTrade {
                        symbol: symbols.join("+"),
                        side: "BUY".into(),
                        price: 0.0,
                        amount: 0.0,
                        usd: r.total_deployed_usd,
                        fee_usd: r.fees_paid_usd / 2.0,
                        pnl_usd: 0.0,
                        note: format!(
                            "faketrade {budget} auto {} {}..{} ({} buys, {} sells)",
                            symbols.join(","),
                            r.start_date,
                            r.end_date,
                            r.total_buys,
                            r.total_sells
                        ),
                    })?;
                    store.record_fake_trade(&super::store::NewFakeTrade {
                        symbol: symbols.join("+"),
                        side: "SELL".into(),
                        price: 0.0,
                        amount: 0.0,
                        usd: r.realized_profit_usd + r.total_deployed_usd,
                        fee_usd: r.fees_paid_usd / 2.0,
                        pnl_usd: r.realized_profit_usd,
                        note: "faketrade auto sell summary".into(),
                    })?;
                    print!("{}", super::advisor::report(&r));
                    println!("Fake account updated — run `trade fakestats` for balances.");
                    Ok(())
                }
            }
        }
        "fakestats" => {
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let s = store.fake_stats()?;
            println!("Fake account stats [{}]:", mode.as_str());
            println!(
                "  Buys: {} (${:.2}, fees ${:.2}) | Sells: {} (${:.2}, fees ${:.2})",
                s.buy_count, s.buy_usd, s.buy_fees, s.sell_count, s.sell_usd, s.sell_fees
            );
            println!(
                "  Realized PnL: ${:.2} | Net PnL (after fees): ${:.2} ({:.2}% of deployed)",
                s.realized_pnl, s.net_pnl, s.net_pct
            );
            if s.per_symbol.is_empty() {
                println!("  No virtual trades yet — run `trade faketrade 50 auto`.");
            } else {
                println!("  Per symbol (bought amt/usd, sold amt/usd):");
                for (sym, b_amt, b_usd, s_amt, s_usd) in &s.per_symbol {
                    println!("    {sym}: bought {b_amt:.6} (${b_usd:.2}) | sold {s_amt:.6} (${s_usd:.2})");
                }
            }
            let limit = flag_i64(rest, "--limit").unwrap_or(10);
            let rows = store.list_fake_trades(limit)?;
            if !rows.is_empty() {
                println!("  Recent virtual trades:");
                for t in rows {
                    println!(
                        "    [{}] {} {} @ {:.4} amt {:.6} usd ${:.2} fee ${:.2} pnl ${:.2} — {}",
                        t.ts,
                        t.side,
                        t.symbol,
                        t.price,
                        t.amount,
                        t.usd,
                        t.fee_usd,
                        t.pnl_usd,
                        t.note
                    );
                }
            }
            Ok(())
        }
        "pnl" => {
            // Aggregate realized PnL + mark-to-market of open/cold lots at the
            // latest price (fetched live, or overridden with --price SYM=P).
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let executions = store.list_executions(10_000)?;
            let mut realized = 0.0f64;
            let mut fees = 0.0f64;
            let mut buy_usd = 0.0f64;
            for e in &executions {
                fees += e.fee_usd;
                match e.side {
                    Side::Buy => buy_usd += e.net_usd,
                    Side::Sell => realized += e.net_usd,
                }
            }
            // Realized PnL = sell proceeds − buy cost (both net of fees).
            let realized_pnl = realized - buy_usd;
            let lots = store.list_lots(None)?;
            let mut open_value = 0.0f64;
            let mut open_cost = 0.0f64;
            let mut prices: std::collections::HashMap<String, f64> =
                std::collections::HashMap::new();
            for (i, a) in rest.iter().enumerate() {
                if a == "--price" {
                    if let Some(pair) = rest.get(i + 1) {
                        if let Some((sym, p)) = pair.split_once('=') {
                            if let Ok(p) = p.parse::<f64>() {
                                prices.insert(sym.to_uppercase(), p);
                            }
                        }
                    }
                }
            }
            let today = chrono::Local::now().date_naive();
            for lot in &lots {
                if lot.status == LotStatus::Closed {
                    continue;
                }
                let sym = lot.pair.split('/').next().unwrap_or(&lot.pair).to_string();
                let price = if let Some(p) = prices.get(&sym) {
                    *p
                } else if let Ok(h) = super::backtest::fetch_history_blocking(
                    &sym,
                    today - chrono::Duration::days(7),
                    today,
                ) {
                    h.last().map(|(_, p)| *p).unwrap_or(lot.entry_price)
                } else {
                    lot.entry_price
                };
                open_value += lot.amount * price;
                open_cost += lot.cost_basis_usd;
            }
            let unrealized = open_value - open_cost;
            let total = realized_pnl + unrealized;
            let deployed = buy_usd + open_cost;
            let pct = if deployed > 0.0 {
                total / deployed * 100.0
            } else {
                0.0
            };
            println!("PnL report [{}]:", mode.as_str());
            println!(
                "  Realized: ${:.2} (sell proceeds ${:.2} − buy cost ${:.2})",
                realized_pnl, realized, buy_usd
            );
            println!(
                "  Open/cold lots: {} lots, cost ${:.2}, mark-to-market ${:.2} → unrealized ${:.2}",
                lots.iter()
                    .filter(|l| l.status != LotStatus::Closed)
                    .count(),
                open_cost,
                open_value,
                unrealized
            );
            println!(
                "  Total PnL: ${:.2} ({:+.2}% on ${:.2} deployed) | fees paid ${:.2}",
                total, pct, deployed, fees
            );
            println!(
                "  (mark-to-market uses the latest fetched close; override with --price BTC=81000)"
            );
            Ok(())
        }
        "debate" => {
            // AI sell-vs-hold debate: `trade debate SYMBOL [--price P] [--model ALIAS]`.
            // Safe HOLD fallback when no API key / model is available.
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let symbol = rest
                .iter()
                .find(|a| !a.starts_with('-'))
                .ok_or_else(|| {
                    anyhow::anyhow!("usage: trade debate <SYMBOL> [--price P] [--model ALIAS]")
                })?
                .to_uppercase();
            let price = flag_f64(rest, "--price").unwrap_or_else(|| {
                let today = chrono::Local::now().date_naive();
                super::backtest::fetch_history_blocking(
                    &symbol,
                    today - chrono::Duration::days(7),
                    today,
                )
                .ok()
                .and_then(|h| h.last().map(|(_, p)| *p))
                .unwrap_or(0.0)
            });
            let model = flag_str(rest, "--model");
            let outcome = super::debate::run_sell_debate(&store, &symbol, price, model);
            println!(
                "DEBATE [{}] @ ${:.2} — verdict: {}",
                outcome.symbol,
                outcome.price,
                outcome.action.as_str()
            );
            println!("  bull: {}", outcome.bull_rationale);
            println!("  bear: {}", outcome.bear_rationale);
            println!("  arbiter: {}", outcome.arbiter_verdict);
            println!("  model: {}", outcome.model_alias);
            Ok(())
        }
        "analyze" => {
            // Deterministic market-state snapshot + engine/ensemble verdict:
            // `trade analyze SYMBOL [--price P] [--mode live|sim]`.
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let symbol = rest
                .iter()
                .find(|a| !a.starts_with('-'))
                .ok_or_else(|| {
                    anyhow::anyhow!("usage: trade analyze <SYMBOL> [--price P] [--mode live|sim]")
                })?
                .to_uppercase();
            let today = chrono::Local::now().date_naive();
            let price = flag_f64(rest, "--price").unwrap_or_else(|| {
                super::backtest::fetch_history_blocking(
                    &symbol,
                    today - chrono::Duration::days(7),
                    today,
                )
                .ok()
                .and_then(|h| h.last().map(|(_, p)| *p))
                .unwrap_or(0.0)
            });
            // ~400 days of closes: enough for SMA200 + RSI14 + seasonality.
            let history = super::backtest::fetch_history_blocking(
                &symbol,
                today - chrono::Duration::days(400),
                today,
            )?;
            let state = super::analyze::market_state(&symbol, price, &history);
            println!("MARKET STATE [{}]", super::analyze::render(&state));
            println!(
                "{}",
                super::analyze::engine_verdict(&store, &symbol, price, &history)
            );
            let profiles = super::analyze::top_profiles(&store, &symbol);
            if profiles.is_empty() {
                println!("  trained profiles: none — run `trade strategy train` first");
            } else {
                println!("  trained profiles (top-3):");
                for p in profiles {
                    println!("    {p}");
                }
            }
            Ok(())
        }
        "regime" => {
            // Quick live check: which market regime is `SYMBOL` in right now,
            // and which config the bot auto-selected for it.
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let symbol = rest
                .iter()
                .find(|a| !a.starts_with('-'))
                .ok_or_else(|| anyhow::anyhow!("usage: trade regime <SYMBOL> [--mode live|sim]"))?
                .to_uppercase();
            let history = regime_history(&symbol);
            if history.is_empty() {
                bail!("no history for {symbol} — check data/history/{symbol}.csv or run `trade history refresh`");
            }
            let regime = super::regime::classify_history(&history);
            println!("{} regime: {}", symbol, regime.as_str());
            let config = super::strategies::auto_engine_config(&store, &symbol, &history)?;
            println!(
                "  auto config: margin {:.3} | fee {:.4} | tranches {} | crash {:.1}% | tp {:.2} | trail {:.2} | {}",
                config.min_profit_margin,
                config.taker_fee_rate,
                config.tranche_count,
                config.crash_drop_pct,
                config.take_profit_pct,
                config.trail_pct,
                config.profit_mode.as_str()
            );
            let row = store.get_regime_profile(&symbol, regime.as_str())?;
            match row {
                Some(r) => println!(
                    "  regime winner: {} (score {:.1}, avg PnL {:+.1}%, {} runs)",
                    r.config_name, r.score, r.avg_pnl_pct, r.runs
                ),
                None => println!(
                    "  no regime winner for {} {} — run `trade strategy train --force`",
                    symbol,
                    regime.as_str()
                ),
            }
            Ok(())
        }
        "pause" | "resume" => {
            if rest.is_empty() {
                bail!("usage: trade {cmd} <alloc-id>");
            }
            let mode = mode_from_args(rest)?;
            let store = TradingStore::open(mode)?;
            let status = if cmd == "pause" {
                AllocationStatus::Paused
            } else {
                AllocationStatus::Active
            };
            store.set_allocation_status(&rest[0], status)?;
            println!("Allocation {} -> {}", rest[0], status.as_str());
            Ok(())
        }
        other => bail!("unknown trade subcommand '{other}'\n\n{USAGE}"),
    }
}

/// Join free-text args into one string, dropping CLI flags ("--mode sim").
/// Lets the operator write the report unquoted: `recommend parse ок я купил
/// ETH за 1800` works exactly like the quoted form.
fn free_text_args(args: &[String]) -> Result<String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let tok = args[i].as_str();
        if tok == "--mode" || tok == "-m" {
            i += 2;
            continue;
        }
        if tok.starts_with("--") {
            i += 1;
            continue;
        }
        parts.push(tok);
        i += 1;
    }
    let text = parts.join(" ");
    if text.trim().is_empty() {
        bail!("usage: trade recommend parse \"<free text>\" [--mode live|sim]");
    }
    Ok(text)
}

/// Re-check every guardrail at the ACTUAL fill price, mirror the fill in the
/// ledger, and record the outcome on the recommendation. Shared by
/// `recommend execute` and `recommend parse`.
fn execute_recommendation(
    store: &TradingStore,
    engine: &TradingEngine,
    rec_id: &str,
    actual_price: f64,
    note: Option<String>,
) -> Result<()> {
    let rec = store
        .get_recommendation(rec_id)?
        .ok_or_else(|| anyhow::anyhow!("recommendation {rec_id} not found"))?;
    if rec.status != RecommendationStatus::Pending {
        bail!(
            "recommendation {rec_id} is {} — only PENDING can be executed",
            rec.status.as_str()
        );
    }
    match rec.side {
        Side::Buy => {
            // Re-run the engine's guardrails at the ACTUAL fill price:
            // constraints, spread, no averaging up, pool funds. If they still
            // pass, mirror the fill in the ledger by opening the lot.
            match engine.plan_buy(store, &rec.symbol, actual_price, actual_price, actual_price)? {
                super::engine::Decision::Proceed(plan) => {
                    let lot = engine.execute_buy(store, &plan)?;
                    store.mark_recommendation_executed(
                        rec_id,
                        Some(lot.id.clone()),
                        actual_price,
                        plan.amount,
                        plan.fee_usd,
                        plan.cost_basis_usd,
                        0.0,
                        note,
                    )?;
                    println!(
                        "EXECUTED buy recommendation {rec_id}: lot {} opened @ {:.4}",
                        lot.id, actual_price
                    );
                    println!(
                        "  amount {} | cost {:.2} | target exit {:.4}",
                        plan.amount, plan.cost_basis_usd, plan.target_exit_price
                    );
                }
                super::engine::Decision::Skip(reason) => {
                    bail!("cannot execute at {actual_price}: {reason}");
                }
            }
        }
        Side::Sell => {
            let lot_id = rec
                .lot_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("sell recommendation {rec_id} has no lot"))?;
            match engine.plan_sell(store, &lot_id, actual_price)? {
                super::engine::Decision::Proceed(plan) => {
                    let ex = engine.execute_sell(store, &plan)?;
                    store.mark_recommendation_executed(
                        rec_id,
                        None,
                        actual_price,
                        plan.amount,
                        plan.fee_usd,
                        ex.net_usd,
                        plan.profit_usd,
                        note,
                    )?;
                    println!(
                        "EXECUTED sell recommendation {rec_id}: lot {lot_id} closed @ {:.4}",
                        actual_price
                    );
                    println!(
                        "  net {:.2} | profit {:.2} | fee {:.2}",
                        ex.net_usd, plan.profit_usd, plan.fee_usd
                    );
                }
                super::engine::Decision::Skip(reason) => {
                    bail!("cannot execute at {actual_price}: {reason}");
                }
            }
        }
    }
    Ok(())
}

/// Record an operator's manual trade that has no matching PENDING advice:
/// run the engine's guardrails at the reported price, mirror the fill in the
/// ledger, and persist a recommendation row so the audit trail is complete.
fn record_manual_trade(
    store: &TradingStore,
    engine: &TradingEngine,
    parsed: &ParsedTradeReport,
) -> Result<()> {
    match parsed.side {
        Side::Buy => {
            match engine.plan_buy(
                store,
                &parsed.symbol,
                parsed.price,
                parsed.price,
                parsed.price,
            )? {
                super::engine::Decision::Proceed(plan) => {
                    let lot = engine.execute_buy(store, &plan)?;
                    let rec = store.record_recommendation(NewRecommendation {
                        side: Side::Buy,
                        symbol: plan.symbol.clone(),
                        lot_id: None,
                        recommended_price: plan.entry_price,
                        amount: plan.amount,
                        value_usd: plan.tranche_usd,
                        target_exit_price: Some(plan.target_exit_price),
                        rationale: format!("operator-reported fill: {}", parsed.note),
                    })?;
                    store.mark_recommendation_executed(
                        &rec.id,
                        Some(lot.id.clone()),
                        parsed.price,
                        plan.amount,
                        plan.fee_usd,
                        plan.cost_basis_usd,
                        0.0,
                        Some(parsed.note.clone()),
                    )?;
                    println!(
                        "RECORDED manual buy: lot {} opened @ {:.4} (target exit {:.4})",
                        lot.id, parsed.price, plan.target_exit_price
                    );
                    println!(
                        "  amount {} | cost {:.2} | note: {}",
                        plan.amount, plan.cost_basis_usd, parsed.note
                    );
                }
                super::engine::Decision::Skip(reason) => {
                    bail!("cannot record buy at {:.4}: {reason}", parsed.price);
                }
            }
        }
        Side::Sell => {
            let lot = store
                .list_lots(None)?
                .into_iter()
                .find(|l| l.pair == parsed.symbol && l.status != LotStatus::Closed)
                .ok_or_else(|| {
                    anyhow::anyhow!("no open lot for {} — nothing to sell", parsed.symbol)
                })?;
            match engine.plan_sell(store, &lot.id, parsed.price)? {
                super::engine::Decision::Proceed(plan) => {
                    let ex = engine.execute_sell(store, &plan)?;
                    let rec = store.record_recommendation(NewRecommendation {
                        side: Side::Sell,
                        symbol: parsed.symbol.clone(),
                        lot_id: Some(lot.id.clone()),
                        recommended_price: plan.exit_price,
                        amount: plan.amount,
                        value_usd: plan.net_usd,
                        target_exit_price: None,
                        rationale: format!("operator-reported fill: {}", parsed.note),
                    })?;
                    store.mark_recommendation_executed(
                        &rec.id,
                        None,
                        parsed.price,
                        plan.amount,
                        plan.fee_usd,
                        ex.net_usd,
                        plan.profit_usd,
                        Some(parsed.note.clone()),
                    )?;
                    println!(
                        "RECORDED manual sell: lot {} closed @ {:.4} (net {:.2}, profit {:.2})",
                        lot.id, parsed.price, ex.net_usd, plan.profit_usd
                    );
                    println!("  note: {}", parsed.note);
                }
                super::engine::Decision::Skip(reason) => {
                    bail!("cannot record sell at {:.4}: {reason}", parsed.price);
                }
            }
        }
    }
    Ok(())
}

fn parse_rule_type(s: &str) -> Result<RuleType> {
    match s.to_ascii_lowercase().as_str() {
        "ceiling" | "max_price_ceiling" | "max" => Ok(RuleType::MaxPriceCeiling),
        "floor" | "min_price_floor" | "min" => Ok(RuleType::MinPriceFloor),
        "drawdown" | "max_drawdown_step" => Ok(RuleType::MaxDrawdownStep),
        "pause-buy" | "pause_buy" => Ok(RuleType::PauseBuy),
        "pause-sell" | "pause_sell" => Ok(RuleType::PauseSell),
        other => bail!("unknown rule type '{other}' (ceiling|floor|drawdown|pause-buy|pause-sell)"),
    }
}

/// Read `--flag <value>` from args.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<f64> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}

/// Apply a validated manual override to the current config. Field names match
/// `EngineConfig`; values are parsed and checked against the tuner bounds so a
/// typo can never persist garbage.
fn set_config_field(current: &EngineConfig, field: &str, value: &str) -> Result<EngineConfig> {
    let mut next = current.clone();
    match field {
        "min_profit_margin" => {
            let v: f64 = value.parse()?;
            next.min_profit_margin = v;
        }
        "taker_fee_rate" => {
            let v: f64 = value.parse()?;
            next.taker_fee_rate = v;
        }
        "tranche_count" => {
            let v: u32 = value.parse()?;
            next.tranche_count = v;
        }
        "min_tranche_fraction" => {
            let v: f64 = value.parse()?;
            next.min_tranche_fraction = v;
        }
        "max_spread_pct" => {
            let v: f64 = value.parse()?;
            next.max_spread_pct = v;
        }
        "crash_drop_pct" => {
            let v: f64 = value.parse()?;
            next.crash_drop_pct = v;
        }
        "crash_window_secs" => {
            let v: i64 = value.parse()?;
            next.crash_window_secs = v;
        }
        "ath_lookback_days" => {
            let v: i64 = value.parse()?;
            next.ath_lookback_days = v;
        }
        "max_buy_from_high_pct" => {
            let v: f64 = value.parse()?;
            next.max_buy_from_high_pct = v;
        }
        "min_dip_from_high_pct" => {
            let v: f64 = value.parse()?;
            next.min_dip_from_high_pct = v;
        }
        "max_runup_pct" => {
            let v: f64 = value.parse()?;
            next.max_runup_pct = v;
        }
        "trend_filter" => {
            let t = TrendFilter::parse(value).ok_or_else(|| {
                anyhow::anyhow!("invalid trend_filter '{value}' (OFF|SMA50|GOLDEN_CROSS)")
            })?;
            next.trend_filter = t;
        }
        "trend_exit" => {
            let v: bool = match value.to_lowercase().as_str() {
                "true" | "1" | "on" | "yes" => true,
                "false" | "0" | "off" | "no" => false,
                _ => bail!("invalid trend_exit '{value}' (true|false)"),
            };
            next.trend_exit = v;
        }
        "take_profit_pct" => {
            let v: f64 = value.parse()?;
            next.take_profit_pct = v;
        }
        "trail_pct" => {
            let v: f64 = value.parse()?;
            next.trail_pct = v;
        }
        "profit_mode" => {
            let m = ProfitMode::parse(value).ok_or_else(|| {
                anyhow::anyhow!("invalid profit_mode '{value}' (REINVEST|SEPARATE|PARTIAL:<0..1>)")
            })?;
            next.profit_mode = m;
        }
        other => bail!(
            "unknown config field '{other}' (min_profit_margin|taker_fee_rate|tranche_count|\
             min_tranche_fraction|max_spread_pct|crash_drop_pct|crash_window_secs|\
             ath_lookback_days|max_buy_from_high_pct|min_dip_from_high_pct|max_runup_pct|\
             trend_filter|trend_exit|take_profit_pct|trail_pct|profit_mode)"
        ),
    }
    next.validate()?;
    check_tuner_bounds(&next)?;
    Ok(next)
}

fn print_outcome(out: &TickOutcome, executed: bool) {
    println!(
        "=== Tick {} @ {:.4} ({}) ===",
        out.symbol,
        out.price,
        if executed { "executed" } else { "planned" }
    );
    for t in &out.transitions {
        println!("  {t}");
    }
    for s in &out.sells {
        println!(
            "  SELL plan: lot {} @ {:.4} net {:.2} profit {:.2}",
            s.lot_id, s.exit_price, s.net_usd, s.profit_usd
        );
    }
    for b in &out.buys {
        println!(
            "  BUY plan: {} {:.6} @ {:.4} (cost {:.2}, target {:.4}, tranche {:.2})",
            b.symbol, b.amount, b.entry_price, b.cost_basis_usd, b.target_exit_price, b.tranche_usd
        );
    }
    if !out.holds.is_empty() {
        println!("  HOLD: {}", out.holds.join(", "));
    }
    for n in &out.notes {
        println!("  note: {n}");
    }
}

fn export_json(store: &TradingStore) -> Result<()> {
    let out = serde_json::json!({
        "mode": store.mode().as_str(),
        "db": store.db_path(),
        "constraints": store.list_constraints(None, false)?,
        "allocations": store.list_allocations(None)?,
        "lots": store.list_lots(None)?,
        "debates": store.list_debates(1000)?,
        "executions": store.list_executions(1000)?,
        "backtests": store.list_backtests(100)?,
        "lessons": store.list_lessons(100)?,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn export_csv(store: &TradingStore) -> Result<()> {
    let stdout = std::io::stdout();
    let mut w = std::io::BufWriter::new(stdout.lock());
    writeln!(w, "lots")?;
    writeln!(w, "id,pair,entry_price,amount,fee_paid_usd,cost_basis_usd,target_exit_price,status,opened_at,closed_at")?;
    for lot in store.list_lots(None)? {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{},{},{}",
            lot.id,
            lot.pair,
            lot.entry_price,
            lot.amount,
            lot.fee_paid_usd,
            lot.cost_basis_usd,
            lot.target_exit_price,
            lot.status.as_str(),
            lot.opened_at,
            lot.closed_at.unwrap_or_default()
        )?;
    }
    writeln!(w, "\nexecutions")?;
    writeln!(w, "id,lot_id,side,price,amount,fee_usd,net_usd,executed_at")?;
    for e in store.list_executions(1000)? {
        writeln!(
            w,
            "{},{},{},{},{},{},{},{}",
            e.id,
            e.lot_id.unwrap_or_default(),
            e.side.as_str(),
            e.price,
            e.amount,
            e.fee_usd,
            e.net_usd,
            e.executed_at
        )?;
    }
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trading::engine::{Decision, EngineConfig, TradingEngine};
    use crate::trading::store::{LotStatus, NewLot};
    use tempfile::tempdir;

    fn test_store() -> (TradingStore, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("cli_test.db");
        let store = TradingStore::open_path(path.to_str().expect("path"), TradingMode::Simulation)
            .expect("open store");
        (store, dir)
    }

    #[test]
    fn rule_type_aliases() {
        assert_eq!(
            parse_rule_type("ceiling").unwrap(),
            RuleType::MaxPriceCeiling
        );
        assert_eq!(
            parse_rule_type("MAX_PRICE_CEILING").unwrap(),
            RuleType::MaxPriceCeiling
        );
        assert_eq!(parse_rule_type("pause-buy").unwrap(), RuleType::PauseBuy);
        assert!(parse_rule_type("bogus").is_err());
    }

    #[test]
    fn mode_flag_defaults_to_sim() {
        assert_eq!(mode_from_args(&[]).unwrap(), TradingMode::Simulation);
        assert_eq!(
            mode_from_args(&["--mode".to_string(), "live".to_string()]).unwrap(),
            TradingMode::Live
        );
        assert!(mode_from_args(&["--mode".to_string(), "nope".to_string()]).is_err());
    }

    #[test]
    fn config_set_validates_field_and_value() {
        let cur = EngineConfig::default();
        let next = set_config_field(&cur, "min_profit_margin", "0.02").unwrap();
        assert_eq!(next.min_profit_margin, 0.02);
        assert_eq!(next.tranche_count, cur.tranche_count);

        let next = set_config_field(&cur, "tranche_count", "5").unwrap();
        assert_eq!(next.tranche_count, 5);

        let next = set_config_field(&cur, "crash_window_secs", "604800").unwrap();
        assert_eq!(next.crash_window_secs, 604800);

        // garbage values / unknown fields are rejected
        assert!(set_config_field(&cur, "min_profit_margin", "abc").is_err());
        assert!(set_config_field(&cur, "min_profit_margin", "-1").is_err());
        assert!(set_config_field(&cur, "tranche_count", "0").is_err());
        assert!(set_config_field(&cur, "tranche_count", "3.5").is_err());
        assert!(set_config_field(&cur, "crash_drop_pct", "0").is_err());
        assert!(set_config_field(&cur, "bogus", "1").is_err());
        // values beyond the tuner bounds are rejected too (engine would accept)
        assert!(set_config_field(&cur, "min_profit_margin", "0.5").is_err());
        assert!(set_config_field(&cur, "tranche_count", "13").is_err());
        assert!(set_config_field(&cur, "crash_window_secs", "999999999").is_err());
    }

    #[test]
    fn recommend_buy_then_execute_opens_lot_and_records_outcome() {
        let (store, _dir) = test_store();
        let alloc = store
            .create_allocation(NewAllocation {
                asset: "ETH".to_string(),
                total_budget_usd: 300.0,
            })
            .unwrap();
        let engine = TradingEngine::new(EngineConfig::default()).unwrap();

        // Plan a buy recommendation at a dip.
        let plan = match engine
            .plan_buy(&store, "ETH", 1_800.0, 1_800.0, 1_800.0)
            .unwrap()
        {
            Decision::Proceed(p) => p,
            Decision::Skip(r) => panic!("unexpected skip: {r}"),
        };
        let rec = store
            .record_recommendation(NewRecommendation {
                side: Side::Buy,
                symbol: plan.symbol.clone(),
                lot_id: None,
                recommended_price: plan.entry_price,
                amount: plan.amount,
                value_usd: plan.tranche_usd,
                target_exit_price: Some(plan.target_exit_price),
                rationale: plan.rationale.clone(),
            })
            .unwrap();
        assert_eq!(rec.status, RecommendationStatus::Pending);

        // Execute at a slightly different (still valid) price — the engine
        // re-checks every guardrail and mirrors the fill in the ledger.
        let actual = 1_795.0;
        let plan2 = match engine
            .plan_buy(&store, "ETH", actual, actual, actual)
            .unwrap()
        {
            Decision::Proceed(p) => p,
            Decision::Skip(r) => panic!("unexpected skip: {r}"),
        };
        let lot = engine.execute_buy(&store, &plan2).unwrap();
        let done = store
            .mark_recommendation_executed(
                &rec.id,
                Some(lot.id.clone()),
                actual,
                plan2.amount,
                plan2.fee_usd,
                plan2.cost_basis_usd,
                0.0,
                Some("filled on DEX".to_string()),
            )
            .unwrap();
        assert_eq!(done.status, RecommendationStatus::Executed);
        assert_eq!(done.lot_id.as_deref(), Some(lot.id.as_str()));
        assert_eq!(done.actual_price, Some(actual));

        // The lot exists and the pool was deployed.
        let stored_lot = store.get_lot(&lot.id).unwrap().unwrap();
        assert_eq!(stored_lot.pair, "ETH");
        let alloc_after = store.get_allocation(&alloc.id).unwrap().unwrap();
        assert!(alloc_after.available_usd < 300.0);
    }

    #[test]
    fn recommend_sell_requires_no_loss_price() {
        let (store, _dir) = test_store();
        let engine = TradingEngine::new(EngineConfig::default()).unwrap();
        let lot = store
            .open_lot(NewLot {
                allocation_id: None,
                pair: "SOL".to_string(),
                entry_price: 75.0,
                amount: 1.0,
                fee_paid_usd: 0.5,
                target_exit_price: 75.0 * 1.05,
                cost_basis_usd: None,
                peak_price: Some(75.0),
            })
            .unwrap();

        // Selling below the no-loss exit must be refused (never advise a loss).
        match engine.plan_sell(&store, &lot.id, 74.0).unwrap() {
            Decision::Skip(_) => {}
            Decision::Proceed(_) => panic!("loss sale must be refused"),
        }

        // At a profitable price the advice is recorded and can be executed.
        let plan = match engine.plan_sell(&store, &lot.id, 80.0).unwrap() {
            Decision::Proceed(p) => p,
            Decision::Skip(r) => panic!("unexpected skip: {r}"),
        };
        let rec = store
            .record_recommendation(NewRecommendation {
                side: Side::Sell,
                symbol: "SOL".to_string(),
                lot_id: Some(lot.id.clone()),
                recommended_price: plan.exit_price,
                amount: plan.amount,
                value_usd: plan.net_usd,
                target_exit_price: None,
                rationale: plan.rationale.clone(),
            })
            .unwrap();
        let ex = engine.execute_sell(&store, &plan).unwrap();
        let done = store
            .mark_recommendation_executed(
                &rec.id,
                None,
                80.0,
                plan.amount,
                plan.fee_usd,
                ex.net_usd,
                plan.profit_usd,
                None,
            )
            .unwrap();
        assert_eq!(done.status, RecommendationStatus::Executed);
        assert!(done.profit_usd.unwrap() > 0.0);
        assert_eq!(
            store.get_lot(&lot.id).unwrap().unwrap().status,
            LotStatus::Closed
        );
    }

    #[test]
    fn recommend_reject_and_expire_are_immutable_after_terminal() {
        let (store, _dir) = test_store();
        let rec = store
            .record_recommendation(NewRecommendation {
                side: Side::Buy,
                symbol: "LINK".to_string(),
                lot_id: None,
                recommended_price: 9.0,
                amount: 10.0,
                value_usd: 90.0,
                target_exit_price: Some(9.5),
                rationale: "test".to_string(),
            })
            .unwrap();
        let rejected = store
            .mark_recommendation_rejected(&rec.id, Some("not now".to_string()))
            .unwrap();
        assert_eq!(rejected.status, RecommendationStatus::Rejected);
        assert!(store.mark_recommendation_expired(&rec.id).is_err());
        assert!(store
            .mark_recommendation_executed(&rec.id, None, 9.0, 10.0, 0.0, 90.0, 0.0, None)
            .is_err());

        let rec2 = store
            .record_recommendation(NewRecommendation {
                side: Side::Buy,
                symbol: "LINK".to_string(),
                lot_id: None,
                recommended_price: 9.0,
                amount: 10.0,
                value_usd: 90.0,
                target_exit_price: Some(9.5),
                rationale: "test".to_string(),
            })
            .unwrap();
        assert_eq!(
            store.mark_recommendation_expired(&rec2.id).unwrap().status,
            RecommendationStatus::Expired
        );
    }

    #[test]
    fn free_text_args_drops_flags() {
        let args = vec![
            "ок".to_string(),
            "я".to_string(),
            "купил".to_string(),
            "ETH".to_string(),
            "за".to_string(),
            "1800".to_string(),
            "--mode".to_string(),
            "sim".to_string(),
        ];
        assert_eq!(free_text_args(&args).unwrap(), "ок я купил ETH за 1800");
        assert!(free_text_args(&["--mode".to_string(), "sim".to_string()]).is_err());
    }

    #[test]
    fn parse_buy_report_opens_lot_and_records_outcome() {
        let (store, _dir) = test_store();
        store
            .create_allocation(NewAllocation {
                asset: "ETH".to_string(),
                total_budget_usd: 300.0,
            })
            .unwrap();
        let engine = TradingEngine::new(EngineConfig::default()).unwrap();
        let parsed = crate::trading::nlp::parse_trade_report("ок я купил ETH за 1800").unwrap();
        record_manual_trade(&store, &engine, &parsed).unwrap();

        let recs = store.list_recommendations(10, None).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].status, RecommendationStatus::Executed);
        assert_eq!(recs[0].symbol, "ETH");
        assert_eq!(recs[0].actual_price, Some(1800.0));
        assert!(recs[0]
            .user_note
            .as_deref()
            .unwrap()
            .contains("купил ETH за 1800"));
        let lot = store
            .get_lot(recs[0].lot_id.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(lot.pair, "ETH");
        assert_eq!(lot.status, LotStatus::Open);
    }

    #[test]
    fn parse_sell_report_closes_lot_with_profit() {
        let (store, _dir) = test_store();
        let engine = TradingEngine::new(EngineConfig::default()).unwrap();
        let lot = store
            .open_lot(NewLot {
                allocation_id: None,
                pair: "SOL".to_string(),
                entry_price: 75.0,
                amount: 1.0,
                fee_paid_usd: 0.5,
                target_exit_price: 75.0 * 1.05,
                cost_basis_usd: None,
                peak_price: Some(75.0),
            })
            .unwrap();
        let parsed =
            crate::trading::nlp::parse_trade_report("продал SOL, получилось 85.5").unwrap();
        record_manual_trade(&store, &engine, &parsed).unwrap();

        let recs = store.list_recommendations(10, None).unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].side, Side::Sell);
        assert_eq!(recs[0].status, RecommendationStatus::Executed);
        assert!(recs[0].profit_usd.unwrap() > 0.0);
        assert_eq!(
            store.get_lot(&lot.id).unwrap().unwrap().status,
            LotStatus::Closed
        );
    }

    #[test]
    fn parse_sell_at_loss_is_refused() {
        let (store, _dir) = test_store();
        let engine = TradingEngine::new(EngineConfig::default()).unwrap();
        store
            .open_lot(NewLot {
                allocation_id: None,
                pair: "SOL".to_string(),
                entry_price: 75.0,
                amount: 1.0,
                fee_paid_usd: 0.5,
                target_exit_price: 75.0 * 1.05,
                cost_basis_usd: None,
                peak_price: Some(75.0),
            })
            .unwrap();
        let parsed = crate::trading::nlp::parse_trade_report("продал SOL за 74").unwrap();
        assert!(record_manual_trade(&store, &engine, &parsed).is_err());
        // The lot is untouched — never a realized loss.
        let lots = store.list_lots(None).unwrap();
        assert_eq!(lots[0].status, LotStatus::Open);
    }

    #[test]
    fn parse_executes_matching_pending_recommendation() {
        let (store, _dir) = test_store();
        store
            .create_allocation(NewAllocation {
                asset: "ETH".to_string(),
                total_budget_usd: 300.0,
            })
            .unwrap();
        let engine = TradingEngine::new(EngineConfig::default()).unwrap();
        let plan = match engine
            .plan_buy(&store, "ETH", 1_800.0, 1_800.0, 1_800.0)
            .unwrap()
        {
            Decision::Proceed(p) => p,
            Decision::Skip(r) => panic!("unexpected skip: {r}"),
        };
        let rec = store
            .record_recommendation(NewRecommendation {
                side: Side::Buy,
                symbol: plan.symbol.clone(),
                lot_id: None,
                recommended_price: plan.entry_price,
                amount: plan.amount,
                value_usd: plan.tranche_usd,
                target_exit_price: Some(plan.target_exit_price),
                rationale: plan.rationale.clone(),
            })
            .unwrap();

        // The operator reports the fill in plain language; the bot matches the
        // PENDING advice and executes it at the reported price.
        let parsed = crate::trading::nlp::parse_trade_report("купил ETH за 1795").unwrap();
        let pending = store
            .list_recommendations(100, Some(RecommendationStatus::Pending))
            .unwrap()
            .into_iter()
            .find(|r| r.side == parsed.side && r.symbol == parsed.symbol)
            .unwrap();
        execute_recommendation(
            &store,
            &engine,
            &pending.id,
            parsed.price,
            Some(parsed.note),
        )
        .unwrap();

        let done = store.get_recommendation(&rec.id).unwrap().unwrap();
        assert_eq!(done.status, RecommendationStatus::Executed);
        assert_eq!(done.actual_price, Some(1795.0));
        assert!(done.lot_id.is_some());
    }
}
