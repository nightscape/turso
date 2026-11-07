//! A materialized view whose SELECT holds `WITH RECURSIVE` must hold exactly
//! what a fresh run of that SELECT returns, after every statement. Random
//! statements edit a forest of `node(id, parent)` rows: insert a leaf, delete,
//! edit content, reparent (also under the node's own descendant or itself,
//! which makes a cycle), move all children, and change tags and focus rows,
//! inside and outside transactions with ROLLBACK and ROLLBACK TO. After every
//! statement each view is compared, as a multiset, with its SELECT run fresh
//! on Turso and on an in-memory SQLite database that receives the same
//! statements. Every scenario runs once without and once with indexes on the
//! join columns, so the views' joins both scan and seek. Each seed declares
//! `node`'s join columns with one of several type pairs, so a join can convert
//! a CTE value to a value that differs from the stored one. At the end, each
//! view must stay equal across REFRESH, VACUUM, REINDEX and edits after them,
//! must be equal when created again over the final data, and DROP must leave no
//! storage; `PRAGMA integrity_check` must be ok before REFRESH and after REINDEX.
//!
//! A failing scenario is shrunk to a short statement list. `SEED=<n>` runs
//! one seed.

use crate::common::{limbo_exec_rows, try_limbo_exec_rows, TempDatabase};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rusqlite::types::Value;
use std::sync::Arc;
use tempfile::TempDir;

struct Shape {
    name: &'static str,
    select: &'static str,
}

