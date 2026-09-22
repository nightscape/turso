//! Test for IVM nested JOIN with materialized views
//!
//! These tests verify that chained materialized views work correctly,
//! including with JOINs and CDC callbacks.

use crate::common::TempDatabase;

/// Test that creating a matview that JOINs with another matview works
#[turso_macros::test(views)]
fn test_nested_matview_join_with_cdc_callback(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    // Setup base tables
    conn.execute("CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT, content TEXT)")?;
    conn.execute(
        "CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT, block_id TEXT)",
    )?;
    conn.execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;
    conn.execute("INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL)")?;

    // Add blocks data
    // Add blocks data - these will match when current_focus is populated
    // The parent_id 'root-block' will match the block_id in navigation_history
    conn.execute("INSERT INTO blocks VALUES ('child-1', 'root-block', 'Content 1')")?;
    conn.execute("INSERT INTO blocks VALUES ('child-2', 'root-block', 'Content 2')")?;
    conn.execute("INSERT INTO blocks VALUES ('child-3', 'root-block', 'Content 3')")?;

    // Create first matview with JOIN
    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    // Create second matview that JOINs with the first (nested)
    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view AS
         SELECT blocks.id, blocks.content
         FROM blocks
         INNER JOIN current_focus cf ON blocks.parent_id = cf.block_id
         WHERE cf.region = 'main'",
    )?;

    // Set up CDC callback
    // Set up CDC callback - this is the key trigger for the bug
    conn.set_change_callback(|event| {
        println!(
            "CDC callback: {} changes to {}",
            event.changes.len(),
            event.relation_name
        );
    });

    // Insert into navigation_history
    // Insert into navigation_history - the block_id matches blocks.parent_id
    conn.execute(
        "INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-block')",
    )?;

    // Update cursor to point to the new history entry
    // This triggers cascading IVM updates:
    // 1. current_focus updates (because navigation_cursor changed)
    // 2. watch_view updates (because it JOINs with current_focus AND blocks match)
    // 3. During this cascade, JoinOperator::commit is called re-entrantly
    // 4. BUG: cursor state corrupted -> PANIC
    conn.execute("UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'")?;

    Ok(())
}

/// Test nested matview join without CDC callback
#[turso_macros::test(views)]
fn test_nested_matview_join_without_cdc_no_panic(tmp_db: TempDatabase) -> anyhow::Result<()> {
    // Same test but WITHOUT CDC callback - should not panic
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT, content TEXT)")?;
    conn.execute(
        "CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT, block_id TEXT)",
    )?;
    conn.execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;
    conn.execute("INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL)")?;

    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view AS
         SELECT blocks.id, blocks.content
         FROM blocks
         INNER JOIN current_focus cf ON blocks.parent_id = cf.block_id
         WHERE cf.region = 'main'",
    )?;

    // NO CDC callback set

    conn.execute(
        "INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-block')",
    )?;
    conn.execute("UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'")?;

    Ok(())
}

