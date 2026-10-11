use super::*;
use tempfile::tempdir;

fn test_store(mode: TradingMode) -> (TradingStore, tempfile::TempDir) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("trading_test.db");
    let store = TradingStore::open_path(path.to_str().expect("path"), mode).expect("open store");
    (store, dir)
}

fn sample_constraint(asset: &str) -> NewConstraint {
    NewConstraint {
        asset: asset.to_string(),
        rule_type: RuleType::MaxPriceCeiling,
        threshold_value: 65_000.0,
        raw_prompt: format!("don't buy {asset} above 65k"),
        expires_at: None,
    }
}

fn sample_lot(pair: &str, entry: f64, amount: f64) -> NewLot {
    NewLot {
        allocation_id: None,
        pair: pair.to_string(),
        entry_price: entry,
        amount,
        fee_paid_usd: 1.0,
        target_exit_price: entry * 1.05,
        cost_basis_usd: None,
        peak_price: Some(entry),
    }
}

#[test]
fn schema_creates_all_tables() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let conn = store.conn.lock().unwrap();
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap();
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    for expected in [
        "agent_debates",
        "backtest_runs",
        "operator_constraints",
        "strategy_allocations",
        "strategy_lessons",
        "trade_executions",
        "trade_lots",
        "trade_recommendations",
    ] {
        assert!(
            tables.iter().any(|t| t == expected),
            "missing table {expected}"
        );
    }
}

#[test]
fn constraint_crud_roundtrip() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let c = store.add_constraint(sample_constraint("BTC")).unwrap();
    assert!(c.is_active);
    assert_eq!(c.asset, "BTC");

    let listed = store.list_constraints(None, true).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, c.id);

    let by_asset = store.list_constraints(Some("btc"), true).unwrap();
    assert_eq!(by_asset.len(), 1);

    store.set_constraint_active(&c.id, false).unwrap();
    assert!(store.list_constraints(None, true).unwrap().is_empty());
    assert_eq!(store.list_constraints(None, false).unwrap().len(), 1);

    let fetched = store.get_constraint(&c.id).unwrap().unwrap();
    assert!(!fetched.is_active);

    store.delete_constraint(&c.id).unwrap();
    assert!(store.get_constraint(&c.id).unwrap().is_none());
    assert!(store.delete_constraint(&c.id).is_err());
}

#[test]
fn constraint_rejects_bad_input() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let mut bad = sample_constraint("BTC");
    bad.threshold_value = -5.0;
    assert!(store.add_constraint(bad).is_err());
    let mut empty = sample_constraint("");
    empty.asset = "   ".to_string();
    assert!(store.add_constraint(empty).is_err());
}

#[test]
fn allocation_deploy_respects_budget() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let a = store
        .create_allocation(NewAllocation {
            asset: "SOL".to_string(),
            total_budget_usd: 50.0,
        })
        .unwrap();
    assert_eq!(a.available_usd, 50.0);
    assert_eq!(a.deployed_usd, 0.0);

    let after1 = store.deploy_funds(&a.id, 30.0).unwrap();
    assert!((after1.available_usd - 20.0).abs() < EPSILON);
    assert!((after1.deployed_usd - 30.0).abs() < EPSILON);

    let after2 = store.deploy_funds(&a.id, 20.0).unwrap();
    assert!((after2.available_usd - 0.0).abs() < EPSILON);

    // Overdraw must fail — capital boundary invariant.
    assert!(store.deploy_funds(&a.id, 0.01).is_err());
    // Negative deploy must fail.
    assert!(store.deploy_funds(&a.id, -1.0).is_err());
}

#[test]
fn allocation_release_restores_available() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let a = store
        .create_allocation(NewAllocation {
            asset: "ETH".to_string(),
            total_budget_usd: 100.0,
        })
        .unwrap();
    store.deploy_funds(&a.id, 40.0).unwrap();
    let after = store.release_funds(&a.id, 40.0).unwrap();
    assert!((after.available_usd - 100.0).abs() < EPSILON);
    assert!((after.deployed_usd - 0.0).abs() < EPSILON);
    // Cannot release more than deployed.
    assert!(store.release_funds(&a.id, 1.0).is_err());
}

