//! Secondary indexes on materialized views.
//!
//! A matview is registered in the schema as an ordinary rowid `BTreeTable`
//! (`core/schema.rs`), yet `CREATE INDEX` on one is refused
//! (`core/translate/index.rs`). A join matview's rowid is a hash of the whole
//! joined row, so no user-visible column is seekable either: every point read
//! on a matview is a full `SCAN`, linear in the view's size.
//!
//! These tests pin the two halves of the fix: the plan must become a `SEARCH`,
//! and the index must stay in step with the view under arbitrary base-table
//! churn — including updates that move a row across the indexed column, and
//! no-op updates.

use std::sync::Arc;

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rusqlite::types::Value;

use crate::common::{limbo_exec_rows, TempDatabase};

/// `block_raw` + a tag junction + the aggregate matview it feeds, then the
/// join matview `block` on top — the shape Holon reads through.
fn setup_schema(conn: &Arc<turso_core::Connection>) -> anyhow::Result<()> {
    conn.execute(
        "CREATE TABLE block_raw (\
            id TEXT PRIMARY KEY, \
            parent_id TEXT, \
            content TEXT NOT NULL DEFAULT '')",
    )?;
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
            LEFT OUTER JOIN block_tags_agg ON block_tags_agg.source_id = b.id \
            WHERE b.id != 'sentinel:no_parent'",
    )?;
    Ok(())
}

fn sort_rows(rows: &mut [Vec<Value>]) {
    rows.sort_by_key(|r| format!("{r:?}"));
}

fn plan_of(conn: &Arc<turso_core::Connection>, query: &str) -> String {
    limbo_exec_rows(conn, &format!("EXPLAIN QUERY PLAN {query}"))
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|v| match v {
                    Value::Text(t) => t,
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The plan half: with an index on the matview, a point read must SEARCH.
#[turso_macros::test(views)]
fn matview_point_read_uses_index(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    conn.execute("INSERT INTO block_raw VALUES ('b1', 'root', 'one')")?;
    conn.execute("INSERT INTO block_raw VALUES ('b2', 'b1', 'two')")?;

    conn.execute("CREATE INDEX idx_block_id ON block(id)")?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;

    let by_id = plan_of(&conn, "SELECT content FROM block WHERE id = 'b2'");
    assert!(
        by_id.contains("SEARCH") && by_id.contains("idx_block_id"),
        "point read on the matview by id must use the index, plan was:\n{by_id}"
    );

    let by_parent = plan_of(&conn, "SELECT id FROM block WHERE parent_id = 'b1'");
    assert!(
        by_parent.contains("SEARCH") && by_parent.contains("idx_block_parent"),
        "point read on the matview by parent_id must use the index, plan was:\n{by_parent}"
    );

    // The index must also answer correctly, not merely be chosen.
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT content FROM block WHERE id = 'b2'"),
        vec![vec![Value::Text("two".into())]],
    );
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id FROM block WHERE parent_id = 'b1'"),
        vec![vec![Value::Text("b2".into())]],
    );
    Ok(())
}

/// Every live row of the view, read by a predicate-free scan. The oracle the
/// indexed point queries are held against.
fn scan_view(conn: &Arc<turso_core::Connection>) -> Vec<Vec<Value>> {
    let mut rows = limbo_exec_rows(conn, "SELECT id, parent_id, content, tags FROM block");
    sort_rows(&mut rows);
    rows
}

/// Recompute the view's defining SELECT straight off the base tables — an
/// oracle that does not touch the view's btree at all.
fn recompute_view(conn: &Arc<turso_core::Connection>) -> Vec<Vec<Value>> {
    let mut rows = limbo_exec_rows(
        &conn.clone(),
        "SELECT b.id, b.parent_id, b.content, \
                COALESCE((SELECT json_group_array(tag) FROM block_tags t \
                          WHERE t.block_id = b.id), '[]') \
         FROM block_raw b WHERE b.id != 'sentinel:no_parent'",
    );
    sort_rows(&mut rows);
    rows
}

