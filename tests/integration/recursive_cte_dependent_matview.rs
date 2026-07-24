//! Regression: a matview that selects FROM a recursive-CTE matview must not
//! return every row twice when it is read inside an open transaction.
//!
//! A recursive-CTE matview cannot compute an incremental delta for uncommitted
//! data, so `IncrementalView::execute_with_uncommitted` evaluates the view's SQL
//! directly and returns the COMPLETE result, flagged with `is_full_result`. A
//! cursor reading that view honours the flag and ignores its btree. A cursor
//! reading a DEPENDENT view used to discard the flag and hand the complete
//! result to its own circuit as if it were an ordinary delta, so the dependent
//! added the whole upstream result on top of the rows it already held on disk.

use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value;
use std::sync::Arc;
use turso_core::Connection;

/// `node` is a parent chain `1 <- 2 <- 3`; `reach` walks it recursively and
/// `reach_ids` is a plain projection over `reach`.
fn seed(conn: &Arc<Connection>) {
    limbo_exec_rows(
        conn,
        "CREATE TABLE node (id INTEGER PRIMARY KEY, parent_id INTEGER)",
    );
    limbo_exec_rows(conn, "INSERT INTO node VALUES (1, NULL), (2, 1), (3, 2)");
    limbo_exec_rows(
        conn,
        "CREATE MATERIALIZED VIEW reach AS \
         WITH RECURSIVE walk(id, depth) AS ( \
             SELECT id, 0 FROM node WHERE parent_id IS NULL \
             UNION ALL \
             SELECT n.id, w.depth + 1 FROM node n JOIN walk w ON n.parent_id = w.id \
         ) \
         SELECT id, depth FROM walk",
    );
    limbo_exec_rows(
        conn,
        "CREATE MATERIALIZED VIEW reach_ids AS SELECT id FROM reach",
    );
    limbo_exec_rows(
        conn,
        "CREATE MATERIALIZED VIEW reach_count AS SELECT count(*) AS c FROM reach",
    );
}

fn ids(rows: Vec<Vec<Value>>) -> Vec<i64> {
    let mut out: Vec<i64> = rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref other => panic!("expected integer, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

#[track_caller]
fn one(rows: Vec<Vec<Value>>) -> i64 {
    assert_eq!(rows.len(), 1, "expected a single row, got {rows:?}");
    ids(rows)[0]
}

#[test]
fn dependent_of_recursive_matview_reads_once_inside_a_transaction() {
    let db = TempDatabase::builder().with_views(true).build();
    let conn = db.connect_limbo();
    seed(&conn);

    assert_eq!(
        ids(limbo_exec_rows(&conn, "SELECT id FROM reach_ids")),
        vec![1, 2, 3],
        "committed read must return the three seeded nodes"
    );
    assert_eq!(
        one(limbo_exec_rows(&conn, "SELECT c FROM reach_count")),
        3,
        "committed aggregate over the recursive matview must count three nodes"
    );

    limbo_exec_rows(&conn, "BEGIN");
    limbo_exec_rows(&conn, "INSERT INTO node VALUES (4, 3)");

    assert_eq!(
        ids(limbo_exec_rows(&conn, "SELECT id FROM reach")),
        vec![1, 2, 3, 4],
        "the recursive matview itself must see the uncommitted node exactly once"
    );
    assert_eq!(
        ids(limbo_exec_rows(&conn, "SELECT id FROM reach_ids")),
        vec![1, 2, 3, 4],
        "the dependent projection must not add the upstream's complete result \
         on top of its own committed rows"
    );
    // The aggregate is where the accumulation is visible: adding the upstream's
    // complete result (4 rows) to a count that already stands at 3 gives 7.
    assert_eq!(
        one(limbo_exec_rows(&conn, "SELECT c FROM reach_count")),
        4,
        "the dependent aggregate must count each row of the recursive matview once"
    );

    limbo_exec_rows(&conn, "COMMIT");

    assert_eq!(
        ids(limbo_exec_rows(&conn, "SELECT id FROM reach_ids")),
        vec![1, 2, 3, 4],
        "the committed projection after COMMIT must agree with the in-transaction read"
    );
    assert_eq!(
        one(limbo_exec_rows(&conn, "SELECT c FROM reach_count")),
        4,
        "the committed aggregate after COMMIT must agree with the in-transaction read"
    );
}
