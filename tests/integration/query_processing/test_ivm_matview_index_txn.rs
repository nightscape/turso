//! Transaction sequences around an indexed materialized view.
//!
//! An index on a view must not change when a transaction is open. Each step
//! asserts `get_auto_commit()` as well as the statement's own result, so a
//! failure names the statement that lost the transaction rather than the
//! `COMMIT` that noticed.

use std::sync::Arc;

use rusqlite::types::Value;

use crate::common::{limbo_exec_rows, TempDatabase};

fn step(conn: &Arc<turso_core::Connection>, sql: &str, in_txn_after: bool) -> anyhow::Result<()> {
    conn.execute(sql)?;
    assert_eq!(
        conn.get_auto_commit(),
        !in_txn_after,
        "after `{sql}` the connection should {} be in a transaction",
        if in_txn_after { "" } else { "not" }
    );
    Ok(())
}

fn setup(conn: &Arc<turso_core::Connection>, with_index: bool) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t_raw")?;
    if with_index {
        conn.execute("CREATE INDEX idx_v_id ON v(id)")?;
    }
    conn.execute("INSERT INTO t_raw VALUES ('b0', 'w')")?;
    Ok(())
}

/// A rolled-back transaction must leave the connection able to open the next
/// one. With an index on the view the second `BEGIN` did not take effect, so
/// the write after it auto-committed and `COMMIT` reported no active
/// transaction — atomicity lost, not merely a spurious error.
#[turso_macros::test(views)]
fn rollback_then_commit_on_an_indexed_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn, true)?;

    step(&conn, "BEGIN", true)?;
    step(&conn, "INSERT INTO t_raw VALUES ('c0', 'x')", true)?;
    step(&conn, "ROLLBACK", false)?;

    step(&conn, "BEGIN", true)?;
    step(&conn, "INSERT INTO t_raw VALUES ('d0', 'y')", true)?;
    step(&conn, "COMMIT", false)?;

    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id FROM v WHERE +id > '' ORDER BY id"),
        vec![
            vec![Value::Text("b0".into())],
            vec![Value::Text("d0".into())]
        ],
        "the rolled-back row must be gone and the committed one present"
    );
    Ok(())
}

/// The second transaction must still be rollback-able: if its `BEGIN` was
/// lost the write is already durable and this leaves 'd0' behind.
#[turso_macros::test(views)]
fn rollback_then_rollback_on_an_indexed_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn, true)?;

    step(&conn, "BEGIN", true)?;
    step(&conn, "INSERT INTO t_raw VALUES ('c0', 'x')", true)?;
    step(&conn, "ROLLBACK", false)?;

    step(&conn, "BEGIN", true)?;
    step(&conn, "INSERT INTO t_raw VALUES ('d0', 'y')", true)?;
    step(&conn, "ROLLBACK", false)?;

    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id FROM v WHERE +id > '' ORDER BY id"),
        vec![vec![Value::Text("b0".into())]],
        "both rolled-back rows must be gone"
    );
    Ok(())
}

/// Every combination of first-transaction ending and index presence, so a
/// failure says which leg broke rather than only that one did.
#[turso_macros::test(views)]
fn matrix_of_transaction_sequences(tmp_db: TempDatabase) -> anyhow::Result<()> {
    for with_index in [false, true] {
        for first_end in ["ROLLBACK", "COMMIT"] {
            let conn = tmp_db.connect_limbo();
            let sfx = format!("{}_{first_end}", if with_index { "idx" } else { "noidx" });
            conn.execute(&format!(
                "CREATE TABLE r_{sfx} (id TEXT PRIMARY KEY, st TEXT)"
            ))?;
            conn.execute(&format!(
                "CREATE MATERIALIZED VIEW v_{sfx} AS SELECT id, st FROM r_{sfx}"
            ))?;
            if with_index {
                conn.execute(&format!("CREATE INDEX i_{sfx} ON v_{sfx}(id)"))?;
            }
            conn.execute(&format!("INSERT INTO r_{sfx} VALUES ('b0', 'w')"))?;

            conn.execute("BEGIN")?;
            conn.execute(&format!("INSERT INTO r_{sfx} VALUES ('c0', 'x')"))?;
            conn.execute(first_end)?;

            conn.execute("BEGIN")?;
            assert!(
                !conn.get_auto_commit(),
                "[index={with_index}, first={first_end}] the second BEGIN did not \
                 open a transaction"
            );
            conn.execute(&format!("INSERT INTO r_{sfx} VALUES ('d0', 'y')"))?;
            conn.execute("COMMIT").map_err(|e| {
                anyhow::anyhow!("[index={with_index}, first={first_end}] COMMIT failed: {e}")
            })?;
        }
    }
    Ok(())
}

