//! Regression tests for the recursive-CTE iteration cap.
//!
//! `translate_recursive_cte` emits a guard at the top of the main recursion loop
//! (`core/translate/select.rs`) that compares an iteration counter against
//! `DEFAULT_RECURSIVE_MAX_ITERATIONS` (`core/translate/logical.rs`). Historically that guard
//! jumped straight to the output label on breach, so the query returned whatever had been
//! accumulated so far *as if the recursion had converged* -- a silent wrong answer. A 2000-node
//! parent chain returned 101 rows where SQLite returns 2000.
//!
//! Two properties are asserted here:
//!   1. An ordinary hierarchy deeper than the old cap is walked completely.
//!   2. A recursion that genuinely cannot converge fails loudly instead of silently truncating.

use crate::common::{limbo_exec_rows, try_limbo_exec_rows, TempDatabase};
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

/// Walks the whole chain from the root. With `len` well above the old cap of 100, a truncating
/// implementation returns ~101 rows instead of `len`.
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

/// A `UNION ALL` self-reference with no termination condition can never converge. It must surface
/// as an error. Returning a truncated result set would be a silent wrong answer, and looping
/// forever would be an unkillable hang -- neither is acceptable.
#[test]
fn runaway_recursive_cte_errors_instead_of_truncating() {
    let db = TempDatabase::new_empty();
    let conn = db.connect_limbo();

    let result = try_limbo_exec_rows(
        &db,
        &conn,
        "WITH RECURSIVE forever(x) AS (
             SELECT 1
             UNION ALL
             SELECT x FROM forever
         )
         SELECT count(*) FROM (SELECT x FROM forever)",
    );

    assert!(
        result.is_err(),
        "a non-converging recursive CTE must raise an error, but it returned: {result:?}"
    );
}
