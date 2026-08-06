//! `REFRESH MATERIALIZED VIEW` rebuilds a view by clearing its btree row by row and
//! repopulating it. The per-row deletes reach every matview defined over it as
//! retractions, so the repopulation has to hand those views the matching insertions or
//! they keep only the retractions and go empty.
//!
//! These tests pin that a refreshed view's dependents stay equal to their oracle —
//! immediately, across a reopen, and under subsequent DML — and that the `CREATE` path,
//! which shares the same repopulation instruction, stays free of that emission.

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

#[tokio::test]
async fn refresh_base_preserves_dependent_no_txn() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-dep-no-txn.db").await?;
    seed(&conn).await?;

    let mv1_before = rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?;
    let mv2_before = rows(&conn, "SELECT n, tot FROM mv2").await?;
    assert_eq!(mv2_before, mv2_oracle(&conn).await?);

    conn.execute("REFRESH MATERIALIZED VIEW mv1", ()).await?;

    assert_eq!(
        rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?,
        mv1_before
    );
    assert_eq!(
        rows(&conn, "SELECT n, tot FROM mv2").await?,
        mv2_oracle(&conn).await?,
        "dependent must still equal its oracle after the base view was refreshed"
    );
    assert_eq!(rows(&conn, "SELECT n, tot FROM mv2").await?, mv2_before);

    Ok(())
}

#[tokio::test]
async fn refresh_base_preserves_dependent_in_txn() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-dep-in-txn.db").await?;
    seed(&conn).await?;

    let mv1_before = rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?;
    let mv2_before = rows(&conn, "SELECT n, tot FROM mv2").await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW mv1", ()).await?;
    // Read the dependent while the transaction is still open. The refreshed view reads
    // its own rebuild here, and everything defined over it has to as well.
    assert_eq!(
        rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?,
        mv1_before
    );
    assert_eq!(
        rows(&conn, "SELECT n, tot FROM mv2").await?,
        mv2_before,
        "dependent must be readable at its refreshed value before COMMIT"
    );
    conn.execute("COMMIT", ()).await?;

    assert_eq!(
        rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?,
        mv1_before
    );
    assert_eq!(
        rows(&conn, "SELECT n, tot FROM mv2").await?,
        mv2_oracle(&conn).await?
    );
    assert_eq!(rows(&conn, "SELECT n, tot FROM mv2").await?, mv2_before);

    Ok(())
}

#[tokio::test]
async fn refresh_base_dependent_survives_reopen() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-dep-reopen.db").await?;
    seed(&conn).await?;

    let mv2_before = rows(&conn, "SELECT n, tot FROM mv2").await?;
    conn.execute("REFRESH MATERIALIZED VIEW mv1", ()).await?;
    drop(conn);

    let conn = file_conn(&dir, "refresh-dep-reopen.db").await?;
    assert_eq!(
        rows(&conn, "SELECT n, tot FROM mv2").await?,
        mv2_oracle(&conn).await?
    );
    assert_eq!(rows(&conn, "SELECT n, tot FROM mv2").await?, mv2_before);

    Ok(())
}

/// Contents restored but DBSP state desynchronized would show up only on the next
/// change, so drive one through after the refresh.
#[tokio::test]
async fn refresh_then_dml_tracks_oracle() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-dep-dml.db").await?;
    seed(&conn).await?;

    conn.execute("REFRESH MATERIALIZED VIEW mv1", ()).await?;
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

/// The refreshed view's insertions have to reach its direct dependents through the
/// ordinary delta path, which is what carries them on to *their* dependents.
#[tokio::test]
async fn refresh_base_of_three_deep_chain_tracks_oracle() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-dep-chain.db").await?;

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
        "CREATE MATERIALIZED VIEW mv2 AS SELECT a, s FROM mv1 WHERE s > 0",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv3 AS SELECT count(*) AS n, sum(s) AS tot FROM mv2",
        (),
    )
    .await?;

    let mv2_oracle_sql = "SELECT a, x FROM (SELECT a, sum(b) AS x FROM t GROUP BY a) \
                          WHERE x > 0 ORDER BY a";
    let mv3_oracle_sql = "SELECT count(*), sum(x) FROM \
                          (SELECT a, sum(b) AS x FROM t GROUP BY a) WHERE x > 0";

    // Refresh inside a transaction and read every level before COMMIT.
    conn.execute("BEGIN", ()).await?;
    conn.execute("REFRESH MATERIALIZED VIEW mv1", ()).await?;
    assert_eq!(
        rows(&conn, "SELECT a, s FROM mv2 ORDER BY a").await?,
        rows(&conn, mv2_oracle_sql).await?,
        "direct dependent must be readable at its refreshed value before COMMIT"
    );
    assert_eq!(
        rows(&conn, "SELECT n, tot FROM mv3").await?,
        rows(&conn, mv3_oracle_sql).await?,
        "transitive dependent must be readable at its refreshed value before COMMIT"
    );
    conn.execute("COMMIT", ()).await?;

    assert_eq!(
        rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?,
        mv1_oracle(&conn).await?
    );
    assert_eq!(
        rows(&conn, "SELECT a, s FROM mv2 ORDER BY a").await?,
        rows(&conn, mv2_oracle_sql).await?
    );
    assert_eq!(
        rows(&conn, "SELECT n, tot FROM mv3").await?,
        rows(&conn, mv3_oracle_sql).await?
    );

    // And the whole chain still maintains itself afterwards.
    conn.execute("INSERT INTO t VALUES (3, 7)", ()).await?;
    assert_eq!(
        rows(&conn, "SELECT a, s FROM mv1 ORDER BY a").await?,
        mv1_oracle(&conn).await?
    );
    assert_eq!(
        rows(&conn, "SELECT a, s FROM mv2 ORDER BY a").await?,
        rows(&conn, mv2_oracle_sql).await?
    );
    assert_eq!(
        rows(&conn, "SELECT n, tot FROM mv3").await?,
        rows(&conn, mv3_oracle_sql).await?
    );

    Ok(())
}

/// `CREATE MATERIALIZED VIEW` shares the repopulation instruction with `REFRESH`, but
/// nothing retracted the view's contents beforehand — a delta emitted there would be
/// counted on top of what the dependent's own population already read.
#[tokio::test]
async fn create_over_populated_base_emits_no_spurious_deltas() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let conn = file_conn(&dir, "refresh-dep-create.db").await?;
    seed(&conn).await?;

    // `mv3` is created over a base that already holds rows, and over a matview that
    // already holds rows. Both populations run while a dependency edge exists.
    conn.execute(
        "CREATE MATERIALIZED VIEW mv3 AS SELECT count(*) AS n FROM t",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv4 AS SELECT sum(s) AS tot FROM mv1",
        (),
    )
    .await?;

    assert_eq!(rows(&conn, "SELECT n FROM mv3").await?, vec!["3"]);
    assert_eq!(rows(&conn, "SELECT tot FROM mv4").await?, vec!["60"]);

    conn.execute("INSERT INTO t VALUES (3, 7)", ()).await?;

    assert_eq!(rows(&conn, "SELECT n FROM mv3").await?, vec!["4"]);
    assert_eq!(rows(&conn, "SELECT tot FROM mv4").await?, vec!["67"]);
    assert_eq!(
        rows(&conn, "SELECT n, tot FROM mv2").await?,
        mv2_oracle(&conn).await?
    );

    Ok(())
}

/// A leaf view — nothing defined over it — must still refresh.
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
