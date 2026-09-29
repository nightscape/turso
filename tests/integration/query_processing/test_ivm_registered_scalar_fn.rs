use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::common::limbo_exec_rows;
use turso_core::{
    Connection, Database, DatabaseOpts, OpenFlags, OpenOptions, SqliteDialect, Value,
};

static T_TAG_COUNTED_CALLS: AtomicUsize = AtomicUsize::new(0);
static T_PARSE_COUNTED_CALLS: AtomicUsize = AtomicUsize::new(0);

fn t_tag(args: &[Value]) -> Result<Value, String> {
    Ok(Value::build_text(format!("{}:{}", args[0], args[1])))
}

fn t_tag_counted(args: &[Value]) -> Result<Value, String> {
    T_TAG_COUNTED_CALLS.fetch_add(1, Ordering::SeqCst);
    t_tag(args)
}

fn t_tag_checked(args: &[Value]) -> Result<Value, String> {
    if args[1].to_string() == "bad" {
        return Err("t_tag_checked refuses 'bad'".to_string());
    }
    t_tag(args)
}

fn t_parse(args: &[Value]) -> Result<Value, String> {
    let mut children: Vec<String> = match &args[2] {
        Value::Null => Vec::new(),
        Value::Text(json) => serde_json::from_str::<Vec<String>>(json.as_str())
            .map_err(|e| format!("children is not a JSON array of strings: {e}"))?,
        other => return Err(format!("children must be TEXT or NULL, got {other:?}")),
    };
    children.sort();
    Ok(Value::build_text(format!(
        "{}|{}|{}",
        args[0],
        args[1],
        children.join(",")
    )))
}

fn t_parse_counted(args: &[Value]) -> Result<Value, String> {
    T_PARSE_COUNTED_CALLS.fetch_add(1, Ordering::SeqCst);
    t_parse(args)
}

fn open(path: &Path, options: OpenOptions) -> (Arc<Database>, Arc<Connection>) {
    let io: Arc<dyn turso_core::IO> = Arc::new(turso_core::PlatformIO::new().unwrap());
    let db = Database::open(
        io,
        path.to_str().unwrap(),
        options
            .flags(OpenFlags::Create)
            .db_opts(DatabaseOpts::new().with_views(true)),
    )
    .unwrap();
    let conn = db.connect().unwrap();
    (db, conn)
}

fn plain_options() -> OpenOptions {
    OpenOptions::new(Arc::new(SqliteDialect))
}

fn options_with_tag_and_parse() -> OpenOptions {
    plain_options()
        .deterministic_scalar_function("t_tag", 2, t_tag)
        .deterministic_scalar_function("t_parse", 3, t_parse)
}

fn execute_all(conn: &Arc<Connection>, statements: &[&str]) {
    for sql in statements {
        conn.execute(sql)
            .unwrap_or_else(|e| panic!("`{sql}` failed: {e}"));
    }
}

fn assert_same_rows(conn: &Arc<Connection>, view_sql: &str, recompute_sql: &str) {
    assert_eq!(
        limbo_exec_rows(conn, view_sql),
        limbo_exec_rows(conn, recompute_sql),
        "`{view_sql}` differs from the recompute `{recompute_sql}`"
    );
}

fn assert_refused(conn: &Arc<Connection>, sql: &str, view: &str, reason: &str) {
    let err = conn.execute(sql).expect_err(&format!(
        "`{sql}` must be refused while view {view} is unusable"
    ));
    let msg = err.to_string();
    assert!(
        msg.contains(&format!("'{view}'")) && msg.contains(reason),
        "`{sql}`: the error must name view {view} and say `{reason}`: {msg}"
    );
}

const TAG_VIEW: &str = "SELECT * FROM v ORDER BY id";
const TAG_RECOMPUTE: &str = "SELECT id, t_tag(id, content) FROM t ORDER BY id";

