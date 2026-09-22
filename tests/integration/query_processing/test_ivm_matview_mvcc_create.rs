//! Materialized views must be refused cleanly under `journal_mode=mvcc`, on both
//! the DDL and the DML side.
//!
//! `CREATE MATERIALIZED VIEW` used to panic in `op_drop_table` ("DROP TABLE: table
//! must exist in schema") on the DBSP state table's speculative orphan teardown.
//! Removing that panic only exposed the real incompatibility: under MVCC the
//! matview and DBSP btrees get negative, non-checkpointed root pages, which the IVM
//! cursors cannot read and which the schema parser rejects on reload — so a
//! successfully-created matview leaves a file that no longer opens, in MVCC or WAL.
//!
//! The DDL refusal alone is only half a fence: a matview created in WAL mode and
//! then written to under MVCC computed its maintenance, served it to the
//! same-session read, and discarded it — the view btree writes go through the pager
//! while the commit goes through the MV store, so later WAL sessions saw permanent
//! divergence. That DML is refused too, at commit time.

use std::sync::Arc;

use tempfile::TempDir;
use turso_core::Connection;

use crate::common::{self, ExecRows, TempDatabase};

const REFUSAL: &str = "Materialized views are not supported in MVCC mode";

fn rows_of(conn: &Arc<Connection>, sql: &str) -> Vec<(i64, String)> {
    conn.exec_rows(sql)
}

#[turso_macros::test(views, mvcc)]
fn test_matview_create_under_mvcc(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)",
    )?;
    common::run_query(&tmp_db, &conn, "INSERT INTO t VALUES (1, 'x')")?;

    let create = common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW v AS SELECT a, b FROM t",
    );

    if !tmp_db.enable_mvcc {
        // WAL control: the same DDL keeps working, including incremental maintenance.
        create?;
        let rows: Vec<(i64, String)> = conn.exec_rows("SELECT a, b FROM v ORDER BY a");
        assert_eq!(rows, vec![(1, "x".to_string())]);

        common::run_query(&tmp_db, &conn, "INSERT INTO t VALUES (2, 'y')")?;
        let rows: Vec<(i64, String)> = conn.exec_rows("SELECT a, b FROM v ORDER BY a");
        assert_eq!(rows, vec![(1, "x".to_string()), (2, "y".to_string())]);

        return Ok(());
    }

    let err = create.expect_err("CREATE MATERIALIZED VIEW must be refused under MVCC");
    assert!(
        err.to_string()
            .contains("Materialized views are not supported in MVCC mode"),
        "unexpected error: {err}"
    );

    // The refusal must leave the connection usable...
    common::run_query(&tmp_db, &conn, "INSERT INTO t VALUES (2, 'y')")?;
    let rows: Vec<(i64, String)> = conn.exec_rows("SELECT a, b FROM t ORDER BY a");
    assert_eq!(rows, vec![(1, "x".to_string()), (2, "y".to_string())]);
    conn.close()?;

    // ...and the file openable. A matview schema row written under MVCC carries a
    // negative root page, which schema load rejects as corruption.
    let reopened = TempDatabase::builder()
        .with_db_path(&tmp_db.path)
        .with_opts(tmp_db.db_opts)
        .with_mvcc(true)
        .build();
    let conn = reopened.connect_limbo();
    let rows: Vec<(i64, String)> = conn.exec_rows("SELECT a, b FROM t ORDER BY a");
    assert_eq!(rows, vec![(1, "x".to_string()), (2, "y".to_string())]);

    Ok(())
}

