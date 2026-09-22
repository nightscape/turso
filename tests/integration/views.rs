use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::common::{ExecRows, TempDatabase};

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
