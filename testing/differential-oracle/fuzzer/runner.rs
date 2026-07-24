//! Main simulation runner.
//!
//! This module orchestrates the simulation by:
//! 1. Creating both Turso and SQLite databases
//! 2. Generating and executing CREATE TABLE statements
//! 3. Generating statements (DML and DDL) using sql_gen
//! 4. Executing them on both databases
//! 5. Checking the differential oracle
//! 6. Re-introspecting schemas after DDL statements

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::panic::{AssertUnwindSafe, RefUnwindSafe};
use std::path::PathBuf;
use std::sync::Arc;
use turso_core::SqliteDialect;

use anyhow::{Context, Result, bail};
use comfy_table::{Attribute, Cell, Color, ContentArrangement, Table};
use parking_lot::Mutex;
use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use turso_core::Database;

use crate::generate::{GeneratorKind, PropTestBackend, SqlGenBackend, SqlGenerator};
use crate::memory::{MemorySimIO, SimIO};
use crate::oracle::{DifferentialOracle, OracleResult, QueryResult, check_differential};

use crate::schema::SchemaIntrospector;
pub use sql_gen::TreeMode;
use sql_gen_prop::SqlValue;

/// Configuration for the simulator.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// Random seed for deterministic execution.
    pub seed: u64,
    /// Number of tables to create.
    pub num_tables: usize,
    /// Number of columns per table.
    pub columns_per_table: usize,
    /// Number of statements to generate and execute.
    pub num_statements: usize,
    /// Whether to print verbose output.
    pub verbose: bool,
    /// Keep simulation databases
    pub keep_files: bool,
    /// Which SQL generator backend to use.
    pub generator: GeneratorKind,
    /// Whether to write a coverage report.
    pub coverage: bool,
    /// Coverage report tree mode.
    pub tree_mode: TreeMode,
    /// Whether to enable MVCC mode.
    pub mvcc: bool,
    /// Whether to enable materialized view fuzzing (Turso IVM vs SQLite regular views).
    pub matview: bool,
    /// Probability (0.0–1.0) that a DML statement starts a batch transaction
    /// collecting 2–10 DML statements wrapped in BEGIN/COMMIT.
    pub batch_dml_probability: f64,
    /// Maximum number of statements in a normal batch transaction.
    pub max_batch_size: usize,
    /// Probability (0.0–1.0) that a batch is "large" (50–300 stmts).
    /// Large batches exercise pager page allocation under IVM pressure.
    pub large_batch_probability: f64,
    /// Use real file I/O (PlatformIO) instead of in-memory I/O (MemorySimIO).
    /// Enables pager/freelist code paths that only trigger with actual disk I/O.
    pub file_io: bool,
    /// Probability (0.0–1.0) that a successful DML statement is immediately
    /// re-executed verbatim on both databases. Catches idempotency bugs in
    /// IVM (e.g., redundant UPDATE-to-same-value drops null-padded LEFT JOIN
    /// rows from a matview).
    pub redundant_dml_probability: f64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            seed: rand::rng().next_u64(),
            num_tables: 2,
            columns_per_table: 5,
            num_statements: 100,
            verbose: false,
            keep_files: false,
            generator: GeneratorKind::default(),
            coverage: false,
            tree_mode: TreeMode::default(),
            mvcc: false,
            matview: false,
            batch_dml_probability: 0.0,
            max_batch_size: 10,
            large_batch_probability: 0.0,
            file_io: false,
            redundant_dml_probability: 0.0,
        }
    }
}

/// Statistics from a simulation run.
#[derive(Debug, Default)]
pub struct SimStats {
    /// Number of statements executed.
    pub statements_executed: usize,
    /// Number of oracle warnings (e.g., LIMIT without ORDER BY mismatches).
    pub warnings: usize,
    /// Number of oracle failures.
    pub oracle_failures: usize,
    /// Number of errors encountered.
    pub errors: usize,
}

impl SimStats {
    /// Returns true if the simulation completed successfully (no failures).
    pub fn is_success(&self) -> bool {
        self.oracle_failures == 0
    }

    /// Create a colorful table displaying simulation results.
    pub fn to_table(&self, config: &SimConfig) -> Table {
        let mut table = Table::new();
        table.set_content_arrangement(ContentArrangement::Dynamic);

        // Header
        let status = if self.is_success() {
            Cell::new("PASSED")
                .fg(Color::Green)
                .add_attribute(Attribute::Bold)
        } else {
            Cell::new("FAILED")
                .fg(Color::Red)
                .add_attribute(Attribute::Bold)
        };

        table.set_header(vec![
            Cell::new("Simulation Results").add_attribute(Attribute::Bold),
            status,
        ]);

        // Config section
        table.add_row(vec![
            Cell::new("Seed").fg(Color::Cyan),
            Cell::new(config.seed),
        ]);
        table.add_row(vec![
            Cell::new("Target Statements").fg(Color::Cyan),
            Cell::new(config.num_statements),
        ]);

        // Results section
        table.add_row(vec![
            Cell::new("Statements Executed").fg(Color::Blue),
            Cell::new(self.statements_executed).fg(Color::Blue),
        ]);

        // Warnings - yellow if any
        let warnings_cell = if self.warnings > 0 {
            Cell::new(self.warnings).fg(Color::Yellow)
        } else {
            Cell::new(self.warnings).fg(Color::Green)
        };
        table.add_row(vec![Cell::new("Warnings").fg(Color::Yellow), warnings_cell]);

        // Failures - red if any
        let failures_cell = if self.oracle_failures > 0 {
            Cell::new(self.oracle_failures)
                .fg(Color::Red)
                .add_attribute(Attribute::Bold)
        } else {
            Cell::new(self.oracle_failures).fg(Color::Green)
        };
        table.add_row(vec![
            Cell::new("Oracle Failures").fg(Color::Red),
            failures_cell,
        ]);

        // Errors - red if any
        let errors_cell = if self.errors > 0 {
            Cell::new(self.errors).fg(Color::Red)
        } else {
            Cell::new(self.errors).fg(Color::Green)
        };
        table.add_row(vec![Cell::new("Errors").fg(Color::Red), errors_cell]);

        table
    }

