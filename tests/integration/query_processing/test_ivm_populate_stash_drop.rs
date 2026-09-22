//! The statement `populate_from_table` stashes across IO yields.
//!
//! `CREATE MATERIALIZED VIEW` populates by preparing an ordinary SELECT on the
//! *parent* connection (`core/incremental/view.rs`, `PopulateState::
//! ProcessingAllTables` -> `ProcessingOneTable`) and stashing it in the view's
//! populate state across every suspension. The statement is prepared with
//! `Connection::prepare`, i.e. `StatementOrigin::Root`, so it carries no nested
//! guard of its own: the only thing marking it nested is the enclosing
//! `conn.start_nested()` / `end_nested()` pair around `populate_from_table`.
//!
//! Every suspension leaves that window. If the stashed statement is dropped
//! while the window is open nowhere — the CREATE abandoned, or interrupted so
//! `populate_from_table` returns `Busy` with the statement still stashed — its
//! `Drop` runs `reset_best_effort` with `is_nested_stmt() == false` and takes
//! the *top-level* transaction-finalization branch.
//!
//! These probes drive that window at every boundary the populate has and ask
//! the connection to prove it is still whole afterwards.

#![cfg(feature = "io_memory_yield")]

use std::sync::Arc;
use turso_core::{
    Connection, Database, DatabaseOpts, MemoryYieldIO, Numeric, OpenFlags, SqliteDialect,
    StepResult, Value, IO,
};

fn exec(conn: &Arc<Connection>, io: &dyn IO, sql: &str) -> turso_core::Result<Vec<Vec<Value>>> {
    let mut stmt = conn.prepare(sql)?;
    let mut out = Vec::new();
    loop {
        match stmt.step()? {
            StepResult::IO | StepResult::Yield => io.step()?,
            StepResult::Row => out.push(stmt.row().unwrap().get_values().cloned().collect()),
            StepResult::Done => return Ok(out),
            StepResult::Interrupt | StepResult::Busy => {
                return Err(turso_core::LimboError::Busy);
            }
        }
    }
}

fn render(value: &Value) -> String {
    match value {
        Value::Text(t) => t.as_str().to_string(),
        Value::Numeric(Numeric::Integer(i)) => i.to_string(),
        Value::Null => "NULL".to_string(),
        other => format!("{other:?}"),
    }
}

fn strings(rows: &[Vec<Value>]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(render).collect())
        .collect()
}

/// Enough rows to span many pages, so the populate scan reads pages the
/// connection's cache does not already hold and therefore genuinely suspends.
const ROWS: usize = 400;
const CREATE_MV: &str = "CREATE MATERIALIZED VIEW mv AS SELECT id, body FROM t";

fn open(io: &Arc<dyn IO>, name: &str) -> Arc<Connection> {
    let db = Database::open_file_with_flags(
        io.clone(),
        name,
        OpenFlags::default(),
        DatabaseOpts::new().with_views(true),
        None,
        Arc::new(SqliteDialect),
    )
    .unwrap();
    db.connect().unwrap()
}

/// A database with `t` filled and no view yet, handed back on a *freshly opened*
/// connection so the populate scan starts with a cold page cache.
fn setup(io: &Arc<dyn IO>, name: &str) -> Arc<Connection> {
    {
        let conn = open(io, name);
        exec(&conn, io.as_ref(), "CREATE TABLE t(id INTEGER, body TEXT)").unwrap();
        exec(&conn, io.as_ref(), "BEGIN").unwrap();
        for i in 0..ROWS {
            exec(
                &conn,
                io.as_ref(),
                &format!("INSERT INTO t VALUES ({i}, 'body{i}')"),
            )
            .unwrap();
        }
        exec(&conn, io.as_ref(), "COMMIT").unwrap();
    }
    open(io, name)
}

fn expected_view() -> Vec<Vec<String>> {
    (0..ROWS)
        .map(|i| vec![i.to_string(), format!("body{i}")])
        .collect()
}

fn view_rows(conn: &Arc<Connection>, io: &dyn IO) -> Vec<Vec<String>> {
    strings(&exec(conn, io, "SELECT id, body FROM mv ORDER BY id").unwrap())
}

/// How many suspensions the CREATE has on this backend.
fn create_boundaries(name: &str) -> usize {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let conn = setup(&io, name);
    let mut stmt = conn.prepare(CREATE_MV).unwrap();
    let mut count = 0usize;
    loop {
        match stmt.step().unwrap() {
            StepResult::IO | StepResult::Yield => {
                count += 1;
                io.step().unwrap();
            }
            StepResult::Row => {}
            StepResult::Done => break,
            other => panic!("CREATE did not complete: {other:?}"),
        }
    }
    assert!(count > 0, "the CREATE performed no IO at all");
    count
}

