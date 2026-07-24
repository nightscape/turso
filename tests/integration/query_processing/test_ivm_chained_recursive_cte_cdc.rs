//! Test: IVM CDC propagation through chained matviews with recursive CTE.
//!
//! Reproduces a production bug from holon PKMS where the matview chain:
//!   current_focus (matview) → focus_roots (matview) → recursive CTE (matview)
//! correctly updates the matview data on base table UPDATE, but the CDC callback
//! reports changes=0 (empty delta).
//!
//! The matview is queryable with the new value, but CDC subscribers never see
//! the change — breaking reactive UI updates.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value;

fn setup_and_seed(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    // Schema — matches production holon schema exactly
    conn.execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;
    conn.execute(
        "CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, region TEXT NOT NULL, block_id TEXT, timestamp TEXT DEFAULT (datetime('now')))",
    )?;
    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY,
            parent_id TEXT,
            document_id TEXT,
            depth INTEGER NOT NULL DEFAULT 0,
            sort_key TEXT NOT NULL DEFAULT 'a0',
            content TEXT NOT NULL DEFAULT '',
            content_type TEXT NOT NULL DEFAULT 'text',
            source_language TEXT,
            source_name TEXT,
            properties TEXT,
            collapsed INTEGER NOT NULL DEFAULT 0,
            completed INTEGER NOT NULL DEFAULT 0,
            block_type TEXT NOT NULL DEFAULT 'text',
            created_at INTEGER NOT NULL DEFAULT 0,
            updated_at INTEGER NOT NULL DEFAULT 0,
            _change_origin TEXT
        )",
    )?;

    // Seed navigation
    conn.execute(
        "INSERT INTO navigation_history (id, region, block_id) VALUES (1, 'main', 'root')",
    )?;
    conn.execute("INSERT INTO navigation_cursor VALUES ('main', 1)")?;

    // Seed blocks (full production schema)
    conn.execute("INSERT INTO block (id, parent_id, content, content_type, properties) VALUES ('root', NULL, 'Root', 'text', '{}')")?;
    conn.execute(
        "INSERT INTO block (id, parent_id, content, content_type, properties) VALUES ('child1', 'root', 'Task 1', 'text', '{\"task_state\":\"TODO\",\"seq\":1}')",
    )?;
    conn.execute(
        "INSERT INTO block (id, parent_id, content, content_type, properties) VALUES ('child2', 'root', 'Task 2', 'text', '{\"task_state\":\"\",\"seq\":2}')",
    )?;
    conn.execute(
        "INSERT INTO block (id, parent_id, content, content_type, properties) VALUES ('grandchild1', 'child1', 'Subtask', 'text', '{\"task_state\":\"DOING\",\"seq\":3}')",
    )?;

    // Matview chain
    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS
         SELECT nc.region, nh.block_id
         FROM navigation_cursor nc
         JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW focus_roots AS
         SELECT cf.region, cf.block_id, b.id AS root_id
         FROM current_focus cf
         JOIN block b ON b.parent_id = cf.block_id
         UNION ALL
         SELECT cf.region, cf.block_id, b.id AS root_id
         FROM current_focus cf
         JOIN block b ON b.id = cf.block_id",
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW mv_tree AS
         WITH RECURSIVE tree AS (
             SELECT b.id AS node_id, b.id AS source_id, 0 AS depth,
                    CAST(b.id AS TEXT) AS visited
             FROM block AS b
             UNION ALL
             SELECT child.id, tree.source_id, tree.depth + 1,
                    tree.visited || ',' || CAST(child.id AS TEXT)
             FROM tree
             JOIN block child ON child.parent_id = tree.node_id
             WHERE tree.depth < 20
               AND ',' || tree.visited || ',' NOT LIKE '%,' || CAST(child.id AS TEXT) || ',%'
         )
         SELECT blk.*, json_extract(blk.properties, '$.seq') AS seq
         FROM focus_roots fr
         JOIN block root ON root.id = fr.root_id
         JOIN tree ON tree.source_id = root.id
         JOIN block blk ON blk.id = tree.node_id
         WHERE fr.region = 'main'
           AND blk.content_type <> 'source'
           AND tree.depth >= 0 AND tree.depth <= 20",
    )?;

    Ok(())
}

