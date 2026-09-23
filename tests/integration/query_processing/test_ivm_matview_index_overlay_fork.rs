//! An index on a materialized view read inside a transaction, for view shapes
//! whose overlay is more than the view's own circuit over its own deltas.

use std::sync::Arc;

use crate::common::{limbo_exec_rows, TempDatabase};

fn assert_index_matches_scan(conn: &Arc<turso_core::Connection>, indexed: &str, scanned: &str) {
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
        conn.execute(&format!("INSERT INTO t VALUES ('{id}', '{st}')"))?;
    }

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t VALUES ('d', 'p')")?;
    conn.execute("UPDATE t SET st = 'X' WHERE id = 'a'")?;
    conn.execute("UPDATE t SET st = 'q' WHERE id = 'c'")?;
    for st in ["p", "q", "X"] {
        assert_index_matches_scan(
            &conn,
            &format!("SELECT id, st FROM v2 WHERE st = '{st}' ORDER BY id"),
            &format!("SELECT id, st FROM v2 WHERE +st = '{st}' ORDER BY id"),
        );
    }
    assert_index_matches_scan(
        &conn,
        "SELECT id, st FROM v2 ORDER BY st, id",
        "SELECT id, st FROM v2 ORDER BY +st, id",
    );
    conn.execute("COMMIT")?;
    assert_index_matches_scan(
        &conn,
        "SELECT id, st FROM v2 ORDER BY st, id",
        "SELECT id, st FROM v2 ORDER BY +st, id",
    );
    Ok(())
}

/// A recursive view read inside a transaction is recomputed whole, so the
/// committed index must not be read at all.
#[turso_macros::test(views)]
fn an_index_on_a_recursive_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE edges (src TEXT, dst TEXT)")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW reach AS \
         WITH RECURSIVE r(node) AS (SELECT 'n1' UNION SELECT e.dst FROM edges e JOIN r ON e.src = r.node) \
         SELECT node FROM r",
    )?;
    conn.execute("CREATE INDEX idx_reach_node ON reach(node)")?;
    conn.execute("INSERT INTO edges VALUES ('n1', 'n2'), ('n2', 'n3')")?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO edges VALUES ('n3', 'n4')")?;
    conn.execute("DELETE FROM edges WHERE src = 'n1'")?;
    for node in ["n1", "n2", "n3", "n4"] {
        assert_index_matches_scan(
            &conn,
            &format!("SELECT node FROM reach WHERE node = '{node}'"),
            &format!("SELECT node FROM reach WHERE +node = '{node}'"),
        );
    }
    assert_index_matches_scan(
        &conn,
        "SELECT node FROM reach ORDER BY node",
        "SELECT node FROM reach ORDER BY +node",
    );
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM (SELECT +node FROM reach)"),
        vec![vec![rusqlite::types::Value::Integer(1)]],
        "fixture: inside the transaction only n1 is reachable"
    );
    conn.execute("COMMIT")?;
    assert_index_matches_scan(
        &conn,
        "SELECT node FROM reach ORDER BY node",
        "SELECT node FROM reach ORDER BY +node",
    );
    Ok(())
}

/// Under NOCASE, 'a' and 'A' are the same key but different values. A covering
/// read returns the value from the index entry, so the base table is the
/// oracle; a `+name` scan of the view is not, since it may use the index too.
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