/// Reproducer from holon PKMS app: recursive CTE matview + JOIN matview + watch matviews + CDC.
/// The combination of a recursive CTE matview (blocks_with_paths) alongside chained JOIN
/// matviews (current_focus -> watch_view) with CDC callbacks triggers cursor corruption
/// during cascading IVM updates.
#[turso_macros::test(views)]
fn test_holon_full_schema_with_recursive_cte_and_cdc(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    // Base tables (mirrors holon schema)
    conn.execute("CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT, content TEXT)")?;
    conn.execute(
        "CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT, block_id TEXT)",
    )?;
    conn.execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;
    conn.execute("INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL)")?;

    // Recursive CTE matview — this is the key ingredient missing from simpler tests
    conn.execute(
        "CREATE MATERIALIZED VIEW blocks_with_paths AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, '/' || id as path
             FROM blocks
             WHERE parent_id LIKE 'doc:%'
                OR parent_id = '__no_parent__'
             UNION ALL
             SELECT b.id, b.parent_id, b.content, p.path || '/' || b.id as path
             FROM blocks b
             INNER JOIN paths p ON b.parent_id = p.id
         )
         SELECT * FROM paths",
    )?;

    // JOIN matview on base tables
    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    // Chained matview: JOIN with current_focus
    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view AS
         SELECT blocks.id, blocks.content
         FROM blocks
         INNER JOIN current_focus cf ON blocks.parent_id = cf.block_id
         WHERE cf.region = 'main'",
    )?;

    // CDC callback — triggers re-entrancy during cascading IVM updates
    conn.set_change_callback(|event| {
        println!(
            "CDC callback: {} changes to {}",
            event.changes.len(),
            event.relation_name
        );
    });

    // Insert blocks that will be picked up by blocks_with_paths
    conn.execute("INSERT INTO blocks VALUES ('root-1', 'doc:test.org', 'Root content')")?;
    conn.execute("INSERT INTO blocks VALUES ('child-1', 'root-1', 'Child content')")?;
    conn.execute("INSERT INTO blocks VALUES ('child-2', 'root-1', 'Child 2')")?;

    // Navigate: insert history + update cursor.
    // This triggers cascading IVM:
    //   1. blocks_with_paths updates (blocks changed)
    //   2. current_focus updates (navigation_cursor changed)
    //   3. watch_view updates (current_focus changed)
    //   4. CDC fires during cascade
    // The combination can corrupt BTree cursors during re-entrant commit.
    conn.execute("INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-1')")?;
    conn.execute("UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'")?;

    // Insert more blocks after navigation is active — stresses the cascade further
    conn.execute("INSERT INTO blocks VALUES ('grandchild-1', 'child-1', 'Grandchild')")?;
    conn.execute("INSERT INTO blocks VALUES ('child-3', 'root-1', 'Child 3')")?;
    conn.execute("INSERT INTO blocks VALUES ('child-4', 'root-1', 'Child 4')")?;

    Ok(())
}

/// Test with multiple watch views - closer to the real app scenario
#[turso_macros::test(views)]
fn test_multiple_nested_matviews_with_cdc(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    // Setup base tables
    conn.execute("CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT, content TEXT)")?;
    conn.execute(
        "CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT, block_id TEXT)",
    )?;
    conn.execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;

    // Multiple regions
    conn.execute("INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL)")?;
    conn.execute(
        "INSERT INTO navigation_cursor (region, history_id) VALUES ('left_sidebar', NULL)",
    )?;
    conn.execute(
        "INSERT INTO navigation_cursor (region, history_id) VALUES ('right_sidebar', NULL)",
    )?;

    // Add blocks data
    conn.execute("INSERT INTO blocks VALUES ('child-1', 'root-block', 'Content 1')")?;
    conn.execute("INSERT INTO blocks VALUES ('child-2', 'root-block', 'Content 2')")?;
    conn.execute("INSERT INTO blocks VALUES ('child-3', 'other-block', 'Content 3')")?;

    // Create first matview with JOIN on base tables
    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    // Create matview that references current_focus
    // Create MULTIPLE watch views that all JOIN with current_focus
    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view_1 AS
         SELECT blocks.id, blocks.content
         FROM blocks
         INNER JOIN current_focus cf ON blocks.parent_id = cf.block_id
         WHERE cf.region = 'main'",
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view_2 AS
        SELECT blocks.id, blocks.content
        FROM blocks
        INNER JOIN current_focus cf ON blocks.parent_id = cf.block_id
        WHERE cf.region = 'left_sidebar'",
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view_3 AS
        SELECT blocks.id, blocks.content
        FROM blocks
        INNER JOIN current_focus cf ON blocks.parent_id = cf.block_id
        WHERE cf.region = 'right_sidebar'",
    )?;

    // Set up CDC callback
    conn.set_change_callback(|event| {
        println!(
            "CDC callback: {} changes to {}",
            event.changes.len(),
            event.relation_name
        );
    });

    // Insert navigation history for multiple regions
    conn.execute(
        "INSERT INTO navigation_history (region, block_id) VALUES ('main', 'root-block')",
    )?;
    conn.execute(
        "INSERT INTO navigation_history (region, block_id) VALUES ('left_sidebar', 'other-block')",
    )?;

    // Update multiple cursors - this should trigger more complex IVM cascades
    conn.execute("UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main'")?;
    conn.execute("UPDATE navigation_cursor SET history_id = 2 WHERE region = 'left_sidebar'")?;

    Ok(())
}
