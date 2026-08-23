//! count(x) counts non-NULL values of x, and that per-column count lives in the
//! DBSP aggregate state blob. A count the blob cannot carry reads back as 0, so
//! each case here reopens the database and then writes again, which forces the
//! state to be loaded from the blob and updated in place.

use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value;
use tempfile::TempDir;

fn open(path: &std::path::Path) -> TempDatabase {
    TempDatabase::builder()
        .with_db_path(path)
        .with_views(true)
        .build()
}

#[test]
fn matview_count_column_survives_reopen() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("matview_count_column_reopen.db");

    {
        let db = open(&path);
        let conn = db.connect_limbo();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY, x INTEGER)")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (1, NULL), (2, 5), (3, 7)")
            .unwrap();
        conn.execute(
            "CREATE MATERIALIZED VIEW v AS
             SELECT count(x) AS cx, count(*) AS n FROM t",
        )
        .unwrap();
        assert_eq!(
            limbo_exec_rows(&conn, "SELECT cx, n FROM v"),
            vec![vec![Value::Integer(2), Value::Integer(3)]],
            "before reopen"
        );
        conn.close().unwrap();
    }

    {
        let db = open(&path);
        let conn = db.connect_limbo();
        assert_eq!(
            limbo_exec_rows(&conn, "SELECT cx, n FROM v"),
            vec![vec![Value::Integer(2), Value::Integer(3)]],
            "after reopen"
        );

        conn.execute("INSERT INTO t VALUES (4, 9), (5, NULL)")
            .unwrap();
        assert_eq!(
            limbo_exec_rows(&conn, "SELECT cx, n FROM v"),
            vec![vec![Value::Integer(3), Value::Integer(5)]],
            "after a write following the reopen"
        );

        conn.execute("DELETE FROM t WHERE id = 2").unwrap();
        assert_eq!(
            limbo_exec_rows(&conn, "SELECT cx, n FROM v"),
            vec![vec![Value::Integer(2), Value::Integer(4)]],
            "after a delete following the reopen"
        );
    }
}

#[test]
fn matview_grouped_count_column_survives_reopen() {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("matview_count_column_group_reopen.db");

    {
        let db = open(&path);
        let conn = db.connect_limbo();
        conn.execute("CREATE TABLE g(id INTEGER PRIMARY KEY, k TEXT, x INTEGER)")
            .unwrap();
        conn.execute("INSERT INTO g VALUES (1, 'a', NULL), (2, 'a', 5), (3, 'b', NULL)")
            .unwrap();
        conn.execute(
            "CREATE MATERIALIZED VIEW v AS
             SELECT k, count(x) AS cx, count(*) AS n FROM g GROUP BY k",
        )
        .unwrap();
        conn.close().unwrap();
    }

    {
        let db = open(&path);
        let conn = db.connect_limbo();
        assert_eq!(
            limbo_exec_rows(&conn, "SELECT k, cx, n FROM v ORDER BY k"),
            vec![
                vec![
                    Value::Text("a".into()),
                    Value::Integer(1),
                    Value::Integer(2)
                ],
                vec![
                    Value::Text("b".into()),
                    Value::Integer(0),
                    Value::Integer(1)
                ],
            ],
            "after reopen"
        );

        // Group 'a' already counts one non-NULL value, so this only lands on 2
        // if the count loaded from the blob was carried into the update.
        conn.execute("INSERT INTO g VALUES (4, 'a', 8), (5, 'b', 3)")
            .unwrap();
        assert_eq!(
            limbo_exec_rows(&conn, "SELECT k, cx, n FROM v ORDER BY k"),
            vec![
                vec![
                    Value::Text("a".into()),
                    Value::Integer(2),
                    Value::Integer(3)
                ],
                vec![
                    Value::Text("b".into()),
                    Value::Integer(1),
                    Value::Integer(2)
                ],
            ],
            "after a write following the reopen"
        );
    }
}
