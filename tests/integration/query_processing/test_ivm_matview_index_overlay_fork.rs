//! An index on a materialized view read inside a transaction, for view shapes
//! whose overlay is more than the view's own circuit over its own deltas.

use std::sync::Arc;

use super::matview_index_oracle::assert_reads_no_view_index;
use crate::common::{limbo_exec_rows, TempDatabase};

fn assert_index_matches_scan(conn: &Arc<turso_core::Connection>, indexed: &str, scanned: &str) {
    assert_reads_no_view_index(conn, scanned);
    let via_index = limbo_exec_rows(conn, indexed);
    let via_scan = limbo_exec_rows(conn, scanned);
    assert_eq!(
        via_index, via_scan,
        "an index-driven read of the view disagrees with a scan of it\n  indexed: {indexed}\n  scanned: {scanned}"
    );
}

/// The indexed view reads another view, so its overlay is fed by the
/// upstream view's uncommitted output.
#[turso_macros::test(views)]
fn an_index_on_a_chained_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, st TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v1 AS SELECT id, st FROM t WHERE st <> 'X'")?;
    conn.execute("CREATE MATERIALIZED VIEW v2 AS SELECT id, st FROM v1")?;
    conn.execute("CREATE INDEX idx_v2_st ON v2(st)")?;
    for (id, st) in [("a", "p"), ("b", "q"), ("c", "X")] {
        conn.execute(format!("INSERT INTO t VALUES ('{id}', '{st}')"))?;
    }

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t VALUES ('d', 'p')")?;
    conn.execute("UPDATE t SET st = 'X' WHERE id = 'a'")?;
    conn.execute("UPDATE t SET st = 'q' WHERE id = 'c'")?;
    for st in ["p", "q", "X"] {
        assert_index_matches_scan(
            &conn,
            &format!("SELECT id, st FROM v2 WHERE st = '{st}' ORDER BY id"),
            &format!("SELECT id, st FROM v2 NOT INDEXED WHERE st = '{st}' ORDER BY id"),
        );
    }
    assert_index_matches_scan(
        &conn,
        "SELECT id, st FROM v2 ORDER BY st, id",
        "SELECT id, st FROM v2 NOT INDEXED ORDER BY st, id",
    );
    conn.execute("COMMIT")?;
    assert_index_matches_scan(
        &conn,
        "SELECT id, st FROM v2 ORDER BY st, id",
        "SELECT id, st FROM v2 NOT INDEXED ORDER BY st, id",
    );
    Ok(())
}

/// Under NOCASE, 'a' and 'A' are the same key but different values. A covering
/// read returns the value from the index entry, so the base table is the
/// oracle.
#[turso_macros::test(views)]
fn a_case_change_under_a_nocase_index(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT)")?;
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, name FROM t")?;
    conn.execute("CREATE INDEX idx_v_name ON v(name COLLATE NOCASE)")?;
    conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')")?;

    let reads = [
        "SELECT name FROM {} WHERE name = 'a' COLLATE NOCASE",
        "SELECT name FROM {} ORDER BY name COLLATE NOCASE",
    ];
    conn.execute("BEGIN")?;
    conn.execute("UPDATE t SET name = 'A' WHERE id = 1")?;
    for read in reads {
        assert_index_matches_scan(&conn, &read.replace("{}", "v"), &read.replace("{}", "t"));
    }
    conn.execute("COMMIT")?;
    for read in reads {
        assert_index_matches_scan(&conn, &read.replace("{}", "v"), &read.replace("{}", "t"));
    }
    Ok(())
}
