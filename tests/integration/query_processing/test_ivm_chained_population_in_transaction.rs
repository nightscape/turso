//! Populating a matview whose source is another matview, inside an explicit
//! transaction that already staged DML on the base table.
//!
//! The rebuild reads its upstream on the connection that issued it. If that
//! read saw the upstream's uncommitted delta, the rebuilt rows would already
//! contain it — and the COMMIT-time cascade, which delivers the upstream's
//! output delta afterwards, would apply it a second time. A rebuild therefore
//! reads upstream matviews at their committed state.
//!
//! sum/count expose the double application; DISTINCT would hide it.

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

/// `t` = {(1,10), (1,5), (2,20)} with `mv1 = a, sum(b) GROUP BY a` = {1:15, 2:20}.
async fn seed(conn: &Connection) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE t (a INTEGER, b INTEGER)", ())
        .await?;
    conn.execute("INSERT INTO t VALUES (1, 10), (1, 5), (2, 20)", ())
        .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv1 AS SELECT a, sum(b) AS s FROM t GROUP BY a",
        (),
    )
    .await?;
    Ok(())
}

const CREATE_MV2: &str =
    "CREATE MATERIALIZED VIEW mv2 AS SELECT count(*) AS c, sum(s) AS ts FROM mv1";

async fn pair(conn: &Connection, sql: &str) -> anyhow::Result<(f64, f64)> {
    let mut rows = conn.query(sql, ()).await?;
    let row = rows.next().await?.expect("one row");
    let num = |i: usize| -> anyhow::Result<f64> {
        Ok(row
            .get::<f64>(i)
            .or_else(|_| row.get::<i64>(i).map(|v| v as f64))?)
    };
    Ok((num(0)?, num(1)?))
}

async fn mv2(conn: &Connection) -> anyhow::Result<(f64, f64)> {
    pair(conn, "SELECT c, ts FROM mv2").await
}

/// The view must report the same thing a fresh process reads back from disk.
async fn assert_mv2_persisted(
    dir: &TempDir,
    name: &str,
    conn: Connection,
    expected: (f64, f64),
) -> anyhow::Result<()> {
    assert_eq!(mv2(&conn).await?, expected, "in-process");
    drop(conn);
    let reopened = file_conn(dir, name).await?;
    assert_eq!(mv2(&reopened).await?, expected, "after reopen");
    Ok(())
}

#[tokio::test]
async fn test_create_dependent_matview_after_insert_in_transaction() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "chained-create-insert.db").await?;
    seed(&conn).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("INSERT INTO t VALUES (3, 7)", ()).await?;
    conn.execute(CREATE_MV2, ()).await?;
    assert_eq!(
        mv2(&conn).await?,
        (3.0, 42.0),
        "read inside the transaction"
    );
    conn.execute("COMMIT", ()).await?;

    assert_mv2_persisted(&dir, "chained-create-insert.db", conn, (3.0, 42.0)).await
}

#[tokio::test]
async fn test_create_dependent_matview_after_update_in_transaction() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "chained-create-update.db").await?;
    seed(&conn).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("UPDATE t SET b = 100 WHERE a = 2", ()).await?;
    conn.execute(CREATE_MV2, ()).await?;
    conn.execute("COMMIT", ()).await?;

    // mv1 = {1:15, 2:100}
    assert_mv2_persisted(&dir, "chained-create-update.db", conn, (2.0, 115.0)).await
}

#[tokio::test]
async fn test_create_dependent_matview_after_delete_in_transaction() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "chained-create-delete.db").await?;
    seed(&conn).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("DELETE FROM t WHERE a = 2", ()).await?;
    conn.execute(CREATE_MV2, ()).await?;
    conn.execute("COMMIT", ()).await?;

    // mv1 = {1:15}
    assert_mv2_persisted(&dir, "chained-create-delete.db", conn, (1.0, 15.0)).await
}

