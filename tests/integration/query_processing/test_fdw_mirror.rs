//! Mirrors of foreign tables that back incremental materialized views.
//!
//! A mirror is an internal btree shadowing the foreign rows a view reads. It
//! exists only when the driver declares an identity, which is what makes a row
//! recognisable across scans. The view reads the mirror rather than the foreign
//! table, and `REFRESH` syncs the mirror instead of rebuilding the view, so
//! only the rows that actually changed cost anything.

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

/// Rewrite the CSV the foreign table reads.
fn rewrite_csv(csv_path: &std::path::Path, rows: &[(&str, &str, &str)]) {
    let mut f = std::fs::File::create(csv_path).unwrap();
    writeln!(f, "uuid,session_id,body").unwrap();
    for (uuid, session, body) in rows {
        writeln!(f, "{uuid},{session},{body}").unwrap();
    }
}

/// REFRESH over a mirrored source must rebuild the mirror rather than insert
/// into it a second time, and must stay repeatable.
#[turso_macros::test(views)]
fn test_refresh_rebuilds_mirror(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("mirror_refresh.csv");
    setup_fdw_rows(&tmp_db, &conn, &csv_path, &[("m1", "s1", "one")]);

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_refresh AS SELECT uuid, body FROM msg_fdw",
    )?;

    rewrite_csv(&csv_path, &[("m1", "s1", "changed"), ("m2", "s1", "two")]);
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_refresh")?;

    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_refresh ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "changed".to_string()),
            ("m2".to_string(), "two".to_string()),
        ]
    );
    assert_eq!(
        mirror_rows(&conn, "mv_refresh"),
        vec![
            ("m1".to_string(), "s1".to_string(), "changed".to_string()),
            ("m2".to_string(), "s1".to_string(), "two".to_string()),
        ],
        "mirror must track the source across REFRESH, not accumulate"
    );

    // A second REFRESH over an unchanged source must be a no-op, not a
    // duplicate-key failure.
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_refresh")?;
    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_refresh ORDER BY uuid");
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(mirror_rows(&conn, "mv_refresh").len(), 2);

    // A row leaving the source must leave the mirror too.
    rewrite_csv(&csv_path, &[("m2", "s1", "two")]);
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_refresh")?;
    assert_eq!(
        mirror_rows(&conn, "mv_refresh"),
        vec![("m2".to_string(), "s1".to_string(), "two".to_string())]
    );
    let rows: Vec<(String, String)> = conn.exec_rows("SELECT uuid, body FROM mv_refresh");
    assert_eq!(rows, vec![("m2".to_string(), "two".to_string())]);

    Ok(())
}

/// A view built through its mirror must hold exactly the source rows. The
/// redirect happens between the user's SQL and the circuit, so this is the
/// end-to-end guard that nothing was lost in translation.
#[turso_macros::test(views)]
fn test_view_content_through_mirror(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("mirror_reads.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[("m1", "s1", "one"), ("m2", "s1", "two")],
    );

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_reads AS SELECT uuid, body FROM msg_fdw",
    )?;
    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_reads ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m2".to_string(), "two".to_string()),
        ]
    );
    Ok(())
}

/// Reading through the mirror must not disturb view shapes that keep state:
/// an aggregate is where a doubled or dropped input delta shows up first, and
/// the mirror fill stages deltas of its own alongside the population.
///
/// REFRESH is not exercised here: `REFRESH MATERIALIZED VIEW` over *any*
/// aggregate view is broken at this base (it clears the DBSP state table
/// without its automatic index, leaving "Index points to non-existent table
/// row"), independently of foreign tables. Increment 5 routes mirrored views
/// around that path entirely.
#[turso_macros::test(views)]
fn test_mirrored_aggregate_view_counts_each_row_once(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("mirror_agg.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[
            ("m1", "s1", "one"),
            ("m2", "s1", "two"),
            ("m3", "s2", "three"),
        ],
    );

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_agg AS \
         SELECT session_id, count(*) AS n FROM msg_fdw GROUP BY session_id",
    )?;
    let counts: Vec<(String, i64)> =
        conn.exec_rows("SELECT session_id, n FROM mv_agg ORDER BY session_id");
    assert_eq!(
        counts,
        vec![("s1".to_string(), 2), ("s2".to_string(), 1)],
        "population through the mirror must count every source row exactly once"
    );
    Ok(())
}