/// A matview created under WAL must not be silently diverged by MVCC writes:
/// the DML is refused and the view stays correct for later WAL sessions.
#[test]
fn test_matview_dml_refused_under_mvcc() -> anyhow::Result<()> {
    let path = TempDir::new().unwrap().keep().join("matview_mvcc_dml.db");

    {
        let db = TempDatabase::builder()
            .with_db_path(&path)
            .with_views(true)
            .build();
        let conn = db.connect_limbo();
        common::run_query(&db, &conn, "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)")?;
        common::run_query(&db, &conn, "INSERT INTO t VALUES (1, 'x')")?;
        common::run_query(
            &db,
            &conn,
            "CREATE MATERIALIZED VIEW v AS SELECT a, b FROM t",
        )?;
        assert_eq!(
            rows_of(&conn, "SELECT a, b FROM v ORDER BY a"),
            vec![(1, "x".to_string())]
        );
        conn.close()?;
    }

    {
        let db = TempDatabase::builder()
            .with_db_path(&path)
            .with_views(true)
            .with_mvcc(true)
            .build();
        let conn = db.connect_limbo();

        for dml in [
            "INSERT INTO t VALUES (2, 'y')",
            "UPDATE t SET b = 'z' WHERE a = 1",
            "DELETE FROM t WHERE a = 1",
        ] {
            let err = common::run_query(&db, &conn, dml)
                .expect_err("DML on a table with a dependent matview must be refused under MVCC");
            assert!(
                err.to_string().contains(REFUSAL),
                "unexpected error for `{dml}`: {err}"
            );
        }
        conn.close()?;
    }

    {
        let db = TempDatabase::builder()
            .with_db_path(&path)
            .with_views(true)
            .build();
        let conn = db.connect_limbo();
        assert_eq!(
            rows_of(&conn, "SELECT a, b FROM t ORDER BY a"),
            vec![(1, "x".to_string())],
            "refused DML must not have landed in the base table"
        );
        assert_eq!(
            rows_of(&conn, "SELECT a, b FROM v ORDER BY a"),
            vec![(1, "x".to_string())],
            "matview must not have diverged from its base table"
        );
    }

    Ok(())
}

/// The refusal must key on dependent matviews, not on MVCC alone.
#[turso_macros::test(views, mvcc)]
fn test_dml_without_matview_unaffected(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)",
    )?;
    common::run_query(&tmp_db, &conn, "INSERT INTO t VALUES (1, 'x'), (2, 'y')")?;
    common::run_query(&tmp_db, &conn, "UPDATE t SET b = 'z' WHERE a = 1")?;
    common::run_query(&tmp_db, &conn, "DELETE FROM t WHERE a = 2")?;
    assert_eq!(
        rows_of(&conn, "SELECT a, b FROM t ORDER BY a"),
        vec![(1, "z".to_string())]
    );
    Ok(())
}

/// Switching to MVCC does not bump the schema cookie, so a statement prepared
/// under WAL is not reprepared. A translate-time guard would miss it; the
/// commit-time refusal catches it.
#[test]
fn test_matview_dml_prepared_under_wal_stepped_under_mvcc() -> anyhow::Result<()> {
    let db = TempDatabase::builder()
        .with_db_name("matview_mvcc_prepared.db")
        .with_views(true)
        .build();
    let conn = db.connect_limbo();
    common::run_query(&db, &conn, "CREATE TABLE t (a INTEGER PRIMARY KEY, b TEXT)")?;
    common::run_query(&db, &conn, "INSERT INTO t VALUES (1, 'x')")?;
    common::run_query(
        &db,
        &conn,
        "CREATE MATERIALIZED VIEW v AS SELECT a, b FROM t",
    )?;

    let mut stmt = conn
        .query("INSERT INTO t VALUES (2, 'y')")?
        .expect("INSERT must produce a statement");

    common::run_query(&db, &conn, "PRAGMA journal_mode=mvcc")?;

    let err = stmt
        .run_with_row_callback(Box::new(|_: &turso_core::Row| Ok(())))
        .expect_err("INSERT stepped under MVCC must be refused");
    assert!(err.to_string().contains(REFUSAL), "unexpected error: {err}");

    assert_eq!(
        rows_of(&conn, "SELECT a, b FROM v ORDER BY a"),
        vec![(1, "x".to_string())]
    );

    Ok(())
}
