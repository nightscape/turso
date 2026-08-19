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
