//! Test: an outdent chain where a node is re-adopted by its OWN moved parent in
//! the same transaction must not leave the intermediate derivation behind in a
//! recursive-CTE matview — even while a live CDC subscriber is cascading.
//!
//! Sequential form of this delta sequence is green (holon rung12/rung13). The
//! reported field failure only shows up in the composed engine, where live CDC
//! subscriptions fire cascades concurrently with the reparent transaction's
//! retraction. These tests add exactly that missing ingredient.
//!
//! Oracle: the matview's rowset must equal a recompute of its own defining
//! SELECT, after quiesce.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::common::TempDatabase;

/// The main-panel watch shape: a recursive CTE whose base case joins a
/// *dynamic* anchor matview (`focus_roots`), walking `blk` (itself a matview
/// over `blk_raw`). This is holon's `FOCUS_SWITCH_SQL`.
const FOCUS_SWITCH_SQL: &str = "\
    WITH RECURSIVE focus_descendants AS ( \
        SELECT b.id AS node_id, b.id AS source_id, 0 AS depth, CAST(b.id AS TEXT) AS visited \
        FROM blk b JOIN focus_roots fr ON b.id = fr.root_id \
        UNION ALL \
        SELECT child.id, focus_descendants.source_id, focus_descendants.depth + 1, \
               focus_descendants.visited || ',' || CAST(child.id AS TEXT) \
        FROM focus_descendants \
        JOIN blk child ON child.parent_id = focus_descendants.node_id \
        WHERE focus_descendants.depth < 20 \
          AND ',' || focus_descendants.visited || ',' NOT LIKE \
              '%,' || CAST(child.id AS TEXT) || ',%' \
    ) \
    SELECT d.id AS node_id, focus_descendants.depth AS depth \
    FROM focus_roots fr \
    JOIN blk root ON root.id = fr.root_id \
    JOIN focus_descendants ON focus_descendants.source_id = root.id \
    JOIN blk d ON d.id = focus_descendants.node_id \
    JOIN navigation_cursor nc ON nc.region = fr.region AND nc.history_id = fr.history_id \
    WHERE fr.region = 'main'";

/// Build the base tables + the two upstream matviews + the outer watch matview.
/// `groups` × `depth` chains hang off `doc`, mirroring the reported bulk tree.
fn seed(conn: &Arc<turso_core::Connection>, groups: usize, depth: usize) -> anyhow::Result<()> {
    conn.execute(
        "CREATE TABLE blk_raw (id TEXT PRIMARY KEY, parent_id TEXT, content TEXT, sort_key TEXT)",
    )?;
    conn.execute("INSERT INTO blk_raw VALUES ('doc', 'root', 'doc', 'a')")?;
    for g in 0..groups {
        let mut parent = "doc".to_string();
        for d in 0..depth {
            let id = format!("bulk-{g}-{d}");
            conn.execute(&format!(
                "INSERT INTO blk_raw VALUES ('{id}', '{parent}', '{id}', '{d}')"
            ))?;
            parent = id;
        }
    }
    conn.execute(
        "CREATE MATERIALIZED VIEW blk AS SELECT id, parent_id, content, sort_key FROM blk_raw",
    )?;

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
        "CREATE MATERIALIZED VIEW watch_view AS {FOCUS_SWITCH_SQL}"
    ))?;

    // A second chain hanging off the outer watch matview, so a CDC cascade has
    // somewhere to propagate to (holon's dependent-matview maintenance).
    conn.execute(
        "CREATE MATERIALIZED VIEW watch_depth_rollup AS \
         SELECT depth, count(*) AS n FROM watch_view GROUP BY depth",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW watch_joined AS \
         SELECT w.node_id, w.depth, b.sort_key \
         FROM watch_view w JOIN blk b ON b.id = w.node_id",
    )?;

    // A side table the cascade writes into, itself feeding a matview — this is
    // what makes the cascade re-enter the WRITE path, not just the read path.
    conn.execute("CREATE TABLE cascade_log (id INTEGER PRIMARY KEY, n INTEGER)")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW cascade_totals AS \
         SELECT n, count(*) AS c FROM cascade_log GROUP BY n",
    )?;
    Ok(())
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// `(node_id, depth)` rowset of an arbitrary query, canonicalized for compare.
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