/// How many suspensions a cold full scan of `t` has — the yardstick that says
/// whether the CREATE's boundary count reflects the populate scan at all.
#[test]
fn test_report_cold_scan_boundaries() {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let conn = setup(&io, "stash_drop_scan_yardstick.db");
    let mut stmt = conn.prepare("SELECT count(*) FROM t").unwrap();
    let mut count = 0usize;
    loop {
        match stmt.step().unwrap() {
            StepResult::IO | StepResult::Yield => {
                count += 1;
                io.step().unwrap();
            }
            StepResult::Row => {}
            StepResult::Done => break,
            other => panic!("scan did not complete: {other:?}"),
        }
    }
    eprintln!("cold scan of {ROWS} rows: boundaries={count}");

    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let conn = setup(&io, "stash_drop_tiny_cache.db");
    exec(&conn, io.as_ref(), "PRAGMA cache_size = 2").unwrap();
    let mut stmt = conn.prepare(CREATE_MV).unwrap();
    let mut count = 0usize;
    loop {
        match stmt.step().unwrap() {
            StepResult::IO | StepResult::Yield => {
                count += 1;
                io.step().unwrap();
            }
            StepResult::Row => {}
            StepResult::Done => break,
            other => panic!("CREATE did not complete: {other:?}"),
        }
    }
    eprintln!("CREATE with cache_size=2: boundaries={count}");
}

/// Every oracle the hazard would break, asked of a connection whose CREATE was
/// torn at `target`.
fn assert_connection_is_whole(conn: &Arc<Connection>, io: &dyn IO, target: usize) {
    assert!(
        !conn.is_nested_stmt(),
        "the connection is still marked nested after tearing at {target}"
    );
    // (1) The connection can still run an ordinary explicit transaction.
    exec(conn, io, "BEGIN").unwrap_or_else(|e| panic!("BEGIN wedged at {target}: {e}"));
    exec(conn, io, "INSERT INTO t VALUES (900, 'after')")
        .unwrap_or_else(|e| panic!("INSERT in txn wedged at {target}: {e}"));
    exec(conn, io, "COMMIT").unwrap_or_else(|e| panic!("COMMIT wedged at {target}: {e}"));
    let after = strings(&exec(conn, io, "SELECT count(*) FROM t").unwrap());
    assert_eq!(
        after[0][0],
        (ROWS + 1).to_string(),
        "the committed write is missing at {target}"
    );

    // (2)+(3) Retrying the CREATE succeeds and the view holds exactly the table,
    // including the row written after the torn attempt. A leaked staged delta
    // from the torn populate would show up here as a duplicate or a phantom.
    exec(conn, io, CREATE_MV).unwrap_or_else(|e| panic!("retried CREATE failed at {target}: {e}"));
    let mut expected = expected_view();
    expected.push(vec!["900".to_string(), "after".to_string()]);
    assert_eq!(
        view_rows(conn, io),
        expected,
        "the view did not converge after tearing the CREATE at {target}"
    );

    // (4) A later unrelated DML produces exactly its own delta.
    exec(conn, io, "INSERT INTO t VALUES (901, 'later')").unwrap();
    expected.push(vec!["901".to_string(), "later".to_string()]);
    assert!(
        !conn.is_nested_stmt(),
        "the connection was left nested at {target}"
    );
    assert_eq!(
        view_rows(conn, io),
        expected,
        "a later INSERT did not produce exactly its own view delta at {target}"
    );
}

/// Vector (c): abandon the CREATE at each of its suspensions.
#[test]
fn test_create_matview_abandoned_at_each_yield_leaves_connection_whole() {
    let boundaries = create_boundaries("stash_drop_abandon_probe.db");
    let mut torn_count = 0usize;
    for target in 1..=boundaries {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let conn = setup(&io, &format!("stash_drop_abandon_{target}.db"));

        let mut stmt = conn.prepare(CREATE_MV).unwrap();
        let mut seen = 0usize;
        let mut abandoned = false;
        loop {
            match stmt.step().unwrap() {
                StepResult::IO | StepResult::Yield => {
                    seen += 1;
                    if seen == target {
                        drop(stmt);
                        abandoned = true;
                        break;
                    }
                    io.step().unwrap();
                }
                StepResult::Row => {}
                StepResult::Done => break,
                other => panic!("CREATE did not complete: {other:?}"),
            }
        }
        if !abandoned {
            continue;
        }
        torn_count += 1;
        assert_connection_is_whole(&conn, io.as_ref(), target);
    }
    eprintln!("abandon: boundaries={boundaries} torn={torn_count}");
    assert!(
        torn_count >= boundaries,
        "the probe never actually abandoned the CREATE"
    );
}

