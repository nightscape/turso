//! Regression: an ORDER BY on the *outer* query over a recursive CTE must not
//! panic the access-method / sort-elimination optimizer.
//!
//! After `translate_recursive_cte` began running each arm (and the outer query)
//! through `optimize_plan`, the outer query's reference to the recursive CTE --
//! a `Table::RecursiveCte` pseudo-table backed by the result cursor (see
//! `core/translate/select.rs`) -- reaches `plan_satisfies_order_target` in
//! `core/translate/optimizer/order.rs` whenever the outer query carries an
//! ORDER BY / GROUP BY / MIN-MAX. That table is planned with
//! `AccessMethodParams::Subquery`, whose match arm used to `unreachable!` unless
//! the underlying `Table` was a `FromClauseSubquery`. A recursive working table
//! is *not* a `FromClauseSubquery`, so the arm tripped the `unreachable!`.
//!
//! This mirrors holon's `get_blocks` main-panel query: a recursive descendant
//! walk over a parent/child edge table, joining each round back to the working
//! table, with the outer query ordering the result. Downstream saw 225 such
//! panics in a single debug test run; a `panic=abort` release build would hard
//! abort instead of self-healing on unwind.

use crate::common::{limbo_exec_rows, TempDatabase};
use std::sync::Arc;
use turso_core::Connection;

const SCHEMA: &str = "
CREATE TABLE block_raw (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    sort_key TEXT NOT NULL DEFAULT 'A0',
    content TEXT NOT NULL DEFAULT ''
);
CREATE INDEX idx_block_raw_parent_id ON block_raw(parent_id);
CREATE TABLE block_tags (
    block_id TEXT NOT NULL,
    tag TEXT NOT NULL,
    PRIMARY KEY (block_id, tag)
);
";

/// Holon's `get_blocks`, faithfully reduced. A recursive descendant walk names
/// the working table `descendants`; the *outer* query joins the real base table
/// back to that working table (`block_raw b JOIN descendants d ON d.id = b.id`)
/// and orders by columns of the real table (`ORDER BY b.sort_key, b.id`).
///
/// The join + outer ORDER BY is what routes the working table's `Subquery`
/// access method into the ORDER-BY / sort-elimination optimizer: the join
/// planner enumerates orders over both `block_raw` and the `descendants`
/// working table, and `plan_satisfies_order_target` walks a candidate plan that
/// includes the `Table::RecursiveCte` working table. A single-table
/// `SELECT ... FROM descendants ORDER BY id` does *not* reach this arm; the
/// real-table join does.
const RECURSIVE_CTE_ORDER_BY: &str = "
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
SELECT b.id, b.sort_key
FROM block_raw b
JOIN descendants d ON d.id = b.id
ORDER BY b.sort_key, b.id
";

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

fn setup() -> (TempDatabase, Arc<Connection>) {
    let db = TempDatabase::new_empty();
    let conn = db.connect_limbo();
    for stmt in SCHEMA.split(';').filter(|s| !s.trim().is_empty()) {
        limbo_exec_rows(&conn, stmt);
    }
    seed_tree(&conn, 300);
    (db, conn)
}

fn as_text(v: &rusqlite::types::Value) -> &str {
    match v {
        rusqlite::types::Value::Text(s) => s.as_str(),
        other => panic!("expected text id, got {other:?}"),
    }
}

/// RED before the fix: preparing this statement panics with
/// `access_method.params::Subquery must be for a FromClauseSubquery table`
/// because the outer query's recursive working table reaches the ORDER-BY
/// access-method optimizer. GREEN after: it plans, runs, and returns every
/// descendant, correctly ordered by id (the fix keeps an explicit sort rather
/// than claiming the working table provides one).
#[test]
fn outer_order_by_over_recursive_cte_does_not_panic() {
    let (_db, conn) = setup();
    let rows = limbo_exec_rows(&conn, RECURSIVE_CTE_ORDER_BY);

    // 300 nodes seeded under 'doc' (b1..=b300); all are descendants of 'doc'.
    assert_eq!(
        rows.len(),
        300,
        "expected every seeded descendant to be returned"
    );

    // The explicit ORDER BY must still be honored end-to-end. The fix keeps a
    // real sort (the working table advertises no usable ordering), so the rows
    // must come back sorted by (sort_key, id).
    let keyed: Vec<(&str, &str)> = rows
        .iter()
        .map(|r| (as_text(&r[1]), as_text(&r[0])))
        .collect();
    let mut sorted = keyed.clone();
    sorted.sort_unstable();
    assert_eq!(keyed, sorted, "outer ORDER BY b.sort_key, b.id must hold");
}