#[test]
fn allocation_credit_profit_compounds_pool() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let a = store
        .create_allocation(NewAllocation {
            asset: "SOL".to_string(),
            total_budget_usd: 100.0,
        })
        .unwrap();
    store.deploy_funds(&a.id, 40.0).unwrap();
    let after = store.credit_profit(&a.id, 12.5).unwrap();
    assert!((after.total_budget_usd - 112.5).abs() < EPSILON);
    assert!((after.available_usd - 72.5).abs() < EPSILON);
    assert!((after.deployed_usd - 40.0).abs() < EPSILON);
    // Cumulative realized profit is tracked regardless of routing.
    assert!((after.total_profit_usd - 12.5).abs() < EPSILON);
    assert!((after.profit_reserve_usd - 0.0).abs() < EPSILON);
    // Negative / garbage profit must fail.
    assert!(store.credit_profit(&a.id, -1.0).is_err());
    assert!(store.credit_profit(&a.id, f64::NAN).is_err());
}

#[test]
fn allocation_credit_profit_reserve_keeps_working_capital() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let a = store
        .create_allocation(NewAllocation {
            asset: "SOL".to_string(),
            total_budget_usd: 100.0,
        })
        .unwrap();
    store.deploy_funds(&a.id, 40.0).unwrap();
    let after = store.credit_profit_reserve(&a.id, 12.5).unwrap();
    // Working pool untouched: total stays 100, deployed stays 40.
    assert!((after.total_budget_usd - 100.0).abs() < EPSILON);
    assert!((after.deployed_usd - 40.0).abs() < EPSILON);
    assert!((after.available_usd - 60.0).abs() < EPSILON);
    // Profit parked in the reserve + tracked cumulatively.
    assert!((after.profit_reserve_usd - 12.5).abs() < EPSILON);
    assert!((after.total_profit_usd - 12.5).abs() < EPSILON);
    // Reserve is never deployable: deploy only draws from available.
    let after2 = store.deploy_funds(&a.id, 60.0).unwrap();
    assert!((after2.available_usd - 0.0).abs() < EPSILON);
    assert!((after2.profit_reserve_usd - 12.5).abs() < EPSILON);
    assert!(store.deploy_funds(&a.id, 0.01).is_err());
    // Garbage must fail.
    assert!(store.credit_profit_reserve(&a.id, -1.0).is_err());
    assert!(store.credit_profit_reserve(&a.id, f64::NAN).is_err());
}

#[test]
fn allocation_status_lifecycle() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let a = store
        .create_allocation(NewAllocation {
            asset: "LINK".to_string(),
            total_budget_usd: 10.0,
        })
        .unwrap();
    store
        .set_allocation_status(&a.id, AllocationStatus::Paused)
        .unwrap();
    assert_eq!(
        store.get_allocation(&a.id).unwrap().unwrap().status,
        AllocationStatus::Paused
    );
    assert_eq!(
        store
            .list_allocations(Some(AllocationStatus::Paused))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .list_allocations(Some(AllocationStatus::Active))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn lot_open_computes_cost_basis_and_deploys() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let a = store
        .create_allocation(NewAllocation {
            asset: "SOL".to_string(),
            total_budget_usd: 100.0,
        })
        .unwrap();
    let mut lot = sample_lot("SOL", 75.0, 1.0);
    lot.allocation_id = Some(a.id.clone());
    lot.fee_paid_usd = 0.5;
    let opened = store.open_lot(lot).unwrap();
    // cost_basis = 75.0 * 1.0 + 0.5
    assert!((opened.cost_basis_usd - 75.5).abs() < EPSILON);
    assert_eq!(opened.status, LotStatus::Open);
    assert!(opened.closed_at.is_none());

    let alloc = store.get_allocation(&a.id).unwrap().unwrap();
    assert!((alloc.available_usd - 24.5).abs() < EPSILON);
    assert!((alloc.deployed_usd - 75.5).abs() < EPSILON);
}

