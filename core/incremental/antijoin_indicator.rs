//! `EmitMode::Indicator`: the antijoin operator as an EXISTS indicator.
//!
//! NullPad emits a row only while a left row is unmatched. Indicator emits one for
//! every left row, carrying whether the key matches, so a flip is a retraction of the
//! old row plus an insertion of the new one. The delta must therefore stay balanced
//! across the flip in both directions — matched→unmatched is the direction the LEFT
//! JOIN path exercises, unmatched→matched is the one it never does.

use crate::incremental::dbsp::{Delta, DeltaPair};
use crate::incremental::operator::{
    create_dbsp_state_index, AntijoinOperator, DbspStateCursors, EmitMode, IncrementalOperator,
};
use crate::storage::btree::BTreeCursor;
use crate::storage::pager::CreateBTreeFlags;
use crate::sync::Arc;
use crate::util::IOExt;
use crate::{Database, MemoryIO, Pager, SqliteDialect, Value, IO};

fn harness() -> (Arc<Pager>, DbspStateCursors) {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap();
    let conn = db.connect().unwrap();
    let pager = conn.pager.load().clone();
    let _ = pager.io.block(|| pager.allocate_page1());
    let table_root = pager
        .io
        .block(|| pager.btree_create(&CreateBTreeFlags::new_table()))
        .unwrap() as i64;
    let index_root = pager
        .io
        .block(|| pager.btree_create(&CreateBTreeFlags::new_index()))
        .unwrap() as i64;
    let table_cursor = BTreeCursor::new_table(pager.clone(), table_root, 5);
    let index_def = create_dbsp_state_index(index_root);
    let index_cursor = BTreeCursor::new_index(pager.clone(), index_root, &index_def, 3).unwrap();
    (
        pager.clone(),
        DbspStateCursors::new(table_cursor, index_cursor),
    )
}

fn indicator_operator() -> AntijoinOperator {
    AntijoinOperator::new(1, vec![0], vec![0], EmitMode::Indicator)
}

/// One commit of `(left, right)`, returning the output delta as `(values, weight)`
/// pairs sorted for comparison.
fn commit(
    op: &mut AntijoinOperator,
    pager: &Arc<Pager>,
    cursors: &mut DbspStateCursors,
    left: Delta,
    right: Delta,
) -> Vec<(Vec<Value>, isize)> {
    let out = pager
        .io
        .block(|| op.commit(DeltaPair::new(left.clone(), right.clone()), cursors))
        .unwrap();
    let mut rows: Vec<(Vec<Value>, isize)> = out
        .changes
        .into_iter()
        .map(|(row, w)| (row.values.to_vec(), w))
        .collect();
    rows.sort_by_key(|(v, w)| (format!("{v:?}"), *w));
    rows
}

fn l_row(key: i64) -> Delta {
    let mut d = Delta::new();
    d.insert(key, vec![Value::from_i64(key)]);
    d
}

fn r_row(key: i64) -> Delta {
    let mut d = Delta::new();
    d.insert(key + 1000, vec![Value::from_i64(key)]);
    d
}

fn r_row_deleted(key: i64) -> Delta {
    let mut d = Delta::new();
    d.delete(key + 1000, vec![Value::from_i64(key)]);
    d
}

fn tagged(key: i64, matched: bool) -> (Vec<Value>, isize) {
    (
        vec![Value::from_i64(key), Value::from_i64(matched as i64)],
        1,
    )
}

fn tagged_w(key: i64, matched: bool, weight: isize) -> (Vec<Value>, isize) {
    (
        vec![Value::from_i64(key), Value::from_i64(matched as i64)],
        weight,
    )
}

#[test]
fn left_row_with_no_match_is_emitted_unmatched() {
    let (pager, mut cursors) = harness();
    let mut op = indicator_operator();

    let out = commit(&mut op, &pager, &mut cursors, l_row(1), Delta::new());

    assert_eq!(out, vec![tagged(1, false)]);
}

#[test]
fn left_row_with_a_match_is_emitted_matched() {
    let (pager, mut cursors) = harness();
    let mut op = indicator_operator();

    let out = commit(&mut op, &pager, &mut cursors, l_row(1), r_row(1));

    assert_eq!(out, vec![tagged(1, true)]);
}

#[test]
fn a_key_becoming_matched_retracts_the_unmatched_row() {
    let (pager, mut cursors) = harness();
    let mut op = indicator_operator();
    commit(&mut op, &pager, &mut cursors, l_row(1), Delta::new());

    let out = commit(&mut op, &pager, &mut cursors, Delta::new(), r_row(1));

    assert_eq!(
        out,
        vec![tagged_w(1, false, -1), tagged_w(1, true, 1)],
        "a flip must retract the old row and insert the new one"
    );
}

#[test]
fn a_key_becoming_unmatched_retracts_the_matched_row() {
    let (pager, mut cursors) = harness();
    let mut op = indicator_operator();
    commit(&mut op, &pager, &mut cursors, l_row(1), r_row(1));

    let out = commit(
        &mut op,
        &pager,
        &mut cursors,
        Delta::new(),
        r_row_deleted(1),
    );

    assert_eq!(out, vec![tagged_w(1, false, 1), tagged_w(1, true, -1)]);
}

/// Only the 0↔1 crossings change the output; a second matching right row leaves
/// every left row's indicator alone.
#[test]
fn a_second_match_emits_nothing() {
    let (pager, mut cursors) = harness();
    let mut op = indicator_operator();
    commit(&mut op, &pager, &mut cursors, l_row(1), r_row(1));

    let mut second = Delta::new();
    second.insert(2001, vec![Value::from_i64(1)]);
    let out = commit(&mut op, &pager, &mut cursors, Delta::new(), second);

    assert_eq!(out, vec![]);
}

