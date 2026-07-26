//! Plan-shape regression tests for recursive CTEs.
//!
//! The arms of a recursive CTE are planned by `translate_recursive_cte` in
//! `core/translate/select.rs`, which historically emitted each arm's plan without ever
//! running it through `optimize_plan`. Every table in every arm therefore kept its
//! default full-table `Operation::Scan`, even when an equality predicate had a perfectly
//! usable index -- making a tree walk quadratic. These tests assert the *plan shape*
//! rather than timings, because timing assertions are flaky under CI contention.

use crate::common::{limbo_exec_rows, TempDatabase};
use std::sync::Arc;
use turso_core::Connection;

const SCHEMA: &str = "
CREATE TABLE block_raw (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    content TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_block_raw_parent_id ON block_raw(parent_id);
CREATE TABLE block_tags (
    block_id TEXT NOT NULL,
    tag TEXT NOT NULL,
    PRIMARY KEY (block_id, tag)
);
";

/// The recursive descendant walk. Mirrors the downstream query that exposed the defect:
/// the recursive arm joins the base table to the working table on an indexed column.
const RECURSIVE_CTE: &str = "
WITH RECURSIVE descendants(id, depth_acc) AS (
  SELECT b.id, 0 FROM block_raw b
  LEFT JOIN block_tags bt ON bt.block_id = b.id AND bt.tag = 'Page'
  WHERE b.parent_id = 'doc' AND bt.block_id IS NULL
  UNION ALL
  SELECT b.id, d.depth_acc + 1 FROM block_raw b
  JOIN descendants d ON b.parent_id = d.id
  LEFT JOIN block_tags bt ON bt.block_id = b.id AND bt.tag = 'Page'
  WHERE bt.block_id IS NULL AND d.depth_acc < 100
)
SELECT count(*) FROM descendants
";

/// Seeds a ternary tree rooted at 'doc': node `i`'s parent is `b<(i-1)/3>`, so the tree
/// reaches depth ~5 at this size. Enough rows that a full scan is visibly the wrong plan.
fn seed_tree(conn: &Arc<Connection>, n: usize) {
    limbo_exec_rows(
        conn,
        "INSERT INTO block_raw(id, parent_id) VALUES ('doc', NULL)",
    );
    for i in 1..=n {
        let parent = if i <= 3 {
            "doc".to_string()
        } else {
            format!("b{}", (i - 1) / 3)
        };
        limbo_exec_rows(
            conn,
            &format!("INSERT INTO block_raw(id, parent_id) VALUES ('b{i}', '{parent}')"),
        );
    }
}

/// Runs `EXPLAIN QUERY PLAN` and flattens each output row into one string, so the
/// assertions do not depend on the exact column layout of the EQP result.
fn plan_lines(conn: &Arc<Connection>, sql: &str) -> Vec<String> {
    limbo_exec_rows(conn, &format!("EXPLAIN QUERY PLAN {sql}"))
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| match v {
                    rusqlite::types::Value::Text(s) => s.clone(),
                    rusqlite::types::Value::Integer(i) => i.to_string(),
                    rusqlite::types::Value::Real(f) => f.to_string(),
                    rusqlite::types::Value::Null => String::new(),
                    rusqlite::types::Value::Blob(_) => "<blob>".to_string(),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn setup() -> (TempDatabase, Arc<Connection>) {
    let db = TempDatabase::new_empty();
    let conn = db.connect_limbo();
    for stmt in SCHEMA.split(';').filter(|s| !s.trim().is_empty()) {
        limbo_exec_rows(&conn, stmt);
    }
    seed_tree(&conn, 300);
    (db, conn)
}

/// Control: outside a recursive CTE the planner already picks the index. If this ever
/// fails, the other assertions in this file are meaningless (wrong index name, missing
/// index, changed EQP wording) rather than evidence about recursive CTEs.
#[test]
fn indexed_equality_outside_recursive_cte_is_a_search() {
    let (_db, conn) = setup();
    let lines = plan_lines(&conn, "SELECT id FROM block_raw WHERE parent_id = 'doc'");
    assert!(
        lines
            .iter()
            .any(|l| l.contains("idx_block_raw_parent_id") && l.contains("SEARCH")),
        "control probe should use idx_block_raw_parent_id, got:\n{}",
        lines.join("\n")
    );
}

/// The base arm of a recursive CTE contains no working table at all, so the identical
/// predicate must plan identically to the control probe above.
#[test]
fn recursive_cte_base_arm_uses_index() {
    let (_db, conn) = setup();
    let lines = plan_lines(&conn, RECURSIVE_CTE);
    let indexed = lines
        .iter()
        .filter(|l| l.contains("idx_block_raw_parent_id") && l.contains("SEARCH"))
        .count();
    assert!(
        indexed >= 1,
        "recursive CTE base arm should search idx_block_raw_parent_id, got:\n{}",
        lines.join("\n")
    );
}

/// The recursive arm joins `block_raw` to the working table on `parent_id = d.id`.
/// Both arms must use the index, so we require two distinct indexed searches.
#[test]
fn recursive_cte_recursive_arm_uses_index() {
    let (_db, conn) = setup();
    let lines = plan_lines(&conn, RECURSIVE_CTE);
    let indexed = lines
        .iter()
        .filter(|l| l.contains("idx_block_raw_parent_id") && l.contains("SEARCH"))
        .count();
    assert!(
        indexed >= 2,
        "both recursive CTE arms should search idx_block_raw_parent_id \
         (found {indexed} indexed searches), got:\n{}",
        lines.join("\n")
    );
}

/// The recursion must still be driven by the working table: each round iterates the rows
/// produced by the previous round and probes `block_raw` by index. If the optimizer were
/// to flip this so `block_raw` drives and the working table is probed, the fixpoint
/// semantics would be wrong even where a one-shot result happens to look right.
#[test]
fn recursive_cte_working_table_drives_the_recursive_arm() {
    let (_db, conn) = setup();
    let lines = plan_lines(&conn, RECURSIVE_CTE);
    let first_working_table = lines
        .iter()
        .position(|l| l.contains("descendants"))
        .unwrap_or_else(|| {
            panic!(
                "expected the working table to appear in the plan, got:\n{}",
                lines.join("\n")
            )
        });
    let indexed_after = lines
        .iter()
        .skip(first_working_table)
        .any(|l| l.contains("idx_block_raw_parent_id") && l.contains("SEARCH"));
    assert!(
        indexed_after,
        "recursive arm should scan the working table and then probe block_raw by index, got:\n{}",
        lines.join("\n")
    );
}
