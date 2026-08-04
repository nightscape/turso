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

/// Like [`setup_fdw`], but with caller-chosen rows and a caller-chosen CSV so a
/// case can span several sessions.
fn setup_fdw_rows(
    tmp_db: &TempDatabase,
    conn: &std::sync::Arc<turso_core::Connection>,
    csv_path: &std::path::Path,
    rows: &[(&str, &str, &str)],
) {
    let mut f = std::fs::File::create(csv_path).unwrap();
    writeln!(f, "uuid,session_id,body").unwrap();
    for (uuid, session, body) in rows {
        writeln!(f, "{uuid},{session},{body}").unwrap();
    }
    drop(f);

    common::run_query(tmp_db, conn, "CREATE SERVER csv_srv OPTIONS (driver 'csv')").unwrap();
    common::run_query(
        tmp_db,
        conn,
        &format!(
            "CREATE FOREIGN TABLE msg_fdw (uuid TEXT, session_id TEXT, body TEXT) \
             SERVER csv_srv OPTIONS (path '{}', skip_header 'true', identity 'uuid')",
            csv_path.display()
        ),
    )
    .unwrap();
}

/// Rows of a mirror, ordered so a case can compare them directly.
fn mirror_rows(
    conn: &std::sync::Arc<turso_core::Connection>,
    view: &str,
) -> Vec<(String, String, String)> {
    conn.exec_rows(&format!(
        "SELECT uuid, session_id, body FROM \"{}\" ORDER BY uuid",
        mirror_name(view)
    ))
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
        assert_eq!(
            mirrors.len(),
            1,
            "expected exactly one mirror row: {mirrors:?}"
        );
        common::run_query(&tmp_db, &conn, "DROP VIEW mv_cycle")?;
    }
    Ok(())
}

/// Creating the view fills its mirror with the foreign rows it read.
#[turso_macros::test(views)]
fn test_mirror_populated_at_create(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_fdw(&tmp_db, &conn, ", identity 'uuid'");

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_fill AS SELECT uuid, body FROM msg_fdw",
    )?;

    let mirrored = mirror_rows(&conn, "mv_fill");
    let source: Vec<(String, String, String)> =
        conn.exec_rows("SELECT uuid, session_id, body FROM msg_fdw ORDER BY uuid");
    assert_eq!(
        mirrored, source,
        "mirror must hold exactly the foreign rows the view read"
    );
    assert_eq!(mirrored.len(), 2, "{mirrored:?}");
    Ok(())
}

/// The mirror is scoped by the view's predicate: rows the view never reads must
/// not be mirrored, since the driver may not even be able to enumerate them.
#[turso_macros::test(views)]
fn test_mirror_respects_view_predicate(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("mirror_sessions.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[
            ("m1", "s1", "one"),
            ("m2", "s2", "two"),
            ("m3", "s1", "three"),
        ],
    );

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_scoped AS SELECT uuid, body FROM msg_fdw WHERE session_id = 's1'",
    )?;

    let mirrored = mirror_rows(&conn, "mv_scoped");
    assert_eq!(
        mirrored,
        vec![
            ("m1".to_string(), "s1".to_string(), "one".to_string()),
            ("m3".to_string(), "s1".to_string(), "three".to_string()),
        ],
        "mirror must hold only the rows the view's predicate selects"
    );
    Ok(())
}

