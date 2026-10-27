//! Reproducer for holon's cross-junction duplicate-row bug (2026-07-07).
//!
//! Shape: matview over `base LEFT JOIN j1 LEFT JOIN j2` with
//! `json_group_array(...) FILTER` + `GROUP BY <base cols>`.
//!
//! Trigger: ONE transaction containing both a base-row UPDATE (touching a
//! GROUP BY column) and an INSERT into the earlier join's table (j1). The
//! first LEFT JOIN's output delta is then unconsolidated and contains both
//! -1 and +1 for the same row: `L_state x dR` yields +(old_base, j1row)
//! while `dL x dR` yields -(old_base, j1row). Committing that delta
//! row-by-row into the second join's left state turned the retraction of a
//! not-yet-existing row into a no-op, leaving a +1 ghost keyed by the OLD
//! base values. A later INSERT into j2 probed that state and emitted a
//! spurious duplicate aggregated row (stale group key, current aggregates).
//!
//! Fixed by consolidating input deltas in JoinOperator::commit (matching
//! what Antijoin/MatchCounter/Aggregate already did).

use tempfile::TempDir;
use turso::Builder;

async fn setup(db_path: &str) -> anyhow::Result<turso::Connection> {
    let db = Builder::new_local(db_path)
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;

    conn.execute(
        "CREATE TABLE base (id TEXT PRIMARY KEY, v TEXT, updated_at INTEGER)",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE j1 (base_id TEXT, a TEXT, PRIMARY KEY (base_id, a))",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE j2 (base_id TEXT, b TEXT, PRIMARY KEY (base_id, b))",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW m AS \
         SELECT base.id, base.v, base.updated_at, \
                COALESCE(json_group_array(j1.a) FILTER (WHERE j1.a IS NOT NULL), '[]') AS a_s, \
                COALESCE(json_group_array(j2.b) FILTER (WHERE j2.b IS NOT NULL), '[]') AS b_s \
         FROM base \
         LEFT OUTER JOIN j1 ON j1.base_id = base.id \
         LEFT OUTER JOIN j2 ON j2.base_id = base.id \
         GROUP BY base.id, base.v, base.updated_at",
        (),
    )
    .await?;
    conn.execute("INSERT INTO base VALUES ('x', 'v', 0)", ())
        .await?;
    Ok(conn)
}

async fn assert_single_row(conn: &turso::Connection, label: &str) -> anyhow::Result<()> {
    let mut rows = conn.query("SELECT * FROM m", ()).await?;
    let mut dump = Vec::new();
    while let Some(row) = rows.next().await? {
        let mut cols = Vec::new();
        for i in 0..row.column_count() {
            cols.push(format!("{:?}", row.get_value(i)?));
        }
        dump.push(format!("({})", cols.join(", ")));
    }
    assert_eq!(
        dump.len(),
        1,
        "[{label}] matview must have exactly 1 row for group 'x', got {}: {:?}",
        dump.len(),
        dump
    );
    Ok(())
}

/// Minimal reproducer: txn(base UPDATE of a GROUP BY column + j1 INSERT),
/// then a plain j2 insert. Pre-fix this yielded 2 rows: the correct one and
/// a ghost keyed by the base row's PRE-transaction updated_at.
#[tokio::test]
async fn test_ivm_left_join_aggregate_duplicate() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("ljagg-dup-min.db");
    let conn = setup(db_path.to_str().unwrap()).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("UPDATE base SET updated_at = 1 WHERE id = 'x'", ())
        .await?;
    conn.execute("INSERT INTO j1 (base_id, a) VALUES ('x', 'proj')", ())
        .await?;
    conn.execute("COMMIT", ()).await?;
    assert_single_row(&conn, "after txn(base update + j1 insert)").await?;

    conn.execute("INSERT INTO j2 (base_id, b) VALUES ('x', 'req')", ())
        .await?;
    assert_single_row(&conn, "after plain j2 insert").await?;
    Ok(())
}

/// Holon's full per-edge-write pattern: each write is one transaction of
/// base UPDATE + junction DELETE-all + junction INSERT; tags (j1) first,
/// then requires (j2). This is the exact statement stream captured from
/// holon's failing matview_duplicate_row_repro test.
#[tokio::test]
async fn test_ivm_left_join_aggregate_duplicate_holon_txn_pattern() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("ljagg-dup-txn.db");
    let conn = setup(db_path.to_str().unwrap()).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("UPDATE base SET updated_at = 1 WHERE id = 'x'", ())
        .await?;
    conn.execute("DELETE FROM j1 WHERE base_id = 'x'", ())
        .await?;
    conn.execute("INSERT INTO j1 (base_id, a) VALUES ('x', 'proj')", ())
        .await?;
    conn.execute("COMMIT", ()).await?;
    assert_single_row(&conn, "after txn edge write 1 (j1)").await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("UPDATE base SET updated_at = 2 WHERE id = 'x'", ())
        .await?;
    conn.execute("DELETE FROM j2 WHERE base_id = 'x'", ())
        .await?;
    conn.execute("INSERT INTO j2 (base_id, b) VALUES ('x', 'req')", ())
        .await?;
    conn.execute("COMMIT", ()).await?;
    assert_single_row(&conn, "after txn edge write 2 (j2)").await?;
    Ok(())
}

/// Control: plain autocommit inserts (no combined txn) never duplicated.
#[tokio::test]
async fn test_ivm_left_join_aggregate_plain_inserts_control() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("ljagg-dup-plain.db");
    let conn = setup(db_path.to_str().unwrap()).await?;

    conn.execute("INSERT INTO j1 VALUES ('x', 'proj')", ())
        .await?;
    assert_single_row(&conn, "after j1 insert").await?;

    conn.execute("INSERT INTO j2 VALUES ('x', 'req')", ())
        .await?;
    assert_single_row(&conn, "after j2 insert").await?;
    Ok(())
}

/// Control: second write into the SAME junction (holon matrix: tags->tags OK).
#[tokio::test]
async fn test_ivm_left_join_aggregate_same_junction_control() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("ljagg-dup-samej.db");
    let conn = setup(db_path.to_str().unwrap()).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("UPDATE base SET updated_at = 1 WHERE id = 'x'", ())
        .await?;
    conn.execute("INSERT INTO j1 (base_id, a) VALUES ('x', 'proj')", ())
        .await?;
    conn.execute("COMMIT", ()).await?;

    conn.execute("INSERT INTO j1 (base_id, a) VALUES ('x', 'proj2')", ())
        .await?;
    assert_single_row(&conn, "after second j1 insert").await?;
    Ok(())
}

/// Control: reverse junction order (j2 before j1) was observed OK in holon.
#[tokio::test]
async fn test_ivm_left_join_aggregate_reverse_order_control() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("ljagg-dup-rev.db");
    let conn = setup(db_path.to_str().unwrap()).await?;

    conn.execute("INSERT INTO j2 VALUES ('x', 'req')", ())
        .await?;
    assert_single_row(&conn, "after j2 insert").await?;

    conn.execute("INSERT INTO j1 VALUES ('x', 'proj')", ())
        .await?;
    assert_single_row(&conn, "after j1 insert").await?;
    Ok(())
}
