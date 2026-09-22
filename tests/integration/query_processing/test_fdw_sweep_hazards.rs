//! The sweep against this subsystem's historical failure shapes.
//!
//! Reading a mirror puts foreign rows through the ordinary IVM machinery, which
//! means it inherits that machinery's known hazards rather than escaping them.
//! Each case here is one of them, driven from a foreign source: a DBSP state
//! index that outgrows a single leaf, a driver that repeats an identity, and an
//! input delta carrying an insert, an update and a delete at once.

use crate::common::{self, ExecRows, TempDatabase};
use crate::query_processing::fdw_test_driver::MemFdw;
use std::sync::{Arc, Mutex};
use turso_core::foreign::{ForeignCursor, ForeignDataWrapper, KeyColumn, PushedConstraint};
use turso_core::{Connection, Numeric, Value};

/// A foreign row: identity, group, and a value to aggregate.
type Row = (String, String, i64);

/// `(uuid TEXT, grp TEXT, val INTEGER)` over caller-controlled rows, with
/// `uuid` declared as the identity.
#[derive(Debug)]
struct RowsFdw {
    table: String,
    rows: Arc<Mutex<Vec<Row>>>,
}

impl ForeignDataWrapper for RowsFdw {
    fn key_columns(&self) -> &[KeyColumn] {
        &[]
    }

    fn identity_columns(&self) -> Option<&[u32]> {
        Some(&[0])
    }

    fn schema_sql(&self) -> String {
        format!(
            "CREATE TABLE {}(uuid TEXT, grp TEXT, val INTEGER)",
            self.table
        )
    }

    fn open_cursor(&self, _conn: Arc<Connection>) -> turso_core::Result<Box<dyn ForeignCursor>> {
        Ok(Box::new(RowsCursor {
            source: self.rows.clone(),
            rows: Vec::new(),
            index: 0,
        }))
    }
}

struct RowsCursor {
    source: Arc<Mutex<Vec<Row>>>,
    rows: Vec<Row>,
    index: usize,
}

impl ForeignCursor for RowsCursor {
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
            0 => Value::build_text(row.0.clone()),
            1 => Value::build_text(row.1.clone()),
            2 => Value::Numeric(Numeric::Integer(row.2)),
            _ => Value::Null,
        })
    }

    fn rowid(&self) -> i64 {
        self.index as i64
    }
}

/// Register `table` on `conn` over `rows` and hand back the row store.
fn register(conn: &Arc<Connection>, table: &str, rows: Vec<Row>) -> Arc<Mutex<Vec<Row>>> {
    let store = Arc::new(Mutex::new(rows));
    conn.register_foreign_table(
        table,
        Arc::new(RowsFdw {
            table: table.to_string(),
            rows: store.clone(),
        }),
    )
    .unwrap();
    store
}

fn row(uuid: &str, grp: &str, val: i64) -> Row {
    (uuid.to_string(), grp.to_string(), val)
}

/// The DBSP state index must exceed one leaf for the seek at its leaf boundary
/// to be exercised at all, so this asserts the page size the count was chosen
/// against rather than assuming it.
fn assert_page_size(conn: &Arc<Connection>) {
    let rows: Vec<(i64,)> = conn.exec_rows("PRAGMA page_size");
    assert_eq!(
        rows[0].0, 4096,
        "the group count below is calibrated to a 4096-byte page"
    );
}

/// An aggregate over a mirrored source with enough groups that the DBSP state
/// index spans several leaves, then a sweep that changes exactly one group.
///
/// This is the shape of the historical first-UPDATE undercount: a seek for a
/// group's existing state landing on a leaf boundary reported "not found", so
/// the group's prior state was discarded and rebuilt from the new row alone.
/// It only ever showed up on a group's first UPDATE, and only past the leaf
/// boundary, which is why the group count is far above it.
#[turso_macros::test(views)]
fn test_sweep_updates_one_group_exactly_past_the_leaf_boundary(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    assert_page_size(&conn);

    const GROUPS: usize = 120;
    let initial: Vec<Row> = (0..GROUPS)
        .flat_map(|g| {
            [
                row(&format!("m{g:04}a"), &format!("g{g:04}"), 10),
                row(&format!("m{g:04}b"), &format!("g{g:04}"), 20),
            ]
        })
        .collect();
    let store = register(&conn, "msg_grp", initial.clone());

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_groups AS \
         SELECT grp, count(*) AS n, sum(val) AS s FROM msg_grp GROUP BY grp",
    )?;
    let totals: Vec<(i64, i64)> =
        conn.exec_rows("SELECT count(*), cast(sum(n) AS INTEGER) FROM mv_groups");
    assert_eq!(
        totals[0],
        (GROUPS as i64, 2 * GROUPS as i64),
        "population must see every group exactly once"
    );

    // One row of one group changes value: the group's FIRST update, which is
    // the only occasion the boundary bug ever fired.
    let target = "g0100";
    {
        let mut rows = store.lock().unwrap();
        let entry = rows
            .iter_mut()
            .find(|r| r.0 == "m0100a")
            .expect("target row");
        entry.2 = 70;
    }
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_groups")?;

    let changed: Vec<(i64, i64)> = conn.exec_rows(&format!(
        "SELECT n, cast(s AS INTEGER) FROM mv_groups WHERE grp = '{target}'"
    ));
    assert_eq!(
        changed,
        vec![(2, 90)],
        "the updated group must keep both its rows and sum the new value"
    );

    // Nothing else may have moved, and no group may have lost its prior state.
    let totals: Vec<(i64, i64, i64)> = conn.exec_rows(
        "SELECT count(*), cast(sum(n) AS INTEGER), cast(sum(s) AS INTEGER) FROM mv_groups",
    );
    assert_eq!(
        totals[0],
        (
            GROUPS as i64,
            2 * GROUPS as i64,
            30 * GROUPS as i64 + 60 // every group sums 30; the target gained 50
        ),
        "a single group's update disturbed the rest of the state index"
    );
    Ok(())
}

