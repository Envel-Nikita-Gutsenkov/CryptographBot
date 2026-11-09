use super::*;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

// ---------------------------------------------------------------------------
// Row mappers
// ---------------------------------------------------------------------------

pub(crate) fn row_constraint(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperatorConstraint> {
    Ok(OperatorConstraint {
        id: row.get(0)?,
        mode: TradingMode::parse(row.get::<_, String>(1)?.as_str())
            .expect("stored mode is always valid"),
        asset: row.get(2)?,
        rule_type: RuleType::parse(row.get::<_, String>(3)?.as_str())
            .expect("stored rule_type is always valid"),
        threshold_value: row.get(4)?,
        raw_prompt: row.get(5)?,
        is_active: row.get::<_, i64>(6)? != 0,
        created_at: row.get(7)?,
        expires_at: row.get(8)?,
    })
}

pub(crate) fn row_allocation(row: &rusqlite::Row<'_>) -> rusqlite::Result<StrategyAllocation> {
    Ok(StrategyAllocation {
        id: row.get(0)?,
        mode: TradingMode::parse(row.get::<_, String>(1)?.as_str())
            .expect("stored mode is always valid"),
        asset: row.get(2)?,
        total_budget_usd: row.get(3)?,
        deployed_usd: row.get(4)?,
        available_usd: row.get(5)?,
        profit_reserve_usd: row.get(6)?,
        total_profit_usd: row.get(7)?,
        status: AllocationStatus::parse(row.get::<_, String>(8)?.as_str())
            .expect("stored status is always valid"),
        created_at: row.get(9)?,
    })
}

pub(crate) fn row_lot(row: &rusqlite::Row<'_>) -> rusqlite::Result<TradeLot> {
    Ok(TradeLot {
        id: row.get(0)?,
        allocation_id: row.get(1)?,
        mode: TradingMode::parse(row.get::<_, String>(2)?.as_str())
            .expect("stored mode is always valid"),
        pair: row.get(3)?,
        entry_price: row.get(4)?,
        amount: row.get(5)?,
        fee_paid_usd: row.get(6)?,
        cost_basis_usd: row.get(7)?,
        target_exit_price: row.get(8)?,
        status: LotStatus::parse(row.get::<_, String>(9)?.as_str())
            .expect("stored status is always valid"),
        opened_at: row.get(10)?,
        closed_at: row.get(11)?,
        peak_price: row.get(12)?,
    })
}

pub(crate) fn row_debate(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentDebate> {
    Ok(AgentDebate {
        id: row.get(0)?,
        lot_id: row.get(1)?,
        mode: TradingMode::parse(row.get::<_, String>(2)?.as_str())
            .expect("stored mode is always valid"),
        market_context_json: row.get(3)?,
        bull_rationale: row.get(4)?,
        bear_rationale: row.get(5)?,
        arbiter_verdict: row.get(6)?,
        verdict_action: VerdictAction::parse(row.get::<_, String>(7)?.as_str())
            .expect("stored verdict_action is always valid"),
        created_at: row.get(8)?,
    })
}

pub(crate) fn row_execution(row: &rusqlite::Row<'_>) -> rusqlite::Result<TradeExecution> {
    Ok(TradeExecution {
        id: row.get(0)?,
        lot_id: row.get(1)?,
        mode: TradingMode::parse(row.get::<_, String>(2)?.as_str())
            .expect("stored mode is always valid"),
        side: Side::parse(row.get::<_, String>(3)?.as_str()).expect("stored side is always valid"),
        price: row.get(4)?,
        amount: row.get(5)?,
        fee_usd: row.get(6)?,
        net_usd: row.get(7)?,
        executed_at: row.get(8)?,
    })
}

pub(crate) fn row_recommendation(row: &rusqlite::Row<'_>) -> rusqlite::Result<TradeRecommendation> {
    Ok(TradeRecommendation {
        id: row.get(0)?,
        mode: TradingMode::parse(row.get::<_, String>(1)?.as_str())
            .expect("stored mode is always valid"),
        side: Side::parse(row.get::<_, String>(2)?.as_str()).expect("stored side is always valid"),
        symbol: row.get(3)?,
        lot_id: row.get(4)?,
        recommended_price: row.get(5)?,
        amount: row.get(6)?,
        value_usd: row.get(7)?,
        target_exit_price: row.get(8)?,
        rationale: row.get(9)?,
        status: RecommendationStatus::parse(row.get::<_, String>(10)?.as_str())
            .expect("stored status is always valid"),
        created_at: row.get(11)?,
        executed_at: row.get(12)?,
        actual_price: row.get(13)?,
        actual_amount: row.get(14)?,
        fee_usd: row.get(15)?,
        net_usd: row.get(16)?,
        profit_usd: row.get(17)?,
        user_note: row.get(18)?,
    })
}

pub(crate) fn row_backtest(row: &rusqlite::Row<'_>) -> rusqlite::Result<BacktestRun> {
    Ok(BacktestRun {
        id: row.get(0)?,
        mode: TradingMode::parse(row.get::<_, String>(1)?.as_str())
            .expect("stored mode is always valid"),
        symbol: row.get(2)?,
        start_date: row.get(3)?,
        end_date: row.get(4)?,
        initial_capital_usd: row.get(5)?,
        final_capital_usd: row.get(6)?,
        total_trades: row.get(7)?,
        wins: row.get(8)?,
        losses: row.get(9)?,
        pnl_usd: row.get(10)?,
        pnl_pct: row.get(11)?,
        max_drawdown_pct: row.get(12)?,
        created_at: row.get(13)?,
    })
}

pub(crate) fn row_lesson(row: &rusqlite::Row<'_>) -> rusqlite::Result<StrategyLesson> {
    Ok(StrategyLesson {
        id: row.get(0)?,
        mode: TradingMode::parse(row.get::<_, String>(1)?.as_str())
            .expect("stored mode is always valid"),
        symbol: row.get(2)?,
        lesson: row.get(3)?,
        context_json: row.get(4)?,
        created_at: row.get(5)?,
    })
}

pub(crate) fn row_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<TradeTask> {
    Ok(TradeTask {
        id: row.get(0)?,
        mode: TradingMode::parse(row.get::<_, String>(1)?.as_str())
            .expect("stored mode is always valid"),
        name: row.get(2)?,
        assets: row.get(3)?,
        execution: TaskExecution::parse(row.get::<_, String>(4)?.as_str())
            .expect("stored execution is always valid"),
        monthly_budget_usd: row.get(5)?,
        max_alerts_per_day: row.get(6)?,
        tranche_count: row.get(7)?,
        min_profit_margin: row.get(8)?,
        status: TaskStatus::parse(row.get::<_, String>(9)?.as_str())
            .expect("stored status is always valid"),
        month_spend_usd: row.get(10)?,
        month_started_at: row.get(11)?,
        alerts_today: row.get(12)?,
        alerts_date: row.get(13)?,
        created_at: row.get(14)?,
        updated_at: row.get(15)?,
    })
}

/// Split an RFC3339 timestamp into ("YYYY-MM", "YYYY-MM-DD") keys used for
/// the monthly-budget and daily-alert rollovers.
pub(crate) fn month_day_keys(iso: &str) -> (String, String) {
    let date = iso.split('T').next().unwrap_or(iso);
    let month = date.chars().take(7).collect::<String>();
    (month, date.to_string())
}

pub(crate) fn row_engine_config(row: &rusqlite::Row<'_>) -> rusqlite::Result<EngineConfigRecord> {
    Ok(EngineConfigRecord {
        id: row.get(0)?,
        mode: TradingMode::parse(row.get::<_, String>(1)?.as_str())
            .expect("stored mode is always valid"),
        min_profit_margin: row.get(2)?,
        taker_fee_rate: row.get(3)?,
        tranche_count: row.get::<_, i64>(4)? as u32,
        min_tranche_fraction: row.get(5)?,
        max_spread_pct: row.get(6)?,
        crash_drop_pct: row.get(7)?,
        crash_window_secs: row.get(8)?,
        ath_lookback_days: row.get(9)?,
        max_buy_from_high_pct: row.get(10)?,
        min_dip_from_high_pct: row.get(11)?,
        max_runup_pct: row.get(12)?,
        trend_filter: row.get(13)?,
        trend_exit: row.get::<_, i64>(14)? != 0,
        take_profit_pct: row.get(15)?,
        trail_pct: row.get(16)?,
        profit_mode: row.get(17)?,
        rationale: row.get(18)?,
        source: row.get(19)?,
        created_at: row.get(20)?,
    })
}

pub(crate) fn row_strategy_profile(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<StrategyProfileRecord> {
    Ok(StrategyProfileRecord {
        name: row.get(0)?,
        description: row.get(1)?,
        engine_config_json: row.get(2)?,
        default_constraints_json: row.get(3)?,
        source: row.get(4)?,
        score: row.get(5)?,
        created_at: row.get(6)?,
    })
}

pub(crate) fn row_training_stats(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrainingStatsRow> {
    Ok(TrainingStatsRow {
        id: row.get(0)?,
        mode: TradingMode::parse(row.get::<_, String>(1)?.as_str())
            .expect("stored mode is always valid"),
        symbol: row.get(2)?,
        config_name: row.get(3)?,
        min_profit_margin: row.get(4)?,
        taker_fee_rate: row.get(5)?,
        tranche_count: row.get::<_, i64>(6)? as u32,
        crash_drop_pct: row.get(7)?,
        profit_mode: row.get(8)?,
        runs: row.get(9)?,
        avg_pnl_pct: row.get(10)?,
        avg_max_dd_pct: row.get(11)?,
        win_rate: row.get(12)?,
        cold_ratio: row.get(13)?,
        profitable_ratio: row.get(14)?,
        score: row.get(15)?,
        best_window_pnl_pct: row.get(16)?,
        worst_window_pnl_pct: row.get(17)?,
        trained_at: row.get(18)?,
    })
}

pub(crate) fn row_regime_profile(row: &rusqlite::Row<'_>) -> rusqlite::Result<RegimeProfileRow> {
    Ok(RegimeProfileRow {
        id: row.get(0)?,
        mode: TradingMode::parse(row.get::<_, String>(1)?.as_str())
            .expect("stored mode is always valid"),
        symbol: row.get(2)?,
        regime: row.get(3)?,
        config_name: row.get(4)?,
        engine_config_json: row.get(5)?,
        score: row.get(6)?,
        avg_pnl_pct: row.get(7)?,
        runs: row.get(8)?,
    })
}

// -- helpers that work on a borrowed connection (used inside transactions) --

pub(crate) fn get_allocation_conn(
    conn: &Connection,
    id: &str,
    mode: TradingMode,
) -> Result<Option<StrategyAllocation>> {
    Ok(conn
        .query_row(
            "SELECT id, mode, asset, total_budget_usd, deployed_usd, available_usd,
                    profit_reserve_usd, total_profit_usd, status, created_at
             FROM strategy_allocations WHERE id = ?1 AND mode = ?2",
            params![id, mode.as_str()],
            row_allocation,
        )
        .optional()?)
}

pub(crate) fn get_lot_conn(
    conn: &Connection,
    id: &str,
    mode: TradingMode,
) -> Result<Option<TradeLot>> {
    Ok(conn
        .query_row(
            "SELECT id, allocation_id, mode, pair, entry_price, amount, fee_paid_usd,
                    cost_basis_usd, target_exit_price, status, opened_at, closed_at, peak_price
             FROM trade_lots WHERE id = ?1 AND mode = ?2",
            params![id, mode.as_str()],
            row_lot,
        )
        .optional()?)
}
