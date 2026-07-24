//! Reproducer for holon's chained-agg duplicate-row bug (docblock-count
//! divergence, 2026-07-07).
//!
//! Shape (the CHAINED-agg refactor, distinct from
//! `test_ivm_left_join_aggregate_duplicate`'s single view):
//!   - a per-junction aggregation matview `j1_agg =
//!     SELECT base_id AS source_id, json_group_array(a) AS vals
//!     FROM j1 GROUP BY base_id`  (at most one row per source id), and
//!   - an outer matview `m = SELECT base.*, COALESCE(j1_agg.vals, '[]')
//!     FROM base LEFT OUTER JOIN j1_agg ON j1_agg.source_id = base.id`.
//!
//! Trigger: insert a junction row (group becomes non-empty; the LEFT JOIN's
//! left row transitions unmatched->matched), THEN delete that junction row
//! (the group EMPTIES; `j1_agg` retracts its only row). Under the outer
//! LEFT JOIN the retraction of the matching right row must retract the joined
//! output row AND re-emit the unmatched (COALESCE '[]') row. Pre-fix the
//! retraction is lost and the matview holds BOTH the stale matched row and
//! the new unmatched row -> two rows for one base id.

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
    // per-junction aggregation matview: one row per source id.
    conn.execute(
        "CREATE MATERIALIZED VIEW j1_agg AS \
         SELECT base_id AS source_id, json_group_array(a) AS vals \
         FROM j1 GROUP BY base_id",
        (),
    )
    .await?;
    // outer matview: base LEFT JOIN the agg matview (matview-on-matview).
    conn.execute(
        "CREATE MATERIALIZED VIEW m AS \
         SELECT base.id, base.v, base.updated_at, \
                COALESCE(j1_agg.vals, '[]') AS a_s \
         FROM base \
         LEFT OUTER JOIN j1_agg ON j1_agg.source_id = base.id",
        (),
    )
    .await?;
    conn.execute("INSERT INTO base VALUES ('x', 'v', 0)", ())
        .await?;
    Ok(conn)
}

async fn dump(conn: &turso::Connection, label: &str) -> anyhow::Result<usize> {
    let mut rows = conn.query("SELECT * FROM m", ()).await?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().await? {
        let mut cols = Vec::new();
        for i in 0..row.column_count() {
            cols.push(format!("{:?}", row.get_value(i)?));
        }
        out.push(format!("({})", cols.join(", ")));
    }
    eprintln!("[{label}] m rows = {}: {:?}", out.len(), out);
    Ok(out.len())
}

async fn assert_single_row(conn: &turso::Connection, label: &str) -> anyhow::Result<()> {
    let n = dump(conn, label).await?;
    assert_eq!(
        n, 1,
        "[{label}] matview must have exactly 1 row for 'x', got {n}"
    );
    Ok(())
}

/// Minimal: insert a junction row (group fills), then delete it (group empties).
#[tokio::test]
async fn test_ivm_agg_matview_stale_group_on_delete() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("aggstale-min.db");
    let conn = setup(db_path.to_str().unwrap()).await?;
    assert_single_row(&conn, "initial (unmatched)").await?;

    conn.execute("INSERT INTO j1 (base_id, a) VALUES ('x', 'Page')", ())
        .await?;
    assert_single_row(&conn, "after j1 insert (matched)").await?;

    conn.execute("DELETE FROM j1 WHERE base_id = 'x'", ())
        .await?;
    assert_single_row(&conn, "after j1 delete (group empties)").await?;
    Ok(())
}

/// Keystone-faithful: the base row and its junction row are inserted TOGETHER
/// (ingest creates the block already carrying the `Page` tag), THEN the tag is
/// deleted. Mirrors ref-doc-6's lifecycle where block_raw + block_tags(Page)
/// land in the same ingest batch, distinct from adding the tag in a later batch.
#[tokio::test]
async fn test_ivm_agg_stale_group_born_matched_then_delete() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let path = dir.path().join("aggstale-born.db");
    let path = path.to_str().unwrap();
    let db = Builder::new_local(path)
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
        "CREATE MATERIALIZED VIEW j1_agg AS \
         SELECT base_id AS source_id, json_group_array(a) AS vals \
         FROM j1 GROUP BY base_id",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW m AS \
         SELECT base.id, base.v, base.updated_at, \
                COALESCE(j1_agg.vals, '[]') AS a_s \
         FROM base \
         LEFT OUTER JOIN j1_agg ON j1_agg.source_id = base.id",
        (),
    )
    .await?;

    // Born matched: base row + junction row inserted together (ingest batch).
    conn.execute("BEGIN", ()).await?;
    conn.execute("INSERT INTO base VALUES ('x', 'v', 0)", ())
        .await?;
    conn.execute("INSERT INTO j1 (base_id, a) VALUES ('x', 'Page')", ())
        .await?;
    conn.execute("COMMIT", ()).await?;
    assert_single_row(&conn, "born matched (base+j1 same txn)").await?;

    // Delete the tag (group empties).
    conn.execute("DELETE FROM j1 WHERE base_id = 'x'", ())
        .await?;
    assert_single_row(&conn, "after tag delete").await?;
    Ok(())
}

