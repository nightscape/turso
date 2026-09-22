//! `REFRESH MATERIALIZED VIEW … WHERE <scope>` over a partially scanned source.
//!
//! A driver that can only afford a partial scan — "rows changed since the
//! watermark", "children of these parents" — hands the sweep a scan that is
//! authoritative over part of the source and silent about the rest. Absence
//! from such a scan means "deleted" inside the scope and "not looked at"
//! outside it, and the difference is the whole point: an unscoped sweep over a
//! partial scan retracts every row the scan did not happen to cover.
//!
//! What these cases pin is that the bound is the engine's, and typed: "the scan
//! found nothing" and "no scope was given" are different values, never an empty
//! predicate to be guessed at.

use crate::common::{self, ExecRows, TempDatabase};
use crate::query_processing::fdw_test_driver::{MemFdw, MemRows};
use std::sync::{Arc, Mutex};
use turso_core::foreign::{ForeignCursor, ForeignDataWrapper, KeyColumn, PushedConstraint};
use turso_core::{Connection, Numeric, Value};
use turso_ext::ConstraintOp;

fn row(uuid: &str, part: &str, val: i64) -> Vec<Value> {
    vec![
        Value::build_text(uuid.to_string()),
        Value::build_text(part.to_string()),
        Value::Numeric(Numeric::Integer(val)),
    ]
}

/// A view over `msg_scope(uuid, part, val)`, `uuid` the identity.
///
/// The view projects `uuid` and `val` only: `part` is a mirror column the view
/// never reads, which is where a scan scope lives — it bounds what the source
/// was asked for, not what the view shows.
fn setup(
    tmp_db: &TempDatabase,
    conn: &Arc<Connection>,
    table: &str,
    view: &str,
    seed: Vec<Vec<Value>>,
) -> anyhow::Result<MemRows> {
    let (fdw, rows) = MemFdw::new(
        &format!("CREATE TABLE {table}(uuid TEXT, part TEXT, val INTEGER)"),
        vec![0],
    );
    rows.set(seed);
    conn.register_foreign_table(table, fdw)?;
    common::run_query(
        tmp_db,
        conn,
        &format!("CREATE MATERIALIZED VIEW {view} AS SELECT uuid, val FROM {table}"),
    )?;
    Ok(rows)
}

fn view_rows(conn: &Arc<Connection>, view: &str) -> Vec<(String, i64)> {
    conn.exec_rows(&format!("SELECT uuid, val FROM {view} ORDER BY uuid"))
}

fn seed() -> Vec<Vec<Value>> {
    vec![row("a1", "p1", 1), row("a2", "p1", 2), row("b1", "p2", 3)]
}

/// The case the primitive exists for: a scan that answered for `p1` only.
///
/// `a2` is gone from the source and inside the scope, so its absence is a
/// deletion. `b1` is gone too, but outside the scope — the scan never asked
/// about it, so its absence says nothing and it must survive.
#[turso_macros::test(views)]
fn test_scoped_refresh_retracts_inside_the_scope_and_leaves_the_rest(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let rows = setup(&tmp_db, &conn, "msg_scope", "mv_scope", seed())?;

    // The source now answers the scoped scan with `a1` alone.
    rows.set(vec![row("a1", "p1", 1)]);
    common::run_query(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_scope WHERE part = 'p1'",
    )?;

    assert_eq!(
        view_rows(&conn, "mv_scope"),
        vec![("a1".to_string(), 1), ("b1".to_string(), 3)],
        "only the in-scope row the scan could witness the deletion of may be retracted"
    );

    // And the same view, refreshed unscoped, does retract it: the scope is the
    // only thing that spared it.
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_scope")?;
    assert_eq!(
        view_rows(&conn, "mv_scope"),
        vec![("a1".to_string(), 1)],
        "an unscoped REFRESH is authoritative over the whole source"
    );
    Ok(())
}