/// Matview data should update after properties UPDATE (queryable check)
#[turso_macros::test(views)]
fn test_chained_matview_data_updates(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_and_seed(&conn)?;

    // Verify initial state
    let initial = limbo_exec_rows(
        &conn,
        "SELECT json_extract(properties, '$.task_state') FROM mv_tree WHERE id = 'child1'",
    );
    assert_eq!(initial[0][0], Value::Text("TODO".to_string()));

    // UPDATE properties
    conn.execute(
        "UPDATE block SET properties = '{\"task_state\":\"DOING\",\"seq\":1}' WHERE id = 'child1'",
    )?;

    // Matview should reflect the new value
    let updated = limbo_exec_rows(
        &conn,
        "SELECT json_extract(properties, '$.task_state') FROM mv_tree WHERE id = 'child1'",
    );
    assert_eq!(
        updated[0][0],
        Value::Text("DOING".to_string()),
        "chained matview data should update after properties change"
    );

    Ok(())
}

/// CDC should report non-zero changes for the leaf matview after base table UPDATE
#[turso_macros::test(views)]
fn test_chained_matview_cdc_reports_changes(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_and_seed(&conn)?;

    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_clone = cdc_count.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name == "mv_tree" {
            cdc_clone.fetch_add(event.changes.len(), Ordering::SeqCst);
        }
    });

    // UPDATE properties on a child block
    conn.execute(
        "UPDATE block SET properties = '{\"task_state\":\"DOING\",\"seq\":1}' WHERE id = 'child1'",
    )?;

    let changes = cdc_count.load(Ordering::SeqCst);
    assert!(
        changes > 0,
        "CDC for chained recursive CTE matview should report >0 changes after UPDATE, got {}",
        changes
    );

    Ok(())
}

/// CDC should deliver the NEW property value, not the old one
#[turso_macros::test(views)]
fn test_chained_matview_cdc_delivers_new_value(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_and_seed(&conn)?;

    let cdc_records: Arc<std::sync::Mutex<Vec<String>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let records_clone = cdc_records.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name == "mv_tree" {
            for change in &event.changes {
                if let Some(values) = change.parse_record() {
                    records_clone.lock().unwrap().push(format!("{:?}", values));
                }
            }
        }
    });

    conn.execute(
        "UPDATE block SET properties = '{\"task_state\":\"DOING\",\"seq\":1}' WHERE id = 'child1'",
    )?;

    let records = cdc_records.lock().unwrap();
    assert!(
        !records.is_empty(),
        "CDC should deliver parsed records for chained matview UPDATE"
    );

    let has_new_value = records.iter().any(|s| s.contains("DOING"));
    assert!(
        has_new_value,
        "CDC records should contain new task_state 'DOING', got: {:?}",
        *records
    );

    Ok(())
}

/// Sequential updates should each propagate through the chain
#[turso_macros::test(views)]
fn test_chained_matview_sequential_updates(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_and_seed(&conn)?;

    let states = ["DOING", "DONE", ""];
    for state in &states {
        let props = if state.is_empty() {
            format!("{{\"task_state\":\"\",\"seq\":1}}")
        } else {
            format!("{{\"task_state\":\"{}\",\"seq\":1}}", state)
        };
        conn.execute(&format!(
            "UPDATE block SET properties = '{}' WHERE id = 'child1'",
            props
        ))?;

        let result = limbo_exec_rows(
            &conn,
            "SELECT json_extract(properties, '$.task_state') FROM mv_tree WHERE id = 'child1'",
        );
        let expected = if state.is_empty() {
            Value::Text(String::new())
        } else {
            Value::Text(state.to_string())
        };
        assert_eq!(
            result[0][0], expected,
            "matview should show '{}' after sequential update",
            state
        );
    }

    Ok(())
}

