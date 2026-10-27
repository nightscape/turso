//! State rows that a SUM/TOTAL/AVG materialized view wrote before it kept
//! +Infinity and -Infinity counts have fewer columns than the state rows it
//! writes now. The fixture was created by building a9e4bc66 unmodified:
//!
//!   CREATE TABLE t(g INTEGER, x REAL, y REAL);
//!   INSERT INTO t VALUES (1,1.5,10.0),(1,2.5,20.0),(2,-9e999,1.0),(2,3.0,2.0),
//!     (3,9e999,5.0),(3,-9e999,6.0),(4,4.0,7.0);
//!   CREATE MATERIALIZED VIEW v AS SELECT g, SUM(x) AS sx, TOTAL(x) AS tx,
//!     AVG(y) AS ay, COUNT(*) AS c FROM t GROUP BY g;
//!
//! Groups 1 and 4 hold only finite rows. Group 2 already holds a -Infinity in
//! its sum, and group 3 the NULL that +Infinity plus -Infinity leave behind.

use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value::{Integer, Null, Real};
use std::path::Path;
use std::sync::Arc;

const FRESH_SELECT: &str =
    "SELECT g, SUM(x), TOTAL(x), AVG(y), COUNT(*) FROM t WHERE g IN (1, 4) GROUP BY g ORDER BY g";
const VIEW_SELECT: &str = "SELECT * FROM v WHERE g IN (1, 4) ORDER BY g";

#[test]
fn matview_sum_state_written_without_infinity_counts_is_maintained() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("integration/query_processing/fixtures/ivm_sum_state_a9e4bc66.db");
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let db_path = tmp_dir.path().join("old_state.db");
    std::fs::copy(&fixture, &db_path).unwrap();

    let db = open(&db_path);
    let conn = db.connect_limbo();
    assert_legacy_groups_unchanged(&conn);
    assert_finite_groups_match_fresh_select(&conn);

    conn.execute("INSERT INTO t VALUES (1, 9e999, 1.0), (4, -9e999, 1.0), (4, -9e999, 1.0)")
        .unwrap();
    assert_finite_groups_match_fresh_select(&conn);
    assert_eq!(
        limbo_exec_rows(
            &conn,
            "SELECT g, sx, tx FROM v WHERE g IN (1, 4) ORDER BY g"
        ),
        vec![
            vec![Integer(1), Real(f64::INFINITY), Real(f64::INFINITY)],
            vec![Integer(4), Real(f64::NEG_INFINITY), Real(f64::NEG_INFINITY)],
        ]
    );
    assert_legacy_groups_unchanged(&conn);
    drop(conn);
    drop(db);

    let db = open(&db_path);
    let conn = db.connect_limbo();
    conn.execute("DELETE FROM t WHERE rowid = 10").unwrap();
    assert_finite_groups_match_fresh_select(&conn);
    conn.execute("DELETE FROM t WHERE rowid IN (8, 9)").unwrap();
    assert_finite_groups_match_fresh_select(&conn);
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT g, sx FROM v WHERE g IN (1, 4) ORDER BY g"),
        vec![vec![Integer(1), Real(4.0)], vec![Integer(4), Real(4.0)]]
    );
}

fn open(path: &Path) -> TempDatabase {
    TempDatabase::builder()
        .with_db_path(path)
        .with_views(true)
        .build()
}

fn assert_finite_groups_match_fresh_select(conn: &Arc<turso_core::Connection>) {
    assert_eq!(
        limbo_exec_rows(conn, VIEW_SELECT),
        limbo_exec_rows(conn, FRESH_SELECT)
    );
}

fn assert_legacy_groups_unchanged(conn: &Arc<turso_core::Connection>) {
    assert_eq!(
        limbo_exec_rows(conn, "SELECT * FROM v WHERE g IN (2, 3) ORDER BY g"),
        vec![
            vec![
                Integer(2),
                Real(f64::NEG_INFINITY),
                Real(f64::NEG_INFINITY),
                Real(1.5),
                Integer(2)
            ],
            vec![Integer(3), Null, Null, Real(5.5), Integer(2)],
        ]
    );
}
