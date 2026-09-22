use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rusqlite::types::Value::{Integer, Null, Text};

use super::common::{limbo_exec_rows, ExecRows, TempDatabase};

#[test]
fn concurrent_view_expansion_is_not_spuriously_circular() {
    let tmp_db = TempDatabase::builder().with_views(true).build();
    {
        let conn = tmp_db.connect_limbo();
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .unwrap();
        conn.execute(
            "CREATE VIEW v AS SELECT id FROM t a \
             WHERE NOT EXISTS (SELECT 1 FROM t b WHERE b.id = a.id + 1)",
        )
        .unwrap();
    }

    let db = tmp_db.db.clone();
    let failed = Arc::new(AtomicBool::new(false));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let db = db.clone();
            let failed = failed.clone();
            std::thread::spawn(move || {
                let conn = db.connect().unwrap();
                for _ in 0..5000 {
                    if let Err(e) = conn.prepare("SELECT id FROM v") {
                        assert!(
                            e.to_string().contains("circularly defined"),
                            "unexpected prepare error: {e}"
                        );
                        failed.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert!(
        !failed.load(Ordering::Relaxed),
        "concurrent view expansion produced a spurious 'circularly defined' error"
    );
}

/// `CREATE INDEX` on a materialized view was accepted, and the database could
/// then never be opened again.
///
/// A materialized view is registered in `Schema::tables` so it can be read
/// like a table, so `get_table` returns it and the `is_materialized_view`
/// guard in `translate_create_index` — which sits in that lookup's `else`
/// arm — was unreachable. The accepted index left a `sqlite_schema` row whose
/// table does not exist at load time, because indexes are populated before
/// materialized views; every later open then failed with
/// "Corrupt database: sqlite_schema contains index for missing table", down
/// to `SELECT 1`, and `DROP INDEX` could not undo it.
#[test]
fn create_index_on_materialized_view_is_refused_and_leaves_the_database_openable() {
    let tmp_db = TempDatabase::builder().with_views(true).build();
    let path = tmp_db.path.clone();
    {
        let conn = tmp_db.connect_limbo();
        conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, st TEXT)")
            .unwrap();
        conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, st FROM t")
            .unwrap();
        conn.execute("INSERT INTO t VALUES ('a', 'x')").unwrap();

        let err = conn
            .execute("CREATE INDEX idx_v_id ON v (id)")
            .expect_err("CREATE INDEX on a materialized view must be refused")
            .to_string();
        assert!(err.contains("views may not be indexed"), "got: {err}");
    }

    // The refusal is only half of it: the file must still be openable.
    let reopened = TempDatabase::builder()
        .with_db_path(&path)
        .with_views(true)
        .build();
    let conn = reopened.connect_limbo();
    let rows: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM t");
    assert_eq!(rows, vec![(1,)]);
    let rows: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM v");
    assert_eq!(rows, vec![(1,)]);
}

/// An aggregate over an empty view read alongside a non-aggregate expression
/// points the view's cursor at a `NullRow`; `SELECT count(*)` alone does not.
#[test]
fn aggregate_over_an_empty_materialized_view_with_a_literal_does_not_panic() {
    let tmp_db = TempDatabase::builder().with_views(true).build();
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT)")
        .unwrap();
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, k FROM t")
        .unwrap();

    let rows: Vec<(String, i64)> = conn.exec_rows("SELECT 'c', count(*) FROM v");
    assert_eq!(rows, vec![("c".to_string(), 0)]);

    // An outer join whose matview side never matches must give NULLs, not the
    // previous row's values.
    conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    conn.execute("CREATE TABLE probe (id INTEGER PRIMARY KEY, want TEXT)")
        .unwrap();
    conn.execute("INSERT INTO probe VALUES (1, 'a'), (2, 'missing')")
        .unwrap();
    let rows: Vec<(i64, String)> = conn.exec_rows(
        "SELECT p.id, COALESCE(v.k, '<null>') FROM probe p \
         LEFT JOIN v ON v.k = p.want ORDER BY p.id",
    );
    assert_eq!(
        rows,
        vec![(1, "a".to_string()), (2, "<null>".to_string())],
        "the unmatched side must be NULL, not the previous row's value"
    );
}

#[test]
fn left_join_on_a_materialized_view_rowid_is_null_for_a_null_key() {
    let tmp_db = TempDatabase::builder().with_views(true).build();
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT)")
        .unwrap();
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, k FROM t")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    conn.execute("CREATE TABLE probe (id INTEGER PRIMARY KEY, vid)")
        .unwrap();
    // A NULL key makes `SeekRowid` jump to `NullRow` without moving the view's
    // cursor off the row it found for probe 1.
    conn.execute("INSERT INTO probe VALUES (1, 1), (2, NULL)")
        .unwrap();

    let rows = limbo_exec_rows(
        &conn,
        "SELECT p.id, v.rowid, v.k FROM probe p LEFT JOIN v ON v.rowid = p.vid ORDER BY p.id",
    );
    assert_eq!(
        rows,
        vec![
            vec![Integer(1), Integer(1), Text("a".into())],
            vec![Integer(2), Null, Null],
        ],
        "a NULL key must read NULL, not the row matched for probe 1"
    );
}

#[test]
fn materialized_view_cursor_reads_rows_again_after_a_null_row() {
    let tmp_db = TempDatabase::builder().with_views(true).build();
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT)")
        .unwrap();
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, k FROM t ORDER BY 2")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    conn.execute("CREATE TABLE probe (id INTEGER PRIMARY KEY, want TEXT)")
        .unwrap();
    conn.execute("INSERT INTO probe VALUES (1, 'zz'), (2, 'a'), (3, 'c')")
        .unwrap();
    // An uncommitted change makes `Rewind` read the ordered view from its
    // in-memory snapshot, never repositioning the inner btree cursor.
    conn.execute("BEGIN").unwrap();
    conn.execute("INSERT INTO t VALUES (3, 'c')").unwrap();

    // `instr` keeps the join a nested loop that rewinds the view per probe.
    let rows = limbo_exec_rows(
        &conn,
        "SELECT p.id, v.k FROM probe p LEFT JOIN v ON instr(v.k, p.want) > 0 ORDER BY p.id",
    );
    assert_eq!(
        rows,
        vec![
            vec![Integer(1), Null],
            vec![Integer(2), Text("a".into())],
            vec![Integer(3), Text("c".into())],
        ],
        "probe 1's NullRow must not null out the rows later probes match"
    );
}
