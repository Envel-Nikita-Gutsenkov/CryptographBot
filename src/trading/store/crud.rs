use super::*;
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

impl TradingStore {
    /// Open (creating if needed) the database for `mode` in the current
    /// working directory. On first open of a missing database, a bundled seed
    /// file (see [`SEED_DB_FILE`]) is copied into place so the engine starts
    /// from trained profiles and lessons rather than a blank slate.
    pub fn open(mode: TradingMode) -> Result<Self> {
        let db_file = mode.db_file();
        if !Path::new(db_file).exists() {
            let seed = match std::env::var("TRADING_SEED_DB") {
                Ok(v) if !v.is_empty() => Some(v),
                Ok(_) => None, // explicitly disabled
                Err(_) => Some(SEED_DB_FILE.to_string()),
            };
            if let Some(seed) = seed {
                if Path::new(&seed).exists() {
                    std::fs::copy(&seed, db_file)?;
                }
            }
        }
        Self::open_path(db_file, mode)
    }

    /// Open (creating if needed) an arbitrary database file for `mode`.
    pub fn open_path(db_path: &str, mode: TradingMode) -> Result<Self> {
        if let Some(parent) = Path::new(db_path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(db_path)?;
        init_schema(&conn)?;
        Ok(Self {
            mode,
            db_path: db_path.to_string(),
            conn: Mutex::new(conn),
        })
    }

    /// Open a throwaway in-memory database for `mode`. Used by the backtest /
    /// training replay path, where every run is pure and must never touch disk
    /// (a full grid sweep replays tens of thousands of windows).
    pub fn open_in_memory(mode: TradingMode) -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        // Replay path is the engine as sole writer, so FK enforcement is pure
        // overhead here (and `reset_sim_state` between windows is hot). File
        // DBs keep FKs on — they see external writers (CLI, Telegram).
        conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
        init_schema(&conn)?;
        Ok(Self {
            mode,
            db_path: ":memory:".to_string(),
            conn: Mutex::new(conn),
        })
    }

    /// Wipe all simulation state so one in-memory store can be reused for many
    /// independent replays. Training sweeps replay tens of thousands of windows;
    /// recreating the store (schema init) per replay is the dominant fixed cost,
    /// so workers keep one store and reset it between windows. Delete order
    /// matters: child rows (debates/executions/recommendations -> trade_lots ->
    /// strategy_allocations) must go before their parents when foreign keys are
    /// enforced, so children are deleted first.
    pub fn reset_sim_state(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        // One transaction for all 17 DELETEs — per-statement autocommit was a
        // measurable fixed cost across the 60k-replay training sweep.
        conn.execute_batch(
            "BEGIN;
             DELETE FROM agent_debates;
             DELETE FROM trade_executions;
             DELETE FROM trade_recommendations;
             DELETE FROM trade_lots;
             DELETE FROM strategy_allocations;
             DELETE FROM operator_constraints;
             DELETE FROM backtest_runs;
             DELETE FROM strategy_lessons;
             DELETE FROM engine_configs;
             DELETE FROM strategy_profiles;
             DELETE FROM training_stats;
             DELETE FROM seasonality_stats;
             DELETE FROM exchange_fees;
             DELETE FROM trade_tasks;
             DELETE FROM experiments;
             DELETE FROM task_sim_runs;
             DELETE FROM fake_trades;
             COMMIT;",
        )?;
        Ok(())
    }

    pub fn mode(&self) -> TradingMode {
        self.mode
    }

    pub fn db_path(&self) -> &str {
        &self.db_path
    }

    // -- constraints --------------------------------------------------------