/// A predicate-scoped view keeps its scope once it reads the mirror.
#[turso_macros::test(views)]
fn test_mirrored_view_keeps_its_predicate(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("mirror_pred.csv");
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
        "CREATE MATERIALIZED VIEW mv_pred AS \
         SELECT uuid, body FROM msg_fdw WHERE session_id = 's1'",
    )?;
    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_pred ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m3".to_string(), "three".to_string()),
        ]
    );
    Ok(())
}

/// A qualified column reference must keep resolving after the source under it
/// becomes the mirror.
#[turso_macros::test(views)]
fn test_mirrored_view_resolves_qualified_columns(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("mirror_qual.csv");
    setup_fdw_rows(&tmp_db, &conn, &csv_path, &[("m1", "s1", "one")]);

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_qual AS \
         SELECT msg_fdw.uuid, msg_fdw.body FROM msg_fdw WHERE msg_fdw.session_id = 's1'",
    )?;
    let rows: Vec<(String, String)> = conn.exec_rows("SELECT uuid, body FROM mv_qual");
    assert_eq!(rows, vec![("m1".to_string(), "one".to_string())]);
    Ok(())
}

/// An aliased source must keep its user-written alias after the redirect.
#[turso_macros::test(views)]
fn test_mirrored_view_keeps_user_alias(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("mirror_alias.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[("m1", "s1", "one"), ("m2", "s2", "two")],
    );

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_alias AS \
         SELECT m.uuid, m.body FROM msg_fdw m WHERE m.session_id = 's1'",
    )?;
    let rows: Vec<(String, String)> = conn.exec_rows("SELECT uuid, body FROM mv_alias");
    assert_eq!(rows, vec![("m1".to_string(), "one".to_string())]);
    Ok(())
}

/// Row-level CDC records recorded for `table`.
///
/// The filter on `table_name` is what makes this a sharp oracle: at this base
/// every autocommit statement also writes a transaction-boundary COMMIT record
/// (empty `table_name`) even when it captured nothing. That is fixed upstream
/// by 4d1c058d ("translate: gate the CDC autocommit COMMIT record on captured
/// changes"), which is not in this stream's base — drop the filter once it
/// merges.
fn cdc_row_records(conn: &std::sync::Arc<turso_core::Connection>, table: &str) -> i64 {
    let rows: Vec<(i64,)> = conn.exec_rows(&format!(
        "SELECT count(*) FROM turso_cdc WHERE table_name = '{table}'"
    ));
    rows[0].0
}

/// A REFRESH whose source did not change must be completely inert: no row of
/// the mirror is written, so no delta reaches the view and nothing is captured.
/// This is the property the whole sweep exists for — a rebuild would retract
/// and re-insert every row.
#[turso_macros::test(views)]
fn test_sweep_over_unchanged_source_is_inert(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("sweep_inert.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[("m1", "s1", "one"), ("m2", "s1", "two")],
    );
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_inert AS SELECT uuid, body FROM msg_fdw",
    )?;

    conn.execute("PRAGMA capture_data_changes_conn('full')")?;
    let mirror = mirror_name("mv_inert");

    for _ in 0..3 {
        common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_inert")?;
    }

    assert_eq!(
        cdc_row_records(&conn, &mirror),
        0,
        "a no-change REFRESH must write nothing to the mirror"
    );
    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_inert ORDER BY uuid");
    assert_eq!(rows.len(), 2, "{rows:?}");
    Ok(())
}

/// Reordering the source's lines changes no row, so it must change nothing.
/// This fails loudly under a rebuild, where a row's identity is its scan
/// position.
#[turso_macros::test(views)]
fn test_sweep_ignores_source_reordering(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("sweep_reorder.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[
            ("m1", "s1", "one"),
            ("m2", "s1", "two"),
            ("m3", "s1", "three"),
        ],
    );
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_reorder AS SELECT uuid, body FROM msg_fdw",
    )?;

    conn.execute("PRAGMA capture_data_changes_conn('full')")?;
    rewrite_csv(
        &csv_path,
        &[
            ("m3", "s1", "three"),
            ("m1", "s1", "one"),
            ("m2", "s1", "two"),
        ],
    );
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_reorder")?;

    assert_eq!(
        cdc_row_records(&conn, &mirror_name("mv_reorder")),
        0,
        "reordering the source must not disturb a single row"
    );
    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_reorder ORDER BY uuid");
    assert_eq!(rows.len(), 3, "{rows:?}");
    Ok(())
}

