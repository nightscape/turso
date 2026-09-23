//! Reads that combine an index on a materialized view with the view's rowid.
//!
//! The index yields a rowid and the view row is fetched lazily, so selecting
//! `rowid` before any view column reads it while that fetch is still pending.
//! Holon reads its views as `SELECT *, rowid AS _rowid …`.

use std::sync::Arc;

use super::matview_index_oracle::assert_reads_no_view_index;
use crate::common::{limbo_exec_rows, TempDatabase};

fn setup(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, watch_key TEXT, ord TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, watch_key, ord FROM t")?;
    conn.execute("CREATE INDEX idx_v_ord ON v(ord)")?;
    for (id, wk, ord) in [
        ("a", "w1", "o3"),
        ("b", "w1", "o1"),
        ("c", "w2", "o2"),
        ("d", "w1", "o2"),
    ] {
        conn.execute(&format!("INSERT INTO t VALUES ('{id}', '{wk}', '{ord}')"))?;
    }
    Ok(())
}

fn assert_index_matches_scan(conn: &Arc<turso_core::Connection>, indexed: &str, scanned: &str) {
    assert_reads_no_view_index(conn, scanned);
    assert_eq!(
        limbo_exec_rows(conn, indexed),
        limbo_exec_rows(conn, scanned),
        "an index-driven read of the view disagrees with a scan of it\n  indexed: {indexed}\n  scanned: {scanned}"
    );
}

const READS: [(&str, &str); 4] = [
    (
        "SELECT *, rowid AS _rowid FROM v WHERE watch_key = 'w1' ORDER BY ord",
        "SELECT *, rowid AS _rowid FROM v NOT INDEXED WHERE watch_key = 'w1' ORDER BY ord",
    ),
    (
        "SELECT rowid AS _rowid, * FROM v WHERE watch_key = 'w1' ORDER BY ord",
        "SELECT rowid AS _rowid, * FROM v NOT INDEXED WHERE watch_key = 'w1' ORDER BY ord",
    ),
    (
        "SELECT rowid, id FROM v WHERE ord = 'o1'",
        "SELECT rowid, id FROM v NOT INDEXED WHERE ord = 'o1'",
    ),
    (
        "SELECT rowid, id, ord FROM v ORDER BY ord",
        "SELECT rowid, id, ord FROM v NOT INDEXED ORDER BY ord",
    ),
];

#[turso_macros::test(views)]
fn rowid_with_an_indexed_read_of_a_committed_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
    for (indexed, scanned) in READS {
        assert_index_matches_scan(&conn, indexed, scanned);
    }
    Ok(())
}

#[turso_macros::test(views)]
fn rowid_with_an_indexed_read_inside_a_transaction(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t VALUES ('e', 'w1', 'o0')")?;
    conn.execute("UPDATE t SET ord = 'o9' WHERE id = 'b'")?;
    conn.execute("DELETE FROM t WHERE id = 'd'")?;
    for (indexed, scanned) in READS {
        assert_index_matches_scan(&conn, indexed, scanned);
    }
    conn.execute("COMMIT")?;
    for (indexed, scanned) in READS {
        assert_index_matches_scan(&conn, indexed, scanned);
    }
    Ok(())
}
