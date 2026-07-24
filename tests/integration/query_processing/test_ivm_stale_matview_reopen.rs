//! Test that stale materialized views (referencing dropped columns/tables)
//! are gracefully skipped on DB reopen instead of causing a fatal error.

use tempfile::TempDir;

#[tokio::test]
async fn test_stale_matview_dropped_column_reopen() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("stale-matview-reopen-test.db");
    let db_path = db_path.to_str().unwrap();

    // Session 1: Create table with columns A, B and a matview referencing both
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
            (),
        )
        .await?;
        conn.execute("INSERT INTO t VALUES (1, 'hello', 'world')", ())
            .await?;

        conn.execute(
            "CREATE MATERIALIZED VIEW mv_ab AS SELECT id, a, b FROM t",
            (),
        )
        .await?;

        let mut rows = conn.query("SELECT COUNT(*) FROM mv_ab", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(count, 1);

        drop(conn);
        drop(db);
    }

    // Session 2: Recreate the table WITHOUT column b, then reopen.
    // The matview references column b which no longer exists.
    // This should NOT fail with "circular dependency" — the stale view
    // should be skipped gracefully.
    {
        // Open without matviews to alter schema freely
        let db = turso::Builder::new_local(db_path).build().await?;
        let conn = db.connect()?;

        conn.execute("DROP TABLE t", ()).await?;
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)", ())
            .await?;
        conn.execute("INSERT INTO t VALUES (1, 'hello')", ())
            .await?;

        drop(conn);
        drop(db);
    }

    // Session 3: Reopen with matviews enabled — should succeed
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        // The stale matview should be marked incompatible, not queryable as a matview
        // but the DB should open successfully. The base table should work fine.
        let mut rows = conn.query("SELECT COUNT(*) FROM t", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(count, 1);

        drop(conn);
        drop(db);
    }

    Ok(())
}

#[tokio::test]
async fn test_stale_matview_dropped_table_reopen() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("stale-matview-dropped-table-test.db");
    let db_path = db_path.to_str().unwrap();

    // Session 1: Create table and matview
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute("CREATE TABLE source (id INTEGER PRIMARY KEY, val TEXT)", ())
            .await?;
        conn.execute("INSERT INTO source VALUES (1, 'x')", ())
            .await?;

        conn.execute(
            "CREATE MATERIALIZED VIEW mv_source AS SELECT id, val FROM source",
            (),
        )
        .await?;

        drop(conn);
        drop(db);
    }

    // Session 2: Drop the source table
    {
        let db = turso::Builder::new_local(db_path).build().await?;
        let conn = db.connect()?;
        conn.execute("DROP TABLE source", ()).await?;
        drop(conn);
        drop(db);
    }

    // Session 3: Reopen with matviews — should succeed despite stale view
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        // DB opened successfully — that's the main assertion
        drop(conn);
        drop(db);
    }

    Ok(())
}

#[tokio::test]
async fn test_valid_chained_matview_still_works() -> anyhow::Result<()> {
    // Ensure the fix doesn't break valid chained matview loading
    let dir = TempDir::new()?;
    let db_path = dir.path().join("stale-matview-chain-ok-test.db");
    let db_path = db_path.to_str().unwrap();

    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", ())
            .await?;
        conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')", ())
            .await?;

        conn.execute("CREATE MATERIALIZED VIEW mv_a AS SELECT id, v FROM t", ())
            .await?;
        conn.execute(
            "CREATE MATERIALIZED VIEW mv_b AS SELECT id, v FROM mv_a WHERE id > 0",
            (),
        )
        .await?;

        drop(conn);
        drop(db);
    }

    // Reopen — chained matviews should load fine
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        let mut rows = conn.query("SELECT COUNT(*) FROM mv_b", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(count, 2);

        drop(conn);
        drop(db);
    }

    Ok(())
}
