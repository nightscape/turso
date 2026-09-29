//! A materialized view that cannot be loaded cannot be maintained. Every write
//! that would change its input fails and names the view, reads keep working,
//! and DROP VIEW makes the tables writable again.

use std::path::Path;
use std::sync::Arc;

use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value;
use turso_core::{Connection, DatabaseOpts};

const MISSING_COLUMN: &str = "gone_col";

fn open(path: &Path) -> (TempDatabase, Arc<Connection>) {
    let db = TempDatabase::builder()
        .with_db_path(path)
        .with_opts(DatabaseOpts::new().with_encryption(true).with_attach(true))
        .with_views(true)
        .build();
    let conn = db.connect_limbo();
    (db, conn)
}

fn setup(path: &Path, statements: &[&str]) {
    let (db, conn) = open(path);
    for sql in statements {
        conn.execute(sql).unwrap();
    }
    drop(conn);
    drop(db);
}

/// Rewrites the stored definition of `view` so that it selects a column its
/// base table does not have.
fn make_view_unloadable(path: &Path, view: &str, broken_sql: &str) {
    assert!(broken_sql.contains(MISSING_COLUMN));
    let sqlite = rusqlite::Connection::open(path).unwrap();
    sqlite.pragma_update(None, "writable_schema", "ON").unwrap();
    let updated = sqlite
        .execute(
            "UPDATE sqlite_master SET sql = ?1 WHERE type = 'view' AND name = ?2",
            [broken_sql, view],
        )
        .unwrap();
    assert_eq!(updated, 1, "expected one stored row for view {view}");
    sqlite
        .pragma_update(None, "writable_schema", "OFF")
        .unwrap();
}

fn assert_refused(conn: &Arc<Connection>, sql: &str, view: &str) {
    let err = conn.execute(sql).expect_err(&format!(
        "`{sql}` must be refused while view {view} is unusable"
    ));
    let msg = err.to_string();
    assert!(
        msg.contains(&format!("'{view}'")),
        "`{sql}`: the error must name view {view}: {msg}"
    );
    assert!(
        msg.contains(MISSING_COLUMN),
        "`{sql}`: the error must say why view {view} is unusable: {msg}"
    );
}

fn count(conn: &Arc<Connection>, table: &str) -> i64 {
    match &limbo_exec_rows(conn, &format!("SELECT count(*) FROM {table}"))[0][0] {
        Value::Integer(n) => *n,
        other => panic!("count(*) returned {other:?}"),
    }
}

#[test]
fn writes_to_the_base_table_of_an_unusable_view_are_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("direct.db");
    setup(
        &path,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT UNIQUE)",
            "INSERT INTO t VALUES (1, 'x')",
            "CREATE MATERIALIZED VIEW v AS SELECT id, a FROM t",
        ],
    );
    make_view_unloadable(
        &path,
        "v",
        "CREATE MATERIALIZED VIEW v AS SELECT id, a, gone_col FROM t",
    );
    let (_db, conn) = open(&path);

    assert_refused(&conn, "INSERT INTO t VALUES (2, 'y')", "v");
    assert_refused(&conn, "UPDATE t SET a = 'z' WHERE id = 1", "v");
    assert_refused(&conn, "DELETE FROM t WHERE id = 1", "v");
    assert_refused(&conn, "INSERT OR REPLACE INTO t VALUES (1, 'r')", "v");
    assert_refused(
        &conn,
        "INSERT INTO t VALUES (1, 'u') ON CONFLICT (id) DO UPDATE SET a = excluded.a",
        "v",
    );
    assert_eq!(count(&conn, "t"), 1, "reads of the base table keep working");

    conn.execute("DROP VIEW v").unwrap();
    conn.execute("INSERT INTO t VALUES (2, 'y')").unwrap();
    assert_eq!(count(&conn, "t"), 2);
    conn.execute("ALTER TABLE t ADD COLUMN b TEXT").unwrap();
}

#[test]
fn a_write_that_reaches_an_unusable_view_through_a_loaded_view_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("transitive.db");
    setup(
        &path,
        &[
            "CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER)",
            "CREATE TABLE parent (id INTEGER PRIMARY KEY, label TEXT)",
            "INSERT INTO parent VALUES (1, 'p')",
            "INSERT INTO child VALUES (10, 1)",
            "CREATE MATERIALIZED VIEW kids AS \
             SELECT parent_id, count(*) AS n FROM child GROUP BY parent_id",
            "CREATE MATERIALIZED VIEW top AS \
             SELECT p.id, p.label, k.n FROM parent p JOIN kids k ON k.parent_id = p.id",
        ],
    );
    make_view_unloadable(
        &path,
        "top",
        "CREATE MATERIALIZED VIEW top AS \
         SELECT p.id, p.label, p.gone_col, k.n FROM parent p JOIN kids k ON k.parent_id = p.id",
    );
    let (_db, conn) = open(&path);

    assert_eq!(count(&conn, "kids"), 1, "the upstream view still loads");
    assert_refused(&conn, "INSERT INTO child VALUES (11, 1)", "top");
    assert_refused(&conn, "INSERT INTO parent VALUES (2, 'q')", "top");

    conn.execute("DROP VIEW top").unwrap();
    conn.execute("INSERT INTO child VALUES (11, 1)").unwrap();
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT n FROM kids WHERE parent_id = 1"),
        vec![vec![Value::Integer(2)]]
    );
}

