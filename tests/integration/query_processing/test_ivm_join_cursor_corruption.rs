//! Reproducer for IVM JoinOperator BTree cursor corruption.
//!
//! Bug: When chained materialized views (a matview that JOINs with another matview)
//! exist alongside other matviews with CDC callbacks active, inserts into base tables
//! cause BTree cursor corruption in the JoinOperator's commit phase.
//!
//! The panic manifests as:
//!   [PageStack::current] current_page=-1 is negative! stack_depth=0, loaded_pages=[]
//! in btree.rs, which then leaves the JoinOperator in a permanent Invalid state
//! (via the mem::replace sentinel in join_operator.rs).
//!
//! The critical ingredient is **chained matview dependencies**: a matview that JOINs
//! with another matview (e.g., `watch_view_main` JOINs with `current_focus`).
//! Without this chaining, even hundreds of inserts across multiple matviews work fine.
//!
//! Discovered in the Holon PKMS app during org file sync (startup with ~200 blocks).

use crate::common::TempDatabase;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Reproducer: chained matview dependencies (watch views JOIN with current_focus).
///
/// Schema has 3 levels of matview dependencies:
///   Level 0 (base tables): blocks, events, navigation_cursor, navigation_history
///   Level 1 (base matviews): blocks_with_paths, events_view_block, current_focus
///   Level 2 (chained): watch_view_main, watch_view_sidebar JOIN with current_focus
///
/// The panic occurs during commit_txn when a block INSERT triggers IVM cascades
/// through all 3 levels. The BTree cursor used by the JoinOperator at level 2
/// has its page stack corrupted (current_page=-1, loaded_pages=[]).
#[turso_macros::test(views)]
fn test_ivm_join_cursor_corruption_chained_watch_views(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    // Base tables
    conn.execute(
        "CREATE TABLE blocks (
            id TEXT PRIMARY KEY,
            parent_id TEXT NOT NULL,
            content TEXT DEFAULT '',
            content_type TEXT DEFAULT 'text',
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )",
    )?;
    conn.execute(
        "CREATE TABLE events (
            id TEXT PRIMARY KEY,
            event_type TEXT NOT NULL,
            aggregate_type TEXT NOT NULL,
            aggregate_id TEXT NOT NULL,
            origin TEXT NOT NULL,
            status TEXT DEFAULT 'confirmed',
            payload TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
    )?;
    conn.execute(
        "CREATE TABLE navigation_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            region TEXT NOT NULL,
            block_id TEXT
        )",
    )?;
    conn.execute(
        "CREATE TABLE navigation_cursor (
            region TEXT PRIMARY KEY,
            history_id INTEGER
        )",
    )?;
    conn.execute("INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL)")?;
    conn.execute(
        "INSERT INTO navigation_cursor (region, history_id) VALUES ('left_sidebar', NULL)",
    )?;
    conn.execute(
        "INSERT INTO navigation_cursor (region, history_id) VALUES ('right_sidebar', NULL)",
    )?;

    // Level 1 matviews (depend on base tables only)
    conn.execute(
        "CREATE MATERIALIZED VIEW blocks_with_paths AS
        WITH RECURSIVE paths AS (
            SELECT id, parent_id, content, created_at, updated_at, '/' || id as path
            FROM blocks WHERE parent_id LIKE 'holon-doc://%' OR parent_id = '__no_parent__'
            UNION ALL
            SELECT b.id, b.parent_id, b.content, b.created_at, b.updated_at, p.path || '/' || b.id
            FROM blocks b INNER JOIN paths p ON b.parent_id = p.id
        ) SELECT * FROM paths",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW events_view_block AS
        SELECT * FROM events WHERE status = 'confirmed' AND aggregate_type = 'block'",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
        SELECT nc.region, nh.block_id
        FROM navigation_cursor nc JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    // CDC callback
    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_clone = cdc_count.clone();
    conn.set_change_callback(move |event| {
        cdc_clone.fetch_add(1, Ordering::Relaxed);
        eprintln!(
            "CDC: {} changes to {}",
            event.changes.len(),
            event.relation_name
        );
    });

    let now = 1740000000000_i64;

    // Seed root blocks
    for i in 0..5 {
        conn.execute(&format!(
            "INSERT INTO blocks VALUES ('root-{}', 'holon-doc://doc-{}.org', 'Root {}', 'text', {}, {})",
            i, i, i, now, now
        ))?;
    }

    // Set up navigation (populates current_focus)
    conn.execute("INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-0')")?;
    conn.execute("UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'")?;

    // Level 2 matviews: watch views that JOIN with current_focus (CHAINED dependency)
    // This is the critical ingredient — without these, the bug does not reproduce.
    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view_main AS
        SELECT b.id, b.content FROM blocks b
        INNER JOIN current_focus cf ON b.parent_id = cf.block_id
        WHERE cf.region = 'main'",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view_sidebar AS
        SELECT b.id, b.content FROM blocks b
        INNER JOIN current_focus cf ON b.parent_id = cf.block_id
        WHERE cf.region = 'left_sidebar'",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view_all_blocks AS
        SELECT id, parent_id, content FROM blocks WHERE content_type = 'text'",
    )?;

    // Rapidly insert blocks + events while all views are active.
    // Each block INSERT triggers IVM cascades through:
    //   blocks_with_paths (recursive CTE), watch_view_all_blocks (filter),
    //   watch_view_main (chained JOIN), watch_view_sidebar (chained JOIN)
    // Each event INSERT triggers: events_view_block (filter)
    for i in 0..100 {
        let parent = format!("root-{}", i % 5);
        let block_id = format!("b-{}", i);

        conn.execute(&format!(
            "INSERT INTO blocks VALUES ('{}', '{}', 'Content {}', 'text', {}, {})",
            block_id,
            parent,
            i,
            now + i,
            now + i
        ))?;

        conn.execute(&format!(
            "INSERT INTO events VALUES ('e-{}', 'block.created', 'block', '{}', 'org', 'confirmed', '{{\"id\":\"{}\"}}', {})",
            i, block_id, block_id, now + i
        ))?;

        // Every 20 inserts, change navigation target.
        // This triggers: current_focus IVM → watch_view_main IVM → watch_view_sidebar IVM
        if i % 20 == 19 {
            let nav_target = format!("root-{}", (i / 20) % 5);
            conn.execute(&format!(
                "INSERT INTO navigation_history (region, block_id) VALUES ('main', '{}')",
                nav_target
            ))?;
            conn.execute(&format!(
                "UPDATE navigation_cursor SET history_id = {} WHERE region = 'main'",
                (i / 20) + 2
            ))?;
        }
    }

    // Create another chained watch view mid-stream (simulates Flutter startup
    // where watch views are created dynamically via query_and_watch())
    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view_right AS
        SELECT b.id, b.content FROM blocks b
        INNER JOIN current_focus cf ON b.parent_id = cf.block_id
        WHERE cf.region = 'right_sidebar'",
    )?;

    // More inserts after creating the new view
    for i in 100..200 {
        let parent = format!("root-{}", i % 5);
        let block_id = format!("b-{}", i);

        conn.execute(&format!(
            "INSERT INTO blocks VALUES ('{}', '{}', 'Content {}', 'text', {}, {})",
            block_id,
            parent,
            i,
            now + i,
            now + i
        ))?;
        conn.execute(&format!(
            "INSERT INTO events VALUES ('e-{}', 'block.created', 'block', '{}', 'org', 'confirmed', '{{}}', {})",
            i, block_id, now + i
        ))?;

        if i % 10 == 9 {
            conn.execute(&format!(
                "INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-{}')",
                i % 5
            ))?;
            conn.execute(&format!(
                "UPDATE navigation_cursor SET history_id = {} WHERE region = 'main'",
                (i / 10) + 2
            ))?;
        }
    }

    let total = cdc_count.load(Ordering::Relaxed);
    eprintln!("Total CDC callbacks: {}", total);
    assert!(total > 0);

    Ok(())
}