/// One appended source row must cost exactly one change.
#[turso_macros::test(views)]
fn test_sweep_appended_row_costs_one_change(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("sweep_append.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[("m1", "s1", "one"), ("m2", "s1", "two")],
    );
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_append AS SELECT uuid, body FROM msg_fdw",
    )?;

    conn.execute("PRAGMA capture_data_changes_conn('full')")?;
    rewrite_csv(
        &csv_path,
        &[
            ("m1", "s1", "one"),
            ("m2", "s1", "two"),
            ("m3", "s1", "three"),
        ],
    );
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_append")?;

    assert_eq!(
        cdc_row_records(&conn, &mirror_name("mv_append")),
        1,
        "one appended source row must touch exactly one mirror row"
    );
    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_append ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m2".to_string(), "two".to_string()),
            ("m3".to_string(), "three".to_string()),
        ]
    );
    Ok(())
}

/// A row leaving the source must be retracted from the view and from every
/// matview chained onto it, at the cost of exactly one change.
#[turso_macros::test(views)]
fn test_sweep_deleted_row_retracts_through_the_chain(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("sweep_delete.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[
            ("m1", "s1", "one"),
            ("m2", "s1", "two"),
            ("m3", "s1", "three"),
        ],
    );
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_del AS SELECT uuid, body FROM msg_fdw",
    )?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_del_downstream AS \
         SELECT uuid FROM mv_del WHERE uuid <> 'm1'",
    )?;

    conn.execute("PRAGMA capture_data_changes_conn('full')")?;
    rewrite_csv(&csv_path, &[("m1", "s1", "one"), ("m3", "s1", "three")]);
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_del")?;

    assert_eq!(
        cdc_row_records(&conn, &mirror_name("mv_del")),
        1,
        "one vanished source row must touch exactly one mirror row"
    );
    let rows: Vec<(String,)> = conn.exec_rows("SELECT uuid FROM mv_del ORDER BY uuid");
    assert_eq!(
        rows,
        vec![("m1".to_string(),), ("m3".to_string(),)],
        "the vanished row must leave the view"
    );
    let downstream: Vec<(String,)> =
        conn.exec_rows("SELECT uuid FROM mv_del_downstream ORDER BY uuid");
    assert_eq!(
        downstream,
        vec![("m3".to_string(),)],
        "the retraction must reach the chained matview"
    );
    Ok(())
}

/// A changed row must be updated in place, not retracted and re-inserted. The
/// rowid alone is a weak oracle — a rebuild reassigns rowids in scan order and
/// so often reproduces them — so the change count carries the assertion.
#[turso_macros::test(views)]
fn test_sweep_changed_row_updates_in_place(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("sweep_change.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[("m1", "s1", "one"), ("m2", "s1", "two")],
    );
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_change AS SELECT uuid, body FROM msg_fdw",
    )?;
    let mirror = mirror_name("mv_change");
    let rowids_before: Vec<(String, i64)> = conn.exec_rows(&format!(
        "SELECT uuid, rowid FROM \"{mirror}\" ORDER BY uuid"
    ));

    conn.execute("PRAGMA capture_data_changes_conn('full')")?;

    rewrite_csv(&csv_path, &[("m1", "s1", "CHANGED"), ("m2", "s1", "two")]);
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_change")?;

    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_change ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "CHANGED".to_string()),
            ("m2".to_string(), "two".to_string()),
        ]
    );
    let rowids_after: Vec<(String, i64)> = conn.exec_rows(&format!(
        "SELECT uuid, rowid FROM \"{mirror}\" ORDER BY uuid"
    ));
    assert_eq!(
        rowids_after, rowids_before,
        "an updated row must keep its rowid, or its retraction carries a different identity"
    );
    assert_eq!(
        cdc_row_records(&conn, &mirror),
        1,
        "only the row that changed may be written"
    );
    Ok(())
}

