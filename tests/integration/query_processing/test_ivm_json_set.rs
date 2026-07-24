//! Test: IVM matview propagation for json_set() in UPDATE.
//!
//! When UPDATE uses `json_set()` to modify a JSON column, IVM should deliver
//! the NEW (post-json_set) value via both matview reads and CDC events.
//!
//! Reported by holon PKMS: task state toggle clicks appeared to have no effect
//! because IVM delivered stale `properties` JSON to CDC subscribers after
//! `UPDATE block SET properties = json_set(properties, '$.task_state', 'DOING')`.
//!
//! Workaround: full JSON replacement (`SET properties = '{...}'`) instead of
//! `json_set()`. This test documents the bug so it can be fixed in IVM.

use std::sync::{Arc, Mutex};

use crate::common::{limbo_exec_rows, TempDatabase};
use rusqlite::types::Value;

/// json_set() UPDATE should propagate new value to matview
#[turso_macros::test(views)]
fn test_json_set_propagates_to_matview(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, props TEXT NOT NULL DEFAULT '{}')")?;
    conn.execute("INSERT INTO t VALUES ('a', '{\"state\":\"TODO\",\"seq\":1}')")?;

    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, props FROM t")?;

    // UPDATE using json_set
    conn.execute("UPDATE t SET props = json_set(props, '$.state', 'DOING') WHERE id = 'a'")?;

    // Verify base table
    let base = limbo_exec_rows(
        &conn,
        "SELECT json_extract(props, '$.state') FROM t WHERE id = 'a'",
    );
    assert_eq!(
        base[0][0],
        Value::Text("DOING".to_string()),
        "base table should have new state"
    );

    // Verify matview — THIS IS THE BUG: matview may show 'TODO' (stale)
    let mv = limbo_exec_rows(
        &conn,
        "SELECT json_extract(props, '$.state') FROM mv WHERE id = 'a'",
    );
    assert_eq!(
        mv[0][0],
        Value::Text("DOING".to_string()),
        "matview should reflect json_set() result but shows stale data"
    );

    Ok(())
}

/// json_set() should preserve other JSON fields in matview
#[turso_macros::test(views)]
fn test_json_set_preserves_other_fields(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, props TEXT NOT NULL DEFAULT '{}')")?;
    conn.execute(
        "INSERT INTO t VALUES ('x', '{\"state\":\"TODO\",\"priority\":3,\"tags\":\"work\"}')",
    )?;

    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, props FROM t")?;

    conn.execute("UPDATE t SET props = json_set(props, '$.state', 'DONE') WHERE id = 'x'")?;

    let mv = limbo_exec_rows(
        &conn,
        "SELECT json_extract(props, '$.state'), json_extract(props, '$.priority') FROM mv WHERE id = 'x'",
    );
    assert_eq!(
        mv[0][0],
        Value::Text("DONE".to_string()),
        "matview state should be DONE"
    );
    assert_eq!(
        mv[0][1],
        Value::Integer(3),
        "matview should preserve other JSON fields"
    );

    Ok(())
}

/// Multiple sequential json_set() UPDATEs should each propagate
#[turso_macros::test(views)]
fn test_json_set_sequential_updates(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, props TEXT NOT NULL DEFAULT '{}')")?;
    conn.execute("INSERT INTO t VALUES ('c', '{\"state\":\"\"}')")?;

    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, props FROM t")?;

    let states = ["TODO", "DOING", "DONE"];
    for state in &states {
        let sql = format!(
            "UPDATE t SET props = json_set(props, '$.state', '{}') WHERE id = 'c'",
            state
        );
        conn.execute(&sql)?;

        let mv = limbo_exec_rows(
            &conn,
            "SELECT json_extract(props, '$.state') FROM mv WHERE id = 'c'",
        );
        assert_eq!(
            mv[0][0],
            Value::Text(state.to_string()),
            "matview should show '{}' after json_set update",
            state
        );
    }

    Ok(())
}

/// json_set() CDC should deliver changes and the matview record should contain the NEW value
#[turso_macros::test(views)]
fn test_json_set_cdc_delivers_new_value(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, props TEXT NOT NULL DEFAULT '{}')")?;
    conn.execute("INSERT INTO t VALUES ('a', '{\"state\":\"TODO\"}')")?;

    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, props FROM t")?;

    // Track CDC: count changes and capture record data as strings
    let cdc_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cdc_records: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let count_clone = cdc_count.clone();
    let records_clone = cdc_records.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name == "mv" {
            count_clone.fetch_add(event.changes.len(), std::sync::atomic::Ordering::SeqCst);
            for change in &event.changes {
                if let Some(values) = change.parse_record() {
                    records_clone.lock().unwrap().push(format!("{:?}", values));
                }
            }
        }
    });

    conn.execute("UPDATE t SET props = json_set(props, '$.state', 'DOING') WHERE id = 'a'")?;

    let count = cdc_count.load(std::sync::atomic::Ordering::SeqCst);
    assert!(count > 0, "CDC should have delivered at least one change");

    // Verify via matview read (most reliable check)
    let mv = limbo_exec_rows(
        &conn,
        "SELECT json_extract(props, '$.state') FROM mv WHERE id = 'a'",
    );
    assert_eq!(
        mv[0][0],
        Value::Text("DOING".to_string()),
        "matview should show new state after json_set CDC"
    );

    // Also check if any CDC record contains the new value
    let records = cdc_records.lock().unwrap();
    if !records.is_empty() {
        let has_new_value = records.iter().any(|s| s.contains("DOING"));
        assert!(
            has_new_value,
            "CDC records should contain new JSON with 'DOING', got: {:?}",
            *records
        );
    }

    Ok(())
}

/// Control test: full JSON replacement (workaround) should always work
#[turso_macros::test(views)]
fn test_full_json_replacement_works(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE t (id TEXT PRIMARY KEY, props TEXT NOT NULL DEFAULT '{}')")?;
    conn.execute("INSERT INTO t VALUES ('e', '{\"state\":\"TODO\",\"seq\":1}')")?;

    conn.execute("CREATE MATERIALIZED VIEW mv AS SELECT id, props FROM t")?;

    // Full replacement (the workaround for the json_set bug)
    conn.execute("UPDATE t SET props = '{\"state\":\"DOING\",\"seq\":1}' WHERE id = 'e'")?;

    let mv = limbo_exec_rows(
        &conn,
        "SELECT json_extract(props, '$.state') FROM mv WHERE id = 'e'",
    );
    assert_eq!(
        mv[0][0],
        Value::Text("DOING".to_string()),
        "full JSON replacement should always propagate correctly"
    );

    Ok(())
}
