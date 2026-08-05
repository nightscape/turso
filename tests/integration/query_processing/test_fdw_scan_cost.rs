//! What a mirror sync costs the foreign source, counted at the driver.
//!
//! The sweep deliberately does not stage its first scan into a temp table, so
//! it reads the source twice per REFRESH: once to upsert, once to bound the
//! anti-join that deletes vanished rows. That is a deliberate trade (no
//! per-sync DDL, no second copy of the data) and it is only defensible if the
//! cost is *constant* — in particular if the `NOT IN (<subquery>)` materialises
//! its subquery once rather than re-running it per mirror row.
//!
//! These tests pin the counts so a regression in either direction is loud.

use crate::common::{self, ExecRows, TempDatabase};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use turso_core::foreign::{ForeignCursor, ForeignDataWrapper, KeyColumn, PushedConstraint};
use turso_core::{Connection, Value};

/// Scans the driver was asked for, counted where the source would pay: a
/// `filter` call is one round trip to the external system.
#[derive(Debug, Default)]
struct ScanCounters {
    opens: AtomicUsize,
    filters: AtomicUsize,
    rows_read: AtomicUsize,
}

impl ScanCounters {
    fn filters(&self) -> usize {
        self.filters.load(Ordering::SeqCst)
    }
    fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }
    fn rows_read(&self) -> usize {
        self.rows_read.load(Ordering::SeqCst)
    }
    fn reset(&self) {
        self.opens.store(0, Ordering::SeqCst);
        self.filters.store(0, Ordering::SeqCst);
        self.rows_read.store(0, Ordering::SeqCst);
    }
}

type Row = (String, String, String);

/// `msg_count(uuid, session_id, body)` over caller-controlled rows, declaring
/// no key columns so every scan is a full one and the count is unambiguous.
#[derive(Debug)]
struct CountingFdw {
    counters: Arc<ScanCounters>,
    rows: Arc<Mutex<Vec<Row>>>,
}

impl ForeignDataWrapper for CountingFdw {
    fn key_columns(&self) -> &[KeyColumn] {
        &[]
    }

    fn identity_columns(&self) -> Option<&[u32]> {
        Some(&[0])
    }

    fn schema_sql(&self) -> String {
        "CREATE TABLE msg_count(uuid TEXT, session_id TEXT, body TEXT)".to_string()
    }

    fn open_cursor(&self, _conn: Arc<Connection>) -> turso_core::Result<Box<dyn ForeignCursor>> {
        self.counters.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(CountingCursor {
            counters: self.counters.clone(),
            source: self.rows.clone(),
            rows: Vec::new(),
            index: 0,
        }))
    }
}

struct CountingCursor {
    counters: Arc<ScanCounters>,
    source: Arc<Mutex<Vec<Row>>>,
    rows: Vec<Row>,
    index: usize,
}

impl ForeignCursor for CountingCursor {
    fn filter(&mut self, _constraints: &[PushedConstraint]) -> turso_core::Result<bool> {
        self.counters.filters.fetch_add(1, Ordering::SeqCst);
        self.rows = self.source.lock().unwrap().clone();
        self.counters
            .rows_read
            .fetch_add(self.rows.len(), Ordering::SeqCst);
        self.index = 0;
        Ok(!self.rows.is_empty())
    }

    fn next(&mut self) -> turso_core::Result<bool> {
        self.index += 1;
        Ok(self.index < self.rows.len())
    }

    fn column(&self, idx: usize) -> turso_core::Result<Value> {
        let row = &self.rows[self.index];
        Ok(match idx {
            0 => Value::build_text(row.0.clone()),
            1 => Value::build_text(row.1.clone()),
            2 => Value::build_text(row.2.clone()),
            _ => Value::Null,
        })
    }

    fn rowid(&self) -> i64 {
        self.index as i64
    }
}

/// Register `msg_count` on `conn` and hand back the counters and the row store.
fn register(
    conn: &Arc<Connection>,
    rows: &[(&str, &str, &str)],
) -> (Arc<ScanCounters>, Arc<Mutex<Vec<Row>>>) {
    let counters = Arc::new(ScanCounters::default());
    let store = Arc::new(Mutex::new(
        rows.iter()
            .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
            .collect::<Vec<_>>(),
    ));
    conn.register_foreign_table(
        "msg_count",
        Arc::new(CountingFdw {
            counters: counters.clone(),
            rows: store.clone(),
        }),
    )
    .unwrap();
    (counters, store)
}