/// Vector (a): interrupt the connection while the CREATE's populate is
/// suspended, so the *inner* stashed SELECT is the thing that observes the
/// interrupt and `populate_from_table` returns with the statement still stashed.
#[test]
fn test_create_matview_interrupted_during_populate_leaves_connection_whole() {
    let boundaries = create_boundaries("stash_drop_interrupt_probe.db");
    let mut torn_count = 0usize;
    let mut kinds: Vec<String> = Vec::new();
    for target in 1..=boundaries {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let conn = setup(&io, &format!("stash_drop_interrupt_{target}.db"));

        let mut stmt = conn.prepare(CREATE_MV).unwrap();
        let mut seen = 0usize;
        let mut torn = false;
        loop {
            let stepped = stmt.step();
            match stepped {
                Ok(StepResult::IO) | Ok(StepResult::Yield) => {
                    seen += 1;
                    io.step().unwrap();
                    if seen == target {
                        conn.interrupt();
                    }
                }
                Ok(StepResult::Row) => {}
                Ok(StepResult::Done) => break,
                // The tear kind is the discriminator for *where* the interrupt
                // landed. `Ok(Interrupt)` is the outer CREATE's own program
                // noticing the flag. `Err(Busy)` can only come out of
                // `populate_from_table`, which converts the inner stashed
                // statement's `StepResult::Interrupt` into `LimboError::Busy`
                // after re-stashing it — that is the hazard's window.
                Ok(other) => {
                    kinds.push(format!("{target}:Ok({other:?})"));
                    torn = true;
                    break;
                }
                Err(e) => {
                    kinds.push(format!("{target}:Err({e})"));
                    torn = true;
                    break;
                }
            }
        }
        drop(stmt);
        if !torn {
            continue;
        }
        torn_count += 1;
        assert_connection_is_whole(&conn, io.as_ref(), target);
    }
    eprintln!("interrupt: boundaries={boundaries} torn={torn_count} kinds={kinds:?}");
    assert!(
        torn_count > 0,
        "no interrupt at any boundary ever tore the CREATE"
    );
}

/// A table large enough that a single `OP_PopulateViews` instruction runs for
/// milliseconds, so an interrupt raised from another thread can land *inside*
/// it rather than at the outer program's per-instruction check.
const BIG_ROWS: usize = 400 << 7;

fn setup_big(io: &Arc<dyn IO>, name: &str) -> Arc<Connection> {
    {
        let conn = open(io, name);
        exec(&conn, io.as_ref(), "CREATE TABLE t(id INTEGER, body TEXT)").unwrap();
        exec(&conn, io.as_ref(), "BEGIN").unwrap();
        for i in 0..ROWS {
            exec(
                &conn,
                io.as_ref(),
                &format!("INSERT INTO t VALUES ({i}, 'body{i}')"),
            )
            .unwrap();
        }
        exec(&conn, io.as_ref(), "COMMIT").unwrap();
        for shift in 0..7 {
            let offset = ROWS << shift;
            exec(
                &conn,
                io.as_ref(),
                &format!("INSERT INTO t SELECT id + {offset}, body FROM t"),
            )
            .unwrap();
        }
        let n = strings(&exec(&conn, io.as_ref(), "SELECT count(*) FROM t").unwrap());
        assert_eq!(n[0][0], BIG_ROWS.to_string());
    }
    open(io, name)
}

/// Vector (a), aimed at the window rather than at the outer program: raise the
/// interrupt from another thread while the populate instruction is running, so
/// the *inner* stashed SELECT is what observes it. `populate_from_table` then
/// re-stashes the statement and returns `LimboError::Busy`, and the statement
/// stays stashed with the nested window closed.
#[test]
fn test_interrupt_landing_inside_populate_leaves_connection_whole() {
    let mut kinds: Vec<String> = Vec::new();
    let mut inside = 0usize;

    for (attempt, delay_us) in [0u64, 50, 200, 500, 1000, 2000, 4000, 8000]
        .into_iter()
        .enumerate()
    {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let conn = setup_big(&io, &format!("stash_drop_race_{attempt}.db"));

        let mut stmt = conn.prepare(CREATE_MV).unwrap();
        // Get past the boundaries that precede the populate, then arm the
        // interrupt and step straight into the long populate instruction.
        match stmt.step().unwrap() {
            StepResult::IO | StepResult::Yield => io.step().unwrap(),
            other => panic!("CREATE did not suspend first: {other:?}"),
        }
        let racer = {
            let conn = conn.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_micros(delay_us));
                conn.interrupt();
            })
        };

        let mut torn = false;
        loop {
            match stmt.step() {
                Ok(StepResult::IO) | Ok(StepResult::Yield) => io.step().unwrap(),
                Ok(StepResult::Row) => {}
                Ok(StepResult::Done) => break,
                // `StepResult::Busy` is the populate-path signal, and the only
                // one that proves the interrupt landed inside the window:
                // `populate_from_table` maps the inner statement's
                // `StepResult::Interrupt` to `Err(LimboError::Busy)`
                // (core/incremental/view.rs:1928-1937) after re-stashing it, and
                // the VDBE turns that into `Ok(StepResult::Busy)` with the PC
                // unmoved and no abort (core/vdbe/mod.rs:2021-2024).
                // `StepResult::Interrupt` is the outer program's own check.
                Ok(StepResult::Busy) => {
                    kinds.push(format!("{delay_us}us:Busy(inside populate)"));
                    inside += 1;
                    torn = true;
                    break;
                }
                Ok(other) => {
                    kinds.push(format!("{delay_us}us:Ok({other:?})"));
                    torn = true;
                    break;
                }
                Err(e) => {
                    kinds.push(format!("{delay_us}us:Err({e})"));
                    torn = true;
                    break;
                }
            }
        }
        racer.join().unwrap();
        drop(stmt);
        if !torn {
            kinds.push(format!("{delay_us}us:completed"));
            continue;
        }
        assert_connection_is_whole_big(&conn, io.as_ref(), delay_us as usize);
    }

    eprintln!("race: inside_populate={inside} kinds={kinds:?}");
    assert!(
        inside > 0,
        "no interrupt ever landed inside the populate instruction: {kinds:?}"
    );
}