    /// Print the stats as a colorful table to stdout.
    pub fn print_table(&self, config: &SimConfig) {
        println!("\n{}", self.to_table(config));
    }
}

/// Tally of empty CDC batches per relation, populated by a registered
/// `set_change_callback`. IVM should never emit a CDC batch with zero
/// changes — every fired callback is a real delta or a bug.
#[derive(Debug, Default)]
struct CdcEmptyBatchTally {
    /// relation_name -> count of empty batches observed
    counts: HashMap<String, usize>,
}

/// The main simulator.
pub struct Fuzzer {
    config: SimConfig,
    rng: RefCell<ChaCha8Rng>,
    turso_conn: Arc<turso_core::Connection>,
    sqlite_conn: rusqlite::Connection,
    turso_db: Arc<Database>,
    /// In-memory IO for the Turso database.
    io: Arc<dyn turso_core::IO>,
    sim_io: Option<Arc<MemorySimIO>>,
    /// Directory to save run artifacts
    pub out_dir: PathBuf,
    /// Captures panic hook info (location + backtrace) for the last panic.
    panic_context: Arc<Mutex<Option<String>>>,
    /// Tracks empty CDC batches observed since the last reset. Populated by
    /// the callback registered in `Fuzzer::new` when matview mode is enabled.
    cdc_empty_batches: Arc<Mutex<CdcEmptyBatchTally>>,
}

impl RefUnwindSafe for Fuzzer {}

impl Fuzzer {
    /// Create a new simulator with in-memory databases.
    ///
    /// Uses `MemorySimIO` for deterministic in-memory storage.
    pub fn new(config: SimConfig) -> Result<Self> {
        let out_dir: PathBuf = "simulator-output".into();
        let rng = ChaCha8Rng::seed_from_u64(config.seed);

        if !out_dir.exists() {
            std::fs::create_dir_all(&out_dir)?;
        }

        // Create Turso database — file IO for pager/freelist testing, memory IO otherwise
        let sim_io: Option<Arc<MemorySimIO>> = if config.file_io {
            None
        } else {
            Some(Arc::new(MemorySimIO::new(config.seed)))
        };
        let io: Arc<dyn turso_core::IO> = if config.file_io {
            let db_path = out_dir.join("test.db");
            if db_path.exists() {
                std::fs::remove_file(&db_path)?;
            }
            for ext in ["-wal", "-shm"] {
                let p = out_dir.join(format!("test.db{ext}"));
                let _ = std::fs::remove_file(&p);
            }
            Arc::new(turso_core::PlatformIO::new()?)
        } else {
            sim_io.clone().unwrap() as Arc<dyn turso_core::IO>
        };
        let mut opts = turso_core::DatabaseOpts::new().with_attach(true);

        if config.matview {
            opts = opts.with_views(true);
        }

        let turso_db = Database::open_file_with_flags(
            io.clone(),
            out_dir.join("test.db").to_str().unwrap(),
            turso_core::OpenFlags::default(),
            opts,
            None,
            Arc::new(SqliteDialect),
        )?;
        let turso_conn = turso_db.connect()?;

        // Create SQLite in-memory database
        let sqlite_conn = if config.keep_files {
            let path = out_dir.join("test-sqlite.db");
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            rusqlite::Connection::open(path.to_str().unwrap())
        } else {
            rusqlite::Connection::open_in_memory()
        }
        .context("Failed to open SQLite database")?;

        // Attach an in-memory database on both connections
        turso_conn
            .execute("ATTACH ':memory:' AS aux")
            .context("Failed to ATTACH on Turso")?;
        sqlite_conn
            .execute("ATTACH ':memory:' AS aux", [])
            .context("Failed to ATTACH on SQLite")?;
        tracing::info!("Attached ':memory:' AS aux on both connections");

        // Enable MVCC after ATTACH (ATTACH is not supported in MVCC mode)
        if config.mvcc {
            turso_conn
                .execute("PRAGMA journal_mode = 'mvcc'")
                .context("Failed to enable MVCC mode")?;
        }

        let cdc_empty_batches = Arc::new(Mutex::new(CdcEmptyBatchTally::default()));
        if config.matview {
            let tally = Arc::clone(&cdc_empty_batches);
            turso_conn.set_change_callback(move |event| {
                if event.changes.is_empty() {
                    *tally
                        .lock()
                        .counts
                        .entry(event.relation_name.clone())
                        .or_insert(0) += 1;
                }
            });
        }

        Ok(Self {
            config,
            rng: RefCell::new(rng),
            turso_conn,
            sqlite_conn,
            turso_db,
            io,
            sim_io,
            out_dir,
            panic_context: Arc::new(Mutex::new(None)),
            cdc_empty_batches,
        })
    }

