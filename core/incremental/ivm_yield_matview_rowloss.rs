//! End-to-end reproduction, through the public API, of the eq_only
//! leaf-boundary miss (mechanism documented in `eq_only_seek_boundary.rs`):
//! a `GROUP BY` materialized view silently drops aggregate contributions.

use crate::sync::Arc;
use crate::types::Value;
use crate::{Connection, Database, DatabaseOpts, MemoryIO, OpenFlags, SqliteDialect, IO};

fn views_db() -> Arc<Connection> {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = Database::open_file_with_flags(
        io,
        ":memory:",
        OpenFlags::Create,
        DatabaseOpts::new().with_views(true),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();
    db.connect().unwrap()
}

fn scalar(conn: &Arc<Connection>, sql: &str) -> i64 {
    let mut stmt = conn.query(sql).unwrap().unwrap();
    let rows = stmt.run_collect_rows().unwrap();
    assert_eq!(rows.len(), 1, "expected exactly one row from {sql}");
    match &rows[0][0] {
        Value::Numeric(crate::numeric::Numeric::Integer(i)) => *i,
        // sum() over integers can come back as a float here
        Value::Numeric(crate::numeric::Numeric::Float(f)) => f64::from(*f) as i64,
        other => panic!("expected an integer from {sql}, got {other:?}"),
    }
}

fn run_case(groups: i64) {
    let conn = views_db();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, v INTEGER)")
        .unwrap();
    conn.execute(
        "CREATE MATERIALIZED VIEW mv AS SELECT k, count(*) AS c, sum(v) AS s FROM t GROUP BY k",
    )
    .unwrap();

    // Two rows per group, inserted in two passes; the second pass drives every
    // group through the read-modify-write path.
    for pass in 0..2i64 {
        for k in 0..groups {
            conn.execute(format!(
                "INSERT INTO t (id, k, v) VALUES ({}, {k}, {})",
                pass * groups + k + 1,
                k * 2 + pass
            ))
            .unwrap();
        }
    }

    assert_eq!(
        scalar(&conn, "SELECT count(*) FROM t"),
        2 * groups,
        "groups={groups}: base table lost rows"
    );
    assert_eq!(
        scalar(&conn, "SELECT count(*) FROM mv"),
        groups,
        "groups={groups}: materialized view lost groups"
    );
    // Group count stays right; a duplicated per-group state entry drops that
    // group's prior count and sum contribution instead of merging with it.
    assert_eq!(
        scalar(&conn, "SELECT sum(c) FROM mv"),
        2 * groups,
        "groups={groups}: materialized view under-counted"
    );
    let expected_sum: i64 = (0..groups).map(|k| k * 2 + (k * 2 + 1)).sum();
    assert_eq!(
        scalar(&conn, "SELECT sum(s) FROM mv"),
        expected_sum,
        "groups={groups}: materialized view aggregated the wrong values"
    );
}

#[test]
fn group_by_matview_undercounts_at_scale() {
    run_case(50);
    run_case(400);
}