#[tokio::test]
async fn test_refresh_dependent_matview_after_insert_in_transaction() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "chained-refresh-insert.db").await?;
    seed(&conn).await?;
    conn.execute(CREATE_MV2, ()).await?;
    assert_eq!(mv2(&conn).await?, (2.0, 35.0));

    conn.execute("BEGIN", ()).await?;
    conn.execute("INSERT INTO t VALUES (3, 7)", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW mv2", ()).await?;
    assert_eq!(
        mv2(&conn).await?,
        (3.0, 42.0),
        "read inside the transaction"
    );
    conn.execute("COMMIT", ()).await?;

    assert_mv2_persisted(&dir, "chained-refresh-insert.db", conn, (3.0, 42.0)).await
}

#[tokio::test]
async fn test_refresh_dependent_matview_after_delete_in_transaction() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "chained-refresh-delete.db").await?;
    seed(&conn).await?;
    conn.execute(CREATE_MV2, ()).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("DELETE FROM t WHERE a = 1", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW mv2", ()).await?;
    conn.execute("COMMIT", ()).await?;

    assert_mv2_persisted(&dir, "chained-refresh-delete.db", conn, (1.0, 20.0)).await
}

/// The legitimate cascade: the dependent exists before the DML, so the
/// COMMIT-time delivery is the only path by which it learns of the insert. It
/// must still be applied — exactly once.
#[tokio::test]
async fn test_create_dependent_matview_before_dml_applies_cascade_once() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "chained-create-before-dml.db").await?;
    seed(&conn).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute(CREATE_MV2, ()).await?;
    conn.execute("INSERT INTO t VALUES (3, 7)", ()).await?;
    assert_eq!(
        mv2(&conn).await?,
        (3.0, 42.0),
        "read inside the transaction"
    );
    conn.execute("COMMIT", ()).await?;

    assert_mv2_persisted(&dir, "chained-create-before-dml.db", conn, (3.0, 42.0)).await
}

/// DML after the populating transaction closed goes through the ordinary
/// cascade and must land once, on top of a correctly populated view.
#[tokio::test]
async fn test_dml_after_populating_transaction_applies_once() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "chained-post-commit-dml.db").await?;
    seed(&conn).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("INSERT INTO t VALUES (3, 7)", ()).await?;
    conn.execute(CREATE_MV2, ()).await?;
    conn.execute("COMMIT", ()).await?;

    conn.execute("INSERT INTO t VALUES (4, 1)", ()).await?;

    assert_mv2_persisted(&dir, "chained-post-commit-dml.db", conn, (4.0, 43.0)).await
}

/// Rolling the populating transaction back leaves the base untouched, and a
/// later insert must be counted against the original state only.
#[tokio::test]
async fn test_rollback_of_populating_transaction() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "chained-rollback.db").await?;
    seed(&conn).await?;
    conn.execute(CREATE_MV2, ()).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("INSERT INTO t VALUES (3, 7)", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW mv2", ()).await?;
    conn.execute("ROLLBACK", ()).await?;

    assert_eq!(mv2(&conn).await?, (2.0, 35.0));
    conn.execute("INSERT INTO t VALUES (4, 1)", ()).await?;

    assert_mv2_persisted(&dir, "chained-rollback.db", conn, (3.0, 36.0)).await
}

/// Three levels deep: the grandchild is populated in the transaction while both
/// of its ancestors carry uncommitted state.
#[tokio::test]
async fn test_create_three_deep_matview_after_insert_in_transaction() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "chained-three-deep.db").await?;
    seed(&conn).await?;
    conn.execute(CREATE_MV2, ()).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("INSERT INTO t VALUES (3, 7)", ()).await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv3 AS SELECT count(*) AS n, sum(ts) AS tt FROM mv2",
        (),
    )
    .await?;
    conn.execute("COMMIT", ()).await?;

    assert_eq!(pair(&conn, "SELECT n, tt FROM mv3").await?, (1.0, 42.0));
    assert_eq!(mv2(&conn).await?, (3.0, 42.0));

    drop(conn);
    let conn = file_conn(&dir, "chained-three-deep.db").await?;
    assert_eq!(
        pair(&conn, "SELECT n, tt FROM mv3").await?,
        (1.0, 42.0),
        "after reopen"
    );
    assert_eq!(mv2(&conn).await?, (3.0, 42.0), "after reopen");
    Ok(())
}
