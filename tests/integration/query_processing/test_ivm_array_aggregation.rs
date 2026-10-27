//! Cross-session and adversarial-corpus tests for IVM array/string aggregation.
//!
//! Sqltest coverage in `testing/runner/tests/ivm-array-aggregation.sqltest`
//! exercises the in-memory codepath. These tests round-trip through a
//! file-backed DB to lock down `AggregateState::to_value_vector` /
//! `from_value_vector` for the new multiset variants.

use tempfile::TempDir;

#[tokio::test]
async fn test_cross_session_array_agg_restore() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("array-agg-cross-session-test.db");
    let db_path = db_path.to_str().unwrap();

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

    Ok(())
}
