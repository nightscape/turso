//! Cross-session and adversarial-corpus tests for IVM array/string aggregation.
//!
//! Sqltest coverage in `testing/runner/tests/ivm-array-aggregation.sqltest`
//! exercises the in-memory codepath. These tests round-trip through a
//! file-backed DB to lock down `AggregateState::to_value_vector` /
//! `from_value_vector` for the new multiset variants.

#[tokio::test]
async fn test_array_agg_create_matview_succeeds() -> anyhow::Result<()> {
    // Sanity: every variant the IVM compiler now accepts compiles to a matview.
    // Catches the regression where the dispatch returns
    // "Unsupported aggregate function in DBSP compiler".
    let db = turso::Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;

    conn.execute("CREATE TABLE t (g INT, v TEXT)", ()).await?;

    conn.execute(
        "CREATE MATERIALIZED VIEW mv1 AS \
         SELECT g, json_group_array(v) FROM t GROUP BY g",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv2 AS \
         SELECT g, json_group_array(DISTINCT v) FROM t GROUP BY g",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv3 AS \
         SELECT g, group_concat(v) FROM t GROUP BY g",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv4 AS \
         SELECT g, group_concat(v, '|') FROM t GROUP BY g",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv5 AS \
         SELECT g, group_concat(DISTINCT v) FROM t GROUP BY g",
        (),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn test_cross_session_array_agg_restore() -> anyhow::Result<()> {
    let db_path = "/tmp/turso-ivm-array-agg-cross-session-test.db";
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));

    let matview_sql = "CREATE MATERIALIZED VIEW IF NOT EXISTS mv AS \
                       SELECT g, json_group_array(v) AS arr FROM t GROUP BY g";

    // Session 1: create matview, populate.
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, g INT, v TEXT)", ())
            .await?;
        conn.execute(matview_sql, ()).await?;

        for i in 0..6 {
            conn.execute(
                &format!("INSERT INTO t VALUES ({i}, {}, 'v{i}')", i % 2),
                (),
            )
            .await?;
        }

        // Sanity: 2 groups, each with 3 elements.
        let mut rows = conn.query("SELECT COUNT(*) FROM mv", ()).await?;
        let row = rows.next().await?.unwrap();
        let count: i64 = row.get(0)?;
        assert_eq!(count, 2, "Session 1: expected 2 groups");

        drop(conn);
        drop(db);
    }

    // Session 2: reopen, do incremental INSERT and DELETE, verify state survived.
    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute(matview_sql, ()).await?;

        // Read the post-restore state for group 0: should be ["v0","v2","v4"]
        // (Value::Ord lex-sorted).
        let mut rows = conn.query("SELECT arr FROM mv WHERE g = 0", ()).await?;
        let row = rows.next().await?.unwrap();
        let arr: String = row.get(0)?;
        assert_eq!(
            arr, r#"["v0","v2","v4"]"#,
            "Session 2: matview state did not survive reopen"
        );

        // Incremental INSERT.
        conn.execute("INSERT INTO t VALUES (10, 0, 'v_new')", ())
            .await?;
        let mut rows = conn.query("SELECT arr FROM mv WHERE g = 0", ()).await?;
        let row = rows.next().await?.unwrap();
        let arr: String = row.get(0)?;
        assert_eq!(
            arr, r#"["v0","v2","v4","v_new"]"#,
            "Session 2: incremental insert did not propagate"
        );

        // Incremental DELETE.
        conn.execute("DELETE FROM t WHERE id = 0", ()).await?;
        let mut rows = conn.query("SELECT arr FROM mv WHERE g = 0", ()).await?;
        let row = rows.next().await?.unwrap();
        let arr: String = row.get(0)?;
        assert_eq!(
            arr, r#"["v2","v4","v_new"]"#,
            "Session 2: incremental delete did not propagate"
        );

        drop(conn);
        drop(db);
    }

    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));
    Ok(())
}

#[tokio::test]
async fn test_cross_session_group_concat_with_separator() -> anyhow::Result<()> {
    // Locks down that the group_concat separator survives the to_values
    // / from_values metadata round-trip.
    let db_path = "/tmp/turso-ivm-group-concat-sep-cross-session-test.db";
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));

    let matview_sql = "CREATE MATERIALIZED VIEW IF NOT EXISTS mv AS \
                       SELECT g, group_concat(v, '||') AS s FROM t GROUP BY g";

    {
        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, g INT, v TEXT)", ())
            .await?;
        conn.execute(matview_sql, ()).await?;
        conn.execute("INSERT INTO t VALUES (1, 1, 'a'), (2, 1, 'b')", ())
            .await?;

        let mut rows = conn.query("SELECT s FROM mv WHERE g = 1", ()).await?;
        let row = rows.next().await?.unwrap();
        let s: String = row.get(0)?;
        assert_eq!(s, "a||b");

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

        // Verify state survives.
        let mut rows = conn.query("SELECT s FROM mv WHERE g = 1", ()).await?;
        let row = rows.next().await?.unwrap();
        let s: String = row.get(0)?;
        assert_eq!(s, "a||b", "separator should survive reopen");

        // Incremental insert; separator should still be applied.
        conn.execute("INSERT INTO t VALUES (3, 1, 'c')", ()).await?;
        let mut rows = conn.query("SELECT s FROM mv WHERE g = 1", ()).await?;
        let row = rows.next().await?.unwrap();
        let s: String = row.get(0)?;
        assert_eq!(s, "a||b||c");

        drop(conn);
        drop(db);
    }

    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));
    Ok(())
}
