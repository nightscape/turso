use std::sync::Arc;

use rusqlite::types::Value;

use crate::common::limbo_exec_rows;

/// Panics if `sql`'s plan reads an index of a materialized view. A query that
/// does cannot be the oracle for that index: a stale entry misleads both sides.
/// `+col` does not guarantee it; a covering scan still reads the index.
pub fn assert_reads_no_view_index(conn: &Arc<turso_core::Connection>, sql: &str) {
    let view_indexes: Vec<String> = limbo_exec_rows(
        conn,
        "SELECT i.name FROM sqlite_schema i JOIN sqlite_schema v \
         ON v.type = 'view' AND v.name = i.tbl_name WHERE i.type = 'index'",
    )
    .into_iter()
    .map(|row| match &row[..] {
        [Value::Text(name)] => name.clone(),
        other => panic!("unexpected index row {other:?}"),
    })
    .collect();
    for row in limbo_exec_rows(conn, &format!("EXPLAIN QUERY PLAN {sql}")) {
        let Value::Text(detail) = &row[3] else {
            panic!("unexpected plan row {row:?}");
        };
        let words: Vec<&str> = detail.split_whitespace().collect();
        for pair in words.windows(2) {
            assert!(
                !(pair[0] == "INDEX" && view_indexes.iter().any(|n| n == pair[1])),
                "the oracle reads a view's index ({detail}): {sql}"
            );
        }
    }
}