const SHAPES: &[Shape] = &[
    Shape {
        name: "path",
        select: "WITH RECURSIVE p(id, parent, label, path) AS ( \
             SELECT id, parent, label, '/' || id FROM node WHERE parent IS NULL \
             UNION ALL \
             SELECT n.id, n.parent, n.label, p.path || '/' || n.id \
             FROM p JOIN node n ON n.parent = p.id) \
             SELECT * FROM p",
    },
    Shape {
        name: "guarded",
        select: "WITH RECURSIVE subtree(watch_key, node_id, depth, visited) AS ( \
             SELECT wc.watch_key, b.id, 0, CAST(b.id AS TEXT) \
             FROM focus wc JOIN node b ON b.id = wc.context_id \
             WHERE wc.kind = 'root' \
             UNION ALL \
             SELECT subtree.watch_key, child.id, subtree.depth + 1, \
                    subtree.visited || ',' || CAST(child.id AS TEXT) \
             FROM subtree JOIN node child ON child.parent = subtree.node_id \
             WHERE subtree.depth < 20 \
               AND ',' || subtree.visited || ',' NOT LIKE '%,' || CAST(child.id AS TEXT) || ',%') \
             SELECT d.*, subtree.watch_key AS watch_key, subtree.depth AS depth \
             FROM subtree JOIN node d ON d.id = subtree.node_id",
    },
    Shape {
        name: "focus",
        select: "WITH RECURSIVE subtree(watch_key, node_id, depth, visited) AS ( \
             SELECT wc.watch_key, b.id, 0, CAST(b.id AS TEXT) \
             FROM focus wc JOIN node b ON b.id = wc.context_id \
             WHERE wc.kind = 'root' \
             UNION ALL \
             SELECT subtree.watch_key, child.id, subtree.depth + 1, \
                    subtree.visited || ',' || CAST(child.id AS TEXT) \
             FROM subtree JOIN node child ON child.parent = subtree.node_id \
             LEFT JOIN tag pt ON pt.block_id = subtree.node_id AND pt.tag = 'Page' \
             WHERE subtree.depth < 20 \
               AND ',' || subtree.visited || ',' NOT LIKE '%,' || CAST(child.id AS TEXT) || ',%' \
               AND (subtree.depth = 0 OR pt.block_id IS NULL)) \
             SELECT d.*, subtree.watch_key AS watch_key, subtree.depth AS depth \
             FROM subtree JOIN node d ON d.id = subtree.node_id",
    },
    Shape {
        name: "page_stop",
        select: "WITH RECURSIVE fd AS ( \
             SELECT b.*, 0 AS _depth FROM focus fr JOIN node b ON b.id = fr.context_id \
             WHERE fr.kind = 'root' \
             UNION ALL \
             SELECT child.*, fd._depth + 1 FROM fd JOIN node child ON child.parent = fd.id \
             LEFT JOIN tag bt ON bt.block_id = fd.id AND bt.tag = 'Page' \
             WHERE fd._depth < 20 AND (fd._depth = 0 OR bt.block_id IS NULL)) \
             SELECT * FROM fd",
    },
    Shape {
        name: "not_exists",
        select: "WITH RECURSIVE s(id, depth) AS ( \
             SELECT context_id, 0 FROM focus WHERE kind = 'root' \
             UNION ALL \
             SELECT n.id, s.depth + 1 FROM s JOIN node n ON n.parent = s.id \
             WHERE s.depth < 20 \
               AND NOT EXISTS (SELECT 1 FROM tag t WHERE t.block_id = n.id AND t.tag = 'Page')) \
             SELECT * FROM s",
    },
    Shape {
        name: "undirected",
        select: "WITH RECURSIVE w(src, node_id, depth, visited) AS ( \
             SELECT id, id, 0, ',' || id || ',' FROM node WHERE label = 'a' \
             UNION ALL \
             SELECT w.src, n.id, w.depth + 1, w.visited || n.id || ',' \
             FROM w JOIN node n ON n.parent = w.node_id \
             WHERE w.depth < 4 AND w.visited NOT LIKE '%,' || n.id || ',%' \
             UNION ALL \
             SELECT w.src, n.id, w.depth + 1, w.visited || n.id || ',' \
             FROM w JOIN node c ON c.id = w.node_id JOIN node n ON n.id = c.parent \
             WHERE w.depth < 4 AND w.visited NOT LIKE '%,' || n.id || ',%') \
             SELECT src, node_id, depth FROM w WHERE depth >= 1",
    },
    Shape {
        name: "block_with_path",
        select: "WITH RECURSIVE paths AS ( \
             SELECT id, parent, label, '/' || id AS path FROM node WHERE parent IS NULL \
             UNION ALL \
             SELECT b.id, b.parent, b.label, p.path || '/' || b.id \
             FROM node b JOIN paths p ON b.parent = p.id) \
             SELECT * FROM paths",
    },
    Shape {
        name: "siblings",
        select: "SELECT a.id AS aid, b.id AS bid, a.label, b.label AS blabel \
             FROM node a JOIN node b ON a.parent = b.parent",
    },
    Shape {
        name: "tagged",
        select: "SELECT n.id, n.label, t.tag FROM node n JOIN tag t ON t.block_id = n.id",
    },
    // The NOCASE comparison does not fit the BINARY index on tag(tag).
    Shape {
        name: "nocase",
        select: "SELECT n.id, t.tag FROM node n JOIN tag t ON t.tag = n.label COLLATE NOCASE",
    },
    // The comparison applies NUMERIC affinity to the TEXT column watch_key,
    // so its index does not fit; the INTEGER index on node(parent) does.
    Shape {
        name: "affinity",
        select: "SELECT n.id, f.watch_key FROM node n JOIN focus f ON f.watch_key = n.parent",
    },
    Shape {
        name: "depth",
        select: "WITH RECURSIVE d(id, depth) AS ( \
             SELECT id, 0 FROM node WHERE parent IS NULL \
             UNION ALL \
             SELECT n.id, d.depth + 1 FROM d JOIN node n ON n.parent = d.id) \
             SELECT * FROM d",
    },
];

struct NodeSchema {
    create: &'static str,
    /// Write ids as text literals, so a column without affinity stores text.
    text_ids: bool,
    /// Turso's SELECT gives SQLite's rows. With a TEXT `parent` it does not:
    /// it compares a recursive CTE's column without SQLite's affinity. Then
    /// the oracle is the fresh Turso SELECT alone.
    sqlite_oracle: bool,
}

impl NodeSchema {
    fn id(&self, id: i64) -> String {
        if self.text_ids {
            format!("'{id}'")
        } else {
            id.to_string()
        }
    }
}

