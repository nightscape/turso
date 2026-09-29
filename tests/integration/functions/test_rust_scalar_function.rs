use crate::common::limbo_exec_rows;
use std::sync::Arc;
use turso_core::{Database, LimboError, OpenFlags, OpenOptions, SqliteDialect, Value};

fn t_upper(args: &[Value]) -> Result<Value, String> {
    match &args[0] {
        Value::Text(text) => Ok(Value::build_text(text.as_str().to_uppercase())),
        other => Err(format!("t_upper expects TEXT, got {other:?}")),
    }
}

fn t_fail(_args: &[Value]) -> Result<Value, String> {
    Err("boom".to_string())
}

fn options() -> OpenOptions {
    OpenOptions::new(Arc::new(SqliteDialect)).flags(OpenFlags::Create)
}

fn open(path: &str, options: OpenOptions) -> turso_core::Result<Arc<Database>> {
    let io: Arc<dyn turso_core::IO> = Arc::new(turso_core::PlatformIO::new().unwrap());
    Database::open(io, path, options)
}

fn run(conn: &Arc<turso_core::Connection>, sql: &str) -> turso_core::Result<()> {
    conn.prepare(sql)?.run_with_row_callback(|_| Ok(()))
}

#[test]
fn registered_function_is_callable_on_every_connection() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("udf.db");
    let db = open(
        path.to_str().unwrap(),
        options().deterministic_scalar_function("T_Upper", 1, t_upper),
    )
    .unwrap();

    for conn in [db.connect().unwrap(), db.connect().unwrap()] {
        let rows = limbo_exec_rows(&conn, "SELECT t_upper('a'), T_UPPER('bc')");
        assert_eq!(
            rows,
            vec![vec![
                rusqlite::types::Value::Text("A".to_string()),
                rusqlite::types::Value::Text("BC".to_string()),
            ]]
        );
    }
}

#[test]
fn function_error_fails_the_statement_with_its_message() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("udf.db");
    let db = open(
        path.to_str().unwrap(),
        options().deterministic_scalar_function("t_fail", 1, t_fail),
    )
    .unwrap();
    let conn = db.connect().unwrap();

    let err = run(&conn, "SELECT t_fail(1)").unwrap_err();
    assert!(
        matches!(&err, LimboError::ExtensionError(message) if message == "boom"),
        "{err:?}"
    );
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT 1"),
        vec![vec![rusqlite::types::Value::Integer(1)]]
    );
}

#[test]
fn wrong_argument_count_is_a_prepare_error() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("udf.db");
    let db = open(
        path.to_str().unwrap(),
        options().deterministic_scalar_function("t_upper", 1, t_upper),
    )
    .unwrap();
    let conn = db.connect().unwrap();

    let err = run(&conn, "SELECT t_upper('a', 'b')").unwrap_err();
    assert!(err.to_string().contains("t_upper"), "{err:?}");
}

#[test]
fn open_refuses_invalid_function_sets() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("udf.db");
    let path = path.to_str().unwrap();

    let cases = [
        (
            options().deterministic_scalar_function("upper", 1, t_upper),
            "scalar function 'upper' has the name of a built-in function",
        ),
        (
            options().deterministic_scalar_function("upper", 2, t_upper),
            "scalar function 'upper' has the name of a built-in function",
        ),
        (
            options().deterministic_scalar_function("uuid4_str", 0, t_upper),
            "scalar function 'uuid4_str' has the name of a built-in extension function",
        ),
        (
            options()
                .deterministic_scalar_function("t_upper", 1, t_upper)
                .deterministic_scalar_function("T_UPPER", 2, t_fail),
            "scalar function 't_upper' is registered twice",
        ),
    ];
    for (options, expected) in cases {
        let err = open(path, options).unwrap_err();
        assert!(
            matches!(&err, LimboError::InvalidArgument(message) if message == expected),
            "expected {expected:?}, got {err:?}"
        );
    }
}

#[test]
fn open_refuses_function_names_that_sql_cannot_call() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("udf.db");
    let path = path.to_str().unwrap();

    for name in [
        "",
        "\"upper\"",
        "my fn",
        "select",
        "SELECT",
        "1fn",
        "t-upper",
        "fn\u{e9}",
    ] {
        let err = open(
            path,
            options().deterministic_scalar_function(name, 1, t_upper),
        )
        .unwrap_err();
        let expected = format!(
            "scalar function name '{}' is not a plain identifier: it must start with an ASCII \
             letter or '_', contain only ASCII letters, digits and '_', and not be an SQL keyword",
            name.to_ascii_lowercase()
        );
        assert!(
            matches!(&err, LimboError::InvalidArgument(message) if *message == expected),
            "{name:?}: expected {expected:?}, got {err:?}"
        );
    }
}

#[test]
fn a_function_name_may_start_with_an_underscore_and_contain_digits() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("udf.db");
    let db = open(
        path.to_str().unwrap(),
        options().deterministic_scalar_function("_t_upper2", 1, t_upper),
    )
    .unwrap();
    let conn = db.connect().unwrap();
    run(&conn, "SELECT _t_upper2('a')").unwrap();
}

#[test]
fn second_open_must_request_the_same_functions() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("udf.db");
    let path = path.to_str().unwrap();
    let with_upper = || options().deterministic_scalar_function("t_upper", 1, t_upper);

    let db = open(path, with_upper()).unwrap();
    db.connect().unwrap().execute("CREATE TABLE t(x)").unwrap();

    let same = open(path, with_upper()).unwrap();
    assert!(Arc::ptr_eq(&db, &same));

    for different in [
        options(),
        options().deterministic_scalar_function("t_upper", 2, t_upper),
        with_upper().deterministic_scalar_function("t_fail", 1, t_fail),
    ] {
        let err = open(path, different).unwrap_err();
        assert!(
            matches!(&err, LimboError::InvalidArgument(message)
                if message.starts_with("database is already open with scalar functions [\"t_upper/1\"]")),
            "{err:?}"
        );
    }

    let io: Arc<dyn turso_core::IO> = Arc::new(turso_core::PlatformIO::new().unwrap());
    let file = io.open_file(path, OpenFlags::Create, false).unwrap();
    let storage = Arc::new(turso_core::storage::database::DatabaseFile::new(file));
    let err = Database::open(io, path, options().storage(storage)).unwrap_err();
    assert!(
        matches!(&err, LimboError::InvalidArgument(message)
            if message == "database is already open with scalar functions [\"t_upper/1\"]; requested []"),
        "{err:?}"
    );
}
