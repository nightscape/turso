//! `REFRESH MATERIALIZED VIEW` rebuilds a view by clearing its btree row by row and
//! repopulating it. The per-row deletes reach every matview defined over it as
//! retractions; the repopulation emits no matching insertions, so the dependent is
//! silently emptied and stays empty.
//!
//! Until a symmetric rebuild exists, refreshing a view that has dependents is refused
//! at translate time. These tests pin the refusal and pin that the refused statement
//! leaves both views — contents and DBSP state — exactly as they were.

use tempfile::TempDir;
use turso::{Builder, Connection};

async fn file_conn(dir: &TempDir, name: &str) -> anyhow::Result<Connection> {
    let path = dir.path().join(name);
    let db = Builder::new_local(path.to_str().unwrap())
        .experimental_materialized_views(true)
        .build()
        .await?;
    Ok(db.connect()?)
}

/// `t` -> `mv1` (aggregate) -> `mv2` (aggregate over `mv1`).
async fn seed(conn: &Connection) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE t (a INTEGER, b INTEGER)", ())
        .await?;
    conn.execute("INSERT INTO t VALUES (1, 10), (1, 20), (2, 30)", ())
        .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv1 AS SELECT a, sum(b) AS s FROM t GROUP BY a",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv2 AS SELECT count(*) AS n, sum(s) AS tot FROM mv1",
        (),
    )
    .await?;
    Ok(())
}

async fn rows(conn: &Connection, sql: &str) -> anyhow::Result<Vec<String>> {
    let mut result = Vec::new();
    let mut r = conn.query(sql, ()).await?;
    while let Some(row) = r.next().await? {
        let mut cells = Vec::new();
        let mut i = 0;
        while let Ok(v) = row.get_value(i) {
            cells.push(match v {
                turso::Value::Integer(i) => i.to_string(),
                turso::Value::Real(f) => format!("{f}"),
                turso::Value::Text(t) => t,
                turso::Value::Null => "NULL".to_string(),
                other => format!("{other:?}"),
            });
            i += 1;
        }
        result.push(cells.join("|"));
    }
    Ok(result)
}

/// `mv2`'s value recomputed from the base table.
async fn mv2_oracle(conn: &Connection) -> anyhow::Result<Vec<String>> {
    rows(
        conn,
        "SELECT count(*), sum(x) FROM (SELECT a, sum(b) AS x FROM t GROUP BY a)",
    )
    .await
}

/// `mv1`'s value recomputed from the base table.
async fn mv1_oracle(conn: &Connection) -> anyhow::Result<Vec<String>> {
    rows(conn, "SELECT a, sum(b) FROM t GROUP BY a ORDER BY a").await
}

fn assert_names_dependent(err: &turso::Error) {
    let msg = err.to_string();
    assert!(
        msg.contains("mv2"),
        "refusal must name the dependent view, got: {msg}"
    );
    assert!(
        msg.contains("mv1"),
        "refusal must name the refreshed view, got: {msg}"
    );
}

#[tokio::test]
async fn refresh_with_dependent_refused_no_txn() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-dep-no-txn.db").await?;
    seed(&conn).await?;

    let mv1_before = rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?;
    let mv2_before = rows(&conn, "SELECT n, tot FROM mv2").await?;
    assert_eq!(mv2_before, mv2_oracle(&conn).await?);

    let err = conn
        .execute("REFRESH MATERIALIZED VIEW mv1", ())
        .await
        .expect_err("REFRESH of a view with a dependent matview must be refused");
    assert_names_dependent(&err);

    assert_eq!(
        rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?,
        mv1_before
    );
    assert_eq!(rows(&conn, "SELECT n, tot FROM mv2").await?, mv2_before);

    Ok(())
}

#[tokio::test]
async fn refresh_with_dependent_refused_in_txn() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-dep-in-txn.db").await?;
    seed(&conn).await?;

    let mv1_before = rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?;
    let mv2_before = rows(&conn, "SELECT n, tot FROM mv2").await?;

    conn.execute("BEGIN", ()).await?;
    let err = conn
        .execute("REFRESH MATERIALIZED VIEW mv1", ())
        .await
        .expect_err("REFRESH of a view with a dependent matview must be refused");
    assert_names_dependent(&err);
    conn.execute("COMMIT", ()).await?;

    assert_eq!(
        rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?,
        mv1_before
    );
    assert_eq!(rows(&conn, "SELECT n, tot FROM mv2").await?, mv2_before);

    Ok(())
}

#[tokio::test]
async fn refresh_with_dependent_refused_survives_reopen() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-dep-reopen.db").await?;
    seed(&conn).await?;

    let mv2_before = rows(&conn, "SELECT n, tot FROM mv2").await?;
    conn.execute("REFRESH MATERIALIZED VIEW mv1", ())
        .await
        .expect_err("REFRESH of a view with a dependent matview must be refused");
    drop(conn);

    let conn = file_conn(&dir, "refresh-dep-reopen.db").await?;
    assert_eq!(rows(&conn, "SELECT n, tot FROM mv2").await?, mv2_before);
    assert_eq!(
        rows(&conn, "SELECT n, tot FROM mv2").await?,
        mv2_oracle(&conn).await?
    );

    Ok(())
}

#[tokio::test]
async fn refused_refresh_leaves_dml_tracking_intact() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-dep-dml.db").await?;
    seed(&conn).await?;

    conn.execute("REFRESH MATERIALIZED VIEW mv1", ())
        .await
        .expect_err("REFRESH of a view with a dependent matview must be refused");

    conn.execute("INSERT INTO t VALUES (2, 5), (3, 7)", ())
        .await?;

    assert_eq!(
        rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?,
        mv1_oracle(&conn).await?
    );
    assert_eq!(
        rows(&conn, "SELECT n, tot FROM mv2").await?,
        mv2_oracle(&conn).await?
    );

    Ok(())
}

/// A leaf view — nothing defined over it — must still refresh. The refusal is scoped to
/// the destructive clear-and-rebuild of a view that others read.
#[tokio::test]
async fn refresh_leaf_view_still_allowed() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-dep-leaf.db").await?;
    seed(&conn).await?;

    let mv2_before = rows(&conn, "SELECT n, tot FROM mv2").await?;
    conn.execute("REFRESH MATERIALIZED VIEW mv2", ()).await?;
    assert_eq!(rows(&conn, "SELECT n, tot FROM mv2").await?, mv2_before);

    Ok(())
}
