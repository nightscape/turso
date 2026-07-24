//! Test: set_change_callback must survive DDL (CREATE MATERIALIZED VIEW).
//!
//! Production bug: when the CDC callback is registered before matview creation,
//! the callback stops firing after DDL. This is because the DBSP graph rebuild
//! during CREATE MATERIALIZED VIEW may lose the callback reference.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::common::TempDatabase;

/// Callback registered BEFORE matview creation must still fire after DDL.
#[turso_macros::test(views)]
fn test_cdc_callback_survives_create_matview(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, val TEXT)")?;
    conn.execute("INSERT INTO t VALUES ('a', 'hello')")?;

    // Register callback BEFORE creating matview
    let cdc_counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let counts_clone = cdc_counts.clone();
    conn.set_change_callback(move |event| {
        let mut counts = counts_clone.lock().unwrap();
        *counts.entry(event.relation_name.clone()).or_insert(0) += event.changes.len();
    });

    // DDL: create matview (this rebuilds the DBSP graph)
    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, val FROM t")?;

    // DML after DDL
    conn.execute("INSERT INTO t VALUES ('b', 'world')")?;

    let counts = cdc_counts.lock().unwrap();
    let mv_changes = counts.get("mv").copied().unwrap_or(0);
    assert!(
        mv_changes > 0,
        "CDC callback registered before CREATE MATERIALIZED VIEW must fire after INSERT. CDC: {:?}",
        *counts
    );

    Ok(())
}

/// Callback registered before MULTIPLE matview creations must still fire.
#[turso_macros::test(views)]
fn test_cdc_callback_survives_multiple_ddl(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, val TEXT)")?;

    // Register callback BEFORE any matview
    let cdc_counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let counts_clone = cdc_counts.clone();
    conn.set_change_callback(move |event| {
        let mut counts = counts_clone.lock().unwrap();
        *counts.entry(event.relation_name.clone()).or_insert(0) += event.changes.len();
    });

    // Multiple DDL operations
    conn.execute("CREATE MATERIALIZED VIEW mv1 AS SELECT id FROM t")?;
    conn.execute("CREATE MATERIALIZED VIEW mv2 AS SELECT id, val FROM t")?;

    // DML after all DDL
    conn.execute("INSERT INTO t VALUES ('a', 'hello')")?;

    let counts = cdc_counts.lock().unwrap();
    let mv1_changes = counts.get("mv1").copied().unwrap_or(0);
    let mv2_changes = counts.get("mv2").copied().unwrap_or(0);
    assert!(
        mv1_changes > 0,
        "mv1 CDC should fire after multiple DDLs. CDC: {:?}",
        *counts
    );
    assert!(
        mv2_changes > 0,
        "mv2 CDC should fire after multiple DDLs. CDC: {:?}",
        *counts
    );

    Ok(())
}

/// Callback registered before recursive CTE matview must still fire.
#[turso_macros::test(views)]
fn test_cdc_callback_survives_recursive_cte_ddl(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE block (id TEXT PRIMARY KEY, parent_id TEXT, content TEXT)")?;

    // Register callback BEFORE matview
    let cdc_counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let counts_clone = cdc_counts.clone();
    conn.set_change_callback(move |event| {
        let mut counts = counts_clone.lock().unwrap();
        *counts.entry(event.relation_name.clone()).or_insert(0) += event.changes.len();
    });

    // Recursive CTE matview
    conn.execute(
        "CREATE MATERIALIZED VIEW tree_mv AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, '/' || id as path
             FROM block WHERE parent_id IS NULL
             UNION ALL
             SELECT b.id, b.parent_id, b.content, p.path || '/' || b.id
             FROM block b INNER JOIN paths p ON b.parent_id = p.id
         ) SELECT * FROM paths",
    )?;

    // Insert data after matview creation
    conn.execute("INSERT INTO block VALUES ('root', NULL, 'Root')")?;
    conn.execute("INSERT INTO block VALUES ('child1', 'root', 'Child 1')")?;

    let counts = cdc_counts.lock().unwrap();
    let mv_changes = counts.get("tree_mv").copied().unwrap_or(0);
    assert!(
        mv_changes > 0,
        "Recursive CTE matview CDC should fire when callback was registered before DDL. CDC: {:?}",
        *counts
    );

    Ok(())
}