#[test]
fn a_null_correlation_key_never_matches() {
    let (pager, mut cursors) = harness();
    let mut op = indicator_operator();
    let mut left = Delta::new();
    left.insert(1, vec![Value::Null]);
    let mut right = Delta::new();
    right.insert(1001, vec![Value::Null]);

    let out = commit(&mut op, &pager, &mut cursors, left, right);

    assert_eq!(
        out,
        vec![(vec![Value::Null, Value::from_i64(0)], 1)],
        "SQL says NULL = NULL is unknown, so EXISTS is false"
    );
}

/// A left and a right change arriving at the same key in one delta: the left row must
/// see the post-state of the counter, not the pre-state.
#[test]
fn a_left_and_right_change_at_one_key_in_one_delta() {
    let (pager, mut cursors) = harness();
    let mut op = indicator_operator();

    let out = commit(&mut op, &pager, &mut cursors, l_row(7), r_row(7));

    assert_eq!(out, vec![tagged(7, true)]);
}

#[test]
fn removing_a_left_row_retracts_its_indicator() {
    let (pager, mut cursors) = harness();
    let mut op = indicator_operator();
    commit(&mut op, &pager, &mut cursors, l_row(1), r_row(1));

    let mut gone = Delta::new();
    gone.delete(1, vec![Value::from_i64(1)]);
    let out = commit(&mut op, &pager, &mut cursors, gone, Delta::new());

    assert_eq!(out, vec![tagged_w(1, true, -1)]);
}

/// Fires one mid-balance yield inside the DBSP state btree, so the operator must
/// resume its persistence loop from the state it recorded before the write.
#[derive(Debug)]
struct OneShotBalanceYield {
    selection_key: u64,
    fired: std::sync::atomic::AtomicBool,
}

impl crate::mvcc::yield_points::YieldInjector for OneShotBalanceYield {
    fn should_yield(
        &self,
        _instance_id: u64,
        selection_key: u64,
        point: crate::mvcc::yield_points::YieldPoint,
    ) -> bool {
        use crate::mvcc::yield_hooks::YieldPointMarker;
        point
            == crate::storage::btree::BTreeWriteYieldPoint::AfterInsertOverflowCellBeforeBalance
                .point()
            && (selection_key == self.selection_key || self.selection_key == 0)
            && !self.fired.swap(true, std::sync::atomic::Ordering::AcqRel)
    }
}

/// Enough left rows that persisting L_INDEX fills a leaf and the next insert balances.
const BALANCE_ROWS: i64 = 400;

fn flip_sequence(op: &mut AntijoinOperator, pager: &Arc<Pager>, cursors: &mut DbspStateCursors) {
    let mut left = Delta::new();
    for k in 0..BALANCE_ROWS {
        left.insert(
            k,
            vec![Value::from_i64(k), Value::from_text("x".repeat(600))],
        );
    }
    commit(op, pager, cursors, left, Delta::new());
}

#[test]
fn a_mid_balance_yield_does_not_change_the_emitted_delta() {
    let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
    let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect)).unwrap();
    let conn = db.connect().unwrap();
    let pager = conn.pager.load().clone();
    let _ = pager.io.block(|| pager.allocate_page1());
    let table_root = pager
        .io
        .block(|| pager.btree_create(&CreateBTreeFlags::new_table()))
        .unwrap() as i64;
    let index_root = pager
        .io
        .block(|| pager.btree_create(&CreateBTreeFlags::new_index()))
        .unwrap() as i64;
    let mut table_cursor = BTreeCursor::new_table(pager.clone(), table_root, 5);
    let index_def = create_dbsp_state_index(index_root);
    let mut index_cursor =
        BTreeCursor::new_index(pager.clone(), index_root, &index_def, 3).unwrap();
    // The cursor snapshots the pager's injector when the context is installed, so the
    // injector must be on the connection first.
    let injector = Arc::new(OneShotBalanceYield {
        selection_key: 0,
        fired: std::sync::atomic::AtomicBool::new(false),
    });
    conn.set_yield_injector(Some(injector.clone()));
    crate::incremental::operator::install_dbsp_yield_context(&mut table_cursor, &pager);
    crate::incremental::operator::install_dbsp_yield_context(&mut index_cursor, &pager);
    let mut cursors = DbspStateCursors::new(table_cursor, index_cursor);

    let mut op = indicator_operator();
    flip_sequence(&mut op, &pager, &mut cursors);
    // Every key gains its first match: each left row must retract unmatched and insert matched.
    let mut right = Delta::new();
    for k in 0..BALANCE_ROWS {
        right.insert(k + 100_000, vec![Value::from_i64(k)]);
    }
    let out = commit(&mut op, &pager, &mut cursors, Delta::new(), right);

    conn.set_yield_injector(None);
    assert!(
        injector.fired.load(std::sync::atomic::Ordering::Acquire),
        "no mid-balance yield was injected; the test does not exercise re-entry"
    );

    let (_, mut cursors_ref) = harness();
    let mut op_ref = indicator_operator();
    flip_sequence(&mut op_ref, &pager, &mut cursors_ref);
    let mut right_ref = Delta::new();
    for k in 0..BALANCE_ROWS {
        right_ref.insert(k + 100_000, vec![Value::from_i64(k)]);
    }
    let expected = commit(
        &mut op_ref,
        &pager,
        &mut cursors_ref,
        Delta::new(),
        right_ref,
    );

    assert_eq!(
        out, expected,
        "a mid-balance yield changed the output delta"
    );
}
