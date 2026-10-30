//! A SUM/TOTAL/AVG materialized view written before its REAL sum was exact
//! holds a REAL where the exact sum's BLOB belongs. Maintaining it must fail
//! and tell the user to REFRESH the view; after REFRESH it is maintained
//! again. The fixture was created by building a9e4bc66 unmodified:
//!
//!   CREATE TABLE t(g INTEGER, x REAL, y REAL);
//!   INSERT INTO t VALUES (1,1.5,10.0),(1,2.5,20.0),(2,-9e999,1.0),(2,3.0,2.0),
//!     (3,9e999,5.0),(3,-9e999,6.0),(4,4.0,7.0);
//!   CREATE MATERIALIZED VIEW v AS SELECT g, SUM(x) AS sx, TOTAL(x) AS tx,
//!     AVG(y) AS ay, COUNT(*) AS c FROM t GROUP BY g;

use crate::common::{limbo_exec_rows, TempDatabase};
use std::path::Path;
use std::sync::Arc;

const FRESH_SELECT: &str =
    "SELECT g, SUM(x), TOTAL(x), AVG(y), COUNT(*) FROM t GROUP BY g ORDER BY g";
const VIEW_SELECT: &str = "SELECT * FROM v ORDER BY g";

#[test]
fn matview_sum_state_written_before_the_exact_sum_needs_refresh() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("integration/query_processing/fixtures/ivm_sum_state_a9e4bc66.db");
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let db_path = tmp_dir.path().join("old_state.db");
    std::fs::copy(&fixture, &db_path).unwrap();

    let db = open(&db_path);
    let conn = db.connect_limbo();
    let err = conn
        .execute("INSERT INTO t VALUES (1, 0.1, 1.0)")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("REFRESH MATERIALIZED VIEW"),
        "unexpected error: {err}"
    );

    conn.execute("REFRESH MATERIALIZED VIEW v").unwrap();
    assert_view_matches_fresh_select(&conn);
    conn.execute("INSERT INTO t VALUES (1, 0.1, 1.0), (4, 9e999, 1.0)")
        .unwrap();
    assert_view_matches_fresh_select(&conn);
    drop(conn);
    drop(db);

    let db = open(&db_path);
    let conn = db.connect_limbo();
    conn.execute("DELETE FROM t WHERE rowid IN (3, 8, 9)")
        .unwrap();
    assert_view_matches_fresh_select(&conn);
}

fn open(path: &Path) -> TempDatabase {
    TempDatabase::builder()
        .with_db_path(path)
        .with_views(true)
        .build()
}

fn assert_view_matches_fresh_select(conn: &Arc<turso_core::Connection>) {
    assert_eq!(
        limbo_exec_rows(conn, VIEW_SELECT),
        limbo_exec_rows(conn, FRESH_SELECT)
    );
}