/// Grandchild updates should also propagate through recursive CTE
#[turso_macros::test(views)]
fn test_chained_matview_grandchild_update(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_and_seed(&conn)?;

    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_clone = cdc_count.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name == "mv_tree" {
            cdc_clone.fetch_add(event.changes.len(), Ordering::SeqCst);
        }
    });

    conn.execute(
        "UPDATE block SET properties = '{\"task_state\":\"DONE\",\"seq\":3}' WHERE id = 'grandchild1'",
    )?;

    let result = limbo_exec_rows(
        &conn,
        "SELECT json_extract(properties, '$.task_state') FROM mv_tree WHERE id = 'grandchild1'",
    );
    assert_eq!(
        result[0][0],
        Value::Text("DONE".to_string()),
        "grandchild update should propagate through recursive CTE matview"
    );

    let changes = cdc_count.load(Ordering::SeqCst);
    assert!(
        changes > 0,
        "CDC should report changes for grandchild update, got {}",
        changes
    );

    Ok(())
}

/// Add many concurrent matviews on the same base table (matching production density),
/// then test CDC propagation. Production has ~15 matviews on `block` including:
/// - Several `SELECT id, content, ... FROM block WHERE id = ? OR parent_id = ?`
/// - `SELECT * FROM block` (all columns)
/// - `SELECT ... FROM block WHERE content_type = 'source'`
/// - JOINs (block self-join)
/// - Recursive CTE with chained matview dependency
/// - `block_with_path` recursive CTE
fn create_production_matview_density(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    // Additional blocks for realistic data volume
    conn.execute("INSERT INTO block (id, parent_id, content, content_type, properties) VALUES ('src1', 'root', 'query', 'source', '{\"seq\":10}')")?;
    conn.execute(
        "INSERT INTO block (id, parent_id, content, content_type, properties) VALUES ('src2', 'child1', 'render', 'source', '{\"seq\":11}')",
    )?;
    conn.execute("INSERT INTO block (id, parent_id, content, content_type, properties) VALUES ('child3', 'root', 'Notes', 'text', '{\"seq\":4}')")?;
    conn.execute("INSERT INTO block (id, parent_id, content, content_type, properties) VALUES ('child4', 'root', 'Archive', 'text', '{\"seq\":5}')")?;

    // Structural matviews (watch specific block + children)
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_struct_root AS
         SELECT id, content, content_type, parent_id FROM block
         WHERE id = 'root' OR parent_id = 'root'",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_struct_child1 AS
         SELECT id, content, content_type, parent_id FROM block
         WHERE id = 'child1' OR parent_id = 'child1'",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_struct_child2 AS
         SELECT id, content, content_type, parent_id FROM block
         WHERE id = 'child2' OR parent_id = 'child2'",
    )?;

    // Source block matview
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_sources AS
         SELECT id, parent_id, content FROM block
         WHERE content_type = 'source'",
    )?;

    // All-blocks matview (with _change_origin alias, like production)
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_all_blocks AS
         SELECT id, parent_id, content, content_type, properties,
                block._change_origin AS _change_origin
         FROM block",
    )?;

    // Add ~50 more blocks for realistic data volume
    for i in 0..50 {
        conn.execute(&format!(
            "INSERT INTO block (id, parent_id, content, content_type, properties) \
             VALUES ('bulk_{i}', 'root', 'Bulk block {i}', 'text', '{{\"seq\":{}}}')",
            100 + i
        ))?;
    }

    // Text-only matview
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_text_blocks AS
         SELECT id, content FROM block WHERE content_type = 'text'",
    )?;

    // Self-join matview (like root-layout columns)
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_root_children AS
         SELECT c.*, json_extract(c.properties, '$.seq') AS seq
         FROM block p JOIN block c ON c.parent_id = p.id
         WHERE p.id = 'root' AND c.content_type = 'text'",
    )?;

    // Another recursive CTE (like block_with_path)
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_block_paths AS
         WITH RECURSIVE paths AS (
             SELECT id, parent_id, content, properties,
                    '/' || id AS path, id AS root_id
             FROM block WHERE parent_id IS NULL
             UNION ALL
             SELECT b.id, b.parent_id, b.content, b.properties,
                    p.path || '/' || b.id AS path, p.root_id
             FROM block b INNER JOIN paths p ON b.parent_id = p.id
         )
         SELECT * FROM paths",
    )?;

    // Events-style matview (different table, but adds to IVM workload)
    conn.execute(
        "CREATE TABLE events (id TEXT PRIMARY KEY, event_type TEXT, aggregate_id TEXT, payload TEXT)"
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_events AS
         SELECT * FROM events WHERE event_type = 'block.updated'",
    )?;

    // A few more structural matviews to reach ~15 total
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_struct_gc1 AS
         SELECT id, content, content_type, parent_id FROM block
         WHERE id = 'grandchild1' OR parent_id = 'grandchild1'",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_struct_child3 AS
         SELECT id, content, content_type, parent_id FROM block
         WHERE id = 'child3' OR parent_id = 'child3'",
    )?;

    // Children-of with CTE (like right sidebar)
    conn.execute(
        "CREATE MATERIALIZED VIEW mv_right_sidebar AS
         WITH children AS (
             SELECT * FROM block WHERE parent_id = 'child3' AND content_type <> 'source'
         )
         SELECT * FROM children",
    )?;

    Ok(())
}