/// Full production scenario: callback registered early, then many matviews created
/// including chained matviews, recursive CTEs, and lazy watch views.
#[turso_macros::test(views)]
fn test_cdc_callback_full_production_scenario(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    // Step 1: Create base tables
    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY, parent_id TEXT, content TEXT DEFAULT '',
            content_type TEXT DEFAULT 'text', properties TEXT DEFAULT '{}'
        )",
    )?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;
    conn.execute(
        "CREATE TABLE nav_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT)",
    )?;
    conn.execute("CREATE TABLE nav_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;
    conn.execute("INSERT INTO nav_cursor VALUES ('main', NULL)")?;

    // Step 2: Register callback BEFORE any matview creation
    let cdc_counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let counts_clone = cdc_counts.clone();
    conn.set_change_callback(move |event| {
        let mut counts = counts_clone.lock().unwrap();
        *counts.entry(event.relation_name.clone()).or_insert(0) += event.changes.len();
    });

    // Step 3: Create navigation matviews (chained)
    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id FROM nav_cursor nc
         JOIN nav_history nh ON nc.history_id = nh.id",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW focus_roots AS
         SELECT nc.region, nh.block_id, b.id AS root_id
         FROM nav_cursor nc JOIN nav_history nh ON nc.history_id = nh.id
         JOIN block b ON b.parent_id = nh.block_id
         UNION ALL
         SELECT nc.region, nh.block_id, b.id AS root_id
         FROM nav_cursor nc JOIN nav_history nh ON nc.history_id = nh.id
         JOIN block b ON b.id = nh.block_id",
    )?;

    // Step 4: Insert data
    for i in 0..5 {
        conn.execute(&format!(
            "INSERT INTO block VALUES ('b{i}', 'doc:aaa', 'Block {i}', 'text', '{{}}')"
        ))?;
    }
    conn.execute("INSERT INTO nav_history (region, block_id) VALUES ('main', 'doc:aaa')")?;
    conn.execute("INSERT OR REPLACE INTO nav_cursor VALUES ('main', 1)")?;

    // Step 5: Create more matviews AFTER data and callback (production pattern)
    conn.execute(
        "CREATE MATERIALIZED VIEW blocks_with_paths AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, '/' || id as path
             FROM block WHERE parent_id LIKE 'doc:%'
             UNION ALL
             SELECT b.id, b.parent_id, b.content, p.path || '/' || b.id
             FROM block b INNER JOIN paths p ON b.parent_id = p.id
         ) SELECT * FROM paths",
    )?;

    // Create many structural watch matviews (IVM pressure)
    for i in 0..8 {
        conn.execute(&format!(
            "CREATE MATERIALIZED VIEW watch_struct_{i} AS
             SELECT id, content FROM block WHERE id = 'dummy-{i}' OR parent_id = 'dummy-{i}'"
        ))?;
    }

    // Create main watch_view (matview-on-matview, joins focus_roots)
    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view AS
         SELECT b.id, b.parent_id, b.content, b.content_type
         FROM focus_roots fr JOIN block b ON b.id = fr.root_id
         WHERE fr.region = 'main'",
    )?;

    // Step 6: Reset CDC counts and do DML that should trigger CDC
    {
        let mut counts = cdc_counts.lock().unwrap();
        counts.clear();
    }

    // Navigate to doc:bbb — this should trigger CDC on multiple matviews
    for i in 0..3 {
        conn.execute(&format!(
            "INSERT INTO block VALUES ('c{i}', 'doc:bbb', 'Child {i}', 'text', '{{}}')"
        ))?;
    }
    conn.execute("INSERT INTO nav_history (region, block_id) VALUES ('main', 'doc:bbb')")?;
    conn.execute("INSERT OR REPLACE INTO nav_cursor VALUES ('main', 2)")?;

    let counts = cdc_counts.lock().unwrap();

    // At minimum, focus_roots should have CDC (it depends on nav_cursor and nav_history)
    let focus_roots = counts.get("focus_roots").copied().unwrap_or(0);
    assert!(
        focus_roots > 0,
        "focus_roots CDC must fire after navigation. All CDC: {:?}",
        *counts
    );

    // watch_view (matview-on-matview) should also get CDC
    let watch_view = counts.get("watch_view").copied().unwrap_or(0);
    assert!(
        watch_view > 0,
        "watch_view CDC must fire (matview-on-matview). All CDC: {:?}",
        *counts
    );

    Ok(())
}