/// An aggregate over a mirrored source must survive REFRESH. The sweep leaves
/// the DBSP state alone, so it also sidesteps the pre-existing clear-without-
/// its-index defect that breaks REFRESH for every rebuilt aggregate view.
#[turso_macros::test(views)]
fn test_sweep_maintains_an_aggregate_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("sweep_agg.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[
            ("m1", "s1", "one"),
            ("m2", "s1", "two"),
            ("m3", "s2", "three"),
        ],
    );
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_sweep_agg AS \
         SELECT session_id, count(*) AS n FROM msg_fdw GROUP BY session_id",
    )?;

    rewrite_csv(
        &csv_path,
        &[
            ("m1", "s1", "one"),
            ("m3", "s2", "three"),
            ("m4", "s2", "four"),
        ],
    );
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_sweep_agg")?;

    let counts: Vec<(String, i64)> =
        conn.exec_rows("SELECT session_id, n FROM mv_sweep_agg ORDER BY session_id");
    assert_eq!(
        counts,
        vec![("s1".to_string(), 1), ("s2".to_string(), 2)],
        "one row left s1 and one joined s2"
    );

    // Repeat REFRESHes over an unchanged source must not drift the counts.
    for _ in 0..3 {
        common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_sweep_agg")?;
    }
    let counts: Vec<(String, i64)> =
        conn.exec_rows("SELECT session_id, n FROM mv_sweep_agg ORDER BY session_id");
    assert_eq!(counts, vec![("s1".to_string(), 1), ("s2".to_string(), 2)]);
    Ok(())
}

/// A view joining a mirrored source to a local table: REFRESH sweeps the mirror
/// and leaves the local side alone, which is enough because local writes are
/// already incremental.
#[turso_macros::test(views)]
fn test_sweep_maintains_a_mixed_source_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("sweep_mixed.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[("m1", "s1", "one"), ("m2", "s2", "two")],
    );
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE TABLE sessions (session_id TEXT, label TEXT)",
    )?;
    common::run_query(
        &tmp_db,
        &conn,
        "INSERT INTO sessions VALUES ('s1', 'first'), ('s2', 'second')",
    )?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_mixed AS \
         SELECT msg_fdw.uuid, msg_fdw.body, sessions.label \
         FROM msg_fdw JOIN sessions ON msg_fdw.session_id = sessions.session_id",
    )?;
    let rows: Vec<(String, String, String)> =
        conn.exec_rows("SELECT uuid, body, label FROM mv_mixed ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "one".to_string(), "first".to_string()),
            ("m2".to_string(), "two".to_string(), "second".to_string()),
        ]
    );

    // A local write needs no REFRESH.
    common::run_query(
        &tmp_db,
        &conn,
        "INSERT INTO sessions VALUES ('s3', 'third')",
    )?;
    // A foreign write needs one, and must join against the local side.
    rewrite_csv(
        &csv_path,
        &[
            ("m1", "s1", "one"),
            ("m2", "s2", "two"),
            ("m3", "s3", "three"),
        ],
    );
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_mixed")?;

    let rows: Vec<(String, String, String)> =
        conn.exec_rows("SELECT uuid, body, label FROM mv_mixed ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "one".to_string(), "first".to_string()),
            ("m2".to_string(), "two".to_string(), "second".to_string()),
            ("m3".to_string(), "three".to_string(), "third".to_string()),
        ],
        "the swept mirror must join against the local table's current contents"
    );
    Ok(())
}

/// A mirror sync is DML like any other, so it must be able to run inside a
/// transaction the user opened, joining it rather than demanding its own.
#[turso_macros::test(views)]
fn test_create_mirrored_view_inside_transaction(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("intx_create.csv");
    setup_fdw_rows(
        &tmp_db,
        &conn,
        &csv_path,
        &[("m1", "s1", "one"), ("m2", "s1", "two")],
    );

    common::run_query(&tmp_db, &conn, "BEGIN")?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_intx_create AS SELECT uuid, body FROM msg_fdw",
    )?;
    common::run_query(&tmp_db, &conn, "COMMIT")?;

    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_intx_create ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m2".to_string(), "two".to_string()),
        ]
    );
    Ok(())
}

