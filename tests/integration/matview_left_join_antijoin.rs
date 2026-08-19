//! A matview whose WHERE holds `<right>.<col> IS NULL` over a LEFT JOIN — the
//! anti-join idiom — must materialize the same rows the same SELECT returns.
//!
//! `generate_populate_scans` gives each source table its own scan and hands the
//! WHERE conjuncts that name only that table to the scan as a WHERE. On the
//! null-supplying side of an outer join the conjunct describes the padding the
//! join produces, not the rows the scan reads, so it belongs above the join.

use crate::common::{limbo_exec_rows, limbo_exec_rows_fallible, TempDatabase};
use rusqlite::types::Value;
use std::sync::Arc;
use turso_core::Connection;

const VIEW_SELECT: &str = "SELECT b.id, br.required_id \
     FROM block b LEFT JOIN block_requires br ON br.block_id = b.id \
     WHERE br.block_id IS NULL";

fn seed(conn: &Arc<Connection>) {
    limbo_exec_rows(
        conn,
        "CREATE TABLE block (id INTEGER PRIMARY KEY, properties TEXT)",
    );
    limbo_exec_rows(
        conn,
        "CREATE TABLE block_requires (block_id INTEGER, required_id INTEGER)",
    );
    limbo_exec_rows(
        conn,
        "INSERT INTO block VALUES \
         (1, '{\"task_state\":\"TODO\"}'), (2, '{\"task_state\":\"TODO\"}'), \
         (3, '{\"task_state\":\"DONE\"}'), (4, '{\"task_state\":\"TODO\"}')",
    );
    limbo_exec_rows(conn, "INSERT INTO block_requires VALUES (2, 3), (4, 1)");
}

fn rows(conn: &Arc<Connection>, sql: &str) -> Vec<Vec<Value>> {
    let mut out = limbo_exec_rows(conn, sql);
    out.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
    out
}

#[track_caller]
fn assert_matches_recompute(conn: &Arc<Connection>, stage: &str) {
    assert_eq!(
        rows(conn, "SELECT id, required_id FROM unblocked"),
        rows(conn, VIEW_SELECT),
        "matview disagrees with a fresh recompute of the same SELECT after {stage}"
    );
}

#[test]
fn antijoin_matview_matches_recompute_across_maintenance() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    limbo_exec_rows(
        &conn,
        &format!("CREATE MATERIALIZED VIEW unblocked AS {VIEW_SELECT}"),
    );
    assert_matches_recompute(&conn, "materializing over pre-existing rows");

    limbo_exec_rows(&conn, "INSERT INTO block_requires VALUES (1, 3)");
    assert_matches_recompute(&conn, "inserting a requirement for block 1");

    limbo_exec_rows(&conn, "DELETE FROM block_requires WHERE block_id = 2");
    assert_matches_recompute(&conn, "deleting block 2's only requirement");

    limbo_exec_rows(
        &conn,
        "UPDATE block SET properties = '{\"task_state\":\"DONE\"}' WHERE id = 4",
    );
    assert_matches_recompute(&conn, "updating an outer row's properties");
}

/// The correlated `NOT EXISTS` spelling of the same anti-join. The IVM compiler
/// has no `LogicalExpr::Exists` arm, so this is refused at DDL time.
#[test]
fn correlated_not_exists_matview_is_refused_at_ddl() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    let err = limbo_exec_rows_fallible(
        &db,
        &conn,
        "CREATE MATERIALIZED VIEW unblocked AS SELECT b.id FROM block b \
         WHERE NOT EXISTS (SELECT 1 FROM block_requires br WHERE br.block_id = b.id)",
    )
    .expect_err("NOT EXISTS has no IVM compiler support");
    assert!(
        err.to_string()
            .contains("Cannot convert LogicalExpr to AST Expr"),
        "unexpected DDL error: {err}"
    );
}