/// A driver that repeats an identity is lying about what makes its rows
/// distinct. The mirror's primary key catches it, and the error must say which
/// foreign table and which columns, because the table the constraint actually
/// fired on is an internal one the user never wrote.
#[turso_macros::test(views)]
fn test_duplicate_identity_is_refused_at_create(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    register(
        &conn,
        "msg_dup",
        vec![row("d1", "g1", 1), row("d1", "g1", 2)],
    );

    let err = common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_dup AS SELECT uuid, val FROM msg_dup",
    )
    .expect_err("a source repeating an identity must be refused, not silently collapsed");

    let message = err.to_string();
    assert!(
        message.contains("msg_dup") && message.contains("uuid"),
        "the error must name the foreign table and its identity columns: {message}"
    );
    assert!(
        !message.contains("__turso_internal"),
        "naming the internal mirror is not a diagnosis the user can act on: {message}"
    );
    Ok(())
}

/// The same lie told later: the source is fine at CREATE and repeats an
/// identity only at REFRESH.
///
/// CREATE and REFRESH answer this alike, which costs the sweep a third scan of
/// the source: the check needs the scan's row count against its
/// distinct-identity count, and `test_scan_named_once_and_read_twice` shows a
/// scan named once and read twice is scanned twice, so it cannot ride along on
/// the two the sweep already pays.
#[turso_macros::test(views)]
fn test_duplicate_identity_is_refused_at_refresh(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let store = register(&conn, "msg_dup2", vec![row("d1", "g1", 1)]);
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_dup2 AS SELECT uuid, val FROM msg_dup2",
    )?;

    *store.lock().unwrap() = vec![row("d1", "g1", 1), row("d1", "g1", 2)];
    let err = common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_dup2")
        .expect_err("a sweep over a source repeating an identity must be refused");

    let message = err.to_string();
    assert!(
        message.contains("msg_dup2") && message.contains("uuid"),
        "the error must name the foreign table and its identity columns: {message}"
    );
    assert!(
        !message.contains("__turso_internal"),
        "naming the internal mirror is not a diagnosis the user can act on: {message}"
    );

    // And the refusal must not have half-applied: the view still holds what the
    // last good sweep left.
    let rows: Vec<(String, i64)> = conn.exec_rows("SELECT uuid, val FROM mv_dup2");
    assert_eq!(rows, vec![("d1".to_string(), 1)]);
    Ok(())
}

/// A composite identity repeated at REFRESH must be refused too.
///
/// SQLite has no `count(DISTINCT a, b)`, so the check cannot be written the
/// obvious way for more than one column, and a form that works for one is no
/// evidence it works for two.
#[turso_macros::test(views)]
fn test_composite_duplicate_identity_is_refused_at_refresh(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let (fdw, rows) = MemFdw::new(
        "CREATE TABLE msg_pair(session_id TEXT, uuid TEXT, body TEXT)",
        vec![0, 1],
    );
    rows.set(vec![
        pair_row("s1", "u1", "one"),
        pair_row("s2", "u1", "two"),
    ]);
    conn.register_foreign_table("msg_pair", fdw)?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_pair AS SELECT session_id, uuid, body FROM msg_pair",
    )?;

    // Neither column repeats on its own; only the pair does.
    rows.set(vec![
        pair_row("s1", "u1", "one"),
        pair_row("s2", "u1", "two"),
        pair_row("s1", "u1", "again"),
    ]);
    let err = common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_pair")
        .expect_err("a repeated composite identity must be refused");

    let message = err.to_string();
    assert!(
        message.contains("msg_pair") && message.contains("session_id") && message.contains("uuid"),
        "the error must name the foreign table and both identity columns: {message}"
    );
    assert!(
        message.contains("more than one row"),
        "a repeated composite identity is a duplicate: {message}"
    );
    Ok(())
}

