//! Change callbacks of a recursive view that joins a chain of views and is
//! created before any data exists.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusqlite::types::Value;

use crate::common::{limbo_exec_rows, TempDatabase};

/// An UPDATE of a column that the view selects through `v3.*` reaches the
/// callback.
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
        conn.execute(format!(
            "INSERT INTO block (id, parent_id, document_id, content, properties) VALUES ('block:child-{i}', 'block:root', 'doc:test', 'Child {i}', '{{\"task_state\":\"TODO\"}}')"
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
    })
    .unwrap();

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