/// `json_group_array` over an IVM aggregate is a multiset: the element order
/// it emits need not match a correlated-subquery recompute's. Compare the tag
/// column as a set so the oracle tests membership, not aggregate ordering.
fn normalize_tags(rows: &[Vec<Value>]) -> Vec<Vec<Value>> {
    rows.iter()
        .map(|r| {
            let mut r = r.clone();
            if let Value::Text(t) = &r[3] {
                let mut parts: Vec<&str> = t
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .collect();
                parts.sort_unstable();
                r[3] = Value::Text(parts.join(","));
            }
            r
        })
        .collect()
}

/// 200 steps of base-table churn — inserts, updates that move a row across
/// the indexed column, content-only updates, no-op updates and deletes. After
/// every step `check` is handed the step number and a full scan of the view.
///
/// The scan is the oracle for the index, and deliberately not the view's
/// defining SELECT: the join matview drifts from that on its own, with or
/// without an index (see `join_matview_drifts_from_defining_select`).
fn churn(
    conn: &Arc<turso_core::Connection>,
    ids: &[String],
    parents: &[String],
    mut check: impl FnMut(usize, &[Vec<Value>]),
) -> anyhow::Result<()> {
    let mut rng = ChaCha8Rng::seed_from_u64(0xB10C_1DEA);
    for step in 0..200 {
        let id = &ids[rng.random_range(0..ids.len())];
        match rng.random_range(0..6) {
            0 | 1 => {
                let parent = &parents[rng.random_range(0..parents.len())];
                conn.execute(&format!(
                    "INSERT OR REPLACE INTO block_raw VALUES ('{id}', '{parent}', 'c{step}')"
                ))?;
            }
            2 => {
                // UPDATE that moves the row across the indexed column.
                let parent = &parents[rng.random_range(0..parents.len())];
                conn.execute(&format!(
                    "UPDATE block_raw SET parent_id = '{parent}' WHERE id = '{id}'"
                ))?;
            }
            3 => {
                // Content-only UPDATE: the indexed columns do not move.
                conn.execute(&format!(
                    "UPDATE block_raw SET content = 'c{step}' WHERE id = '{id}'"
                ))?;
            }
            4 => {
                // No-op UPDATE: writes the value the row already holds.
                conn.execute(&format!(
                    "UPDATE block_raw SET parent_id = parent_id WHERE id = '{id}'"
                ))?;
            }
            _ => {
                conn.execute(&format!("DELETE FROM block_raw WHERE id = '{id}'"))?;
                conn.execute(&format!("DELETE FROM block_tags WHERE block_id = '{id}'"))?;
            }
        }
        if rng.random_bool(0.3) {
            conn.execute(&format!(
                "INSERT INTO block_tags VALUES ('{id}', 't{}')",
                rng.random_range(0..3)
            ))?;
        }

        let scanned = scan_view(conn);
        check(step, &scanned);
    }
    Ok(())
}

