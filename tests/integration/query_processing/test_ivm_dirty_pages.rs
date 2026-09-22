//! Test for IVM "dirty pages should be empty for read txn" bug
//!
//! Reproduces a pager assertion that fires when:
//! - Multiple matviews (recursive CTE + JOIN + filtered) exist
//! - Rapid inserts trigger IVM updates on all matviews
//! - Interleaved CREATE MATERIALIZED VIEW IF NOT EXISTS DDL
//! - CDC callback is registered
//! - File-backed database with enough data for BTree page pressure
//!
//! The bug manifests as a panic in pager.rs rollback() when a read-txn
//! rollback encounters dirty pages left from the parent write transaction.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tempfile::TempDir;

/// Stress test matching holon PBT pattern: rapid inserts + interleaved matview DDL + CDC
#[tokio::test]
async fn test_dirty_pages_rapid_inserts_with_interleaved_matview_ddl() -> anyhow::Result<()> {
    let dir = TempDir::new()?;
    let db_path = dir.path().join("dirty-pages.db");
    let db_path = db_path.to_str().unwrap();

    let db = turso::Builder::new_local(db_path)
        .experimental_materialized_views(true)
        .build()
        .await?;

    let conn = db.connect()?;

    // Full holon-like schema
    conn.execute(
        "CREATE TABLE blocks (
            id TEXT PRIMARY KEY,
            parent_id TEXT NOT NULL,
            content TEXT DEFAULT '',
            content_type TEXT DEFAULT 'text',
            source_language TEXT DEFAULT '',
            source_name TEXT DEFAULT '',
            properties TEXT DEFAULT '{}',
            created_at TEXT DEFAULT (datetime('now')),
            updated_at TEXT DEFAULT (datetime('now'))
        )",
        (),
    )
    .await?;

    // Recursive CTE matview (heavy IVM work per insert)
    conn.execute(
        "CREATE MATERIALIZED VIEW blocks_with_paths AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, '/' || id as path
             FROM blocks
             WHERE parent_id LIKE 'doc:%' OR parent_id LIKE 'sentinel:%'
             UNION ALL
             SELECT b.id, b.parent_id, b.content, p.path || '/' || b.id as path
             FROM blocks b
             INNER JOIN paths p ON b.parent_id = p.id
         )
         SELECT * FROM paths",
        (),
    )
    .await?;

    // Navigation tables for JOIN matview
    conn.execute(
        "CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT)",
        (),
    )
    .await?;
    conn.execute(
        "CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER)",
        (),
    )
    .await?;
    conn.execute(
        "INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL)",
        (),
    )
    .await?;

    // JOIN matview
    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
        (),
    )
    .await?;

    // Events table + filtered matview
    conn.execute(
        "CREATE TABLE events (
            id TEXT PRIMARY KEY,
            status TEXT NOT NULL DEFAULT 'pending',
            aggregate_type TEXT,
            data TEXT DEFAULT '{}'
        )",
        (),
    )
    .await?;

    conn.execute(
        "CREATE MATERIALIZED VIEW events_view_block AS
         SELECT * FROM events WHERE status = 'confirmed' AND aggregate_type = 'block'",
        (),
    )
    .await?;

    // Set up CDC callback (like holon's set_change_callback)
    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_count_clone = cdc_count.clone();
    conn.set_change_callback(move |event| {
        cdc_count_clone.fetch_add(event.changes.len(), Ordering::Relaxed);
    })?;

    // Set up navigation
    conn.execute(
        "INSERT INTO navigation_history (region, block_id) VALUES ('main', 'doc:root1')",
        (),
    )
    .await?;
    conn.execute(
        "UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'",
        (),
    )
    .await?;

    let large_content = "x".repeat(300);

    // Phase 1: Rapid bulk insert of root blocks (like holon's BulkExternalAdd)
    for i in 0..50 {
        conn.execute(
            &format!(
                "INSERT INTO blocks (id, parent_id, content) VALUES ('doc:root{}', 'sentinel:top', '{}')",
                i, large_content
            ),
            (),
        )
        .await?;
    }

    // Phase 2: Interleave matview DDL with inserts (like holon's query_and_watch)
    for batch in 0..10 {
        // Create a watch_view matview (IF NOT EXISTS — may already exist)
        conn.execute(
            &format!(
                "CREATE MATERIALIZED VIEW IF NOT EXISTS watch_view_{} AS
                 SELECT id, parent_id, content FROM blocks WHERE parent_id = 'doc:root{}'",
                batch, batch
            ),
            (),
        )
        .await?;

        // Burst of child inserts
        for i in 0..20 {
            let block_id = format!("child-{}-{}", batch, i);
            let parent = format!("doc:root{}", batch);
            conn.execute(
                &format!(
                    "INSERT INTO blocks (id, parent_id, content) VALUES ('{}', '{}', '{}')",
                    block_id, parent, large_content
                ),
                (),
            )
            .await?;
        }
    }

    // Phase 3: Deeper tree with grandchildren (trigger recursive CTE depth)
    for i in 0..100 {
        let block_id = format!("gc-{}", i);
        let parent = format!("child-{}-{}", i % 10, i % 20);
        conn.execute(
            &format!(
                "INSERT INTO blocks (id, parent_id, content) VALUES ('{}', '{}', '{}')",
                block_id, parent, large_content
            ),
            (),
        )
        .await?;

        // Interleave more watch_view DDL
        if i % 25 == 0 {
            conn.execute(
                &format!(
                    "CREATE MATERIALIZED VIEW IF NOT EXISTS watch_view_gc_{} AS
                     SELECT id, content FROM blocks WHERE id LIKE 'gc-%' AND content LIKE '%x%'",
                    i
                ),
                (),
            )
            .await?;
        }
    }

    // Phase 4: Events + navigation changes (exercise all matviews simultaneously)
    for i in 0..50 {
        conn.execute(
            &format!(
                "INSERT INTO events (id, status, aggregate_type, data) VALUES ('evt-{}', 'confirmed', 'block', '{{}}')",
                i
            ),
            (),
        )
        .await?;

        if i % 10 == 0 {
            conn.execute(
                &format!(
                    "INSERT INTO navigation_history (region, block_id) VALUES ('main', 'doc:root{}')",
                    i % 50
                ),
                (),
            )
            .await?;
            conn.execute(
                &format!(
                    "UPDATE navigation_cursor SET history_id = {} WHERE region = 'main'",
                    i / 10 + 2
                ),
                (),
            )
            .await?;
        }
    }

    println!("CDC events received: {}", cdc_count.load(Ordering::Relaxed));

    Ok(())
}

