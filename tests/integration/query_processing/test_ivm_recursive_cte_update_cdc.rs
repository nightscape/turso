//! Change callbacks of a recursive view on UPDATE of a base table row.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::common::TempDatabase;
use turso_core::types::DatabaseChangeType;
use turso_core::Value;

/// One row from the CDC stream, captured in order, with the parsed payload.
#[derive(Debug, Clone)]
struct CapturedChange {
    kind: &'static str,
    values: Vec<Value>,
}

fn install_capture(
    conn: &Arc<turso_core::Connection>,
    relation: &'static str,
) -> Arc<Mutex<Vec<CapturedChange>>> {
    let captured: Arc<Mutex<Vec<CapturedChange>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = captured.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name != relation {
            return;
        }
        let mut guard = captured_clone.lock().unwrap();
        for change in &event.changes {
            let kind = match &change.change {
                DatabaseChangeType::Insert { .. } => "Insert",
                DatabaseChangeType::Delete { .. } => "Delete",
                DatabaseChangeType::Update { .. } => "Update",
            };
            let values = change.parse_record().expect("CDC record must decode");
            guard.push(CapturedChange { kind, values });
        }
    })
    .unwrap();
    captured
}

fn payload_contains_text(values: &[Value], needle: &str) -> bool {
    values.iter().any(|v| match v {
        Value::Text(t) => t.as_str().contains(needle),
        _ => false,
    })
}

/// The view is created on an empty table; Holon creates its views before it
/// loads data.
#[turso_macros::test(views)]
fn test_recursive_cte_update_emits_cdc_matview_before_data(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY,
            content TEXT DEFAULT '',
            parent_id TEXT DEFAULT ''
        )",
    )?;

    // Create matview FIRST (empty table) — matches holon order
    conn.execute(
        "CREATE MATERIALIZED VIEW descendants_view AS
         WITH RECURSIVE _vl AS (
             SELECT id AS node_id, id AS source_id, 0 AS depth FROM block
             UNION ALL
             SELECT b.id, _vl.source_id, _vl.depth + 1
             FROM _vl JOIN block b ON b.parent_id = _vl.node_id
             WHERE _vl.depth < 20
         )
         SELECT b.* FROM _vl JOIN block b ON b.id = _vl.node_id
         WHERE _vl.source_id = 'doc' AND _vl.depth >= 0 AND _vl.depth <= 20",
    )?;

    // Register CDC callback
    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_clone = cdc_count.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name == "descendants_view" {
            cdc_clone.fetch_add(event.changes.len(), Ordering::SeqCst);
        }
    })
    .unwrap();

    // Seed data AFTER matview creation
    conn.execute("INSERT INTO block VALUES ('doc', 'Doc', '')")?;
    conn.execute("INSERT INTO block VALUES ('child_1', 'one two three', 'doc')")?;
    conn.execute("INSERT INTO block VALUES ('child_2', 'tail', 'doc')")?;

    // Reset CDC count (ignore seed events)
    cdc_count.store(0, Ordering::SeqCst);

    // UPDATE: truncate child_1 content
    conn.execute("UPDATE block SET content = 'one' WHERE id = 'child_1'")?;

    let changes = cdc_count.load(Ordering::SeqCst);
    assert!(
        changes > 0,
        "CDC for standalone recursive CTE matview should report >0 changes after UPDATE, got {changes}"
    );

    Ok(())
}