/// The same oracles as `assert_connection_is_whole`, over the big table.
fn assert_connection_is_whole_big(conn: &Arc<Connection>, io: &dyn IO, target: usize) {
    // The nested guard around `populate_from_table` must be balanced: a leaked
    // `start_nested` is the historical signature, and it silently disables
    // top-level transaction finalization for every later statement.
    assert!(
        !conn.is_nested_stmt(),
        "the connection is still marked nested after tearing at {target}"
    );
    exec(conn, io, "BEGIN").unwrap_or_else(|e| panic!("BEGIN wedged at {target}: {e}"));
    exec(conn, io, "INSERT INTO t VALUES (-1, 'after')")
        .unwrap_or_else(|e| panic!("INSERT in txn wedged at {target}: {e}"));
    exec(conn, io, "COMMIT").unwrap_or_else(|e| panic!("COMMIT wedged at {target}: {e}"));
    let n = strings(&exec(conn, io, "SELECT count(*) FROM t").unwrap());
    assert_eq!(
        n[0][0],
        (BIG_ROWS + 1).to_string(),
        "the committed write is missing at {target}"
    );

    exec(conn, io, CREATE_MV).unwrap_or_else(|e| panic!("retried CREATE failed at {target}: {e}"));
    let view = strings(&exec(conn, io, "SELECT count(*) FROM mv").unwrap());
    assert_eq!(
        view[0][0],
        (BIG_ROWS + 1).to_string(),
        "the view did not converge after tearing the CREATE at {target}"
    );

    exec(conn, io, "INSERT INTO t VALUES (-2, 'later')").unwrap();
    let view = strings(&exec(conn, io, "SELECT count(*) FROM mv").unwrap());
    assert_eq!(
        view[0][0],
        (BIG_ROWS + 2).to_string(),
        "a later INSERT did not produce exactly its own view delta at {target}"
    );
    assert!(
        !conn.is_nested_stmt(),
        "the connection was left nested at {target}"
    );
}

/// The other half of vector (a): `StepResult::Busy` leaves the PC unmoved and
/// the statement alive, so the SQLite-shaped caller response is to retry rather
/// than to abandon. This asks whether a torn populate can be *resumed*.
#[test]
fn test_populate_busy_is_retryable_or_terminal() {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let conn = setup_big(&io, "stash_drop_retry.db");

    let mut stmt = conn.prepare(CREATE_MV).unwrap();
    match stmt.step().unwrap() {
        StepResult::IO | StepResult::Yield => io.step().unwrap(),
        other => panic!("CREATE did not suspend first: {other:?}"),
    }
    let racer = {
        let conn = conn.clone();
        std::thread::spawn(move || conn.interrupt())
    };

    let mut busy_seen = 0usize;
    let outcome = loop {
        match stmt.step() {
            Ok(StepResult::IO) | Ok(StepResult::Yield) => io.step().unwrap(),
            Ok(StepResult::Row) => {}
            Ok(StepResult::Done) => break "completed".to_string(),
            Ok(StepResult::Busy) => {
                busy_seen += 1;
                if busy_seen > 200 {
                    break "busy-livelock".to_string();
                }
            }
            Ok(other) => break format!("{other:?}"),
            Err(e) => break format!("Err({e})"),
        }
    };
    racer.join().unwrap();
    drop(stmt);
    eprintln!("retry: busy_seen={busy_seen} outcome={outcome}");

    // Whatever the retry story is, the connection must still be whole.
    assert_connection_is_whole_big(&conn, io.as_ref(), 0);
}

/// A torn populate must not damage what other connections see, and the writes
/// the torn connection commits afterwards must really be durable — the proof
/// that top-level transaction finalization was not disabled by a leaked guard.
#[test]
fn test_torn_populate_leaves_no_durable_damage() {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let name = "stash_drop_durable.db";
    let conn = setup_big(&io, name);

    let mut stmt = conn.prepare(CREATE_MV).unwrap();
    match stmt.step().unwrap() {
        StepResult::IO | StepResult::Yield => io.step().unwrap(),
        other => panic!("CREATE did not suspend first: {other:?}"),
    }
    let racer = {
        let conn = conn.clone();
        std::thread::spawn(move || conn.interrupt())
    };
    loop {
        match stmt.step() {
            Ok(StepResult::IO) | Ok(StepResult::Yield) => io.step().unwrap(),
            Ok(StepResult::Row) => {}
            Ok(StepResult::Done) => break,
            Ok(_) | Err(_) => break,
        }
    }
    racer.join().unwrap();
    drop(stmt);

    exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-7, 'durable')").unwrap();

    let fresh = open(&io, name);
    let n = strings(&exec(&fresh, io.as_ref(), "SELECT count(*) FROM t").unwrap());
    assert_eq!(
        n[0][0],
        (BIG_ROWS + 1).to_string(),
        "the write after a torn populate never reached another connection"
    );
    let seen = strings(
        &exec(
            &fresh,
            io.as_ref(),
            "SELECT count(*) FROM sqlite_master WHERE name = 'mv'",
        )
        .unwrap(),
    );
    assert_eq!(
        seen[0][0], "0",
        "a torn CREATE left the view behind for another connection"
    );
}