#[test]
fn lot_open_rejects_insufficient_pool() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let a = store
        .create_allocation(NewAllocation {
            asset: "BTC".to_string(),
            total_budget_usd: 10.0,
        })
        .unwrap();
    let mut lot = sample_lot("BTC", 60_000.0, 0.001);
    lot.allocation_id = Some(a.id);
    assert!(store.open_lot(lot).is_err());
}

#[test]
fn lot_open_rejects_garbage_inputs() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    assert!(store.open_lot(sample_lot("BTC", -1.0, 1.0)).is_err());
    assert!(store.open_lot(sample_lot("BTC", 1.0, 0.0)).is_err());
    assert!(store.open_lot(sample_lot("BTC", f64::NAN, 1.0)).is_err());
    assert!(store
        .open_lot(sample_lot("BTC", f64::INFINITY, 1.0))
        .is_err());
    let mut no_fee = sample_lot("BTC", 1.0, 1.0);
    no_fee.fee_paid_usd = -0.1;
    assert!(store.open_lot(no_fee).is_err());
    let mut empty_pair = sample_lot("", 1.0, 1.0);
    empty_pair.pair = "  ".to_string();
    assert!(store.open_lot(empty_pair).is_err());
}

#[test]
fn lot_status_lifecycle() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let lot = store.open_lot(sample_lot("ETH", 1_800.0, 0.1)).unwrap();
    let cold = store.set_lot_status(&lot.id, LotStatus::ColdHold).unwrap();
    assert_eq!(cold.status, LotStatus::ColdHold);
    assert!(cold.closed_at.is_none());

    let closed = store.close_lot(&lot.id).unwrap();
    assert_eq!(closed.status, LotStatus::Closed);
    assert!(closed.closed_at.is_some());

    assert_eq!(store.list_lots(Some(LotStatus::Open)).unwrap().len(), 0);
    assert_eq!(store.list_lots(Some(LotStatus::ColdHold)).unwrap().len(), 0);
    assert_eq!(store.list_lots(Some(LotStatus::Closed)).unwrap().len(), 1);
}

#[test]
fn debate_and_execution_audit() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let lot = store.open_lot(sample_lot("LINK", 9.0, 10.0)).unwrap();

    let debate = store
        .record_debate(NewDebate {
            lot_id: Some(lot.id.clone()),
            market_context_json: r#"{"fng":34}"#.to_string(),
            bull_rationale: "dip looks shallow".to_string(),
            bear_rationale: "macro risk".to_string(),
            arbiter_verdict: "approved with tranche 1".to_string(),
            verdict_action: VerdictAction::Buy,
        })
        .unwrap();
    assert_eq!(debate.verdict_action, VerdictAction::Buy);

    let exec = store
        .record_execution(NewExecution {
            lot_id: Some(lot.id.clone()),
            side: Side::Buy,
            price: 9.0,
            amount: 10.0,
            fee_usd: 0.9,
            net_usd: 90.9,
        })
        .unwrap();
    assert_eq!(exec.side, Side::Buy);

    let debates = store.list_debates(10).unwrap();
    assert_eq!(debates.len(), 1);
    assert_eq!(debates[0].lot_id.as_deref(), Some(lot.id.as_str()));

    let execs = store.list_executions(10).unwrap();
    assert_eq!(execs.len(), 1);
}

fn sample_recommendation(side: Side, symbol: &str) -> NewRecommendation {
    NewRecommendation {
        side,
        symbol: symbol.to_string(),
        lot_id: None,
        recommended_price: 1_800.0,
        amount: 0.1,
        value_usd: 180.0,
        target_exit_price: Some(1_890.0),
        rationale: "dip tranche 1/3".to_string(),
    }
}

