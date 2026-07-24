//! Test for IVM BTree overflow cell assertion
//!
//! Reproduces the "multiple overflow cells can only occur when a parent overflows
//! during balancing" panic in btree.rs during rapid inserts into tables with
//! JOIN materialized views and CDC callbacks.
//!
//! This matches the production scenario: single-level matviews (not chained),
//! with rapid concurrent inserts triggering IVM updates.

use crate::common::TempDatabase;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Rapid inserts into a table with a JOIN matview + CDC causes BTree overflow assertion
#[turso_macros::test(views)]
fn test_rapid_inserts_with_join_matview_and_cdc(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    // Schema matching production: navigation tables + blocks
    conn.execute("CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT, timestamp TEXT DEFAULT (datetime('now')))")?;
    conn.execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER REFERENCES navigation_history(id))")?;
    conn.execute("INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL)")?;

    // Single JOIN matview on base tables (NOT chained)
    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id, nh.timestamp
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    // Blocks table
    conn.execute("CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT NOT NULL, content TEXT DEFAULT '')")?;

    // Set up CDC callback (required trigger for the bug)
    conn.set_change_callback(|event| {
        let _ = event; // just having the callback registered is enough
    });

    // Populate navigation so the JOIN matview has data
    conn.execute("INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-1')")?;
    conn.execute("UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'")?;

    // Rapid inserts into blocks - this triggers IVM updates on any matview
    // that references blocks. With CDC active, the BTree overflow assertion fires.
    for i in 0..200 {
        conn.execute(&format!(
            "INSERT INTO blocks (id, parent_id, content) VALUES ('block-{}', 'holon-doc://test.org', 'Content {}')",
            i, i
        ))?;
    }

    // Also insert child blocks to create deeper trees
    for i in 0..100 {
        conn.execute(&format!(
            "INSERT INTO blocks (id, parent_id, content) VALUES ('child-{}', 'block-{}', 'Child content {}')",
            i, i % 50, i
        ))?;
    }

    Ok(())
}

/// Rapid inserts with recursive CTE matview (blocks_with_paths) + JOIN matview + CDC
/// This matches the exact production schema that triggers the BTree overflow.
#[turso_macros::test(views)]
fn test_rapid_inserts_with_recursive_cte_matview_and_cdc(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    // Blocks table (production schema)
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
    )?;

    // Recursive CTE matview - this is the heavy IVM workload
    conn.execute(
        "CREATE MATERIALIZED VIEW blocks_with_paths AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, '/' || id as path
             FROM blocks
             WHERE parent_id LIKE 'holon-doc://%'
                OR parent_id = '__no_parent__'
             UNION ALL
             SELECT b.id, b.parent_id, b.content, p.path || '/' || b.id as path
             FROM blocks b
             INNER JOIN paths p ON b.parent_id = p.id
         )
         SELECT * FROM paths",
    )?;

    // Navigation tables
    conn.execute("CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT)")?;
    conn.execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;
    conn.execute("INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL)")?;

    // JOIN matview on base tables
    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    // CDC callback
    conn.set_change_callback(|event| {
        let _ = event;
    });

    // Set up navigation
    conn.execute("INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-0')")?;
    conn.execute("UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'")?;

    // Rapid inserts: root blocks (trigger recursive CTE base case)
    for i in 0..50 {
        conn.execute(&format!(
            "INSERT INTO blocks (id, parent_id, content) VALUES ('root-{}', 'holon-doc://test.org', 'Root {}')",
            i, i
        ))?;
    }

    // Child blocks (trigger recursive CTE recursive case via INNER JOIN)
    for i in 0..200 {
        conn.execute(&format!(
            "INSERT INTO blocks (id, parent_id, content) VALUES ('child-{}', 'root-{}', 'Child {}')",
            i, i % 50, i
        ))?;
    }

    // Grandchild blocks (deeper recursion)
    for i in 0..100 {
        conn.execute(&format!(
            "INSERT INTO blocks (id, parent_id, content) VALUES ('grandchild-{}', 'child-{}', 'GC {}')",
            i, i % 100, i
        ))?;
    }

    Ok(())
}

