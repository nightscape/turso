//! Adversarial probes for the matview cursor's step path.
//!
//! With an empty overlay the matview cursor steps its b-tree instead of
//! seeking from the root. Every probe here drives a matview scan and the same
//! scan over the base table, and asserts both answer the same rows — the base
//! table is the oracle, so a probe fails only when the step path diverges from
//! an ordinary table cursor.

use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value;
use std::sync::Arc;
use turso_core::vdbe::StepResult;
use turso_core::Connection;

fn ints(rows: Vec<Vec<Value>>) -> Vec<i64> {
    rows.into_iter()
        .map(|r| match r[0] {
            Value::Integer(i) => i,
            ref other => panic!("expected integer, got {other:?}"),
        })
        .collect()
}

/// `src` and `mirror` hold identical rows; `mv` is a matview over `src`.
fn seed(conn: &Arc<Connection>, n: i64) {
    limbo_exec_rows(
        conn,
        "CREATE TABLE src (id INTEGER PRIMARY KEY, grp INTEGER, body TEXT)",
    );
    limbo_exec_rows(
        conn,
        "CREATE TABLE mirror (id INTEGER PRIMARY KEY, grp INTEGER, body TEXT)",
    );
    limbo_exec_rows(conn, "BEGIN");
    for id in 1..=n {
        limbo_exec_rows(
            conn,
            &format!("INSERT INTO src VALUES ({id}, {}, 'body-{id}')", id % 7),
        );
        limbo_exec_rows(
            conn,
            &format!("INSERT INTO mirror VALUES ({id}, {}, 'body-{id}')", id % 7),
        );
    }
    limbo_exec_rows(conn, "COMMIT");
    limbo_exec_rows(
        conn,
        "CREATE MATERIALIZED VIEW mv AS SELECT id, grp, body FROM src",
    );
}

fn db() -> TempDatabase {
    TempDatabase::builder()
        .with_views(true)
        .with_db_name("probe.db")
        .build()
}

/// Step a statement until the next Row, or None at Done.
fn next_row(stmt: &mut turso_core::Statement) -> Option<Vec<Value>> {
    loop {
        match stmt.step().unwrap() {
            StepResult::IO => stmt._io().step().unwrap(),
            StepResult::Yield => continue,
            StepResult::Row => {
                let row = stmt
                    .row()
                    .unwrap()
                    .get_values()
                    .map(|v| match v {
                        turso_core::Value::Numeric(turso_core::Numeric::Integer(i)) => {
                            Value::Integer(*i)
                        }
                        turso_core::Value::Text(t) => Value::Text(t.as_str().to_string()),
                        turso_core::Value::Null => Value::Null,
                        other => panic!("unexpected {other:?}"),
                    })
                    .collect();
                return Some(row);
            }
            StepResult::Done => return None,
            StepResult::Interrupt | StepResult::Busy | StepResult::Sleep { .. } => {
                panic!("unexpected step result")
            }
        }
    }
}

/// Drain `sql` on `conn`, running `mid` once after `pause_after` rows.
fn scan_with_interleaved(
    conn: &Arc<Connection>,
    sql: &str,
    pause_after: usize,
    mid: impl FnOnce(),
) -> Vec<i64> {
    let mut stmt = conn.prepare(sql).unwrap();
    let mut out = Vec::new();
    let mut mid = Some(mid);
    while let Some(row) = next_row(&mut stmt) {
        out.push(match row[0] {
            Value::Integer(i) => i,
            ref other => panic!("expected integer, got {other:?}"),
        });
        if out.len() == pause_after {
            if let Some(f) = mid.take() {
                f();
            }
        }
    }
    out
}

// ---------------------------------------------------------------- plain scans

#[test]
fn full_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 3000);
    assert_eq!(
        ints(limbo_exec_rows(&conn, "SELECT id FROM mv")),
        ints(limbo_exec_rows(&conn, "SELECT id FROM mirror")),
    );
}

#[test]
fn seek_then_step_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 3000);
    for bound in [1, 2, 1500, 2999, 3000, 3001] {
        assert_eq!(
            ints(limbo_exec_rows(
                &conn,
                &format!("SELECT id FROM mv WHERE id >= {bound}")
            )),
            ints(limbo_exec_rows(
                &conn,
                &format!("SELECT id FROM mirror WHERE id >= {bound}")
            )),
            "bound {bound}"
        );
    }
}

#[test]
fn reverse_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 3000);
    assert_eq!(
        ints(limbo_exec_rows(&conn, "SELECT id FROM mv ORDER BY id DESC")),
        ints(limbo_exec_rows(
            &conn,
            "SELECT id FROM mirror ORDER BY id DESC"
        )),
    );
}