const PARSED_VIEW: &str = "SELECT * FROM parsed ORDER BY id";
const PARSED_RECOMPUTE: &str = "SELECT d.id, t_parse(d.id, d.content, k.children) FROM d \
     LEFT JOIN (SELECT parent_id, json_group_array(txt) AS children FROM c GROUP BY parent_id) k \
     ON k.parent_id = d.id ORDER BY d.id";

const HOLON_SHAPE: &[&str] = &[
    "CREATE TABLE c (id INTEGER PRIMARY KEY, parent_id INTEGER, txt TEXT)",
    "CREATE TABLE d (id INTEGER PRIMARY KEY, content TEXT)",
    "INSERT INTO d VALUES (1, 'one'), (2, 'two')",
    "INSERT INTO c VALUES (10, 1, 'b'), (11, 1, 'a'), (12, 2, 'z')",
    "CREATE MATERIALIZED VIEW kids AS \
     SELECT parent_id, json_group_array(txt) AS children FROM c GROUP BY parent_id",
    "CREATE MATERIALIZED VIEW parsed AS \
     SELECT d.id, t_parse(d.id, d.content, k.children) AS r \
     FROM d LEFT JOIN kids k ON k.parent_id = d.id",
];

#[test]
fn view_over_registered_function_is_maintained_with_one_call_per_changed_row() {
    let dir = tempfile::TempDir::new().unwrap();
    let (_db, conn) = open(
        &dir.path().join("counted.db"),
        plain_options().deterministic_scalar_function("t_tag", 2, t_tag_counted),
    );
    execute_all(
        &conn,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, content TEXT)",
            "INSERT INTO t VALUES (1, 'a'), (2, 'b')",
            "CREATE MATERIALIZED VIEW v AS SELECT id, t_tag(id, content) AS r FROM t",
        ],
    );
    assert_same_rows(&conn, TAG_VIEW, TAG_RECOMPUTE);

    for (sql, expected_calls) in [
        ("INSERT INTO t VALUES (3, 'c')", 1),
        ("UPDATE t SET content = 'bb' WHERE id = 2", 2),
        ("DELETE FROM t WHERE id = 1", 1),
    ] {
        let before = T_TAG_COUNTED_CALLS.load(Ordering::SeqCst);
        conn.execute(sql).unwrap();
        let calls = T_TAG_COUNTED_CALLS.load(Ordering::SeqCst) - before;
        assert_eq!(calls, expected_calls, "calls of t_tag for `{sql}`");
        assert_same_rows(&conn, TAG_VIEW, TAG_RECOMPUTE);
    }
}

#[test]
fn chained_view_calls_registered_function_only_for_the_changed_parent() {
    let dir = tempfile::TempDir::new().unwrap();
    let (_db, conn) = open(
        &dir.path().join("chain.db"),
        plain_options().deterministic_scalar_function("t_parse", 3, t_parse_counted),
    );
    execute_all(&conn, HOLON_SHAPE);
    assert_same_rows(&conn, PARSED_VIEW, PARSED_RECOMPUTE);

    for sql in [
        "INSERT INTO c VALUES (13, 1, 'c')",
        "DELETE FROM c WHERE id = 11",
        "INSERT INTO c VALUES (14, 2, 'y')",
        "UPDATE d SET content = 'uno' WHERE id = 1",
    ] {
        let before = T_PARSE_COUNTED_CALLS.load(Ordering::SeqCst);
        conn.execute(sql).unwrap();
        let calls = T_PARSE_COUNTED_CALLS.load(Ordering::SeqCst) - before;
        assert_eq!(calls, 2, "`{sql}` must retract and insert one parent row");
        assert_same_rows(&conn, PARSED_VIEW, PARSED_RECOMPUTE);
    }
}

