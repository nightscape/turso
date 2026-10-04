//! Materialized views with sum/avg/count/min/max, plain and DISTINCT, must hold
//! exactly what their SELECT returns when run from scratch, value types
//! included. Random writes hit integers, reals, NULLs, values near the integer
//! limits and infinities; reopening the database makes the views reload their
//! state from disk.
//!
//! SQLite's sum is order dependent in two corners: an integer sum whose partial
//! total leaves the i64 range, and a real sum that reaches +inf and -inf from
//! different rows. Each group draws from a value set where neither can happen,
//! so the recomputed SELECT has one well-defined answer.
//!
//! An in-memory SQLite database receives the same writes. A write after which
//! SQLite's SELECT raises "integer overflow" must fail on Turso too.

use crate::common::{limbo_exec_rows, try_limbo_exec_rows, TempDatabase};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use rusqlite::types::Value;
use std::path::Path;
use tempfile::TempDir;

const VIEWS: &[(&str, &str, &str)] = &[
    (
        "v_grouped",
        "SELECT g, sum(x) AS s, avg(x) AS a, count(x) AS cx, count(*) AS n, \
         min(x) AS mn, max(x) AS mx FROM t GROUP BY g",
        "g",
    ),
    (
        "v_scaled",
        "SELECT g, sum(x * 10) AS s, avg(x * 10) AS a FROM t GROUP BY g",
        "g",
    ),
    (
        "v_distinct",
        "SELECT g, sum(DISTINCT x) AS sd, avg(DISTINCT x) AS ad, count(DISTINCT x) AS cd \
         FROM t GROUP BY g",
        "g",
    ),
    (
        "v_one_group",
        "SELECT sum(x) AS s, avg(x) AS a, count(*) AS n FROM t WHERE g = 0",
        "1",
    ),
];

const NON_NEGATIVE_GROUP: &[&str] = &[
    "NULL",
    "0",
    "1",
    "7",
    "4611686018427387904",
    "9223372036854775806",
    "9223372036854775807",
    "0.5",
    "-1.25",
    "2.75",
    "1e308",
    "9e999",
];

const NON_POSITIVE_GROUP: &[&str] = &[
    "NULL",
    "0",
    "-1",
    "-7",
    "-4611686018427387904",
    "-9223372036854775807",
    "(-9223372036854775807 - 1)",
    "0.5",
    "-1.25",
    "2.75",
    "-1e308",
    "-9e999",
];

const SMALL_GROUP: &[&str] = &[
    "NULL", "-3", "-1", "0", "2", "3", "0.5", "-1.25", "9e999", "-9e999",
];

const GROUPS: usize = 3;

fn values_for_group(g: usize) -> &'static [&'static str] {
    match g {
        0 => NON_NEGATIVE_GROUP,
        1 => NON_POSITIVE_GROUP,
        2 => SMALL_GROUP,
        _ => unreachable!("only {GROUPS} groups"),
    }
}

fn open(path: &Path) -> TempDatabase {
    TempDatabase::builder()
        .with_db_path(path)
        .with_views(true)
        .build()
}

#[test]
fn matview_aggregates_match_recompute_for_random_writes() {
    let seeds: Vec<u64> = match std::env::var("SEED") {
        Ok(seed) => vec![seed.parse().expect("SEED must be a u64")],
        Err(_) => (0..12).collect(),
    };
    for seed in seeds {
        run_seed(seed);
    }
}

