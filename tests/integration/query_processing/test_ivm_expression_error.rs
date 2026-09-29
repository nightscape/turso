//! An expression in a materialized view that fails on one row fails the write
//! that brought the row in. The write rolls back, the view keeps its old
//! content, and the connection stays usable.

use std::sync::Arc;

use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value;
use turso_core::Connection;

const VIEW_SQL: &str = "CREATE MATERIALIZED VIEW parsed AS SELECT id, json(doc) AS j FROM t";

fn open_with_view() -> (TempDatabase, Arc<Connection>) {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, doc TEXT)")
        .unwrap();
    conn.execute(r#"INSERT INTO t VALUES (1, '{"a":1}')"#)
        .unwrap();
    conn.execute(VIEW_SQL).unwrap();
    (db, conn)
}

fn view_rows(conn: &Arc<Connection>) -> Vec<Vec<Value>> {
    limbo_exec_rows(conn, "SELECT id, j FROM parsed ORDER BY id")
}

fn recomputed_rows(conn: &Arc<Connection>) -> Vec<Vec<Value>> {
    limbo_exec_rows(conn, "SELECT id, json(doc) FROM t ORDER BY id")
}

fn assert_malformed_json(result: turso_core::Result<()>) {
    let err = result.expect_err("a row the view cannot compute must fail the write");
    assert!(
        err.to_string().contains("malformed JSON"),
        "unexpected error: {err}"
    );
}

#[test]
fn failing_view_expression_fails_an_autocommit_insert() {
    let (_db, conn) = open_with_view();
    let before = view_rows(&conn);

    assert_malformed_json(conn.execute("INSERT INTO t VALUES (2, 'not json')"));

    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM t"),
        vec![vec![Value::Integer(1)]],
        "the failed insert must not stay in the base table"
    );
    assert_eq!(view_rows(&conn), before);

    conn.execute(r#"INSERT INTO t VALUES (3, '{"b":2}')"#)
        .unwrap();
    assert_eq!(view_rows(&conn), recomputed_rows(&conn));
}

#[test]
fn failing_view_expression_fails_an_update() {
    let (_db, conn) = open_with_view();
    let before = view_rows(&conn);

    assert_malformed_json(conn.execute("UPDATE t SET doc = 'not json' WHERE id = 1"));

    assert_eq!(view_rows(&conn), before);
    assert_eq!(view_rows(&conn), recomputed_rows(&conn));
}

#[test]
fn failing_view_expression_fails_an_explicit_transaction() {
    let (_db, conn) = open_with_view();
    let before = view_rows(&conn);

    conn.execute("BEGIN").unwrap();
    conn.execute(r#"INSERT INTO t VALUES (2, '{"c":3}')"#)
        .unwrap();
    conn.execute("INSERT INTO t VALUES (3, 'not json')")
        .unwrap();
    assert_malformed_json(conn.execute("COMMIT"));
    assert!(
        conn.get_auto_commit(),
        "the failed COMMIT must end the transaction"
    );

    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM t"),
        vec![vec![Value::Integer(1)]],
        "the failed transaction must not stay in the base table"
    );
    assert_eq!(view_rows(&conn), before);

    conn.execute(r#"INSERT INTO t VALUES (3, '{"b":2}')"#)
        .unwrap();
    assert_eq!(view_rows(&conn), recomputed_rows(&conn));
}

#[test]
fn failing_view_expression_fails_create_over_existing_rows() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, doc TEXT)")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'not json')")
        .unwrap();

    assert_malformed_json(conn.execute(VIEW_SQL));

    assert_eq!(
        limbo_exec_rows(
            &conn,
            "SELECT count(*) FROM sqlite_schema WHERE name = 'parsed'"
        ),
        vec![vec![Value::Integer(0)]],
        "a failed CREATE must not leave the view behind"
    );
}