/// Two connections: one for writes, one for CDC - matches production architecture
#[turso_macros::test(views)]
fn test_two_conn_rapid_inserts_with_recursive_cte_and_cdc(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn1 = tmp_db.connect_limbo();

    // Create schema on conn1
    conn1.execute(
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
    )?;

    conn1.execute(
        "CREATE MATERIALIZED VIEW blocks_with_paths AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, '/' || id as path
             FROM blocks
             WHERE parent_id LIKE 'holon-doc://%'
                OR parent_id = '__no_parent__'
             UNION ALL
             SELECT b.id, b.parent_id, b.content, p.path || '/' || b.id as path
             FROM blocks b
             INNER JOIN paths p ON b.parent_id = p.id
         )
         SELECT * FROM paths",
    )?;

    conn1.execute("CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT)")?;
    conn1
        .execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;
    conn1.execute("INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL)")?;

    conn1.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    // Second connection for CDC subscription (matches production pattern)
    let conn2 = tmp_db.connect_limbo();
    conn2.set_change_callback(|event| {
        let _ = event;
    });

    // Set up navigation via conn1
    conn1.execute("INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-0')")?;
    conn1.execute("UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'")?;

    // Rapid inserts via conn1 while conn2 has CDC active
    for i in 0..50 {
        conn1.execute(&format!(
            "INSERT INTO blocks (id, parent_id, content) VALUES ('root-{}', 'holon-doc://test.org', 'Root {}')",
            i, i
        ))?;
    }
    for i in 0..200 {
        conn1.execute(&format!(
            "INSERT INTO blocks (id, parent_id, content) VALUES ('child-{}', 'root-{}', 'Child {}')",
            i, i % 50, i
        ))?;
    }
    for i in 0..100 {
        conn1.execute(&format!(
            "INSERT INTO blocks (id, parent_id, content) VALUES ('gc-{}', 'child-{}', 'GC {}')",
            i,
            i % 100,
            i
        ))?;
    }

    Ok(())
}

/// Async SDK test: concurrent inserts via multiple connections with CDC + IVM matviews.
/// This matches the production architecture where writes and CDC happen concurrently.
#[tokio::test]
async fn test_async_concurrent_inserts_with_ivm_and_cdc() -> anyhow::Result<()> {
    let db_path = "/tmp/turso-btree-overflow-async-test.db";
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));

    let db = turso::Builder::new_local(db_path)
        .experimental_materialized_views(true)
        .build()
        .await?;

    let conn = db.connect()?;

    // Production schema
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

    conn.execute(
        "CREATE MATERIALIZED VIEW blocks_with_paths AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, '/' || id as path
             FROM blocks
             WHERE parent_id LIKE 'holon-doc://%'
                OR parent_id = '__no_parent__'
             UNION ALL
             SELECT b.id, b.parent_id, b.content, p.path || '/' || b.id as path
             FROM blocks b
             INNER JOIN paths p ON b.parent_id = p.id
         )
         SELECT * FROM paths",
        (),
    )
    .await?;

    conn.execute("CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT)", ()).await?;
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

    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
        (),
    )
    .await?;

    // Events table with matview
    conn.execute(
        "CREATE TABLE events (
            id TEXT PRIMARY KEY,
            event_type TEXT NOT NULL,
            entity_id TEXT,
            entity_type TEXT,
            payload TEXT DEFAULT '{}',
            processed_by_cache INTEGER DEFAULT 0,
            created_at TEXT DEFAULT (datetime('now'))
        )",
        (),
    )
    .await?;

    conn.execute(
        "CREATE MATERIALIZED VIEW events_view_block AS
         SELECT * FROM events WHERE entity_type = 'block'",
        (),
    )
    .await?;

    // Set up CDC on a second connection (production pattern)
    let conn2 = db.connect()?;
    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_count_clone = cdc_count.clone();
    conn2.set_change_callback(move |event| {
        cdc_count_clone.fetch_add(event.changes.len(), Ordering::Relaxed);
    })?;

    // Set up navigation
    conn.execute(
        "INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-0')",
        (),
    )
    .await?;
    conn.execute(
        "UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'",
        (),
    )
    .await?;

    let large_content = "x".repeat(500);

    // Rapid inserts - roots
    for i in 0..100 {
        conn.execute(
            &format!(
                "INSERT INTO blocks (id, parent_id, content) VALUES ('root-{}', 'holon-doc://doc-{}.org', '{}')",
                i, i % 5, large_content
            ),
            (),
        ).await?;
        // Event for each insert (like production)
        conn.execute(
            &format!(
                "INSERT INTO events (id, event_type, entity_id, entity_type) VALUES ('evt-r-{}', 'block.created', 'root-{}', 'block')",
                i, i
            ),
            (),
        ).await?;
    }

    // Children
    for i in 0..300 {
        conn.execute(
            &format!(
                "INSERT INTO blocks (id, parent_id, content) VALUES ('child-{}', 'root-{}', '{}')",
                i,
                i % 100,
                large_content
            ),
            (),
        )
        .await?;
    }

    // Grandchildren
    for i in 0..200 {
        conn.execute(
            &format!(
                "INSERT INTO blocks (id, parent_id, content) VALUES ('gc-{}', 'child-{}', '{}')",
                i,
                i % 300,
                large_content
            ),
            (),
        )
        .await?;
    }

    // Interleave navigation changes
    for i in 1..20 {
        conn.execute(
            &format!(
                "INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-{}')",
                i
            ),
            (),
        )
        .await?;
        conn.execute(
            &format!(
                "UPDATE navigation_cursor SET history_id = {} WHERE region = 'main'",
                i + 1
            ),
            (),
        )
        .await?;
    }

    println!("CDC events received: {}", cdc_count.load(Ordering::Relaxed));

    // Cleanup
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    let _ = std::fs::remove_file(format!("{}-shm", db_path));

    Ok(())
}

