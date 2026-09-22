//! CREATE MATERIALIZED VIEW must reject unresolvable column references at DDL
//! time, naming the offending column and its table qualifier.

use tempfile::TempDir;
use turso::Builder;

async fn conn_with_matviews() -> anyhow::Result<turso::Connection> {
    let db = Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;
    conn.execute(
        "CREATE TABLE block_raw (id TEXT PRIMARY KEY, parent_id TEXT, content TEXT)",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE block_links (source_block_id TEXT, resolved_id TEXT)",
        (),
    )
    .await?;
    Ok(conn)
}

#[tokio::test]
async fn test_matview_ddl_rejects_qualified_missing_column() -> anyhow::Result<()> {
    let conn = conn_with_matviews().await?;

    let err = conn
        .execute(
            "CREATE MATERIALIZED VIEW backlinks AS \
             SELECT bl.resolved_id AS target_id, b.id, b.parent_id, b.content, b.depth \
             FROM block_links bl JOIN block_raw b ON b.id = bl.source_block_id",
            (),
        )
        .await
        .expect_err("CREATE MATERIALIZED VIEW naming a nonexistent column must fail at DDL time");

    let msg = err.to_string();
    assert!(
        msg.contains("no such column") && msg.contains("depth") && msg.contains("b"),
        "error must name the missing column and its table qualifier, got: {msg}"
    );
    assert!(
        !msg.contains("incompatible version"),
        "DDL failure must not be reported as a DBSP version mismatch, got: {msg}"
    );
    Ok(())
}

#[tokio::test]
async fn test_matview_ddl_rejects_unqualified_missing_column() -> anyhow::Result<()> {
    let conn = conn_with_matviews().await?;

    let err = conn
        .execute(
            "CREATE MATERIALIZED VIEW mv_bad AS SELECT id, sort_key FROM block_raw",
            (),
        )
        .await
        .expect_err("unqualified nonexistent column must fail at DDL time");

    let msg = err.to_string();
    assert!(
        msg.contains("no such column") && msg.contains("sort_key"),
        "error must name the missing column, got: {msg}"
    );
    Ok(())
}

#[tokio::test]
async fn test_matview_ddl_missing_column_in_where_and_join() -> anyhow::Result<()> {
    let conn = conn_with_matviews().await?;

    let err = conn
        .execute(
            "CREATE MATERIALIZED VIEW mv_bad_where AS \
             SELECT id FROM block_raw WHERE source_language = 'rust'",
            (),
        )
        .await
        .expect_err("nonexistent column in WHERE must fail at DDL time");
    assert!(err.to_string().contains("source_language"), "got: {err}");

    let err = conn
        .execute(
            "CREATE MATERIALIZED VIEW mv_bad_join AS \
             SELECT b.id FROM block_raw b JOIN block_links bl ON b.missing_key = bl.resolved_id",
            (),
        )
        .await
        .expect_err("nonexistent column in a JOIN condition must fail at DDL time");
    assert!(err.to_string().contains("missing_key"), "got: {err}");

    Ok(())
}

#[tokio::test]
async fn test_matview_ddl_reports_first_missing_column_of_several() -> anyhow::Result<()> {
    let conn = conn_with_matviews().await?;

    let err = conn
        .execute(
            "CREATE MATERIALIZED VIEW mv_many_bad AS \
             SELECT id, depth, sort_key, source_language FROM block_raw",
            (),
        )
        .await
        .expect_err("several nonexistent columns must fail at DDL time");
    assert!(
        err.to_string().contains("depth"),
        "error must name the first missing column, got: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn test_matview_ddl_accepts_valid_columns() -> anyhow::Result<()> {
    let conn = conn_with_matviews().await?;

    conn.execute("INSERT INTO block_raw VALUES ('b1', NULL, 'hello')", ())
        .await?;
    conn.execute("INSERT INTO block_links VALUES ('b1', 't1')", ())
        .await?;

    conn.execute(
        "CREATE MATERIALIZED VIEW backlinks AS \
         SELECT bl.resolved_id AS target_id, b.id, b.parent_id, b.content \
         FROM block_links bl JOIN block_raw b ON b.id = bl.source_block_id",
        (),
    )
    .await?;

    let mut rows = conn.query("SELECT COUNT(*) FROM backlinks", ()).await?;
    let count: i64 = rows.next().await?.unwrap().get(0)?;
    assert_eq!(count, 1);

    // `*` expansion still works.
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_star AS SELECT * FROM block_raw",
        (),
    )
    .await?;
    let mut rows = conn.query("SELECT COUNT(*) FROM mv_star", ()).await?;
    let count: i64 = rows.next().await?.unwrap().get(0)?;
    assert_eq!(count, 1);

    Ok(())
}

#[tokio::test]
async fn test_matview_valid_view_survives_reopen() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("matview-ddl-validation-reopen.db");
    let db_path = db_path.to_str().unwrap();

    {
        let db = Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)", ())
            .await?;
        conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')", ())
            .await?;
        conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, v FROM t", ())
            .await?;
        drop(conn);
        drop(db);
    }

    {
        let db = Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        let mut rows = conn.query("SELECT COUNT(*) FROM mv", ()).await?;
        let count: i64 = rows.next().await?.unwrap().get(0)?;
        assert_eq!(count, 2);
        drop(conn);
        drop(db);
    }

    Ok(())
}

/// A view that became unloadable because its base table lost a column is not a
/// DBSP version problem, and must not be reported as one.
#[tokio::test]
async fn test_stale_matview_query_reports_schema_cause_not_version() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("matview-stale-error-shape.db");
    let db_path = db_path.to_str().unwrap();

    {
        let db = Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        conn.execute(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT, b TEXT)",
            (),
        )
        .await?;
        conn.execute("INSERT INTO t VALUES (1, 'x', 'y')", ())
            .await?;
        conn.execute(
            "CREATE MATERIALIZED VIEW mv_ab AS SELECT id, a, b FROM t",
            (),
        )
        .await?;
        drop(conn);
        drop(db);
    }

    {
        let db = Builder::new_local(db_path).build().await?;
        let conn = db.connect()?;
        conn.execute("DROP TABLE t", ()).await?;
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a TEXT)", ())
            .await?;
        drop(conn);
        drop(db);
    }

    {
        let db = Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;
        let conn = db.connect()?;
        let Err(err) = conn.query("SELECT * FROM mv_ab", ()).await else {
            panic!("querying a matview whose base table lost a column must fail");
        };
        let msg = err.to_string();
        assert!(
            !msg.contains("incompatible version") && !msg.contains("DBSP version"),
            "a schema mismatch must not be reported as a version mismatch, got: {msg}"
        );
        assert!(
            msg.contains("mv_ab"),
            "error must name the view, got: {msg}"
        );
        drop(conn);
        drop(db);
    }

    Ok(())
}
