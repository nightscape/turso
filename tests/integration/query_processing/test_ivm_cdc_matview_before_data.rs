//! Test: CDC for matviews that include the updated column in their projection.
//!
//! Validates that IVM CDC correctly reports changes when the updated column
//! is part of the matview's output. Matviews that don't select the changed
//! column correctly report 0 changes (the projected data doesn't change).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusqlite::types::Value;

use crate::common::{limbo_exec_rows, TempDatabase};

/// Core test: matview that includes properties column gets CDC for properties UPDATE.
/// Matviews that don't include properties correctly get 0 CDC changes.
#[turso_macros::test(views)]
fn test_cdc_properties_update_projection_matters(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY, parent_id TEXT, document_id TEXT,
            content TEXT NOT NULL DEFAULT '',
            content_type TEXT NOT NULL DEFAULT 'text',
            source_language TEXT, properties TEXT, _change_origin TEXT
        )",
    )?;
    conn.execute("CREATE INDEX idx_block_parent_id ON block(parent_id)")?;
    conn.execute("CREATE TABLE nav_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT)")?;
    conn.execute("CREATE TABLE nav_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;

    // Chained matviews (navigation)
    conn.execute("CREATE MATERIALIZED VIEW current_focus AS SELECT nc.region, nh.block_id FROM nav_cursor nc JOIN nav_history nh ON nc.history_id = nh.id")?;
    conn.execute("CREATE MATERIALIZED VIEW focus_roots AS SELECT cf.region, cf.block_id, b.id AS root_id FROM current_focus AS cf JOIN block AS b ON b.parent_id = cf.block_id UNION ALL SELECT cf.region, cf.block_id, b.id AS root_id FROM current_focus AS cf JOIN block AS b ON b.id = cf.block_id")?;

    // Recursive CTE
    conn.execute("CREATE MATERIALIZED VIEW block_with_path AS WITH RECURSIVE paths AS (SELECT id, parent_id, content, '/' || id as path FROM block WHERE parent_id LIKE 'doc:%' UNION ALL SELECT b.id, b.parent_id, b.content, p.path || '/' || b.id FROM block b INNER JOIN paths p ON b.parent_id = p.id) SELECT * FROM paths")?;

    // Matview WITHOUT properties (correctly gets 0 CDC for properties UPDATE)
    conn.execute("CREATE MATERIALIZED VIEW mv_no_props AS SELECT id, parent_id, content, _change_origin FROM block")?;

    // Matview WITH properties (MUST get CDC for properties UPDATE)
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_with_props AS SELECT id, content, properties FROM block",
    )?;

    // Matview using json_extract on properties (like holon's watch_view)
    conn.execute("CREATE MATERIALIZED VIEW mv_json_props AS SELECT id, content, json_extract(properties, '$.task_state') as task_state FROM block")?;

    // SELECT * matview (includes all columns including properties)
    conn.execute("CREATE MATERIALIZED VIEW mv_star AS SELECT * FROM block")?;

    // Insert data
    conn.execute("INSERT INTO nav_history (region, block_id) VALUES ('main', 'block:root')")?;
    conn.execute("INSERT INTO nav_cursor VALUES ('main', 1)")?;
    conn.execute("INSERT INTO block (id, parent_id, document_id, content, properties) VALUES ('block:root', 'doc:test', 'doc:test', 'Root', '{}')")?;
    for i in 0..10 {
        conn.execute(&format!(
            "INSERT INTO block (id, parent_id, document_id, content, properties) VALUES ('block:child-{}', 'block:root', 'doc:test', 'Child {}', '{{\"state\":\"TODO\"}}')",
            i, i
        ))?;
    }

    // Track CDC
    let cdc_counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let counts_clone = cdc_counts.clone();
    conn.set_change_callback(move |event| {
        let mut counts = counts_clone.lock().unwrap();
        *counts.entry(event.relation_name.clone()).or_insert(0) += event.changes.len();
    });

    // UPDATE properties column
    conn.execute("UPDATE block SET properties = json_set(COALESCE(properties, '{}'), '$.task_state', 'DOING') WHERE id = 'block:child-5'")?;

    let counts = cdc_counts.lock().unwrap();

    // mv_with_props MUST fire (includes properties)
    let props = counts.get("mv_with_props").copied().unwrap_or(0);
    assert!(
        props > 0,
        "mv_with_props CDC should fire for properties UPDATE. CDC: {:?}",
        *counts
    );

    // mv_star MUST fire (SELECT * includes properties)
    let star = counts.get("mv_star").copied().unwrap_or(0);
    assert!(
        star > 0,
        "mv_star CDC should fire for properties UPDATE. CDC: {:?}",
        *counts
    );

    // mv_no_props correctly gets 0 (properties not in projection)
    let no_props = counts.get("mv_no_props").copied().unwrap_or(0);
    assert_eq!(
        no_props, 0,
        "mv_no_props should NOT get CDC (properties not in projection). CDC: {:?}",
        *counts
    );

    // Verify matview data
    let mv = limbo_exec_rows(
        &conn,
        "SELECT json_extract(properties, '$.task_state') FROM mv_with_props WHERE id = 'block:child-5'",
    );
    assert_eq!(mv[0][0], Value::Text("DOING".to_string()));

    Ok(())
}