/// An empty scoped scan is an answer, not a missing one: it retracts exactly
/// the scope and nothing else. This is the shape a driver-inferred bound kept
/// getting wrong — an empty result read as "no scope" wipes the mirror.
#[turso_macros::test(views)]
fn test_an_empty_scoped_scan_retracts_only_its_scope(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let rows = setup(&tmp_db, &conn, "msg_empty", "mv_empty", seed())?;

    rows.set(vec![]);
    common::run_query(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_empty WHERE part = 'p1'",
    )?;

    assert_eq!(
        view_rows(&conn, "mv_empty"),
        vec![("b1".to_string(), 3)],
        "an empty answer for 'p1' says nothing about 'p2'"
    );
    Ok(())
}

/// The two adversarial edges of the bound: a scope covering everything is a
/// full refresh, and a scope covering nothing is a no-op — even against an
/// empty source, which is the strongest form of "the scan witnessed nothing".
#[turso_macros::test(views)]
fn test_scope_edges_cover_everything_and_nothing(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let rows = setup(&tmp_db, &conn, "msg_edge", "mv_edge", seed())?;

    rows.set(vec![]);
    common::run_query(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_edge WHERE part = 'nothing'",
    )?;
    assert_eq!(
        view_rows(&conn, "mv_edge"),
        vec![
            ("a1".to_string(), 1),
            ("a2".to_string(), 2),
            ("b1".to_string(), 3)
        ],
        "a scope no row matches touches nothing"
    );

    common::run_query(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_edge WHERE val >= 0",
    )?;
    assert!(
        view_rows(&conn, "mv_edge").is_empty(),
        "a scope covering every row is a full refresh"
    );
    Ok(())
}

/// A scoped sweep updates the rows it does cover. The scope bounds absence, not
/// the upsert's reach.
#[turso_macros::test(views)]
fn test_a_scoped_sweep_still_upserts_the_rows_it_covers(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let rows = setup(&tmp_db, &conn, "msg_upsert", "mv_upsert", seed())?;

    rows.set(vec![
        row("a1", "p1", 10),  // changed, in scope
        row("a2", "p1", 2),   // unchanged, in scope
        row("c1", "p1", 4),   // new, in scope
        row("b1", "p2", 300), // changed, out of scope
    ]);
    common::run_query(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_upsert WHERE part = 'p1'",
    )?;

    assert_eq!(
        view_rows(&conn, "mv_upsert"),
        vec![
            ("a1".to_string(), 10),
            ("a2".to_string(), 2),
            ("b1".to_string(), 3),
            ("c1".to_string(), 4)
        ],
        "in-scope rows are inserted and updated; out-of-scope rows are not read at all"
    );
    Ok(())
}

/// The identity guard belongs to the scan it validates, so a scoped sweep
/// refuses a duplicate the scoped scan returns.
#[turso_macros::test(views)]
fn test_a_duplicate_identity_inside_the_scope_is_refused(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let rows = setup(&tmp_db, &conn, "msg_dupscope", "mv_dupscope", seed())?;

    rows.set(vec![
        row("a1", "p1", 1),
        row("a1", "p1", 9),
        row("b1", "p2", 3),
    ]);
    let err = common::run_query(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_dupscope WHERE part = 'p1'",
    )
    .expect_err("a scoped scan repeating an identity must be refused");
    let message = err.to_string();
    assert!(
        message.contains("msg_dupscope") && message.contains("uuid"),
        "the refusal must name the source and its identity: {message}"
    );

    // The same duplicate outside the scope is not the scoped scan's problem:
    // the guard checks what it reads.
    rows.set(vec![
        row("a1", "p1", 1),
        row("a2", "p1", 2),
        row("b1", "p2", 3),
        row("b1", "p2", 4),
    ]);
    common::run_query(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_dupscope WHERE part = 'p1'",
    )?;
    Ok(())
}

fn refusal(tmp_db: &TempDatabase, conn: &Arc<Connection>, sql: &str) -> String {
    common::run_query(tmp_db, conn, sql)
        .expect_err("the scope must be refused")
        .to_string()
}

