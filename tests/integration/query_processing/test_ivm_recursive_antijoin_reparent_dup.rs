//! Regression: a recursive-CTE matview whose recursive arm carries a LEFT OUTER
//! JOIN (anti-join) must not accumulate a duplicate row when a transaction
//! re-parents two correlated edges (an ancestor and a descendant) at once.
//!
//! Root cause (see core/incremental/compiler.rs): the LEFT OUTER JOIN compiles
//! the recursive-step inner join into a diamond — an Inner join and an Antijoin
//! both consume it, merged afterwards. `exec_node_cache` is what commits that
//! shared node once per pass and hands its delta to both consumers. The
//! `RecursiveState::Iterating` arm used to clear that memo at the TOP of the
//! arm, which is re-entered on every IO-resume re-poll; when a yield landed
//! between the two consumers' descents, the resume wiped the memo and the second
//! consumer re-committed the shared node's delta to its persistent state a second
//! time — leaving an orphaned matview row (same logical row under two rowids).
//!
//! Without the anti-join arm there is no diamond and no duplication, so the arm
//! is load-bearing here. Oracle: the matview must equal a recompute of its own
//! defining SELECT after every reparent round.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::common::TempDatabase;

/// Recursive-CTE watch shape WITH the anti-join arm (`LEFT OUTER JOIN blk_tags`).
/// Mirrors holon's production `watch_real`.
const WATCH_ANTIJOIN_SQL: &str = "\
    WITH RECURSIVE focus_descendants AS ( \
        SELECT b.id AS node_id, b.id AS source_id, 0 AS depth, CAST(b.id AS TEXT) AS visited \
        FROM blk b JOIN focus_roots fr ON b.id = fr.root_id \
        UNION ALL \
        SELECT child.id, focus_descendants.source_id, focus_descendants.depth + 1, \
               focus_descendants.visited || ',' || CAST(child.id AS TEXT) \
        FROM focus_descendants \
        JOIN blk child ON child.parent_id = focus_descendants.node_id \
        LEFT OUTER JOIN blk_tags pt ON pt.block_id = focus_descendants.node_id AND pt.tag = 'Page' \
        WHERE focus_descendants.depth < 20 \
          AND ',' || focus_descendants.visited || ',' NOT LIKE \
              '%,' || CAST(child.id AS TEXT) || ',%' \
          AND (focus_descendants.depth = 0 OR pt.block_id IS NULL) \
    ) \
    SELECT d.id AS node_id, focus_descendants.depth AS depth \
    FROM focus_roots fr \
    JOIN blk root ON root.id = fr.root_id \
    JOIN focus_descendants ON focus_descendants.source_id = root.id \
    JOIN blk d ON d.id = focus_descendants.node_id \
    JOIN navigation_cursor nc ON nc.region = fr.region AND nc.history_id = fr.history_id \
    WHERE fr.region = 'main'";

fn seed(conn: &Arc<turso_core::Connection>, groups: usize, depth: usize) -> anyhow::Result<()> {
    conn.execute("CREATE TABLE blk_raw (id TEXT PRIMARY KEY, parent_id TEXT)")?;
    conn.execute("INSERT INTO blk_raw VALUES ('doc', 'root')")?;
    for g in 0..groups {
        let mut parent = "doc".to_string();
        for d in 0..depth {
            let id = format!("bulk-{g}-{d}");
            conn.execute(&format!("INSERT INTO blk_raw VALUES ('{id}', '{parent}')"))?;
            parent = id;
        }
    }
    conn.execute("CREATE MATERIALIZED VIEW blk AS SELECT id, parent_id FROM blk_raw")?;

    // Only `doc` carries the 'Page' tag, so the anti-join predicate is live.
    conn.execute("CREATE TABLE blk_tags (block_id TEXT, tag TEXT)")?;
    conn.execute("INSERT INTO blk_tags VALUES ('doc', 'Page')")?;

    conn.execute(
        "CREATE TABLE navigation_history (id INTEGER PRIMARY KEY AUTOINCREMENT, \
         region TEXT NOT NULL, block_id TEXT, closed_at TEXT NULL)",
    )?;
    conn.execute("CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER)")?;
    conn.execute(
        "INSERT INTO navigation_history (id, region, block_id, closed_at) \
         VALUES (1, 'main', 'doc', NULL)",
    )?;
    conn.execute("INSERT INTO navigation_cursor (region, history_id) VALUES ('main', 1)")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW focus_roots AS \
         SELECT region, block_id AS root_id, id AS history_id \
         FROM navigation_history WHERE closed_at IS NULL AND block_id IS NOT NULL",
    )?;

    conn.execute(&format!(
        "CREATE MATERIALIZED VIEW watch_view AS {WATCH_ANTIJOIN_SQL}"
    ))?;
    Ok(())
}