    pub fn add_constraint(&self, c: NewConstraint) -> Result<OperatorConstraint> {
        validate_positive("threshold_value", c.threshold_value)?;
        if c.asset.trim().is_empty() {
            bail!("asset must not be empty");
        }
        let id = new_id();
        let created_at = now_iso();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO operator_constraints
                (id, mode, asset, rule_type, threshold_value, raw_prompt, is_active, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8)",
            params![
                id,
                self.mode.as_str(),
                c.asset.to_uppercase(),
                c.rule_type.as_str(),
                c.threshold_value,
                c.raw_prompt,
                created_at,
                c.expires_at,
            ],
        )?;
        Ok(OperatorConstraint {
            id,
            mode: self.mode,
            asset: c.asset.to_uppercase(),
            rule_type: c.rule_type,
            threshold_value: c.threshold_value,
            raw_prompt: c.raw_prompt,
            is_active: true,
            created_at,
            expires_at: c.expires_at,
        })
    }

    pub fn list_constraints(
        &self,
        asset: Option<&str>,
        active_only: bool,
    ) -> Result<Vec<OperatorConstraint>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT id, mode, asset, rule_type, threshold_value, raw_prompt,
                    is_active, created_at, expires_at
             FROM operator_constraints WHERE mode = ?1",
        );
        if active_only {
            sql.push_str(" AND is_active = 1");
        }
        if asset.is_some() {
            sql.push_str(" AND asset = ?2");
        }
        sql.push_str(" ORDER BY created_at DESC");
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = if let Some(a) = asset {
            stmt.query_map(
                params![self.mode.as_str(), a.to_uppercase()],
                row_constraint,
            )?
        } else {
            stmt.query_map(params![self.mode.as_str()], row_constraint)?
        };
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Active constraints for `symbol` plus global `ALL` — one query for the
    /// engine hot path (was two `list_constraints` calls per buy-plan tick).
    pub fn list_constraints_for_symbols(&self, symbol: &str) -> Result<Vec<OperatorConstraint>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, mode, asset, rule_type, threshold_value, raw_prompt,
                    is_active, created_at, expires_at
             FROM operator_constraints
             WHERE mode = ?1 AND is_active = 1 AND asset IN ('ALL', ?2)
             ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(
            params![self.mode.as_str(), symbol.to_uppercase()],
            row_constraint,
        )?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    #[allow(dead_code)] // used by tests; planned Telegram /trade mirror
    pub fn get_constraint(&self, id: &str) -> Result<Option<OperatorConstraint>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, mode, asset, rule_type, threshold_value, raw_prompt,
                    is_active, created_at, expires_at
             FROM operator_constraints WHERE id = ?1 AND mode = ?2",
            params![id, self.mode.as_str()],
            row_constraint,
        )
        .optional()
        .map_err(Into::into)
    }

    #[allow(dead_code)] // used by tests; planned Telegram /trade mirror
    pub fn set_constraint_active(&self, id: &str, active: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE operator_constraints SET is_active = ?1 WHERE id = ?2 AND mode = ?3",
            params![active as i64, id, self.mode.as_str()],
        )?;
        if n == 0 {
            bail!("constraint {id} not found in mode {}", self.mode.as_str());
        }
        Ok(())
    }

    pub fn delete_constraint(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM operator_constraints WHERE id = ?1 AND mode = ?2",
            params![id, self.mode.as_str()],
        )?;
        if n == 0 {
            bail!("constraint {id} not found in mode {}", self.mode.as_str());
        }
        Ok(())
    }

    // -- allocations --------------------------------------------------------

    pub fn create_allocation(&self, a: NewAllocation) -> Result<StrategyAllocation> {
        validate_positive("total_budget_usd", a.total_budget_usd)?;
        if a.asset.trim().is_empty() {
            bail!("asset must not be empty");
        }
        let id = new_id();
        let created_at = now_iso();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO strategy_allocations
                (id, mode, asset, total_budget_usd, deployed_usd, available_usd,
                 profit_reserve_usd, total_profit_usd, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 0.0, ?4, 0.0, 0.0, 'ACTIVE', ?5)",
            params![
                id,
                self.mode.as_str(),
                a.asset.to_uppercase(),
                a.total_budget_usd,
                created_at,
            ],
        )?;
        Ok(StrategyAllocation {
            id,
            mode: self.mode,
            asset: a.asset.to_uppercase(),
            total_budget_usd: a.total_budget_usd,
            deployed_usd: 0.0,
            available_usd: a.total_budget_usd,
            profit_reserve_usd: 0.0,
            total_profit_usd: 0.0,
            status: AllocationStatus::Active,
            created_at,
        })
    }

    pub fn list_allocations(
        &self,
        status: Option<AllocationStatus>,
    ) -> Result<Vec<StrategyAllocation>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT id, mode, asset, total_budget_usd, deployed_usd, available_usd,
                    profit_reserve_usd, total_profit_usd, status, created_at
             FROM strategy_allocations WHERE mode = ?1",
        );
        if status.is_some() {
            sql.push_str(" AND status = ?2");
        }
        sql.push_str(" ORDER BY created_at DESC");
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = if let Some(s) = status {
            stmt.query_map(params![self.mode.as_str(), s.as_str()], row_allocation)?
        } else {
            stmt.query_map(params![self.mode.as_str()], row_allocation)?
        };
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_allocation(&self, id: &str) -> Result<Option<StrategyAllocation>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, mode, asset, total_budget_usd, deployed_usd, available_usd,
                    profit_reserve_usd, total_profit_usd, status, created_at
             FROM strategy_allocations WHERE id = ?1 AND mode = ?2",
            params![id, self.mode.as_str()],
            row_allocation,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Most recent active allocation for `asset` with free funds — single-row
    /// replacement for `list_allocations(Active)` + Rust-side `find`, which was
    /// a full scan on every buy-plan tick.
    pub fn get_active_allocation(&self, asset: &str) -> Result<Option<StrategyAllocation>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, mode, asset, total_budget_usd, deployed_usd, available_usd,
                    profit_reserve_usd, total_profit_usd, status, created_at
             FROM strategy_allocations
             WHERE mode = ?1 AND asset = ?2 AND status = 'ACTIVE' AND available_usd > 0
             ORDER BY created_at DESC LIMIT 1",
            params![self.mode.as_str(), asset.to_uppercase()],
            row_allocation,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Highest entry price among non-closed lots for `pair` — the no-average-up
    /// guard. Single-row MAX instead of a full `list_lots` scan per tick.
    pub fn last_entry_price(&self, pair: &str) -> Result<Option<f64>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT MAX(entry_price) FROM trade_lots
             WHERE mode = ?1 AND pair = ?2 AND status != 'CLOSED'",
            params![self.mode.as_str(), pair.to_uppercase()],
            |row| row.get::<_, Option<f64>>(0),
        )
        .map_err(Into::into)
    }

    pub fn set_allocation_status(&self, id: &str, status: AllocationStatus) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE strategy_allocations SET status = ?1 WHERE id = ?2 AND mode = ?3",
            params![status.as_str(), id, self.mode.as_str()],
        )?;
        if n == 0 {
            bail!("allocation {id} not found in mode {}", self.mode.as_str());
        }
        Ok(())
    }

    /// Move `usd` from available to deployed. Refuses to overdraw the pool
    /// (capital boundary invariant).
    #[allow(dead_code)] // used by tests; planned Telegram /trade mirror
    pub fn deploy_funds(&self, id: &str, usd: f64) -> Result<StrategyAllocation> {
        validate_non_negative("usd", usd)?;
        let conn = self.conn.lock().unwrap();
        let cur = get_allocation_conn(&conn, id, self.mode)?
            .ok_or_else(|| anyhow::anyhow!("allocation {id} not found"))?;
        if cur.available_usd + EPSILON < usd {
            bail!(
                "insufficient available funds: need {usd:.2} USD, only {:.2} USD left in allocation {id}",
                cur.available_usd
            );
        }
        conn.execute(
            "UPDATE strategy_allocations
                SET deployed_usd = deployed_usd + ?1, available_usd = available_usd - ?1
             WHERE id = ?2 AND mode = ?3",
            params![usd, id, self.mode.as_str()],
        )?;
        get_allocation_conn(&conn, id, self.mode)?
            .ok_or_else(|| anyhow::anyhow!("allocation {id} disappeared after deploy"))
    }

    /// Move `usd` from deployed back to available (e.g. after a lot closes).
    pub fn release_funds(&self, id: &str, usd: f64) -> Result<StrategyAllocation> {
        validate_non_negative("usd", usd)?;
        let conn = self.conn.lock().unwrap();
        let cur = get_allocation_conn(&conn, id, self.mode)?
            .ok_or_else(|| anyhow::anyhow!("allocation {id} not found"))?;
        if cur.deployed_usd + EPSILON < usd {
            bail!(
                "cannot release {usd:.2} USD: only {:.2} USD deployed in allocation {id}",
                cur.deployed_usd
            );
        }
        conn.execute(
            "UPDATE strategy_allocations
                SET deployed_usd = deployed_usd - ?1, available_usd = available_usd + ?1
             WHERE id = ?2 AND mode = ?3",
            params![usd, id, self.mode.as_str()],
        )?;
        get_allocation_conn(&conn, id, self.mode)?
            .ok_or_else(|| anyhow::anyhow!("allocation {id} disappeared after release"))
    }

    /// Credit realized profit back into an allocation's working pool
    /// (compounding, `ProfitMode::Reinvest`). Increases `total_budget_usd` and
    /// `available_usd`; `deployed_usd` unchanged. Cumulative realized profit
    /// is tracked in `total_profit_usd`.
    pub fn credit_profit(&self, id: &str, usd: f64) -> Result<StrategyAllocation> {
        validate_non_negative("usd", usd)?;
        let conn = self.conn.lock().unwrap();
        get_allocation_conn(&conn, id, self.mode)?
            .ok_or_else(|| anyhow::anyhow!("allocation {id} not found"))?;
        conn.execute(
            "UPDATE strategy_allocations
                SET total_budget_usd = total_budget_usd + ?1,
                    available_usd = available_usd + ?1,
                    total_profit_usd = total_profit_usd + ?1
             WHERE id = ?2 AND mode = ?3",
            params![usd, id, self.mode.as_str()],
        )?;
        get_allocation_conn(&conn, id, self.mode)?
            .ok_or_else(|| anyhow::anyhow!("allocation {id} disappeared after credit"))
    }

    /// Credit realized profit into the allocation's separate reserve
    /// (`ProfitMode::Separate` / `Partial`). Working capital is untouched —
    /// the reserve is never deployed into new buys. Cumulative realized
    /// profit is tracked in `total_profit_usd`.
    pub fn credit_profit_reserve(&self, id: &str, usd: f64) -> Result<StrategyAllocation> {
        validate_non_negative("usd", usd)?;
        let conn = self.conn.lock().unwrap();
        get_allocation_conn(&conn, id, self.mode)?
            .ok_or_else(|| anyhow::anyhow!("allocation {id} not found"))?;
        conn.execute(
            "UPDATE strategy_allocations
                SET profit_reserve_usd = profit_reserve_usd + ?1,
                    total_profit_usd = total_profit_usd + ?1
             WHERE id = ?2 AND mode = ?3",
            params![usd, id, self.mode.as_str()],
        )?;
        get_allocation_conn(&conn, id, self.mode)?
            .ok_or_else(|| anyhow::anyhow!("allocation {id} disappeared after credit"))
    }

    pub fn delete_allocation(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM strategy_allocations WHERE id = ?1 AND mode = ?2",
            params![id, self.mode.as_str()],
        )?;
        if n == 0 {
            bail!("allocation {id} not found in mode {}", self.mode.as_str());
        }
        Ok(())
    }

    // -- lots ---------------------------------------------------------------

    /// Open a new trade lot. `cost_basis` is computed here as
    /// `entry_price * amount + fee_paid_usd` and stored immutably.
    ///
    /// If `allocation_id` is set, the cost basis is atomically deployed from
    /// that pool (fails if the pool cannot cover it).
    pub fn open_lot(&self, l: NewLot) -> Result<TradeLot> {
        validate_positive("entry_price", l.entry_price)?;
        validate_positive("amount", l.amount)?;
        validate_non_negative("fee_paid_usd", l.fee_paid_usd)?;
        validate_positive("target_exit_price", l.target_exit_price)?;
        if l.pair.trim().is_empty() {
            bail!("pair must not be empty");
        }
        let mut cost_basis = match l.cost_basis_usd {
            Some(cb) => cb,
            None => round_to_8(l.entry_price * l.amount + l.fee_paid_usd),
        };
        let id = new_id();
        let opened_at = now_iso();

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        if let Some(aid) = &l.allocation_id {
            let alloc = get_allocation_conn(&tx, aid, self.mode)?
                .ok_or_else(|| anyhow::anyhow!("allocation {aid} not found"))?;
            if alloc.available_usd + EPSILON < cost_basis {
                // Absorb floating-point rounding in the amount→cost-basis
                // recomputation (bounded by ~1e-6 USD): clamp the stored cost
                // basis to the pool so deploy/release stay exactly balanced.
                // Anything larger is a genuine overdraw and is rejected.
                let diff = cost_basis - alloc.available_usd;
                if diff <= 1e-6 {
                    cost_basis = alloc.available_usd;
                } else {
                    bail!(
                        "insufficient available funds: lot costs {cost_basis:.2} USD, only {:.2} USD left in allocation {aid}",
                        alloc.available_usd
                    );
                }
            }
            tx.execute(
                "UPDATE strategy_allocations
                    SET deployed_usd = deployed_usd + ?1, available_usd = available_usd - ?1
                 WHERE id = ?2 AND mode = ?3",
                params![cost_basis, aid, self.mode.as_str()],
            )?;
        }
        tx.execute(
            "INSERT INTO trade_lots
                (id, allocation_id, mode, pair, entry_price, amount, fee_paid_usd,
                 cost_basis_usd, target_exit_price, status, peak_price, opened_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'OPEN', ?10, ?11)",
            params![
                id,
                l.allocation_id,
                self.mode.as_str(),
                l.pair.to_uppercase(),
                l.entry_price,
                l.amount,
                l.fee_paid_usd,
                cost_basis,
                l.target_exit_price,
                l.peak_price.unwrap_or(l.entry_price),
                opened_at,
            ],
        )?;
        tx.commit()?;
        Ok(TradeLot {
            id,
            allocation_id: l.allocation_id,
            mode: self.mode,
            pair: l.pair.to_uppercase(),
            entry_price: l.entry_price,
            amount: l.amount,
            fee_paid_usd: l.fee_paid_usd,
            cost_basis_usd: cost_basis,
            target_exit_price: l.target_exit_price,
            status: LotStatus::Open,
            peak_price: l.peak_price.unwrap_or(l.entry_price),
            opened_at,
            closed_at: None,
        })
    }

    pub fn list_lots(&self, status: Option<LotStatus>) -> Result<Vec<TradeLot>> {
        let conn = self.conn.lock().unwrap();
        let mut sql = String::from(
            "SELECT id, allocation_id, mode, pair, entry_price, amount, fee_paid_usd,
                    cost_basis_usd, target_exit_price, status, opened_at, closed_at, peak_price
             FROM trade_lots WHERE mode = ?1",
        );
        if status.is_some() {
            sql.push_str(" AND status = ?2");
        }
        sql.push_str(" ORDER BY opened_at DESC");
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = if let Some(s) = status {
            stmt.query_map(params![self.mode.as_str(), s.as_str()], row_lot)?
        } else {
            stmt.query_map(params![self.mode.as_str()], row_lot)?
        };
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Non-closed lots only — the per-tick hot path for the engine. The engine
    /// skips `Closed` rows anyway, so filtering in SQL halves the rows and
    /// avoids allocating `TradeLot` for finished lots on every tick.
    pub fn list_lots_active(&self) -> Result<Vec<TradeLot>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, allocation_id, mode, pair, entry_price, amount, fee_paid_usd,
                    cost_basis_usd, target_exit_price, status, opened_at, closed_at, peak_price
             FROM trade_lots WHERE mode = ?1 AND status != 'CLOSED'
             ORDER BY opened_at DESC",
        )?;
        let rows = stmt.query_map(params![self.mode.as_str()], row_lot)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Non-closed lots for one pair only — the engine's per-tick sell check.
    /// Filtering by pair in SQL avoids scanning other symbols' lots on every
    /// tick (and the engine only ever manages the tick's own symbol).
    pub fn list_lots_active_for(&self, pair: &str) -> Result<Vec<TradeLot>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT id, allocation_id, mode, pair, entry_price, amount, fee_paid_usd,
                    cost_basis_usd, target_exit_price, status, opened_at, closed_at, peak_price
             FROM trade_lots
             WHERE mode = ?1 AND status != 'CLOSED' AND pair = ?2
             ORDER BY opened_at DESC",
        )?;
        let rows = stmt.query_map(params![self.mode.as_str(), pair.to_uppercase()], row_lot)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_lot(&self, id: &str) -> Result<Option<TradeLot>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, allocation_id, mode, pair, entry_price, amount, fee_paid_usd,
                    cost_basis_usd, target_exit_price, status, opened_at, closed_at, peak_price
             FROM trade_lots WHERE id = ?1 AND mode = ?2",
            params![id, self.mode.as_str()],
            row_lot,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn set_lot_status(&self, id: &str, status: LotStatus) -> Result<TradeLot> {
        let conn = self.conn.lock().unwrap();
        let closed_at = if status == LotStatus::Closed {
            Some(now_iso())
        } else {
            None
        };
        let n = conn.execute(
            "UPDATE trade_lots SET status = ?1, closed_at = ?2 WHERE id = ?3 AND mode = ?4",
            params![status.as_str(), closed_at, id, self.mode.as_str()],
        )?;
        if n == 0 {
            bail!("lot {id} not found in mode {}", self.mode.as_str());
        }
        get_lot_conn(&conn, id, self.mode)?
            .ok_or_else(|| anyhow::anyhow!("lot {id} disappeared after status update"))
    }

    /// Convenience: mark a lot CLOSED (sets `closed_at`).
    pub fn close_lot(&self, id: &str) -> Result<TradeLot> {
        self.set_lot_status(id, LotStatus::Closed)
    }

    /// Raise the trailing-stop peak for an open lot (never lowers it).
    pub fn update_lot_peak(&self, id: &str, price: f64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE trade_lots SET peak_price = MAX(peak_price, ?1)
             WHERE id = ?2 AND mode = ?3 AND status != 'CLOSED'",
            params![price, id, self.mode.as_str()],
        )?;
        Ok(())
    }

    // -- debates ------------------------------------------------------------

    #[allow(dead_code)] // used by tests; planned AI debate room
    pub fn record_debate(&self, d: NewDebate) -> Result<AgentDebate> {
        let id = new_id();
        let created_at = now_iso();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO agent_debates
                (id, lot_id, mode, market_context_json, bull_rationale, bear_rationale,
                 arbiter_verdict, verdict_action, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                d.lot_id,
                self.mode.as_str(),
                d.market_context_json,
                d.bull_rationale,
                d.bear_rationale,
                d.arbiter_verdict,
                d.verdict_action.as_str(),
                created_at,
            ],
        )?;
        Ok(AgentDebate {
            id,
            lot_id: d.lot_id,
            mode: self.mode,
            market_context_json: d.market_context_json,
            bull_rationale: d.bull_rationale,
            bear_rationale: d.bear_rationale,
            arbiter_verdict: d.arbiter_verdict,
            verdict_action: d.verdict_action,
            created_at,
        })
    }

    pub fn list_debates(&self, limit: i64) -> Result<Vec<AgentDebate>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, lot_id, mode, market_context_json, bull_rationale, bear_rationale,
                    arbiter_verdict, verdict_action, created_at
             FROM agent_debates WHERE mode = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![self.mode.as_str(), limit], row_debate)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    // -- executions ---------------------------------------------------------

    pub fn record_execution(&self, e: NewExecution) -> Result<TradeExecution> {
        validate_positive("price", e.price)?;
        validate_positive("amount", e.amount)?;
        validate_non_negative("fee_usd", e.fee_usd)?;
        let id = new_id();
        let executed_at = now_iso();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO trade_executions
                (id, lot_id, mode, side, price, amount, fee_usd, net_usd, executed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                e.lot_id,
                self.mode.as_str(),
                e.side.as_str(),
                e.price,
                e.amount,
                e.fee_usd,
                e.net_usd,
                executed_at,
            ],
        )?;
        Ok(TradeExecution {
            id,
            lot_id: e.lot_id,
            mode: self.mode,
            side: e.side,
            price: e.price,
            amount: e.amount,
            fee_usd: e.fee_usd,
            net_usd: e.net_usd,
            executed_at,
        })
    }

    pub fn list_executions(&self, limit: i64) -> Result<Vec<TradeExecution>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, lot_id, mode, side, price, amount, fee_usd, net_usd, executed_at
             FROM trade_executions WHERE mode = ?1 ORDER BY executed_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![self.mode.as_str(), limit], row_execution)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    // -- recommendations (commander/advice mode) ----------------------------

    /// Persist a proposed trade. The operator later reports the outcome via
    /// [`Self::mark_recommendation_executed`] / `..._rejected` / `..._expired`.
    pub fn record_recommendation(&self, r: NewRecommendation) -> Result<TradeRecommendation> {
        validate_positive("recommended_price", r.recommended_price)?;
        validate_positive("amount", r.amount)?;
        validate_positive("value_usd", r.value_usd)?;
        if r.symbol.trim().is_empty() {
            bail!("symbol must not be empty");
        }
        let id = new_id();
        let created_at = now_iso();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO trade_recommendations
                (id, mode, side, symbol, lot_id, recommended_price, amount, value_usd,
                 target_exit_price, rationale, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'PENDING', ?11)",
            params![
                id,
                self.mode.as_str(),
                r.side.as_str(),
                r.symbol.to_uppercase(),
                r.lot_id,
                r.recommended_price,
                r.amount,
                r.value_usd,
                r.target_exit_price,
                r.rationale,
                created_at,
            ],
        )?;
        Ok(TradeRecommendation {
            id,
            mode: self.mode,
            side: r.side,
            symbol: r.symbol.to_uppercase(),
            lot_id: r.lot_id,
            recommended_price: r.recommended_price,
            amount: r.amount,
            value_usd: r.value_usd,
            target_exit_price: r.target_exit_price,
            rationale: r.rationale,
            status: RecommendationStatus::Pending,
            created_at,
            executed_at: None,
            actual_price: None,
            actual_amount: None,
            fee_usd: None,
            net_usd: None,
            profit_usd: None,
            user_note: None,
        })
    }

    pub fn list_recommendations(
        &self,
        limit: i64,
        status: Option<RecommendationStatus>,
    ) -> Result<Vec<TradeRecommendation>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match status {
            Some(_) => conn.prepare(
                "SELECT id, mode, side, symbol, lot_id, recommended_price, amount, value_usd,
                        target_exit_price, rationale, status, created_at, executed_at,
                        actual_price, actual_amount, fee_usd, net_usd, profit_usd, user_note
                 FROM trade_recommendations
                 WHERE mode = ?1 AND status = ?2
                 ORDER BY created_at DESC LIMIT ?3",
            )?,
            None => conn.prepare(
                "SELECT id, mode, side, symbol, lot_id, recommended_price, amount, value_usd,
                        target_exit_price, rationale, status, created_at, executed_at,
                        actual_price, actual_amount, fee_usd, net_usd, profit_usd, user_note
                 FROM trade_recommendations
                 WHERE mode = ?1
                 ORDER BY created_at DESC LIMIT ?2",
            )?,
        };
        let rows = match status {
            Some(s) => stmt.query_map(
                params![self.mode.as_str(), s.as_str(), limit],
                row_recommendation,
            )?,
            None => stmt.query_map(params![self.mode.as_str(), limit], row_recommendation)?,
        };
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_recommendation(&self, id: &str) -> Result<Option<TradeRecommendation>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, mode, side, symbol, lot_id, recommended_price, amount, value_usd,
                    target_exit_price, rationale, status, created_at, executed_at,
                    actual_price, actual_amount, fee_usd, net_usd, profit_usd, user_note
             FROM trade_recommendations WHERE id = ?1 AND mode = ?2",
            params![id, self.mode.as_str()],
            row_recommendation,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Record that the operator executed the recommendation on their own
    /// wallet. Only `PENDING` recommendations may transition; the reported
    /// fill is stored verbatim (the caller — the CLI — already re-ran the
    /// engine's guardrails at the actual price before calling this).
    ///
    /// `lot_id` is COALESCEd: for a SELL rec the lot is already linked at
    /// creation; for a BUY rec the caller passes the lot opened on execution.
    pub fn mark_recommendation_executed(
        &self,
        id: &str,
        lot_id: Option<String>,
        actual_price: f64,
        actual_amount: f64,
        fee_usd: f64,
        net_usd: f64,
        profit_usd: f64,
        user_note: Option<String>,
    ) -> Result<TradeRecommendation> {
        validate_positive("actual_price", actual_price)?;
        validate_positive("actual_amount", actual_amount)?;
        validate_non_negative("fee_usd", fee_usd)?;
        let rec = self
            .get_recommendation(id)?
            .ok_or_else(|| anyhow::anyhow!("recommendation {id} not found"))?;
        if rec.status != RecommendationStatus::Pending {
            bail!(
                "recommendation {id} is {} — only PENDING can be executed",
                rec.status.as_str()
            );
        }
        let executed_at = now_iso();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE trade_recommendations
             SET status = 'EXECUTED', executed_at = ?1, lot_id = COALESCE(?2, lot_id),
                 actual_price = ?3, actual_amount = ?4, fee_usd = ?5, net_usd = ?6,
                 profit_usd = ?7, user_note = ?8
             WHERE id = ?9",
            params![
                executed_at,
                lot_id,
                actual_price,
                actual_amount,
                fee_usd,
                net_usd,
                profit_usd,
                user_note,
                id
            ],
        )?;
        Ok(TradeRecommendation {
            status: RecommendationStatus::Executed,
            lot_id: lot_id.or(rec.lot_id),
            executed_at: Some(executed_at),
            actual_price: Some(actual_price),
            actual_amount: Some(actual_amount),
            fee_usd: Some(fee_usd),
            net_usd: Some(net_usd),
            profit_usd: Some(profit_usd),
            user_note,
            ..rec
        })
    }

    /// Record that the operator declined the recommendation.
    pub fn mark_recommendation_rejected(
        &self,
        id: &str,
        user_note: Option<String>,
    ) -> Result<TradeRecommendation> {
        let rec = self
            .get_recommendation(id)?
            .ok_or_else(|| anyhow::anyhow!("recommendation {id} not found"))?;
        if rec.status != RecommendationStatus::Pending {
            bail!(
                "recommendation {id} is {} — only PENDING can be rejected",
                rec.status.as_str()
            );
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE trade_recommendations SET status = 'REJECTED', user_note = ?1 WHERE id = ?2",
            params![user_note, id],
        )?;
        Ok(TradeRecommendation {
            status: RecommendationStatus::Rejected,
            user_note,
            ..rec
        })
    }

    /// Mark a stale PENDING recommendation as expired (no longer actionable).
    pub fn mark_recommendation_expired(&self, id: &str) -> Result<TradeRecommendation> {
        let rec = self
            .get_recommendation(id)?
            .ok_or_else(|| anyhow::anyhow!("recommendation {id} not found"))?;
        if rec.status != RecommendationStatus::Pending {
            bail!(
                "recommendation {id} is {} — only PENDING can be expired",
                rec.status.as_str()
            );
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE trade_recommendations SET status = 'EXPIRED' WHERE id = ?1",
            params![id],
        )?;
        Ok(TradeRecommendation {
            status: RecommendationStatus::Expired,
            ..rec
        })
    }

    // -- backtests ----------------------------------------------------------

    pub fn record_backtest(&self, b: NewBacktest) -> Result<BacktestRun> {
        validate_non_negative("initial_capital_usd", b.initial_capital_usd)?;
        validate_non_negative("final_capital_usd", b.final_capital_usd)?;
        let id = new_id();
        let created_at = now_iso();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO backtest_runs
                (id, mode, symbol, start_date, end_date, initial_capital_usd,
                 final_capital_usd, total_trades, wins, losses, pnl_usd, pnl_pct,
                 max_drawdown_pct, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                id,
                self.mode.as_str(),
                b.symbol.to_uppercase(),
                b.start_date,
                b.end_date,
                b.initial_capital_usd,
                b.final_capital_usd,
                b.total_trades,
                b.wins,
                b.losses,
                b.pnl_usd,
                b.pnl_pct,
                b.max_drawdown_pct,
                created_at,
            ],
        )?;
        Ok(BacktestRun {
            id,
            mode: self.mode,
            symbol: b.symbol.to_uppercase(),
            start_date: b.start_date,
            end_date: b.end_date,
            initial_capital_usd: b.initial_capital_usd,
            final_capital_usd: b.final_capital_usd,
            total_trades: b.total_trades,
            wins: b.wins,
            losses: b.losses,
            pnl_usd: b.pnl_usd,
            pnl_pct: b.pnl_pct,
            max_drawdown_pct: b.max_drawdown_pct,
            created_at,
        })
    }

    pub fn list_backtests(&self, limit: i64) -> Result<Vec<BacktestRun>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, mode, symbol, start_date, end_date, initial_capital_usd,
                    final_capital_usd, total_trades, wins, losses, pnl_usd, pnl_pct,
                    max_drawdown_pct, created_at
             FROM backtest_runs WHERE mode = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![self.mode.as_str(), limit], row_backtest)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    // -- lessons ------------------------------------------------------------

    pub fn record_lesson(&self, l: NewLesson) -> Result<StrategyLesson> {
        if l.lesson.trim().is_empty() {
            bail!("lesson must not be empty");
        }
        let id = new_id();
        let created_at = now_iso();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO strategy_lessons (id, mode, symbol, lesson, context_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                id,
                self.mode.as_str(),
                l.symbol.to_uppercase(),
                l.lesson,
                l.context_json,
                created_at,
            ],
        )?;
        Ok(StrategyLesson {
            id,
            mode: self.mode,
            symbol: l.symbol.to_uppercase(),
            lesson: l.lesson,
            context_json: l.context_json,
            created_at,
        })
    }

    pub fn list_lessons(&self, limit: i64) -> Result<Vec<StrategyLesson>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, mode, symbol, lesson, context_json, created_at
             FROM strategy_lessons WHERE mode = ?1 ORDER BY created_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![self.mode.as_str(), limit], row_lesson)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    // -- engine configs (auto-tuner history) ------------------------------

    /// Persist a validated `EngineConfig` snapshot. Validation goes through the
    /// engine's own `EngineConfig::validate`, so garbage can never be stored.
    pub fn record_engine_config(&self, c: NewEngineConfig) -> Result<EngineConfigRecord> {
        let profit_mode = crate::trading::engine::ProfitMode::parse(&c.profit_mode)
            .ok_or_else(|| anyhow::anyhow!("invalid profit_mode '{}'", c.profit_mode))?;
        let trend_filter = crate::trading::engine::TrendFilter::parse(&c.trend_filter)
            .ok_or_else(|| anyhow::anyhow!("invalid trend_filter '{}'", c.trend_filter))?;
        let cfg = crate::trading::engine::EngineConfig {
            min_profit_margin: c.min_profit_margin,
            taker_fee_rate: c.taker_fee_rate,
            tranche_count: c.tranche_count,
            min_tranche_fraction: c.min_tranche_fraction,
            max_spread_pct: c.max_spread_pct,
            crash_drop_pct: c.crash_drop_pct,
            crash_window_secs: c.crash_window_secs,
            ath_lookback_days: c.ath_lookback_days,
            max_buy_from_high_pct: c.max_buy_from_high_pct,
            min_dip_from_high_pct: c.min_dip_from_high_pct,
            max_runup_pct: c.max_runup_pct,
            trend_filter,
            trend_exit: c.trend_exit,
            take_profit_pct: c.take_profit_pct,
            trail_pct: c.trail_pct,
            profit_mode,
        };
        cfg.validate()?;
        if c.rationale.trim().is_empty() {
            bail!("rationale must not be empty");
        }
        if c.source.trim().is_empty() {
            bail!("source must not be empty");
        }
        let id = new_id();
        let created_at = now_iso();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO engine_configs
                (id, mode, min_profit_margin, taker_fee_rate, tranche_count,
                 min_tranche_fraction, max_spread_pct, crash_drop_pct, crash_window_secs,
                 ath_lookback_days, max_buy_from_high_pct, min_dip_from_high_pct, max_runup_pct,
                 trend_filter, trend_exit, take_profit_pct, trail_pct, profit_mode, rationale, source, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
            params![
                id,
                self.mode.as_str(),
                cfg.min_profit_margin,
                cfg.taker_fee_rate,
                cfg.tranche_count as i64,
                cfg.min_tranche_fraction,
                cfg.max_spread_pct,
                cfg.crash_drop_pct,
                cfg.crash_window_secs,
                cfg.ath_lookback_days,
                cfg.max_buy_from_high_pct,
                cfg.min_dip_from_high_pct,
                cfg.max_runup_pct,
                cfg.trend_filter.as_str(),
                cfg.trend_exit as i64,
                cfg.take_profit_pct,
                cfg.trail_pct,
                cfg.profit_mode.as_str(),
                c.rationale,
                c.source,
                created_at,
            ],
        )?;
        Ok(EngineConfigRecord {
            id,
            mode: self.mode,
            min_profit_margin: cfg.min_profit_margin,
            taker_fee_rate: cfg.taker_fee_rate,
            tranche_count: cfg.tranche_count,
            min_tranche_fraction: cfg.min_tranche_fraction,
            max_spread_pct: cfg.max_spread_pct,
            crash_drop_pct: cfg.crash_drop_pct,
            crash_window_secs: cfg.crash_window_secs,
            ath_lookback_days: cfg.ath_lookback_days,
            max_buy_from_high_pct: cfg.max_buy_from_high_pct,
            min_dip_from_high_pct: cfg.min_dip_from_high_pct,
            max_runup_pct: cfg.max_runup_pct,
            trend_filter: cfg.trend_filter.as_str().to_string(),
            trend_exit: cfg.trend_exit,
            take_profit_pct: cfg.take_profit_pct,
            trail_pct: cfg.trail_pct,
            profit_mode: cfg.profit_mode.as_str(),
            rationale: c.rationale,
            source: c.source,
            created_at,
        })
    }

    pub fn list_engine_configs(&self, limit: i64) -> Result<Vec<EngineConfigRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, mode, min_profit_margin, taker_fee_rate, tranche_count,
                    min_tranche_fraction, max_spread_pct, crash_drop_pct, crash_window_secs,
                    ath_lookback_days, max_buy_from_high_pct, min_dip_from_high_pct, max_runup_pct,
                    trend_filter, trend_exit, take_profit_pct, trail_pct, profit_mode, rationale, source, created_at
             FROM engine_configs WHERE mode = ?1 ORDER BY created_at DESC, rowid DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![self.mode.as_str(), limit], row_engine_config)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// The most recent config (the active one). `None` when the engine has
    /// never been tuned — callers then use `EngineConfig::default()`.
    pub fn latest_engine_config(&self) -> Result<Option<EngineConfigRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, mode, min_profit_margin, taker_fee_rate, tranche_count,
                    min_tranche_fraction, max_spread_pct, crash_drop_pct, crash_window_secs,
                    ath_lookback_days, max_buy_from_high_pct, min_dip_from_high_pct, max_runup_pct,
                    trend_filter, trend_exit, take_profit_pct, trail_pct, profit_mode, rationale, source, created_at
             FROM engine_configs WHERE mode = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![self.mode.as_str()], row_engine_config)?;
        rows.next().transpose().map_err(Into::into)
    }

    // -- strategy profiles (plan §8) ----------------------------------------

    /// Insert or replace a strategy profile (name is the natural key).
    pub fn record_strategy_profile(&self, r: &StrategyProfileRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO strategy_profiles
                (name, description, engine_config_json, default_constraints_json,
                 source, score, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, CURRENT_TIMESTAMP)
             ON CONFLICT(name) DO UPDATE SET
                 description = excluded.description,
                 engine_config_json = excluded.engine_config_json,
                 default_constraints_json = excluded.default_constraints_json,
                 source = excluded.source,
                 score = excluded.score",
            params![
                r.name,
                r.description,
                r.engine_config_json,
                r.default_constraints_json,
                r.source,
                r.score
            ],
        )?;
        Ok(())
    }

    pub fn list_strategy_profiles(&self) -> Result<Vec<StrategyProfileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, description, engine_config_json, default_constraints_json,
                    source, score, created_at
             FROM strategy_profiles ORDER BY source, name",
        )?;
        let rows = stmt.query_map([], row_strategy_profile)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn get_strategy_profile(&self, name: &str) -> Result<Option<StrategyProfileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name, description, engine_config_json, default_constraints_json,
                    source, score, created_at
             FROM strategy_profiles WHERE name = ?1",
        )?;
        let mut rows = stmt.query_map(params![name], row_strategy_profile)?;
        rows.next().transpose().map_err(Into::into)
    }

    /// Delete a profile; returns `true` when a row was actually removed.
    pub fn delete_strategy_profile(&self, name: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM strategy_profiles WHERE name = ?1",
            params![name],
        )?;
        Ok(n > 0)
    }

    // -- training stats (per-symbol sweep aggregates) ----------------------

    /// Upsert one (symbol × config) aggregate row from a training sweep.
    /// Re-running with `--force` overwrites the previous sweep for that pair.
    pub fn record_training_stats(&self, s: &NewTrainingStats) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO training_stats
                (id, mode, symbol, config_name, min_profit_margin, taker_fee_rate,
                 tranche_count, crash_drop_pct, profit_mode, runs, avg_pnl_pct,
                 avg_max_dd_pct, win_rate, cold_ratio, profitable_ratio, score,
                 best_window_pnl_pct, worst_window_pnl_pct, trained_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                     ?15, ?16, ?17, ?18, CURRENT_TIMESTAMP)
             ON CONFLICT(mode, symbol, config_name) DO UPDATE SET
                 min_profit_margin = excluded.min_profit_margin,
                 taker_fee_rate = excluded.taker_fee_rate,
                 tranche_count = excluded.tranche_count,
                 crash_drop_pct = excluded.crash_drop_pct,
                 profit_mode = excluded.profit_mode,
                 runs = excluded.runs,
                 avg_pnl_pct = excluded.avg_pnl_pct,
                 avg_max_dd_pct = excluded.avg_max_dd_pct,
                 win_rate = excluded.win_rate,
                 cold_ratio = excluded.cold_ratio,
                 profitable_ratio = excluded.profitable_ratio,
                 score = excluded.score,
                 best_window_pnl_pct = excluded.best_window_pnl_pct,
                 worst_window_pnl_pct = excluded.worst_window_pnl_pct,
                 trained_at = CURRENT_TIMESTAMP",
            params![
                new_id(),
                self.mode.as_str(),
                s.symbol.to_uppercase(),
                s.config_name,
                s.min_profit_margin,
                s.taker_fee_rate,
                s.tranche_count as i64,
                s.crash_drop_pct,
                s.profit_mode,
                s.runs as i64,
                s.avg_pnl_pct,
                s.avg_max_dd_pct,
                s.win_rate,
                s.cold_ratio,
                s.profitable_ratio,
                s.score,
                s.best_window_pnl_pct,
                s.worst_window_pnl_pct,
            ],
        )?;
        Ok(())
    }

    /// Top-scoring configs for one symbol (or all symbols when `symbol` is
    /// empty), ordered by composite score descending.
    pub fn list_training_stats(&self, symbol: &str, limit: i64) -> Result<Vec<TrainingStatsRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = if symbol.is_empty() {
            conn.prepare(
                "SELECT id, mode, symbol, config_name, min_profit_margin, taker_fee_rate,
                        tranche_count, crash_drop_pct, profit_mode, runs, avg_pnl_pct,
                        avg_max_dd_pct, win_rate, cold_ratio, profitable_ratio, score,
                        best_window_pnl_pct, worst_window_pnl_pct, trained_at
                 FROM training_stats WHERE mode = ?1
                 ORDER BY score DESC LIMIT ?2",
            )?
        } else {
            conn.prepare(
                "SELECT id, mode, symbol, config_name, min_profit_margin, taker_fee_rate,
                        tranche_count, crash_drop_pct, profit_mode, runs, avg_pnl_pct,
                        avg_max_dd_pct, win_rate, cold_ratio, profitable_ratio, score,
                        best_window_pnl_pct, worst_window_pnl_pct, trained_at
                 FROM training_stats WHERE mode = ?1 AND symbol = ?2
                 ORDER BY score DESC LIMIT ?3",
            )?
        };
        let rows = if symbol.is_empty() {
            stmt.query_map(params![self.mode.as_str(), limit], row_training_stats)?
        } else {
            stmt.query_map(
                params![self.mode.as_str(), symbol.to_uppercase(), limit],
                row_training_stats,
            )?
        };
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Drop all sweep rows for this mode (used by `--force` retrains so a stale
    /// grid never shadows the fresh one).
    pub fn clear_training_stats(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM training_stats WHERE mode = ?1",
            params![self.mode.as_str()],
        )?;
        Ok(())
    }

    // -- regime profiles (per-symbol per-regime winners) ---------------------

    /// Upsert the best-scoring config for one (symbol × regime) bucket.
    /// Re-running a sweep with `--force` replaces the previous winners.
    pub fn record_regime_profile(&self, r: &NewRegimeProfile) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO regime_profiles
                (id, mode, symbol, regime, config_name, engine_config_json,
                 score, avg_pnl_pct, runs, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, CURRENT_TIMESTAMP)
             ON CONFLICT(mode, symbol, regime) DO UPDATE SET
                 config_name = excluded.config_name,
                 engine_config_json = excluded.engine_config_json,
                 score = excluded.score,
                 avg_pnl_pct = excluded.avg_pnl_pct,
                 runs = excluded.runs,
                 created_at = CURRENT_TIMESTAMP",
            params![
                new_id(),
                self.mode.as_str(),
                r.symbol.to_uppercase(),
                r.regime.to_uppercase(),
                r.config_name,
                r.engine_config_json,
                r.score,
                r.avg_pnl_pct,
                r.runs as i64,
            ],
        )?;
        Ok(())
    }

    /// The best config for one (symbol × regime) bucket, if the last sweep
    /// produced one.
    pub fn get_regime_profile(
        &self,
        symbol: &str,
        regime: &str,
    ) -> Result<Option<RegimeProfileRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, mode, symbol, regime, config_name, engine_config_json,
                    score, avg_pnl_pct, runs
             FROM regime_profiles WHERE mode = ?1 AND symbol = ?2 AND regime = ?3",
        )?;
        let row = stmt
            .query_row(
                params![
                    self.mode.as_str(),
                    symbol.to_uppercase(),
                    regime.to_uppercase()
                ],
                row_regime_profile,
            )
            .optional()?;
        Ok(row)
    }

    /// All regime winners for one symbol (or all symbols when `symbol` is
    /// empty), ordered by regime name.
    pub fn list_regime_profiles(&self, symbol: &str) -> Result<Vec<RegimeProfileRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = if symbol.is_empty() {
            conn.prepare(
                "SELECT id, mode, symbol, regime, config_name, engine_config_json,
                        score, avg_pnl_pct, runs
                 FROM regime_profiles WHERE mode = ?1 ORDER BY symbol, regime",
            )?
        } else {
            conn.prepare(
                "SELECT id, mode, symbol, regime, config_name, engine_config_json,
                        score, avg_pnl_pct, runs
                 FROM regime_profiles WHERE mode = ?1 AND symbol = ?2 ORDER BY regime",
            )?
        };
        let rows = if symbol.is_empty() {
            stmt.query_map(params![self.mode.as_str()], row_regime_profile)?
        } else {
            stmt.query_map(
                params![self.mode.as_str(), symbol.to_uppercase()],
                row_regime_profile,
            )?
        };
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Drop all regime winners for this mode (used by `--force` retrains).
    pub fn clear_regime_profiles(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM regime_profiles WHERE mode = ?1",
            params![self.mode.as_str()],
        )?;
        Ok(())
    }

    // -- seasonality stats (per-symbol monthly averages) ---------------------

    /// Upsert one (symbol × calendar month) average return from a training
    /// sweep. Re-running with `--force` replaces the previous seasonality.
    pub fn record_seasonality(
        &self,
        symbol: &str,
        month: u32,
        avg_return_pct: f64,
        samples: u32,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO seasonality_stats (symbol, month, avg_return_pct, samples, trained_at)
             VALUES (?1, ?2, ?3, ?4, CURRENT_TIMESTAMP)
             ON CONFLICT(symbol, month) DO UPDATE SET
                 avg_return_pct = excluded.avg_return_pct,
                 samples = excluded.samples,
                 trained_at = CURRENT_TIMESTAMP",
            params![
                symbol.to_uppercase(),
                month as i64,
                avg_return_pct,
                samples as i64
            ],
        )?;
        Ok(())
    }

    /// Drop all seasonality rows (used by `--force` retrains).
    pub fn clear_seasonality(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM seasonality_stats", [])?;
        Ok(())
    }

    /// Per-calendar-month average returns for one symbol: `(month, avg_return_pct, samples)`.
    pub fn list_seasonality(&self, symbol: &str) -> Result<Vec<(u32, f64, u32)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT month, avg_return_pct, samples FROM seasonality_stats
             WHERE symbol = ?1 ORDER BY month",
        )?;
        let rows = stmt.query_map(params![symbol.to_uppercase()], |r| {
            Ok((
                r.get::<_, i64>(0)? as u32,
                r.get::<_, f64>(1)?,
                r.get::<_, i64>(2)? as u32,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    // -- livepatch experiments (config sweeps with B&H benchmark) -------------

    /// Persist one experiment outcome so the next run can be compared against
    /// it ("is this change better than the last one?").
    pub fn record_experiment(&self, e: &NewExperiment) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO experiments
                (id, label, config_name, symbols, from_date, to_date, window_days,
                 step_days, capitals, runs, score, avg_pnl_pct, avg_max_dd_pct,
                 win_rate, profitable_ratio, cold_ratio, windows_beaten_bh,
                 windows_total, avg_pnl_vs_bh, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     ?14, ?15, ?16, ?17, ?18, ?19, CURRENT_TIMESTAMP)",
            params![
                new_id(),
                e.label,
                e.config_name,
                e.symbols,
                e.from_date,
                e.to_date,
                e.window_days as i64,
                e.step_days as i64,
                e.capitals,
                e.runs as i64,
                e.score,
                e.avg_pnl_pct,
                e.avg_max_dd_pct,
                e.win_rate,
                e.profitable_ratio,
                e.cold_ratio,
                e.windows_beaten_bh as i64,
                e.windows_total as i64,
                e.avg_pnl_vs_bh,
            ],
        )?;
        Ok(())
    }

    /// Recent experiments, newest first.
    pub fn list_experiments(&self, limit: i64) -> Result<Vec<ExperimentRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, label, config_name, symbols, from_date, to_date, window_days,
                    step_days, capitals, runs, score, avg_pnl_pct, avg_max_dd_pct,
                    win_rate, profitable_ratio, cold_ratio, windows_beaten_bh,
                    windows_total, avg_pnl_vs_bh, created_at
             FROM experiments ORDER BY created_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(ExperimentRow {
                id: r.get(0)?,
                label: r.get(1)?,
                config_name: r.get(2)?,
                symbols: r.get(3)?,
                from_date: r.get(4)?,
                to_date: r.get(5)?,
                window_days: r.get(6)?,
                step_days: r.get(7)?,
                capitals: r.get(8)?,
                runs: r.get(9)?,
                score: r.get(10)?,
                avg_pnl_pct: r.get(11)?,
                avg_max_dd_pct: r.get(12)?,
                win_rate: r.get(13)?,
                profitable_ratio: r.get(14)?,
                cold_ratio: r.get(15)?,
                windows_beaten_bh: r.get(16)?,
                windows_total: r.get(17)?,
                avg_pnl_vs_bh: r.get(18)?,
                created_at: r.get(19)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Drop all experiment rows (housekeeping).
    pub fn clear_experiments(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM experiments", [])?;
        Ok(())
    }

    // -- advisor task simulations -------------------------------------------

    /// Persist one advisor task-sim run so results survive across invocations.
    pub fn record_task_sim(&self, r: &NewTaskSimRun) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO task_sim_runs
                (id, label, symbols, start_date, end_date, monthly_budget_usd,
                 buy_fee_pct, sell_fee_pct, tranche_count, min_profit_margin,
                 max_alerts_per_day, signal, total_buys, total_sells,
                 realized_profit_usd, unrealized_profit_usd, fees_paid_usd,
                 open_lots, total_deployed_usd, win_rate, total_pnl_pct, score,
                 created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22,
                     CURRENT_TIMESTAMP)",
            params![
                new_id(),
                r.label,
                r.symbols,
                r.start_date,
                r.end_date,
                r.monthly_budget_usd,
                r.buy_fee_pct,
                r.sell_fee_pct,
                r.tranche_count as i64,
                r.min_profit_margin,
                r.max_alerts_per_day,
                r.signal,
                r.total_buys as i64,
                r.total_sells as i64,
                r.realized_profit_usd,
                r.unrealized_profit_usd,
                r.fees_paid_usd,
                r.open_lots as i64,
                r.total_deployed_usd,
                r.win_rate,
                r.total_pnl_pct,
                r.score,
            ],
        )?;
        Ok(())
    }

    /// Recent task-sim runs, newest first.
    #[allow(dead_code)] // planned CLI/Telegram inspection surface
    pub fn list_task_sim_runs(&self, limit: i64) -> Result<Vec<TaskSimRunRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, label, symbols, start_date, end_date, monthly_budget_usd,
                    buy_fee_pct, sell_fee_pct, tranche_count, min_profit_margin,
                    max_alerts_per_day, signal, total_buys, total_sells,
                    realized_profit_usd, unrealized_profit_usd, fees_paid_usd,
                    open_lots, total_deployed_usd, win_rate, total_pnl_pct, score,
                    created_at
             FROM task_sim_runs ORDER BY created_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(TaskSimRunRow {
                id: r.get(0)?,
                label: r.get(1)?,
                symbols: r.get(2)?,
                start_date: r.get(3)?,
                end_date: r.get(4)?,
                monthly_budget_usd: r.get(5)?,
                buy_fee_pct: r.get(6)?,
                sell_fee_pct: r.get(7)?,
                tranche_count: r.get(8)?,
                min_profit_margin: r.get(9)?,
                max_alerts_per_day: r.get(10)?,
                signal: r.get(11)?,
                total_buys: r.get(12)?,
                total_sells: r.get(13)?,
                realized_profit_usd: r.get(14)?,
                unrealized_profit_usd: r.get(15)?,
                fees_paid_usd: r.get(16)?,
                open_lots: r.get(17)?,
                total_deployed_usd: r.get(18)?,
                win_rate: r.get(19)?,
                total_pnl_pct: r.get(20)?,
                score: r.get(21)?,
                created_at: r.get(22)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Drop all task-sim rows (housekeeping).
    #[allow(dead_code)] // planned CLI/Telegram inspection surface
    pub fn clear_task_sim_runs(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM task_sim_runs", [])?;
        Ok(())
    }

    // -- fake (virtual) trading ----------------------------------------------

    /// Record one virtual trade executed by `faketrade` mode.
    pub fn record_fake_trade(&self, t: &NewFakeTrade) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO fake_trades
                (id, ts, symbol, side, price, amount, usd, fee_usd, pnl_usd, note)
             VALUES (?1, CURRENT_TIMESTAMP, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                new_id(),
                t.symbol.to_uppercase(),
                t.side,
                t.price,
                t.amount,
                t.usd,
                t.fee_usd,
                t.pnl_usd,
                t.note,
            ],
        )?;
        Ok(())
    }

    /// Recent virtual trades, newest first.
    pub fn list_fake_trades(&self, limit: i64) -> Result<Vec<FakeTradeRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, ts, symbol, side, price, amount, usd, fee_usd, pnl_usd, note
             FROM fake_trades ORDER BY ts DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |r| {
            Ok(FakeTradeRow {
                id: r.get(0)?,
                ts: r.get(1)?,
                symbol: r.get(2)?,
                side: r.get(3)?,
                price: r.get(4)?,
                amount: r.get(5)?,
                usd: r.get(6)?,
                fee_usd: r.get(7)?,
                pnl_usd: r.get(8)?,
                note: r.get(9)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Drop all virtual trades (housekeeping / restart a fake account).
    pub fn clear_fake_trades(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM fake_trades", [])?;
        Ok(())
    }

    /// Aggregated fake-account stats: spent / earned / fees / open lots per
    /// symbol, plus overall net PnL in USD and %.
    #[allow(clippy::too_many_arguments)]
    pub fn fake_stats(&self) -> Result<FakeStats> {
        let conn = self.conn.lock().unwrap();
        let buys = conn.query_row(
            "SELECT COALESCE(SUM(usd),0), COALESCE(SUM(fee_usd),0), COUNT(*)
             FROM fake_trades WHERE side = 'BUY'",
            [],
            |r| {
                Ok((
                    r.get::<_, f64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )?;
        let sells = conn.query_row(
            "SELECT COALESCE(SUM(usd),0), COALESCE(SUM(fee_usd),0), COUNT(*),
                    COALESCE(SUM(pnl_usd),0)
             FROM fake_trades WHERE side = 'SELL'",
            [],
            |r| {
                Ok((
                    r.get::<_, f64>(0)?,
                    r.get::<_, f64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, f64>(3)?,
                ))
            },
        )?;
        let mut stmt = conn.prepare(
            "SELECT symbol, side, COALESCE(SUM(amount),0), COALESCE(SUM(usd),0)
             FROM fake_trades GROUP BY symbol, side",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })?;
        let mut per_symbol: Vec<(String, f64, f64, f64, f64)> = Vec::new();
        for row in rows {
            let (sym, side, amount, usd) = row?;
            if let Some(e) = per_symbol.iter_mut().find(|e| e.0 == sym) {
                if side == "BUY" {
                    e.1 += amount;
                    e.2 += usd;
                } else {
                    e.3 += amount;
                    e.4 += usd;
                }
            } else if side == "BUY" {
                per_symbol.push((sym, amount, usd, 0.0, 0.0));
            } else {
                per_symbol.push((sym, 0.0, 0.0, amount, usd));
            }
        }
        let (buy_usd, buy_fees, buy_count) = buys;
        let (sell_usd, sell_fees, sell_count, realized_pnl) = sells;
        let net_pnl = realized_pnl - buy_fees - sell_fees;
        let net_pct = if buy_usd > 0.0 {
            net_pnl / buy_usd * 100.0
        } else {
            0.0
        };
        Ok(FakeStats {
            buy_usd,
            buy_fees,
            buy_count,
            sell_usd,
            sell_fees,
            sell_count,
            realized_pnl,
            net_pnl,
            net_pct,
            per_symbol,
        })
    }

    // -- status & backup ----------------------------------------------------

    pub fn counts(&self) -> Result<StoreCounts> {
        let conn = self.conn.lock().unwrap();
        let one = |sql: &str| -> Result<i64> {
            Ok(conn.query_row(sql, params![self.mode.as_str()], |r| r.get(0))?)
        };
        Ok(StoreCounts {
            constraints: one("SELECT COUNT(*) FROM operator_constraints WHERE mode = ?1")?,
            allocations: one("SELECT COUNT(*) FROM strategy_allocations WHERE mode = ?1")?,
            open_lots: one("SELECT COUNT(*) FROM trade_lots WHERE mode = ?1 AND status = 'OPEN'")?,
            cold_lots: one(
                "SELECT COUNT(*) FROM trade_lots WHERE mode = ?1 AND status = 'COLD_HOLD'",
            )?,
            closed_lots: one(
                "SELECT COUNT(*) FROM trade_lots WHERE mode = ?1 AND status = 'CLOSED'",
            )?,
            debates: one("SELECT COUNT(*) FROM agent_debates WHERE mode = ?1")?,
            executions: one("SELECT COUNT(*) FROM trade_executions WHERE mode = ?1")?,
            recommendations: one("SELECT COUNT(*) FROM trade_recommendations WHERE mode = ?1")?,
            backtests: one("SELECT COUNT(*) FROM backtest_runs WHERE mode = ?1")?,
            lessons: one("SELECT COUNT(*) FROM strategy_lessons WHERE mode = ?1")?,
            engine_configs: one("SELECT COUNT(*) FROM engine_configs WHERE mode = ?1")?,
            strategy_profiles: conn.query_row(
                "SELECT COUNT(*) FROM strategy_profiles",
                [],
                |r| r.get(0),
            )?,
        })
    }

    // -- exchange fees ------------------------------------------------------

    /// Current fee config for this mode. Returns zero fees when never set.
    pub fn get_fee_config(&self) -> Result<ExchangeFeeConfig> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT mode, buy_fee_pct, sell_fee_pct, updated_at
             FROM exchange_fees WHERE mode = ?1",
            params![self.mode.as_str()],
            |r| {
                Ok(ExchangeFeeConfig {
                    mode: TradingMode::parse(r.get::<_, String>(0)?.as_str())
                        .expect("stored mode is always valid"),
                    buy_fee_pct: r.get(1)?,
                    sell_fee_pct: r.get(2)?,
                    updated_at: r.get(3)?,
                })
            },
        );
        match row {
            Ok(f) => Ok(f),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(ExchangeFeeConfig {
                mode: self.mode,
                buy_fee_pct: 0.0,
                sell_fee_pct: 0.0,
                updated_at: now_iso(),
            }),
            Err(e) => Err(e.into()),
        }
    }

    /// Set (or replace) the exchange fees for this mode. Percentages, e.g.
    /// `buy 5.0` = 5% charged on buys.
    pub fn set_fee_config(&self, buy_fee_pct: f64, sell_fee_pct: f64) -> Result<ExchangeFeeConfig> {
        validate_non_negative("buy_fee_pct", buy_fee_pct)?;
        validate_non_negative("sell_fee_pct", sell_fee_pct)?;
        if buy_fee_pct >= 100.0 || sell_fee_pct >= 100.0 {
            bail!("fees must be < 100%");
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO exchange_fees (mode, buy_fee_pct, sell_fee_pct, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(mode) DO UPDATE SET
                 buy_fee_pct = excluded.buy_fee_pct,
                 sell_fee_pct = excluded.sell_fee_pct,
                 updated_at = excluded.updated_at",
            params![self.mode.as_str(), buy_fee_pct, sell_fee_pct, now_iso()],
        )?;
        Ok(ExchangeFeeConfig {
            mode: self.mode,
            buy_fee_pct,
            sell_fee_pct,
            updated_at: now_iso(),
        })
    }

    // -- trade tasks ---------------------------------------------------------

    /// Create a standing trade task. Starts ACTIVE with zero monthly spend.
    pub fn create_task(&self, t: NewTradeTask) -> Result<TradeTask> {
        if t.name.trim().is_empty() {
            bail!("task name must not be empty");
        }
        if t.assets.trim().is_empty() {
            bail!("task assets must not be empty");
        }
        validate_positive("monthly_budget_usd", t.monthly_budget_usd)?;
        if t.max_alerts_per_day <= 0 {
            bail!("max_alerts_per_day must be >= 1");
        }
        if t.tranche_count == 0 {
            bail!("tranche_count must be >= 1");
        }
        validate_non_negative("min_profit_margin", t.min_profit_margin)?;
        let id = new_id();
        let now = now_iso();
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO trade_tasks
                 (id, mode, name, assets, execution, monthly_budget_usd, max_alerts_per_day,
                  tranche_count, min_profit_margin, status, month_spend_usd, month_started_at,
                  alerts_today, alerts_date, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                id,
                self.mode.as_str(),
                t.name.trim(),
                t.assets.to_uppercase(),
                t.execution.as_str(),
                t.monthly_budget_usd,
                t.max_alerts_per_day,
                t.tranche_count,
                t.min_profit_margin,
                TaskStatus::Active.as_str(),
                0.0,
                now,
                0,
                now,
                now,
                now,
            ],
        )?;
        Ok(TradeTask {
            id,
            mode: self.mode,
            name: t.name.trim().to_string(),
            assets: t.assets.to_uppercase(),
            execution: t.execution,
            monthly_budget_usd: t.monthly_budget_usd,
            max_alerts_per_day: t.max_alerts_per_day,
            tranche_count: t.tranche_count,
            min_profit_margin: t.min_profit_margin,
            status: TaskStatus::Active,
            month_spend_usd: 0.0,
            month_started_at: now.clone(),
            alerts_today: 0,
            alerts_date: now.clone(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    pub fn list_tasks(&self) -> Result<Vec<TradeTask>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, mode, name, assets, execution, monthly_budget_usd, max_alerts_per_day,
                    tranche_count, min_profit_margin, status, month_spend_usd, month_started_at,
                    alerts_today, alerts_date, created_at, updated_at
             FROM trade_tasks WHERE mode = ?1 ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map(params![self.mode.as_str()], row_task)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Delete a standing trade task by id or name (case-insensitive).
    pub fn delete_task(&self, id_or_name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM trade_tasks WHERE mode = ?1 AND (id = ?2 OR lower(name) = lower(?2))",
            params![self.mode.as_str(), id_or_name],
        )?;
        if n == 0 {
            bail!(
                "task '{id_or_name}' not found in mode {}",
                self.mode.as_str()
            );
        }
        Ok(())
    }

    pub fn get_task(&self, id: &str) -> Result<TradeTask> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, mode, name, assets, execution, monthly_budget_usd, max_alerts_per_day,
                    tranche_count, min_profit_margin, status, month_spend_usd, month_started_at,
                    alerts_today, alerts_date, created_at, updated_at
             FROM trade_tasks WHERE id = ?1 AND mode = ?2",
            params![id, self.mode.as_str()],
            row_task,
        )
        .map_err(Into::into)
    }

    pub fn set_task_status(&self, id: &str, status: TaskStatus) -> Result<TradeTask> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE trade_tasks SET status = ?1, updated_at = ?2 WHERE id = ?3 AND mode = ?4",
            params![status.as_str(), now_iso(), id, self.mode.as_str()],
        )?;
        if n == 0 {
            bail!("task '{id}' not found in mode {}", self.mode.as_str());
        }
        drop(conn);
        self.get_task(id)
    }

    /// Record that the advisor emitted one alert for the task, adding
    /// `spend_usd` to the current month's spend. Rolls the monthly budget and
    /// the daily alert counter over when the calendar month / UTC day changes.
    pub fn bump_task_alert(&self, id: &str, spend_usd: f64) -> Result<TradeTask> {
        let task = self.get_task(id)?;
        let now = now_iso();
        let (month_key, day_key) = month_day_keys(&now);
        let (cur_month, cur_day) = month_day_keys(&task.month_started_at);
        let (month_spend, month_started) = if month_key != cur_month {
            (0.0, now.clone())
        } else {
            (task.month_spend_usd, task.month_started_at)
        };
        let alerts_today = if day_key != cur_day {
            1
        } else {
            task.alerts_today + 1
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE trade_tasks
             SET month_spend_usd = ?1, month_started_at = ?2, alerts_today = ?3,
                 alerts_date = ?4, updated_at = ?5
             WHERE id = ?6 AND mode = ?7",
            params![
                month_spend + spend_usd,
                month_started,
                alerts_today,
                now,
                now,
                id,
                self.mode.as_str()
            ],
        )?;
        drop(conn);
        self.get_task(id)
    }

    /// Hot backup via `VACUUM INTO` — produces a standalone, restorable copy.
    /// Fails if `dest` already exists (SQLite refuses to overwrite).
    pub fn backup_to(&self, dest: &str) -> Result<()> {
        if Path::new(dest).exists() {
            bail!("backup destination {dest} already exists — refusing to overwrite");
        }
        let conn = self.conn.lock().unwrap();
        let escaped = dest.replace('\'', "''");
        conn.execute_batch(&format!("VACUUM INTO '{escaped}';"))?;
        Ok(())
    }

    /// Write a compact seed database: only the knowledge tables that a fresh
    /// install should start with (profiles, lessons, training stats, active
    /// engine config). Runtime state (lots, executions, recommendations,
    /// backtest runs, debates) is deliberately excluded so the bundled file
    /// stays in the 1–5 MB range and never carries stale positions.
    /// Returns the resulting file size in bytes.
    pub fn export_seed(&self, dest: &str) -> Result<u64> {
        // Write to a temp sibling first, then atomically rename over the
        // destination. Writing straight to `dest` fails with a confusing
        // SQLITE_CANTOPEN when another process briefly holds the file open
        // (Windows), and a partial write would corrupt the bundled seed.
        let tmp = format!("{dest}.tmp");
        let res = self.export_seed_to(&tmp).and_then(|size| {
            std::fs::rename(&tmp, dest)?;
            Ok(size)
        });
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res
    }

    /// Core of [`Self::export_seed`]: builds the compact seed at `tmp`.
    fn export_seed_to(&self, tmp: &str) -> Result<u64> {
        // The destination's parent dir (e.g. `data/`) may not exist in CWD;
        // SQLite refuses to create it, so ensure it first (same as open_path).
        if let Some(parent) = Path::new(tmp).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut seed = Connection::open(tmp)?;
        init_schema(&seed)?;
        if self.db_path == ":memory:" {
            // In-memory source cannot be ATTACHed; copy via the backup API
            // (works for both file and memory sources), then strip runtime rows.
            let conn = self.conn.lock().unwrap();
            let backup = rusqlite::backup::Backup::new(&conn, &mut seed)?;
            backup.run_to_completion(-1, std::time::Duration::ZERO, None)?;
            drop(backup);
            seed.execute_batch(
                "DELETE FROM agent_debates;
                 DELETE FROM trade_executions;
                 DELETE FROM trade_recommendations;
                 DELETE FROM trade_lots;
                 DELETE FROM backtest_runs;
                 DELETE FROM operator_constraints;
                 DELETE FROM strategy_allocations;
                 DELETE FROM experiments;",
            )?;
        } else {
            // ATTACH the SOURCE database (this store's file) to the seed and
            // copy only the knowledge tables — runtime state stays behind.
            let escaped = self.db_path.replace('\'', "''");
            seed.execute_batch(&format!("ATTACH DATABASE '{escaped}' AS source_db;"))?;
            seed.execute_batch(
                "INSERT INTO strategy_profiles
                     SELECT * FROM source_db.strategy_profiles;
                 INSERT INTO strategy_lessons
                     SELECT * FROM source_db.strategy_lessons WHERE mode = 'SIMULATION';
                 INSERT INTO training_stats
                     SELECT * FROM source_db.training_stats WHERE mode = 'SIMULATION';
                 INSERT INTO regime_profiles
                     SELECT * FROM source_db.regime_profiles WHERE mode = 'SIMULATION';
                 INSERT INTO seasonality_stats
                     SELECT * FROM source_db.seasonality_stats;
                 INSERT INTO engine_configs
                     SELECT * FROM source_db.engine_configs WHERE mode = 'SIMULATION';",
            )?;
            seed.execute_batch("DETACH DATABASE source_db;")?;
        }
        // Keep the seed compact: lessons accumulate quickly across backtests
        // and training runs (thousands of rows), but a fresh install only
        // needs a recent representative snapshot per symbol. The full lesson
        // history stays in the runtime sim DB. Window function keeps this
        // O(n log n) — a correlated subquery here is O(n²) and hangs on
        // ~14k rows.
        seed.execute_batch(
            "DELETE FROM strategy_lessons
             WHERE id NOT IN (
                 SELECT id FROM (
                     SELECT id, ROW_NUMBER() OVER (
                         PARTITION BY symbol ORDER BY created_at DESC, id DESC
                     ) AS rn
                     FROM strategy_lessons
                 )
                 WHERE rn <= 50
             );",
        )?;
        // Reclaim pages freed by the lesson trim so the shipped file is
        // actually compact, not just logically smaller.
        seed.execute_batch("VACUUM;")?;
        // Flush the WAL so the shipped file is self-contained.
        seed.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode = DELETE;")?;
        drop(seed);
        Ok(std::fs::metadata(tmp)?.len())
    }
}
