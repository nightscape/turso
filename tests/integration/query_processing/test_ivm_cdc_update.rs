//! Test: IVM matview UPDATE propagation and CDC delivery.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use rusqlite::types::Value;

use crate::common::{limbo_exec_rows, TempDatabase};

/// Multi-row UPDATE should propagate to matview
#[turso_macros::test(views)]
fn test_matview_update_propagates(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();

    conn.execute("CREATE TABLE block (id TEXT PRIMARY KEY, val TEXT)")?;
    conn.execute("INSERT INTO block VALUES ('a', 'old_a')")?;
    conn.execute("INSERT INTO block VALUES ('b', 'old_b')")?;
    conn.execute("INSERT INTO block VALUES ('c', 'old_c')")?;

    conn.execute("CREATE MATERIALIZED VIEW mv_block AS SELECT id, val FROM block")?;

    // Track CDC
    let cdc_count = Arc::new(AtomicUsize::new(0));
    let cdc_clone = cdc_count.clone();
    conn.set_change_callback(move |event| {
        if event.relation_name == "mv_block" {
            cdc_clone.fetch_add(event.changes.len(), Ordering::SeqCst);
        }
    });

    // UPDATE one row
    conn.execute("UPDATE block SET val = 'new_b' WHERE id = 'b'")?;

    // Verify base table
    let base = limbo_exec_rows(&conn, "SELECT val FROM block WHERE id = 'b'");
    assert_eq!(
        base[0][0],
        Value::Text("new_b".to_string()),
        "base table should be updated"
    );

    // Verify matview updated
    let mv = limbo_exec_rows(&conn, "SELECT val FROM mv_block WHERE id = 'b'");
    assert_eq!(
        mv[0][0],
        Value::Text("new_b".to_string()),
        "matview should reflect UPDATE but shows stale data"
    );

    // Verify CDC delivered changes
    let changes = cdc_count.load(Ordering::SeqCst);
    assert!(
        changes > 0,
        "CDC should have delivered changes for the UPDATE"
    );

    Ok(())
}
