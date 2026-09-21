//! D170.a — a join/antijoin output row's rowid must be a function of the
//! SOURCE rowids, not a hash of the joined values.
//!
//! `JoinEvalState::combine_rows` (`core/incremental/join_operator.rs`) and
//! `emit_row` (`core/incremental/antijoin_operator.rs`) hash the whole output
//! row to make a synthetic rowid. Two consequences:
//!
//! 1. A content-only UPDATE moves the row to a NEW rowid, so the retraction of
//!    the old row is issued at a rowid that no longer describes it. A
//!    projection over the join that does not select the changed column then
//!    sees a delete at one rowid and an insert at another instead of nothing —
//!    Holon's structural watch would re-render on every keystroke.
//! 2. The retraction can miss the stored row outright, leaving a stale
//!    duplicate in the view's btree.
//!
//! With `rowid = f(left.rowid, right.rowid)` the identity of an output row is
//! the identity of the rows it came from, and both follow.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusqlite::types::Value;

use crate::common::{limbo_exec_rows, TempDatabase};

fn setup(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE block_raw (id TEXT PRIMARY KEY, parent_id TEXT, content TEXT)")?;
    conn.execute("CREATE TABLE block_tags (block_id TEXT NOT NULL, tag TEXT NOT NULL)")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW block_tags_agg AS \
            SELECT block_id AS source_id, json_group_array(tag) AS vals \
            FROM block_tags GROUP BY block_id",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW block AS \
            SELECT b.id, b.parent_id, b.content, \
                   COALESCE(block_tags_agg.vals, '[]') AS tags \
            FROM block_raw b \
            LEFT OUTER JOIN block_tags_agg ON block_tags_agg.source_id = b.id",
    )?;
    Ok(())
}

/// Holon's structural watch, as D170.a specifies it: ONE projection over the
/// join matview, selecting only the structural columns. A content-only edit
/// must be invisible to it.
#[turso_macros::test(views)]
fn content_edit_emits_no_structural_change(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
    conn.execute("CREATE MATERIALIZED VIEW block_structure AS SELECT id, parent_id FROM block")?;
    conn.execute("INSERT INTO block_raw VALUES ('b1', 'root', 'one')")?;
    conn.execute("INSERT INTO block_raw VALUES ('b2', 'b1', 'two')")?;

    let counts: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let sink = counts.clone();
    conn.set_change_callback(move |event| {
        *sink
            .lock()
            .unwrap()
            .entry(event.relation_name.clone())
            .or_default() += event.changes.len();
    });

    conn.execute("UPDATE block_raw SET content = 'edited' WHERE id = 'b2'")?;

    let counts = counts.lock().unwrap().clone();
    assert_eq!(
        counts.get("block_structure").copied().unwrap_or(0),
        0,
        "a content-only edit must produce no change in a projection that does not \
         select content; got {counts:?}"
    );
    // The edit is real, so the join view itself must report it.
    assert!(
        counts.get("block").copied().unwrap_or(0) > 0,
        "the join view must still report the content change; got {counts:?}"
    );
    Ok(())
}

/// Consequence 2: with a content-derived rowid the retraction of the pre-tag
/// row misses and the view keeps both. The view must equal its defining
/// SELECT after the aggregate side moves under a joined row.
#[turso_macros::test(views)]
fn join_row_survives_aggregate_side_change(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
    conn.execute("INSERT INTO block_raw VALUES ('b1', 'root', 'one')")?;
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM block WHERE id = 'b1'"),
        vec![vec![Value::Integer(1)]],
    );

    // The aggregate side gains a row: the joined row's values change, so its
    // content-derived rowid changes with them.
    conn.execute("INSERT INTO block_tags VALUES ('b1', 'red')")?;
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM block WHERE id = 'b1'"),
        vec![vec![Value::Integer(1)]],
        "the pre-tag row must be retracted, not left beside the tagged one"
    );

    conn.execute("INSERT INTO block_tags VALUES ('b1', 'blue')")?;
    conn.execute("DELETE FROM block_tags WHERE block_id = 'b1' AND tag = 'red'")?;
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM block WHERE id = 'b1'"),
        vec![vec![Value::Integer(1)]],
        "every aggregate-side move must retract exactly the row it replaces"
    );

    let rows = limbo_exec_rows(&conn, "SELECT tags FROM block WHERE id = 'b1'");
    assert_eq!(rows, vec![vec![Value::Text("[\"blue\"]".into())]]);
    Ok(())
}

/// A joined row's rowid must not move when a non-key column changes: the same
/// (left, right) pair is the same output row. Read the rowid directly.
#[turso_macros::test(views)]
fn joined_rowid_is_stable_across_content_edits(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&conn)?;
    conn.execute("INSERT INTO block_raw VALUES ('b1', 'root', 'one')")?;
    let before = limbo_exec_rows(&conn, "SELECT rowid FROM block WHERE id = 'b1'");
    conn.execute("UPDATE block_raw SET content = 'two' WHERE id = 'b1'")?;
    let after = limbo_exec_rows(&conn, "SELECT rowid FROM block WHERE id = 'b1'");
    assert_eq!(
        before, after,
        "the joined row's rowid must be a function of the source rowids, so a \
         content edit leaves it where it was"
    );
    Ok(())
}
