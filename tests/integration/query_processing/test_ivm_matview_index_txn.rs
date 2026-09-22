//! Transaction sequences around a materialized view.
//!
//! Each step asserts `get_auto_commit()` as well as the statement's own
//! result, so a failure names the statement that lost the transaction rather
//! than the `COMMIT` that noticed.

use std::sync::Arc;

use crate::common::TempDatabase;

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

fn setup(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE t_raw (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t_raw")?;
    conn.execute("INSERT INTO t_raw VALUES ('b0', 'w')")?;
    Ok(())
}

/// A rolled-back transaction must leave the connection able to open the next
/// one and to commit it.
#[turso_macros::test(views)]
fn control_rollback_then_commit_no_index(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
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