/// Faithful ingest-rescan replay (captured from the failing keystone,
/// block:ref-doc-6): the base row is UPSERTed (ON CONFLICT DO UPDATE, content
/// churns each rescan) and the tag is DELETE-then-INSERTed as SEPARATE
/// autocommit batches (not one transaction). The group thus EMPTIES then
/// REFILLS across two batches while the base row is also churning. The outer
/// LEFT JOIN must retract the null-padded ('[]') row when the tag is
/// re-inserted; if it doesn't, the '[]' ghost survives beside the ['Page'] row.
#[tokio::test]
async fn test_ivm_agg_ingest_rescan_delete_then_reinsert_tag() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let path = dir.path().join("aggstale-rescan.db");
    let path = path.to_str().unwrap();
    let db = Builder::new_local(path)
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;
    conn.execute(
        "CREATE TABLE base (id TEXT PRIMARY KEY, content TEXT, updated_at INTEGER)",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE j1 (base_id TEXT, a TEXT, PRIMARY KEY (base_id, a))",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW j1_agg AS \
         SELECT base_id AS source_id, json_group_array(a) AS vals \
         FROM j1 GROUP BY base_id",
        (),
    )
    .await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW m AS \
         SELECT base.id, base.content, base.updated_at, \
                COALESCE(j1_agg.vals, '[]') AS a_s \
         FROM base \
         LEFT OUTER JOIN j1_agg ON j1_agg.source_id = base.id",
        (),
    )
    .await?;

    // Born matched: base + Page tag.
    conn.execute(
        "INSERT INTO base VALUES ('x', 'c0', 0) ON CONFLICT(id) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at",
        (),
    )
    .await?;
    conn.execute("INSERT INTO j1 (base_id, a) VALUES ('x', 'Page')", ())
        .await?;
    assert_single_row(&conn, "born matched").await?;

    // Several ingest rescans: each = base UPSERT (content churn), then
    // DELETE tag, then INSERT tag -- all separate autocommit batches.
    for i in 1..=4 {
        conn.execute(
            &format!("INSERT INTO base VALUES ('x', 'c{i}', {i}) ON CONFLICT(id) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at"),
            (),
        )
        .await?;
        conn.execute("DELETE FROM j1 WHERE base_id = 'x'", ())
            .await?;
        assert_single_row(&conn, &format!("rescan {i}: after tag delete")).await?;
        conn.execute("INSERT INTO j1 (base_id, a) VALUES ('x', 'Page')", ())
            .await?;
        assert_single_row(&conn, &format!("rescan {i}: after tag reinsert")).await?;
    }
    Ok(())
}

