//! A matview WHERE clause holding both a computed expression and a correlated
//! subquery must be refused at DDL, the way the subquery is refused on its own.
//!
//! The computed expression sends the whole predicate through the IVM compiler's
//! projection rewrite, which carries one sub-expression in a temp column and points
//! every other complex conjunct at that same column. The subquery never reaches the
//! conversion that rejects it, so CREATE reports success for a circuit that cannot
//! compute the query and the view answers empty forever.

use crate::common::{limbo_exec_rows, limbo_exec_rows_fallible, TempDatabase};
use std::sync::Arc;
use turso_core::Connection;

fn seed(conn: &Arc<Connection>) {
    limbo_exec_rows(
        conn,
        "CREATE TABLE block (id INTEGER PRIMARY KEY, properties TEXT)",
    );
    limbo_exec_rows(
        conn,
        "CREATE TABLE block_requires (block_id INTEGER, required_id INTEGER)",
    );
    limbo_exec_rows(conn, "CREATE TABLE block_tags (block_id INTEGER, tag TEXT)");
    limbo_exec_rows(
        conn,
        "INSERT INTO block VALUES \
         (1, '{\"task_state\":\"TODO\"}'), (2, '{\"task_state\":\"TODO\"}'), \
         (3, '{\"task_state\":\"DONE\"}')",
    );
    limbo_exec_rows(conn, "INSERT INTO block_requires VALUES (2, 3)");
    limbo_exec_rows(conn, "INSERT INTO block_tags VALUES (1, 'agent')");
}

#[track_caller]
fn assert_ddl_refused(db: &TempDatabase, conn: &Arc<Connection>, sql: &str) {
    let err = limbo_exec_rows_fallible(db, conn, sql)
        .expect_err("a correlated subquery in a matview WHERE has no IVM support");
    assert!(
        err.to_string()
            .contains("Cannot convert LogicalExpr to AST Expr"),
        "unexpected DDL error: {err}"
    );
}

#[test]
fn computed_expression_beside_not_exists_is_refused() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    assert_ddl_refused(
        &db,
        &conn,
        "CREATE MATERIALIZED VIEW ready AS SELECT b.id FROM block b \
         WHERE json_extract(b.properties, '$.task_state') = 'TODO' \
           AND NOT EXISTS (SELECT 1 FROM block_requires br WHERE br.block_id = b.id)",
    );
}

#[test]
fn computed_expression_beside_or_of_exists_is_refused() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    assert_ddl_refused(
        &db,
        &conn,
        "CREATE MATERIALIZED VIEW ready AS SELECT b.id FROM block b \
         WHERE json_extract(b.properties, '$.task_state') = 'TODO' \
           AND (EXISTS (SELECT 1 FROM block_tags bt WHERE bt.block_id = b.id AND bt.tag = 'agent') \
                OR NOT EXISTS (SELECT 1 FROM block_tags bt WHERE bt.block_id = b.id AND bt.tag = 'human-only'))",
    );
}

/// The same predicate over matview sources rather than base tables.
#[test]
fn computed_expression_beside_not_exists_is_refused_over_matview_sources() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    limbo_exec_rows(
        &conn,
        "CREATE TABLE block_raw (id INTEGER PRIMARY KEY, properties TEXT)",
    );
    limbo_exec_rows(
        &conn,
        "CREATE TABLE req_raw (block_id INTEGER, required_id INTEGER)",
    );
    limbo_exec_rows(
        &conn,
        "INSERT INTO block_raw VALUES (1, '{\"task_state\":\"TODO\"}'), (2, '{\"task_state\":\"TODO\"}')",
    );
    limbo_exec_rows(&conn, "INSERT INTO req_raw VALUES (2, 3)");
    limbo_exec_rows(
        &conn,
        "CREATE MATERIALIZED VIEW block AS SELECT id, properties FROM block_raw",
    );
    limbo_exec_rows(
        &conn,
        "CREATE MATERIALIZED VIEW block_requires AS SELECT block_id, required_id FROM req_raw",
    );

    assert_ddl_refused(
        &db,
        &conn,
        "CREATE MATERIALIZED VIEW ready AS SELECT b.id FROM block b \
         WHERE json_extract(b.properties, '$.task_state') = 'TODO' \
           AND NOT EXISTS (SELECT 1 FROM block_requires br WHERE br.block_id = b.id)",
    );
}

/// Guards the fix against over-reach: a computed expression with no subquery beside
/// it still compiles and materializes.
#[test]
fn computed_expression_alone_still_materializes() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    limbo_exec_rows(
        &conn,
        "CREATE MATERIALIZED VIEW todos AS SELECT b.id FROM block b \
         WHERE json_extract(b.properties, '$.task_state') = 'TODO'",
    );
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id FROM todos ORDER BY id"),
        limbo_exec_rows(
            &conn,
            "SELECT b.id FROM block b \
             WHERE json_extract(b.properties, '$.task_state') = 'TODO' ORDER BY b.id"
        )
    );
}