/// Same as above but repeated in a loop for higher probability of hitting page-boundary conditions
#[tokio::test]
async fn test_dirty_pages_stress_loop() -> anyhow::Result<()> {
    for iteration in 0..5 {
        let dir = TempDir::new()?;
        let db_path = dir
            .path()
            .join(format!("dirty-pages-stress-{}.db", iteration));
        let db_path = db_path.to_str().unwrap();

        let db = turso::Builder::new_local(db_path)
            .experimental_materialized_views(true)
            .build()
            .await?;

        let conn = db.connect()?;

        conn.execute(
            "CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT NOT NULL, content TEXT DEFAULT '')",
            (),
        )
        .await?;

        conn.execute(
            "CREATE MATERIALIZED VIEW blocks_with_paths AS
             WITH RECURSIVE paths AS (
                 SELECT id, parent_id, content, '/' || id as path
                 FROM blocks WHERE parent_id LIKE 'doc:%' OR parent_id LIKE 'sentinel:%'
                 UNION ALL
                 SELECT b.id, b.parent_id, b.content, p.path || '/' || b.id
                 FROM blocks b INNER JOIN paths p ON b.parent_id = p.id
             ) SELECT * FROM paths",
            (),
        )
        .await?;

        conn.execute(
            "CREATE TABLE events (id TEXT PRIMARY KEY, status TEXT, aggregate_type TEXT)",
            (),
        )
        .await?;

        conn.execute(
            "CREATE MATERIALIZED VIEW events_view AS SELECT * FROM events WHERE status = 'confirmed'",
            (),
        )
        .await?;

        conn.set_change_callback(|_event| {})?;

        let content = "y".repeat(500);

        // Rapid alternating: inserts + DDL
        for batch in 0..20 {
            // Root block
            conn.execute(
                &format!(
                    "INSERT INTO blocks VALUES ('doc:r{}', 'sentinel:top', '{}')",
                    batch, content
                ),
                (),
            )
            .await?;

            // Watch view DDL
            conn.execute(
                &format!(
                    "CREATE MATERIALIZED VIEW IF NOT EXISTS wv_{} AS SELECT * FROM blocks WHERE parent_id = 'doc:r{}'",
                    batch, batch
                ),
                (),
            )
            .await?;

            // Child blocks (trigger recursive CTE)
            for j in 0..10 {
                conn.execute(
                    &format!(
                        "INSERT INTO blocks VALUES ('c{}_{}', 'doc:r{}', '{}')",
                        batch, j, batch, content
                    ),
                    (),
                )
                .await?;
            }

            // Grandchildren
            for j in 0..5 {
                conn.execute(
                    &format!(
                        "INSERT INTO blocks VALUES ('g{}_{}', 'c{}_0', '{}')",
                        batch, j, batch, content
                    ),
                    (),
                )
                .await?;
            }

            // Event
            conn.execute(
                &format!(
                    "INSERT INTO events VALUES ('e{}', 'confirmed', 'block')",
                    batch
                ),
                (),
            )
            .await?;
        }
    }

    Ok(())
}
