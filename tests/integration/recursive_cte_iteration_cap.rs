//! Regression test against silent truncation of deep recursive CTEs.
//!
//! Bounding a runaway recursion is the job of interruption and the query deadline
//! (`tests/integration/recursive_cte_runaway.rs`), not of an iteration cap.

use crate::common::{limbo_exec_rows, TempDatabase};
use std::sync::Arc;
use turso_core::Connection;

/// Builds a single parent chain `n0 -> n1 -> ... -> n{len-1}`, i.e. depth `len`.
fn seed_chain(conn: &Arc<Connection>, len: usize) {
    limbo_exec_rows(
        conn,
        "CREATE TABLE node (id TEXT PRIMARY KEY, parent_id TEXT)",
    );
    limbo_exec_rows(conn, "BEGIN");
    limbo_exec_rows(conn, "INSERT INTO node VALUES ('n0', NULL)");
    for i in 1..len {
        limbo_exec_rows(
            conn,
            &format!("INSERT INTO node VALUES ('n{i}', 'n{}')", i - 1),
        );
    }
    limbo_exec_rows(conn, "COMMIT");
}

/// Walks the whole chain from the root. `len` is far past any plausible per-query iteration
/// bound, so a silently-truncating implementation would return fewer than `len` rows.
#[test]
fn recursive_cte_walks_chain_deeper_than_the_old_cap() {
    let len = 2000;
    let db = TempDatabase::new_empty();
    let conn = db.connect_limbo();
    seed_chain(&conn, len);

    let rows = limbo_exec_rows(
        &conn,
        "WITH RECURSIVE walk(id) AS (
             SELECT id FROM node WHERE parent_id IS NULL
             UNION ALL
             SELECT n.id FROM node n JOIN walk w ON n.parent_id = w.id
         )
         SELECT count(*) FROM (SELECT id FROM walk)",
    );

    assert_eq!(
        rows,
        vec![vec![rusqlite::types::Value::Integer(len as i64)]],
        "recursive CTE must walk all {len} levels, not stop at the iteration cap"
    );
}