/// The same for the sweep: `REFRESH` inside an explicit transaction must apply
/// with the enclosing `COMMIT`.
#[turso_macros::test(views)]
fn test_refresh_mirrored_view_inside_transaction(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("intx_refresh.csv");
    setup_fdw_rows(&tmp_db, &conn, &csv_path, &[("m1", "s1", "one")]);
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_intx_refresh AS SELECT uuid, body FROM msg_fdw",
    )?;

    rewrite_csv(&csv_path, &[("m1", "s1", "one"), ("m2", "s1", "two")]);
    common::run_query(&tmp_db, &conn, "BEGIN")?;
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_intx_refresh")?;
    common::run_query(&tmp_db, &conn, "COMMIT")?;

    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_intx_refresh ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m2".to_string(), "two".to_string()),
        ],
        "a sweep inside a transaction must apply when that transaction commits"
    );
    Ok(())
}

/// A swept mirror is transaction state like any other: rolling back must undo
/// the mirror's rows *and* the view's, leaving both as they were.
#[turso_macros::test(views)]
fn test_refresh_mirrored_view_rolled_back(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("intx_rollback.csv");
    setup_fdw_rows(&tmp_db, &conn, &csv_path, &[("m1", "s1", "one")]);
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_intx_rb AS SELECT uuid, body FROM msg_fdw",
    )?;
    let mirror_before = mirror_rows(&conn, "mv_intx_rb");
    let view_before: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_intx_rb ORDER BY uuid");

    rewrite_csv(&csv_path, &[("m1", "s1", "changed"), ("m2", "s1", "two")]);
    common::run_query(&tmp_db, &conn, "BEGIN")?;
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_intx_rb")?;
    common::run_query(&tmp_db, &conn, "ROLLBACK")?;

    assert_eq!(
        mirror_rows(&conn, "mv_intx_rb"),
        mirror_before,
        "a rolled-back sweep must leave the mirror untouched"
    );
    let view_after: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_intx_rb ORDER BY uuid");
    assert_eq!(
        view_after, view_before,
        "a rolled-back sweep must leave the view untouched"
    );
    Ok(())
}

/// A `CREATE MATERIALIZED VIEW` that fails while populating from a foreign
/// source is one aborted statement, not a poisoned database. It fails after its
/// own `SetCookie`, so the connection's in-memory schema version is one ahead of
/// the cookie the statement rolled back; committing the surrounding transaction
/// publishes that phantom version to every connection, and each of them then
/// compares it against the real cookie and reprepares forever.
///
/// What this pins is that the abort leaves the schema where the statement found
/// it, so the transaction it ran in stays usable and so does the database.
#[turso_macros::test(views)]
fn test_failed_mirror_create_in_transaction_leaves_the_database_usable(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    use crate::query_processing::fdw_test_driver::MemFdw;
    use turso_core::Value;

    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new("CREATE TABLE msg_wedge(uuid TEXT, body TEXT)", vec![0]);
    // A good row first, so the failure lands mid-populate rather than on the
    // very first row the scan sees.
    rows.set(vec![
        vec![Value::build_text("u1"), Value::build_text("one")],
        vec![Value::Null, Value::build_text("orphan")],
    ]);
    conn.register_foreign_table("msg_wedge", fdw)?;
    common::run_query(&tmp_db, &conn, "CREATE TABLE existing (x)")?;
    common::run_query(&tmp_db, &conn, "INSERT INTO existing VALUES (1)")?;

    common::run_query(&tmp_db, &conn, "BEGIN")?;
    common::run_query(&tmp_db, &conn, "CREATE TABLE marker (x)")?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_wedge AS SELECT uuid, body FROM msg_wedge",
    )
    .expect_err("a NULL identity must fail the create");
    common::run_query(&tmp_db, &conn, "COMMIT")?;

    common::run_query(&tmp_db, &conn, "CREATE TABLE probe_after (x)")
        .expect("DDL must still work after the failed create");
    let existing: Vec<(i64,)> = conn.exec_rows("SELECT x FROM existing");
    assert_eq!(
        existing,
        vec![(1,)],
        "reading an existing table must still work"
    );

    let fresh = tmp_db.connect_limbo();
    let from_fresh: Vec<(i64,)> = fresh.exec_rows("SELECT x FROM existing");
    assert_eq!(
        from_fresh,
        vec![(1,)],
        "a connection opened after the failure must not inherit a stale schema version"
    );
    common::run_query(&tmp_db, &fresh, "CREATE TABLE probe_fresh (x)")
        .expect("DDL on a fresh connection must work too");
    Ok(())
}