/// The bug trigger: an autocommit outdent, then a single transaction that
/// re-parents BOTH an ancestor edge (`parent → great_grandparent`) and its
/// descendant (`child → parent`). Two correlated edges chaining through the
/// recursion in one commit is what forces the diamond's shared node to be
/// re-committed under an adverse IO-yield interleaving.
fn outdent_chain_step(
    conn: &Arc<turso_core::Connection>,
    child: &str,
    parent: &str,
    grandparent: &str,
    great_grandparent: &str,
) -> anyhow::Result<()> {
    conn.execute(&format!(
        "UPDATE blk_raw SET parent_id = '{grandparent}' WHERE id = '{child}'"
    ))?;
    conn.execute("BEGIN")?;
    conn.execute(&format!(
        "UPDATE blk_raw SET parent_id = '{great_grandparent}' WHERE id = '{parent}'"
    ))?;
    conn.execute(&format!(
        "UPDATE blk_raw SET parent_id = '{parent}' WHERE id = '{child}'"
    ))?;
    conn.execute("COMMIT")?;
    Ok(())
}

fn link(g: usize, d: usize) -> (String, String, String, String) {
    let node = |i: i64| {
        if i < 0 {
            "doc".to_string()
        } else {
            format!("bulk-{g}-{i}")
        }
    };
    let d = d as i64;
    (node(d), node(d - 1), node(d - 2), node(d - 3))
}

fn reset_tree(
    conn: &Arc<turso_core::Connection>,
    groups: usize,
    depth: usize,
) -> anyhow::Result<()> {
    for g in 0..groups {
        conn.execute("BEGIN")?;
        for d in 0..depth {
            let id = format!("bulk-{g}-{d}");
            let parent = if d == 0 {
                "doc".to_string()
            } else {
                format!("bulk-{g}-{}", d - 1)
            };
            conn.execute(&format!(
                "UPDATE blk_raw SET parent_id = '{parent}' WHERE id = '{id}'"
            ))?;
        }
        conn.execute("COMMIT")?;
    }
    Ok(())
}

fn rowset(conn: &Arc<turso_core::Connection>, sql: &str) -> anyhow::Result<Vec<String>> {
    let rows = Arc::new(Mutex::new(Vec::new()));
    let sink = rows.clone();
    let mut stmt = conn.prepare(sql)?;
    stmt.run_with_row_callback(|row| {
        let node = row.get::<String>(0).unwrap_or_default();
        let depth = row.get::<i64>(1).unwrap_or(-1);
        sink.lock().unwrap().push(format!("{node}@d{depth}"));
        Ok(())
    })?;
    let mut out = rows.lock().unwrap().clone();
    out.sort();
    Ok(out)
}

/// Oracle: the matview's rowset (a multiset — duplicate rows survive) must equal
/// a fresh recompute of its own defining SELECT. A duplicated orphan row shows up
/// as an extra element in the matview multiset.
fn assert_no_drift(conn: &Arc<turso_core::Connection>, label: &str) -> anyhow::Result<()> {
    let matview = rowset(conn, "SELECT node_id, depth FROM watch_view")?;
    let recompute = rowset(conn, WATCH_ANTIJOIN_SQL)?;
    if matview != recompute {
        let mv: HashSet<_> = matview.iter().collect();
        let rc: HashSet<_> = recompute.iter().collect();
        let stale: Vec<_> = mv.difference(&rc).collect();
        let missing: Vec<_> = rc.difference(&mv).collect();
        anyhow::bail!(
            "MATVIEW-VS-RECOMPUTE DRIFT at {label}: matview has {} rows, recompute {}. \
             stale-in-matview={stale:?} missing-from-matview={missing:?}",
            matview.len(),
            recompute.len()
        );
    }
    Ok(())
}

/// Replay the outdent/re-adopt hammering round after round against the live
/// matview and assert it never drifts from a recompute. On the unfixed compiler
/// the mid-pass memo clear duplicates a deep-tail row within a handful of rounds;
/// on the fixed compiler the matview stays exact.
#[turso_macros::test(views)]
fn recursive_antijoin_reparent_keeps_matview_exact(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (groups, depth) = (6, 6);
    seed(&conn, groups, depth)?;
    assert_no_drift(&conn, "after seed")?;

    let rounds = std::env::var("TURSO_ANTIJOIN_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30usize);
    for round in 0..rounds {
        reset_tree(&conn, groups, depth)?;
        for g in 0..groups {
            for d in (2..depth).rev() {
                let (child, parent, gp, ggp) = link(g, d);
                outdent_chain_step(&conn, &child, &parent, &gp, &ggp)?;
                assert_no_drift(&conn, &format!("round {round} group {g} link {d}"))?;
            }
        }
    }
    Ok(())
}