/// High volume with large payloads to stress BTree page splits during IVM
#[turso_macros::test(views)]
fn test_large_payload_inserts_with_recursive_cte_and_cdc(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

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
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW blocks_with_paths AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, content_type, source_language,
                    source_name, properties, created_at, updated_at,
                    '/' || id as path
             FROM blocks
             WHERE parent_id LIKE 'holon-doc://%'
                OR parent_id = '__no_parent__'
             UNION ALL
             SELECT b.id, b.parent_id, b.content, b.content_type, b.source_language,
                    b.source_name, b.properties, b.created_at, b.updated_at,
                    p.path || '/' || b.id as path
             FROM blocks b
             INNER JOIN paths p ON b.parent_id = p.id
         )
         SELECT * FROM paths",
    )?;

    conn.execute("CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT)")?;
    conn.execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;
    conn.execute("INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL)")?;

    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    // Events table (production also has this with matviews)
    conn.execute(
        "CREATE TABLE events (
            id TEXT PRIMARY KEY,
            event_type TEXT NOT NULL,
            entity_id TEXT,
            entity_type TEXT,
            payload TEXT DEFAULT '{}',
            processed_by_cache INTEGER DEFAULT 0,
            created_at TEXT DEFAULT (datetime('now'))
        )",
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW events_view_block AS
         SELECT * FROM events WHERE entity_type = 'block'",
    )?;

    conn.set_change_callback(|event| {
        let _ = event;
    });

    // Set up navigation
    conn.execute("INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-0')")?;
    conn.execute("UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'")?;

    // Large content to stress BTree pages
    let large_content = "x".repeat(500);
    let large_props = format!("{{\"key\": \"{}\"}}", "v".repeat(200));

    // Insert roots with large payloads
    for i in 0..100 {
        conn.execute(&format!(
            "INSERT INTO blocks (id, parent_id, content, properties) VALUES ('root-{}', 'holon-doc://doc-{}.org', '{}', '{}')",
            i, i % 5, large_content, large_props
        ))?;
        // Also insert event for each block (like production)
        conn.execute(&format!(
            "INSERT INTO events (id, event_type, entity_id, entity_type, payload) VALUES ('evt-root-{}', 'block.created', 'root-{}', 'block', '{{}}')",
            i, i
        ))?;
    }

    // Children with large payloads - triggers recursive CTE IVM
    for i in 0..500 {
        conn.execute(&format!(
            "INSERT INTO blocks (id, parent_id, content, properties) VALUES ('child-{}', 'root-{}', '{}', '{}')",
            i, i % 100, large_content, large_props
        ))?;
    }

    // Grandchildren - deeper recursion
    for i in 0..200 {
        conn.execute(&format!(
            "INSERT INTO blocks (id, parent_id, content) VALUES ('gc-{}', 'child-{}', '{}')",
            i,
            i % 500,
            large_content
        ))?;
    }

    Ok(())
}
