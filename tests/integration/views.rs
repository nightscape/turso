use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use rusqlite::types::Value::{Integer, Null, Text};

use super::common::{limbo_exec_rows, TempDatabase};

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

/// Runs `sql` with `@` bound to the base table `t` and to its identity view
/// `v`, and asserts both read the same rows.
fn assert_view_reads_like_its_table(conn: &Arc<turso_core::Connection>, sql: &str) {
    let over_table = limbo_exec_rows(conn, &sql.replace('@', "t"));
    let over_view = limbo_exec_rows(conn, &sql.replace('@', "v"));
    assert_eq!(over_view, over_table, "view and table disagree on: {sql}");
}

fn identity_view_over_five_rows() -> (TempDatabase, Arc<turso_core::Connection>) {
    let tmp_db = TempDatabase::builder().with_views(true).build();
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT)")
        .unwrap();
    conn.execute("CREATE MATERIALIZED VIEW v AS SELECT id, k FROM t")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e')")
        .unwrap();
    (tmp_db, conn)
}

#[test]
fn reverse_rowid_reads_of_an_ordered_materialized_view_are_refused() {
    let (_tmp_db, conn) = identity_view_over_five_rows();
    conn.execute("CREATE MATERIALIZED VIEW vo AS SELECT id, k FROM t ORDER BY 2 DESC")
        .unwrap();
    for sql in [
        "SELECT max(rowid) FROM vo",
        "SELECT k FROM vo ORDER BY rowid DESC",
    ] {
        let err = conn
            .prepare(sql)
            .and_then(|mut stmt| stmt.run_with_row_callback(|_| Ok(())))
            .expect_err(sql);
        assert!(
            err.to_string()
                .contains("Reverse rowid-order reads are not supported"),
            "{sql}: {err}"
        );
    }
}

/// `t` holds 300 rows `k1..k300`; `probe` has one partner for each even `k`.
fn view_and_probe_for_a_hash_join(view_sql: &str) -> (TempDatabase, Arc<turso_core::Connection>) {
    let tmp_db = TempDatabase::builder().with_views(true).build();
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k TEXT)")
        .unwrap();
    conn.execute(view_sql).unwrap();
    conn.execute("CREATE TABLE probe (id INTEGER PRIMARY KEY, kk TEXT)")
        .unwrap();
    for i in 1..=300 {
        conn.execute(format!("INSERT INTO t VALUES ({i}, 'k{i}')"))
            .unwrap();
        if i % 2 == 0 {
            conn.execute(format!("INSERT INTO probe VALUES ({i}, 'k{i}')"))
                .unwrap();
        }
    }
    (tmp_db, conn)
}

// The view is the FROM side, which the planner makes the hash-join build side.
const VIEW_AS_HASH_BUILD_SIDE: [&str; 2] = [
    "SELECT count(*) FROM @ X JOIN probe p ON p.kk = X.k",
    "SELECT sum(X.id) FROM @ X JOIN probe p ON p.kk = X.k",
];

#[test]
fn ordered_materialized_view_on_the_hash_build_side_reads_its_rows() {
    let (_tmp_db, conn) = view_and_probe_for_a_hash_join(
        "CREATE MATERIALIZED VIEW v AS SELECT id, k FROM t ORDER BY 2",
    );
    for sql in VIEW_AS_HASH_BUILD_SIDE {
        assert_view_reads_like_its_table(&conn, sql);
    }
}