/// The sharpest form of the hazard: tear the populate inside an *explicit*
/// transaction that already holds uncommitted writes.
///
/// If the stashed populate SELECT's `Drop` ran top-level finalization — the
/// branch it takes whenever `is_nested_stmt()` is false, which is every moment
/// outside the `start_nested`/`end_nested` pair around `populate_from_table` —
/// it would commit or roll back the *caller's* open transaction underneath it.
/// It does not: the transaction stays the caller's to end, and the write made
/// before the torn CREATE is still undone by `ROLLBACK`.
///
/// The torn transaction *is* doomed — every later statement on it answers
/// "Database schema changed". That is not this hazard: it reproduces with an
/// interrupted `CREATE TABLE ... AS SELECT`, which stashes nothing and has no
/// materialized view anywhere near it (see `test_explicit_txn_tear_controls`).
#[test]
fn test_torn_populate_inside_explicit_txn_does_not_steal_the_txn() {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let name = "stash_drop_explicit_txn.db";
    let conn = setup_big(&io, name);

    exec(&conn, io.as_ref(), "BEGIN").unwrap();
    exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-11, 'before')").unwrap();

    let mut stmt = conn.prepare(CREATE_MV).unwrap();
    match stmt.step().unwrap() {
        StepResult::IO | StepResult::Yield => io.step().unwrap(),
        StepResult::Done => panic!("CREATE completed before suspending"),
        other => panic!("unexpected first step: {other:?}"),
    }
    let racer = {
        let conn = conn.clone();
        std::thread::spawn(move || conn.interrupt())
    };
    let mut torn = None;
    loop {
        match stmt.step() {
            Ok(StepResult::IO) | Ok(StepResult::Yield) => io.step().unwrap(),
            Ok(StepResult::Row) => {}
            Ok(StepResult::Done) => break,
            Ok(other) => {
                torn = Some(format!("{other:?}"));
                break;
            }
            Err(e) => {
                torn = Some(format!("Err({e})"));
                break;
            }
        }
    }
    racer.join().unwrap();
    // The drop is the event under test: it is where the stashed statement is
    // released, with the connection's nested counter back at zero.
    drop(stmt);
    assert_eq!(
        torn.as_deref(),
        Some("Busy"),
        "the interrupt did not land inside the populate; the probe proved nothing"
    );
    assert!(
        !conn.is_nested_stmt(),
        "the nested guard around populate_from_table was left standing"
    );

    // The caller still owns the transaction: it ends when the caller says so,
    // and it ends the way the caller says. A stolen commit would leave -11
    // behind; a stolen rollback would have closed the transaction already.
    exec(&conn, io.as_ref(), "ROLLBACK").expect("the transaction was not the caller's to end");
    let fresh = open(&io, name);
    let n = strings(&exec(&fresh, io.as_ref(), "SELECT count(*) FROM t").unwrap());
    assert_eq!(
        n[0][0],
        BIG_ROWS.to_string(),
        "the torn populate's stashed statement committed the caller's transaction"
    );
    let seen = strings(
        &exec(
            &fresh,
            io.as_ref(),
            "SELECT count(*) FROM sqlite_master WHERE name = 'mv'",
        )
        .unwrap(),
    );
    assert_eq!(seen[0][0], "0", "a torn CREATE left the view behind");
}

/// Run `sql` inside an already-open explicit transaction on `conn`, tearing it
/// with a racing interrupt, and report how the transaction fares afterwards.
fn tear_in_txn(conn: &Arc<Connection>, io: &dyn IO, sql: &str, interrupt: bool) -> String {
    let mut stmt = conn.prepare(sql).unwrap();
    match stmt.step().unwrap() {
        StepResult::IO | StepResult::Yield => io.step().unwrap(),
        StepResult::Done => return "completed-immediately".to_string(),
        other => panic!("unexpected first step: {other:?}"),
    }
    let racer = interrupt.then(|| {
        let conn = conn.clone();
        std::thread::spawn(move || conn.interrupt())
    });
    let mut torn = "completed".to_string();
    loop {
        match stmt.step() {
            Ok(StepResult::IO) | Ok(StepResult::Yield) => io.step().unwrap(),
            Ok(StepResult::Row) => {}
            Ok(StepResult::Done) => break,
            Ok(other) => {
                torn = format!("{other:?}");
                break;
            }
            Err(e) => {
                torn = format!("Err({e})");
                break;
            }
        }
    }
    if let Some(racer) = racer {
        racer.join().unwrap();
    }
    drop(stmt);

    match exec(conn, io, "INSERT INTO t VALUES (-12, 'after')") {
        Ok(_) => format!("torn={torn} then=ok"),
        Err(e) => format!("torn={torn} then=Err({e})"),
    }
}