/// The rung13 delta pair, on holon's `page → b0 → b4 → b5` shape where
/// `great_grandparent`=page, `grandparent`=b0, `parent`=b4, `child`=b5:
///
/// 1. Outdent(child): `child` rises to sit beside `parent` under `grandparent`.
/// 2. Outdent(parent), in ONE txn: `parent` rises to `great_grandparent`, and
///    `child` is re-adopted BY `parent` — so `child`'s new parent is the very
///    node that also moved in that batch.
///
/// The great-grandparent level is load-bearing: without it step 2's first
/// UPDATE is a no-op on a straight chain (a chain node's parent already IS its
/// grandparent), the re-adoption merely reverts step 1, and the whole sequence
/// collapses to a net-zero change that exercises nothing.
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

/// The `(child, parent, grandparent, great_grandparent)` ids for link `d` of
/// chain `g`, clamping the top of the chain to `doc`.
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

/// Re-indent every chain back to its pristine `doc → g-0 → g-1 → …` shape, so
/// the outdent sequence can be replayed round after round against a live,
/// already-populated matview (rather than a freshly-built one).
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

/// `focus_replace` + cursor move as ONE transaction — holon's `NavigateFocus`:
/// close the region's open history row, open a new one on `target`, point the
/// cursor at it. This retracts the whole old anchor closure and asserts a new
/// one through `focus_roots`.
fn navigate_focus_to(
    conn: &Arc<turso_core::Connection>,
    new_history_id: i64,
    target: &str,
) -> anyhow::Result<()> {
    conn.execute("BEGIN")?;
    conn.execute(
        "UPDATE navigation_history SET closed_at = '2026-07-27T00:00:00' \
         WHERE region = 'main' AND closed_at IS NULL",
    )?;
    conn.execute(&format!(
        "INSERT INTO navigation_history (id, region, block_id, closed_at) \
         VALUES ({new_history_id}, 'main', '{target}', NULL)"
    ))?;
    conn.execute(&format!(
        "UPDATE navigation_cursor SET history_id = {new_history_id} WHERE region = 'main'"
    ))?;
    conn.execute("COMMIT")?;
    Ok(())
}

