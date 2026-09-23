//! An index on a view column computed by an expression. The column has no
//! affinity, so an index seek compares the key as written, like a scan.

use std::sync::Arc;

use super::matview_index_oracle::assert_reads_no_view_index;
use crate::common::{limbo_exec_rows, TempDatabase};

fn assert_index_matches_scan(conn: &Arc<turso_core::Connection>, indexed: &str, scanned: &str) {
    assert_reads_no_view_index(conn, scanned);
    assert_eq!(
        limbo_exec_rows(conn, indexed),
        limbo_exec_rows(conn, scanned),
        "an index-driven read of the view disagrees with a scan of it\n  indexed: {indexed}\n  scanned: {scanned}"
    );
}

#[turso_macros::test(views)]
fn an_index_on_a_count_column(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT st, count(*) AS n FROM t GROUP BY st")?;
    conn.execute("CREATE INDEX idx_v_n ON v(n)")?;
    for (id, st) in [("a", "X"), ("b", "X"), ("c", "Y")] {
        conn.execute(&format!("INSERT INTO t VALUES ('{id}', '{st}')"))?;
    }
    for key in ["1", "2", "'1'"] {
        assert_index_matches_scan(
            &conn,
            &format!("SELECT st, n FROM v WHERE n = {key}"),
            &format!("SELECT st, n FROM v NOT INDEXED WHERE n = {key}"),
        );
    }
    assert_index_matches_scan(
        &conn,
        "SELECT st, n FROM v WHERE n > 0 ORDER BY n",
        "SELECT st, n FROM v NOT INDEXED WHERE n > 0 ORDER BY n",
    );
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM v WHERE n = 1"),
        vec![vec![rusqlite::types::Value::Integer(1)]],
        "fixture: one group of size 1"
    );
    Ok(())
}

#[turso_macros::test(views)]
fn an_index_on_an_integer_recursive_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE edges (src INTEGER, dst INTEGER)")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW reach AS \
         WITH RECURSIVE r(node) AS (SELECT 1 UNION SELECT e.dst FROM edges e JOIN r ON e.src = r.node) \
         SELECT node FROM r",
    )?;
    conn.execute("CREATE INDEX idx_reach_node ON reach(node)")?;
    conn.execute("INSERT INTO edges VALUES (1, 2), (2, 3)")?;
    for node in 1..4 {
        assert_index_matches_scan(
            &conn,
            &format!("SELECT node FROM reach WHERE node = {node}"),
            &format!("SELECT node FROM reach NOT INDEXED WHERE node = {node}"),
        );
    }
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM reach WHERE node = 1"),
        vec![vec![rusqlite::types::Value::Integer(1)]],
        "fixture: node 1 is reachable"
    );
    Ok(())
}