/// A crash between dropping the view's schema row and dropping its mirror
/// leaves an orphan mirror. Recreating the view under that name must converge
/// on exactly one mirror, not trip over the leftover.
#[test]
fn test_create_over_orphaned_mirror() {
    let dir = tempfile::TempDir::new().unwrap().keep();
    let db_path = dir.join("orphan_mirror.db");
    let csv_path = dir.join("orphan_mirror.csv");
    let opts = turso_core::DatabaseOpts::new().with_views(true);

    {
        let db = TempDatabase::new_with_existent_with_opts(&db_path, opts.clone());
        let conn = db.connect_limbo();
        setup_fdw_rows(&db, &conn, &csv_path, &[("m1", "s1", "one")]);
        common::run_query(
            &db,
            &conn,
            "CREATE MATERIALIZED VIEW mv_orphan AS SELECT uuid, body FROM msg_fdw",
        )
        .unwrap();
        conn.close().unwrap();
    }

    // Fabricate the crash: the view's schema row is gone, its mirror is not.
    // Turso refuses DML on sqlite_schema, so the surgery goes through SQLite's
    // writable_schema instead.
    {
        let sqlite = rusqlite::Connection::open(&db_path).unwrap();
        // Must come first: turso's `CREATE SERVER` rows are not SQLite syntax,
        // so any statement that parses the schema fails without it.
        sqlite.execute_batch("PRAGMA writable_schema=ON").unwrap();
        sqlite
            .execute(
                "DELETE FROM sqlite_schema WHERE name = 'mv_orphan' AND type = 'view'",
                [],
            )
            .unwrap();
        let cookie: i32 = sqlite
            .query_row("PRAGMA schema_version", [], |r| r.get(0))
            .unwrap();
        sqlite
            .execute_batch(&format!(
                "PRAGMA schema_version = {}; PRAGMA writable_schema=OFF",
                cookie + 1
            ))
            .unwrap();
        drop(sqlite);
    }

    {
        let db = TempDatabase::new_with_existent_with_opts(&db_path, opts);
        let conn = db.connect_limbo();
        common::run_query(
            &db,
            &conn,
            "CREATE MATERIALIZED VIEW mv_orphan AS SELECT uuid, body FROM msg_fdw",
        )
        .unwrap();

        let mirrors: Vec<String> = schema_names(&conn, "table")
            .into_iter()
            .filter(|t| t == &mirror_name("mv_orphan"))
            .collect();
        assert_eq!(
            mirrors.len(),
            1,
            "expected exactly one mirror row: {mirrors:?}"
        );
        assert_eq!(mirror_rows(&conn, "mv_orphan").len(), 1);
        let rows: Vec<(String, String)> = conn.exec_rows("SELECT uuid, body FROM mv_orphan");
        assert_eq!(rows, vec![("m1".to_string(), "one".to_string())]);
        conn.close().unwrap();
    }
}

/// The mirror is persistent state: reopening the database must find it, its
/// automatic index, and its rows intact.
#[test]
fn test_mirror_survives_reopen() {
    let dir = tempfile::TempDir::new().unwrap().keep();
    let db_path = dir.join("mirror_reopen.db");
    let csv_path = dir.join("mirror_reopen.csv");
    let opts = turso_core::DatabaseOpts::new().with_views(true);

    {
        let db = TempDatabase::new_with_existent_with_opts(&db_path, opts.clone());
        let conn = db.connect_limbo();
        setup_fdw_rows(
            &db,
            &conn,
            &csv_path,
            &[("m1", "s1", "one"), ("m2", "s1", "two")],
        );
        common::run_query(
            &db,
            &conn,
            "CREATE MATERIALIZED VIEW mv_reopen AS SELECT uuid, body FROM msg_fdw",
        )
        .unwrap();
        assert_eq!(mirror_rows(&conn, "mv_reopen").len(), 2);
        conn.close().unwrap();
    }

    {
        let db = TempDatabase::new_with_existent_with_opts(&db_path, opts);
        let conn = db.connect_limbo();

        assert!(
            schema_names(&conn, "table").contains(&mirror_name("mv_reopen")),
            "mirror table lost across reopen"
        );
        let index = format!("sqlite_autoindex_{}_1", mirror_name("mv_reopen"));
        assert!(
            schema_names(&conn, "index").contains(&index),
            "mirror primary-key index lost across reopen"
        );
        assert_eq!(
            mirror_rows(&conn, "mv_reopen"),
            vec![
                ("m1".to_string(), "s1".to_string(), "one".to_string()),
                ("m2".to_string(), "s1".to_string(), "two".to_string()),
            ],
            "mirror contents lost across reopen"
        );
        conn.close().unwrap();
    }
}