fn churn_ids() -> (Vec<String>, Vec<String>) {
    (
        (0..12).map(|i| format!("b{i}")).collect(),
        ["root", "b0", "b1", "b2"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    )
}

/// A PRE-EXISTING defect, not an index one: under the same churn, and with no
/// index anywhere, the join matview `block` keeps a stale duplicate row. At
/// step 102 it holds b7 twice — once with the tag it has just gained, once
/// with the `[]` it had before. A join row's rowid is a hash of the whole
/// joined row (`core/incremental/join_operator.rs`), so the pre-tag row is
/// retracted at a rowid that no longer matches and survives in the btree.
/// Ignored because it is red on this revision; unignore it with the fix.
#[turso_macros::test(views)]
#[ignore = "pre-existing: join matview keeps a stale row when the aggregate side changes"]
fn join_matview_drifts_from_defining_select(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    let (ids, parents) = churn_ids();
    churn(&conn, &ids, &parents, |step, scanned| {
        assert_eq!(
            normalize_tags(scanned),
            normalize_tags(&recompute_view(&tmp_db.connect_limbo())),
            "step {step}: the view drifted from its defining SELECT with NO index present"
        );
    })
}

/// `PRAGMA integrity_check`, flattened. The only oracle that can see an index
/// entry pointing at a rowid the view's btree does not hold: a point read
/// compared against a scan cannot, because a dangling entry contributes no row
/// to either side.
fn integrity_check(conn: &Arc<turso_core::Connection>) -> Vec<String> {
    limbo_exec_rows(conn, "PRAGMA integrity_check")
        .into_iter()
        .flatten()
        .map(|v| match v {
            Value::Text(t) => t,
            other => format!("{other:?}"),
        })
        .filter(|s| s != "ok")
        .collect()
}

/// The correctness half: under the same churn, an indexed point query must
/// agree with a full scan of the view, for every id and every parent_id —
/// including keys that hold no row.
#[turso_macros::test(views)]
fn matview_index_tracks_base_table_churn(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    conn.execute("CREATE INDEX idx_block_id ON block(id)")?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;
    let (ids, parents) = churn_ids();
    let probe_conn = conn.clone();
    let probe_ids = ids.clone();
    let probe_parents = parents.clone();
    churn(&conn, &ids, &parents, move |step, scanned| {
        for (col, probes) in [(0usize, &probe_ids), (1usize, &probe_parents)] {
            let column = if col == 0 { "id" } else { "parent_id" };
            for probe in probes {
                let mut got = limbo_exec_rows(
                    &probe_conn,
                    &format!(
                        "SELECT id, parent_id, content, tags FROM block WHERE {column} = '{probe}'"
                    ),
                );
                sort_rows(&mut got);
                let mut want: Vec<Vec<Value>> = scanned
                    .iter()
                    .filter(|r| r[col] == Value::Text(probe.clone()))
                    .cloned()
                    .collect();
                sort_rows(&mut want);
                assert_eq!(
                    got, want,
                    "step {step}: indexed point read of {column}='{probe}' disagrees with a \
                     full scan of the view"
                );
            }
        }
        let problems = integrity_check(&probe_conn);
        assert!(
            problems.is_empty(),
            "step {step}: integrity_check reported {problems:?}"
        );
    })
}

/// Holon's real read predicate (`crates/holon/src/api/ui_watcher.rs:54-61`) is
/// a disjunction over the two indexed columns. On the base table the planner
/// answers it with a MULTI-INDEX OR; on the view it must do the same, or the
/// index buys Holon nothing on the read that matters.
#[turso_macros::test(views)]
fn matview_or_predicate_uses_both_indexes(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    conn.execute("CREATE INDEX idx_block_id ON block(id)")?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;
    for i in 0..20 {
        conn.execute(&format!(
            "INSERT INTO block_raw VALUES ('b{i}', 'b{}', 'c{i}')",
            i / 4
        ))?;
    }

    let plan = plan_of(
        &conn,
        "SELECT id FROM block WHERE id = 'b3' OR parent_id = 'b3'",
    );
    assert!(
        !plan.contains("SCAN block"),
        "Holon's `id = ? OR parent_id = ?` on the matview must not be a full scan, plan was:\n{plan}"
    );

    let mut got = limbo_exec_rows(
        &conn,
        "SELECT id FROM block WHERE id = 'b3' OR parent_id = 'b3'",
    );
    sort_rows(&mut got);
    let mut want = limbo_exec_rows(
        &conn,
        "SELECT id FROM block_raw WHERE (id = 'b3' OR parent_id = 'b3') \
         AND id != 'sentinel:no_parent'",
    );
    sort_rows(&mut want);
    assert_eq!(
        got, want,
        "the OR plan must return the same rows as the base table"
    );
    Ok(())
}

/// Holon's own vault shape, loaded the way Holon loads it: the full column
/// list, the `sentinel:no_parent` anchor the view filters out, a real tree of
/// parents, and the rows arriving in ONE transaction. An index seek then lands
/// on a leaf-page boundary (`SeekResult::TryAdvance`), and resolving that by
/// re-seeking spins the commit forever. The narrower shapes above never reach
/// one, so this is the test that holds the maintenance loop honest.
#[turso_macros::test(views)]
fn matview_index_on_holon_vault_shape(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    conn.execute(
        "CREATE TABLE block_raw (\
            id TEXT PRIMARY KEY, parent_id TEXT, sort_key TEXT NOT NULL DEFAULT 'A0', \
            content TEXT NOT NULL DEFAULT '', content_type TEXT NOT NULL DEFAULT 'text', \
            properties TEXT, collapsed INTEGER NOT NULL DEFAULT 0, \
            created_at INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL DEFAULT 0, \
            write_seq INTEGER NOT NULL DEFAULT 0)",
    )?;
    conn.execute("CREATE INDEX idx_block_raw_parent_id ON block_raw(parent_id)")?;
    conn.execute("CREATE TABLE block_tags (block_id TEXT NOT NULL, tag TEXT NOT NULL)")?;
    conn.execute("CREATE INDEX idx_block_tags_block_id ON block_tags(block_id)")?;
    conn.execute(
        "CREATE MATERIALIZED VIEW block_tags_agg AS \
            SELECT block_id AS source_id, json_group_array(tag) AS vals \
            FROM block_tags GROUP BY block_id",
    )?;
    conn.execute(
        "CREATE MATERIALIZED VIEW block AS \
            SELECT b.id, b.parent_id, b.sort_key, b.content, b.content_type, b.properties, \
                   b.collapsed, b.created_at, b.updated_at, b.write_seq, \
                   COALESCE(block_tags_agg.vals, '[]') AS tags \
            FROM block_raw b \
            LEFT OUTER JOIN block_tags_agg ON block_tags_agg.source_id = b.id \
            WHERE b.id != 'sentinel:no_parent'",
    )?;

    const N: usize = 200;
    const FANOUT: usize = 5;
    conn.execute("BEGIN")?;
    conn.execute(
        "INSERT INTO block_raw (id, parent_id, content) \
         VALUES ('sentinel:no_parent', 'sentinel:no_parent', '')",
    )?;
    let rows: Vec<String> = (0..N)
        .map(|i| {
            let parent = if i == 0 {
                "sentinel:no_parent".to_string()
            } else {
                format!("block:{:07}", (i - 1) / FANOUT)
            };
            format!("('block:{i:07}','{parent}','content for block {i} lorem ipsum dolor',{i},{i})")
        })
        .collect();
    conn.execute(&format!(
        "INSERT INTO block_raw (id,parent_id,content,created_at,updated_at) VALUES {}",
        rows.join(",")
    ))?;
    let tags: Vec<String> = (0..N)
        .step_by(3)
        .flat_map(|i| {
            [
                format!("('block:{i:07}','tag{}')", i % 7),
                format!("('block:{i:07}','tag{}')", i % 11),
            ]
        })
        .collect();
    conn.execute(&format!(
        "INSERT INTO block_tags (block_id,tag) VALUES {}",
        tags.join(",")
    ))?;
    conn.execute("COMMIT")?;

    conn.execute("CREATE INDEX idx_block_id ON block(id)")?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;

    for i in 0..80 {
        let target = (i * 7919) % N;
        conn.execute(&format!(
            "UPDATE block_raw SET content = 'e{i}' WHERE id = 'block:{target:07}'"
        ))?;
    }

    let mut scanned = limbo_exec_rows(&conn, "SELECT id, parent_id, content FROM block");
    sort_rows(&mut scanned);
    assert_eq!(
        scanned.len(),
        N,
        "the sentinel row must stay out of the view"
    );

    for i in [0usize, 7, 93, 199] {
        let probe = format!("block:{i:07}");
        let mut got = limbo_exec_rows(
            &conn,
            &format!("SELECT id, parent_id, content FROM block WHERE id = '{probe}'"),
        );
        sort_rows(&mut got);
        let mut want: Vec<Vec<Value>> = scanned
            .iter()
            .filter(|r| r[0] == Value::Text(probe.clone()))
            .cloned()
            .collect();
        sort_rows(&mut want);
        assert_eq!(
            got, want,
            "indexed read of id='{probe}' disagrees with a scan"
        );
        assert_eq!(want.len(), 1, "{probe} must be present exactly once");
    }

    let problems = integrity_check(&conn);
    assert!(problems.is_empty(), "integrity_check reported {problems:?}");
    Ok(())
}

/// A multi-column key: the entry layout is `[c1, c2, rowid]`, so a wrong
/// key-position order or a missed column shows up as a wrong row here.
#[turso_macros::test(views)]
fn matview_multi_column_index(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    conn.execute("CREATE INDEX idx_block_pc ON block(parent_id, content)")?;
    let (ids, parents) = churn_ids();
    let probe_conn = conn.clone();
    churn(&conn, &ids, &parents, move |step, scanned| {
        for row in scanned {
            let (Value::Text(parent), Value::Text(content)) = (&row[1], &row[2]) else {
                panic!("step {step}: unexpected view row shape {row:?}");
            };
            let mut got = limbo_exec_rows(
                &probe_conn,
                &format!(
                    "SELECT id, parent_id, content, tags FROM block \
                     WHERE parent_id = '{parent}' AND content = '{content}'"
                ),
            );
            sort_rows(&mut got);
            let mut want: Vec<Vec<Value>> = scanned
                .iter()
                .filter(|r| r[1] == row[1] && r[2] == row[2])
                .cloned()
                .collect();
            sort_rows(&mut want);
            assert_eq!(
                got, want,
                "step {step}: composite-index read (parent_id={parent}, content={content}) \
                 disagrees with a full scan"
            );
        }
        let problems = integrity_check(&probe_conn);
        assert!(
            problems.is_empty(),
            "step {step}: integrity_check reported {problems:?}"
        );
    })
}

/// An index on the AGGREGATE matview, and on the RECURSIVE matview that reads
/// the join view. Increment 0 measured both as full scans; both are ordinary
/// rowid btree tables, so both must index and stay in step.
#[turso_macros::test(views)]
fn aggregate_and_recursive_matview_indexes(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    conn.execute(
        "CREATE MATERIALIZED VIEW block_with_path AS \
         WITH RECURSIVE paths AS ( \
             SELECT id, parent_id, '/' || id AS path FROM block \
             WHERE parent_id LIKE 'sentinel:%' \
             UNION ALL \
             SELECT b.id, b.parent_id, p.path || '/' || b.id \
             FROM block b INNER JOIN paths p ON b.parent_id = p.id) \
         SELECT * FROM paths",
    )?;
    conn.execute("CREATE INDEX idx_agg_source ON block_tags_agg(source_id)")?;
    conn.execute("CREATE INDEX idx_bwp_id ON block_with_path(id)")?;

    for i in 0..12 {
        let parent = if i == 0 {
            "sentinel:root".to_string()
        } else {
            format!("b{}", (i - 1) / 3)
        };
        conn.execute(&format!(
            "INSERT INTO block_raw VALUES ('b{i}', '{parent}', 'c{i}')"
        ))?;
        conn.execute(&format!(
            "INSERT INTO block_tags VALUES ('b{i}', 't{}')",
            i % 3
        ))?;
    }

    for view in ["block_tags_agg", "block_with_path"] {
        let col = if view == "block_tags_agg" {
            "source_id"
        } else {
            "id"
        };
        let plan = plan_of(&conn, &format!("SELECT * FROM {view} WHERE {col} = 'b5'"));
        assert!(
            plan.contains("SEARCH"),
            "point read on {view} must use its index, plan was:\n{plan}"
        );
    }

    // Correctness against a predicate-free scan of the same view.
    let mut got = limbo_exec_rows(
        &conn,
        "SELECT id, path FROM block_with_path WHERE id = 'b5'",
    );
    sort_rows(&mut got);
    let mut want: Vec<Vec<Value>> = limbo_exec_rows(&conn, "SELECT id, path FROM block_with_path")
        .into_iter()
        .filter(|r| r[0] == Value::Text("b5".into()))
        .collect();
    sort_rows(&mut want);
    assert_eq!(
        got, want,
        "indexed read of block_with_path disagrees with a scan"
    );
    assert!(!want.is_empty(), "b5 must exist in the recursive view");

    // A write must keep BOTH downstream views' indexes in step.
    conn.execute("UPDATE block_raw SET parent_id = 'b0' WHERE id = 'b5'")?;
    conn.execute("INSERT INTO block_tags VALUES ('b5', 'tz')")?;
    let mut got = limbo_exec_rows(
        &conn,
        "SELECT id, path FROM block_with_path WHERE id = 'b5'",
    );
    sort_rows(&mut got);
    let mut want: Vec<Vec<Value>> = limbo_exec_rows(&conn, "SELECT id, path FROM block_with_path")
        .into_iter()
        .filter(|r| r[0] == Value::Text("b5".into()))
        .collect();
    sort_rows(&mut want);
    assert_eq!(
        got, want,
        "after a write the recursive view's index disagrees with a scan"
    );

    let problems = integrity_check(&conn);
    assert!(problems.is_empty(), "integrity_check reported {problems:?}");
    Ok(())
}

/// A view big enough that an index seek lands on a leaf-page boundary, which
/// the btree reports as `SeekResult::TryAdvance`. The 12-row churn above never
/// reaches one; a real vault reaches one immediately.
#[turso_macros::test(views)]
fn matview_index_handles_leaf_boundary_seeks(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    conn.execute("BEGIN")?;
    for i in 0..400 {
        conn.execute(&format!(
            "INSERT INTO block_raw VALUES ('b{i:05}', 'p{}', 'content for block {i}')",
            i / 5
        ))?;
    }
    conn.execute("COMMIT")?;
    conn.execute("CREATE INDEX idx_block_id ON block(id)")?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;

    for i in (0..400).step_by(17) {
        conn.execute(&format!(
            "UPDATE block_raw SET content = 'edited {i}' WHERE id = 'b{i:05}'"
        ))?;
        conn.execute(&format!(
            "UPDATE block_raw SET parent_id = 'p{}' WHERE id = 'b{i:05}'",
            (i + 3) / 5
        ))?;
    }
    conn.execute("DELETE FROM block_raw WHERE id = 'b00100'")?;
    conn.execute("INSERT INTO block_raw VALUES ('b00100', 'p9', 'reborn')")?;

    let mut scanned = limbo_exec_rows(&conn, "SELECT id, parent_id FROM block");
    sort_rows(&mut scanned);
    assert_eq!(scanned.len(), 400);

    for probe in ["b00000", "b00017", "b00100", "b00399"] {
        let mut got = limbo_exec_rows(
            &conn,
            &format!("SELECT id, parent_id FROM block WHERE id = '{probe}'"),
        );
        sort_rows(&mut got);
        let mut want: Vec<Vec<Value>> = scanned
            .iter()
            .filter(|r| r[0] == Value::Text(probe.into()))
            .cloned()
            .collect();
        sort_rows(&mut want);
        assert_eq!(
            got, want,
            "indexed read of id='{probe}' disagrees with a scan"
        );
    }
    for probe in ["p0", "p9", "p40", "p79"] {
        let mut got = limbo_exec_rows(
            &conn,
            &format!("SELECT id, parent_id FROM block WHERE parent_id = '{probe}'"),
        );
        sort_rows(&mut got);
        let mut want: Vec<Vec<Value>> = scanned
            .iter()
            .filter(|r| r[1] == Value::Text(probe.into()))
            .cloned()
            .collect();
        sort_rows(&mut want);
        assert_eq!(
            got, want,
            "indexed read of parent_id='{probe}' disagrees with a scan"
        );
    }

    let problems = integrity_check(&conn);
    assert!(problems.is_empty(), "integrity_check reported {problems:?}");
    Ok(())
}

/// `DROP INDEX` (not `DROP VIEW`): the btree goes, the plan falls back to a
/// scan, and delta application must stop maintaining an index that is gone.
#[turso_macros::test(views)]
fn drop_index_on_matview_stops_maintenance(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;
    for i in 0..10 {
        conn.execute(&format!(
            "INSERT INTO block_raw VALUES ('b{i}', 'p{}', 'c{i}')",
            i % 2
        ))?;
    }
    conn.execute("DROP INDEX idx_block_parent")?;
    assert_eq!(
        limbo_exec_rows(
            &conn,
            "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name='idx_block_parent'"
        ),
        vec![vec![Value::Integer(0)]],
    );

    // Writes after the drop must not touch the freed btree.
    for i in 10..20 {
        conn.execute(&format!(
            "INSERT INTO block_raw VALUES ('b{i}', 'p{}', 'c{i}')",
            i % 2
        ))?;
    }
    conn.execute("DELETE FROM block_raw WHERE id = 'b3'")?;
    // 20 rows, odd ids sit under p1, less the deleted b3.
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM block WHERE parent_id = 'p1'"),
        vec![vec![Value::Integer(9)]],
    );
    let problems = integrity_check(&conn);
    assert!(problems.is_empty(), "integrity_check reported {problems:?}");

    // And the index can be created again, backfilled from the current view.
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM block WHERE parent_id = 'p1'"),
        vec![vec![Value::Integer(9)]],
    );
    Ok(())
}