fn run_seed(seed: u64) {
    let mut rng = ChaCha8Rng::seed_from_u64(seed);
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path().join("aggregate_differential.db");
    let sqlite = rusqlite::Connection::open_in_memory().unwrap();
    let mut db = open(&path);
    let mut conn = db.connect_limbo();
    let mut log: Vec<String> = Vec::new();

    let setup = "CREATE TABLE t(id INTEGER PRIMARY KEY, g INTEGER, x)";
    conn.execute(setup).unwrap();
    sqlite.execute(setup, ()).unwrap();
    for (name, select, _) in VIEWS {
        conn.execute(format!("CREATE MATERIALIZED VIEW {name} AS {select}"))
            .unwrap();
    }

    let mut next_id = 1i64;
    for step in 0..120 {
        if step % 40 == 39 {
            conn.close().unwrap();
            drop(db);
            db = open(&path);
            conn = db.connect_limbo();
            log.push("-- reopen".to_string());
        }
        let ids: Vec<i64> = sqlite
            .prepare("SELECT id FROM t ORDER BY id")
            .unwrap()
            .query_map((), |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let sql = random_write(&mut rng, &ids, &mut next_id);
        log.push(sql.clone());

        let expect_overflow = sqlite_write_overflows(&sqlite, &sql);
        let result = conn.execute(&sql);
        if let Err(e) = &result {
            log.push(format!("-- failed: {e}"));
        }
        let context = || format!("seed {seed}, step {step}, writes:\n{}", log.join(";\n"));
        match (&result, expect_overflow) {
            (Ok(_), false) => {}
            (Err(e), true) if e.to_string().contains("integer overflow") => {}
            _ => panic!(
                "write result {result:?}, but SQLite's SELECT overflows: {expect_overflow}\n{}",
                context()
            ),
        }

        for (name, select, order) in VIEWS {
            let view = limbo_exec_rows(&conn, &format!("SELECT * FROM {name} ORDER BY {order}"));
            let recompute = try_limbo_exec_rows(&db, &conn, &format!("{select} ORDER BY {order}"))
                .unwrap_or_else(|e| panic!("recompute of {name} failed: {e}\n{}", context()));
            let in_sqlite = sqlite_rows(&sqlite, &format!("{select} ORDER BY {order}"));
            assert_same_rows(&view, &recompute, &format!("{name} vs recompute"), &context);
            assert_same_rows(&view, &in_sqlite, &format!("{name} vs SQLite"), &context);
        }
    }
}

fn random_write(rng: &mut ChaCha8Rng, ids: &[i64], next_id: &mut i64) -> String {
    let g = rng.random_range(0..GROUPS);
    let x = values_for_group(g)[rng.random_range(0..values_for_group(g).len())];
    let choice = if ids.is_empty() {
        0
    } else {
        rng.random_range(0..10)
    };
    match choice {
        0..=4 => {
            let id = *next_id;
            *next_id += 1;
            format!("INSERT INTO t VALUES ({id}, {g}, {x})")
        }
        5..=6 => {
            let id = ids[rng.random_range(0..ids.len())];
            format!("DELETE FROM t WHERE id = {id}")
        }
        7..=8 => {
            let id = ids[rng.random_range(0..ids.len())];
            format!("UPDATE t SET g = {g}, x = {x} WHERE id = {id}")
        }
        _ => format!("DELETE FROM t WHERE g = {g} AND id % 3 = 0"),
    }
}

/// Applies `sql` to the SQLite copy unless some view's SELECT would then
/// raise "integer overflow"; returns whether it did.
fn sqlite_write_overflows(sqlite: &rusqlite::Connection, sql: &str) -> bool {
    sqlite.execute_batch("BEGIN").unwrap();
    sqlite.execute(sql, ()).unwrap();
    let overflows = VIEWS.iter().any(|(_, select, _)| {
        let mut stmt = sqlite.prepare(select).unwrap();
        let mut rows = stmt.query(()).unwrap();
        loop {
            match rows.next() {
                Ok(Some(_)) => {}
                Ok(None) => break false,
                Err(e) if e.to_string().contains("integer overflow") => break true,
                Err(e) => panic!("SQLite failed on {select}: {e}"),
            }
        }
    });
    sqlite
        .execute_batch(if overflows { "ROLLBACK" } else { "COMMIT" })
        .unwrap();
    overflows
}

fn sqlite_rows(sqlite: &rusqlite::Connection, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = sqlite.prepare(sql).unwrap();
    let columns = stmt.column_count();
    stmt.query_map((), |row| (0..columns).map(|i| row.get(i)).collect())
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn assert_same_rows(
    actual: &[Vec<Value>],
    expected: &[Vec<Value>],
    what: &str,
    context: &dyn Fn() -> String,
) {
    let same = actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected)
            .all(|(a, e)| a.len() == e.len() && a.iter().zip(e).all(|(a, e)| same_value(a, e)));
    assert!(
        same,
        "{what}:\n  view:     {actual:?}\n  expected: {expected:?}\n{}",
        context()
    );
}

fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Real(a), Value::Real(b)) => a.to_bits() == b.to_bits(),
        _ => a == b,
    }
}