/// The controls that say whether the wedge above belongs to the populate stash
/// or to torn work in an explicit transaction generally.
#[test]
fn test_explicit_txn_tear_controls() {
    let mut report = Vec::new();

    // Control 1: a long *non-DDL* statement torn by the same racing interrupt.
    {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let conn = setup_big(&io, "stash_ctl_update.db");
        exec(&conn, io.as_ref(), "BEGIN").unwrap();
        exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-11, 'before')").unwrap();
        report.push(format!(
            "long-UPDATE: {}",
            tear_in_txn(
                &conn,
                io.as_ref(),
                "UPDATE t SET body = body || 'x' WHERE id >= 0",
                true
            )
        ));
    }

    // Control 2: an ordinary DDL that *fails* inside the transaction.
    {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let conn = setup_big(&io, "stash_ctl_faildll.db");
        exec(&conn, io.as_ref(), "BEGIN").unwrap();
        exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-11, 'before')").unwrap();
        let err = exec(&conn, io.as_ref(), "CREATE TABLE t(x)").unwrap_err();
        let then = match exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-12, 'after')") {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("Err({e})"),
        };
        report.push(format!("failed-DDL: err=({err}) then={then}"));
    }

    // Control 3: the same CREATE MATERIALIZED VIEW, torn the *other* way — the
    // outer program's own interrupt check, with the populate never entered
    // (interrupt armed before the first step reaches it).
    {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let conn = setup_big(&io, "stash_ctl_outer.db");
        exec(&conn, io.as_ref(), "BEGIN").unwrap();
        exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-11, 'before')").unwrap();
        let mut stmt = conn.prepare(CREATE_MV).unwrap();
        match stmt.step().unwrap() {
            StepResult::IO | StepResult::Yield => io.step().unwrap(),
            other => panic!("unexpected first step: {other:?}"),
        }
        // Armed between steps: the outer program's per-instruction check sees it
        // first, so the populate is never entered and nothing is stashed.
        conn.interrupt();
        let torn = match stmt.step() {
            Ok(other) => format!("{other:?}"),
            Err(e) => format!("Err({e})"),
        };
        drop(stmt);
        let then = match exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-12, 'after')") {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("Err({e})"),
        };
        report.push(format!("outer-interrupt-CMV: torn={torn} then={then}"));
    }

    // Control 4: a successful CREATE MATERIALIZED VIEW in the same transaction —
    // does an untorn matview DDL leave the transaction usable at all?
    {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let conn = setup_big(&io, "stash_ctl_clean.db");
        exec(&conn, io.as_ref(), "BEGIN").unwrap();
        exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-11, 'before')").unwrap();
        let create = match exec(&conn, io.as_ref(), CREATE_MV) {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("Err({e})"),
        };
        let then = match exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-12, 'after')") {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("Err({e})"),
        };
        report.push(format!("clean-CMV: create={create} then={then}"));
    }

    // Control 5: an interrupted DDL with no materialized view anywhere near it.
    // If this wedges too, the wedge belongs to aborted DDL inside an explicit
    // transaction and never touches the populate stash.
    {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let conn = setup_big(&io, "stash_ctl_ddl_index.db");
        exec(&conn, io.as_ref(), "BEGIN").unwrap();
        exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-11, 'before')").unwrap();
        report.push(format!(
            "interrupted-CREATE-INDEX: {}",
            tear_in_txn(&conn, io.as_ref(), "CREATE INDEX ix ON t(body)", true)
        ));
    }
    {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let conn = setup_big(&io, "stash_ctl_ddl_ctas.db");
        exec(&conn, io.as_ref(), "BEGIN").unwrap();
        exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-11, 'before')").unwrap();
        report.push(format!(
            "interrupted-CTAS: {}",
            tear_in_txn(
                &conn,
                io.as_ref(),
                "CREATE TABLE t2 AS SELECT * FROM t",
                true
            )
        ));
    }

    // Is the wedge permanent, or does ROLLBACK recover the connection?
    {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let conn = setup_big(&io, "stash_ctl_recover.db");
        exec(&conn, io.as_ref(), "BEGIN").unwrap();
        exec(&conn, io.as_ref(), "INSERT INTO t VALUES (-11, 'before')").unwrap();
        let torn = tear_in_txn(&conn, io.as_ref(), CREATE_MV, true);
        let rolled = match exec(&conn, io.as_ref(), "ROLLBACK") {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("Err({e})"),
        };
        let after = match exec(&conn, io.as_ref(), "SELECT count(*) FROM t") {
            Ok(rows) => render(&rows[0][0]),
            Err(e) => format!("Err({e})"),
        };
        report.push(format!("recovery: {torn} rollback={rolled} count={after}"));
    }

    for line in &report {
        eprintln!("control: {line}");
    }
}

// ---------------------------------------------------------------------------
// Vector (d): the FDW mirror sync's copy of the idiom.
//
// `sync_fdw_mirrors` (core/vdbe/execute.rs:13726) wraps the same nested window,
// and stashes its in-flight statement into `state.fdw_mirror_sync.in_flight` on
// both the IO path (13828) and the Interrupt/Busy path (13834) — the latter
// again returning `LimboError::Busy` with the statement stashed and the window
// closed. It is the sharper copy: `prepare_mirror_stmt` (13746) prepares *write*
// DML through `Connection::prepare` and then forces
// `needs_stmt_subtransactions` off, so a drop outside the window is a write
// statement taking the top-level finalization branch.
// ---------------------------------------------------------------------------