/// A rolled-back transaction must leave the index exactly as it found it —
/// the index write shares the view write's transaction or it does not.
#[turso_macros::test(views)]
fn matview_index_rolls_back_with_the_view(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;
    conn.execute("INSERT INTO block_raw VALUES ('b1', 'p0', 'one')")?;

    conn.execute("BEGIN")?;
    conn.execute("INSERT INTO block_raw VALUES ('b2', 'p0', 'two')")?;
    conn.execute("DELETE FROM block_raw WHERE id = 'b1'")?;
    conn.execute("ROLLBACK")?;

    let mut got = limbo_exec_rows(&conn, "SELECT id FROM block WHERE parent_id = 'p0'");
    sort_rows(&mut got);
    assert_eq!(
        got,
        vec![vec![Value::Text("b1".into())]],
        "the rolled-back insert/delete must not be visible through the index"
    );
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id FROM block"),
        vec![vec![Value::Text("b1".into())]],
        "nor through a scan"
    );
    let problems = integrity_check(&conn);
    assert!(problems.is_empty(), "integrity_check reported {problems:?}");
    Ok(())
}

/// The shapes `CREATE INDEX` refuses on a view. Each refusal is load-bearing:
/// `Schema::matview_indexes` asserts these never reach delta application.
#[turso_macros::test(views)]
fn matview_index_refuses_unsupported_shapes(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    conn.execute(
        "CREATE MATERIALIZED VIEW block_sorted AS \
         SELECT id, parent_id FROM block_raw ORDER BY 1",
    )?;

    for (sql, want) in [
        (
            "CREATE UNIQUE INDEX i1 ON block(id)",
            "UNIQUE index on materialized view",
        ),
        (
            "CREATE INDEX i2 ON block(parent_id) WHERE parent_id IS NOT NULL",
            "partial index on materialized view",
        ),
        (
            "CREATE INDEX i3 ON block(lower(content))",
            "expression index on materialized view",
        ),
        (
            "CREATE INDEX i4 ON block_sorted(parent_id)",
            "the view has ORDER BY",
        ),
    ] {
        let err = conn
            .execute(sql)
            .expect_err(&format!("`{sql}` must be refused"))
            .to_string();
        assert!(
            err.contains(want),
            "`{sql}` was refused, but not for the stated reason: {err}"
        );
    }

    // The refusals must not have left a schema row behind.
    assert_eq!(
        limbo_exec_rows(
            &conn,
            "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name IN ('i1','i2','i3','i4')"
        ),
        vec![vec![Value::Integer(0)]],
    );
    Ok(())
}

