//! Regression: a recursive (UNION ALL) materialized view that is populated,
//! closed, and REOPENED must still be able to incrementally *retract* rows that
//! were derived before the reopen.
//!
//! The recursive operator (`core/incremental/recursive_operator.rs`) assigns a
//! stable rowid to every UNION-ALL tuple it emits and remembers the mapping in
//! `union_all_rowids: value_hash(internal CTE tuple) -> Vec<rowid>`. On a
//! retraction it pops the rowid the row was inserted under so the `-1` cancels
//! the right committed row. That map is pure in-memory state with NO dedicated
//! on-disk copy: on reopen `DbspCircuit::restore_recursive_operators_if_needed`
//! rebuilds it by hashing the rows of the matview's OUTPUT btree
//! (`main_data_root`) — the OUTER-projection tuples (`SELECT d.*`, 6 columns in
//! the shape below). But `normalize_delta` keys the map by the INTERNAL
//! recursive tuple (`node_id, source_id, depth, visited`, 4 columns). The two
//! shapes can never hash-match, so after reopen every retraction of a
//! pre-existing derived row misses the map and falls through to a silent
//! fallback that invents a fresh rowid whose `-1` cancels nothing — leaving
//! stale rows in the matview.
//!
//! The whole-view output happens to self-correct in many shapes (the matview
//! btree re-keys by a content hash, so a single-multiplicity retraction still
//! cancels by content) which makes an output-drift oracle NON-deterministic
//! across processes (HashMap ordering decides). So this test asserts the
//! underlying INVARIANT directly and deterministically: after a reopen, a
//! retraction of a pre-existing derived row must NEVER trip the
//! invented-rowid fallback. The fallback count is exposed as a process-global
//! counter by `turso_core::take_recursive_restore_fallback_count()`.

use crate::common::{limbo_exec_rows, TempDatabase};
use std::path::Path;
use std::sync::Arc;
use turso_core::Connection;

/// The recursive UNION-ALL view, faithfully mirroring holon's watch view: a
/// descendant walk whose INTERNAL CTE carries 4 columns
/// (node_id, source_id, depth, visited) but whose OUTER projection is the
/// wider block row `d.*` — the arity/shape mismatch that breaks restore.
const VIEW_SQL: &str = "\
CREATE MATERIALIZED VIEW descendants AS \
WITH RECURSIVE dd AS ( \
    SELECT b.id AS node_id, b.id AS source_id, 0 AS depth, CAST(b.id AS TEXT) AS visited \
    FROM block b JOIN roots r ON b.id = r.root_id \
    UNION ALL \
    SELECT c.id, dd.source_id, dd.depth + 1, dd.visited || ',' || CAST(c.id AS TEXT) \
    FROM dd JOIN block c ON c.parent_id = dd.node_id \
    WHERE dd.depth < 20 \
      AND ',' || dd.visited || ',' NOT LIKE '%,' || CAST(c.id AS TEXT) || ',%' \
) \
SELECT d.id AS node_id, d.parent_id AS parent_id, d.label AS label, dd.depth AS depth \
FROM dd JOIN block d ON d.id = dd.node_id";

fn open(path: &Path) -> (TempDatabase, Arc<Connection>) {
    let db = TempDatabase::builder()
        .with_db_path(path)
        .with_views(true)
        .build();
    let conn = db.connect_limbo();
    (db, conn)
}

/// Seed a base table `block` (a matview over `block_raw`) plus a `roots` anchor,
/// with `groups` chains of `depth` nodes hanging off a single anchor `doc`.
fn seed(conn: &Arc<Connection>, groups: usize, depth: usize) {
    conn.execute("CREATE TABLE block_raw (id TEXT PRIMARY KEY, parent_id TEXT, label TEXT)")
        .unwrap();
    conn.execute("INSERT INTO block_raw VALUES ('doc', 'root', 'doc')")
        .unwrap();
    for g in 0..groups {
        let mut parent = "doc".to_string();
        for d in 0..depth {
            let id = format!("n{g}_{d}");
            conn.execute(&format!(
                "INSERT INTO block_raw VALUES ('{id}', '{parent}', 'lbl{g}{d}')"
            ))
            .unwrap();
            parent = id;
        }
    }
    conn.execute("CREATE MATERIALIZED VIEW block AS SELECT id, parent_id, label FROM block_raw")
        .unwrap();
    conn.execute("CREATE TABLE roots (root_id TEXT PRIMARY KEY)")
        .unwrap();
    conn.execute("INSERT INTO roots VALUES ('doc')").unwrap();
    conn.execute(VIEW_SQL).unwrap();
}

fn matview_node_count(conn: &Arc<Connection>) -> i64 {
    let rows = limbo_exec_rows(conn, "SELECT count(*) FROM descendants");
    match &rows[0][0] {
        rusqlite::types::Value::Integer(n) => *n,
        other => panic!("unexpected count value: {other:?}"),
    }
}

#[test]
fn recursive_union_all_reopen_retract_matches_original_rowids() -> anyhow::Result<()> {
    let dir = tempfile::TempDir::new()?;
    let path = dir.path().join("reopen_restore.db");

    // Phase 1: build + populate the recursive matview, then close the database
    // so all in-memory operator state (union_all_rowids, next_rowid) is lost.
    let populated = {
        let (db, conn) = open(&path);
        seed(&conn, 4, 6);
        let n = matview_node_count(&conn);
        assert!(n > 0, "matview did not populate");
        drop(conn);
        drop(db);
        n
    };

    // Phase 2: reopen from disk (rebuilds the circuit -> triggers the restore
    // path) and, in one transaction, RETRACT a whole pre-existing subtree by
    // detaching one of its nodes from the tree.
    let (_db, conn) = open(&path);
    turso_core::take_recursive_restore_fallback_count(); // reset

    conn.execute("BEGIN")?;
    conn.execute("UPDATE block_raw SET parent_id = 'detached' WHERE id = 'n2_2'")?;
    conn.execute("COMMIT")?;

    let fallbacks = turso_core::take_recursive_restore_fallback_count();

    // Detaching n2_2 removes it and its descendants (n2_3, n2_4, n2_5): 4 rows.
    let expected = populated - 4;
    let actual = matview_node_count(&conn);

    assert_eq!(
        fallbacks, 0,
        "after reopen, retracting pre-existing derived rows fell through to the \
         invented-rowid fallback {fallbacks} time(s): the restored union_all_rowids \
         map cannot match internal tuples. matview now has {actual} rows (expected {expected})."
    );
    assert_eq!(
        actual, expected,
        "matview drifted from its defining query after reopen + retraction"
    );
    Ok(())
}
