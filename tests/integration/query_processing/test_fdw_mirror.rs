//! Mirrors of foreign tables that back incremental materialized views.
//!
//! A mirror is an internal btree shadowing the foreign rows a view reads. It
//! exists only when the driver declares an identity, which is what makes a row
//! recognisable across scans. These cases cover its creation and teardown; the
//! view still reads the foreign table directly at this stage.

use crate::common::{self, ExecRows, TempDatabase};
use std::io::Write;

/// Foreign table over a CSV file. `identity` is passed through verbatim so a
/// case can omit it and get today's snapshot behaviour.
fn setup_fdw(tmp_db: &TempDatabase, conn: &std::sync::Arc<turso_core::Connection>, identity: &str) {
    let csv_path = tmp_db.path.parent().unwrap().join("mirror_msgs.csv");
    let mut f = std::fs::File::create(&csv_path).unwrap();
    writeln!(f, "uuid,session_id,body").unwrap();
    writeln!(f, "m1,s1,hello").unwrap();
    writeln!(f, "m2,s1,there").unwrap();
    drop(f);

    common::run_query(tmp_db, conn, "CREATE SERVER csv_srv OPTIONS (driver 'csv')").unwrap();
    common::run_query(
        tmp_db,
        conn,
        &format!(
            "CREATE FOREIGN TABLE msg_fdw (uuid TEXT, session_id TEXT, body TEXT) \
             SERVER csv_srv OPTIONS (path '{}', skip_header 'true'{identity})",
            csv_path.display()
        ),
    )
    .unwrap();
}

fn mirror_name(view: &str) -> String {
    format!("__turso_internal_fdw_mirror_v1_{view}__msg_fdw")
}

/// Names in sqlite_schema of the given type.
fn schema_names(conn: &std::sync::Arc<turso_core::Connection>, entry_type: &str) -> Vec<String> {
    let rows: Vec<(String,)> = conn.exec_rows(&format!(
        "SELECT name FROM sqlite_schema WHERE type = '{entry_type}'"
    ));
    rows.into_iter().map(|(n,)| n).collect()
}

/// A declared identity gets the view a mirror table registered in sqlite_schema.
#[turso_macros::test(views)]
fn test_mirror_created_for_identity_declaring_source(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_fdw(&tmp_db, &conn, ", identity 'uuid'");

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_mirror AS SELECT uuid, body FROM msg_fdw",
    )?;

    let tables = schema_names(&conn, "table");
    assert!(
        tables.contains(&mirror_name("mv_mirror")),
        "mirror table missing from sqlite_schema; tables = {tables:?}"
    );
    Ok(())
}

/// The mirror carries the identity as a PRIMARY KEY, which is what gives each
/// foreign row a stable rowid and rejects a source that repeats an identity.
#[turso_macros::test(views)]
fn test_mirror_declares_identity_primary_key(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_fdw(&tmp_db, &conn, ", identity 'uuid'");

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_pk AS SELECT uuid, body FROM msg_fdw",
    )?;

    let rows: Vec<(String,)> = conn.exec_rows(&format!(
        "SELECT sql FROM sqlite_schema WHERE name = '{}'",
        mirror_name("mv_pk")
    ));
    let sql = &rows.first().expect("mirror should have a schema row").0;
    assert!(sql.contains("PRIMARY KEY (uuid)"), "mirror sql: {sql}");
    assert!(
        sql.contains("uuid TEXT NOT NULL"),
        "identity columns must be NOT NULL: {sql}"
    );

    // The automatic index backing that PRIMARY KEY must be registered too.
    let indexes = schema_names(&conn, "index");
    let expected = format!("sqlite_autoindex_{}_1", mirror_name("mv_pk"));
    assert!(
        indexes.contains(&expected),
        "mirror primary-key index missing; indexes = {indexes:?}"
    );
    Ok(())
}

/// No declared identity means no mirror: such a view keeps snapshot semantics.
#[turso_macros::test(views)]
fn test_no_mirror_without_declared_identity(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_fdw(&tmp_db, &conn, "");

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_plain AS SELECT uuid, body FROM msg_fdw",
    )?;

    let tables = schema_names(&conn, "table");
    assert!(
        !tables.iter().any(|t| t.contains("fdw_mirror")),
        "a driver declaring no identity must get no mirror; tables = {tables:?}"
    );
    // ... and the view itself still works.
    let rows: Vec<(String,)> = conn.exec_rows("SELECT uuid FROM mv_plain");
    assert_eq!(rows.len(), 2);
    Ok(())
}

/// A matview over ordinary btree tables must be entirely unaffected.
#[turso_macros::test(views)]
fn test_no_mirror_for_non_foreign_sources(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    common::run_query(&tmp_db, &conn, "CREATE TABLE t (a INTEGER, b TEXT)")?;
    common::run_query(&tmp_db, &conn, "INSERT INTO t VALUES (1, 'x'), (2, 'y')")?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_local AS SELECT a, b FROM t",
    )?;

    let tables = schema_names(&conn, "table");
    assert!(
        !tables.iter().any(|t| t.contains("fdw_mirror")),
        "local tables must get no mirror; tables = {tables:?}"
    );
    let rows: Vec<(i64, String)> = conn.exec_rows("SELECT a, b FROM mv_local");
    assert_eq!(rows.len(), 2);
    Ok(())
}

/// DROP must take the mirror and its index with it, leaving no orphan rows.
#[turso_macros::test(views)]
fn test_drop_view_removes_mirror(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_fdw(&tmp_db, &conn, ", identity 'uuid'");

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_drop AS SELECT uuid, body FROM msg_fdw",
    )?;
    assert!(schema_names(&conn, "table").contains(&mirror_name("mv_drop")));

    common::run_query(&tmp_db, &conn, "DROP VIEW mv_drop")?;

    let tables = schema_names(&conn, "table");
    assert!(
        !tables.contains(&mirror_name("mv_drop")),
        "mirror table survived DROP; tables = {tables:?}"
    );
    let indexes = schema_names(&conn, "index");
    let index = format!("sqlite_autoindex_{}_1", mirror_name("mv_drop"));
    assert!(
        !indexes.contains(&index),
        "mirror index survived DROP; indexes = {indexes:?}"
    );
    Ok(())
}

/// Recreating a view under the same name must not trip over its old mirror —
/// the case a leaked schema row would break.
#[turso_macros::test(views)]
fn test_recreate_view_after_drop(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_fdw(&tmp_db, &conn, ", identity 'uuid'");

    for _ in 0..2 {
        common::run_query(
            &tmp_db,
            &conn,
            "CREATE MATERIALIZED VIEW mv_cycle AS SELECT uuid, body FROM msg_fdw",
        )?;
        let mirrors: Vec<String> = schema_names(&conn, "table")
            .into_iter()
            .filter(|t| t == &mirror_name("mv_cycle"))
            .collect();
        assert_eq!(mirrors.len(), 1, "expected exactly one mirror row: {mirrors:?}");
        common::run_query(&tmp_db, &conn, "DROP VIEW mv_cycle")?;
    }
    Ok(())
}