fn pair_row(session: &str, uuid: &str, body: &str) -> Vec<Value> {
    vec![
        Value::build_text(session.to_string()),
        Value::build_text(uuid.to_string()),
        Value::build_text(body.to_string()),
    ]
}

/// A refused REFRESH must have written nothing: the check runs before the
/// sweep's first statement, so the mirror keeps not just its contents but the
/// rowids those contents sit on — the identities every later retraction is
/// carried by. And the refusal is not terminal: the same view syncs normally
/// once the source stops lying.
#[turso_macros::test(views)]
fn test_refused_refresh_leaves_the_mirror_and_the_view_untouched(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let store = register(
        &conn,
        "msg_intact",
        vec![row("i1", "g1", 1), row("i2", "g1", 2)],
    );
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_intact AS SELECT uuid, val FROM msg_intact",
    )?;

    let mirror = "__turso_internal_fdw_mirror_v1_mv_intact__msg_intact";
    let mirror_state = |conn: &Arc<Connection>| -> Vec<(i64, String, i64)> {
        conn.exec_rows(&format!(
            "SELECT rowid, uuid, val FROM \"{mirror}\" ORDER BY uuid"
        ))
    };
    let view_state = |conn: &Arc<Connection>| -> Vec<(String, i64)> {
        conn.exec_rows("SELECT uuid, val FROM mv_intact ORDER BY uuid")
    };

    let mirror_before = mirror_state(&conn);
    let view_before = view_state(&conn);
    assert_eq!(view_before.len(), 2, "{view_before:?}");

    // A duplicate plus a genuine change, so a sweep that got as far as its
    // upsert would leave visible damage.
    *store.lock().unwrap() = vec![
        row("i1", "g1", 100),
        row("i2", "g1", 2),
        row("i1", "g1", 200),
    ];
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_intact")
        .expect_err("a sweep over a source repeating an identity must be refused");

    assert_eq!(
        mirror_state(&conn),
        mirror_before,
        "a refused sweep must not have written the mirror, rowids included"
    );
    assert_eq!(
        view_state(&conn),
        view_before,
        "a refused sweep must not have moved the view"
    );

    // The corrected source sweeps normally, so the refusal cost nothing but the
    // scan.
    *store.lock().unwrap() = vec![row("i1", "g1", 100), row("i2", "g1", 2)];
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_intact")?;
    assert_eq!(
        view_state(&conn),
        vec![("i1".to_string(), 100), ("i2".to_string(), 2)]
    );
    Ok(())
}

/// One sweep carrying an insert, an update and a delete at once, through the
/// operator stack where unconsolidated input deltas have historically gone
/// wrong: an aggregate over a LEFT JOIN with a FILTER, where a retraction that
/// arrives unpaired drives a multiset count negative and the group either
/// vanishes or double-counts.
#[turso_macros::test(views)]
fn test_sweep_mixes_insert_update_and_delete_in_one_pass(
    tmp_db: TempDatabase,
) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let store = register(
        &conn,
        "msg_mix",
        vec![
            row("m1", "g1", 5),
            row("m2", "g1", 15),
            row("m3", "g2", 25),
            // g3 has no label: the unmatched side of the LEFT JOIN.
            row("m4", "g3", 35),
        ],
    );
    common::run_query(&tmp_db, &conn, "CREATE TABLE labels (grp TEXT, label TEXT)")?;
    common::run_query(
        &tmp_db,
        &conn,
        "INSERT INTO labels VALUES ('g1', 'first'), ('g2', 'second')",
    )?;
    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_mix AS \
         SELECT m.grp, l.label, count(*) AS n, \
                sum(m.val) FILTER (WHERE m.val > 10) AS big \
         FROM msg_mix m LEFT JOIN labels l ON m.grp = l.grp \
         GROUP BY m.grp, l.label",
    )?;

    let before: Vec<(String, String, i64, i64)> = conn.exec_rows(
        "SELECT grp, coalesce(label, '-'), n, cast(big AS INTEGER) FROM mv_mix ORDER BY grp",
    );
    assert_eq!(
        before,
        vec![
            ("g1".to_string(), "first".to_string(), 2, 15),
            ("g2".to_string(), "second".to_string(), 1, 25),
            ("g3".to_string(), "-".to_string(), 1, 35),
        ]
    );

    // m1 updated (crosses the FILTER threshold), m3 deleted (empties g2),
    // m5 inserted into the unmatched group — all in one sweep.
    *store.lock().unwrap() = vec![
        row("m1", "g1", 50),
        row("m2", "g1", 15),
        row("m4", "g3", 35),
        row("m5", "g3", 1),
    ];
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_mix")?;

    let after: Vec<(String, String, i64, i64)> = conn.exec_rows(
        "SELECT grp, coalesce(label, '-'), n, cast(big AS INTEGER) FROM mv_mix ORDER BY grp",
    );
    assert_eq!(
        after,
        vec![
            ("g1".to_string(), "first".to_string(), 2, 65),
            ("g3".to_string(), "-".to_string(), 2, 35),
        ],
        "the emptied group must be gone and no count may be stale"
    );
    assert!(
        after.iter().all(|r| r.2 >= 0 && r.3 >= 0),
        "a negative multiset count leaked into the view: {after:?}"
    );
    Ok(())
}

