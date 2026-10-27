//! `REFRESH MATERIALIZED VIEW` cleared the DBSP state table's btree but not the
//! automatic primary-key index that `CREATE MATERIALIZED VIEW` registers for it.
//! The surviving index entries pointed at deleted rowids, so the repopulation's
//! index-then-table seek aborted with "Index points to non-existent table row".
//!
//! Any view whose DBSP operators keep state is affected: aggregate, DISTINCT and
//! join views. Projection-only views have an empty state table, which is why the
//! bug stayed invisible.

use turso::Builder;

async fn views_conn() -> anyhow::Result<turso::Connection> {
    let db = Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await?;
    Ok(db.connect()?)
}

#[tokio::test]
async fn test_refresh_aggregate_view_keeps_state_index_consistent() -> anyhow::Result<()> {
    let conn = views_conn().await?;

    conn.execute("CREATE TABLE t (a INTEGER, b INTEGER)", ())
        .await?;
    conn.execute("INSERT INTO t VALUES (1, 10), (1, 20), (2, 30)", ())
        .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv AS SELECT a, sum(b) AS s FROM t GROUP BY a",
        (),
    )
    .await?;

    conn.execute("REFRESH MATERIALIZED VIEW mv", ()).await?;

    // The refresh must be a no-op with respect to the view's contents.
    let mut rows = conn.query("SELECT a, s FROM mv ORDER BY a", ()).await?;
    let row = rows.next().await?.unwrap();
    assert_eq!((row.get::<i64>(0)?, row.get::<f64>(1)?), (1, 30.0));
    let row = rows.next().await?.unwrap();
    assert_eq!((row.get::<i64>(0)?, row.get::<f64>(1)?), (2, 30.0));
    assert!(rows.next().await?.is_none());

    // And the DBSP state it rebuilt must still maintain the view incrementally.
    conn.execute("INSERT INTO t VALUES (1, 5)", ()).await?;
    let mut rows = conn.query("SELECT a, s FROM mv ORDER BY a", ()).await?;
    let row = rows.next().await?.unwrap();
    assert_eq!((row.get::<i64>(0)?, row.get::<f64>(1)?), (1, 35.0));
    let row = rows.next().await?.unwrap();
    assert_eq!((row.get::<i64>(0)?, row.get::<f64>(1)?), (2, 30.0));
    assert!(rows.next().await?.is_none());

    Ok(())
}

#[tokio::test]
async fn test_refresh_distinct_view_keeps_state_index_consistent() -> anyhow::Result<()> {
    let conn = views_conn().await?;

    conn.execute("CREATE TABLE t (a INTEGER)", ()).await?;
    conn.execute("INSERT INTO t VALUES (1), (1), (2)", ())
        .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv AS SELECT DISTINCT a FROM t",
        (),
    )
    .await?;

    conn.execute("REFRESH MATERIALIZED VIEW mv", ()).await?;
    conn.execute("INSERT INTO t VALUES (3)", ()).await?;

    let mut rows = conn.query("SELECT a FROM mv ORDER BY a", ()).await?;
    let mut got = Vec::new();
    while let Some(row) = rows.next().await? {
        got.push(row.get::<i64>(0)?);
    }
    assert_eq!(got, vec![1, 2, 3]);

    Ok(())
}

#[tokio::test]
async fn test_refresh_join_view_keeps_state_index_consistent() -> anyhow::Result<()> {
    let conn = views_conn().await?;

    conn.execute("CREATE TABLE t (a INTEGER, b INTEGER)", ())
        .await?;
    conn.execute("CREATE TABLE u (a INTEGER, c TEXT)", ())
        .await?;
    conn.execute("INSERT INTO t VALUES (1, 10), (2, 30)", ())
        .await?;
    conn.execute("INSERT INTO u VALUES (1, 'x'), (2, 'y')", ())
        .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv AS SELECT t.a AS a, u.c AS c FROM t JOIN u ON t.a = u.a",
        (),
    )
    .await?;

    conn.execute("REFRESH MATERIALIZED VIEW mv", ()).await?;
    conn.execute("INSERT INTO t VALUES (1, 11)", ()).await?;

    let mut rows = conn.query("SELECT a, c FROM mv ORDER BY a, c", ()).await?;
    let mut got = Vec::new();
    while let Some(row) = rows.next().await? {
        got.push((row.get::<i64>(0)?, row.get::<String>(1)?));
    }
    assert_eq!(
        got,
        vec![
            (1, "x".to_string()),
            (1, "x".to_string()),
            (2, "y".to_string()),
        ]
    );

    Ok(())
}
