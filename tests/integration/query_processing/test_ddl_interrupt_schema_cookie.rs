//! Regression: an interrupted `CREATE TABLE ... AS SELECT` inside an explicit
//! transaction doomed the transaction — every later statement returned
//! `SchemaUpdated` until ROLLBACK.
//!
//! The DDL bumps the schema cookie before its long copy loop, which refreshes
//! the pager's cached cookie. Statement rollback undid the pages and the
//! in-memory schema but not that cache, so `CheckSchemaCookie` compared the
//! cached bumped value against the prepared restored one forever.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::common::{self, ExecRows, TempDatabase};
use turso_core::StepResult;

/// Runs `ddl` under a progress handler that interrupts after `steps` VM steps.
/// Returns true if the statement was actually interrupted.
///
/// `ops = 1` is required: `should_interrupt` fires when `vm_steps % ops == 0`,
/// and `0 % ops == 0`, so any larger interval interrupts at step 0 instead of
/// mid-flight.
fn run_interrupted(
    tmp_db: &TempDatabase,
    conn: &Arc<turso_core::Connection>,
    ddl: &str,
    steps: usize,
) -> anyhow::Result<bool> {
    let seen = Arc::new(AtomicUsize::new(0));
    let seen_cb = seen.clone();
    conn.set_progress_handler(
        1,
        Some(Box::new(move || {
            seen_cb.fetch_add(1, Ordering::SeqCst) >= steps
        })),
    );

    let mut stmt = conn.prepare(ddl)?;
    let interrupted = loop {
        match stmt.step()? {
            StepResult::Interrupt | StepResult::Busy => break true,
            StepResult::Done => break false,
            StepResult::IO => tmp_db.io.step()?,
            StepResult::Row | StepResult::Yield => {}
        }
    };
    drop(stmt);
    conn.set_progress_handler(0, None);
    Ok(interrupted)
}

fn seed_t1(tmp_db: &TempDatabase, conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    common::run_query(tmp_db, conn, "CREATE TABLE t1(a INTEGER, b TEXT)")?;
    common::run_query(tmp_db, conn, "INSERT INTO t1 VALUES (1,'row-1')")?;
    for _ in 0..13 {
        common::run_query(
            tmp_db,
            conn,
            "INSERT INTO t1 SELECT a + (SELECT max(a) FROM t1), b FROM t1",
        )?;
    }
    Ok(())
}

#[turso_macros::test]
fn test_interrupted_ctas_in_txn_does_not_doom(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    seed_t1(&tmp_db, &conn)?;

    common::run_query(&tmp_db, &conn, "BEGIN")?;

    // 20_000 VM steps is past the SetCookie/ParseSchema that create t2 and deep
    // inside the row-copy loop.
    let interrupted = run_interrupted(
        &tmp_db,
        &conn,
        "CREATE TABLE t2 AS SELECT * FROM t1",
        20_000,
    )?;
    assert!(interrupted, "DDL completed before the interrupt fired");

    // Before the fix every one of these failed with `Database schema changed`.
    let rows: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM t1");
    assert_eq!(rows, vec![(8192,)]);
    common::run_query(&tmp_db, &conn, "INSERT INTO t1 VALUES (99999,'x')")?;
    common::run_query(&tmp_db, &conn, "CREATE TABLE t3(z)")?;

    // The transaction is still live and still rolls back cleanly.
    assert!(!conn.get_auto_commit());
    common::run_query(&tmp_db, &conn, "ROLLBACK")?;

    let rows: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM t1");
    assert_eq!(rows, vec![(8192,)]);
    let rows: Vec<(i64,)> =
        conn.exec_rows("SELECT count(*) FROM sqlite_schema WHERE name IN ('t2','t3')");
    assert_eq!(rows, vec![(0,)]);

    Ok(())
}

/// `CREATE INDEX` bumps the cookie *after* its build loop, so an interrupt
/// lands before the cache is ever poisoned. Guards against a future reordering
/// of `SetCookie` into the build giving it the same window.
#[turso_macros::test]
fn test_interrupted_create_index_in_txn_does_not_doom(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    seed_t1(&tmp_db, &conn)?;

    common::run_query(&tmp_db, &conn, "BEGIN")?;

    let interrupted = run_interrupted(&tmp_db, &conn, "CREATE INDEX ix1 ON t1(b)", 5_000)?;
    assert!(interrupted, "CREATE INDEX completed before the interrupt");

    let rows: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM t1");
    assert_eq!(rows, vec![(8192,)]);
    common::run_query(&tmp_db, &conn, "CREATE TABLE t3(z)")?;
    common::run_query(&tmp_db, &conn, "ROLLBACK")?;

    let rows: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM t1");
    assert_eq!(rows, vec![(8192,)]);

    Ok(())
}