/// Assert the matview agrees with a recompute of its own defining SELECT.
fn assert_no_drift(conn: &Arc<turso_core::Connection>, label: &str) -> anyhow::Result<()> {
    let matview = rowset(conn, "SELECT node_id, depth FROM watch_view")?;
    let recompute = rowset(conn, FOCUS_SWITCH_SQL)?;
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

/// Baseline: the same delta sequence with NO CDC subscriber. Locks in that the
/// sequential path is correct, so a failure in the cascading tests below is
/// attributable to the CDC cascade and not to the delta shape itself.
#[turso_macros::test(views)]
fn test_outdent_chain_readoption_without_cdc(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (groups, depth) = (6, 6);
    seed(&conn, groups, depth)?;

    let rounds = env_usize("TURSO_RACE_ROUNDS", 20);
    for round in 0..rounds {
        reset_tree(&conn, groups, depth)?;
        for g in 0..groups {
            for d in (2..depth).rev() {
                let (child, parent, gp, ggp) = link(g, d);
                outdent_chain_step(&conn, &child, &parent, &gp, &ggp)?;
            }
            assert_no_drift(&conn, &format!("round {round} group {g} (no cdc)"))?;
        }
    }
    Ok(())
}

/// The reported shape: a LIVE CDC subscriber on the outer watch matview whose
/// callback cascades — it re-enters the database on a second connection and
/// reads the very matview being maintained, while the reparent transaction's
/// retraction is in flight.
#[turso_macros::test(views)]
fn test_outdent_chain_readoption_with_cascading_cdc(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let tmp_db = Arc::new(tmp_db);
    let conn = tmp_db.connect_limbo();
    let (groups, depth) = (6, 6);
    seed(&conn, groups, depth)?;

    // The cascade connection: a second connection the callback drives, standing
    // in for holon's dependent-matview maintenance / re-entrant watch reads.
    let cascade_conn = tmp_db.connect_limbo();
    let cascade_errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fired = Arc::new(AtomicUsize::new(0));

    let cascade_sink = cascade_errors.clone();
    let fired_sink = fired.clone();
    conn.set_change_callback_for(&["watch_view"], move |event| {
        let n = fired_sink.fetch_add(event.changes.len(), Ordering::SeqCst);
        // Re-entrant READS of the matview under maintenance and of the two
        // matviews chained off it — dependent-matview maintenance.
        for sql in [
            "SELECT node_id, depth FROM watch_view",
            "SELECT node_id, depth FROM watch_joined",
            "SELECT depth, n FROM watch_depth_rollup",
        ] {
            if let Err(e) = rowset(&cascade_conn, sql) {
                cascade_sink.lock().unwrap().push(format!("{sql}: {e}"));
            }
        }
        // Re-entrant WRITE attempt from the cascade connection. While the
        // reparent txn holds the write lock this is legitimately rejected as
        // busy — that rejection is expected and is NOT a defect, so only
        // non-busy failures are recorded.
        if let Err(e) = cascade_conn.execute(&format!(
            "INSERT OR REPLACE INTO cascade_log (id, n) VALUES ({}, {})",
            n % 64,
            event.changes.len()
        )) {
            let msg = e.to_string();
            if !msg.contains("busy") {
                cascade_sink
                    .lock()
                    .unwrap()
                    .push(format!("cascade write: {msg}"));
            }
        }
    });

    let rounds = env_usize("TURSO_RACE_ROUNDS", 20);
    for round in 0..rounds {
        reset_tree(&conn, groups, depth)?;
        assert_no_drift(&conn, &format!("round {round} after reset (cascading cdc)"))?;
        for g in 0..groups {
            for d in (2..depth).rev() {
                let (child, parent, gp, ggp) = link(g, d);
                outdent_chain_step(&conn, &child, &parent, &gp, &ggp)?;
            }
            // Post-commit cascade write on the writing connection: this is how
            // holon's actor actually applies dependent maintenance (serialized
            // behind the same write lock), and it drives cascade_totals.
            conn.execute(&format!(
                "INSERT OR REPLACE INTO cascade_log (id, n) VALUES ({}, {g})",
                g % 64
            ))?;
            assert_no_drift(&conn, &format!("round {round} group {g} (cascading cdc)"))?;
        }
    }

    assert!(
        fired.load(Ordering::SeqCst) > 0,
        "the CDC subscriber never fired — the cascade ingredient was not actually exercised"
    );
    let errs = cascade_errors.lock().unwrap();
    assert!(errs.is_empty(), "cascade reads/writes failed: {errs:?}");
    Ok(())
}

/// True concurrency: a reader thread hammers the outer matview on its own
/// connection for the whole duration of the outdent transactions, so matview
/// maintenance and reads genuinely interleave.
#[turso_macros::test(views)]
fn test_outdent_chain_readoption_with_concurrent_reader(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let tmp_db = Arc::new(tmp_db);
    let conn = tmp_db.connect_limbo();
    let (groups, depth) = (6, 6);
    seed(&conn, groups, depth)?;

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicUsize::new(0));

    let readers: Vec<_> = (0..env_usize("TURSO_RACE_READERS", 3))
        .map(|i| {
            let tmp_db = tmp_db.clone();
            let stop = stop.clone();
            let reads = reads.clone();
            std::thread::spawn(move || {
                let rconn = tmp_db.connect_limbo();
                let sql = match i % 3 {
                    0 => "SELECT node_id, depth FROM watch_view",
                    1 => "SELECT node_id, depth FROM watch_joined",
                    _ => "SELECT depth, n FROM watch_depth_rollup",
                };
                while !stop.load(Ordering::SeqCst) {
                    if rowset(&rconn, sql).is_ok() {
                        reads.fetch_add(1, Ordering::SeqCst);
                    }
                }
            })
        })
        .collect();

    let rounds = env_usize("TURSO_RACE_ROUNDS", 20);
    let mut drift = None;
    'outer: for round in 0..rounds {
        reset_tree(&conn, groups, depth)?;
        for g in 0..groups {
            for d in (2..depth).rev() {
                let (child, parent, gp, ggp) = link(g, d);
                outdent_chain_step(&conn, &child, &parent, &gp, &ggp)?;
            }
            if let Err(e) = assert_no_drift(
                &conn,
                &format!("round {round} group {g} (concurrent reader)"),
            ) {
                drift = Some(e);
                break 'outer;
            }
        }
    }

    stop.store(true, Ordering::SeqCst);
    for r in readers {
        r.join().unwrap();
    }

    if let Some(e) = drift {
        return Err(e);
    }
    assert!(
        reads.load(Ordering::SeqCst) > 0,
        "the concurrent reader never completed a read — no interleaving was exercised"
    );
    Ok(())
}