/// CDC with production-density matviews: many matviews on same base table
#[turso_macros::test(views)]
fn test_cdc_with_many_concurrent_matviews(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_and_seed(&conn)?;
    create_production_matview_density(&conn)?;

    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_clone = cdc_count.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name == "mv_tree" {
            cdc_clone.fetch_add(event.changes.len(), Ordering::SeqCst);
        }
    });

    // UPDATE properties on a child block
    conn.execute(
        "UPDATE block SET properties = '{\"task_state\":\"DOING\",\"seq\":1}' WHERE id = 'child1'",
    )?;

    // Verify matview data is correct
    let result = limbo_exec_rows(
        &conn,
        "SELECT json_extract(properties, '$.task_state') FROM mv_tree WHERE id = 'child1'",
    );
    assert_eq!(
        result[0][0],
        Value::Text("DOING".to_string()),
        "matview data should update even with many concurrent matviews"
    );

    // THE KEY ASSERTION: CDC should report non-zero changes
    let changes = cdc_count.load(Ordering::SeqCst);
    assert!(
        changes > 0,
        "CDC for chained recursive CTE matview should report >0 changes \
         even with {} other matviews on the same base table, got 0 changes. \
         The matview data IS updated (queryable), but the CDC delta is empty.",
        12 // approximate count of additional matviews
    );

    Ok(())
}

/// Same test but with json_set instead of full replacement
#[turso_macros::test(views)]
fn test_cdc_with_many_matviews_json_set(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_and_seed(&conn)?;
    create_production_matview_density(&conn)?;

    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_clone = cdc_count.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name == "mv_tree" {
            cdc_clone.fetch_add(event.changes.len(), Ordering::SeqCst);
        }
    });

    conn.execute(
        "UPDATE block SET properties = json_set(properties, '$.task_state', 'DOING') WHERE id = 'child1'",
    )?;

    let result = limbo_exec_rows(
        &conn,
        "SELECT json_extract(properties, '$.task_state') FROM mv_tree WHERE id = 'child1'",
    );
    assert_eq!(
        result[0][0],
        Value::Text("DOING".to_string()),
        "matview data should update with json_set + many matviews"
    );

    let changes = cdc_count.load(Ordering::SeqCst);
    assert!(
        changes > 0,
        "CDC should report changes with json_set + many concurrent matviews, got {}",
        changes
    );

    Ok(())
}
