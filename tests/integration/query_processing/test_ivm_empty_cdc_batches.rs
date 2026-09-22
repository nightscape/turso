//! Test: IVM must NOT fire CDC callbacks for upstream writes that don't change matview output.
//!
//! Bug: Chained matviews (current_focus → focus_roots → region_main_view) emit empty
//! CDC batches (changes.len() == 0) when upstream tables receive writes that don't
//! affect the matview's projection. Two known triggers:
//!   1. INSERT into a base table whose row doesn't match any matview predicate.
//!   2. DELETE that matches zero rows (no-op delete).
//!
//! Both cases produce an empty delta, but the IVM commit pipeline still calls
//! set_change_callback for every chained matview, polluting the CDC stream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::common::TempDatabase;

#[derive(Debug, Default, Clone)]
struct CdcStats {
    batches: usize,
    items: usize,
    empty_batches: usize,
}

fn install_callback(conn: &Arc<turso_core::Connection>) -> Arc<Mutex<HashMap<String, CdcStats>>> {
    let stats: Arc<Mutex<HashMap<String, CdcStats>>> = Arc::new(Mutex::new(HashMap::new()));
    let stats_clone = stats.clone();
    conn.set_change_callback(move |event| {
        let mut s = stats_clone.lock().unwrap();
        let entry = s.entry(event.relation_name.clone()).or_default();
        entry.batches += 1;
        entry.items += event.changes.len();
        if event.changes.is_empty() {
            entry.empty_batches += 1;
        }
    });
    stats
}

fn setup_schema(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute(
        "CREATE TABLE block (\
            id TEXT PRIMARY KEY, \
            parent_id TEXT NOT NULL, \
            content TEXT DEFAULT '', \
            content_type TEXT DEFAULT 'text')",
    )?;
    conn.execute(
        "CREATE TABLE navigation_history (\
            id INTEGER PRIMARY KEY AUTOINCREMENT, \
            region TEXT NOT NULL, \
            block_id TEXT)",
    )?;
    conn.execute(
        "CREATE TABLE navigation_cursor (\
            region TEXT PRIMARY KEY, \
            history_id INTEGER REFERENCES navigation_history(id))",
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW current_focus AS \
            SELECT nc.region, nh.block_id \
            FROM navigation_cursor nc \
            JOIN navigation_history nh ON nc.history_id = nh.id",
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW focus_roots AS \
            SELECT cf.region, cf.block_id, b.id AS root_id \
            FROM current_focus cf \
            JOIN block b ON b.parent_id = cf.block_id \
            UNION ALL \
            SELECT cf.region, cf.block_id, b.id AS root_id \
            FROM current_focus cf \
            JOIN block b ON b.id = cf.block_id",
    )?;

    conn.execute(
        "CREATE MATERIALIZED VIEW region_main_view AS \
            SELECT fr.root_id AS id, b.content, b.parent_id \
            FROM focus_roots fr \
            JOIN block b ON b.id = fr.root_id \
            WHERE fr.region = 'main'",
    )?;

    Ok(())
}

/// INSERT into block with a parent_id that doesn't match the focused block must not
/// produce a CDC batch on focus_roots / region_main_view.
#[turso_macros::test(views)]
fn test_no_empty_cdc_for_unrelated_insert(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;

    // Seed: focus on doc1, with two children under doc1 so focus_roots has rows.
    conn.execute("INSERT INTO block VALUES ('doc1', 'root', 'Doc 1', 'text')")?;
    conn.execute("INSERT INTO block VALUES ('child1', 'doc1', 'Child 1', 'text')")?;
    conn.execute("INSERT INTO block VALUES ('child2', 'doc1', 'Child 2', 'text')")?;
    conn.execute("INSERT INTO navigation_history (region, block_id) VALUES ('main', 'doc1')")?;
    conn.execute(
        "INSERT OR REPLACE INTO navigation_cursor (region, history_id) VALUES ('main', 1)",
    )?;

    // Install callback now — we only care about events after this point.
    let stats = install_callback(&conn);

    // Trigger: insert a block whose parent_id does NOT match the focused doc.
    // This row is invisible to focus_roots' UNION ALL — both branches require
    // a match against current_focus.block_id ('doc1').
    conn.execute("INSERT INTO block VALUES ('orphan', 'unrelated_parent', 'Orphan', 'text')")?;

    let s = stats.lock().unwrap();
    let fr = s.get("focus_roots").cloned().unwrap_or_default();
    let rmv = s.get("region_main_view").cloned().unwrap_or_default();

    assert_eq!(
        fr.batches, 0,
        "focus_roots must not receive a CDC batch for an unrelated INSERT (got {} batches, {} empty). All stats: {:?}",
        fr.batches, fr.empty_batches, *s
    );
    assert_eq!(
        rmv.batches, 0,
        "region_main_view must not receive a CDC batch when focus_roots didn't change (got {} batches). All stats: {:?}",
        rmv.batches, *s
    );

    Ok(())
}