/// The view is created on an empty table and then filled incrementally.
#[turso_macros::test(views)]
fn test_recursive_cte_update_alone_matview_before_data(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY,
            content TEXT DEFAULT '',
            parent_id TEXT DEFAULT ''
        )",
    )?;

    // Matview FIRST (empty table)
    conn.execute(
        "CREATE MATERIALIZED VIEW descendants_view AS
         WITH RECURSIVE _vl AS (
             SELECT id AS node_id, id AS source_id, 0 AS depth FROM block
             UNION ALL
             SELECT b.id, _vl.source_id, _vl.depth + 1
             FROM _vl JOIN block b ON b.parent_id = _vl.node_id
             WHERE _vl.depth < 20
         )
         SELECT b.* FROM _vl JOIN block b ON b.id = _vl.node_id
         WHERE _vl.source_id = 'doc' AND _vl.depth >= 0 AND _vl.depth <= 20",
    )?;

    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_clone = cdc_count.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name == "descendants_view" {
            cdc_clone.fetch_add(event.changes.len(), Ordering::SeqCst);
        }
    })
    .unwrap();

    // Seed
    conn.execute("INSERT INTO block VALUES ('doc', 'Doc', '')")?;
    conn.execute("INSERT INTO block VALUES ('child_1', 'one two three', 'doc')")?;

    // Reset
    cdc_count.store(0, Ordering::SeqCst);
    conn.execute("UPDATE block SET content = 'one' WHERE id = 'child_1'")?;

    let changes = cdc_count.load(Ordering::SeqCst);
    assert!(
        changes > 0,
        "CDC for standalone recursive CTE matview after UPDATE should have >0 changes, got {changes}"
    );

    Ok(())
}

/// The view is created over existing rows.
#[turso_macros::test(views)]
fn test_recursive_cte_update_emits_cdc_data_before_matview(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY,
            content TEXT DEFAULT '',
            parent_id TEXT DEFAULT ''
        )",
    )?;

    conn.execute("INSERT INTO block VALUES ('doc', 'Doc', '')")?;
    conn.execute("INSERT INTO block VALUES ('child_1', 'one two three', 'doc')")?;
    conn.execute("INSERT INTO block VALUES ('child_2', 'tail', 'doc')")?;

    // Matview AFTER data (from-scratch population)
    conn.execute(
        "CREATE MATERIALIZED VIEW descendants_view AS
         WITH RECURSIVE _vl AS (
             SELECT id AS node_id, id AS source_id, 0 AS depth FROM block
             UNION ALL
             SELECT b.id, _vl.source_id, _vl.depth + 1
             FROM _vl JOIN block b ON b.parent_id = _vl.node_id
             WHERE _vl.depth < 20
         )
         SELECT b.* FROM _vl JOIN block b ON b.id = _vl.node_id
         WHERE _vl.source_id = 'doc' AND _vl.depth >= 0 AND _vl.depth <= 20",
    )?;

    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_clone = cdc_count.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name == "descendants_view" {
            cdc_clone.fetch_add(event.changes.len(), Ordering::SeqCst);
        }
    })
    .unwrap();

    // Reset
    cdc_count.store(0, Ordering::SeqCst);

    conn.execute("UPDATE block SET content = 'one' WHERE id = 'child_1'")?;

    let changes = cdc_count.load(Ordering::SeqCst);
    assert!(
        changes > 0,
        "CDC for data-before-matview UPDATE should have >0 changes, got {changes}"
    );

    Ok(())
}