const NODE_SCHEMAS: &[NodeSchema] = &[
    NodeSchema {
        create: "CREATE TABLE node(id INTEGER PRIMARY KEY, parent INTEGER, label TEXT)",
        text_ids: false,
        sqlite_oracle: true,
    },
    NodeSchema {
        create: "CREATE TABLE node(id TEXT, parent INTEGER, label TEXT)",
        text_ids: false,
        sqlite_oracle: true,
    },
    NodeSchema {
        create: "CREATE TABLE node(id INTEGER, parent TEXT, label TEXT)",
        text_ids: false,
        sqlite_oracle: false,
    },
    NodeSchema {
        create: "CREATE TABLE node(id NUMERIC, parent TEXT, label TEXT)",
        text_ids: true,
        sqlite_oracle: false,
    },
    NodeSchema {
        create: "CREATE TABLE node(id, parent INTEGER, label TEXT)",
        text_ids: true,
        sqlite_oracle: true,
    },
    NodeSchema {
        create: "CREATE TABLE node(id BLOB, parent INTEGER, label TEXT)",
        text_ids: true,
        sqlite_oracle: true,
    },
];

const OTHER_TABLES: &[&str] = &[
    "CREATE TABLE tag(block_id INTEGER, tag TEXT)",
    "CREATE TABLE focus(watch_key TEXT, context_id INTEGER, kind TEXT)",
];

const INDEXES: &[&str] = &[
    "CREATE INDEX node_parent ON node(parent)",
    "CREATE INDEX node_label ON node(label)",
    "CREATE INDEX tag_block ON tag(block_id)",
    "CREATE INDEX tag_tag ON tag(tag)",
    "CREATE INDEX focus_key ON focus(watch_key)",
];

#[test]
fn recursive_views_match_recompute_for_random_tree_edits() {
    for (name, schema, statements) in fixed_examples() {
        for indexed in [false, true] {
            if let Some(failure) = run_scenario(schema, &statements, indexed) {
                panic!("fixed example {name}, indexed {indexed}: {failure}");
            }
        }
    }
    let seeds: Vec<u64> = match std::env::var("SEED") {
        Ok(seed) => vec![seed.parse().expect("SEED must be a u64")],
        Err(_) => (0..40).collect(),
    };
    for seed in seeds {
        let schema = &NODE_SCHEMAS[seed as usize % NODE_SCHEMAS.len()];
        let statements = random_scenario(schema, seed, 80);
        for indexed in [false, true] {
            if let Some(failure) = run_scenario(schema, &statements, indexed) {
                let shrunk = shrink(schema, statements.clone(), indexed);
                panic!(
                    "seed {seed}, `{}`, indexed {indexed}: {failure}\nshrunk statements:\n{}\nshrunk failure: {}",
                    schema.create,
                    shrunk.join(";\n"),
                    run_scenario(schema, &shrunk, indexed).unwrap()
                );
            }
        }
    }
}

/// Cases worth keeping, run before the random scenarios.
fn fixed_examples() -> Vec<(&'static str, &'static NodeSchema, Vec<String>)> {
    let text_id = &NODE_SCHEMAS[1];
    let cases: [(&str, &NodeSchema, &[&str]); 4] = [
        (
            "moving a block under its own descendant empties its subtree",
            &NODE_SCHEMAS[0],
            &[
                "INSERT INTO node VALUES (1, NULL, 'a')",
                "INSERT INTO node VALUES (2, 1, 'a')",
                "INSERT INTO node VALUES (3, 2, 'b')",
                "INSERT INTO focus VALUES ('k', 1, 'root')",
                "UPDATE node SET parent = 3 WHERE id = 2",
                "UPDATE node SET parent = 1 WHERE id = 2",
                "UPDATE node SET parent = 2 WHERE id = 2",
                "UPDATE node SET parent = 1 WHERE id = 2",
                "BEGIN",
                "UPDATE node SET parent = 3 WHERE id = 2",
                "UPDATE node SET label = 'c' WHERE id = 3",
                "ROLLBACK",
            ],
        ),
        // The CTE rows hold TEXT ids; the join compares them as INTEGER.
        (
            "an index rebuild keeps the converted join key of the CTE rows",
            text_id,
            &[
                "INSERT INTO node VALUES ('1', NULL, 'a')",
                "INSERT INTO node VALUES ('2', 1, 'b')",
                "REINDEX",
                "INSERT INTO node VALUES ('3', 1, 'b')",
                "DELETE FROM node WHERE id = '2'",
            ],
        ),
        (
            "the index of converted join keys matches the CTE rows",
            text_id,
            &[
                "INSERT INTO node VALUES ('1', NULL, 'a')",
                "INSERT INTO node VALUES ('2', 1, 'b')",
            ],
        ),
        (
            "deleting one of two ids with the same join key keeps the other's children",
            text_id,
            &[
                "INSERT INTO node VALUES ('1', NULL, 'a')",
                "INSERT INTO node VALUES ('01', NULL, 'a')",
                "INSERT INTO focus VALUES ('k', 1, 'root')",
                "DELETE FROM node WHERE id = '01'",
                "INSERT INTO node VALUES ('c', 1, 'b')",
            ],
        ),
    ];
    cases
        .into_iter()
        .map(|(name, schema, statements)| {
            (
                name,
                schema,
                statements.iter().map(|s| s.to_string()).collect(),
            )
        })
        .collect()
}