#[test]
fn self_join_nested_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 300);
    assert_eq!(
        ints(limbo_exec_rows(
            &conn,
            "SELECT a.id FROM mv a, mv b WHERE b.id = a.id + 1",
        )),
        ints(limbo_exec_rows(
            &conn,
            "SELECT a.id FROM mirror a, mirror b WHERE b.id = a.id + 1",
        )),
    );
}

#[test]
fn scan_with_tiny_page_cache_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 5000);
    limbo_exec_rows(&conn, "PRAGMA cache_size=1");
    assert_eq!(
        ints(limbo_exec_rows(&conn, "SELECT id FROM mv")),
        ints(limbo_exec_rows(&conn, "SELECT id FROM mirror")),
    );
}

// ------------------------------------------------- writes interleaved in-scan

/// The same connection deletes the row under the cursor and a range after it
/// while its own matview scan is paused.
#[test]
fn same_conn_delete_at_cursor_mid_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 3000);

    let write = |conn: &Arc<Connection>| {
        limbo_exec_rows(conn, "DELETE FROM src WHERE id BETWEEN 100 AND 1200");
        limbo_exec_rows(conn, "DELETE FROM mirror WHERE id BETWEEN 100 AND 1200");
    };

    let mv = scan_with_interleaved(&conn, "SELECT id FROM mv", 100, || write(&conn));

    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, 3000);
    let table = scan_with_interleaved(&conn2, "SELECT id FROM mirror", 100, || write(&conn2));

    assert_eq!(mv, table, "matview scan diverged from the table scan");
}

/// Shrunk form of `same_conn_delete_at_cursor_mid_scan`: 10 rows, the scan
/// paused on one row, one same-connection DELETE, then resume. `cut_lo..=cut_hi`
/// is what the DELETE removes; `pause_after` rows were already read.
fn deleted_range_mid_scan(
    n: i64,
    pause_after: usize,
    cut_lo: i64,
    cut_hi: i64,
) -> (Vec<i64>, Vec<i64>) {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, n);
    let mv = scan_with_interleaved(&conn, "SELECT id FROM mv", pause_after, || {
        limbo_exec_rows(
            &conn,
            &format!("DELETE FROM src WHERE id BETWEEN {cut_lo} AND {cut_hi}"),
        );
    });

    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, n);
    let table = scan_with_interleaved(&conn2, "SELECT id FROM mirror", pause_after, || {
        limbo_exec_rows(
            &conn2,
            &format!("DELETE FROM mirror WHERE id BETWEEN {cut_lo} AND {cut_hi}"),
        );
    });
    (mv, table)
}

#[test]
fn deleted_range_mid_scan_cursor_row_included() {
    // The cursor sits on row 3; the DELETE removes 3..=5.
    let (mv, table) = deleted_range_mid_scan(10, 3, 3, 5);
    assert_eq!(mv, table, "cursor row deleted: matview != table");
}

#[test]
fn deleted_range_mid_scan_cursor_row_excluded() {
    // The cursor sits on row 3; the DELETE removes 4..=5 and leaves row 3.
    let (mv, table) = deleted_range_mid_scan(10, 3, 4, 5);
    assert_eq!(mv, table, "cursor row kept: matview != table");
}

#[test]
fn deleted_range_mid_scan_single_row_at_cursor() {
    // The cursor sits on row 3; the DELETE removes only row 3.
    let (mv, table) = deleted_range_mid_scan(10, 3, 3, 3);
    assert_eq!(mv, table, "single cursor row deleted: matview != table");
}

/// A peer write while the scan is paused AND the page cache is one page, so
/// the paused b-tree position cannot survive in the cache.
#[test]
fn peer_write_with_tiny_cache_mid_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 4000);
    limbo_exec_rows(&conn, "PRAGMA cache_size=1");
    let peer = t.connect_limbo();

    let write = |peer: &Arc<Connection>| {
        limbo_exec_rows(peer, "BEGIN");
        for id in 60_001..60_400 {
            limbo_exec_rows(peer, &format!("INSERT INTO src VALUES ({id}, 1, 'w')"));
            limbo_exec_rows(peer, &format!("INSERT INTO mirror VALUES ({id}, 1, 'w')"));
        }
        limbo_exec_rows(peer, "DELETE FROM src WHERE id BETWEEN 200 AND 2500");
        limbo_exec_rows(peer, "DELETE FROM mirror WHERE id BETWEEN 200 AND 2500");
        limbo_exec_rows(peer, "COMMIT");
    };

    let mv = scan_with_interleaved(&conn, "SELECT id FROM mv", 150, || write(&peer));

    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, 4000);
    limbo_exec_rows(&conn2, "PRAGMA cache_size=1");
    let peer2 = t2.connect_limbo();
    let table = scan_with_interleaved(&conn2, "SELECT id FROM mirror", 150, || write(&peer2));

    assert_eq!(mv, table, "matview scan diverged from the table scan");
}