/// One UPDATE reaches the callback as the delete of the old row, then the
/// insert of the new row: consumers fold that pair into an update.
#[turso_macros::test(views)]
fn test_recursive_cte_update_emits_delete_before_insert(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY,
            content TEXT DEFAULT '',
            parent_id TEXT DEFAULT ''
        )",
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW descendants_view AS
         WITH RECURSIVE _vl AS (
             SELECT id AS node_id, id AS source_id, 0 AS depth FROM block
             UNION ALL
             SELECT b.id, _vl.source_id, _vl.depth + 1
             FROM _vl JOIN block b ON b.parent_id = _vl.node_id
             WHERE _vl.depth < 20
         )
         SELECT b.* FROM _vl JOIN block b ON b.id = _vl.node_id
         WHERE _vl.source_id = 'doc' AND _vl.depth >= 0 AND _vl.depth <= 20",
    )?;

    let captured = install_capture(&conn, "descendants_view");

    conn.execute("INSERT INTO block VALUES ('doc', 'Doc', '')")?;
    conn.execute("INSERT INTO block VALUES ('child_1', 'old content', 'doc')")?;

    // Drop the seed events; we only care about the UPDATE's CDC.
    captured.lock().unwrap().clear();

    conn.execute("UPDATE block SET content = 'new content' WHERE id = 'child_1'")?;

    let events = captured.lock().unwrap().clone();
    assert!(
        !events.is_empty(),
        "UPDATE on a base table feeding a recursive matview must emit CDC; got nothing"
    );

    let child_events: Vec<&CapturedChange> = events
        .iter()
        .filter(|e| payload_contains_text(&e.values, "child_1"))
        .collect();

    assert_eq!(
        child_events.len(),
        2,
        "child_1 UPDATE should produce exactly one Delete(old) + one Insert(new); \
         got {} events: {:#?}",
        child_events.len(),
        child_events
    );

    let first = child_events[0];
    let second = child_events[1];

    assert_eq!(
        first.kind, "Delete",
        "Delete(old) must precede Insert(new) for matview UPDATE; got {first:#?} then {second:#?}. \
         Order-aware coalescers (DELETE→INSERT folds to UPDATE) depend on this."
    );
    assert_eq!(
        second.kind, "Insert",
        "the second event for child_1 must be the Insert(new); got {second:#?}"
    );

    assert!(
        payload_contains_text(&first.values, "old content"),
        "Delete event payload must carry the OLD row content ('old content'); \
         got values {:#?}",
        first.values
    );
    assert!(
        payload_contains_text(&second.values, "new content"),
        "Insert event payload must carry the NEW row content ('new content'); \
         got values {:#?}",
        second.values
    );

    Ok(())
}

/// Delete before insert holds for every UPDATE of a sequence, on rows at
/// different depths.
#[turso_macros::test(views)]
fn test_recursive_cte_update_order_stable_across_iterations(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute(
        "CREATE TABLE block (
            id TEXT PRIMARY KEY,
            content TEXT DEFAULT '',
            parent_id TEXT DEFAULT ''
        )",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW descendants_view AS
         WITH RECURSIVE _vl AS (
             SELECT id AS node_id, id AS source_id, 0 AS depth FROM block
             UNION ALL
             SELECT b.id, _vl.source_id, _vl.depth + 1
             FROM _vl JOIN block b ON b.parent_id = _vl.node_id
             WHERE _vl.depth < 20
         )
         SELECT b.* FROM _vl JOIN block b ON b.id = _vl.node_id
         WHERE _vl.source_id = 'doc' AND _vl.depth >= 0 AND _vl.depth <= 20",
    )?;

    let captured = install_capture(&conn, "descendants_view");

    conn.execute("INSERT INTO block VALUES ('doc', 'Doc', '')")?;
    conn.execute("INSERT INTO block VALUES ('child_1', 'v0', 'doc')")?;
    conn.execute("INSERT INTO block VALUES ('child_2', 'v0', 'doc')")?;
    conn.execute("INSERT INTO block VALUES ('grand', 'v0', 'child_1')")?;
    captured.lock().unwrap().clear();

    for i in 0..25u32 {
        let target = match i % 3 {
            0 => "child_1",
            1 => "child_2",
            _ => "grand",
        };
        let new_value = format!("v{}", i + 1);
        conn.execute(format!(
            "UPDATE block SET content = '{new_value}' WHERE id = '{target}'"
        ))?;

        let events = captured.lock().unwrap().clone();
        captured.lock().unwrap().clear();

        let by_target: Vec<&CapturedChange> = events
            .iter()
            .filter(|e| payload_contains_text(&e.values, target))
            .collect();

        let first_delete_pos = by_target.iter().position(|e| e.kind == "Delete");
        let first_insert_pos = by_target.iter().position(|e| e.kind == "Insert");

        let (Some(d), Some(i_pos)) = (first_delete_pos, first_insert_pos) else {
            panic!(
                "iteration {i}: target {target} should have both a Delete and an Insert in CDC; got events {by_target:#?}"
            );
        };
        assert!(
            d < i_pos,
            "iteration {i}: target {target} should see Delete before Insert in CDC; \
             got delete at {d}, insert at {i_pos}, events: {by_target:#?}"
        );
    }

    Ok(())
}