#[test]
fn recommendation_crud_and_status_lifecycle() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let rec = store
        .record_recommendation(sample_recommendation(Side::Buy, "ETH"))
        .unwrap();
    assert_eq!(rec.status, RecommendationStatus::Pending);
    assert_eq!(rec.side, Side::Buy);
    assert_eq!(rec.symbol, "ETH");
    assert!(rec.executed_at.is_none());

    // List + filter by status.
    assert_eq!(store.list_recommendations(10, None).unwrap().len(), 1);
    assert_eq!(
        store
            .list_recommendations(10, Some(RecommendationStatus::Pending))
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        store
            .list_recommendations(10, Some(RecommendationStatus::Executed))
            .unwrap()
            .len(),
        0
    );

    // Execute with reported fill (lot must exist — FK constraint).
    let lot = store.open_lot(sample_lot("ETH", 1_800.0, 0.1)).unwrap();
    let done = store
        .mark_recommendation_executed(
            &rec.id,
            Some(lot.id.clone()),
            1_795.0,
            0.1003,
            0.54,
            179.5,
            0.0,
            Some("filled on DEX".to_string()),
        )
        .unwrap();
    assert_eq!(done.status, RecommendationStatus::Executed);
    assert!(done.executed_at.is_some());
    assert_eq!(done.actual_price, Some(1_795.0));
    assert_eq!(done.lot_id.as_deref(), Some(lot.id.as_str()));
    assert_eq!(done.user_note.as_deref(), Some("filled on DEX"));

    // Terminal states are immutable.
    assert!(store
        .mark_recommendation_executed(&rec.id, None, 1.0, 1.0, 0.0, 1.0, 0.0, None)
        .is_err());
    assert!(store.mark_recommendation_rejected(&rec.id, None).is_err());
    assert!(store.mark_recommendation_expired(&rec.id).is_err());

    // Reject path.
    let rec2 = store
        .record_recommendation(sample_recommendation(Side::Sell, "SOL"))
        .unwrap();
    let rejected = store
        .mark_recommendation_rejected(&rec2.id, Some("changed my mind".to_string()))
        .unwrap();
    assert_eq!(rejected.status, RecommendationStatus::Rejected);
    assert_eq!(rejected.user_note.as_deref(), Some("changed my mind"));

    // Expire path.
    let rec3 = store
        .record_recommendation(sample_recommendation(Side::Buy, "LINK"))
        .unwrap();
    assert_eq!(
        store.mark_recommendation_expired(&rec3.id).unwrap().status,
        RecommendationStatus::Expired
    );

    // Garbage inputs are rejected at the boundary.
    let mut bad = sample_recommendation(Side::Buy, "BTC");
    bad.recommended_price = -1.0;
    assert!(store.record_recommendation(bad).is_err());
    let mut bad2 = sample_recommendation(Side::Buy, "BTC");
    bad2.amount = 0.0;
    assert!(store.record_recommendation(bad2).is_err());
    assert!(store.get_recommendation("missing").unwrap().is_none());
}

#[test]
fn recommendation_counts_in_status() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    store
        .record_recommendation(sample_recommendation(Side::Buy, "ETH"))
        .unwrap();
    let counts = store.counts().unwrap();
    assert_eq!(counts.recommendations, 1);
}

