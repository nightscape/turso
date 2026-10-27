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
    })
    .unwrap();

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
    })
    .unwrap();

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