    /// Persist the in-memory database files to disk.
    ///
    /// Writes `.db`, `.wal`, and `.log` files to the filesystem.
    /// Only applies when using in-memory IO; file IO already writes to disk.
    pub fn persist_files(&self) -> Result<()> {
        if let Some(sim_io) = &self.sim_io {
            sim_io.persist_files()?;
        }
        Ok(())
    }

    /// Introspect and return the current schema from the Turso
    /// database, including attached databases.
    ///
    /// Uses `from_turso_with_attached` so callers see the same view
    /// the internal schema verification path (`introspect_and_verify_schemas`)
    /// uses; otherwise the diff fuzzer's "my view of the schema" and
    /// "the schema I run integrity checks against" could silently
    /// diverge when attached databases are present.
    pub fn get_schema(&self) -> Result<sql_gen::Schema> {
        SchemaIntrospector::from_turso_with_attached(&self.turso_conn)
            .context("Failed to introspect Turso schema (with attached)")
    }

    /// Run the simulation.
    pub fn run(&mut self) -> Result<SimStats> {
        let mut stats = SimStats::default();
        let mut executed_sql = Vec::new();
        let mut coverage = None;

        let result = self.run_inner(&mut stats, &mut executed_sql, &mut coverage);

        // Always write SQL file and print stats, even on error
        if let Err(e) = self.write_sql_file(&executed_sql) {
            tracing::warn!("Failed to write test.sql: {e}");
        }
        if self.config.coverage {
            if let Some(cov) = coverage {
                if let Err(e) = self.write_coverage_report(&cov) {
                    tracing::warn!("Failed to write coverage report: {e}");
                }
            }
        }
        stats.print_table(&self.config);

        result.map(|()| stats)
    }

    /// Write the coverage report to simulator-output/coverage.txt
    fn write_coverage_report(&self, coverage: &sql_gen::Coverage) -> Result<()> {
        let report = coverage.report_with_mode(self.config.tree_mode);
        let full_path = self.out_dir.join("coverage.txt");
        std::fs::write(&full_path, report.to_string())?;
        tracing::info!("Wrote coverage report to {}", full_path.display());
        Ok(())
    }

    /// Write all executed SQL statements to test.sql
    fn write_sql_file(&self, statements: &[String]) -> Result<()> {
        let full_path = self.out_dir.join("test.sql");
        let mut file = std::fs::File::create(full_path.clone())?;
        for sql in statements {
            writeln!(file, "{sql};")?;
        }
        tracing::info!(
            "Wrote {} statements to {}",
            statements.len(),
            full_path.display()
        );
        Ok(())
    }