#[test]
fn backtest_and_lesson_records() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let bt = store
        .record_backtest(NewBacktest {
            symbol: "BTC".to_string(),
            start_date: "2026-08-01".to_string(),
            end_date: "2026-08-31".to_string(),
            initial_capital_usd: 1000.0,
            final_capital_usd: 1120.0,
            total_trades: 4,
            wins: 3,
            losses: 1,
            pnl_usd: 120.0,
            pnl_pct: 12.0,
            max_drawdown_pct: 4.5,
        })
        .unwrap();
    assert_eq!(bt.symbol, "BTC");
    assert_eq!(store.list_backtests(10).unwrap().len(), 1);

    let lesson = store
        .record_lesson(NewLesson {
            symbol: "SOL".to_string(),
            lesson: "buying within 24h of a >5% drop worked 3/4 times".to_string(),
            context_json: r#"{"regime":"accumulate"}"#.to_string(),
        })
        .unwrap();
    assert_eq!(lesson.symbol, "SOL");
    assert_eq!(store.list_lessons(10).unwrap().len(), 1);
    assert!(store
        .record_lesson(NewLesson {
            symbol: "SOL".to_string(),
            lesson: "   ".to_string(),
            context_json: "{}".to_string(),
        })
        .is_err());
}

#[test]
fn engine_config_crud_roundtrip() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    assert!(store.latest_engine_config().unwrap().is_none());

    let rec = store
        .record_engine_config(NewEngineConfig {
            min_profit_margin: 0.02,
            taker_fee_rate: 0.003,
            tranche_count: 4,
            min_tranche_fraction: 0.01,
            max_spread_pct: 1.0,
            crash_drop_pct: 20.0,
            crash_window_secs: 3600,
            ath_lookback_days: 30,
            max_buy_from_high_pct: 5.0,
            min_dip_from_high_pct: 0.0,
            max_runup_pct: 30.0,
            trend_filter: "OFF".to_string(),
            trend_exit: false,
            take_profit_pct: 0.25,
            trail_pct: 0.15,
            profit_mode: "PARTIAL:0.5000".to_string(),
            rationale: "tuner test: wider margin after many tiny wins".to_string(),
            source: "test-model".to_string(),
        })
        .unwrap();
    assert_eq!(rec.tranche_count, 4);
    assert_eq!(rec.profit_mode, "PARTIAL:0.5000");
    assert_eq!(rec.ath_lookback_days, 30);
    assert_eq!(rec.max_buy_from_high_pct, 5.0);
    assert_eq!(rec.max_runup_pct, 30.0);
    assert_eq!(store.list_engine_configs(10).unwrap().len(), 1);
    let latest = store.latest_engine_config().unwrap().unwrap();
    assert_eq!(latest.id, rec.id);
    assert_eq!(latest.to_engine_config().min_profit_margin, 0.02);
    assert_eq!(latest.to_engine_config().ath_lookback_days, 30);
    assert_eq!(latest.to_engine_config().max_buy_from_high_pct, 5.0);
    assert_eq!(latest.to_engine_config().max_runup_pct, 30.0);
    assert_eq!(
        latest.to_engine_config().profit_mode,
        crate::trading::engine::ProfitMode::Partial(0.5)
    );

    // Garbage must never persist: NaN margin is rejected by the engine
    // validator before the INSERT runs.
    assert!(store
        .record_engine_config(NewEngineConfig {
            min_profit_margin: f64::NAN,
            taker_fee_rate: 0.003,
            tranche_count: 3,
            min_tranche_fraction: 0.005,
            max_spread_pct: 0.75,
            crash_drop_pct: 15.0,
            crash_window_secs: 900,
            ath_lookback_days: 30,
            max_buy_from_high_pct: 5.0,
            min_dip_from_high_pct: 0.0,
            max_runup_pct: 30.0,
            trend_filter: "OFF".to_string(),
            trend_exit: false,
            take_profit_pct: 0.15,
            trail_pct: 0.0,
            profit_mode: "REINVEST".to_string(),
            rationale: "bad".to_string(),
            source: "test-model".to_string(),
        })
        .is_err());
    // Unknown profit_mode must be rejected too.
    assert!(store
        .record_engine_config(NewEngineConfig {
            min_profit_margin: 0.01,
            taker_fee_rate: 0.003,
            tranche_count: 3,
            min_tranche_fraction: 0.005,
            max_spread_pct: 0.75,
            crash_drop_pct: 15.0,
            crash_window_secs: 900,
            ath_lookback_days: 30,
            max_buy_from_high_pct: 5.0,
            min_dip_from_high_pct: 0.0,
            max_runup_pct: 30.0,
            trend_filter: "OFF".to_string(),
            trend_exit: false,
            take_profit_pct: 0.15,
            trail_pct: 0.0,
            profit_mode: "MOON".to_string(),
            rationale: "bad mode".to_string(),
            source: "test-model".to_string(),
        })
        .is_err());
    assert!(store
        .record_engine_config(NewEngineConfig {
            min_profit_margin: 0.01,
            taker_fee_rate: 0.003,
            tranche_count: 3,
            min_tranche_fraction: 0.005,
            max_spread_pct: 0.75,
            crash_drop_pct: 15.0,
            crash_window_secs: 900,
            ath_lookback_days: 30,
            max_buy_from_high_pct: 5.0,
            min_dip_from_high_pct: 0.0,
            max_runup_pct: 30.0,
            trend_filter: "OFF".to_string(),
            trend_exit: false,
            take_profit_pct: 0.15,
            trail_pct: 0.0,
            profit_mode: "REINVEST".to_string(),
            rationale: "  ".to_string(),
            source: "test-model".to_string(),
        })
        .is_err());
    assert_eq!(store.list_engine_configs(10).unwrap().len(), 1);
}