/// A second scan of the same view starts while the first one is paused: the
/// b-tree root gains a peer cursor mid-step.
#[test]
fn second_cursor_opened_mid_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 3000);

    let mut inner: Vec<i64> = Vec::new();
    let mv = scan_with_interleaved(&conn, "SELECT id FROM mv", 100, || {
        inner = ints(limbo_exec_rows(&conn, "SELECT id FROM mv WHERE id >= 2000"));
    });
    assert_eq!(
        mv,
        ints(limbo_exec_rows(&conn, "SELECT id FROM mirror")),
        "outer matview scan diverged from the table scan"
    );
    assert_eq!(
        inner,
        ints(limbo_exec_rows(
            &conn,
            "SELECT id FROM mirror WHERE id >= 2000"
        )),
        "nested matview scan diverged from the table scan"
    );
}

/// A peer connection inserts (and commits) while a matview scan is paused.
/// The scan holds a read snapshot, so it must still answer the rows it started
/// with — exactly as the same paused scan over the base table does.
#[test]
fn peer_insert_mid_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 3000);
    let peer = t.connect_limbo();

    let write = |peer: &Arc<Connection>| {
        limbo_exec_rows(peer, "BEGIN");
        for id in 10_001..10_600 {
            limbo_exec_rows(
                peer,
                &format!("INSERT INTO src VALUES ({id}, 1, 'body-{id}')"),
            );
            limbo_exec_rows(
                peer,
                &format!("INSERT INTO mirror VALUES ({id}, 1, 'body-{id}')"),
            );
        }
        limbo_exec_rows(peer, "COMMIT");
    };

    let mv = scan_with_interleaved(&conn, "SELECT id FROM mv", 100, || write(&peer));
    // Reset both tables to the same pre-write state for the oracle run.
    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, 3000);
    let peer2 = t2.connect_limbo();
    let table = scan_with_interleaved(&conn2, "SELECT id FROM mirror", 100, || write(&peer2));

    assert_eq!(mv.len(), table.len(), "row count differs");
    assert_eq!(mv, table, "matview scan diverged from the table scan");
}

/// A peer connection deletes the row under the cursor and its neighbours.
#[test]
fn peer_delete_at_cursor_mid_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 3000);
    let peer = t.connect_limbo();

    let write = |peer: &Arc<Connection>| {
        limbo_exec_rows(peer, "DELETE FROM src WHERE id BETWEEN 95 AND 1500");
        limbo_exec_rows(peer, "DELETE FROM mirror WHERE id BETWEEN 95 AND 1500");
    };

    let mv = scan_with_interleaved(&conn, "SELECT id FROM mv", 100, || write(&peer));

    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, 3000);
    let peer2 = t2.connect_limbo();
    let table = scan_with_interleaved(&conn2, "SELECT id FROM mirror", 100, || write(&peer2));

    assert_eq!(mv, table, "matview scan diverged from the table scan");
}

/// The same connection writes while its own matview scan is paused. The write
/// stages an uncommitted overlay, so the rest of the scan leaves the step path.
#[test]
fn same_conn_insert_mid_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 3000);

    let write = |conn: &Arc<Connection>| {
        for id in 20_001..20_050 {
            limbo_exec_rows(
                conn,
                &format!("INSERT INTO src VALUES ({id}, 2, 'body-{id}')"),
            );
            limbo_exec_rows(
                conn,
                &format!("INSERT INTO mirror VALUES ({id}, 2, 'body-{id}')"),
            );
        }
    };

    let mv = scan_with_interleaved(&conn, "SELECT id FROM mv", 100, || write(&conn));

    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, 3000);
    let table = scan_with_interleaved(&conn2, "SELECT id FROM mirror", 100, || write(&conn2));

    assert_eq!(mv, table, "matview scan diverged from the table scan");
}

/// A same-connection UPDATE that grows the rows at and after the paused cursor,
/// so the view btree rebalances the leaf the cursor sits on.
#[test]
fn same_conn_update_grows_rows_at_cursor_mid_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 3000);

    let body = "x".repeat(600);
    let write = |conn: &Arc<Connection>| {
        for table in ["src", "mirror"] {
            limbo_exec_rows(
                conn,
                &format!("UPDATE {table} SET body = '{body}' WHERE id BETWEEN 100 AND 400"),
            );
        }
    };

    let mv = scan_with_interleaved(&conn, "SELECT id FROM mv", 100, || write(&conn));

    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, 3000);
    let table = scan_with_interleaved(&conn2, "SELECT id FROM mirror", 100, || write(&conn2));

    assert_eq!(mv, table, "matview scan diverged from the table scan");
}