/// FAITHFUL to holon's `block` matview: THREE chained agg LEFT JOINs in the
/// exact order block_raw LEFT JOIN requires_agg LEFT JOIN tags_agg LEFT JOIN
/// advice_agg. The toggling tags junction is the MIDDLE join, so its LEFT input
/// is the MERGE output of the prior (requires) LEFT JOIN -- not the raw base
/// table. This is the chained-LEFT-JOIN-fed-by-a-merge scenario. requires and
/// advice groups stay empty (as for ref-doc-6); only tags toggles.
#[tokio::test]
async fn test_ivm_agg_three_chain_tags_middle_toggle() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let path = dir.path().join("aggstale-3chain.db");
    let path = path.to_str().unwrap();
    let db = Builder::new_local(path)
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;
    conn.execute(
        "CREATE TABLE base (id TEXT PRIMARY KEY, content TEXT, updated_at INTEGER)",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE reqs (block_id TEXT, required_id TEXT, PRIMARY KEY (block_id, required_id))",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE tags (block_id TEXT, tag TEXT, PRIMARY KEY (block_id, tag))",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE adv (anchor_id TEXT, lesson_id TEXT, PRIMARY KEY (anchor_id, lesson_id))",
        (),
    )
    .await?;
    conn.execute("CREATE MATERIALIZED VIEW reqs_agg AS SELECT block_id AS source_id, json_group_array(required_id) AS vals FROM reqs GROUP BY block_id", ()).await?;
    conn.execute("CREATE MATERIALIZED VIEW tags_agg AS SELECT block_id AS source_id, json_group_array(tag) AS vals FROM tags GROUP BY block_id", ()).await?;
    conn.execute("CREATE MATERIALIZED VIEW adv_agg AS SELECT anchor_id AS source_id, json_group_array(lesson_id) AS vals FROM adv GROUP BY anchor_id", ()).await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW m AS \
         SELECT b.id, b.content, b.updated_at, \
                COALESCE(reqs_agg.vals, '[]') AS requires, \
                COALESCE(tags_agg.vals, '[]') AS tags, \
                COALESCE(adv_agg.vals, '[]') AS adv \
         FROM base b \
         LEFT OUTER JOIN reqs_agg ON reqs_agg.source_id = b.id \
         LEFT OUTER JOIN tags_agg ON tags_agg.source_id = b.id \
         LEFT OUTER JOIN adv_agg ON adv_agg.source_id = b.id",
        (),
    )
    .await?;

    conn.execute("INSERT INTO base VALUES ('x', 'c0', 0) ON CONFLICT(id) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at", ()).await?;
    conn.execute("INSERT INTO tags (block_id, tag) VALUES ('x', 'Page')", ())
        .await?;
    assert_single_row(&conn, "born matched (tags=Page)").await?;

    for i in 1..=4 {
        conn.execute(
            &format!("INSERT INTO base VALUES ('x', 'c{i}', {i}) ON CONFLICT(id) DO UPDATE SET content = excluded.content, updated_at = excluded.updated_at"),
            (),
        )
        .await?;
        conn.execute("DELETE FROM tags WHERE block_id = 'x'", ())
            .await?;
        assert_single_row(&conn, &format!("rescan {i}: tags deleted")).await?;
        conn.execute("INSERT INTO tags (block_id, tag) VALUES ('x', 'Page')", ())
            .await?;
        assert_single_row(&conn, &format!("rescan {i}: tags reinserted")).await?;
        conn.execute("DELETE FROM reqs WHERE block_id = 'x'", ())
            .await?;
        assert_single_row(&conn, &format!("rescan {i}: reqs delete (noop)")).await?;
    }
    Ok(())
}