/// A view over two mirrored sources, refused because of the second one.
///
/// One `REFRESH` sweeps both mirrors, so the guard for the source that lies is
/// not the first thing the statement does — the other source's sweep can have
/// run already. What this pins is that it did not land: the refusal is
/// all-or-nothing across every mirror the view has, not per-mirror. A per-mirror
/// refusal would leave the honest source's change durably applied against a view
/// that never saw it, and no later `REFRESH` would put that back, because the
/// mirror already agrees with the source.
#[turso_macros::test(views)]
fn test_a_refusal_in_one_mirror_rolls_back_the_other(tmp_db: TempDatabase) -> anyhow::Result<()> {
    let conn = tmp_db.connect_limbo();
    let left = register(
        &conn,
        "msg_left",
        vec![row("l1", "g1", 1), row("l2", "g2", 2)],
    );
    let right = register(&conn, "msg_right", vec![row("r1", "g1", 10)]);

    common::run_query(
        &tmp_db,
        &conn,
        "CREATE MATERIALIZED VIEW mv_two AS \
         SELECT l.uuid AS lu, r.uuid AS ru, l.val AS lv, r.val AS rv \
         FROM msg_left l JOIN msg_right r ON l.grp = r.grp",
    )?;

    let mirror_of = |table: &str| format!("__turso_internal_fdw_mirror_v1_mv_two__{table}");
    let mirror_state = |conn: &Arc<Connection>, table: &str| -> Vec<(i64, String, String, i64)> {
        conn.exec_rows(&format!(
            "SELECT rowid, uuid, grp, val FROM \"{}\" ORDER BY rowid",
            mirror_of(table)
        ))
    };
    let view_state = |conn: &Arc<Connection>| -> Vec<(String, String, i64, i64)> {
        conn.exec_rows("SELECT lu, ru, lv, rv FROM mv_two ORDER BY lu")
    };

    let left_before = mirror_state(&conn, "msg_left");
    let right_before = mirror_state(&conn, "msg_right");
    let view_before = view_state(&conn);
    assert_eq!(
        view_before,
        vec![("l1".to_string(), "r1".to_string(), 1, 10)]
    );

    // The honest source changes for real; the other repeats an identity.
    *left.lock().unwrap() = vec![row("l1", "g1", 100), row("l2", "g2", 2)];
    *right.lock().unwrap() = vec![row("r1", "g1", 10), row("r1", "g1", 999)];

    for attempt in 1..=2 {
        let err = common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_two")
            .expect_err("a source repeating an identity must be refused");
        let message = err.to_string();
        assert!(
            message.contains("msg_right") && message.contains("uuid"),
            "attempt {attempt}: the error must name the offending table: {message}"
        );
        assert!(
            !message.contains("msg_left"),
            "attempt {attempt}: the honest source is not the problem: {message}"
        );
        assert!(
            message.contains("more than one row"),
            "attempt {attempt}: {message}"
        );

        assert_eq!(
            mirror_state(&conn, "msg_left"),
            left_before,
            "attempt {attempt}: the honest source's change landed despite the refusal"
        );
        assert_eq!(
            mirror_state(&conn, "msg_right"),
            right_before,
            "attempt {attempt}: the refused mirror was written"
        );
        assert_eq!(
            view_state(&conn),
            view_before,
            "attempt {attempt}: a refused sweep moved the view"
        );
    }

    // With the lie removed, the change the refusal held back applies.
    *right.lock().unwrap() = vec![row("r1", "g1", 10)];
    common::run_query(&tmp_db, &conn, "REFRESH MATERIALIZED VIEW mv_two")?;
    assert_eq!(
        view_state(&conn),
        vec![("l1".to_string(), "r1".to_string(), 100, 10)],
        "the honest source's change must survive the refusals and apply on recovery"
    );
    Ok(())
}