#[test]
fn views_over_registered_functions_load_and_stay_maintained_after_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("reopen.db");
    {
        let (_db, conn) = open(&path, options_with_tag_and_parse());
        execute_all(&conn, HOLON_SHAPE);
        execute_all(
            &conn,
            &[
                "CREATE TABLE t (id INTEGER PRIMARY KEY, content TEXT)",
                "INSERT INTO t VALUES (1, 'a')",
                "CREATE MATERIALIZED VIEW v AS SELECT id, t_tag(id, content) AS r FROM t",
            ],
        );
    }

    let (_db, conn) = open(&path, options_with_tag_and_parse());
    assert_same_rows(&conn, TAG_VIEW, TAG_RECOMPUTE);
    assert_same_rows(&conn, PARSED_VIEW, PARSED_RECOMPUTE);
    execute_all(
        &conn,
        &[
            "INSERT INTO t VALUES (2, 'b')",
            "UPDATE t SET content = 'aa' WHERE id = 1",
            "INSERT INTO c VALUES (13, 1, 'c')",
            "DELETE FROM c WHERE id = 12",
            "INSERT INTO d VALUES (3, 'three')",
        ],
    );
    assert_same_rows(&conn, TAG_VIEW, TAG_RECOMPUTE);
    assert_same_rows(&conn, PARSED_VIEW, PARSED_RECOMPUTE);
}

#[test]
fn reopen_without_the_function_refuses_writes_that_would_stale_its_views() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("missing.db");
    {
        let (_db, conn) = open(&path, options_with_tag_and_parse());
        execute_all(&conn, HOLON_SHAPE);
        execute_all(
            &conn,
            &[
                "CREATE TABLE t (id INTEGER PRIMARY KEY, content TEXT)",
                "INSERT INTO t VALUES (1, 'a')",
                "CREATE MATERIALIZED VIEW v AS SELECT id, t_tag(id, content) AS r FROM t",
            ],
        );
    }

    let (_db, conn) = open(&path, plain_options());
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM t"),
        vec![vec![rusqlite::types::Value::Integer(1)]]
    );
    assert_eq!(limbo_exec_rows(&conn, "SELECT * FROM kids").len(), 2);

    assert_refused(
        &conn,
        "INSERT INTO t VALUES (2, 'b')",
        "v",
        "no such function: t_tag",
    );
    assert_refused(
        &conn,
        "INSERT INTO c VALUES (13, 1, 'c')",
        "parsed",
        "no such function: t_parse",
    );
    assert_refused(
        &conn,
        "DELETE FROM d WHERE id = 1",
        "parsed",
        "no such function: t_parse",
    );

    conn.execute("DROP VIEW v").unwrap();
    conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
    conn.execute("DROP VIEW parsed").unwrap();
    conn.execute("INSERT INTO c VALUES (13, 1, 'c')").unwrap();
}

#[test]
fn function_error_during_maintenance_fails_the_write_and_keeps_the_view() {
    let dir = tempfile::TempDir::new().unwrap();
    let (_db, conn) = open(
        &dir.path().join("error.db"),
        plain_options().deterministic_scalar_function("t_tag", 2, t_tag_checked),
    );
    execute_all(
        &conn,
        &[
            "CREATE TABLE t (id INTEGER PRIMARY KEY, content TEXT)",
            "INSERT INTO t VALUES (1, 'a')",
            "CREATE MATERIALIZED VIEW v AS SELECT id, t_tag(id, content) AS r FROM t",
        ],
    );

    let err = conn
        .execute("INSERT INTO t VALUES (2, 'bad')")
        .expect_err("the function error must fail the write");
    assert!(
        err.to_string().contains("t_tag_checked refuses 'bad'"),
        "{err}"
    );
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM t"),
        vec![vec![rusqlite::types::Value::Integer(1)]]
    );
    assert_same_rows(&conn, TAG_VIEW, TAG_RECOMPUTE);

    conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
    assert_same_rows(&conn, TAG_VIEW, TAG_RECOMPUTE);
}
