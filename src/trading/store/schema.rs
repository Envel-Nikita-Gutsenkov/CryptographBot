use rusqlite::Connection;

pub(crate) fn init_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;

        CREATE TABLE IF NOT EXISTS operator_constraints (
            id TEXT PRIMARY KEY,
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            asset TEXT NOT NULL,
            rule_type TEXT NOT NULL
                CHECK(rule_type IN ('MAX_PRICE_CEILING','MIN_PRICE_FLOOR',
                                    'MAX_DRAWDOWN_STEP','PAUSE_BUY','PAUSE_SELL')),
            threshold_value REAL NOT NULL,
            raw_prompt TEXT NOT NULL,
            is_active INTEGER NOT NULL DEFAULT 1,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            expires_at TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS strategy_allocations (
            id TEXT PRIMARY KEY,
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            asset TEXT NOT NULL,
            total_budget_usd REAL NOT NULL,
            deployed_usd REAL NOT NULL DEFAULT 0.0,
            available_usd REAL NOT NULL,
            profit_reserve_usd REAL NOT NULL DEFAULT 0.0,
            total_profit_usd REAL NOT NULL DEFAULT 0.0,
            status TEXT NOT NULL CHECK(status IN ('ACTIVE','PAUSED','COMPLETED')),
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS trade_lots (
            id TEXT PRIMARY KEY,
            allocation_id TEXT REFERENCES strategy_allocations(id),
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            pair TEXT NOT NULL,
            entry_price REAL NOT NULL,
            amount REAL NOT NULL,
            fee_paid_usd REAL NOT NULL,
            cost_basis_usd REAL NOT NULL,
            target_exit_price REAL NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('OPEN','COLD_HOLD','CLOSED')),
            peak_price REAL NOT NULL DEFAULT 0,
            opened_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            closed_at TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS agent_debates (
            id TEXT PRIMARY KEY,
            lot_id TEXT REFERENCES trade_lots(id),
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            market_context_json TEXT NOT NULL,
            bull_rationale TEXT NOT NULL,
            bear_rationale TEXT NOT NULL,
            arbiter_verdict TEXT NOT NULL,
            verdict_action TEXT NOT NULL CHECK(verdict_action IN ('BUY','PASS','DEFER')),
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS trade_executions (
            id TEXT PRIMARY KEY,
            lot_id TEXT REFERENCES trade_lots(id),
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            side TEXT NOT NULL CHECK(side IN ('BUY','SELL')),
            price REAL NOT NULL,
            amount REAL NOT NULL,
            fee_usd REAL NOT NULL,
            net_usd REAL NOT NULL,
            executed_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS trade_recommendations (
            id TEXT PRIMARY KEY,
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            side TEXT NOT NULL CHECK(side IN ('BUY','SELL')),
            symbol TEXT NOT NULL,
            lot_id TEXT REFERENCES trade_lots(id),
            recommended_price REAL NOT NULL,
            amount REAL NOT NULL,
            value_usd REAL NOT NULL,
            target_exit_price REAL,
            rationale TEXT NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('PENDING','EXECUTED','REJECTED','EXPIRED')),
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            executed_at TIMESTAMP,
            actual_price REAL,
            actual_amount REAL,
            fee_usd REAL,
            net_usd REAL,
            profit_usd REAL,
            user_note TEXT
        );

        CREATE TABLE IF NOT EXISTS backtest_runs (
            id TEXT PRIMARY KEY,
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            symbol TEXT NOT NULL,
            start_date TEXT NOT NULL,
            end_date TEXT NOT NULL,
            initial_capital_usd REAL NOT NULL,
            final_capital_usd REAL NOT NULL,
            total_trades INTEGER NOT NULL,
            wins INTEGER NOT NULL,
            losses INTEGER NOT NULL,
            pnl_usd REAL NOT NULL,
            pnl_pct REAL NOT NULL,
            max_drawdown_pct REAL NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS strategy_lessons (
            id TEXT PRIMARY KEY,
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            symbol TEXT NOT NULL,
            lesson TEXT NOT NULL,
            context_json TEXT NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS engine_configs (
            id TEXT PRIMARY KEY,
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            min_profit_margin REAL NOT NULL,
            taker_fee_rate REAL NOT NULL,
            tranche_count INTEGER NOT NULL,
            min_tranche_fraction REAL NOT NULL,
            max_spread_pct REAL NOT NULL,
            crash_drop_pct REAL NOT NULL,
            crash_window_secs INTEGER NOT NULL,
            ath_lookback_days INTEGER NOT NULL DEFAULT 30,
            max_buy_from_high_pct REAL NOT NULL DEFAULT 5.0,
            min_dip_from_high_pct REAL NOT NULL DEFAULT 0.0,
            max_runup_pct REAL NOT NULL DEFAULT 30.0,
            trend_filter TEXT NOT NULL DEFAULT 'OFF',
            trend_exit INTEGER NOT NULL DEFAULT 0,
            take_profit_pct REAL NOT NULL DEFAULT 0.15,
            trail_pct REAL NOT NULL DEFAULT 0,
            profit_mode TEXT NOT NULL DEFAULT 'REINVEST',
            rationale TEXT NOT NULL,
            source TEXT NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS strategy_profiles (
            name TEXT PRIMARY KEY,
            description TEXT NOT NULL,
            engine_config_json TEXT NOT NULL,
            default_constraints_json TEXT NOT NULL,
            source TEXT NOT NULL CHECK(source IN ('builtin','trained','manual')),
            score REAL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS training_stats (
            id TEXT PRIMARY KEY,
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            symbol TEXT NOT NULL,
            config_name TEXT NOT NULL,
            min_profit_margin REAL NOT NULL,
            taker_fee_rate REAL NOT NULL,
            tranche_count INTEGER NOT NULL,
            crash_drop_pct REAL NOT NULL,
            profit_mode TEXT NOT NULL,
            runs INTEGER NOT NULL,
            avg_pnl_pct REAL NOT NULL,
            avg_max_dd_pct REAL NOT NULL,
            win_rate REAL NOT NULL,
            cold_ratio REAL NOT NULL,
            profitable_ratio REAL NOT NULL,
            score REAL NOT NULL,
            best_window_pnl_pct REAL NOT NULL,
            worst_window_pnl_pct REAL NOT NULL,
            trained_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(mode, symbol, config_name)
        );

        CREATE TABLE IF NOT EXISTS regime_profiles (
            id TEXT PRIMARY KEY,
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            symbol TEXT NOT NULL,
            regime TEXT NOT NULL,
            config_name TEXT NOT NULL,
            engine_config_json TEXT NOT NULL,
            score REAL NOT NULL,
            avg_pnl_pct REAL NOT NULL,
            runs INTEGER NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(mode, symbol, regime)
        );

        CREATE TABLE IF NOT EXISTS seasonality_stats (
            symbol TEXT NOT NULL,
            month INTEGER NOT NULL CHECK(month BETWEEN 1 AND 12),
            avg_return_pct REAL NOT NULL,
            samples INTEGER NOT NULL,
            trained_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (symbol, month)
        );

        CREATE TABLE IF NOT EXISTS exchange_fees (
            mode TEXT PRIMARY KEY CHECK(mode IN ('LIVE','SIMULATION')),
            buy_fee_pct REAL NOT NULL DEFAULT 0.0,
            sell_fee_pct REAL NOT NULL DEFAULT 0.0,
            updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS trade_tasks (
            id TEXT PRIMARY KEY,
            mode TEXT NOT NULL CHECK(mode IN ('LIVE','SIMULATION')),
            name TEXT NOT NULL,
            assets TEXT NOT NULL,
            execution TEXT NOT NULL CHECK(execution IN ('MANUAL','AUTO')),
            monthly_budget_usd REAL NOT NULL,
            max_alerts_per_day INTEGER NOT NULL,
            tranche_count INTEGER NOT NULL,
            min_profit_margin REAL NOT NULL,
            status TEXT NOT NULL CHECK(status IN ('ACTIVE','PAUSED','COMPLETED')),
            month_spend_usd REAL NOT NULL DEFAULT 0.0,
            month_started_at TEXT NOT NULL,
            alerts_today INTEGER NOT NULL DEFAULT 0,
            alerts_date TEXT NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS experiments (
            id TEXT PRIMARY KEY,
            label TEXT NOT NULL,
            config_name TEXT NOT NULL,
            symbols TEXT NOT NULL,
            from_date TEXT NOT NULL,
            to_date TEXT NOT NULL,
            window_days INTEGER NOT NULL,
            step_days INTEGER NOT NULL,
            capitals TEXT NOT NULL,
            runs INTEGER NOT NULL,
            score REAL NOT NULL,
            avg_pnl_pct REAL NOT NULL,
            avg_max_dd_pct REAL NOT NULL,
            win_rate REAL NOT NULL,
            profitable_ratio REAL NOT NULL,
            cold_ratio REAL NOT NULL,
            windows_beaten_bh INTEGER NOT NULL DEFAULT 0,
            windows_total INTEGER NOT NULL DEFAULT 0,
            avg_pnl_vs_bh REAL NOT NULL DEFAULT 0.0,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS task_sim_runs (
            id TEXT PRIMARY KEY,
            label TEXT NOT NULL,
            symbols TEXT NOT NULL,
            start_date TEXT NOT NULL,
            end_date TEXT NOT NULL,
            monthly_budget_usd REAL NOT NULL,
            buy_fee_pct REAL NOT NULL,
            sell_fee_pct REAL NOT NULL,
            tranche_count INTEGER NOT NULL,
            min_profit_margin REAL NOT NULL,
            max_alerts_per_day INTEGER NOT NULL,
            signal TEXT NOT NULL,
            total_buys INTEGER NOT NULL,
            total_sells INTEGER NOT NULL,
            realized_profit_usd REAL NOT NULL,
            unrealized_profit_usd REAL NOT NULL,
            fees_paid_usd REAL NOT NULL,
            open_lots INTEGER NOT NULL,
            total_deployed_usd REAL NOT NULL,
            win_rate REAL NOT NULL,
            total_pnl_pct REAL NOT NULL,
            score REAL NOT NULL,
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        );

        CREATE TABLE IF NOT EXISTS fake_trades (
            id TEXT PRIMARY KEY,
            ts TEXT NOT NULL,
            symbol TEXT NOT NULL,
            side TEXT NOT NULL CHECK(side IN ('BUY','SELL')),
            price REAL NOT NULL,
            amount REAL NOT NULL,
            usd REAL NOT NULL,
            fee_usd REAL NOT NULL,
            pnl_usd REAL NOT NULL DEFAULT 0.0,
            note TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_fake_trades_ts ON fake_trades(ts);

        CREATE INDEX IF NOT EXISTS idx_constraints_mode_asset ON operator_constraints(mode, asset);
        CREATE INDEX IF NOT EXISTS idx_allocations_mode_status ON strategy_allocations(mode, status);
        CREATE INDEX IF NOT EXISTS idx_lots_mode_status ON trade_lots(mode, status);
        CREATE INDEX IF NOT EXISTS idx_debates_lot ON agent_debates(lot_id);
        CREATE INDEX IF NOT EXISTS idx_executions_lot ON trade_executions(lot_id);
        CREATE INDEX IF NOT EXISTS idx_recommendations_mode_status
            ON trade_recommendations(mode, status);
        CREATE INDEX IF NOT EXISTS idx_training_stats_symbol_score
            ON training_stats(mode, symbol, score DESC);
        CREATE INDEX IF NOT EXISTS idx_seasonality_symbol
            ON seasonality_stats(symbol);
        CREATE INDEX IF NOT EXISTS idx_experiments_created
            ON experiments(created_at DESC);
        "
    )?;

    // Lightweight migrations for DBs created before profit-mode columns
    // existed (new DBs already get them in CREATE TABLE above).
    ensure_column(
        conn,
        "strategy_allocations",
        "profit_reserve_usd",
        "REAL NOT NULL DEFAULT 0.0",
    )?;
    ensure_column(
        conn,
        "strategy_allocations",
        "total_profit_usd",
        "REAL NOT NULL DEFAULT 0.0",
    )?;
    ensure_column(
        conn,
        "engine_configs",
        "profit_mode",
        "TEXT NOT NULL DEFAULT 'REINVEST'",
    )?;
    ensure_column(
        conn,
        "engine_configs",
        "ath_lookback_days",
        "INTEGER NOT NULL DEFAULT 30",
    )?;
    ensure_column(
        conn,
        "engine_configs",
        "max_buy_from_high_pct",
        "REAL NOT NULL DEFAULT 5.0",
    )?;
    ensure_column(
        conn,
        "engine_configs",
        "min_dip_from_high_pct",
        "REAL NOT NULL DEFAULT 0.0",
    )?;
    ensure_column(
        conn,
        "engine_configs",
        "max_runup_pct",
        "REAL NOT NULL DEFAULT 30.0",
    )?;
    ensure_column(
        conn,
        "engine_configs",
        "trend_filter",
        "TEXT NOT NULL DEFAULT 'OFF'",
    )?;
    ensure_column(
        conn,
        "engine_configs",
        "trend_exit",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "engine_configs",
        "take_profit_pct",
        "REAL NOT NULL DEFAULT 0.15",
    )?;
    // Livepatch experiments: B&H benchmark columns (DBs created before the
    // experiment module existed lack them).
    ensure_column(
        conn,
        "experiments",
        "windows_beaten_bh",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "experiments",
        "windows_total",
        "INTEGER NOT NULL DEFAULT 0",
    )?;
    ensure_column(
        conn,
        "experiments",
        "avg_pnl_vs_bh",
        "REAL NOT NULL DEFAULT 0.0",
    )?;
    // Trailing-stop peak tracking (DBs created before the trail exit existed).
    ensure_column(conn, "trade_lots", "peak_price", "REAL NOT NULL DEFAULT 0")?;
    ensure_column(
        conn,
        "engine_configs",
        "trail_pct",
        "REAL NOT NULL DEFAULT 0",
    )?;
    Ok(())
}

/// Add `column` to `table` when it is missing (idempotent schema migration).
pub(crate) fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> rusqlite::Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt.query_map([], |r| r.get::<_, String>(1))?;
    let exists = cols
        .collect::<rusqlite::Result<Vec<String>>>()?
        .iter()
        .any(|c| c == column);
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
    }
    Ok(())
}
