//! The mirror sweep across IO yields.
//!
//! `REFRESH` over a mirrored source runs two nested statements. Every IO they
//! perform is a suspension point: the statement returns, the caller pumps IO,
//! and the statement is re-entered. The historical bug family in this subsystem
//! is state advanced *before* the yield, so the resumed statement repeats or
//! skips work — silently, because the answer is only wrong by a row.
//!
//! [`MemoryYieldIO`] defers every completion to `IO::step`, so running the
//! sweep on it forces a yield at *every* IO boundary rather than at the few
//! that happen to miss the page cache. The oracle is a run on ordinary
//! synchronous IO: same view, same mirror, same number of captured changes.

#![cfg(feature = "io_memory_yield")]

use std::sync::{Arc, Mutex};
use turso_core::foreign::{ForeignCursor, ForeignDataWrapper, KeyColumn, PushedConstraint};
use turso_core::{
    Connection, Database, DatabaseOpts, MemoryIO, MemoryYieldIO, Numeric, OpenFlags, SqliteDialect,
    StepResult, Value, IO,
};

type Row = (String, String, String);

/// `msg_yield(uuid, session_id, body)` over caller-controlled rows.
#[derive(Debug)]
struct SweepFdw {
    rows: Arc<Mutex<Vec<Row>>>,
}

impl ForeignDataWrapper for SweepFdw {
    fn key_columns(&self) -> &[KeyColumn] {
        &[]
    }

    fn identity_columns(&self) -> Option<&[u32]> {
        Some(&[0])
    }

    fn schema_sql(&self) -> String {
        "CREATE TABLE msg_yield(uuid TEXT, session_id TEXT, body TEXT)".to_string()
    }

    fn open_cursor(&self, _conn: Arc<Connection>) -> turso_core::Result<Box<dyn ForeignCursor>> {
        Ok(Box::new(SweepCursor {
            source: self.rows.clone(),
            rows: Vec::new(),
            index: 0,
        }))
    }
}

struct SweepCursor {
    source: Arc<Mutex<Vec<Row>>>,
    rows: Vec<Row>,
    index: usize,
}