fn random_scenario(schema: &NodeSchema, seed: u64, length: usize) -> Vec<String> {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    for sql in std::iter::once(&schema.create).chain(OTHER_TABLES) {
        sqlite.execute(sql, ()).unwrap();
    }
    let mut statements = Vec::new();
    let push = |sql: String, statements: &mut Vec<String>| {
        if sqlite.execute_batch(&sql).is_ok() {
            statements.push(sql);
        }
    };
    for id in 1..=8 {
        let parent = if id == 1 || rng.random_bool(0.2) {
            "NULL".to_string()
        } else {
            schema.id(rng.random_range(1..id))
        };
        let label = if rng.random_bool(0.4) { "a" } else { "b" };
        push(
            format!(
                "INSERT INTO node VALUES ({}, {parent}, '{label}')",
                schema.id(id)
            ),
            &mut statements,
        );
    }
    push(
        "INSERT INTO focus VALUES ('k1', 1, 'root')".to_string(),
        &mut statements,
    );
    let mut next_id = 9;
    let mut in_transaction = false;
    let mut has_savepoint = false;
    while statements.len() < length {
        let ids: Vec<i64> = sqlite
            .prepare("SELECT CAST(id AS INTEGER) FROM node ORDER BY 1")
            .unwrap()
            .query_map((), |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let pick = |rng: &mut ChaCha8Rng| -> String {
            schema.id(if ids.is_empty() {
                1
            } else {
                ids[rng.random_range(0..ids.len())]
            })
        };
        let sql = match rng.random_range(0..102) {
            0..=14 => {
                let id = next_id;
                next_id += 1;
                let parent = pick(&mut rng);
                format!("INSERT INTO node VALUES ({}, {parent}, 'b')", schema.id(id))
            }
            15..=21 => format!("DELETE FROM node WHERE id = {}", pick(&mut rng)),
            22..=33 => format!(
                "UPDATE node SET label = '{}' WHERE id = {}",
                if rng.random_bool(0.5) { "a" } else { "b" },
                pick(&mut rng)
            ),
            34..=55 => format!(
                "UPDATE node SET parent = {} WHERE id = {}",
                pick(&mut rng),
                pick(&mut rng)
            ),
            56..=59 => format!(
                "UPDATE node SET parent = NULL WHERE id = {}",
                pick(&mut rng)
            ),
            60..=63 => format!(
                "UPDATE node SET parent = {} WHERE parent = {}",
                pick(&mut rng),
                pick(&mut rng)
            ),
            64..=69 => format!(
                "INSERT INTO tag VALUES ({}, '{}')",
                pick(&mut rng),
                ["Page", "Page", "a", "A"][rng.random_range(0..4)]
            ),
            70..=73 => format!(
                "DELETE FROM tag WHERE rowid = (SELECT min(rowid) FROM tag WHERE block_id = {})",
                pick(&mut rng)
            ),
            74..=77 => {
                let watch_key = if rng.random_bool(0.5) {
                    format!("k{}", rng.random_range(1..3))
                } else {
                    pick(&mut rng).trim_matches('\'').to_string()
                };
                format!(
                    "INSERT INTO focus VALUES ('{watch_key}', {}, 'root')",
                    pick(&mut rng)
                )
            }
            78..=79 => format!("DELETE FROM focus WHERE context_id = {}", pick(&mut rng)),
            80..=85 if !in_transaction => {
                in_transaction = true;
                "BEGIN".to_string()
            }
            86..=88 if in_transaction && !has_savepoint => {
                has_savepoint = true;
                "SAVEPOINT s".to_string()
            }
            89..=91 if has_savepoint => {
                if rng.random_bool(0.5) {
                    "ROLLBACK TO s".to_string()
                } else {
                    has_savepoint = false;
                    "RELEASE s".to_string()
                }
            }
            92..=99 if in_transaction => {
                in_transaction = false;
                has_savepoint = false;
                if rng.random_bool(0.5) {
                    "COMMIT".to_string()
                } else {
                    "ROLLBACK".to_string()
                }
            }
            100..=101 => "REINDEX".to_string(),
            _ => continue,
        };
        push(sql, &mut statements);
    }
    if in_transaction {
        statements.push("COMMIT".to_string());
    }
    statements
}

/// Run `statements` against SQLite and Turso; return the first disagreement.
fn run_scenario(schema: &NodeSchema, statements: &[String], indexed: bool) -> Option<String> {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("recursive_differential.db");
    let db = TempDatabase::builder()
        .with_db_path(&path)
        .with_views(true)
        .build();
    let conn = db.connect_limbo();
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    let indexes = if indexed { INDEXES } else { &[] };
    for sql in std::iter::once(&schema.create)
        .chain(OTHER_TABLES)
        .chain(indexes)
    {
        conn.execute(sql).unwrap();
        sqlite.execute(sql, ()).unwrap();
    }
    for shape in SHAPES {
        conn.execute(format!(
            "CREATE MATERIALIZED VIEW v_{} AS {}",
            shape.name, shape.select
        ))
        .unwrap();
    }
    for (index, sql) in statements.iter().enumerate() {
        if sqlite.execute_batch(sql).is_err() {
            continue;
        }
        let at = format!("after statement {index} `{sql}`");
        if let Err(e) = conn.execute(sql) {
            return Some(format!("{at}: Turso failed where SQLite succeeded: {e}"));
        }
        if let Some(failure) = compare_views(schema, &db, &conn, &sqlite, "v_", &at) {
            return Some(failure);
        }
    }
    if let Some(failure) = storage_round_trip(schema, &db, &conn, &sqlite) {
        return Some(failure);
    }
    for shape in SHAPES {
        conn.execute(format!(
            "CREATE MATERIALIZED VIEW late_{} AS {}",
            shape.name, shape.select
        ))
        .unwrap();
    }
    if let Some(failure) = compare_views(
        schema,
        &db,
        &conn,
        &sqlite,
        "late_",
        "after creating the views last",
    ) {
        return Some(failure);
    }
    for shape in SHAPES {
        for prefix in ["v_", "late_"] {
            conn.execute(format!("DROP VIEW {prefix}{}", shape.name))
                .unwrap();
        }
    }
    let left = limbo_exec_rows(
        &conn,
        "SELECT type, name FROM sqlite_schema WHERE name LIKE '__turso_internal_dbsp%' \
         OR tbl_name LIKE '__turso_internal_dbsp%'",
    );
    (!left.is_empty()).then(|| format!("DROP VIEW left storage behind: {left:?}"))
}

/// The views stay equal to their SELECT across REFRESH, VACUUM, REINDEX and
/// edits after them, and the database stays consistent.
fn storage_round_trip(
    schema: &NodeSchema,
    db: &TempDatabase,
    conn: &Arc<turso_core::Connection>,
    sqlite: &rusqlite::Connection,
) -> Option<String> {
    if let Some(failure) = integrity_check(conn, "before REFRESH") {
        return Some(failure);
    }
    for shape in SHAPES {
        conn.execute(format!("REFRESH MATERIALIZED VIEW v_{}", shape.name))
            .unwrap();
    }
    if let Some(failure) = compare_views(schema, db, conn, sqlite, "v_", "after REFRESH") {
        return Some(failure);
    }
    conn.execute("VACUUM").unwrap();
    if let Some(failure) = compare_views(schema, db, conn, sqlite, "v_", "after VACUUM") {
        return Some(failure);
    }
    let edit =
        "UPDATE node SET parent = (SELECT min(id) FROM node) WHERE id = (SELECT max(id) FROM node)";
    if let Some(failure) = edit_and_compare(schema, db, conn, sqlite, edit, "after VACUUM") {
        return Some(failure);
    }
    conn.execute("REINDEX").unwrap();
    if let Some(failure) = integrity_check(conn, "after REINDEX") {
        return Some(failure);
    }
    if let Some(failure) = compare_views(schema, db, conn, sqlite, "v_", "after REINDEX") {
        return Some(failure);
    }
    let edits = [
        format!(
            "INSERT INTO node VALUES ({}, (SELECT min(id) FROM node), 'b')",
            schema.id(1000)
        ),
        "DELETE FROM node WHERE id = (SELECT max(id) FROM node WHERE parent IS NOT NULL)"
            .to_string(),
    ];
    edits
        .iter()
        .find_map(|edit| edit_and_compare(schema, db, conn, sqlite, edit, "after REINDEX"))
}

fn edit_and_compare(
    schema: &NodeSchema,
    db: &TempDatabase,
    conn: &Arc<turso_core::Connection>,
    sqlite: &rusqlite::Connection,
    edit: &str,
    after: &str,
) -> Option<String> {
    sqlite.execute_batch(edit).unwrap();
    if let Err(e) = conn.execute(edit) {
        return Some(format!("{after}: `{edit}` failed: {e}"));
    }
    compare_views(
        schema,
        db,
        conn,
        sqlite,
        "v_",
        &format!("{after}, then `{edit}`"),
    )
}

fn integrity_check(conn: &Arc<turso_core::Connection>, at: &str) -> Option<String> {
    let check = limbo_exec_rows(conn, "PRAGMA integrity_check");
    (check != vec![vec![Value::Text("ok".to_string())]])
        .then(|| format!("integrity_check {at}: {check:?}"))
}

fn compare_views(
    schema: &NodeSchema,
    db: &TempDatabase,
    conn: &Arc<turso_core::Connection>,
    sqlite: &rusqlite::Connection,
    prefix: &str,
    at: &str,
) -> Option<String> {
    for shape in SHAPES {
        let fresh = sorted(limbo_exec_rows(conn, shape.select));
        let expected = if schema.sqlite_oracle {
            sorted(sqlite_rows(sqlite, shape.select))
        } else {
            fresh.clone()
        };
        if fresh != expected {
            return Some(format!(
                "{at}: fresh Turso SELECT of {} differs from SQLite\nturso:  {fresh:?}\nsqlite: {expected:?}",
                shape.name
            ));
        }
        let held =
            match try_limbo_exec_rows(db, conn, &format!("SELECT * FROM {prefix}{}", shape.name)) {
                Ok(rows) => sorted(rows),
                Err(e) => return Some(format!("{at}: reading {prefix}{} failed: {e}", shape.name)),
            };
        if held != expected {
            return Some(format!(
                "{at}: view {prefix}{} differs from recompute\nview:   {held:?}\nsqlite: {expected:?}",
                shape.name
            ));
        }
    }
    None
}

fn shrink(schema: &NodeSchema, mut statements: Vec<String>, indexed: bool) -> Vec<String> {
    let mut chunk = statements.len() / 2;
    while chunk >= 1 {
        let mut start = 0;
        while start < statements.len() {
            let mut candidate = statements.clone();
            candidate.drain(start..(start + chunk).min(candidate.len()));
            if run_scenario(schema, &candidate, indexed).is_some() {
                statements = candidate;
            } else {
                start += chunk;
            }
        }
        chunk /= 2;
    }
    statements
}

fn sqlite_rows(sqlite: &rusqlite::Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = sqlite.prepare(sql).unwrap();
    let width = stmt.column_count();
    stmt.query_map((), |row| {
        (0..width)
            .map(|i| row.get::<_, Value>(i))
            .collect::<Result<Vec<_>, _>>()
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn sorted(rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut rows: Vec<String> = rows.into_iter().map(|row| format!("{row:?}")).collect();
    rows.sort();
    rows
}
