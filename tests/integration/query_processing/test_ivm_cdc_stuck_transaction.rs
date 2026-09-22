//! Test: CDC callback goes silent when connection is stuck in a transaction.
//!
//! Production scenario: A failed BEGIN (due to an already-active transaction)
//! leaves the connection in a transaction state. Subsequent auto-commit DML
//! operations don't actually commit, so apply_view_deltas never runs and
//! CDC callbacks never fire.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::common::TempDatabase;

/// When a transaction is left open, CDC callbacks stop firing for auto-commit DML.
#[turso_macros::test(views)]
fn test_cdc_silent_when_stuck_in_transaction(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, val TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, val FROM t")?;

    let cdc_counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let counts_clone = cdc_counts.clone();
    conn.set_change_callback(move |event| {
        let mut counts = counts_clone.lock().unwrap();
        *counts.entry(event.relation_name.clone()).or_insert(0) += event.changes.len();
    });

    // Verify CDC works normally
    conn.execute("INSERT INTO t VALUES ('a', 'hello')")?;
    {
        let counts = cdc_counts.lock().unwrap();
        assert!(
            counts.get("mv").copied().unwrap_or(0) > 0,
            "CDC should fire in auto-commit mode"
        );
    }

    // Now simulate the stuck transaction scenario:
    // 1. Start an explicit transaction
    conn.execute("BEGIN TRANSACTION")?;
    conn.execute("INSERT INTO t VALUES ('b', 'in-tx')")?;

    // 2. Simulate a crash/error where COMMIT/ROLLBACK never happens.
    //    In production, this happens when a batch transaction's commit fails
    //    and the rollback is best-effort (and also fails or is skipped).

    // 3. Try to BEGIN again — this should fail
    let begin_result = conn.execute("BEGIN TRANSACTION");
    assert!(
        begin_result.is_err(),
        "BEGIN should fail when already in a transaction"
    );

    // 4. Reset CDC counts
    {
        let mut counts = cdc_counts.lock().unwrap();
        counts.clear();
    }

    // 5. Now do "auto-commit" style DML — but we're still in the old transaction!
    conn.execute("INSERT INTO t VALUES ('c', 'after-stuck')")?;

    // 6. Check: CDC should NOT have fired because we're still in the transaction
    {
        let counts = cdc_counts.lock().unwrap();
        let mv_count = counts.get("mv").copied().unwrap_or(0);
        // This is the bug: no CDC because the commit never happens
        assert_eq!(
            mv_count, 0,
            "CDC should NOT fire when stuck in an uncommitted transaction. CDC: {:?}",
            *counts
        );
    }

    // 7. Now rollback to fix the connection state
    conn.execute("ROLLBACK")?;

    // 8. After rollback, CDC should work again
    {
        let mut counts = cdc_counts.lock().unwrap();
        counts.clear();
    }

    conn.execute("INSERT INTO t VALUES ('d', 'after-rollback')")?;
    {
        let counts = cdc_counts.lock().unwrap();
        let mv_count = counts.get("mv").copied().unwrap_or(0);
        assert!(
            mv_count > 0,
            "CDC should fire again after ROLLBACK clears stuck transaction. CDC: {:?}",
            *counts
        );
    }

    Ok(())
}

/// Holon production pattern: batch transaction BEGIN fails, connection stuck.
/// Subsequent single-statement executions (which should auto-commit) don't trigger CDC.
#[turso_macros::test(views)]
fn test_cdc_silent_after_failed_begin(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, val TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, val FROM t")?;

    let cdc_counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let counts_clone = cdc_counts.clone();
    conn.set_change_callback(move |event| {
        let mut counts = counts_clone.lock().unwrap();
        *counts.entry(event.relation_name.clone()).or_insert(0) += event.changes.len();
    });

    // Simulate: batch transaction starts but commit fails, leaving tx open
    conn.execute("BEGIN TRANSACTION")?;
    conn.execute("INSERT INTO t VALUES ('batch1', 'data')")?;
    // Simulate commit failure — just skip COMMIT, leaving tx dangling

    // Now simulate another batch transaction attempt (this is what the error log shows)
    let result = conn.execute("BEGIN TRANSACTION");
    assert!(result.is_err(), "Second BEGIN should fail");

    // At this point the connection is stuck. The holon code logs the error
    // and continues. But ALL subsequent operations happen inside the zombie tx.

    // Reset CDC
    {
        let mut counts = cdc_counts.lock().unwrap();
        counts.clear();
    }

    // Normal DML — but it's inside the zombie transaction, no commit, no CDC
    conn.execute("INSERT INTO t VALUES ('normal1', 'data')")?;

    {
        let counts = cdc_counts.lock().unwrap();
        let mv_count = counts.get("mv").copied().unwrap_or(0);
        // This confirms the hypothesis: stuck transaction → no CDC
        assert_eq!(
            mv_count, 0,
            "CDC silent because connection is stuck in zombie transaction. CDC: {:?}",
            *counts
        );
    }

    Ok(())
}