/// DELETE that matches zero rows is a no-op and must not fire CDC anywhere.
#[turso_macros::test(views)]
fn test_no_empty_cdc_for_zero_row_delete(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;

    conn.execute("INSERT INTO block VALUES ('doc1', 'root', 'Doc 1', 'text')")?;
    conn.execute("INSERT INTO block VALUES ('child1', 'doc1', 'Child 1', 'text')")?;
    conn.execute("INSERT INTO navigation_history (region, block_id) VALUES ('main', 'doc1')")?;
    conn.execute(
        "INSERT OR REPLACE INTO navigation_cursor (region, history_id) VALUES ('main', 1)",
    )?;

    let stats = install_callback(&conn);

    // navigation_history has a single row id=1; this DELETE matches zero rows.
    conn.execute("DELETE FROM navigation_history WHERE id > 100")?;

    let s = stats.lock().unwrap();
    let cf = s.get("current_focus").cloned().unwrap_or_default();
    let fr = s.get("focus_roots").cloned().unwrap_or_default();
    let rmv = s.get("region_main_view").cloned().unwrap_or_default();

    assert_eq!(
        cf.batches, 0,
        "current_focus must not receive a CDC batch for a zero-row DELETE (got {} batches, {} empty). All stats: {:?}",
        cf.batches, cf.empty_batches, *s
    );
    assert_eq!(
        fr.batches, 0,
        "focus_roots must not cascade an empty batch from current_focus (got {} batches). All stats: {:?}",
        fr.batches, *s
    );
    assert_eq!(
        rmv.batches, 0,
        "region_main_view must not cascade an empty batch (got {} batches). All stats: {:?}",
        rmv.batches, *s
    );

    Ok(())
}

/// End-to-end: real navigation followed by no-op writes and another navigation.
/// Mirrors the production scenario in the handoff. Asserts batch counts:
///   - current_focus: 2 batches (one per real navigation)
///   - focus_roots:   2 batches
///   - region_main_view: 2 batches
#[turso_macros::test(views)]
fn test_two_navigations_no_spurious_batches(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;

    // Two docs with children
    conn.execute("INSERT INTO block VALUES ('doc1', 'root', 'Doc 1', 'text')")?;
    conn.execute("INSERT INTO block VALUES ('child1a', 'doc1', '1a', 'text')")?;
    conn.execute("INSERT INTO block VALUES ('child1b', 'doc1', '1b', 'text')")?;

    conn.execute("INSERT INTO block VALUES ('doc2', 'root', 'Doc 2', 'text')")?;
    conn.execute("INSERT INTO block VALUES ('child2a', 'doc2', '2a', 'text')")?;
    conn.execute("INSERT INTO block VALUES ('child2b', 'doc2', '2b', 'text')")?;

    conn.execute("INSERT INTO navigation_history (region, block_id) VALUES ('main', 'doc1')")?;
    conn.execute(
        "INSERT OR REPLACE INTO navigation_cursor (region, history_id) VALUES ('main', 1)",
    )?;

    // Install callback after first navigation completes.
    let stats = install_callback(&conn);

    // Production-like sequence between two navigations:
    //   tx1: DELETE FROM navigation_history WHERE region='main' AND id > 1   (0 rows)
    //   tx2: INSERT INTO block (...)                                          (unrelated row)
    //   tx3: INSERT INTO navigation_history (region, block_id)                (real change)
    //   tx4: INSERT OR REPLACE INTO navigation_cursor                         (real change → cascade)
    conn.execute("DELETE FROM navigation_history WHERE region = 'main' AND id > 1")?;
    conn.execute("INSERT INTO block VALUES ('orphan', 'unrelated', 'O', 'text')")?;
    conn.execute("INSERT INTO navigation_history (region, block_id) VALUES ('main', 'doc2')")?;
    conn.execute(
        "INSERT OR REPLACE INTO navigation_cursor (region, history_id) VALUES ('main', 2)",
    )?;

    let s = stats.lock().unwrap();
    let cf = s.get("current_focus").cloned().unwrap_or_default();
    let fr = s.get("focus_roots").cloned().unwrap_or_default();
    let rmv = s.get("region_main_view").cloned().unwrap_or_default();

    // Only the cursor INSERT OR REPLACE should produce a real cascade.
    assert_eq!(
        cf.batches, 1,
        "current_focus should fire exactly once (the cursor switch). Got {} batches ({} empty). All: {:?}",
        cf.batches, cf.empty_batches, *s
    );
    assert_eq!(
        fr.batches, 1,
        "focus_roots should fire exactly once. Got {} batches ({} empty). All: {:?}",
        fr.batches, fr.empty_batches, *s
    );
    assert_eq!(
        rmv.batches, 1,
        "region_main_view should fire exactly once. Got {} batches ({} empty). All: {:?}",
        rmv.batches, rmv.empty_batches, *s
    );

    // No empty batches anywhere on chained matviews.
    for (name, st) in s.iter() {
        assert_eq!(
            st.empty_batches, 0,
            "relation {name} produced {} empty CDC batches. All stats: {:?}",
            st.empty_batches, *s
        );
    }

    Ok(())
}