    fn run_inner(
        &mut self,
        stats: &mut SimStats,
        executed_sql: &mut Vec<String>,
        coverage_out: &mut Option<sql_gen::Coverage>,
    ) -> Result<()> {
        tracing::info!(
            "Starting simulation with seed={}, tables={}, statements={}, generator={:?}",
            self.config.seed,
            self.config.num_tables,
            self.config.num_statements,
            self.config.generator,
        );

        let mut generator: Box<dyn SqlGenerator> = match self.config.generator {
            GeneratorKind::SqlGen => {
                let seed: u64 = self.rng.borrow_mut().next_u64();
                Box::new(SqlGenBackend::new(seed))
            }
            GeneratorKind::SqlGenProp => {
                let seed_bytes: [u8; 32] = {
                    let mut bytes = [0u8; 32];
                    self.rng.borrow_mut().fill_bytes(&mut bytes);
                    bytes
                };
                Box::new(PropTestBackend::new(seed_bytes, self.config.matview))
            }
        };

        let mut matview_info: HashMap<String, Vec<sql_gen_prop::ColumnDef>> = HashMap::new();
        let mut schema = self.introspect_and_verify_schemas(&matview_info)?;

        for i in 0..self.config.num_statements {
            let stmt = generator.generate_with_matviews(&schema, &matview_info)?;

            if stmt.is_reopen {
                executed_sql.push("-- REOPEN DATABASE".to_string());
                tracing::info!("Reopening Turso database at statement {i}");
                self.reopen_turso()?;
                schema = self.introspect_and_verify_schemas(&matview_info)?;
                self.verify_all_tables_match(&schema, &matview_info, stats, executed_sql)?;
                stats.statements_executed += 1;
                continue;
            }

            if self.config.verbose {
                let stmt_type = if stmt.is_ddl { "DDL" } else { "DML" };
                tracing::info!("Statement {} [{}]: {}", i, stmt_type, stmt.sql);
            }

            if stmt.is_matview_ddl {
                // Split execution: materialized DDL on Turso, regular view DDL on SQLite
                let sqlite_sql = stmt
                    .sqlite_sql
                    .as_deref()
                    .expect("matview DDL must have sqlite_sql");

                if self.config.verbose {
                    tracing::info!(
                        "  Matview split: Turso='{}', SQLite='{}'",
                        stmt.sql,
                        sqlite_sql
                    );
                }

                // Execute materialized DDL on Turso
                let turso_result = DifferentialOracle::execute_turso(&self.turso_conn, &stmt.sql);
                // Execute regular view DDL on SQLite
                let sqlite_result =
                    DifferentialOracle::execute_sqlite(&self.sqlite_conn, sqlite_sql);

                match (&turso_result, &sqlite_result) {
                    (QueryResult::Error(turso_err), QueryResult::Error(_)) => {
                        // Both errored — not an oracle failure, just log it
                        stats.errors += 1;
                        executed_sql.push(format!("-- ERROR (both): {turso_err}"));
                        tracing::warn!("Matview DDL error on both at {i}: {turso_err}");
                    }
                    (QueryResult::Error(turso_err), _) => {
                        // Turso errored but SQLite succeeded — oracle failure
                        stats.oracle_failures += 1;
                        executed_sql.push(format!("-- FAILED: {}", stmt.sql));
                        executed_sql.push(format!("-- SQLITE: {sqlite_sql}"));
                        executed_sql.push(format!(
                            "-- Turso errored but SQLite succeeded: {turso_err}"
                        ));
                        bail!(
                            "Oracle failure at statement {i}: Turso errored on matview DDL but SQLite succeeded.\n  Turso SQL: {}\n  SQLite SQL: {}\n  Error: {turso_err}",
                            stmt.sql,
                            sqlite_sql
                        );
                    }
                    (_, QueryResult::Error(sqlite_err)) => {
                        // SQLite errored but Turso succeeded — also an oracle failure
                        stats.oracle_failures += 1;
                        executed_sql.push(format!("-- FAILED: {}", stmt.sql));
                        executed_sql.push(format!("-- SQLITE: {sqlite_sql}"));
                        executed_sql.push(format!(
                            "-- SQLite errored but Turso succeeded: {sqlite_err}"
                        ));
                        bail!(
                            "Oracle failure at statement {i}: SQLite errored on view DDL but Turso succeeded.\n  Turso SQL: {}\n  SQLite SQL: {}\n  Error: {sqlite_err}",
                            stmt.sql,
                            sqlite_sql
                        );
                    }
                    _ => {
                        stats.statements_executed += 1;
                        executed_sql.push(stmt.sql.clone());
                        executed_sql.push(format!("-- SQLITE: {sqlite_sql}"));

                        // Track matview info
                        if let Some(name) = extract_view_name_from_sql(&stmt.sql) {
                            if stmt.sql.contains("CREATE") {
                                matview_info.insert(
                                    name,
                                    stmt.matview_output_columns.clone().unwrap_or_default(),
                                );
                            } else {
                                matview_info.remove(&name);
                            }
                        }
                    }
                }

                schema = self
                    .introspect_and_verify_schemas(&matview_info)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Schema mismatch after matview DDL statement {i} ({}): {e}",
                            stmt.sql
                        )
                    })?;
                continue;
            }

            // Skip ALTER TABLE when matviews exist.
            // Turso rejects ALTER TABLE on matview base tables, and SQLite
            // validates ALL views during ALTER TABLE (even on unrelated tables),
            // so prior DROP/RENAME that broke a view in SQLite causes spurious
            // failures. Safest to skip all ALTER TABLE while matviews are active.
            if !matview_info.is_empty() && stmt.sql.to_uppercase().starts_with("ALTER TABLE") {
                stats.statements_executed += 1;
                executed_sql.push(format!("-- SKIPPED (matviews active): {}", stmt.sql));
                tracing::debug!(
                    "Skipping ALTER TABLE at {i}: matviews active ({})",
                    matview_info.len()
                );
                continue;
            }

            // Batch transaction: with some probability, collect multiple DML
            // statements into a BEGIN/COMMIT transaction before executing.
            let is_dml = !stmt.is_ddl && !stmt.is_matview_ddl && !stmt.is_reopen;
            let should_batch = is_dml && self.config.batch_dml_probability > 0.0 && {
                let roll: f64 = self.rng.borrow_mut().next_u64() as f64 / u64::MAX as f64;
                roll < self.config.batch_dml_probability
            };

            if should_batch {
                // Decide batch size: with large_batch_probability, generate 50-300 stmts
                // to exercise pager page allocation under IVM pressure.
                let is_large = self.config.large_batch_probability > 0.0 && {
                    let roll: f64 = self.rng.borrow_mut().next_u64() as f64 / u64::MAX as f64;
                    roll < self.config.large_batch_probability
                };
                let batch_size: usize = if is_large {
                    50 + (self.rng.borrow_mut().next_u64() as usize % 251) // 50-300
                } else {
                    2 + (self.rng.borrow_mut().next_u64() as usize
                        % (self.config.max_batch_size - 1)) // 2..max_batch_size
                };
                let mut batch_stmts = vec![stmt];
                for _ in 1..batch_size {
                    let next = generator.generate_with_matviews(&schema, &matview_info)?;
                    if next.is_ddl || next.is_matview_ddl || next.is_reopen {
                        break;
                    }
                    batch_stmts.push(next);
                }

                executed_sql.push("BEGIN TRANSACTION".to_string());
                DifferentialOracle::execute_turso(&self.turso_conn, "BEGIN TRANSACTION");
                self.sqlite_conn.execute("BEGIN TRANSACTION", []).ok();

                for batch_stmt in &batch_stmts {
                    DifferentialOracle::execute_turso(&self.turso_conn, &batch_stmt.sql);
                    DifferentialOracle::execute_sqlite(&self.sqlite_conn, &batch_stmt.sql);
                    executed_sql.push(batch_stmt.sql.clone());
                    stats.statements_executed += 1;
                    if !matview_info.is_empty() {
                        self.verify_matviews_match(&matview_info, stats, executed_sql)?;
                    }
                }

                executed_sql.push("COMMIT".to_string());
                DifferentialOracle::execute_turso(&self.turso_conn, "COMMIT");
                self.sqlite_conn.execute("COMMIT", []).ok();

                if self.config.verbose {
                    tracing::info!("Batch transaction at {i}: {} statements", batch_stmts.len());
                }

                if !matview_info.is_empty() {
                    self.verify_matviews_match(&matview_info, stats, executed_sql)?;
                    self.verify_no_empty_cdc_batches(
                        stats,
                        executed_sql,
                        &format!("batch transaction ending at statement {i}"),
                    )?;
                }

                continue;
            }

            // Execute on both databases and check oracle.
            // catch_unwind so that a panic inside Turso still reports
            // stats and the offending SQL instead of just a stack trace.
            let ctx = Arc::clone(&self.panic_context);
            let prev_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let bt = std::backtrace::Backtrace::force_capture();
                *ctx.lock() = Some(format!("{info}\n{bt}"));
            }));

            let turso_conn = Arc::clone(&self.turso_conn);
            let sqlite_conn = &self.sqlite_conn;
            let oracle_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                check_differential(&turso_conn, sqlite_conn, &schema, &stmt)
            }));

            std::panic::set_hook(prev_hook);

            let oracle_result = match oracle_result {
                Ok(result) => result,
                Err(panic) => {
                    let msg = panic
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "Unknown panic".to_string());
                    let context = self.panic_context.lock().take().unwrap_or_default();
                    executed_sql.push(format!("-- PANIC: {}", stmt.sql));
                    stats.oracle_failures += 1;
                    tracing::error!("Panic at statement {i}: {msg}");
                    tracing::error!("Panicking SQL: {}", stmt.sql);
                    tracing::error!("Backtrace:\n{context}");
                    return Err(anyhow::anyhow!(
                        "Panic during statement {i}: {msg}\n  SQL: {}\n{context}",
                        stmt.sql
                    ));
                }
            };

            match oracle_result {
                OracleResult::Pass => {
                    stats.statements_executed += 1;
                    executed_sql.push(stmt.sql.clone());
                }
                OracleResult::Warning(reason) => {
                    stats.statements_executed += 1;
                    stats.warnings += 1;
                    push_warning_comments(executed_sql, i, &reason);
                    executed_sql.push(stmt.sql.clone());
                    tracing::warn!("Oracle warning at statement {i}: {reason}");
                }
                OracleResult::Fail(reason) => {
                    stats.oracle_failures += 1;
                    executed_sql.push(format!("-- FAILED: {}", stmt.sql));
                    tracing::error!("Oracle failure at statement {i}: {reason}");
                    if !self.config.verbose {
                        tracing::error!("Failing SQL: {}", stmt.sql);
                    }
                    return Err(anyhow::anyhow!("Oracle failure: {reason}"));
                }
            }

            if is_dml && !matview_info.is_empty() {
                self.verify_matviews_match(&matview_info, stats, executed_sql)?;
                self.verify_no_empty_cdc_batches(
                    stats,
                    executed_sql,
                    &format!("DML statement {i}: {}", stmt.sql),
                )?;
            }

            // Redundant DML: with some probability re-execute the same DML
            // statement. Catches IVM idempotency bugs (e.g., redundant
            // UPDATE-to-same-value drops null-padded LEFT JOIN rows).
            let should_repeat = is_dml && self.config.redundant_dml_probability > 0.0 && {
                let roll: f64 = self.rng.borrow_mut().next_u64() as f64 / u64::MAX as f64;
                roll < self.config.redundant_dml_probability
            };
            if should_repeat {
                executed_sql.push(format!("-- REDUNDANT REPEAT: {}", stmt.sql));
                let turso_conn = Arc::clone(&self.turso_conn);
                let sqlite_conn = &self.sqlite_conn;
                let oracle_result = check_differential(&turso_conn, sqlite_conn, &schema, &stmt);
                match oracle_result {
                    OracleResult::Pass | OracleResult::Warning(_) => {
                        stats.statements_executed += 1;
                        executed_sql.push(stmt.sql.clone());
                    }
                    OracleResult::Fail(reason) => {
                        stats.oracle_failures += 1;
                        executed_sql.push(format!("-- FAILED REDUNDANT: {}", stmt.sql));
                        tracing::error!(
                            "Oracle failure on redundant DML at statement {i}: {reason}"
                        );
                        return Err(anyhow::anyhow!("Oracle failure (redundant): {reason}"));
                    }
                }
                if !matview_info.is_empty() {
                    self.verify_matviews_match(&matview_info, stats, executed_sql)?;
                    self.verify_no_empty_cdc_batches(
                        stats,
                        executed_sql,
                        &format!("redundant DML statement {i}: {}", stmt.sql),
                    )?;
                }
            }

            if stmt.is_ddl {
                schema = self
                    .introspect_and_verify_schemas(&matview_info)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Schema mismatch after DDL statement {i} ({}): {e}",
                            stmt.sql
                        )
                    })?;
                tracing::debug!(
                    "Schema updated after DDL: {} tables, {} indexes",
                    schema.tables.len(),
                    schema.indexes.len()
                );
            }
        }

        self.run_integrity_check(stats, executed_sql)?;

        *coverage_out = generator.take_coverage();

        Ok(())
    }

    /// Run `PRAGMA integrity_check` on both databases and fail if either reports corruption.
    fn run_integrity_check(
        &self,
        stats: &mut SimStats,
        executed_sql: &mut Vec<String>,
    ) -> Result<()> {
        if self.config.mvcc {
            tracing::info!("Skipping integrity check (not supported with MVCC)");
            return Ok(());
        }
        tracing::info!("Running integrity check on both databases...");

        let sql = "PRAGMA integrity_check";
        executed_sql.push(sql.to_string());

        let turso_result = DifferentialOracle::execute_turso(&self.turso_conn, sql);
        let sqlite_result = DifferentialOracle::execute_sqlite(&self.sqlite_conn, sql);

        let check_ok = |result: &QueryResult, db_name: &str| -> Result<()> {
            match result {
                QueryResult::Rows(rows) if rows.len() == 1 && rows[0].0.len() == 1 => {
                    if let SqlValue::Text(ref text) = rows[0].0[0] {
                        if text == "ok" {
                            return Ok(());
                        }
                    }
                    bail!("{db_name} integrity check failed: {:?}", rows);
                }
                QueryResult::Rows(rows) => {
                    // Multiple rows means multiple integrity errors
                    bail!("{db_name} integrity check failed: {:?}", rows);
                }
                QueryResult::Error(e) => {
                    bail!("{db_name} integrity check errored: {e}");
                }
                QueryResult::Ok => {
                    bail!("{db_name} integrity check returned no results");
                }
            }
        };

        if let Err(e) = check_ok(&turso_result, "Turso") {
            stats.oracle_failures += 1;
            executed_sql.push(format!("-- FAILED: {sql} ({e})"));
            tracing::error!("{e}");
            return Err(e);
        }

        if let Err(e) = check_ok(&sqlite_result, "SQLite") {
            stats.oracle_failures += 1;
            executed_sql.push(format!("-- FAILED: {sql} ({e})"));
            tracing::error!("{e}");
            return Err(e);
        }

        tracing::info!("Integrity check passed on both databases");
        Ok(())
    }

    /// Introspect schemas from both databases and verify they match.
    /// Matviews are filtered from Turso's table set (matviews appear as tables
    /// in Turso but as views in SQLite, and views aren't introspected as tables).
    fn introspect_and_verify_schemas(
        &self,
        matview_info: &HashMap<String, Vec<sql_gen_prop::ColumnDef>>,
    ) -> Result<sql_gen::Schema> {
        let (turso_schema, sqlite_schema) = (
            SchemaIntrospector::from_turso_with_attached(&self.turso_conn)
                .context("Failed to introspect Turso schema (with attached)")?,
            SchemaIntrospector::from_sqlite_with_attached(&self.sqlite_conn)
                .context("Failed to introspect SQLite schema (with attached)")?,
        );

        // Verify table names match (using qualified names to distinguish databases).
        // Matviews appear as tables in Turso but as views in SQLite, so filter them out.
        let turso_tables: HashSet<_> = turso_schema
            .tables
            .iter()
            .map(|t| t.qualified_name())
            .filter(|name| !matview_info.contains_key(name))
            .collect();
        let sqlite_tables: HashSet<_> = sqlite_schema
            .tables
            .iter()
            .map(|t| t.qualified_name())
            .collect();

        if turso_tables != sqlite_tables {
            bail!(
                "Table mismatch: Turso has {:?}, SQLite has {:?}",
                turso_tables,
                sqlite_tables
            );
        }

        let turso_indexes: std::collections::HashSet<_> = turso_schema
            .indexes
            .iter()
            .map(|i| i.qualified_name())
            .collect();
        let sqlite_indexes: std::collections::HashSet<_> = sqlite_schema
            .indexes
            .iter()
            .map(|i| i.qualified_name())
            .collect();

        if turso_indexes != sqlite_indexes {
            bail!(
                "Index mismatch: Turso has {:?}, SQLite has {:?}",
                turso_indexes,
                sqlite_indexes
            );
        }

        // Verify each table's columns and strict flags match (skip matviews)
        for turso_table in turso_schema
            .tables
            .iter()
            .filter(|t| !matview_info.contains_key(&t.qualified_name()))
        {
            let sqlite_table = sqlite_schema
                .tables
                .iter()
                .find(|t| t.name == turso_table.name && t.database == turso_table.database)
                .expect("Table should exist in SQLite schema");

            if turso_table.strict != sqlite_table.strict {
                bail!(
                    "STRICT mismatch in table '{}': Turso strict={}, SQLite strict={}",
                    turso_table.name,
                    turso_table.strict,
                    sqlite_table.strict
                );
            }

            let turso_cols: Vec<_> = turso_table.columns.iter().map(|c| &c.name).collect();
            let sqlite_cols: Vec<_> = sqlite_table.columns.iter().map(|c| &c.name).collect();

            if turso_cols != sqlite_cols {
                bail!(
                    "Column mismatch in table '{}': Turso has {:?}, SQLite has {:?}",
                    turso_table.qualified_name(),
                    turso_cols,
                    sqlite_cols
                );
            }
        }

        for turso_index in turso_schema.indexes.iter() {
            let sqlite_index = sqlite_schema
                .indexes
                .iter()
                .find(|i| i.name == turso_index.name && i.database == turso_index.database)
                .expect("Index should exist in SQLite schema");

            if turso_index.table_name != sqlite_index.table_name {
                bail!(
                    "Index target mismatch for '{}': Turso targets '{}', SQLite targets '{}'",
                    turso_index.qualified_name(),
                    turso_index.table_name,
                    sqlite_index.table_name
                );
            }

            if turso_index.unique != sqlite_index.unique {
                bail!(
                    "UNIQUE mismatch for index '{}': Turso unique={}, SQLite unique={}",
                    turso_index.qualified_name(),
                    turso_index.unique,
                    sqlite_index.unique
                );
            }

            if turso_index.columns != sqlite_index.columns {
                bail!(
                    "Index column mismatch for '{}': Turso has {:?}, SQLite has {:?}",
                    turso_index.qualified_name(),
                    turso_index.columns,
                    sqlite_index.columns
                );
            }
        }

        Ok(turso_schema)
    }

    /// Close and reopen the Turso database, preserving the same IO layer.
    fn reopen_turso(&mut self) -> Result<()> {
        let mut opts = turso_core::DatabaseOpts::new().with_attach(true);
        if self.config.matview {
            opts = opts.with_views(true);
        }

        let turso_db = Database::open_file_with_flags(
            self.io.clone(),
            self.out_dir.join("test.db").to_str().unwrap(),
            turso_core::OpenFlags::default(),
            opts,
            None,
            Arc::new(SqliteDialect),
        )
        .context("Failed to reopen Turso database")?;

        let turso_conn = turso_db.connect().context("Failed to reconnect to Turso")?;

        turso_conn
            .execute("ATTACH ':memory:' AS aux")
            .context("Failed to re-ATTACH on Turso after reopen")?;

        if self.config.mvcc {
            turso_conn
                .execute("PRAGMA journal_mode = 'experimental_mvcc'")
                .context("Failed to re-enable MVCC after reopen")?;
        }

        self.turso_db = turso_db;
        self.turso_conn = turso_conn;

        tracing::info!("Turso database reopened successfully");
        Ok(())
    }

    /// Verify that all tables and matviews have matching data between Turso and SQLite.
    fn verify_all_tables_match(
        &self,
        schema: &sql_gen::Schema,
        matview_info: &HashMap<String, Vec<sql_gen_prop::ColumnDef>>,
        stats: &mut SimStats,
        executed_sql: &mut Vec<String>,
    ) -> Result<()> {
        for table in &schema.tables {
            let name = &table.name;
            if matview_info.contains_key(name) {
                continue;
            }
            let sql = format!("SELECT * FROM \"{name}\" ORDER BY rowid");
            let turso_result = DifferentialOracle::execute_turso(&self.turso_conn, &sql);
            let sqlite_result = DifferentialOracle::execute_sqlite(&self.sqlite_conn, &sql);
            if turso_result != sqlite_result {
                stats.oracle_failures += 1;
                executed_sql.push(format!(
                    "-- REOPEN VERIFY FAILED: table '{name}' data mismatch"
                ));
                bail!(
                    "Data mismatch after reopen in table '{name}':\n  Turso:  {turso_result:?}\n  SQLite: {sqlite_result:?}"
                );
            }
        }

        for (name, columns) in matview_info.iter() {
            // Use column positions for ORDER BY instead of rowid, since SQLite views don't have rowid
            let order_cols: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, _)| format!("{}", i + 1))
                .collect();
            let order_by = if order_cols.is_empty() {
                String::new()
            } else {
                format!(" ORDER BY {}", order_cols.join(", "))
            };
            let sql = format!("SELECT * FROM \"{name}\"{order_by}");
            let turso_result = DifferentialOracle::execute_turso(&self.turso_conn, &sql);
            let sqlite_result = DifferentialOracle::execute_sqlite(&self.sqlite_conn, &sql);
            // Views may error if their base table was dropped — skip those
            if matches!(sqlite_result, QueryResult::Error(_))
                || matches!(turso_result, QueryResult::Error(_))
            {
                tracing::debug!(
                    "Skipping matview '{name}' verification: query failed on one or both DBs"
                );
                continue;
            }
            if turso_result != sqlite_result {
                stats.oracle_failures += 1;
                executed_sql.push(format!(
                    "-- REOPEN VERIFY FAILED: matview '{name}' data mismatch"
                ));
                bail!(
                    "Data mismatch after reopen in matview '{name}':\n  Turso:  {turso_result:?}\n  SQLite: {sqlite_result:?}"
                );
            }
        }

        tracing::info!("Post-reopen verification passed: all tables/matviews match");
        Ok(())
    }

    /// Drain any empty CDC batches observed since the last call. Empty batches
    /// signal an IVM bug: the matview output didn't change, but a CDC callback
    /// still fired. Returns `Err` if any empty batches were seen.
    fn verify_no_empty_cdc_batches(
        &self,
        stats: &mut SimStats,
        executed_sql: &mut Vec<String>,
        context: &str,
    ) -> Result<()> {
        let drained: HashMap<String, usize> = {
            let mut tally = self.cdc_empty_batches.lock();
            std::mem::take(&mut tally.counts)
        };
        if drained.is_empty() {
            return Ok(());
        }
        stats.oracle_failures += 1;
        let detail = drained
            .iter()
            .map(|(name, n)| format!("{name}={n}"))
            .collect::<Vec<_>>()
            .join(", ");
        executed_sql.push(format!("-- CDC EMPTY BATCH FAILURE ({context}): {detail}"));
        bail!("Empty CDC batches observed ({context}): {detail}");
    }

    /// Verify that all matviews have matching data between Turso and SQLite.
    fn verify_matviews_match(
        &self,
        matview_info: &HashMap<String, Vec<sql_gen_prop::ColumnDef>>,
        stats: &mut SimStats,
        executed_sql: &mut Vec<String>,
    ) -> Result<()> {
        for (name, columns) in matview_info.iter() {
            // Use column names for ORDER BY instead of rowid, since SQLite views don't have rowid
            let order_cols: Vec<String> = columns
                .iter()
                .enumerate()
                .map(|(i, _)| format!("{}", i + 1))
                .collect();
            let order_by = if order_cols.is_empty() {
                String::new()
            } else {
                format!(" ORDER BY {}", order_cols.join(", "))
            };
            let sql = format!("SELECT * FROM \"{name}\"{order_by}");
            let turso_result = DifferentialOracle::execute_turso(&self.turso_conn, &sql);
            let sqlite_result = DifferentialOracle::execute_sqlite(&self.sqlite_conn, &sql);
            if matches!(sqlite_result, QueryResult::Error(_))
                || matches!(turso_result, QueryResult::Error(_))
            {
                continue;
            }
            if turso_result != sqlite_result {
                stats.oracle_failures += 1;
                executed_sql.push(format!("-- MATVIEW VERIFY FAILED: '{name}' data mismatch"));
                bail!(
                    "Matview data mismatch in '{name}':\n  Turso:  {turso_result:?}\n  SQLite: {sqlite_result:?}"
                );
            }
        }
        Ok(())
    }
}

