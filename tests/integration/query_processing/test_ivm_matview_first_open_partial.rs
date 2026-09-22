//! Test for IVM matview first-open partial-result bug.
//!
//! When a matview is built/populated on one connection, the first
//! `SELECT` against that matview from a *fresh* connection returns a
//! deterministic prefix of rows rather than the full result. The second
//! identical SELECT on the same fresh connection returns the full result.
//!
//! Adjacent fix `7cf0a2e68a3a` covered the in-transaction case via
//! `MaterializedViewCursor::ensure_tx_changes_computed`. This test is
//! pure autocommit — the autocommit cursor open path needs a symmetric
//! fix that drains pending incremental state before returning rows.
//!
//! See `bugs/holon_block_matview_first_open_empty_2026-05-08.md` and
//! `bindings/rust/examples/matview_first_open_partial.rs` for the
//! original bug report.

const NUM_ROWS: usize = 1000;

async fn populate_and_create_matview(
    matview_before_inserts: bool,
) -> anyhow::Result<turso::Database> {
    let db = turso::Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;

    conn.execute(
        "CREATE TABLE t_main (
            id TEXT PRIMARY KEY,
            payload TEXT NOT NULL DEFAULT '',
            properties TEXT
        )",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE t_tags (
            row_id TEXT NOT NULL,
            tag TEXT NOT NULL,
            PRIMARY KEY (row_id, tag)
        )",
        (),
    )
    .await?;

    let create_mv = "CREATE MATERIALIZED VIEW v AS
            SELECT
                m.id, m.payload, m.properties,
                COALESCE(json_group_array(t.tag) FILTER (WHERE t.tag IS NOT NULL), '[]') AS tags
            FROM t_main m
            LEFT OUTER JOIN t_tags t ON t.row_id = m.id
            GROUP BY m.id, m.payload, m.properties";

    if matview_before_inserts {
        conn.execute(create_mv, ()).await?;
    }

    for i in 0..NUM_ROWS {
        let id = format!("row-{:05}", i);
        let props = format!(r#"{{"k":{}}}"#, i);
        conn.execute(
            "INSERT INTO t_main (id, payload, properties) VALUES (?1, ?2, ?3)",
            turso::params![id.clone(), format!("p{}", i), props],
        )
        .await?;
        if i % 4 == 0 {
            conn.execute(
                "INSERT INTO t_tags (row_id, tag) VALUES (?1, ?2)",
                turso::params![id, "tag-a"],
            )
            .await?;
        }
    }

    if !matview_before_inserts {
        conn.execute(create_mv, ()).await?;
    }

    drop(conn);
    Ok(db)
}

async fn count(conn: &turso::Connection, sql: &str) -> anyhow::Result<i64> {
    let mut rows = conn.query(sql, ()).await?;
    let row = rows.next().await?.unwrap();
    Ok(row.get(0)?)
}

#[tokio::test]
async fn test_matview_first_open_full_result_matview_before_inserts() -> anyhow::Result<()> {
    let db = populate_and_create_matview(true).await?;
    let read_conn = db.connect()?;
    let base = count(&read_conn, "SELECT COUNT(*) FROM t_main").await?;
    assert_eq!(base, NUM_ROWS as i64, "base table not fully populated");

    let first = count(&read_conn, "SELECT COUNT(*) FROM v").await?;
    let second = count(&read_conn, "SELECT COUNT(*) FROM v").await?;

    assert_eq!(
        first, second,
        "fresh-conn first matview read returned {first} rows, second returned {second} \
         (no writes between reads — cursor first-open path is leaking partial state)"
    );
    assert_eq!(first, base, "matview row count diverges from base table");
    Ok(())
}

#[tokio::test]
async fn test_matview_first_open_full_result_matview_after_inserts() -> anyhow::Result<()> {
    let db = populate_and_create_matview(false).await?;
    let read_conn = db.connect()?;
    let base = count(&read_conn, "SELECT COUNT(*) FROM t_main").await?;
    assert_eq!(base, NUM_ROWS as i64, "base table not fully populated");

    let first = count(&read_conn, "SELECT COUNT(*) FROM v").await?;
    let second = count(&read_conn, "SELECT COUNT(*) FROM v").await?;

    assert_eq!(
        first, second,
        "fresh-conn first matview read returned {first} rows, second returned {second} \
         (no writes between reads — cursor first-open path is leaking partial state)"
    );
    assert_eq!(first, base, "matview row count diverges from base table");
    Ok(())
}