#[test]
fn writes_to_the_sources_of_a_view_that_reads_an_unusable_view_are_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("chained.db");
    setup(
        &path,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)",
            "CREATE TABLE u (id INTEGER PRIMARY KEY, b TEXT)",
            "CREATE MATERIALIZED VIEW base_view AS SELECT id, a FROM t",
            "CREATE MATERIALIZED VIEW chained AS \
             SELECT bv.id, bv.a, u.b FROM base_view bv JOIN u ON u.id = bv.id",
        ],
    );
    make_view_unloadable(
        &path,
        "base_view",
        "CREATE MATERIALIZED VIEW base_view AS SELECT id, a, gone_col FROM t",
    );
    let (_db, conn) = open(&path);

    assert_refused(&conn, "INSERT INTO t VALUES (1, 'x')", "base_view");
    let err = conn
        .execute("INSERT INTO u VALUES (1, 'y')")
        .expect_err("u feeds `chained`, which reads the unusable base_view");
    assert!(err.to_string().contains("'chained'"), "{err}");
}

#[test]
fn a_trigger_that_writes_the_base_table_of_an_unusable_view_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("trigger.db");
    setup(
        &path,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)",
            "CREATE TABLE log (id INTEGER PRIMARY KEY, a TEXT)",
            "CREATE TRIGGER copy_to_t AFTER INSERT ON log \
             BEGIN INSERT INTO t VALUES (new.id, new.a); END",
            "CREATE MATERIALIZED VIEW v AS SELECT id, a FROM t",
        ],
    );
    make_view_unloadable(
        &path,
        "v",
        "CREATE MATERIALIZED VIEW v AS SELECT id, a, gone_col FROM t",
    );
    let (_db, conn) = open(&path);

    assert_refused(&conn, "INSERT INTO log VALUES (1, 'x')", "v");
    assert_eq!(count(&conn, "t"), 0);
    assert_eq!(count(&conn, "log"), 0);
}

#[test]
fn a_foreign_key_cascade_into_the_base_table_of_an_unusable_view_is_refused() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("cascade.db");
    setup(
        &path,
        &[
            "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE t (id INTEGER PRIMARY KEY, \
             parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE ON UPDATE CASCADE)",
            "INSERT INTO parent VALUES (1)",
            "INSERT INTO t VALUES (10, 1)",
            "CREATE MATERIALIZED VIEW v AS SELECT id, parent_id FROM t",
        ],
    );
    make_view_unloadable(
        &path,
        "v",
        "CREATE MATERIALIZED VIEW v AS SELECT id, parent_id, gone_col FROM t",
    );
    let (_db, conn) = open(&path);
    conn.execute("PRAGMA foreign_keys = ON").unwrap();

    assert_refused(&conn, "DELETE FROM parent WHERE id = 1", "v");
    assert_refused(&conn, "UPDATE parent SET id = 2 WHERE id = 1", "v");
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id, parent_id FROM t"),
        vec![vec![Value::Integer(10), Value::Integer(1)]]
    );
}

#[test]
fn tables_in_temp_and_attached_databases_that_share_a_name_with_a_view_source_stay_writable() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("other_databases.db");
    setup(
        &path,
        &[
            "CREATE TABLE c (id INTEGER PRIMARY KEY, parent INTEGER, body TEXT)",
            "CREATE TABLE d (id INTEGER PRIMARY KEY, body TEXT)",
            "CREATE MATERIALIZED VIEW kids AS \
             SELECT parent, json_group_array(body) AS bodies FROM c GROUP BY parent",
            "CREATE MATERIALIZED VIEW parsed AS \
             SELECT d.id, d.body, k.bodies FROM d LEFT JOIN kids k ON k.parent = d.id",
        ],
    );
    make_view_unloadable(
        &path,
        "parsed",
        "CREATE MATERIALIZED VIEW parsed AS \
         SELECT d.id, d.gone_col, k.bodies FROM d LEFT JOIN kids k ON k.parent = d.id",
    );
    let (_db, conn) = open(&path);
    let aux_path = dir.path().join("aux.db");
    conn.execute(format!("ATTACH '{}' AS aux", aux_path.display()))
        .unwrap();
    conn.execute("CREATE TEMP TABLE c (id INTEGER PRIMARY KEY, parent INTEGER, body TEXT)")
        .unwrap();
    conn.execute("CREATE TABLE aux.c (id INTEGER PRIMARY KEY, parent INTEGER, body TEXT)")
        .unwrap();

    for schema in ["temp", "aux"] {
        conn.execute(format!("INSERT INTO {schema}.c VALUES (1, 1, 'x')"))
            .unwrap();
        conn.execute(format!("UPDATE {schema}.c SET body = 'y' WHERE id = 1"))
            .unwrap();
        assert_eq!(
            limbo_exec_rows(&conn, &format!("SELECT body FROM {schema}.c")),
            vec![vec![Value::Text("y".into())]]
        );
        conn.execute(format!("DELETE FROM {schema}.c WHERE id = 1"))
            .unwrap();
        assert_eq!(count(&conn, &format!("{schema}.c")), 0);
    }

    assert_refused(&conn, "INSERT INTO main.c VALUES (1, 1, 'x')", "parsed");
    assert_refused(&conn, "UPDATE main.c SET body = 'y'", "parsed");
    assert_refused(&conn, "DELETE FROM main.c", "parsed");
}