#[test]
fn mode_isolation_between_live_and_sim() {
    let dir = tempdir().unwrap();
    let live_path = dir.path().join("live.db");
    let sim_path = dir.path().join("sim.db");
    let live = TradingStore::open_path(live_path.to_str().unwrap(), TradingMode::Live).unwrap();
    let sim = TradingStore::open_path(sim_path.to_str().unwrap(), TradingMode::Simulation).unwrap();

    live.add_constraint(sample_constraint("BTC")).unwrap();
    live.create_allocation(NewAllocation {
        asset: "ETH".to_string(),
        total_budget_usd: 50.0,
    })
    .unwrap();

    // Sim store must see none of it.
    assert!(sim.list_constraints(None, false).unwrap().is_empty());
    assert!(sim.list_allocations(None).unwrap().is_empty());
    assert_eq!(live.list_constraints(None, false).unwrap().len(), 1);
}

#[test]
fn backup_to_creates_restorable_file() {
    let (store, dir) = test_store(TradingMode::Simulation);
    store.add_constraint(sample_constraint("BTC")).unwrap();
    store.open_lot(sample_lot("ETH", 1_800.0, 0.1)).unwrap();

    let dest = dir.path().join("backup.db");
    let dest_str = dest.to_str().unwrap().to_string();
    store.backup_to(&dest_str).unwrap();

    // Refuse to overwrite an existing file.
    assert!(store.backup_to(&dest_str).is_err());

    let restored = TradingStore::open_path(&dest_str, TradingMode::Simulation).unwrap();
    assert_eq!(restored.list_constraints(None, false).unwrap().len(), 1);
    assert_eq!(restored.list_lots(None).unwrap().len(), 1);
}