impl ForeignCursor for SweepCursor {
    fn filter(&mut self, _constraints: &[PushedConstraint]) -> turso_core::Result<bool> {
        self.rows = self.source.lock().unwrap().clone();
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
            0 if row.0 == NULL_UUID => Value::Null,
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

/// A `uuid` this driver hands back as SQL NULL, so a scan can break the identity
/// contract the only way a driver can.
const NULL_UUID: &str = "<null>";

fn rows_of(src: &[(&str, &str, &str)]) -> Vec<Row> {
    src.iter()
        .map(|(a, b, c)| (a.to_string(), b.to_string(), c.to_string()))
        .collect()
}

/// Run `sql` to completion, pumping IO at every suspension.
fn exec(conn: &Arc<Connection>, io: &dyn IO, sql: &str) -> turso_core::Result<Vec<Vec<Value>>> {
    let mut stmt = conn.prepare(sql)?;
    let mut out = Vec::new();
    loop {
        match stmt.step()? {
            StepResult::IO | StepResult::Yield | StepResult::Sleep { .. } => io.step()?,
            StepResult::Row => out.push(stmt.row().unwrap().get_values().cloned().collect()),
            StepResult::Done => return Ok(out),
            StepResult::Interrupt | StepResult::Busy => {
                return Err(turso_core::LimboError::Busy);
            }
        }
    }
}

/// Run `sql`, but abandon the statement at the `target`th suspension. Returns
/// false when the statement finished before reaching it.
fn exec_abandoning_at(
    conn: &Arc<Connection>,
    io: &dyn IO,
    sql: &str,
    target: usize,
) -> turso_core::Result<bool> {
    let mut stmt = conn.prepare(sql)?;
    let mut seen = 0usize;
    loop {
        match stmt.step()? {
            StepResult::IO | StepResult::Yield | StepResult::Sleep { .. } => {
                seen += 1;
                if seen == target {
                    drop(stmt);
                    return Ok(true);
                }
                io.step()?;
            }
            StepResult::Row => {}
            StepResult::Done => return Ok(false),
            StepResult::Interrupt | StepResult::Busy => return Ok(false),
        }
    }
}

fn as_strings(rows: &[Vec<Value>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(render).collect())
        .collect()
}

fn render(value: &Value) -> String {
    match value {
        Value::Text(t) => t.as_str().to_string(),
        Value::Numeric(Numeric::Integer(i)) => i.to_string(),
        Value::Null => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

const MIRROR: &str = "__turso_internal_fdw_mirror_v1_mv_yield__msg_yield";

/// Bring up a database on `io` with `msg_yield` registered over `rows`, create
/// the view, and start capturing changes.
fn setup(io: Arc<dyn IO>, name: &str, rows: &Arc<Mutex<Vec<Row>>>) -> Arc<Connection> {
    let db = Database::open_file_with_flags(
        io.clone(),
        name,
        OpenFlags::default(),
        DatabaseOpts::new().with_views(true),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();
    let conn = db.connect().unwrap();
    conn.register_foreign_table("msg_yield", Arc::new(SweepFdw { rows: rows.clone() }))
        .unwrap();
    exec(
        &conn,
        io.as_ref(),
        "CREATE MATERIALIZED VIEW mv_yield AS SELECT uuid, body FROM msg_yield",
    )
    .unwrap();
    exec(
        &conn,
        io.as_ref(),
        "PRAGMA capture_data_changes_conn('full')",
    )
    .unwrap();
    conn
}

/// The observable result of a sweep: what the view holds, what the mirror
/// holds, and how many mirror rows were written getting there.
fn observe(conn: &Arc<Connection>, io: &dyn IO) -> (Vec<Vec<String>>, Vec<Vec<String>>, i64) {
    let view =
        as_strings(&exec(conn, io, "SELECT uuid, body FROM mv_yield ORDER BY uuid").unwrap());
    let mirror = as_strings(
        &exec(
            conn,
            io,
            &format!("SELECT uuid, session_id, body FROM \"{MIRROR}\" ORDER BY uuid"),
        )
        .unwrap(),
    );
    let cdc = exec(
        conn,
        io,
        &format!("SELECT count(*) FROM turso_cdc WHERE table_name = '{MIRROR}'"),
    )
    .unwrap();
    let count: i64 = render(&cdc[0][0])
        .parse()
        .unwrap_or_else(|_| panic!("count(*) returned {:?}", cdc[0][0]));
    (view, mirror, count)
}

const INITIAL: &[(&str, &str, &str)] = &[
    ("m1", "s1", "one"),
    ("m2", "s1", "two"),
    ("m3", "s1", "three"),
];
/// One row updated, one deleted, one inserted — every branch of the sweep.
const CHANGED: &[(&str, &str, &str)] = &[
    ("m1", "s1", "CHANGED"),
    ("m3", "s1", "three"),
    ("m4", "s1", "four"),
];

/// Yielding at every IO the sweep performs must not change its outcome, and
/// must not cost a single extra written row.
#[test]
fn test_sweep_under_io_yields_matches_synchronous_run() {
    let mut results = Vec::new();
    for (label, io) in [
        ("sync", Arc::new(MemoryIO::new()) as Arc<dyn IO>),
        ("yield", Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>),
    ] {
        let source = Arc::new(Mutex::new(rows_of(INITIAL)));
        let conn = setup(io.clone(), &format!("sweep_yield_{label}.db"), &source);

        *source.lock().unwrap() = rows_of(CHANGED);
        exec(&conn, io.as_ref(), "REFRESH MATERIALIZED VIEW mv_yield").unwrap();

        results.push((label, observe(&conn, io.as_ref())));
    }

    let (_, sync) = &results[0];
    let (_, yielded) = &results[1];
    assert_eq!(
        sync.0, yielded.0,
        "view diverged when the sweep yielded at every IO"
    );
    assert_eq!(
        sync.1, yielded.1,
        "mirror diverged when the sweep yielded at every IO"
    );
    assert_eq!(
        sync.2, yielded.2,
        "the yielding sweep wrote a different number of mirror rows"
    );
    // The oracle is only sharp if it saw the change it was supposed to.
    assert_eq!(sync.0.len(), 3, "{:?}", sync.0);
    assert_eq!(
        sync.2, 3,
        "one update, one delete and one insert is three written rows"
    );
}

/// Repeated sweeps under yields must converge, not drift: the second and third
/// REFRESH over an unchanged source must write nothing at all.
#[test]
fn test_repeated_sweeps_under_io_yields_are_inert() {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let source = Arc::new(Mutex::new(rows_of(INITIAL)));
    let conn = setup(io.clone(), "sweep_yield_inert.db", &source);

    for _ in 0..3 {
        exec(&conn, io.as_ref(), "REFRESH MATERIALIZED VIEW mv_yield").unwrap();
    }

    let (view, mirror, cdc) = observe(&conn, io.as_ref());
    assert_eq!(
        cdc, 0,
        "a no-change sweep must write nothing even under yields"
    );
    assert_eq!(view.len(), 3, "{view:?}");
    assert_eq!(mirror.len(), 3, "{mirror:?}");
}

/// How many suspensions a full sweep has on this backend.
fn sweep_boundaries(name: &str) -> usize {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let source = Arc::new(Mutex::new(rows_of(INITIAL)));
    let conn = setup(io.clone(), name, &source);
    *source.lock().unwrap() = rows_of(CHANGED);

    let mut count = 0usize;
    let mut stmt = conn.prepare("REFRESH MATERIALIZED VIEW mv_yield").unwrap();
    loop {
        match stmt.step().unwrap() {
            StepResult::IO | StepResult::Yield => {
                count += 1;
                io.step().unwrap();
            }
            StepResult::Row => {}
            StepResult::Done => break,
            other => panic!("sweep did not complete: {other:?}"),
        }
    }
    assert!(count > 0, "the sweep performed no IO at all");
    count
}

const CONVERGED: &[[&str; 2]] = &[["m1", "CHANGED"], ["m3", "three"], ["m4", "four"]];

fn expect_converged(view: &[Vec<String>], target: usize) {
    let expected: Vec<Vec<String>> = CONVERGED
        .iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect();
    assert_eq!(
        view, expected,
        "the view did not converge after abandoning at suspension {target}"
    );
}

/// A sweep abandoned at any of its suspensions must not lose the change: the
/// next complete sweep still reaches the source's contents. Every boundary the
/// sweep actually has is covered, discovered rather than assumed.
#[test]
fn test_sweep_abandoned_at_each_yield_converges() {
    let boundaries = sweep_boundaries("sweep_yield_probe.db");

    for target in 1..=boundaries {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let source = Arc::new(Mutex::new(rows_of(INITIAL)));
        let conn = setup(
            io.clone(),
            &format!("sweep_yield_abandon_{target}.db"),
            &source,
        );
        *source.lock().unwrap() = rows_of(CHANGED);

        if !exec_abandoning_at(
            &conn,
            io.as_ref(),
            "REFRESH MATERIALIZED VIEW mv_yield",
            target,
        )
        .unwrap()
        {
            continue;
        }

        exec(&conn, io.as_ref(), "REFRESH MATERIALIZED VIEW mv_yield").unwrap();
        let (view, mirror, _) = observe(&conn, io.as_ref());
        expect_converged(&view, target);
        assert_eq!(mirror.len(), 3, "at suspension {target}: {mirror:?}");
    }
}

/// A sweep abandoned mid-flight must leave the view in step with its mirror.
///
/// The sweep's mirror writes are what the view's deltas describe, and the two
/// are undone by different machinery: the writes by the pager's transaction
/// rollback, the deltas by the connection's staged view-transaction state. When
/// only the first ran, the connection read a view matching neither the old
/// source nor the new one:
///
/// ```text
/// view   = [m1 CHANGED, m2 two, m3 three, m4 four]   <- upsert deltas kept
/// mirror = [m1 one,     m2 two, m3 three]            <- writes rolled back
/// ```
#[test]
fn test_sweep_abandoned_leaves_view_in_step_with_mirror() {
    let boundaries = sweep_boundaries("sweep_torn_probe.db");

    for target in 1..=boundaries {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let source = Arc::new(Mutex::new(rows_of(INITIAL)));
        let conn = setup(io.clone(), &format!("sweep_torn_{target}.db"), &source);
        *source.lock().unwrap() = rows_of(CHANGED);

        if !exec_abandoning_at(
            &conn,
            io.as_ref(),
            "REFRESH MATERIALIZED VIEW mv_yield",
            target,
        )
        .unwrap()
        {
            continue;
        }

        let (view, mirror, _) = observe(&conn, io.as_ref());
        assert_eq!(
            view.len(),
            mirror.len(),
            "view and mirror disagree after abandoning at suspension {target}: \
             view={view:?} mirror={mirror:?}"
        );
    }
}

/// The torn state above is confined to the connection that abandoned the
/// sweep. This is the blast-radius bound, and it holds today.
#[test]
fn test_abandoned_sweep_leaves_no_durable_damage() {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let source = Arc::new(Mutex::new(rows_of(INITIAL)));
    let conn = setup(io.clone(), "sweep_torn_durability.db", &source);
    *source.lock().unwrap() = rows_of(CHANGED);
    assert!(
        exec_abandoning_at(&conn, io.as_ref(), "REFRESH MATERIALIZED VIEW mv_yield", 1).unwrap(),
        "the sweep finished before its first suspension"
    );

    let db = Database::open_file_with_flags(
        io.clone(),
        "sweep_torn_durability.db",
        OpenFlags::default(),
        DatabaseOpts::new().with_views(true),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();
    let fresh = db.connect().unwrap();
    let view = as_strings(
        &exec(
            &fresh,
            io.as_ref(),
            "SELECT uuid, body FROM mv_yield ORDER BY uuid",
        )
        .unwrap(),
    );
    let mirror = as_strings(
        &exec(
            &fresh,
            io.as_ref(),
            &format!("SELECT uuid, session_id, body FROM \"{MIRROR}\" ORDER BY uuid"),
        )
        .unwrap(),
    );
    assert_eq!(
        view,
        vec![
            vec!["m1".to_string(), "one".to_string()],
            vec!["m2".to_string(), "two".to_string()],
            vec!["m3".to_string(), "three".to_string()],
        ],
        "an abandoned sweep must leave nothing behind for another connection"
    );
    assert_eq!(mirror.len(), 3, "{mirror:?}");
}

/// The discriminator: the same abandonment against a matview over an ordinary
/// local table keeps the table and its view in step, which is what makes the
/// refutation above a defect of the sweep path rather than of abandonment.
#[test]
fn test_abandoned_local_dml_leaves_view_in_step() {
    for target in 1..=4 {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let db = Database::open_file_with_flags(
            io.clone(),
            &format!("abandon_local_{target}.db"),
            OpenFlags::default(),
            DatabaseOpts::new().with_views(true),
            None,
            Arc::new(SqliteDialect),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        exec(&conn, io.as_ref(), "CREATE TABLE t(uuid TEXT, body TEXT)").unwrap();
        exec(
            &conn,
            io.as_ref(),
            "INSERT INTO t VALUES ('m1','one'),('m2','two'),('m3','three')",
        )
        .unwrap();
        exec(
            &conn,
            io.as_ref(),
            "CREATE MATERIALIZED VIEW mv_local AS SELECT uuid, body FROM t",
        )
        .unwrap();

        exec_abandoning_at(
            &conn,
            io.as_ref(),
            "UPDATE t SET body = 'CHANGED' WHERE uuid = 'm1'",
            target,
        )
        .unwrap();

        let table = as_strings(
            &exec(&conn, io.as_ref(), "SELECT uuid, body FROM t ORDER BY uuid").unwrap(),
        );
        let view = as_strings(
            &exec(
                &conn,
                io.as_ref(),
                "SELECT uuid, body FROM mv_local ORDER BY uuid",
            )
            .unwrap(),
        );
        assert_eq!(
            table, view,
            "table and view diverged after abandoning a local UPDATE at suspension {target}"
        );
    }
}

/// Suspend a sweep at its `target`th boundary and hand the caller the paused
/// statement, so it can observe the database while the sweep is mid-flight.
fn suspend_sweep_at(
    conn: &Arc<Connection>,
    io: &dyn IO,
    target: usize,
) -> Option<turso_core::Statement> {
    let mut stmt = conn.prepare("REFRESH MATERIALIZED VIEW mv_yield").unwrap();
    let mut seen = 0usize;
    loop {
        match stmt.step().unwrap() {
            StepResult::IO | StepResult::Yield => {
                seen += 1;
                if seen == target {
                    return Some(stmt);
                }
                io.step().unwrap();
            }
            StepResult::Row => {}
            StepResult::Done => return None,
            other => panic!("sweep did not complete: {other:?}"),
        }
    }
}

/// A push arriving while a sweep is suspended mid-flight must not slip past it.
///
/// A suspended sweep still holds the write lock, so the push is refused for the
/// same reason any other writer would be — and once the sweep finishes, the
/// same push applies and the view converges. What this rules out is the push
/// interleaving *into* a half-applied sweep, where the mirror would carry rows
/// from two different scans of the source.
#[test]
fn test_push_during_a_suspended_sweep_is_refused_then_applies() {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let source = Arc::new(Mutex::new(rows_of(INITIAL)));
    let conn = setup(io.clone(), "sweep_push_interleave.db", &source);
    let db = Database::open_file_with_flags(
        io.clone(),
        "sweep_push_interleave.db",
        OpenFlags::default(),
        DatabaseOpts::new().with_views(true),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();
    let pusher = db.connect().unwrap();

    let pushed = turso_core::foreign::FdwChange {
        values: vec![
            Value::build_text("m9".to_string()),
            Value::build_text("s1".to_string()),
            Value::build_text("pushed".to_string()),
        ],
        weight: 1,
    };

    *source.lock().unwrap() = rows_of(CHANGED);
    let mut suspended =
        suspend_sweep_at(&conn, io.as_ref(), 1).expect("the sweep finished before suspending");

    assert!(
        pusher
            .inject_fdw_changes("msg_yield", std::slice::from_ref(&pushed))
            .is_err(),
        "a push must not proceed while a suspended sweep holds the write lock"
    );

    // Finish the sweep the push could not join.
    loop {
        match suspended.step().unwrap() {
            StepResult::IO | StepResult::Yield => io.step().unwrap(),
            StepResult::Row => {}
            StepResult::Done => break,
            other => panic!("sweep did not complete: {other:?}"),
        }
    }
    drop(suspended);

    pusher
        .inject_fdw_changes("msg_yield", std::slice::from_ref(&pushed))
        .expect("the same push must apply once the sweep is done");

    let (view, mirror, _) = observe(&conn, io.as_ref());
    assert_eq!(
        view,
        vec![
            vec!["m1".to_string(), "CHANGED".to_string()],
            vec!["m3".to_string(), "three".to_string()],
            vec!["m4".to_string(), "four".to_string()],
            vec!["m9".to_string(), "pushed".to_string()],
        ],
        "the sweep's result and the push must both be present, exactly once each"
    );
    assert_eq!(mirror.len(), 4, "{mirror:?}");
}

/// A source that repeats an identity, alongside a genuine change to another
/// row: a sweep that got as far as its upsert would leave that change visible.
const DUPLICATED: &[(&str, &str, &str)] = &[
    ("m1", "s1", "one"),
    ("m2", "s1", "CHANGED"),
    ("m1", "s1", "dup"),
];
/// The same scan with the repeat removed — the change survives it, so a
/// recovery sweep has something to prove.
const REPAIRED: &[(&str, &str, &str)] = &[
    ("m1", "s1", "one"),
    ("m2", "s1", "CHANGED"),
    ("m3", "s1", "three"),
];
/// A source whose driver returns NULL where it declared an identity.
const NULLED: &[(&str, &str, &str)] = &[
    ("m1", "s1", "one"),
    (NULL_UUID, "s1", "orphan"),
    ("m3", "s1", "three"),
];

/// The mirror down to the rowids its contents sit on — the identities every
/// later retraction is carried by, and the part a contents-only comparison
/// would miss.
fn mirror_with_rowids(conn: &Arc<Connection>, io: &dyn IO) -> Vec<Vec<String>> {
    as_strings(
        &exec(
            conn,
            io,
            &format!("SELECT rowid, uuid, session_id, body FROM \"{MIRROR}\" ORDER BY rowid"),
        )
        .unwrap(),
    )
}

fn view_of(conn: &Arc<Connection>, io: &dyn IO) -> Vec<Vec<String>> {
    as_strings(&exec(conn, io, "SELECT uuid, body FROM mv_yield ORDER BY uuid").unwrap())
}

fn expect_rows(rows: &[[&str; 2]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect()
}

fn repaired_view() -> Vec<Vec<String>> {
    expect_rows(&[["m1", "one"], ["m2", "CHANGED"], ["m3", "three"]])
}

fn assert_duplicate_refusal(msg: &str, at: &str) {
    assert!(msg.contains("msg_yield"), "{at}: {msg}");
    assert!(msg.contains("uuid"), "{at}: {msg}");
    assert!(msg.contains("more than one row"), "{at}: {msg}");
    assert!(!msg.contains("__turso_internal"), "{at}: {msg}");
}

/// Count the suspensions `sql` has before it fails, and hand back the error.
fn suspensions_before_failure(
    conn: &Arc<Connection>,
    io: &dyn IO,
    sql: &str,
) -> (usize, turso_core::LimboError) {
    let mut count = 0usize;
    let mut stmt = conn.prepare(sql).unwrap();
    loop {
        match stmt.step() {
            Ok(StepResult::IO) | Ok(StepResult::Yield) => {
                count += 1;
                io.step().unwrap();
            }
            Ok(StepResult::Row) => {}
            Ok(StepResult::Done) => panic!("the statement was not refused"),
            Ok(other) => panic!("the statement did not complete: {other:?}"),
            Err(err) => return (count, err),
        }
    }
}

/// A refused sweep reaches its verdict before its first suspension, so there is
/// no yield schedule that can tear it: the guard is the sweep's first statement
/// and answers out of the source scan alone.
///
/// The count is asserted rather than assumed, because it is the whole reason the
/// refusal needs no per-boundary reasoning. If the guard ever grows IO of its
/// own — a spilled sorter, a mirror read — this is what says the reasoning has
/// to be redone.
#[test]
fn test_a_refused_sweep_refuses_before_its_first_yield() {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let source = Arc::new(Mutex::new(rows_of(INITIAL)));
    let conn = setup(io.clone(), "sweep_refuse_boundaries.db", &source);

    let mirror_before = mirror_with_rowids(&conn, io.as_ref());
    let view_before = view_of(&conn, io.as_ref());
    assert_eq!(view_before.len(), 3, "{view_before:?}");

    *source.lock().unwrap() = rows_of(DUPLICATED);
    let (boundaries, err) =
        suspensions_before_failure(&conn, io.as_ref(), "REFRESH MATERIALIZED VIEW mv_yield");
    assert_duplicate_refusal(&err.to_string(), "under yields");
    assert_eq!(
        boundaries, 0,
        "the guard now suspends before refusing; every boundary needs covering"
    );

    assert_eq!(
        mirror_with_rowids(&conn, io.as_ref()),
        mirror_before,
        "the refusal wrote the mirror"
    );
    assert_eq!(
        view_of(&conn, io.as_ref()),
        view_before,
        "the refusal moved the view"
    );
    let (_, _, cdc) = observe(&conn, io.as_ref());
    assert_eq!(cdc, 0, "a refused sweep wrote mirror rows");

    *source.lock().unwrap() = rows_of(REPAIRED);
    exec(&conn, io.as_ref(), "REFRESH MATERIALIZED VIEW mv_yield").unwrap();
    assert_eq!(view_of(&conn, io.as_ref()), repaired_view());
}

/// The refusal must cost the *next* sweep nothing, at every suspension that
/// sweep has: a refused REFRESH leaves staged state behind on the connection if
/// it leaves anything at all, and the recovery sweep is where that would show —
/// abandoned at any of its boundaries, the one after it must still converge.
#[test]
fn test_recovery_after_a_refused_sweep_converges_at_every_yield() {
    let boundaries = {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let source = Arc::new(Mutex::new(rows_of(INITIAL)));
        let conn = setup(io.clone(), "sweep_recover_probe.db", &source);
        *source.lock().unwrap() = rows_of(DUPLICATED);
        exec(&conn, io.as_ref(), "REFRESH MATERIALIZED VIEW mv_yield").unwrap_err();
        *source.lock().unwrap() = rows_of(REPAIRED);
        sweep_boundaries_on(&conn, io.as_ref())
    };
    assert!(boundaries > 0, "the recovery sweep performed no IO at all");

    for target in 1..=boundaries {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let source = Arc::new(Mutex::new(rows_of(INITIAL)));
        let conn = setup(io.clone(), &format!("sweep_recover_{target}.db"), &source);

        let mirror_before = mirror_with_rowids(&conn, io.as_ref());
        let view_before = view_of(&conn, io.as_ref());

        *source.lock().unwrap() = rows_of(DUPLICATED);
        let err = exec(&conn, io.as_ref(), "REFRESH MATERIALIZED VIEW mv_yield")
            .expect_err("a source repeating an identity must be refused");
        assert_duplicate_refusal(&err.to_string(), &format!("before boundary {target}"));
        assert_eq!(
            mirror_with_rowids(&conn, io.as_ref()),
            mirror_before,
            "the refusal before boundary {target} wrote the mirror"
        );
        assert_eq!(
            view_of(&conn, io.as_ref()),
            view_before,
            "the refusal before boundary {target} moved the view"
        );

        *source.lock().unwrap() = rows_of(REPAIRED);
        exec_abandoning_at(
            &conn,
            io.as_ref(),
            "REFRESH MATERIALIZED VIEW mv_yield",
            target,
        )
        .unwrap();
        exec(&conn, io.as_ref(), "REFRESH MATERIALIZED VIEW mv_yield").unwrap();
        assert_eq!(
            view_of(&conn, io.as_ref()),
            repaired_view(),
            "recovery abandoned at boundary {target} never converged"
        );
    }
}

/// How many suspensions the pending sweep on `conn` has.
fn sweep_boundaries_on(conn: &Arc<Connection>, io: &dyn IO) -> usize {
    let mut count = 0usize;
    let mut stmt = conn.prepare("REFRESH MATERIALIZED VIEW mv_yield").unwrap();
    loop {
        match stmt.step().unwrap() {
            StepResult::IO | StepResult::Yield => {
                count += 1;
                io.step().unwrap();
            }
            StepResult::Row => {}
            StepResult::Done => break,
            other => panic!("sweep did not complete: {other:?}"),
        }
    }
    count
}

/// The other broken promise, under the same yielding IO: a NULL identity is
/// refused as a NULL, not as a duplicate, and costs the mirror nothing.
#[test]
fn test_null_identity_sweep_under_io_yields_is_refused_as_a_null() {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let source = Arc::new(Mutex::new(rows_of(INITIAL)));
    let conn = setup(io.clone(), "sweep_refuse_null.db", &source);

    let mirror_before = mirror_with_rowids(&conn, io.as_ref());
    let view_before = view_of(&conn, io.as_ref());

    *source.lock().unwrap() = rows_of(NULLED);
    let err = exec(&conn, io.as_ref(), "REFRESH MATERIALIZED VIEW mv_yield")
        .expect_err("a NULL identity must be refused by the sweep");
    let msg = err.to_string();
    assert!(msg.contains("msg_yield"), "{msg}");
    assert!(msg.to_lowercase().contains("null"), "{msg}");
    assert!(
        !msg.contains("more than one row"),
        "a NULL identity is not a duplicate identity: {msg}"
    );

    assert_eq!(
        mirror_with_rowids(&conn, io.as_ref()),
        mirror_before,
        "the NULL refusal wrote the mirror"
    );
    assert_eq!(
        view_of(&conn, io.as_ref()),
        view_before,
        "the NULL refusal moved the view"
    );

    *source.lock().unwrap() = rows_of(REPAIRED);
    exec(&conn, io.as_ref(), "REFRESH MATERIALIZED VIEW mv_yield").unwrap();
    assert_eq!(view_of(&conn, io.as_ref()), repaired_view());
}
