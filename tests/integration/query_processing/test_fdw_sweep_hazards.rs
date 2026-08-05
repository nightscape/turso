//! The sweep against this subsystem's historical failure shapes.
//!
//! Reading a mirror puts foreign rows through the ordinary IVM machinery, which
//! means it inherits that machinery's known hazards rather than escaping them.
//! Each case here is one of them, driven from a foreign source: a DBSP state
//! index that outgrows a single leaf, a driver that repeats an identity, and an
//! input delta carrying an insert, an update and a delete at once.

use crate::common::{self, ExecRows, TempDatabase};
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
/// OPEN — the sweep does not refuse it. Its `ON CONFLICT … DO UPDATE` treats
/// the second row as an update of the first, so duplicates collapse in
/// silence, while the same source at CREATE is refused outright. Detecting it
/// needs the scan's row count against its distinct-identity count, and
/// `test_scan_named_once_and_read_twice` measures that a scan read twice is
/// scanned twice — so the check costs a third scan of the foreign source per
/// REFRESH. That is a cost/diagnosability trade to rule on, not a bug to
/// quietly fix.
#[turso_macros::test(views)]
#[ignore = "OPEN: sweep collapses duplicate identities silently; refusing costs a third foreign scan"]
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