#[test]
fn export_seed_keeps_knowledge_and_drops_runtime_state() {
    let (store, dir) = test_store(TradingMode::Simulation);
    // Knowledge: a profile, a lesson, a training-stats row, an engine config.
    let p = crate::trading::strategies::builtin_profiles().remove(0);
    store.record_strategy_profile(&p.to_record()).unwrap();
    store
        .record_lesson(NewLesson {
            symbol: "BTC".into(),
            lesson: "best config for BTC".into(),
            context_json: "{}".into(),
        })
        .unwrap();
    store
        .record_training_stats(&NewTrainingStats {
            symbol: "BTC".into(),
            config_name: "m0.010-t4-REINVEST-c15".into(),
            min_profit_margin: 0.01,
            taker_fee_rate: 0.001,
            tranche_count: 4,
            crash_drop_pct: 15.0,
            profit_mode: "REINVEST".into(),
            runs: 10,
            avg_pnl_pct: 5.0,
            avg_max_dd_pct: 8.0,
            win_rate: 0.9,
            cold_ratio: 0.1,
            profitable_ratio: 0.8,
            score: 3.0,
            best_window_pnl_pct: 12.0,
            worst_window_pnl_pct: -4.0,
        })
        .unwrap();
    store
        .record_engine_config(NewEngineConfig {
            min_profit_margin: 0.01,
            taker_fee_rate: 0.001,
            tranche_count: 4,
            min_tranche_fraction: 0.25,
            max_spread_pct: 0.75,
            crash_drop_pct: 15.0,
            crash_window_secs: 604_800,
            ath_lookback_days: 30,
            max_buy_from_high_pct: 5.0,
            min_dip_from_high_pct: 0.0,
            max_runup_pct: 30.0,
            trend_filter: "OFF".into(),
            trend_exit: false,
            take_profit_pct: 0.15,
            trail_pct: 0.0,
            profit_mode: "REINVEST".into(),
            rationale: "test".into(),
            source: "trained".into(),
        })
        .unwrap();
    // Seasonality knowledge must ship too.
    store.record_seasonality("BTC", 1, 12.5, 3).unwrap();
    store.record_seasonality("BTC", 6, -4.2, 2).unwrap();
    // Runtime state that must NOT ship in the seed.
    store.open_lot(sample_lot("ETH", 1_800.0, 0.1)).unwrap();
    store.add_constraint(sample_constraint("BTC")).unwrap();

    let dest = dir.path().join("seed.db");
    let dest_str = dest.to_str().unwrap().to_string();
    let size = store.export_seed(&dest_str).unwrap();
    assert!(size > 0);

    let seeded = TradingStore::open_path(&dest_str, TradingMode::Simulation).unwrap();
    assert_eq!(seeded.list_strategy_profiles().unwrap().len(), 1);
    assert_eq!(seeded.list_lessons(10).unwrap().len(), 1);
    assert_eq!(seeded.list_training_stats("", 10).unwrap().len(), 1);
    assert_eq!(seeded.list_seasonality("BTC").unwrap().len(), 2);
    assert!(seeded.latest_engine_config().unwrap().is_some());
    // Runtime state excluded.
    assert_eq!(seeded.list_lots(None).unwrap().len(), 0);
    assert_eq!(seeded.list_constraints(None, false).unwrap().len(), 0);
}

#[test]
fn regime_profile_crud_roundtrip_and_seed() {
    let (store, dir) = test_store(TradingMode::Simulation);
    store
        .record_regime_profile(&NewRegimeProfile {
            symbol: "BTC".into(),
            regime: "BULL".into(),
            config_name: "m0.010-t4-REINVEST-c15".into(),
            engine_config_json: "{}".into(),
            score: 2.5,
            avg_pnl_pct: 7.3,
            runs: 42,
        })
        .unwrap();
    store
        .record_regime_profile(&NewRegimeProfile {
            symbol: "BTC".into(),
            regime: "DIP".into(),
            config_name: "m0.050-t2-SEPARATE-c15".into(),
            engine_config_json: "{}".into(),
            score: 1.1,
            avg_pnl_pct: 4.0,
            runs: 18,
        })
        .unwrap();
    let row = store.get_regime_profile("BTC", "bull").unwrap().unwrap();
    assert_eq!(row.regime, "BULL");
    assert_eq!(row.config_name, "m0.010-t4-REINVEST-c15");
    assert_eq!(row.runs, 42);
    // Upsert replaces the winner for the same (symbol, regime).
    store
        .record_regime_profile(&NewRegimeProfile {
            symbol: "BTC".into(),
            regime: "BULL".into(),
            config_name: "m0.020-t6-REINVEST-c15".into(),
            engine_config_json: "{}".into(),
            score: 3.1,
            avg_pnl_pct: 8.0,
            runs: 50,
        })
        .unwrap();
    let rows = store.list_regime_profiles("BTC").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        store
            .get_regime_profile("BTC", "BULL")
            .unwrap()
            .unwrap()
            .config_name,
        "m0.020-t6-REINVEST-c15"
    );
    assert!(store.get_regime_profile("BTC", "BEAR").unwrap().is_none());
    // Regime knowledge ships in the seed.
    let dest = dir.path().join("seed.db");
    let dest_str = dest.to_str().unwrap().to_string();
    store.export_seed(&dest_str).unwrap();
    let seeded = TradingStore::open_path(&dest_str, TradingMode::Simulation).unwrap();
    assert_eq!(seeded.list_regime_profiles("").unwrap().len(), 2);
    // clear drops them for this mode.
    store.clear_regime_profiles().unwrap();
    assert!(store.list_regime_profiles("").unwrap().is_empty());
}