/// An index created on a view that already holds rows must be backfilled, and
/// must survive a reopen of the database.
#[turso_macros::test(views)]
fn matview_index_backfills_and_survives_reopen(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    for i in 0..20 {
        conn.execute(&format!(
            "INSERT INTO block_raw VALUES ('b{i}', 'p{}', 'c{i}')",
            i % 3
        ))?;
    }
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;

    let expected = limbo_exec_rows(&conn, "SELECT count(*) FROM block WHERE parent_id = 'p1'");
    assert_eq!(expected, vec![vec![Value::Integer(7)]]);

    drop(conn);
    let conn = tmp_db.connect_limbo();
    let plan = plan_of(&conn, "SELECT id FROM block WHERE parent_id = 'p1'");
    assert!(
        plan.contains("SEARCH"),
        "after reopen the matview index must still be planned, plan was:\n{plan}"
    );
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM block WHERE parent_id = 'p1'"),
        vec![vec![Value::Integer(7)]],
    );

    // A write after reopen must keep the index in step.
    conn.execute("INSERT INTO block_raw VALUES ('b99', 'p1', 'new')")?;
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT count(*) FROM block WHERE parent_id = 'p1'"),
        vec![vec![Value::Integer(8)]],
    );
    Ok(())
}

/// DROP VIEW must take the view's indexes with it — btrees and schema rows —
/// and the name must be free for a fresh view afterwards.
#[turso_macros::test(views)]
fn drop_view_takes_its_indexes(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup_schema(&conn)?;
    conn.execute("INSERT INTO block_raw VALUES ('b1', 'root', 'one')")?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;
    assert_eq!(
        limbo_exec_rows(
            &conn,
            "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name='idx_block_parent'"
        ),
        vec![vec![Value::Integer(1)]],
    );

    conn.execute("DROP VIEW block")?;
    assert_eq!(
        limbo_exec_rows(
            &conn,
            "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name='idx_block_parent'"
        ),
        vec![vec![Value::Integer(0)]],
        "the index's sqlite_schema row must go with the view"
    );

    // The name is free again, and so is the view's.
    conn.execute(
        "CREATE MATERIALIZED VIEW block AS \
            SELECT b.id, b.parent_id, b.content, \
                   COALESCE(block_tags_agg.vals, '[]') AS tags \
            FROM block_raw b \
            LEFT OUTER JOIN block_tags_agg ON block_tags_agg.source_id = b.id \
            WHERE b.id != 'sentinel:no_parent'",
    )?;
    conn.execute("CREATE INDEX idx_block_parent ON block(parent_id)")?;
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id FROM block WHERE parent_id = 'root'"),
        vec![vec![Value::Text("b1".into())]],
    );

    drop(conn);
    let conn = tmp_db.connect_limbo();
    assert_eq!(
        limbo_exec_rows(&conn, "SELECT id FROM block WHERE parent_id = 'root'"),
        vec![vec![Value::Text("b1".into())]],
        "the recreated view and its index must reopen cleanly"
    );
    Ok(())
}