/// A scan inside an explicit transaction, with the write before the scan
/// (overlay non-empty for the whole scan) and after a pause (overlay empty for
/// the first rows, non-empty after).
#[test]
#[ignore = "pre-existing: same-connection writes inside BEGIN made mid-scan are invisible to a matview scan"]
fn in_transaction_overlay_flip_mid_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 2000);
    limbo_exec_rows(&conn, "BEGIN");

    let write = |conn: &Arc<Connection>| {
        limbo_exec_rows(conn, "INSERT INTO src VALUES (30001, 3, 'x')");
        limbo_exec_rows(conn, "INSERT INTO mirror VALUES (30001, 3, 'x')");
        limbo_exec_rows(conn, "DELETE FROM src WHERE id BETWEEN 500 AND 600");
        limbo_exec_rows(conn, "DELETE FROM mirror WHERE id BETWEEN 500 AND 600");
    };

    let mv = scan_with_interleaved(&conn, "SELECT id FROM mv", 100, || write(&conn));

    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, 2000);
    limbo_exec_rows(&conn2, "BEGIN");
    let table = scan_with_interleaved(&conn2, "SELECT id FROM mirror", 100, || write(&conn2));

    assert_eq!(mv, table, "matview scan diverged from the table scan");
}

/// A rollback while a matview scan is paused: the overlay the scan started with
/// disappears under it.
#[test]
#[ignore = "pre-existing: after ROLLBACK mid-scan a matview scan returns the rolled-back row"]
fn rollback_mid_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 2000);

    let run = |conn: &Arc<Connection>, table: &str| -> Vec<i64> {
        limbo_exec_rows(conn, "BEGIN");
        limbo_exec_rows(conn, "INSERT INTO src VALUES (40001, 4, 'y')");
        limbo_exec_rows(conn, "INSERT INTO mirror VALUES (40001, 4, 'y')");
        scan_with_interleaved(conn, &format!("SELECT id FROM {table}"), 50, || {
            limbo_exec_rows(conn, "ROLLBACK");
        })
    };

    let mv = run(&conn, "mv");
    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, 2000);
    let table = run(&conn2, "mirror");

    assert_eq!(mv, table, "matview scan diverged from the table scan");
}

/// A checkpoint while a matview scan is paused.
#[test]
fn checkpoint_mid_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 3000);
    let peer = t.connect_limbo();

    let mv = scan_with_interleaved(&conn, "SELECT id FROM mv", 100, || {
        limbo_exec_rows(&peer, "PRAGMA wal_checkpoint(PASSIVE)");
    });

    let t2 = db();
    let conn2 = t2.connect_limbo();
    seed(&conn2, 3000);
    let peer2 = t2.connect_limbo();
    let table = scan_with_interleaved(&conn2, "SELECT id FROM mirror", 100, || {
        limbo_exec_rows(&peer2, "PRAGMA wal_checkpoint(PASSIVE)");
    });

    assert_eq!(mv, table, "matview scan diverged from the table scan");
}

// ------------------------------------------------------------ ORDER BY / LIMIT

#[test]
fn order_by_view_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 2000);
    limbo_exec_rows(
        &conn,
        "CREATE MATERIALIZED VIEW mv_ord AS SELECT id, grp FROM src ORDER BY 2, 1",
    );
    assert_eq!(
        ints(limbo_exec_rows(&conn, "SELECT id FROM mv_ord")),
        ints(limbo_exec_rows(
            &conn,
            "SELECT id FROM mirror ORDER BY grp, id"
        )),
    );
}

/// A matview over a matview: the rebuild's nested scan reads `mv` with a
/// cursor on the same b-tree root the commit cascade writes.
#[test]
fn chained_matview_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 2000);
    limbo_exec_rows(
        &conn,
        "CREATE MATERIALIZED VIEW mv2 AS SELECT id, grp FROM mv WHERE grp = 3",
    );
    for id in 50_001..50_040 {
        limbo_exec_rows(
            &conn,
            &format!("INSERT INTO src VALUES ({id}, {}, 'z')", id % 7),
        );
        limbo_exec_rows(
            &conn,
            &format!("INSERT INTO mirror VALUES ({id}, {}, 'z')", id % 7),
        );
    }
    assert_eq!(
        ints(limbo_exec_rows(&conn, "SELECT id FROM mv2")),
        ints(limbo_exec_rows(
            &conn,
            "SELECT id FROM mirror WHERE grp = 3"
        )),
    );
}

#[test]
fn limit_view_scan_matches_table() {
    let t = db();
    let conn = t.connect_limbo();
    seed(&conn, 2000);
    limbo_exec_rows(
        &conn,
        "CREATE MATERIALIZED VIEW mv_lim AS SELECT id, grp FROM src ORDER BY 1 LIMIT 17",
    );
    let got = ints(limbo_exec_rows(&conn, "SELECT id FROM mv_lim"));
    assert_eq!(got.len(), 17, "LIMIT 17 view returned {} rows", got.len());
}
