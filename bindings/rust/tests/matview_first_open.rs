//! Regression test for: first SELECT against a freshly-built materialized
//! view returns a partial subset of rows; a second identical SELECT
//! returns the full result.
//!
//! Originally observed in holon production
//! (`bigdata/turso/bugs/holon_block_matview_first_open_empty_2026-05-08.md`)
//! and fixed in `290fbb4ff` (*"fix: IVM matview cursor returns partial result
//! on first read after IO yield"*). This test is the regression gate for
//! that fix.
//!
//! There's a runnable variant of the same scenario at
//! `bindings/rust/examples/matview_first_open_partial.rs` if you want to
//! see all four scenarios with print-out.

use turso::{params, Builder};

const NUM_ROWS: usize = 1000;

#[tokio::test]
async fn matview_first_read_returns_full_result_same_conn_matview_first() {
    assert_consistent_reads(true, false).await;
}

#[tokio::test]
async fn matview_first_read_returns_full_result_fresh_conn_matview_first() {
    assert_consistent_reads(true, true).await;
}

#[tokio::test]
async fn matview_first_read_returns_full_result_same_conn_matview_after() {
    assert_consistent_reads(false, false).await;
}

#[tokio::test]
async fn matview_first_read_returns_full_result_fresh_conn_matview_after() {
    assert_consistent_reads(false, true).await;
}

async fn assert_consistent_reads(matview_before_inserts: bool, fresh_conn_for_reads: bool) {
    let db = Builder::new_local(":memory:")
        .experimental_materialized_views(true)
        .build()
        .await
        .unwrap();
    let conn = db.connect().unwrap();

    create_base_tables(&conn).await;

    if matview_before_inserts {
        create_matview(&conn).await;
    }

    insert_rows(&conn).await;

    if !matview_before_inserts {
        create_matview(&conn).await;
    }

    let read_conn = if fresh_conn_for_reads {
        db.connect().unwrap()
    } else {
        conn.clone()
    };

    let base = count(&read_conn, "SELECT COUNT(*) FROM t_main").await;
    let first = count(&read_conn, "SELECT COUNT(*) FROM v").await;
    let second = count(&read_conn, "SELECT COUNT(*) FROM v").await;

    assert_eq!(
        base as i64, NUM_ROWS as i64,
        "base table population precondition"
    );
    assert_eq!(
        first, base,
        "first matview read should already see every row \
         (matview_before_inserts={matview_before_inserts}, \
         fresh_conn_for_reads={fresh_conn_for_reads})"
    );
    assert_eq!(first, second, "two identical SELECTs must agree");
}

async fn create_base_tables(conn: &turso::Connection) {
    conn.execute(
        "CREATE TABLE t_main (
            id TEXT PRIMARY KEY,
            payload TEXT NOT NULL DEFAULT '',
            properties TEXT
        )",
        (),
    )
    .await
    .unwrap();
    conn.execute(
        "CREATE TABLE t_tags (
            row_id TEXT NOT NULL,
            tag TEXT NOT NULL,
            PRIMARY KEY (row_id, tag)
        )",
        (),
    )
    .await
    .unwrap();
}

async fn create_matview(conn: &turso::Connection) {
    conn.execute(
        "CREATE MATERIALIZED VIEW v AS
            SELECT
                m.id, m.payload, m.properties,
                COALESCE(json_group_array(t.tag) FILTER (WHERE t.tag IS NOT NULL), '[]') AS tags
            FROM t_main m
            LEFT OUTER JOIN t_tags t ON t.row_id = m.id
            GROUP BY m.id, m.payload, m.properties",
        (),
    )
    .await
    .unwrap();
}

async fn insert_rows(conn: &turso::Connection) {
    for i in 0..NUM_ROWS {
        let id = format!("row-{:05}", i);
        let props = format!(r#"{{"k":{}}}"#, i);
        conn.execute(
            "INSERT INTO t_main (id, payload, properties) VALUES (?1, ?2, ?3)",
            params![id.clone(), format!("p{}", i), props],
        )
        .await
        .unwrap();
        if i % 4 == 0 {
            conn.execute(
                "INSERT INTO t_tags (row_id, tag) VALUES (?1, ?2)",
                params![id, "tag-a"],
            )
            .await
            .unwrap();
        }
    }
}

async fn count(conn: &turso::Connection, sql: &str) -> i64 {
    let mut rows = conn.query(sql, ()).await.unwrap();
    let row = rows
        .next()
        .await
        .unwrap()
        .unwrap_or_else(|| panic!("`{sql}` returned no rows for COUNT(*)"));
    row.get(0).unwrap()
}
