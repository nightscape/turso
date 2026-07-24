//! Test for IVM DBSP chain break after DROP+CREATE of upstream matviews.
//!
//! When an upstream matview is DROPped and re-CREATEd, downstream matviews
//! that were auto-loaded from `sqlite_master` (not re-created in this session)
//! must remain connected in the DBSP dependency graph. Otherwise CDC events
//! stop cascading and downstream matviews become stale.

use tempfile::TempDir;

#[tokio::test]
async fn test_drop_create_upstream_breaks_downstream_chain() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("dbsp-chain-break-test.db");
    let db_path = db_path.to_str().unwrap();

    // Session 1: Create tables + 2-level matview chain
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute("CREATE TABLE t1 (id INTEGER PRIMARY KEY, val TEXT)", ())
            .await?;
        conn.execute(
            "CREATE TABLE t2 (id INTEGER PRIMARY KEY, ref_id INTEGER, name TEXT)",
            (),
        )
        .await?;

        conn.execute("INSERT INTO t1 VALUES (1, 'a'), (2, 'b')", ())
            .await?;
        conn.execute("INSERT INTO t2 VALUES (10, 1, 'x'), (20, 2, 'y')", ())
            .await?;

        conn.execute(
            "CREATE MATERIALIZED VIEW mv_a AS SELECT id, val FROM t1",
            (),
        )
        .await?;
        conn.execute(
            "CREATE MATERIALIZED VIEW mv_b AS \
             SELECT a.id, a.val, t2.name FROM mv_a a JOIN t2 ON t2.ref_id = a.id",
            (),
        )
        .await?;

        let mut rows = conn.query("SELECT COUNT(*) FROM mv_b", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(count, 2, "Session 1: mv_b should have 2 rows");

        drop(conn);
        drop(db);
    }

    // Session 2: DROP+CREATE mv_a, leave mv_b auto-loaded
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        // Verify matviews loaded correctly from sqlite_master
        let mut rows = conn.query("SELECT COUNT(*) FROM mv_b", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(count, 2, "Session 2 pre-check: mv_b should have 2 rows");

        // DROP+CREATE mv_a (simulates app schema migration on restart)
        conn.execute("DROP VIEW IF EXISTS mv_a", ()).await?;
        conn.execute(
            "CREATE MATERIALIZED VIEW mv_a AS SELECT id, val FROM t1",
            (),
        )
        .await?;

        // Insert new data into base tables
        conn.execute("INSERT INTO t1 VALUES (3, 'c')", ()).await?;
        conn.execute("INSERT INTO t2 VALUES (30, 3, 'z')", ())
            .await?;

        // mv_a should reflect new row
        let mut rows = conn.query("SELECT COUNT(*) FROM mv_a", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(count, 3, "mv_a should have 3 rows after insert");

        // mv_b should also reflect new row — the DBSP chain must still work
        let mut rows = conn.query("SELECT COUNT(*) FROM mv_b", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(
            count, 3,
            "mv_b should have 3 rows — DBSP chain must propagate through recreated mv_a"
        );

        drop(conn);
        drop(db);
    }

    Ok(())
}

#[tokio::test]
async fn test_drop_create_upstream_3_level_chain() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("dbsp-chain-break-3level-test.db");
    let db_path = db_path.to_str().unwrap();

    // Session 1: t1 → mv_a → mv_b → mv_c
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute("CREATE TABLE t1 (id INTEGER PRIMARY KEY, val TEXT)", ())
            .await?;
        conn.execute("INSERT INTO t1 VALUES (1, 'a')", ()).await?;

        conn.execute(
            "CREATE MATERIALIZED VIEW mv_a AS SELECT id, val FROM t1",
            (),
        )
        .await?;
        conn.execute(
            "CREATE MATERIALIZED VIEW mv_b AS SELECT id, val FROM mv_a",
            (),
        )
        .await?;
        conn.execute(
            "CREATE MATERIALIZED VIEW mv_c AS SELECT id, val FROM mv_b",
            (),
        )
        .await?;

        let mut rows = conn.query("SELECT COUNT(*) FROM mv_c", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(count, 1, "Session 1: mv_c should have 1 row");

        drop(conn);
        drop(db);
    }

    // Session 2: DROP+CREATE mv_a and mv_b, leave mv_c
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute("DROP VIEW IF EXISTS mv_b", ()).await?;
        conn.execute("DROP VIEW IF EXISTS mv_a", ()).await?;

        conn.execute(
            "CREATE MATERIALIZED VIEW mv_a AS SELECT id, val FROM t1",
            (),
        )
        .await?;
        conn.execute(
            "CREATE MATERIALIZED VIEW mv_b AS SELECT id, val FROM mv_a",
            (),
        )
        .await?;

        // Insert new data
        conn.execute("INSERT INTO t1 VALUES (2, 'b')", ()).await?;

        let mut rows = conn.query("SELECT COUNT(*) FROM mv_a", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(count, 2, "mv_a should have 2 rows");

        let mut rows = conn.query("SELECT COUNT(*) FROM mv_b", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(count, 2, "mv_b should have 2 rows");

        // mv_c should propagate through the chain
        let mut rows = conn.query("SELECT COUNT(*) FROM mv_c", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(
            count, 2,
            "mv_c should have 2 rows — chain must propagate through recreated mv_a and mv_b"
        );

        drop(conn);
        drop(db);
    }

    Ok(())
}
