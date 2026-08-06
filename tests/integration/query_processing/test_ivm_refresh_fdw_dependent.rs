//! A foreign source that declares no identity gets no mirror, so `REFRESH` of a matview
//! over it is the destructive clear-and-rebuild — the only way to pull in rows the
//! foreign source has grown or dropped. That must remain available when the matview has a
//! dependent, the dependent must follow the rebuild, and it must do so *within* the
//! transaction: an ordinary DML change and a mirror-fed sweep are both readable by their
//! dependents before `COMMIT`, and a rebuild is no different.

use crate::common::{self, ExecRows, TempDatabase};
use std::io::Write;

fn write_csv(path: &std::path::Path, rows: &[&str]) -> anyhow::Result<()> {
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "id,name,score")?;
    for row in rows {
        writeln!(f, "{row}")?;
    }
    Ok(())
}

fn dep1(conn: &std::sync::Arc<turso_core::Connection>) -> Vec<(i64, i64)> {
    conn.exec_rows("SELECT n, CAST(tot AS INTEGER) FROM dep1")
}

fn dep2(conn: &std::sync::Arc<turso_core::Connection>) -> Vec<(i64,)> {
    conn.exec_rows("SELECT n10 FROM dep2")
}

/// `scores` (CSV, no identity) -> `mvs` -> `dep1` -> `dep2`.
fn seed_chain(tmp_db: &TempDatabase, csv_path: &std::path::Path) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    common::run_query(
        tmp_db,
        &conn,
        "CREATE SERVER csv_srv OPTIONS (driver 'csv')",
    )?;
    // No `identity` option: no mirror is created, so REFRESH is a full rebuild.
    common::run_query(
        tmp_db,
        &conn,
        &format!(
            "CREATE FOREIGN TABLE scores (id TEXT, name TEXT, score TEXT) \
             SERVER csv_srv OPTIONS (path '{}', skip_header 'true')",
            csv_path.display()
        ),
    )?;
    common::run_query(
        tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mvs AS SELECT id, name, score FROM scores",
    )?;
    common::run_query(
        tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW dep1 AS \
         SELECT count(*) AS n, sum(CAST(score AS INTEGER)) AS tot FROM mvs",
    )?;
    common::run_query(
        tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW dep2 AS SELECT n * 10 AS n10 FROM dep1",
    )?;
    Ok(())
}

#[turso_macros::test(views)]
fn test_refresh_identityless_fdw_matview_with_dependent(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    let csv_path = tmp_db.path.parent().unwrap().join("scores.csv");
    write_csv(&csv_path, &["1,alice,90", "2,bob,75", "3,carol,60"])?;

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE SERVER csv_srv OPTIONS (driver 'csv')",
    )?;
    // No `identity` option: no mirror is created, so REFRESH is a full rebuild.
    common::run_query(
        &tmp_db,
        &conn,
        &format!(
            "CREATE FOREIGN TABLE scores (id TEXT, name TEXT, score TEXT) \
             SERVER csv_srv OPTIONS (path '{}', skip_header 'true')",
            csv_path.display()
        ),
    )?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mvs AS SELECT id, name, score FROM scores",
    )?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mvdep AS SELECT count(*) AS n FROM mvs",
    )?;

    let n: Vec<(i64,)> = conn.exec_rows("SELECT n FROM mvdep");
    assert_eq!(n, vec![(3,)]);

    write_csv(
        &csv_path,
        &["1,alice,90", "2,bob,75", "3,carol,60", "4,dave,42"],
    )?;
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mvs")?;

    let ids: Vec<(String,)> = conn.exec_rows("SELECT id FROM mvs ORDER BY id");
    assert_eq!(
        ids,
        vec![
            ("1".to_string(),),
            ("2".to_string(),),
            ("3".to_string(),),
            ("4".to_string(),)
        ],
        "the rebuild must pick up the row the foreign source grew"
    );

    let n: Vec<(i64,)> = conn.exec_rows("SELECT n FROM mvdep");
    assert_eq!(n, vec![(4,)], "the dependent must follow the rebuild");

    Ok(())
}