/// Extract the view name from a CREATE [MATERIALIZED] VIEW or DROP VIEW SQL string.
fn extract_view_name_from_sql(sql: &str) -> Option<String> {
    let upper = sql.to_uppercase();
    // Find the position after VIEW [IF NOT EXISTS] / [IF EXISTS]
    let view_pos = upper.find("VIEW")?;
    let after_view = &sql[view_pos + 4..].trim_start();
    let after_clauses = if after_view.to_uppercase().starts_with("IF NOT EXISTS") {
        after_view[13..].trim_start()
    } else if after_view.to_uppercase().starts_with("IF EXISTS") {
        after_view[9..].trim_start()
    } else {
        after_view
    };
    // The next token is the view name (may be followed by space, paren, or AS)
    let name = after_clauses
        .split(|c: char| c.is_whitespace() || c == '(' || c == ';')
        .next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn push_warning_comments(executed_sql: &mut Vec<String>, stmt_idx: usize, reason: &str) {
    for (line_idx, line) in reason.lines().enumerate() {
        executed_sql.push(format!(
            "-- WARNING stmt={stmt_idx} line={line_idx}: {line}"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_sim_config_default() {
        let config = SimConfig::default();
        // seed is now randomly generated by default
        assert!(config.seed > 0);
        assert_eq!(config.num_tables, 2);
        assert_eq!(config.num_statements, 100);
    }

    #[test]
    fn test_simulator_creation() {
        let config = SimConfig {
            seed: 12345,
            num_tables: 1,
            columns_per_table: 3,
            num_statements: 10,
            verbose: false,
            keep_files: false,
            generator: GeneratorKind::default(),
            coverage: false,
            tree_mode: TreeMode::default(),
            mvcc: false,
            matview: false,
            batch_dml_probability: 0.0,
            max_batch_size: 10,
            large_batch_probability: 0.0,
            file_io: false,
            redundant_dml_probability: 0.0,
        };
        let sim = Fuzzer::new(config);
        assert!(sim.is_ok());
    }

    #[test]
    fn test_push_warning_comments_multiline() {
        let mut out = Vec::new();
        push_warning_comments(&mut out, 465, "first\nsecond");
        assert_eq!(out[0], "-- WARNING stmt=465 line=0: first");
        assert_eq!(out[1], "-- WARNING stmt=465 line=1: second");
    }
}
