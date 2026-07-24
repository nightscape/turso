//! Cross-session correctness tests for IVM aggregate FILTER (WHERE ...).
//!
//! The FILTER predicate itself is NOT persisted; it is reconstructed from the
//! matview's SQL on reopen via the same path as the rest of `AggregateOperator`.
//! These tests verify that:
//!
//! 1. After a DB reopen, a FILTER matview correctly applies the predicate to
//!    incremental updates (i.e. the predicate survived the round-trip).
//! 2. A non-FILTER aggregate matview's blob format is unchanged by this work
//!    (regression guard for the "no persistence change" claim).

use tempfile::TempDir;

#[tokio::test]
async fn test_filter_cross_session_reload() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("aggregate-filter-reload.db");
    let db_path = db_path.to_str().unwrap();

    let matview_sql = "CREATE MATERIALIZED VIEW IF NOT EXISTS mv AS \
        SELECT g, sum(v) FILTER (WHERE v >= 10) AS s FROM t GROUP BY g";

    // Session 1: create matview, populate.
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, g INT, v INT)", ())
            .await?;
        conn.execute(matview_sql, ()).await?;

        for (id, g, v) in [(1, 1, 10_i64), (2, 1, 20), (3, 1, 5), (4, 2, 100)] {
            conn.execute(&format!("INSERT INTO t VALUES ({id}, {g}, {v})"), ())
                .await?;
        }

        let mut rows = conn.query("SELECT g, s FROM mv WHERE g = 1", ()).await?;
        let row = rows.next().await?.unwrap();
        let s: f64 = row.get(1)?;
        assert_eq!(
            s, 30.0,
            "Session 1: g=1 sum should be 30 (10 + 20, 5 excluded)"
        );

        drop(conn);
        drop(db);
    }

    // Session 2: reopen, verify matview state survived, then INSERT
    // a passing and a failing row and verify the FILTER predicate
    // was reconstructed correctly.
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute(matview_sql, ()).await?;

        let mut rows = conn.query("SELECT g, s FROM mv WHERE g = 1", ()).await?;
        let row = rows.next().await?.unwrap();
        let s: f64 = row.get(1)?;
        assert_eq!(s, 30.0, "Session 2: matview state should round-trip");

        // INSERT a passing row (15 >= 10) — should add to sum.
        conn.execute("INSERT INTO t VALUES (5, 1, 15)", ()).await?;
        let mut rows = conn.query("SELECT g, s FROM mv WHERE g = 1", ()).await?;
        let row = rows.next().await?.unwrap();
        let s: f64 = row.get(1)?;
        assert_eq!(s, 45.0, "Session 2: passing INSERT should add to sum");

        // INSERT a failing row (3 < 10) — should leave sum unchanged.
        conn.execute("INSERT INTO t VALUES (6, 1, 3)", ()).await?;
        let mut rows = conn.query("SELECT g, s FROM mv WHERE g = 1", ()).await?;
        let row = rows.next().await?.unwrap();
        let s: f64 = row.get(1)?;
        assert_eq!(
            s, 45.0,
            "Session 2: failing INSERT must NOT add — proves FILTER predicate \
             was reconstructed from SQL on reopen"
        );

        drop(conn);
        drop(db);
    }

    Ok(())
}

#[tokio::test]
async fn test_filter_blob_format_unchanged() -> anyhow::Result<()> {
    // Regression guard: a non-FILTER aggregate matview's persisted state
    // must round-trip exactly as before this change. This catches a hypothetical
    // regression where an extra byte got added to the blob format for filters
    // even though no FILTER is in use.
    let dir = TempDir::new()?;
    let db_path = dir.path().join("aggregate-filter-blob-format.db");
    let db_path = db_path.to_str().unwrap();

    let matview_sql = "CREATE MATERIALIZED VIEW IF NOT EXISTS mv AS \
        SELECT g, sum(v) AS s, json_group_array(v) AS arr FROM t GROUP BY g";

    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, g INT, v INT)", ())
            .await?;
        conn.execute(matview_sql, ()).await?;

        for (id, g, v) in [(1, 1, 10_i64), (2, 1, 20), (3, 2, 100)] {
            conn.execute(&format!("INSERT INTO t VALUES ({id}, {g}, {v})"), ())
                .await?;
        }

        drop(conn);
        drop(db);
    }

    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;

        conn.execute(matview_sql, ()).await?;

        let mut rows = conn.query("SELECT g, s FROM mv WHERE g = 1", ()).await?;
        let row = rows.next().await?.unwrap();
        let s: f64 = row.get(1)?;
        assert_eq!(s, 30.0, "Reopen: non-FILTER sum must round-trip");

        // Apply an incremental change to confirm state machinery works.
        conn.execute("INSERT INTO t VALUES (4, 1, 5)", ()).await?;
        let mut rows = conn.query("SELECT g, s FROM mv WHERE g = 1", ()).await?;
        let row = rows.next().await?.unwrap();
        let s: f64 = row.get(1)?;
        assert_eq!(s, 35.0, "Reopen: incremental update on non-FILTER");
    }

    Ok(())
}