/// A foreign table whose declared name is not already lower case.
///
/// The schema keys tables by their folded name but a virtual table keeps the
/// spelling it was declared with, so a mirror source is the one place the two
/// forms meet. `declared` is interpolated into the DDL verbatim, so a case can
/// pass a quoted identifier.
fn setup_fdw_named(
    tmp_db: &TempDatabase,
    conn: &std::sync::Arc<turso_core::Connection>,
    declared: &str,
    csv_path: &std::path::Path,
    rows: &[(&str, &str, &str)],
) {
    rewrite_csv(csv_path, rows);
    common::run_query(tmp_db, conn, "CREATE SERVER csv_srv OPTIONS (driver 'csv')").unwrap();
    common::run_query(
        tmp_db,
        conn,
        &format!(
            "CREATE FOREIGN TABLE {declared} (uuid TEXT, session_id TEXT, body TEXT) \
             SERVER csv_srv OPTIONS (path '{}', skip_header 'true', identity 'uuid')",
            csv_path.display()
        ),
    )
    .unwrap();
}

/// A source declared in mixed case must be usable as a mirror source at all.
/// The mirror's identity is folded, so anything comparing it against the
/// verbatim declared name fails to match and the view cannot be created.
#[turso_macros::test(views)]
fn test_mixed_case_declared_source_is_mirrored(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("declared_mixed.csv");
    setup_fdw_named(
        &tmp_db,
        &conn,
        "MsgSrc",
        &csv_path,
        &[("m1", "s1", "one"), ("m2", "s1", "two")],
    );

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_declared AS SELECT uuid, body FROM MsgSrc",
    )?;
    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_declared ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m2".to_string(), "two".to_string()),
        ]
    );

    // The sweep round-trip: the mirror tracks the source, so the view does too.
    rewrite_csv(&csv_path, &[("m1", "s1", "changed"), ("m3", "s1", "three")]);
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_declared")?;
    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_declared ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "changed".to_string()),
            ("m3".to_string(), "three".to_string()),
        ]
    );
    Ok(())
}

/// A quoted declaration preserves case just as a bare one folds nowhere, and
/// reaches the same seam.
#[turso_macros::test(views)]
fn test_quoted_declared_source_is_mirrored(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("declared_quoted.csv");
    setup_fdw_named(
        &tmp_db,
        &conn,
        "\"MyTable\"",
        &csv_path,
        &[("m1", "s1", "one")],
    );

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_quoted AS SELECT uuid, body FROM \"MyTable\"",
    )?;
    let rows: Vec<(String, String)> = conn.exec_rows("SELECT uuid, body FROM mv_quoted");
    assert_eq!(rows, vec![("m1".to_string(), "one".to_string())]);

    // The source is still reachable under a folded reference, and the sweep
    // keeps the view in step.
    rewrite_csv(&csv_path, &[("m1", "s1", "one"), ("m2", "s1", "two")]);
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_quoted")?;
    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_quoted ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m2".to_string(), "two".to_string()),
        ]
    );
    Ok(())
}

/// The view's predicate scopes the mirror, and the predicate is looked up by
/// the source's name. A verbatim declared name misses a folded lookup key, and
/// the miss is silent: the mirror fills from an unscoped scan of a source the
/// driver may not even be able to enumerate.
#[turso_macros::test(views)]
fn test_mixed_case_declared_source_keeps_its_predicate(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let csv_path = tmp_db.path.parent().unwrap().join("declared_scoped.csv");
    setup_fdw_named(
        &tmp_db,
        &conn,
        "MsgScoped",
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
        "CREATE MATERIALIZED VIEW mv_scoped AS \
         SELECT uuid, body FROM MsgScoped WHERE session_id = 's1'",
    )?;

    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_scoped ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "one".to_string()),
            ("m3".to_string(), "three".to_string()),
        ]
    );

    let mirrored: Vec<(String,)> = conn.exec_rows(
        "SELECT uuid FROM \"__turso_internal_fdw_mirror_v1_mv_scoped__msgscoped\" ORDER BY uuid",
    );
    assert_eq!(
        mirrored,
        vec![("m1".to_string(),), ("m3".to_string(),)],
        "the mirror must hold only the rows the view's predicate selects"
    );
    Ok(())
}