use turso_core::foreign::{ForeignCursor, ForeignDataWrapper, KeyColumn, PushedConstraint};

type FdwRow = (String, String);

#[derive(Debug)]
struct BigFdw {
    rows: Arc<std::sync::Mutex<Vec<FdwRow>>>,
}

impl ForeignDataWrapper for BigFdw {
    fn key_columns(&self) -> &[KeyColumn] {
        &[]
    }
    fn identity_columns(&self) -> Option<&[u32]> {
        Some(&[0])
    }
    fn schema_sql(&self) -> String {
        "CREATE TABLE src(uuid TEXT, body TEXT)".to_string()
    }
    fn open_cursor(&self, _conn: Arc<Connection>) -> turso_core::Result<Box<dyn ForeignCursor>> {
        Ok(Box::new(BigCursor {
            source: self.rows.clone(),
            rows: Vec::new(),
            index: 0,
        }))
    }
}

struct BigCursor {
    source: Arc<std::sync::Mutex<Vec<FdwRow>>>,
    rows: Vec<FdwRow>,
    index: usize,
}

impl ForeignCursor for BigCursor {
    fn filter(&mut self, _c: &[PushedConstraint]) -> turso_core::Result<bool> {
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
            0 => Value::build_text(row.0.clone()),
            1 => Value::build_text(row.1.clone()),
            _ => Value::Null,
        })
    }
    fn rowid(&self) -> i64 {
        self.index as i64
    }
}

const FDW_ROWS: usize = 20_000;

fn fdw_rows(tag: &str) -> Vec<FdwRow> {
    (0..FDW_ROWS)
        .map(|i| (format!("u{i:06}"), format!("{tag}{i}")))
        .collect()
}

fn setup_fdw(
    io: &Arc<dyn IO>,
    name: &str,
    rows: &Arc<std::sync::Mutex<Vec<FdwRow>>>,
) -> Arc<Connection> {
    let conn = open(io, name);
    conn.register_foreign_table("src", Arc::new(BigFdw { rows: rows.clone() }))
        .unwrap();
    conn
}

/// Run the FDW CREATE with an interrupt armed `delay_us` after the racer
/// starts, stepping from the very first step so a statement that performs no IO
/// at all is still torn mid-instruction. Returns how it ended.
fn tear_fdw_create(conn: &Arc<Connection>, io: &dyn IO, delay_us: u64) -> String {
    let mut stmt = conn
        .prepare("CREATE MATERIALIZED VIEW mvf AS SELECT uuid, body FROM src")
        .unwrap();
    let racer = {
        let conn = conn.clone();
        std::thread::spawn(move || {
            if delay_us > 0 {
                std::thread::sleep(std::time::Duration::from_micros(delay_us));
            }
            conn.interrupt();
        })
    };
    let mut torn = "completed".to_string();
    loop {
        match stmt.step() {
            Ok(StepResult::IO) | Ok(StepResult::Yield) => io.step().unwrap(),
            Ok(StepResult::Row) => {}
            Ok(StepResult::Done) => break,
            Ok(other) => {
                torn = format!("{other:?}");
                break;
            }
            Err(e) => {
                torn = format!("Err({e})");
                break;
            }
        }
    }
    racer.join().unwrap();
    drop(stmt);
    torn
}

/// The delays at which the interrupt lands inside the mirror sync rather than
/// at the outer program's own check. `Busy` is the mirror-sync signal, for the
/// same reason it is the populate signal: `sync_fdw_mirrors_inner` stashes the
/// statement and returns `LimboError::Busy` (execute.rs:13834-13836). Swept
/// densely across three orders of magnitude so at least one delay lands inside
/// the window on machines much faster or much slower than this one.
const FDW_DELAYS: [u64; 20] = [
    50, 100, 200, 350, 500, 750, 1000, 1500, 2000, 3000, 4000, 6000, 8000, 12000, 16000, 24000,
    32000, 48000, 64000, 100000,
];

