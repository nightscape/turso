//! Change callbacks for writes to a table in an ATTACHed database, with a
//! version 1 `turso_cdc` table (the one CDC mode that reports base tables).

use std::sync::{Arc, Mutex};

use turso_core::types::RelationChangeEvent;
use turso_core::DatabaseOpts;

use crate::common::TempDatabase;

#[test]
fn a_write_to_an_attached_table_reports_that_table() -> anyhow::Result<()> {
    let db = TempDatabase::builder()
        .with_opts(DatabaseOpts::new().with_attach(true))
        .build();
    let conn = db.connect_limbo();
    let aux_path = db.path.with_extension("cdc_callback_aux.db");
    conn.execute(format!("ATTACH '{}' AS aux", aux_path.display()))?;
    conn.execute("CREATE TABLE t (x)")?;
    conn.execute("CREATE TABLE aux.t (a, b)")?;
    conn.execute(
        "CREATE TABLE turso_cdc (change_id INTEGER PRIMARY KEY AUTOINCREMENT, change_time INTEGER, change_type INTEGER, table_name TEXT, id, before BLOB, after BLOB, updates BLOB)",
    )?;
    conn.execute("PRAGMA capture_data_changes_conn('full')")?;

    let events: Arc<Mutex<Vec<(String, Vec<String>)>>> = Arc::default();
    let sink = events.clone();
    conn.set_change_callback(move |event: &RelationChangeEvent| {
        sink.lock()
            .unwrap()
            .push((event.relation_name.clone(), event.columns.clone()));
    });

    conn.execute("INSERT INTO aux.t VALUES (1, 2)")?;
    conn.execute("INSERT INTO t VALUES (3)")?;

    assert_eq!(
        *events.lock().unwrap(),
        vec![
            ("aux.t".to_string(), vec!["a".to_string(), "b".to_string()]),
            ("t".to_string(), vec!["x".to_string()]),
        ]
    );
    Ok(())
}