/// The retraction bound is evaluated against the mirror's stored columns, so a
/// scope that cannot be is refused where the user can still act on it, rather
/// than half-applied.
#[turso_macros::test(views)]
fn test_a_scope_the_mirror_cannot_evaluate_is_refused(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&tmp_db, &conn, "msg_bad", "mv_bad", seed())?;

    let unknown = refusal(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_bad WHERE modified > 5",
    );
    assert!(
        unknown.contains("modified") && unknown.contains("msg_bad"),
        "an unknown scope column must name itself and the source: {unknown}"
    );

    let qualified = refusal(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_bad WHERE msg_bad.part = 'p1'",
    );
    assert!(qualified.contains("unqualified"), "{qualified}");

    let parameterised = refusal(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_bad WHERE part = ?",
    );
    assert!(parameterised.contains("parameter"), "{parameterised}");

    // Nothing was applied by any of them.
    assert_eq!(
        view_rows(&conn, "mv_bad"),
        vec![
            ("a1".to_string(), 1),
            ("a2".to_string(), 2),
            ("b1".to_string(), 3)
        ]
    );
    Ok(())
}

/// The two sides of a scope must select the same rows, including when the
/// answer turns on affinity.
///
/// The scope is evaluated twice over different tables: pushed into the scan
/// over the foreign source, and again over the mirror rows the anti-join walks.
/// `part` is declared `TEXT` and holds digits, so `part = 1` is decided by the
/// column's affinity being applied to the numeric literal — if only one side
/// applies it, the sweep retracts rows no scan spoke for.
///
/// A local table declared the same way is the oracle for what that comparison
/// means at all.
#[turso_macros::test(views)]
fn test_a_scope_needing_affinity_selects_alike_on_both_sides(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let digits = vec![row("a1", "1", 1), row("a2", "1", 2), row("b1", "2", 3)];
    let rows = setup(&tmp_db, &conn, "msg_aff", "mv_aff", digits)?;

    // What the comparison means on an ordinary table of the same declaration.
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE TABLE aff_local(uuid TEXT, part TEXT, val INTEGER)",
    )?;
    common::run_query(
        &tmp_db,
        &conn,
        "INSERT INTO aff_local VALUES ('a1','1',1),('a2','1',2),('b1','2',3)",
    )?;
    let local: Vec<(String,)> = conn.exec_rows("SELECT uuid FROM aff_local WHERE part = 1");
    assert_eq!(
        local,
        vec![("a1".to_string(),), ("a2".to_string(),)],
        "TEXT affinity applies to the literal, so the digits match"
    );

    // Side one: the rows the scan covers.
    let scanned: Vec<(String,)> =
        conn.exec_rows("SELECT uuid FROM msg_aff WHERE part = 1 ORDER BY uuid");
    assert_eq!(
        scanned, local,
        "the scan side must read the scope as SQLite does"
    );

    // Side two: the rows the retraction bound covers. An empty scan makes the
    // bound the only thing deciding what goes.
    rows.set(vec![]);
    common::run_query(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_aff WHERE part = 1",
    )?;
    let survivors = view_rows(&conn, "mv_aff");
    assert_eq!(
        survivors,
        vec![("b1".to_string(), 3)],
        "the retraction bound must cover exactly the rows the scan does: \
         it retracted something other than {scanned:?}"
    );
    Ok(())
}