/// A read of the view through its index between the two transactions must not
/// change the outcome either.
#[turso_macros::test(views)]
fn rollback_then_indexed_read_then_commit(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn, true)?;

    step(&conn, "BEGIN", true)?;
    step(&conn, "INSERT INTO t_raw VALUES ('c0', 'x')", true)?;
    let _ = limbo_exec_rows(&conn, "SELECT id FROM v WHERE id = 'c0'");
    step(&conn, "ROLLBACK", false)?;
    let _ = limbo_exec_rows(&conn, "SELECT id FROM v WHERE id = 'b0'");

    step(&conn, "BEGIN", true)?;
    step(&conn, "INSERT INTO t_raw VALUES ('d0', 'y')", true)?;
    step(&conn, "COMMIT", false)?;
    Ok(())
}

/// The same sequence on a matview with NO index. Present on upstream main
/// before this series: any extra I/O in the commit path exposes it, and an
/// index is only one way to get there.
#[turso_macros::test(views)]
fn control_rollback_then_commit_no_index(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn, false)?;
    step(&conn, "BEGIN", true)?;
    step(&conn, "INSERT INTO t_raw VALUES ('c0', 'x')", true)?;
    step(&conn, "ROLLBACK", false)?;
    step(&conn, "BEGIN", true)?;
    step(&conn, "INSERT INTO t_raw VALUES ('d0', 'y')", true)?;
    step(&conn, "COMMIT", false)?;
    Ok(())
}

/// Control: the same sequence with no materialized view at all.
#[turso_macros::test(views)]
fn control_rollback_then_commit_plain_table(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE INDEX idx_raw ON t_raw(st)")?;
    conn.execute("INSERT INTO t_raw VALUES ('b0', 'w')")?;
    step(&conn, "BEGIN", true)?;
    step(&conn, "INSERT INTO t_raw VALUES ('c0', 'x')", true)?;
    step(&conn, "ROLLBACK", false)?;
    step(&conn, "BEGIN", true)?;
    step(&conn, "INSERT INTO t_raw VALUES ('d0', 'y')", true)?;
    step(&conn, "COMMIT", false)?;
    Ok(())
}

/// The verifier's scenario shape: a read of the view BETWEEN the two
/// transactions. It is what makes the unindexed leg pass, so it isolates
/// whatever the index adds on top.
fn scenario_with_read_between(
    tmp_db: &TempDatabase,
    with_index: bool,
    first_end: &str,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let sfx = format!("{}_{first_end}", if with_index { "i" } else { "n" });
    conn.execute(&format!(
        "CREATE TABLE r{sfx} (id TEXT PRIMARY KEY, st TEXT)"
    ))?;
    conn.execute(&format!(
        "CREATE MATERIALIZED VIEW w{sfx} AS SELECT id, st FROM r{sfx}"
    ))?;
    if with_index {
        conn.execute(&format!("CREATE INDEX ix{sfx} ON w{sfx}(id)"))?;
    }
    conn.execute(&format!("INSERT INTO r{sfx} VALUES ('b0','s0')"))?;

    conn.execute("BEGIN")?;
    conn.execute(&format!("INSERT INTO r{sfx} VALUES ('c0','x')"))?;
    conn.execute(first_end)?;
    let _ = limbo_exec_rows(
        &conn,
        &format!("SELECT count(*) FROM (SELECT +id AS z FROM w{sfx})"),
    );
    conn.execute("BEGIN")?;
    conn.execute(&format!("INSERT INTO r{sfx} VALUES ('d0','y')"))?;
    conn.execute("COMMIT")
        .map_err(|e| anyhow::anyhow!("[index={with_index} first={first_end}] COMMIT: {e}"))?;
    Ok(())
}

#[turso_macros::test(views)]
fn read_between_transactions_no_index(tmp_db: TempDatabase) -> anyhow::Result<()> {
    scenario_with_read_between(&tmp_db, false, "ROLLBACK")
}

#[turso_macros::test(views)]
fn read_between_transactions_with_index(tmp_db: TempDatabase) -> anyhow::Result<()> {
    scenario_with_read_between(&tmp_db, true, "ROLLBACK")
}