#[test]
fn counts_reflect_store_contents() {
    let (store, _dir) = test_store(TradingMode::Simulation);
    let c0 = store.counts().unwrap();
    assert_eq!(c0.constraints, 0);
    assert_eq!(c0.open_lots, 0);

    store.add_constraint(sample_constraint("BTC")).unwrap();
    store.open_lot(sample_lot("ETH", 1_800.0, 0.1)).unwrap();
    let c1 = store.counts().unwrap();
    assert_eq!(c1.constraints, 1);
    assert_eq!(c1.open_lots, 1);
    assert_eq!(c1.cold_lots, 0);
}

#[test]
fn helpers_round_and_parse() {
    assert!((round_to_8(1.23456789123) - 1.23456789).abs() < EPSILON);
    assert_eq!(TradingMode::parse("sim"), Some(TradingMode::Simulation));
    assert_eq!(TradingMode::parse("LIVE"), Some(TradingMode::Live));
    assert_eq!(TradingMode::parse("nope"), None);
    assert_eq!(RuleType::parse("PAUSE_BUY"), Some(RuleType::PauseBuy));
    assert_eq!(LotStatus::parse("COLD_HOLD"), Some(LotStatus::ColdHold));
    assert_eq!(
        AllocationStatus::parse("ACTIVE"),
        Some(AllocationStatus::Active)
    );
    assert_eq!(VerdictAction::parse("DEFER"), Some(VerdictAction::Defer));
    assert_eq!(Side::parse("SELL"), Some(Side::Sell));
    assert_eq!(TradingMode::Live.db_file(), LIVE_DB_FILE);
    assert_eq!(TradingMode::Simulation.db_file(), SIM_DB_FILE);
}

/// SIGKILL simulation: drop the store mid-write (no clean close), reopen
/// the same file, and verify WAL recovery keeps every committed row and
/// the DB passes `PRAGMA integrity_check`. SQLite's atomic commit is the
/// crash-safety guarantee the live engine relies on.
#[test]
fn crash_recovery_after_abrupt_drop_keeps_rows() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("crash.db");
    let path_str = path.to_str().unwrap();
    {
        let store = TradingStore::open_path(path_str, TradingMode::Simulation).unwrap();
        store.add_constraint(sample_constraint("BTC")).unwrap();
        store
            .create_allocation(NewAllocation {
                asset: "BTC".into(),
                total_budget_usd: 500.0,
            })
            .unwrap();
        store.open_lot(sample_lot("BTC", 60_000.0, 0.005)).unwrap();
        // Abrupt drop: no backup_to, no explicit close — simulates SIGKILL.
    }
    let store = TradingStore::open_path(path_str, TradingMode::Simulation).unwrap();
    let c = store.counts().unwrap();
    assert_eq!(c.constraints, 1, "constraint lost after crash");
    assert_eq!(c.allocations, 1, "allocation lost after crash");
    assert_eq!(c.open_lots, 1, "lot lost after crash");
    let integrity: String = {
        let conn = store.conn.lock().unwrap();
        conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(integrity, "ok", "integrity_check failed: {integrity}");
}
