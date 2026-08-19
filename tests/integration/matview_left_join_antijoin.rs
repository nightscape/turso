//! A matview whose WHERE holds `<right>.<col> IS NULL` over a LEFT JOIN — the
//! anti-join idiom — must materialize the same rows the same SELECT returns.
//!
//! `generate_populate_scans` gives each source table its own scan and hands the
//! WHERE conjuncts that name only that table to the scan as a WHERE. On the
//! null-supplying side of an outer join the conjunct describes the padding the
//! join produces, not the rows the scan reads, so it belongs above the join.
//! `IncrementalView::extract_conditions_for_table` already skips pushdown for
//! such tables (via `null_extended_tables`), so this fix is ready — but
//! `JoinOperator::new` (core/incremental/join_operator.rs) currently refuses
//! `LEFT`/`RIGHT`/`FULL`/`CROSS` joins in materialized views outright, so the
//! populate-scan path below is unreachable today. The test asserts that
//! refusal instead of matview correctness; swap it back to
//! `assert_matches_recompute` once outer joins are supported.

use crate::common::{limbo_exec_rows, limbo_exec_rows_fallible, TempDatabase};
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

#[test]
fn antijoin_matview_is_refused_at_ddl() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    let err = limbo_exec_rows_fallible(
        &db,
        &conn,
        &format!("CREATE MATERIALIZED VIEW unblocked AS {VIEW_SELECT}"),
    )
    .expect_err("LEFT JOIN has no IVM compiler support");
    assert!(
        err.to_string()
            .contains("LEFT OUTER JOIN is not yet supported in incremental views"),
        "unexpected DDL error: {err}"
    );
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