/// The sharper affinity case: a driver whose values do not take the column's
/// declared affinity.
///
/// `part` is declared `TEXT` and this driver hands back integers, so the two
/// sides of a scope genuinely hold different types — the source reports
/// `integer`, the mirror stores `text`, because inserting into the mirror
/// applies the declaration and reading a foreign row does not. A scope over
/// `part` must still select the same rows on both sides, which it does because
/// the comparison takes its affinity from the declaration rather than from the
/// value it finds.
#[turso_macros::test(views)]
fn test_a_scope_selects_alike_when_the_driver_ignores_the_declared_affinity(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let int_part = |uuid: &str, part: i64, val: i64| {
        vec![
            Value::build_text(uuid.to_string()),
            Value::Numeric(Numeric::Integer(part)),
            Value::Numeric(Numeric::Integer(val)),
        ]
    };
    let rows = setup(
        &tmp_db,
        &conn,
        "msg_typed",
        "mv_typed",
        vec![
            int_part("a1", 1, 1),
            int_part("a2", 1, 2),
            int_part("b1", 2, 3),
        ],
    )?;
    let mirror = "__turso_internal_fdw_mirror_v1_mv_typed__msg_typed";

    // The premise: the two sides really do hold different types, so their
    // agreeing below is not vacuous.
    let source_types: Vec<(String,)> =
        conn.exec_rows("SELECT DISTINCT typeof(part) FROM msg_typed");
    let mirror_types: Vec<(String,)> =
        conn.exec_rows(&format!("SELECT DISTINCT typeof(part) FROM \"{mirror}\""));
    assert_eq!(source_types, vec![("integer".to_string(),)]);
    assert_eq!(mirror_types, vec![("text".to_string(),)]);

    for predicate in ["part = 1", "part = '1'"] {
        let scan: Vec<(String,)> = conn.exec_rows(&format!(
            "SELECT uuid FROM msg_typed WHERE {predicate} ORDER BY uuid"
        ));
        let bound: Vec<(String,)> = conn.exec_rows(&format!(
            "SELECT uuid FROM \"{mirror}\" WHERE {predicate} ORDER BY uuid"
        ));
        assert_eq!(
            scan,
            vec![("a1".to_string(),), ("a2".to_string(),)],
            "{predicate}: the scan side"
        );
        assert_eq!(
            bound, scan,
            "{predicate}: the retraction bound covers rows the scan did not"
        );
    }

    // End to end: the row the scoped scan stopped returning goes, the one its
    // scope never covered stays.
    rows.set(vec![int_part("a1", 1, 1)]);
    common::run_query(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_typed WHERE part = 1",
    )?;
    assert_eq!(
        view_rows(&conn, "mv_typed"),
        vec![("a1".to_string(), 1), ("b1".to_string(), 3)]
    );
    Ok(())
}

/// A scope that answers differently each time it runs is refused at the
/// statement rather than half-applied.
///
/// The scan evaluates the predicate against the source and the retraction bound
/// evaluates it again against the mirror. Nothing makes two evaluations of
/// `random()` agree, so the bound would retract rows the scan never spoke for.
#[turso_macros::test(views)]
fn test_a_non_deterministic_scope_is_refused(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    setup(&tmp_db, &conn, "msg_nd", "mv_nd", seed())?;

    for predicate in ["random() > 0", "val > changes()", "part < datetime('now')"] {
        let message = refusal(
            &tmp_db,
            &conn,
            &format!("REFRESH MATERIALIZED VIEW mv_nd WHERE {predicate}"),
        );
        assert!(
            message.contains("evaluate the scope separately"),
            "{predicate} must be refused: {message}"
        );
    }

    assert_eq!(
        view_rows(&conn, "mv_nd"),
        vec![
            ("a1".to_string(), 1),
            ("a2".to_string(), 2),
            ("b1".to_string(), 3)
        ],
        "a refused scope must not have moved the view"
    );

    // A deterministic scope over the same columns is unaffected.
    common::run_query(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_nd WHERE part = 'p1'",
    )?;
    Ok(())
}

/// `msg_key(uuid, part, val)` with `part` a key column the source can filter
/// on, recording every qualifier it is handed.
#[derive(Debug)]
struct RecordingFdw {
    key_columns: Vec<KeyColumn>,
    rows: Arc<Mutex<Vec<Vec<Value>>>>,
    seen: Arc<Mutex<Vec<Vec<(u32, Value)>>>>,
}

impl ForeignDataWrapper for RecordingFdw {
    fn key_columns(&self) -> &[KeyColumn] {
        &self.key_columns
    }

    fn identity_columns(&self) -> Option<&[u32]> {
        Some(&[0])
    }