/// Multi-block, multi-junction interleaved toggling in autocommit (each
/// statement its own maintenance pass), mirroring how holon drives the shared
/// agg matviews. Two blocks P and C; P carries requires+tags+advice, C requires
/// P. Each "rescan" deletes+reinserts every junction for P as separate
/// autocommit statements, interleaved with base upserts. Reproduces the stale
/// null-padded LEFT-JOIN row in the chained block matview.
#[tokio::test]
async fn test_ivm_agg_multiblock_interleaved_toggle() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let path = dir.path().join("aggstale-multi.db");
    let path = path.to_str().unwrap();
    let db = Builder::new_local(path)
        .experimental_materialized_views(true)
        .build()
        .await?;
    let conn = db.connect()?;
    conn.execute(
        "CREATE TABLE base (id TEXT PRIMARY KEY, content TEXT, updated_at INTEGER)",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE reqs (block_id TEXT, required_id TEXT, PRIMARY KEY (block_id, required_id))",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE tags (block_id TEXT, tag TEXT, PRIMARY KEY (block_id, tag))",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE adv (anchor_id TEXT, lesson_id TEXT, PRIMARY KEY (anchor_id, lesson_id))",
        (),
    )
    .await?;
    conn.execute("CREATE MATERIALIZED VIEW reqs_agg AS SELECT block_id AS source_id, json_group_array(required_id) AS vals FROM reqs GROUP BY block_id", ()).await?;
    conn.execute("CREATE MATERIALIZED VIEW tags_agg AS SELECT block_id AS source_id, json_group_array(tag) AS vals FROM tags GROUP BY block_id", ()).await?;
    conn.execute("CREATE MATERIALIZED VIEW adv_agg AS SELECT anchor_id AS source_id, json_group_array(lesson_id) AS vals FROM adv GROUP BY anchor_id", ()).await?;
    conn.execute(
        "CREATE MATERIALIZED VIEW m AS \
         SELECT b.id, b.content, b.updated_at, \
                COALESCE(reqs_agg.vals, '[]') AS requires, \
                COALESCE(tags_agg.vals, '[]') AS tags, \
                COALESCE(adv_agg.vals, '[]') AS adv \
         FROM base b \
         LEFT OUTER JOIN reqs_agg ON reqs_agg.source_id = b.id \
         LEFT OUTER JOIN tags_agg ON tags_agg.source_id = b.id \
         LEFT OUTER JOIN adv_agg ON adv_agg.source_id = b.id",
        (),
    )
    .await?;

    for b in ["P", "C", "R", "A"] {
        conn.execute(&format!("INSERT INTO base VALUES ('{b}', 'c0', 0)"), ())
            .await?;
    }
    conn.execute(
        "INSERT INTO reqs (block_id, required_id) VALUES ('P', 'R')",
        (),
    )
    .await?;
    conn.execute(
        "INSERT INTO tags (block_id, tag) VALUES ('P', 'ctx'), ('P', 'task')",
        (),
    )
    .await?;
    conn.execute(
        "INSERT INTO adv (anchor_id, lesson_id) VALUES ('P', 'A')",
        (),
    )
    .await?;
    conn.execute(
        "INSERT INTO reqs (block_id, required_id) VALUES ('C', 'P')",
        (),
    )
    .await?;

    async fn no_dup(conn: &turso::Connection, label: &str) -> anyhow::Result<()> {
        let mut rows = conn
            .query(
                "SELECT id, COUNT(*) c FROM m GROUP BY id HAVING COUNT(*)>1",
                (),
            )
            .await?;
        let mut dups = Vec::new();
        while let Some(r) = rows.next().await? {
            dups.push(format!("{:?}x{:?}", r.get_value(0)?, r.get_value(1)?));
        }
        assert!(dups.is_empty(), "[{label}] duplicate rows: {dups:?}");
        Ok(())
    }
    no_dup(&conn, "initial").await?;

    for i in 1..=6 {
        conn.execute(
            &format!("UPDATE base SET content='c{i}', updated_at={i} WHERE id='P'"),
            (),
        )
        .await?;
        conn.execute("DELETE FROM tags WHERE block_id='P'", ())
            .await?;
        no_dup(&conn, &format!("r{i} tags-del")).await?;
        conn.execute(
            "INSERT INTO tags (block_id, tag) VALUES ('P','ctx'),('P','task')",
            (),
        )
        .await?;
        no_dup(&conn, &format!("r{i} tags-ins")).await?;
        conn.execute("DELETE FROM reqs WHERE block_id='P'", ())
            .await?;
        no_dup(&conn, &format!("r{i} reqs-del")).await?;
        conn.execute(
            "INSERT INTO reqs (block_id, required_id) VALUES ('P','R')",
            (),
        )
        .await?;
        no_dup(&conn, &format!("r{i} reqs-ins")).await?;
        conn.execute("DELETE FROM adv WHERE anchor_id='P'", ())
            .await?;
        no_dup(&conn, &format!("r{i} adv-del")).await?;
        conn.execute(
            "INSERT INTO adv (anchor_id, lesson_id) VALUES ('P','A')",
            (),
        )
        .await?;
        no_dup(&conn, &format!("r{i} adv-ins")).await?;
    }
    Ok(())
}

/// Holon's per-edge-write pattern: each write is base UPDATE + junction
/// DELETE-all + optional junction INSERT, in ONE transaction. Tag {Page}
/// then REPLACE tags with {} (delete Page).
#[tokio::test]
async fn test_ivm_agg_matview_stale_group_on_delete_holon_txn() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("aggstale-txn.db");
    let conn = setup(db_path.to_str().unwrap()).await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("UPDATE base SET updated_at = 1 WHERE id = 'x'", ())
        .await?;
    conn.execute("DELETE FROM j1 WHERE base_id = 'x'", ())
        .await?;
    conn.execute("INSERT INTO j1 (base_id, a) VALUES ('x', 'Page')", ())
        .await?;
    conn.execute("COMMIT", ()).await?;
    assert_single_row(&conn, "after set tags {Page}").await?;

    conn.execute("BEGIN", ()).await?;
    conn.execute("UPDATE base SET updated_at = 2 WHERE id = 'x'", ())
        .await?;
    conn.execute("DELETE FROM j1 WHERE base_id = 'x'", ())
        .await?;
    conn.execute("COMMIT", ()).await?;
    assert_single_row(&conn, "after set tags {} (delete Page)").await?;
    Ok(())
}