fn set_rows(store: &Arc<Mutex<Vec<Row>>>, rows: &[(&str, &str, &str)]) {
    *store.lock().unwrap() = rows
        .iter()
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
        .collect();
}

fn seed(n: usize) -> Vec<(String, String, String)> {
    (0..n)
        .map(|i| (format!("m{i:04}"), "s1".to_string(), format!("body{i}")))
        .collect()
}

fn seed_refs(rows: &[(String, String, String)]) -> Vec<(&str, &str, &str)> {
    rows.iter()
        .map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str()))
        .collect()
}

/// Creating the view scans the source once to populate its mirror.
#[turso_macros::test(views)]
fn test_scan_count_at_create(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (counters, _store) = register(&conn, &[("m1", "s1", "one"), ("m2", "s1", "two")]);

    counters.reset();
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_cost_create AS SELECT uuid, body FROM msg_count",
    )?;

    assert_eq!(
        counters.filters(),
        1,
        "CREATE must scan the source exactly once (opens={}, rows={})",
        counters.opens(),
        counters.rows_read()
    );
    Ok(())
}

/// A REFRESH that changes nothing still costs the two sweep scans and no more.
#[turso_macros::test(views)]
fn test_scan_count_per_no_change_refresh(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (counters, _store) = register(&conn, &[("m1", "s1", "one"), ("m2", "s1", "two")]);
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_cost_noop AS SELECT uuid, body FROM msg_count",
    )?;

    counters.reset();
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_cost_noop")?;

    assert_eq!(
        counters.filters(),
        2,
        "the sweep is an upsert scan plus an anti-join scan (opens={}, rows={})",
        counters.opens(),
        counters.rows_read()
    );
    Ok(())
}

/// A REFRESH that does change rows costs the same two scans: the sweep's cost
/// is a function of the source, not of what it found.
#[turso_macros::test(views)]
fn test_scan_count_per_changed_refresh(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (counters, store) = register(&conn, &[("m1", "s1", "one"), ("m2", "s1", "two")]);
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_cost_change AS SELECT uuid, body FROM msg_count",
    )?;

    // One update, one insert and one delete in a single sweep.
    set_rows(&store, &[("m1", "s1", "CHANGED"), ("m3", "s1", "three")]);
    counters.reset();
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_cost_change")?;

    assert_eq!(
        counters.filters(),
        2,
        "a changed sweep must cost no more scans than an unchanged one (opens={}, rows={})",
        counters.opens(),
        counters.rows_read()
    );
    let rows: Vec<(String, String)> =
        conn.exec_rows("SELECT uuid, body FROM mv_cost_change ORDER BY uuid");
    assert_eq!(
        rows,
        vec![
            ("m1".to_string(), "CHANGED".to_string()),
            ("m3".to_string(), "three".to_string()),
        ]
    );
    Ok(())
}

/// The sharp one: `NOT IN (<subquery>)` must materialise its subquery once. If
/// it re-executed per mirror row, the scan count would grow with the mirror and
/// every REFRESH would be O(rows) round trips to the external source.
#[turso_macros::test(views)]
fn test_sweep_scan_count_is_independent_of_mirror_size(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let small = seed(2);
    let (counters, store) = register(&conn, &seed_refs(&small));
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_cost_scale AS SELECT uuid, body FROM msg_count",
    )?;

    counters.reset();
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_cost_scale")?;
    let small_scans = counters.filters();

    let large = seed(120);
    set_rows(&store, &seed_refs(&large));
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_cost_scale")?;
    counters.reset();
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_cost_scale")?;
    let large_scans = counters.filters();

    assert_eq!(
        small_scans, large_scans,
        "sweep scans must not grow with the mirror: {small_scans} at 2 rows, \
         {large_scans} at 120 rows — a per-row subquery re-execution"
    );
    let rows: Vec<(i64,)> = conn.exec_rows("SELECT count(*) FROM mv_cost_scale");
    assert_eq!(rows[0].0, 120);
    Ok(())
}