    fn schema_sql(&self) -> String {
        "CREATE TABLE msg_key(uuid TEXT, part TEXT, val INTEGER)".to_string()
    }

    fn open_cursor(&self, _conn: Arc<Connection>) -> turso_core::Result<Box<dyn ForeignCursor>> {
        Ok(Box::new(RecordingCursor {
            rows: self.rows.lock().unwrap().clone(),
            seen: self.seen.clone(),
            index: 0,
        }))
    }
}

struct RecordingCursor {
    rows: Vec<Vec<Value>>,
    seen: Arc<Mutex<Vec<Vec<(u32, Value)>>>>,
    index: usize,
}

impl ForeignCursor for RecordingCursor {
    fn filter(&mut self, constraints: &[PushedConstraint]) -> turso_core::Result<bool> {
        self.seen.lock().unwrap().push(
            constraints
                .iter()
                .map(|c| (c.column_index, c.value.clone()))
                .collect(),
        );
        self.index = 0;
        Ok(!self.rows.is_empty())
    }

    fn next(&mut self) -> turso_core::Result<bool> {
        self.index += 1;
        Ok(self.index < self.rows.len())
    }

    fn column(&self, idx: usize) -> turso_core::Result<Value> {
        Ok(self.rows[self.index]
            .get(idx)
            .cloned()
            .unwrap_or(Value::Null))
    }

    fn rowid(&self) -> i64 {
        self.index as i64
    }
}

/// The scope is what the driver is asked for, not only what the engine filters
/// afterwards: a source that can serve `part = 'p1'` cheaply must be told that
/// is all this refresh wants. Without the qualifier reaching it, a scoped
/// refresh costs a full scan and the primitive buys nothing.
#[turso_macros::test(views)]
fn test_the_scope_is_pushed_down_to_the_driver(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let rows = Arc::new(Mutex::new(seed()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    conn.register_foreign_table(
        "msg_key",
        Arc::new(RecordingFdw {
            key_columns: vec![KeyColumn::new("part", 1, vec![ConstraintOp::Eq])],
            rows: rows.clone(),
            seen: seen.clone(),
        }),
    )?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_key AS SELECT uuid, val FROM msg_key",
    )?;

    seen.lock().unwrap().clear();
    common::run_query(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_key WHERE part = 'p1'",
    )?;

    let seen = seen.lock().unwrap().clone();
    assert!(!seen.is_empty(), "the sweep must have scanned the source");
    for qualifiers in &seen {
        assert_eq!(
            qualifiers.len(),
            1,
            "every scan of a scoped sweep carries the scope: {seen:?}"
        );
        assert_eq!(qualifiers[0].0, 1, "the qualifier is on `part`: {seen:?}");
        assert_eq!(
            qualifiers[0].1.to_string(),
            "p1",
            "the qualifier carries the scope's value: {seen:?}"
        );
    }
    Ok(())
}

/// A view with no mirror is refreshed by rebuilding it from scratch, which has
/// no notion of a partial scan to bound. Refuse rather than accept a scope and
/// ignore it.
#[turso_macros::test(views)]
fn test_a_scope_on_a_view_with_no_mirror_is_refused(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE TABLE local_t(id INTEGER, val INTEGER)",
    )?;
    common::run_query(&tmp_db, &conn, "INSERT INTO local_t VALUES (1, 1)")?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_local AS SELECT id, val FROM local_t",
    )?;

    let message = refusal(
        &tmp_db,
        &conn,
        "REFRESH MATERIALIZED VIEW mv_local WHERE val > 0",
    );
    assert!(
        message.contains("mv_local") && message.contains("mirror"),
        "the refusal must say the view has no mirrored source: {message}"
    );

    // Unscoped, the same view refreshes as it always did.
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_local")?;
    let rows: Vec<(i64, i64)> = conn.exec_rows("SELECT id, val FROM mv_local");
    assert_eq!(rows, vec![(1, 1)]);
    Ok(())
}
