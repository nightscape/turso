use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value;
use std::sync::Arc;

fn seed_chain(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE t (a INTEGER, b INTEGER)")?;
    conn.execute("INSERT INTO t VALUES (1, 10), (1, 20), (2, 30)")?;
    conn.execute("CREATE MATERIALIZED VIEW mv1 AS SELECT a, sum(b) AS s FROM t GROUP BY a")?;
    conn.execute("CREATE MATERIALIZED VIEW mv2 AS SELECT count(*) AS n, sum(s) AS tot FROM mv1")?;
    conn.execute("CREATE MATERIALIZED VIEW mv3 AS SELECT count(*) AS c, sum(tot) AS tt FROM mv2")?;
    Ok(())
}

fn assert_chain_matches_oracle(conn: &Arc<turso_core::Connection>, when: &str) {
    let per_a = "SELECT a, sum(b) AS s FROM t GROUP BY a";
    let checks = [
        (
            "SELECT a, s FROM mv1 ORDER BY a".to_string(),
            format!("{per_a} ORDER BY a"),
        ),
        (
            "SELECT n, tot FROM mv2".to_string(),
            format!("SELECT count(*), sum(s) FROM ({per_a})"),
        ),
        (
            "SELECT c, tt FROM mv3".to_string(),
            format!(
                "SELECT count(*), sum(tot) FROM (SELECT count(*) AS n, sum(s) AS tot FROM ({per_a}))"
            ),
        ),
    ];
    for (view_sql, oracle_sql) in checks {
        assert_eq!(
            limbo_exec_rows(conn, &view_sql),
            limbo_exec_rows(conn, &oracle_sql),
            "{when}: {view_sql}"
        );
    }
}

fn reopen(tmp_db: &TempDatabase) -> TempDatabase {
    TempDatabase::builder()
        .with_db_path(&tmp_db.path)
        .with_views(true)
        .build()
}

#[turso_macros::test(views)]
fn refresh_of_a_table_is_refused(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (a INTEGER)")?;
    let err = conn.execute("REFRESH MATERIALIZED VIEW t").unwrap_err();
    assert!(
        err.to_string().contains("t is not a materialized view"),
        "{err}"
    );
    Ok(())
}

#[turso_macros::test(views)]
fn refresh_of_a_plain_view_is_refused(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute("CREATE TABLE t (a INTEGER)")?;
    conn.execute("CREATE VIEW pv AS SELECT a FROM t")?;
    let err = conn.execute("REFRESH MATERIALIZED VIEW pv").unwrap_err();
    assert!(
        err.to_string().contains("pv is not a materialized view"),
        "{err}"
    );
    Ok(())
}

#[turso_macros::test(views)]
fn refresh_of_an_unknown_name_is_refused(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let err = conn.execute("REFRESH MATERIALIZED VIEW nope").unwrap_err();
    assert!(
        err.to_string().contains("no such materialized view: nope"),
        "{err}"
    );
    Ok(())
}

#[test]
fn refresh_needs_the_views_flag() {
    let tmp_db = TempDatabase::builder().build();
    let conn = tmp_db.connect_limbo();
    let err = conn.execute("REFRESH MATERIALIZED VIEW v").unwrap_err();
    assert!(
        err.to_string()
            .contains("REFRESH MATERIALIZED VIEW is an experimental feature"),
        "{err}"
    );
}

#[turso_macros::test(views)]
fn refresh_middle_view_in_transaction_keeps_the_chain_correct(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    seed_chain(&conn)?;
    drop(conn);

    let reopened = reopen(&tmp_db);
    let conn = reopened.connect_limbo();
    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO t VALUES (3, 5)")?;
    assert_chain_matches_oracle(&conn, "after insert, before refresh");
    conn.execute("REFRESH MATERIALIZED VIEW mv2")?;
    assert_chain_matches_oracle(&conn, "after refresh, before commit");
    conn.execute("COMMIT")?;
    assert_chain_matches_oracle(&conn, "after commit");
    conn.execute("INSERT INTO t VALUES (3, 7), (4, 1)")?;
    assert_chain_matches_oracle(&conn, "after a later insert");
    Ok(())
}

#[turso_macros::test(views)]
fn refresh_middle_view_without_reopen_keeps_the_chain_correct(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    seed_chain(&conn)?;
    conn.execute("BEGIN")?;
    conn.execute("DELETE FROM t WHERE a = 2")?;
    conn.execute("REFRESH MATERIALIZED VIEW mv2")?;
    assert_chain_matches_oracle(&conn, "after refresh, before commit");
    conn.execute("COMMIT")?;
    assert_chain_matches_oracle(&conn, "after commit");
    conn.execute("UPDATE t SET b = b + 1")?;
    assert_chain_matches_oracle(&conn, "after a later update");
    Ok(())
}

#[test]
fn refresh_repairs_a_view_that_missed_a_write() -> anyhow::Result<()> {
    let tmp_db = TempDatabase::builder().with_views(true).build();
    let path = tmp_db.path.clone();
    let conn = tmp_db.connect_limbo();
    seed_chain(&conn)?;
    drop(conn);
    drop(tmp_db);

    let sqlite = rusqlite::Connection::open(&path)?;
    sqlite.pragma_update(None, "writable_schema", "ON")?;
    assert_eq!(sqlite.execute("UPDATE t SET b = b + 100", [])?, 3);
    drop(sqlite);

    let reopened = TempDatabase::builder()
        .with_db_path(&path)
        .with_views(true)
        .build();
    let conn = reopened.connect_limbo();
    let stale = limbo_exec_rows(&conn, "SELECT a, s FROM mv1 ORDER BY a");
    conn.execute("REFRESH MATERIALIZED VIEW mv1")?;
    let refreshed = limbo_exec_rows(&conn, "SELECT a, s FROM mv1 ORDER BY a");
    assert_ne!(stale, refreshed, "REFRESH kept the stale rows");
    assert_eq!(
        refreshed,
        vec![
            vec![Value::Integer(1), Value::Integer(230)],
            vec![Value::Integer(2), Value::Integer(130)],
        ]
    );
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT n, tot FROM mv2"),
        vec![vec![Value::Integer(2), Value::Integer(360)]]
    );
    conn.execute("INSERT INTO t VALUES (3, 1)")?;
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT n, tot FROM mv2"),
        vec![vec![Value::Integer(3), Value::Integer(361)]]
    );
    Ok(())
}

#[turso_macros::test(views)]
fn refresh_keeps_aggregate_row_ids(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    seed_chain(&conn)?;
    let before = limbo_exec_rows(&conn, "SELECT rowid, a, s FROM mv1 ORDER BY a");
    conn.execute("REFRESH MATERIALIZED VIEW mv1")?;
    let after = limbo_exec_rows(&conn, "SELECT rowid, a, s FROM mv1 ORDER BY a");
    assert_eq!(before, after);
    Ok(())
}
