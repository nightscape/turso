//! `REFRESH MATERIALIZED VIEW` rebuilds a view from its sources by reading them
//! on the connection that issued it, so inside an explicit transaction it reads
//! that transaction's uncommitted rows. Those rows' deltas are also queued for
//! the view and applied again at COMMIT, counting every one of them twice.
//!
//! DISTINCT views hide it (applying the same row twice is idempotent); sum and
//! count(*) do not.

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

async fn seed(conn: &Connection, view: &str) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE t (a INTEGER, b INTEGER)", ())
        .await?;
    conn.execute("INSERT INTO t VALUES (1, 10)", ()).await?;
    conn.execute(
        &format!("CREATE MATERIALIZED VIEW v AS SELECT a, {view} FROM t GROUP BY a"),
        (),
    )
    .await?;
    Ok(())
}

async fn scalar(conn: &Connection, sql: &str) -> anyhow::Result<f64> {
    let mut rows = conn.query(sql, ()).await?;
    let row = rows.next().await?.unwrap();
    Ok(row
        .get::<f64>(0)
        .or_else(|_| row.get::<i64>(0).map(|i| i as f64))?)
}

#[tokio::test]
async fn test_refresh_after_insert_in_transaction_sum() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-txn-sum.db").await?;
    seed(&conn, "sum(b) AS s").await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("INSERT INTO t VALUES (1, 5)", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW v", ()).await?;
    conn.execute("COMMIT", ()).await?;

    assert_eq!(scalar(&conn, "SELECT s FROM v WHERE a = 1").await?, 15.0);

    // And it must not have persisted a different answer than it reports.
    drop(conn);
    let conn = file_conn(&dir, "refresh-txn-sum.db").await?;
    assert_eq!(scalar(&conn, "SELECT s FROM v WHERE a = 1").await?, 15.0);

    Ok(())
}

#[tokio::test]
async fn test_refresh_after_insert_in_transaction_count() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-txn-count.db").await?;
    seed(&conn, "count(*) AS c").await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("INSERT INTO t VALUES (1, 5)", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW v", ()).await?;
    conn.execute("COMMIT", ()).await?;

    assert_eq!(scalar(&conn, "SELECT c FROM v WHERE a = 1").await?, 2.0);

    drop(conn);
    let conn = file_conn(&dir, "refresh-txn-count.db").await?;
    assert_eq!(scalar(&conn, "SELECT c FROM v WHERE a = 1").await?, 2.0);

    Ok(())
}

/// The rebuild runs before the row exists, so the row's delta is the only thing
/// that can carry it into the view — it must still be applied.
#[tokio::test]
async fn test_refresh_before_insert_in_transaction() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-txn-swapped.db").await?;
    seed(&conn, "sum(b) AS s").await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW v", ()).await?;
    conn.execute("INSERT INTO t VALUES (1, 5)", ()).await?;
    conn.execute("COMMIT", ()).await?;

    assert_eq!(scalar(&conn, "SELECT s FROM v WHERE a = 1").await?, 15.0);
    Ok(())
}

#[tokio::test]
async fn test_refresh_in_rolled_back_transaction() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-txn-rollback.db").await?;
    seed(&conn, "sum(b) AS s").await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("INSERT INTO t VALUES (1, 5)", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW v", ()).await?;
    conn.execute("ROLLBACK", ()).await?;

    assert_eq!(scalar(&conn, "SELECT s FROM v WHERE a = 1").await?, 10.0);
    Ok(())
}

#[tokio::test]
async fn test_refresh_after_insert_without_transaction() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-no-txn.db").await?;
    seed(&conn, "sum(b) AS s").await?;

    conn.execute("INSERT INTO t VALUES (1, 5)", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW v", ()).await?;

    assert_eq!(scalar(&conn, "SELECT s FROM v WHERE a = 1").await?, 15.0);
    Ok(())
}

/// A rebuild absorbs the deltas staged before it, but `ROLLBACK TO` undoes the
/// rebuild — so it has to put those deltas back. Losing them drops the row from
/// the view for good, and every later change compounds on the deficit.
#[tokio::test]
async fn test_refresh_rolled_back_to_savepoint_keeps_staged_deltas() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-savepoint.db").await?;
    seed(&conn, "sum(b) AS s").await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("INSERT INTO t VALUES (1, 5)", ()).await?;
    conn.execute("SAVEPOINT sp", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW v", ()).await?;
    conn.execute("ROLLBACK TO sp", ()).await?;
    conn.execute("COMMIT", ()).await?;

    assert_eq!(scalar(&conn, "SELECT s FROM v WHERE a = 1").await?, 15.0);

    drop(conn);
    let conn = file_conn(&dir, "refresh-savepoint.db").await?;
    assert_eq!(scalar(&conn, "SELECT s FROM v WHERE a = 1").await?, 15.0);

    // A lost delta is permanent: later changes accumulate on the deficit.
    conn.execute("INSERT INTO t VALUES (1, 1)", ()).await?;
    assert_eq!(scalar(&conn, "SELECT s FROM v WHERE a = 1").await?, 16.0);

    Ok(())
}

/// RELEASE keeps the savepoint's work, so the rebuild that follows it absorbs
/// the released delta exactly as it would without any savepoint.
#[tokio::test]
async fn test_refresh_after_released_savepoint() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-release.db").await?;
    seed(&conn, "sum(b) AS s").await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("SAVEPOINT sp", ()).await?;
    conn.execute("INSERT INTO t VALUES (1, 5)", ()).await?;
    conn.execute("RELEASE sp", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW v", ()).await?;
    conn.execute("COMMIT", ()).await?;

    assert_eq!(scalar(&conn, "SELECT s FROM v WHERE a = 1").await?, 15.0);
    Ok(())
}

/// Clearing the refreshed view's staged deltas must not touch a sibling view
/// fed by the same table.
#[tokio::test]
async fn test_refresh_leaves_sibling_view_deltas_alone() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-txn-sibling.db").await?;
    seed(&conn, "sum(b) AS s").await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW w AS SELECT a, sum(b) AS s FROM t GROUP BY a",
        (),
    )
    .await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("INSERT INTO t VALUES (1, 5)", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW v", ()).await?;
    conn.execute("COMMIT", ()).await?;

    assert_eq!(scalar(&conn, "SELECT s FROM v WHERE a = 1").await?, 15.0);
    assert_eq!(scalar(&conn, "SELECT s FROM w WHERE a = 1").await?, 15.0);
    Ok(())
}