/// The refreshed view reads its own rebuild before `COMMIT`; so must every view defined
/// over it, at any depth. Read-your-own-writes is what the ordinary DML path and the
/// mirror-fed sweep both give, and a rebuild that only becomes visible at commit is a
/// hole in the same guarantee.
#[turso_macros::test(views)]
fn test_refresh_dependents_are_fresh_before_commit(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    let csv_path = tmp_db.path.parent().unwrap().join("scores.csv");
    write_csv(&csv_path, &["1,alice,90", "2,bob,75", "3,carol,28"])?;
    seed_chain(&tmp_db, &csv_path)?;

    assert_eq!(dep1(&conn), vec![(3, 193)]);
    assert_eq!(dep2(&conn), vec![(30,)]);

    // The foreign source collapses to a single row — a rebuild that actually changes
    // content, which a local-table REFRESH can never produce.
    write_csv(&csv_path, &["9,zed,5"])?;

    common::run_query(&tmp_db, &conn, "BEGIN")?;
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mvs")?;

    let mvs_ids: Vec<(String,)> = conn.exec_rows("SELECT id FROM mvs");
    assert_eq!(
        mvs_ids,
        vec![("9".to_string(),)],
        "the refreshed view reads its own rebuild in-txn"
    );
    assert_eq!(
        dep1(&conn),
        vec![(1, 5)],
        "the direct dependent must read the rebuild in-txn, not the pre-REFRESH value"
    );
    assert_eq!(
        dep2(&conn),
        vec![(10,)],
        "the transitive dependent must read the rebuild in-txn too"
    );

    common::run_query(&tmp_db, &conn, "COMMIT")?;

    assert_eq!(dep1(&conn), vec![(1, 5)]);
    assert_eq!(dep2(&conn), vec![(10,)]);

    Ok(())
}

/// The transitive level on its own, so its staleness is visible even when the direct
/// dependent is never read inside the transaction.
#[turso_macros::test(views)]
fn test_refresh_transitive_dependent_is_fresh_before_commit(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    let csv_path = tmp_db.path.parent().unwrap().join("scores.csv");
    write_csv(&csv_path, &["1,alice,90", "2,bob,75", "3,carol,28"])?;
    seed_chain(&tmp_db, &csv_path)?;
    assert_eq!(dep2(&conn), vec![(30,)]);

    write_csv(&csv_path, &["9,zed,5"])?;

    common::run_query(&tmp_db, &conn, "BEGIN")?;
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mvs")?;
    assert_eq!(
        dep2(&conn),
        vec![(10,)],
        "a dependent two levels down must read the rebuild in-txn"
    );
    common::run_query(&tmp_db, &conn, "COMMIT")?;
    assert_eq!(dep2(&conn), vec![(10,)]);

    Ok(())
}

/// The committed outcome must survive the in-txn reads above — a read that feeds the
/// wrong delta into a dependent's circuit would show up here as a double-application.
#[turso_macros::test(views)]
fn test_refresh_read_in_txn_then_commit_is_not_double_applied(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    let csv_path = tmp_db.path.parent().unwrap().join("scores.csv");
    write_csv(&csv_path, &["1,alice,90", "2,bob,75", "3,carol,28"])?;
    seed_chain(&tmp_db, &csv_path)?;

    write_csv(&csv_path, &["9,zed,5", "10,yan,7"])?;

    common::run_query(&tmp_db, &conn, "BEGIN")?;
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mvs")?;
    // Read every level while the transaction is open, then read them again after commit.
    let _ = dep1(&conn);
    let _ = dep2(&conn);
    common::run_query(&tmp_db, &conn, "COMMIT")?;

    assert_eq!(dep1(&conn), vec![(2, 12)]);
    assert_eq!(dep2(&conn), vec![(20,)]);

    Ok(())
}

/// A rollback after in-txn dependent reads must leave every level at its pre-REFRESH
/// value: the reads must not have folded anything into a circuit's committed state.
#[turso_macros::test(views)]
fn test_refresh_read_in_txn_then_rollback_reverts(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    let csv_path = tmp_db.path.parent().unwrap().join("scores.csv");
    write_csv(&csv_path, &["1,alice,90", "2,bob,75", "3,carol,28"])?;
    seed_chain(&tmp_db, &csv_path)?;

    write_csv(&csv_path, &["9,zed,5"])?;

    common::run_query(&tmp_db, &conn, "BEGIN")?;
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mvs")?;
    assert_eq!(dep1(&conn), vec![(1, 5)]);
    common::run_query(&tmp_db, &conn, "ROLLBACK")?;

    let mvs_ids: Vec<(String,)> = conn.exec_rows("SELECT id FROM mvs ORDER BY id");
    assert_eq!(mvs_ids.len(), 3);
    assert_eq!(dep1(&conn), vec![(3, 193)]);
    assert_eq!(dep2(&conn), vec![(30,)]);

    Ok(())
}