/// Anchor movement interleaved with the outdent chain: `focus_roots` (the
/// recursive CTE's base case) is itself a matview, and navigation retracts the
/// whole old closure and asserts a new one. Doing that WHILE the outdent chain
/// is reparenting means the anchor delta and the reparent delta are in flight
/// against the same recursive operator.
#[turso_macros::test(views)]
fn test_outdent_chain_readoption_with_anchor_switching(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let tmp_db = Arc::new(tmp_db);
    let conn = tmp_db.connect_limbo();
    let (groups, depth) = (6, 6);
    seed(&conn, groups, depth)?;

    let cascade_conn = tmp_db.connect_limbo();
    let fired = Arc::new(AtomicUsize::new(0));
    let fired_sink = fired.clone();
    conn.set_change_callback_for(&["watch_view"], move |event| {
        fired_sink.fetch_add(event.changes.len(), Ordering::SeqCst);
        let _ = rowset(&cascade_conn, "SELECT node_id, depth FROM watch_view");
        let _ = rowset(&cascade_conn, "SELECT node_id, depth FROM watch_joined");
    });

    let rounds = env_usize("TURSO_RACE_ROUNDS", 20);
    let mut history_id = 1i64;
    for round in 0..rounds {
        reset_tree(&conn, groups, depth)?;
        for g in 0..groups {
            // Re-anchor onto this group's chain head, then back onto `doc`,
            // straddling the outdent steps.
            history_id += 1;
            navigate_focus_to(&conn, history_id, &format!("bulk-{g}-0"))?;

            for d in (2..depth).rev() {
                let (child, parent, gp, ggp) = link(g, d);
                outdent_chain_step(&conn, &child, &parent, &gp, &ggp)?;
            }
            assert_no_drift(
                &conn,
                &format!("round {round} group {g} (anchored on bulk-{g}-0)"),
            )?;

            history_id += 1;
            navigate_focus_to(&conn, history_id, "doc")?;
            assert_no_drift(
                &conn,
                &format!("round {round} group {g} (re-anchored on doc)"),
            )?;
        }
    }

    assert!(
        fired.load(Ordering::SeqCst) > 0,
        "the CDC subscriber never fired — the cascade ingredient was not exercised"
    );
    Ok(())
}

/// NEGATIVE CONTROL for the oracle itself. Every test above passes only because
/// `rowset` + the matview-vs-recompute comparison would have *caught* a real
/// divergence. If the comparison were insensitive (e.g. `rowset` silently
/// returned empty, or the recompute resolved to the matview's own storage),
/// the green results would be meaningless. This pins that it discriminates.
#[turso_macros::test(views)]
fn test_drift_detector_is_sensitive(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    seed(&conn, 3, 5)?;

    let matview = rowset(&conn, "SELECT node_id, depth FROM watch_view")?;
    assert!(
        !matview.is_empty(),
        "matview read returned nothing — the oracle would pass vacuously"
    );

    // A deliberately truncated recompute: the real closure walks to depth 20,
    // this one stops at 2, so the two sides MUST differ.
    let truncated = FOCUS_SWITCH_SQL.replace("depth < 20", "depth < 2");
    let wrong = rowset(&conn, &truncated)?;
    assert_ne!(
        matview, wrong,
        "the drift detector cannot distinguish a truncated closure from the real \
         one — every green result in this file would be vacuous"
    );

    // And the honest recompute must agree, on the same code path.
    let recompute = rowset(&conn, FOCUS_SWITCH_SQL)?;
    assert_eq!(
        matview, recompute,
        "freshly-populated matview already disagrees with its defining SELECT"
    );
    Ok(())
}

/// CONTROL: the outdent sequence must actually MUTATE the tree.
///
/// A 3-level step (`parent` moved to its `grandparent`) is a no-op on a straight
/// chain: the re-adoption just reverts the child's move, for a net-zero change.
/// This pins that the sequence does real work, so a green result means "no drift"
/// rather than "nothing happened".
#[turso_macros::test(views)]
fn test_outdent_sequence_actually_mutates_the_tree(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (groups, depth) = (2, 6);
    seed(&conn, groups, depth)?;

    let before = rowset(&conn, "SELECT node_id, depth FROM watch_view")?;
    for g in 0..groups {
        for d in (2..depth).rev() {
            let (child, parent, gp, ggp) = link(g, d);
            outdent_chain_step(&conn, &child, &parent, &gp, &ggp)?;
        }
    }
    let after = rowset(&conn, "SELECT node_id, depth FROM watch_view")?;

    assert_ne!(
        before, after,
        "the outdent sequence left the tree unchanged — it is a no-op and every \
         other test in this file is vacuous"
    );
    // And the mutation must be a real flattening, not just churn: outdenting
    // lifts upper chain nodes toward `doc`, so total depth must strictly drop.
    let total_depth = |rows: &[String]| -> i64 {
        rows.iter()
            .filter_map(|r| r.rsplit_once("@d").and_then(|(_, d)| d.parse::<i64>().ok()))
            .sum()
    };
    let (sum_before, sum_after) = (total_depth(&before), total_depth(&after));
    assert!(
        sum_after < sum_before,
        "outdenting must flatten the tree: total depth before={sum_before} after={sum_after}"
    );
    assert_no_drift(&conn, "after mutation control")?;
    Ok(())
}