/// Test the complex holon pattern: recursive CTE + join to focus_roots + SELECT _v3.*
/// This is the actual production matview (watch_view_4348389a5df1b560) that needs CDC.
#[turso_macros::test(views)]
fn test_cdc_holon_focus_tree_matview(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY, parent_id TEXT, document_id TEXT,
            content TEXT NOT NULL DEFAULT '',
            content_type TEXT NOT NULL DEFAULT 'text',
            source_language TEXT, properties TEXT, _change_origin TEXT
        )",
    )?;
    conn.execute("CREATE INDEX idx_block_parent_id ON block(parent_id)")?;
    conn.execute("CREATE TABLE nav_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT)")?;
    conn.execute("CREATE TABLE nav_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;

    // Navigation chain
    conn.execute("CREATE MATERIALIZED VIEW current_focus AS SELECT nc.region, nh.block_id FROM nav_cursor nc JOIN nav_history nh ON nc.history_id = nh.id")?;
    conn.execute("CREATE MATERIALIZED VIEW focus_roots AS SELECT cf.region, cf.block_id, b.id AS root_id FROM current_focus AS cf JOIN block AS b ON b.parent_id = cf.block_id UNION ALL SELECT cf.region, cf.block_id, b.id AS root_id FROM current_focus AS cf JOIN block AS b ON b.id = cf.block_id")?;

    // The actual production matview (simplified but structurally identical)
    conn.execute(
        "CREATE MATERIALIZED VIEW watch_view AS
         WITH RECURSIVE tree AS (
             SELECT v1.id AS node_id, v1.id AS source_id, 0 AS depth,
                    CAST(v1.id AS TEXT) AS visited
             FROM block AS v1
             UNION ALL
             SELECT fk.id, tree.source_id, tree.depth + 1,
                    tree.visited || ',' || CAST(fk.id AS TEXT)
             FROM tree JOIN block fk ON fk.parent_id = tree.node_id
             WHERE tree.depth < 20
                   AND ',' || tree.visited || ',' NOT LIKE '%,' || CAST(fk.id AS TEXT) || ',%'
         )
         SELECT v3.*, json_extract(v3.properties, '$.sequence') AS sequence
         FROM focus_roots AS v0
         JOIN block AS v1 ON v1.id = v0.root_id
         JOIN tree ON tree.source_id = v1.id
         JOIN block AS v3 ON v3.id = tree.node_id
         WHERE v0.region = 'main' AND v3.content_type <> 'source'
               AND tree.depth >= 0 AND tree.depth <= 20",
    )?;

    // Insert navigation data
    conn.execute("INSERT INTO nav_history (region, block_id) VALUES ('main', 'block:root')")?;
    conn.execute("INSERT INTO nav_cursor VALUES ('main', 1)")?;

    // Insert block tree
    conn.execute("INSERT INTO block (id, parent_id, document_id, content, properties) VALUES ('block:root', 'doc:test', 'doc:test', 'Root', '{}')")?;
    for i in 0..10 {
        conn.execute(&format!(
            "INSERT INTO block (id, parent_id, document_id, content, properties) VALUES ('block:child-{}', 'block:root', 'doc:test', 'Child {}', '{{\"task_state\":\"TODO\"}}')",
            i, i
        ))?;
    }

    // Verify watch_view has data
    let count = limbo_exec_rows(&conn, "SELECT COUNT(*) FROM watch_view");
    let row_count = match &count[0][0] {
        Value::Integer(n) => *n,
        _ => 0,
    };
    assert!(
        row_count > 0,
        "watch_view should have rows after inserting block data"
    );

    // Track CDC
    let cdc_counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let counts_clone = cdc_counts.clone();
    conn.set_change_callback(move |event| {
        let mut counts = counts_clone.lock().unwrap();
        *counts.entry(event.relation_name.clone()).or_insert(0) += event.changes.len();
    });

    // UPDATE properties (the production trigger)
    conn.execute("UPDATE block SET properties = json_set(COALESCE(properties, '{}'), '$.task_state', 'DOING') WHERE id = 'block:child-5'")?;

    let counts = cdc_counts.lock().unwrap();

    // watch_view includes properties via v3.* — MUST get CDC
    let watch = counts.get("watch_view").copied().unwrap_or(0);
    assert!(
        watch > 0,
        "watch_view CDC should fire for properties UPDATE (view includes properties via v3.*). CDC: {:?}",
        *counts
    );

    Ok(())
}