/// Tear a mirror-syncing statement with a racing interrupt and demand the
/// connection prove it is still whole — including that it did not silently
/// finalize a transaction the caller owns.
#[test]
fn test_torn_fdw_mirror_sync_does_not_steal_the_txn() {
    let mut kinds = Vec::new();
    let mut inside = 0usize;

    for (attempt, delay_us) in FDW_DELAYS.into_iter().enumerate() {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let name = format!("stash_drop_fdw_txn_{attempt}.db");
        let source = Arc::new(std::sync::Mutex::new(fdw_rows("a")));
        let conn = setup_fdw(&io, &name, &source);

        exec(&conn, io.as_ref(), "CREATE TABLE keep(id INTEGER)").unwrap();
        exec(&conn, io.as_ref(), "BEGIN").unwrap();
        exec(&conn, io.as_ref(), "INSERT INTO keep VALUES (-11)").unwrap();

        let torn = tear_fdw_create(&conn, io.as_ref(), delay_us);
        kinds.push(format!("{delay_us}us:{torn}"));
        if torn != "Busy" {
            continue;
        }
        inside += 1;

        assert!(
            !conn.is_nested_stmt(),
            "the nested guard was left standing at {delay_us}us"
        );
        // The caller's transaction is still the caller's: it ends when and how
        // the caller says. A stolen commit would leave -11 durable.
        exec(&conn, io.as_ref(), "ROLLBACK")
            .unwrap_or_else(|e| panic!("the transaction was not the caller's to end: {e}"));
        let fresh = open(&io, &name);
        let kept = strings(&exec(&fresh, io.as_ref(), "SELECT count(*) FROM keep").unwrap());
        assert_eq!(
            kept[0][0], "0",
            "the torn mirror sync's stashed write statement committed the caller's transaction"
        );
        let left = strings(
            &exec(
                &fresh,
                io.as_ref(),
                "SELECT count(*) FROM sqlite_master WHERE name LIKE '%fdw_mirror%' OR name = 'mvf'",
            )
            .unwrap(),
        );
        assert_eq!(
            left[0][0], "0",
            "a torn mirror sync left mirror or view schema behind"
        );
    }

    eprintln!("fdw txn: inside={inside} kinds={kinds:?}");
    assert!(
        inside > 0,
        "no interrupt landed inside the mirror sync: {kinds:?}"
    );
}

/// The same tear in autocommit, followed by the convergence oracle: the retried
/// CREATE must reach exactly the source's contents, with no leaked mirror rows.
#[test]
fn test_torn_fdw_mirror_sync_converges_on_retry() {
    let mut kinds = Vec::new();
    let mut inside = 0usize;

    for (attempt, delay_us) in FDW_DELAYS.into_iter().enumerate() {
        let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
        let source = Arc::new(std::sync::Mutex::new(fdw_rows("a")));
        let conn = setup_fdw(&io, &format!("stash_drop_fdw_retry_{attempt}.db"), &source);

        let torn = tear_fdw_create(&conn, io.as_ref(), delay_us);
        kinds.push(format!("{delay_us}us:{torn}"));
        if torn != "Busy" {
            continue;
        }
        inside += 1;
        assert!(
            !conn.is_nested_stmt(),
            "the nested guard was left standing at {delay_us}us"
        );

        exec(
            &conn,
            io.as_ref(),
            "CREATE MATERIALIZED VIEW mvf AS SELECT uuid, body FROM src",
        )
        .unwrap_or_else(|e| panic!("the retried CREATE failed at {delay_us}us: {e}"));
        let n = strings(&exec(&conn, io.as_ref(), "SELECT count(*) FROM mvf").unwrap());
        assert_eq!(
            n[0][0],
            FDW_ROWS.to_string(),
            "the view did not converge on the source after a torn mirror sync"
        );
        let mirror = strings(
            &exec(
                &conn,
                io.as_ref(),
                "SELECT count(*) FROM \"__turso_internal_fdw_mirror_v1_mvf__src\"",
            )
            .unwrap(),
        );
        assert_eq!(
            mirror[0][0],
            FDW_ROWS.to_string(),
            "the mirror carries rows the torn sync leaked"
        );
    }

    eprintln!("fdw retry: inside={inside} kinds={kinds:?}");
    assert!(
        inside > 0,
        "no interrupt landed inside the mirror sync: {kinds:?}"
    );
}

/// Vector (b): a second connection contending for the write lock while the
/// populate is suspended.
///
/// The `Busy` arm of the populate loop (view.rs:1928) is shared with
/// `Interrupt`, so a `Busy` from the inner statement would stash it the same
/// way. It cannot arrive from lock contention: the suspended CREATE already
/// holds the write lock, and the inner populate SELECT reads through the same
/// connection. The contending writer is the one refused, and the CREATE
/// resumes and completes untouched.
#[test]
fn test_contending_writer_cannot_make_the_populate_busy() {
    let io = Arc::new(MemoryYieldIO::new()) as Arc<dyn IO>;
    let name = "stash_drop_contend.db";
    let conn = setup(&io, name);
    let other = open(&io, name);

    let mut stmt = conn.prepare(CREATE_MV).unwrap();
    match stmt.step().unwrap() {
        StepResult::IO | StepResult::Yield => io.step().unwrap(),
        other => panic!("unexpected first step: {other:?}"),
    }

    let refused = exec(&other, io.as_ref(), "INSERT INTO t VALUES (-3, 'contend')");
    assert!(
        refused.is_err(),
        "a second connection wrote while the CREATE held the lock"
    );

    loop {
        match stmt.step() {
            Ok(StepResult::IO) | Ok(StepResult::Yield) => io.step().unwrap(),
            Ok(StepResult::Row) => {}
            Ok(StepResult::Done) => break,
            Ok(other) => panic!("contention tore the populate: {other:?}"),
            Err(e) => panic!("contention tore the populate: {e}"),
        }
    }
    drop(stmt);

    assert_eq!(
        view_rows(&conn, io.as_ref()),
        expected_view(),
        "the view did not converge after a contending writer was refused"
    );
    exec(&other, io.as_ref(), "INSERT INTO t VALUES (-3, 'contend')")
        .expect("the refused write must apply once the CREATE is done");
}
