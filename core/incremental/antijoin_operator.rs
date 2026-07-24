//! AntijoinOperator — produces null-padded rows for LEFT JOIN.
//!
//! DBSP semantics: `L ⟕ R = (L ⋈ R) ⊎ NullPad(L ⋈ {k : count_R[k] == 0})`.
//! This operator maintains the antijoin half; the matched half is produced
//! by a sibling `JoinOperator` (`JoinType::Inner`) and the two are merged via
//! `MergeOperator` (`UnionMode::All`). The output is already null-padded —
//! `NullPad` is folded into this operator's projection.
//!
//! ## Delta formula (Algorithm 4.6 of the DBSP paper, applied to
//! `Antijoin = L ⋈ { k : count_R(k) == 0 }`):
//!
//! ```text
//! Δ(Antijoin)(l, k) =
//!     L_pre(l, k) · ([c_post(k)==0] − [c_pre(k)==0])    ── R-term
//!   + δL(l, k)    ·  [c_post(k)==0]                     ── L-term
//! ```
//!
//! The two terms are computed in a fixed order:
//!
//! 1. **R-term first.** While `L_INDEX` still reflects `L_pre` (we haven't
//!    yet persisted δL), iterate keys with non-zero `δR_count(k)`. For each
//!    key, read `c_pre`, compute `c_post = c_pre + δR_count`. If the
//!    indicator delta `([c_post==0] − [c_pre==0])` is non-zero, scan
//!    `L_INDEX` at that key and emit each L row with weight
//!    `L_pre(l,k) · indicator_delta`. Then persist updated `R_COUNT`.
//! 2. **L-term second.** Iterate δL. For each `(l, w)` at key `k`, read
//!    `c_post(k)` (= `c_pre + δR_count[k]`) and emit `w · [c_post==0]`
//!    null-padded. Then persist updated `L_INDEX`.
//!
//! This ordering partitions the formula cleanly: R-term sees only `L_pre`
//! (the persisted state during eval, before commit writes δL), L-term sees
//! only δL. The "MatchCounter Phase B/Phase C double-emit" bug class is
//! gone by construction.
//!
//! ## Storage
//!
//! Two btree-backed shards, identified by `column_index` in
//! `generate_storage_id(operator_id, column_index, JOIN_TYPE_BIT)`:
//!
//! - **`L_INDEX`** (`column_index = 0`): multiset of L rows, indexed by
//!   join key. Btree key `(storage_id_LI, l_join_key_hash, l_row_hash)`,
//!   value blob = serialized L row, weight = L multiplicity.
//! - **`R_COUNT`** (`column_index = 1`): per-join-key R multiplicity.
//!   Btree key `(storage_id_RC, l_join_key_hash, sentinel)`, weight =
//!   `count_R[k]` (uses `WriteRow`'s `weight += δ` semantics).
//!
//! Storage layout is identical to the predecessor `MatchCounterOperator`
//! so on-disk matviews built before the rewrite can be dropped and
//! rebuilt; there is no migration shim (single-user dev context).
//!
//! ## State machine (one `return_if_io!` per state)
//!
//! See MEMORY.md "IVM State Index Duplicate Entry Bug" for why this
//! discipline matters. Each btree operation yields IO at most once;
//! states with seek+mutate use a `sought: bool` flag to route re-entry to
//! the mutate phase.
//!
//! ## NULL join keys
//!
//! Rows whose join key contains NULL never match anything in R (SQL
//! standard); they enter `L_INDEX` only via the L-term path and emit a
//! null-padded row at every weight change. R rows with NULL keys do not
//! contribute to `R_COUNT`.

#![allow(dead_code)]

use std::collections::HashMap;

use crate::incremental::aggregate_operator::JOIN_TYPE_BIT;
use crate::incremental::dbsp::{Delta, DeltaPair, Hash128, HashableRow};
use crate::incremental::join_operator::read_next_join_row;
use crate::incremental::operator::{
    generate_storage_id, ComputationTracker, DbspStateCursors, EvalState, IncrementalOperator,
};
use crate::incremental::persistence::WriteRow;
use crate::numeric::Numeric;
use crate::storage::btree::CursorTrait;
use crate::sync::Arc;
use crate::sync::Mutex;
use crate::types::{IOResult, ImmutableRecord, SeekKey, SeekOp, SeekResult};
use crate::{return_and_restore_if_io, return_if_io, Result, Value};

/// AntijoinOperator state during eval.
#[derive(Debug, Default)]
pub enum AntijoinEvalState {
    #[default]
    Uninitialized,
    /// R-pass: walk keys with non-zero δR_count, scan L_INDEX (= L_pre)
    /// for each, emit `L_pre(l,k) · indicator_delta`.
    EmitRTransitions {
        deltas: DeltaPair,
        output: Delta,
        affected_keys: Vec<AffectedKey>,
        key_idx: usize,
        sub: Box<RKeyScan>,
    },
    /// L-pass: walk δL, emit `δL(l) · [c_post(k)==0]`.
    ProcessLDelta {
        deltas: DeltaPair,
        output: Delta,
        /// δR_count per key. `c_post(k) = c_pre(k) + δR_count[k]`.
        right_count_deltas: HashMap<Hash128, isize>,
        l_idx: usize,
        sub: Box<LRowResolve>,
    },
    Done {
        output: Delta,
    },
}

/// Aggregated information about one join key affected by δR.
#[derive(Debug, Clone)]
pub struct AffectedKey {
    pub join_key: HashableRow,
    pub delta_count: isize,
    pub c_pre: Option<isize>,
}

/// Sub-state for the R-pass. Each variant has at most ONE `return_if_io!`.
#[derive(Debug, Default)]
pub enum RKeyScan {
    #[default]
    Idle,
    ReadingRCount {
        sought: bool,
    },
    /// We know the indicator_delta sign; scan L_INDEX at this key and
    /// emit one null-pad row per L row found, weighted by
    /// `L_INDEX_weight · indicator_delta`.
    ScanningL {
        indicator_delta: isize,
        last_l_hash: Option<Hash128>,
    },
    Done,
}

/// Sub-state for the L-pass. Only one btree read per δL entry: c_pre.
#[derive(Debug, Default)]
pub enum LRowResolve {
    #[default]
    Idle,
    ReadingRCount {
        sought: bool,
    },
    Done,
}

/// AntijoinOperator — produces the null-padded "unmatched" half of a
/// LEFT JOIN's output delta. Compiler wires it in parallel with an
/// inner `JoinOperator` under a UNION-ALL `MergeOperator`.
#[derive(Debug)]
pub struct AntijoinOperator {
    operator_id: i64,
    left_key_indices: Vec<usize>,
    right_key_indices: Vec<usize>,
    /// Number of right-side columns — needed to construct NULL padding.
    right_column_count: usize,
    tracker: Option<Arc<Mutex<ComputationTracker>>>,
    commit_state: AntijoinCommitState,
}

#[derive(Debug, Default)]
enum AntijoinCommitState {
    #[default]
    Idle,
    Eval {
        eval_state: EvalState,
    },
    /// Persist R_COUNT updates first, then L_INDEX. Either order is
    /// correct at commit boundaries (eval already produced `output`
    /// against the pre-image state), but R-first matches the eval
    /// ordering invariant for symmetry.
    PersistRCount {
        deltas: DeltaPair,
        output: Delta,
        key_deltas: Vec<(HashableRow, isize)>,
        idx: usize,
        write_row: WriteRow,
    },
    PersistLIndex {
        deltas: DeltaPair,
        output: Delta,
        idx: usize,
        write_row: WriteRow,
    },
    /// Sentinel value used to recover from prior panics during commit.
    Invalid,
}

impl AntijoinOperator {
    pub fn new(
        operator_id: i64,
        left_key_indices: Vec<usize>,
        right_key_indices: Vec<usize>,
        right_column_count: usize,
    ) -> Self {
        Self {
            operator_id,
            left_key_indices,
            right_key_indices,
            right_column_count,
            tracker: None,
            commit_state: AntijoinCommitState::Idle,
        }
    }

    fn l_index_storage_id(&self) -> i64 {
        generate_storage_id(self.operator_id, 0, JOIN_TYPE_BIT)
    }

    fn r_count_storage_id(&self) -> i64 {
        generate_storage_id(self.operator_id, 1, JOIN_TYPE_BIT)
    }

    fn extract_key(&self, values: &[Value], indices: &[usize]) -> HashableRow {
        let key_values: Vec<Value> = indices
            .iter()
            .map(|&idx| values.get(idx).cloned().unwrap_or(Value::Null))
            .collect();
        HashableRow::new(0, key_values)
    }

    /// Returns true if any element of the join key is NULL.
    fn key_has_null(key: &HashableRow) -> bool {
        key.values.iter().any(|v| matches!(v, Value::Null))
    }

    fn null_pad(&self, l_row: &HashableRow) -> HashableRow {
        let mut values = l_row.values.to_vec();
        values.extend(std::iter::repeat_n(Value::Null, self.right_column_count));
        let values: super::dbsp::RowValues = values.into();
        let temp = HashableRow::new(0, values.clone());
        HashableRow::new(temp.cached_hash().as_i64(), values)
    }

    /// Build δR_count[key_hash] from input δR. NULL-keyed R rows are
    /// excluded (they don't match anything in L either).
    fn compute_r_count_deltas(&self, right_delta: &Delta) -> HashMap<Hash128, isize> {
        let mut out: HashMap<Hash128, isize> = HashMap::new();
        for (row, weight) in &right_delta.changes {
            let key = self.extract_key(&row.values, &self.right_key_indices);
            if Self::key_has_null(&key) {
                continue;
            }
            *out.entry(key.cached_hash()).or_default() += *weight;
        }
        out
    }

    /// Build the list of (key, δ_count) for keys whose count actually changed.
    fn compute_key_deltas(&self, right_delta: &Delta) -> Vec<(HashableRow, isize)> {
        let mut by_hash: HashMap<Hash128, (HashableRow, isize)> = HashMap::new();
        for (row, weight) in &right_delta.changes {
            let key = self.extract_key(&row.values, &self.right_key_indices);
            if Self::key_has_null(&key) {
                continue;
            }
            let entry = by_hash
                .entry(key.cached_hash())
                .or_insert_with(|| (key.clone(), 0));
            entry.1 += *weight;
        }
        by_hash.into_values().filter(|(_, d)| *d != 0).collect()
    }
}

/// Read R_COUNT.weight for a given join key. Returns 0 if no entry exists.
fn read_r_count(
    storage_id: i64,
    join_key_hash: Hash128,
    cursors: &mut DbspStateCursors,
) -> Result<IOResult<isize>> {
    if cursors.index_cursor.root_page() == 0 {
        return Ok(IOResult::Done(0));
    }
    let sentinel = Hash128::new(0, 0);
    let index_key_values = vec![
        Value::from_i64(storage_id),
        join_key_hash.to_value()?,
        sentinel.to_value()?,
    ];
    let index_record = ImmutableRecord::from_values(&index_key_values, index_key_values.len())?;
    let res = return_if_io!(cursors.index_cursor.seek(
        SeekKey::IndexKey(index_record.as_record_ref()),
        SeekOp::GE { eq_only: true }
    ));
    // Even with `eq_only: true` the btree can answer `TryAdvance` when the
    // matching entry sits just past the current leaf-page boundary (the
    // cursor is parked at the boundary, not on the record). Treating that as
    // "absent" reads R_COUNT as 0 for a key that is actually present, which
    // makes the antijoin L-term emit a spurious null-padded row for an
    // already-matched left row (the duplicate-row bug: a stale `tags=[]`
    // ghost beside the matched `tags=[Page]` row). We must advance and then
    // re-check the key, mirroring `read_next_join_row` / the AggregateOperator
    // TryAdvance fix.
    let positioned = match res {
        SeekResult::Found => true,
        SeekResult::NotFound => false,
        SeekResult::TryAdvance => {
            return_if_io!(cursors.index_cursor.next());
            cursors.index_cursor.has_record()
        }
    };
    if !positioned {
        return Ok(IOResult::Done(0));
    }
    // Verify the entry the cursor now points at is really our key. After a
    // `TryAdvance` + `next()` the cursor may have stepped onto a different
    // `(storage_id, join_key_hash)`; the `Found` path is already exact but
    // re-checking is cheap and keeps the two paths uniform.
    let index_rec = return_if_io!(cursors.index_cursor.record());
    let key_matches = match index_rec {
        Some(rec) => match rec.get_three_values(0, 1, 2) {
            Ok((v0, v1, _v2)) => {
                let sid_ok = matches!(
                    v0.to_owned()?,
                    Value::Numeric(Numeric::Integer(id)) if id == storage_id
                );
                let hash_ok = match v1.to_owned()? {
                    Value::Blob(ref b) => {
                        Hash128::from_blob(b).map(|h| h == join_key_hash).unwrap_or(false)
                    }
                    _ => false,
                };
                sid_ok && hash_ok
            }
            Err(_) => false,
        },
        None => false,
    };
    if !key_matches {
        return Ok(IOResult::Done(0));
    }
    let rowid = match return_if_io!(cursors.index_cursor.rowid()) {
        Some(r) => r,
        None => return Ok(IOResult::Done(0)),
    };
    return_if_io!(cursors
        .table_cursor
        .seek(SeekKey::TableRowId(rowid), SeekOp::GE { eq_only: true }));
    let rec = return_if_io!(cursors.table_cursor.record());
    let r = match rec {
        Some(r) => r,
        None => return Ok(IOResult::Done(0)),
    };
    let v = r.get_value(4)?.to_owned()?;
    let weight = match v {
        Value::Numeric(Numeric::Integer(w)) => w as isize,
        _ => 0,
    };
    Ok(IOResult::Done(weight))
}

fn serialize_l_row(row: &HashableRow) -> Result<Vec<u8>> {
    let mut all_values = Vec::with_capacity(row.values.len() + 1);
    all_values.push(Value::from_i64(row.rowid));
    all_values.extend_from_slice(&row.values);
    let record = ImmutableRecord::from_values(&all_values, all_values.len())?;
    Ok(record.as_blob().clone())
}

#[allow(dead_code)]
fn deserialize_l_row(blob: &[u8]) -> Result<HashableRow> {
    let record = ImmutableRecord::from_bin_record(blob.to_vec());
    let all_values: Vec<Value> = record.get_values_owned()?;
    if all_values.is_empty() {
        return Err(crate::LimboError::InternalError(
            "L row blob must contain at least rowid".to_string(),
        ));
    }
    let rowid = match &all_values[0] {
        Value::Numeric(Numeric::Integer(i)) => *i,
        _ => {
            return Err(crate::LimboError::InternalError(
                "First value must be rowid (integer)".to_string(),
            ));
        }
    };
    let values = all_values[1..].to_vec();
    Ok(HashableRow::new(rowid, values))
}

fn take_emit_r_transitions(state: &mut EvalState) -> (DeltaPair, Delta, Vec<AffectedKey>, usize) {
    match std::mem::replace(state, EvalState::Uninitialized) {
        EvalState::Antijoin(boxed) => match *boxed {
            AntijoinEvalState::EmitRTransitions {
                deltas,
                output,
                affected_keys,
                key_idx,
                ..
            } => (deltas, output, affected_keys, key_idx),
            _ => unreachable!("take_emit_r_transitions: expected EmitRTransitions"),
        },
        _ => unreachable!("take_emit_r_transitions: expected Antijoin"),
    }
}

fn take_process_l_delta(
    state: &mut EvalState,
) -> (DeltaPair, Delta, HashMap<Hash128, isize>, usize) {
    match std::mem::replace(state, EvalState::Uninitialized) {
        EvalState::Antijoin(boxed) => match *boxed {
            AntijoinEvalState::ProcessLDelta {
                deltas,
                output,
                right_count_deltas,
                l_idx,
                ..
            } => (deltas, output, right_count_deltas, l_idx),
            _ => unreachable!("take_process_l_delta: expected ProcessLDelta"),
        },
        _ => unreachable!("take_process_l_delta: expected Antijoin"),
    }
}

impl AntijoinOperator {
    fn eval_internal(
        &mut self,
        state: &mut EvalState,
        cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Delta>> {
        loop {
            let loop_state = std::mem::replace(state, EvalState::Uninitialized);
            match loop_state {
                EvalState::Uninitialized => {
                    panic!("AntijoinOperator::eval called with Uninitialized state");
                }
                EvalState::Init { mut deltas } => {
                    // Consolidate input deltas. Both passes read pre-state
                    // from storage once and apply consolidated δ — without
                    // this, redundant entries (e.g. delete+reinsert of an
                    // identical row) leak into per-entry calculations.
                    // See MEMORY.md "MatchCounter Unconsolidated Input Bug".
                    deltas.left.consolidate();
                    deltas.right.consolidate();

                    let key_deltas = self.compute_key_deltas(&deltas.right);
                    let mut affected_keys: Vec<AffectedKey> = key_deltas
                        .into_iter()
                        .map(|(k, d)| AffectedKey {
                            join_key: k,
                            delta_count: d,
                            c_pre: None,
                        })
                        .collect();
                    affected_keys.sort_by_key(|a| a.join_key.cached_hash().as_i64());

                    *state = EvalState::Antijoin(Box::new(AntijoinEvalState::EmitRTransitions {
                        deltas,
                        output: Delta::new(),
                        affected_keys,
                        key_idx: 0,
                        sub: Box::new(RKeyScan::Idle),
                    }));
                }
                EvalState::Antijoin(boxed) => {
                    let inner = *boxed;
                    let result = self.process_state(inner, state, cursors)?;
                    match result {
                        IOResult::IO(io) => return Ok(IOResult::IO(io)),
                        IOResult::Done(Some(out)) => return Ok(IOResult::Done(out)),
                        IOResult::Done(None) => continue,
                    }
                }
                EvalState::Done => {
                    return Ok(IOResult::Done(Delta::new()));
                }
                EvalState::Aggregate(_) | EvalState::Join(_) => {
                    panic!("AntijoinOperator received non-Antijoin EvalState");
                }
            }
        }
    }

    fn process_state(
        &mut self,
        inner: AntijoinEvalState,
        outer: &mut EvalState,
        cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Option<Delta>>> {
        match inner {
            AntijoinEvalState::Uninitialized => {
                panic!("AntijoinEvalState::Uninitialized");
            }
            AntijoinEvalState::EmitRTransitions {
                deltas,
                output,
                affected_keys,
                key_idx,
                sub,
            } => {
                // R-pass complete → transition to L-pass.
                if key_idx >= affected_keys.len() {
                    let r_count_deltas = self.compute_r_count_deltas(&deltas.right);
                    *outer = EvalState::Antijoin(Box::new(AntijoinEvalState::ProcessLDelta {
                        deltas,
                        output,
                        right_count_deltas: r_count_deltas,
                        l_idx: 0,
                        sub: Box::new(LRowResolve::Idle),
                    }));
                    return Ok(IOResult::Done(None));
                }

                let key_join = affected_keys[key_idx].join_key.clone();
                let delta_count = affected_keys[key_idx].delta_count;
                let join_key_hash = key_join.cached_hash();
                let r_storage_id = self.r_count_storage_id();
                let li_storage_id = self.l_index_storage_id();

                match *sub {
                    RKeyScan::Idle => {
                        *outer =
                            EvalState::Antijoin(Box::new(AntijoinEvalState::EmitRTransitions {
                                deltas,
                                output,
                                affected_keys,
                                key_idx,
                                sub: Box::new(RKeyScan::ReadingRCount { sought: false }),
                            }));
                        Ok(IOResult::Done(None))
                    }
                    RKeyScan::ReadingRCount { .. } => {
                        *outer =
                            EvalState::Antijoin(Box::new(AntijoinEvalState::EmitRTransitions {
                                deltas,
                                output,
                                affected_keys,
                                key_idx,
                                sub: Box::new(RKeyScan::ReadingRCount { sought: true }),
                            }));
                        let c_pre =
                            return_if_io!(read_r_count(r_storage_id, join_key_hash, cursors));
                        let (deltas, output, mut affected_keys, mut key_idx) =
                            take_emit_r_transitions(outer);
                        let c_post = c_pre + delta_count;
                        affected_keys[key_idx].c_pre = Some(c_pre);

                        // indicator_delta = [c_post==0] − [c_pre==0]
                        // ∈ {-1, 0, +1}
                        let indicator_delta = (c_post == 0) as isize - (c_pre == 0) as isize;

                        if indicator_delta == 0 {
                            key_idx += 1;
                            *outer = EvalState::Antijoin(Box::new(
                                AntijoinEvalState::EmitRTransitions {
                                    deltas,
                                    output,
                                    affected_keys,
                                    key_idx,
                                    sub: Box::new(RKeyScan::Idle),
                                },
                            ));
                            return Ok(IOResult::Done(None));
                        }

                        *outer =
                            EvalState::Antijoin(Box::new(AntijoinEvalState::EmitRTransitions {
                                deltas,
                                output,
                                affected_keys,
                                key_idx,
                                sub: Box::new(RKeyScan::ScanningL {
                                    indicator_delta,
                                    last_l_hash: None,
                                }),
                            }));
                        Ok(IOResult::Done(None))
                    }
                    RKeyScan::ScanningL {
                        indicator_delta,
                        last_l_hash,
                    } => {
                        *outer =
                            EvalState::Antijoin(Box::new(AntijoinEvalState::EmitRTransitions {
                                deltas,
                                output,
                                affected_keys,
                                key_idx,
                                sub: Box::new(RKeyScan::ScanningL {
                                    indicator_delta,
                                    last_l_hash,
                                }),
                            }));
                        let scan = return_if_io!(read_next_join_row(
                            li_storage_id,
                            &key_join,
                            last_l_hash,
                            cursors
                        ));
                        let (deltas, mut output, affected_keys, mut key_idx) =
                            take_emit_r_transitions(outer);
                        match scan {
                            None => {
                                key_idx += 1;
                                *outer = EvalState::Antijoin(Box::new(
                                    AntijoinEvalState::EmitRTransitions {
                                        deltas,
                                        output,
                                        affected_keys,
                                        key_idx,
                                        sub: Box::new(RKeyScan::Idle),
                                    },
                                ));
                            }
                            Some((l_hash, l_row, l_weight)) => {
                                // Emit `L_pre(l,k) · indicator_delta`.
                                let emit_weight = l_weight * indicator_delta;
                                if emit_weight != 0 {
                                    let padded = self.null_pad(&l_row);
                                    output.changes.push((padded, emit_weight));
                                }
                                *outer = EvalState::Antijoin(Box::new(
                                    AntijoinEvalState::EmitRTransitions {
                                        deltas,
                                        output,
                                        affected_keys,
                                        key_idx,
                                        sub: Box::new(RKeyScan::ScanningL {
                                            indicator_delta,
                                            last_l_hash: Some(l_hash),
                                        }),
                                    },
                                ));
                            }
                        }
                        Ok(IOResult::Done(None))
                    }
                    RKeyScan::Done => Ok(IOResult::Done(None)),
                }
            }
            AntijoinEvalState::ProcessLDelta {
                deltas,
                mut output,
                right_count_deltas,
                l_idx,
                sub,
            } => {
                if l_idx >= deltas.left.changes.len() {
                    *outer = EvalState::Antijoin(Box::new(AntijoinEvalState::Done {
                        output: std::mem::take(&mut output),
                    }));
                    return Ok(IOResult::Done(None));
                }

                let (l_row, l_weight) = deltas.left.changes[l_idx].clone();
                let l_key = self.extract_key(&l_row.values, &self.left_key_indices);
                let l_key_hash = l_key.cached_hash();
                let r_storage_id = self.r_count_storage_id();

                match *sub {
                    LRowResolve::Idle => {
                        *outer = EvalState::Antijoin(Box::new(AntijoinEvalState::ProcessLDelta {
                            deltas,
                            output,
                            right_count_deltas,
                            l_idx,
                            sub: Box::new(LRowResolve::ReadingRCount { sought: false }),
                        }));
                        Ok(IOResult::Done(None))
                    }
                    LRowResolve::ReadingRCount { .. } => {
                        *outer = EvalState::Antijoin(Box::new(AntijoinEvalState::ProcessLDelta {
                            deltas,
                            output,
                            right_count_deltas,
                            l_idx,
                            sub: Box::new(LRowResolve::ReadingRCount { sought: true }),
                        }));
                        let c_pre = return_if_io!(read_r_count(r_storage_id, l_key_hash, cursors));
                        let (deltas, mut output, right_count_deltas, mut l_idx) =
                            take_process_l_delta(outer);
                        let dr = right_count_deltas.get(&l_key_hash).copied().unwrap_or(0);
                        let c_post = c_pre + dr;

                        // L-term: emit `δL(l, k) · [c_post == 0]`.
                        if c_post == 0 && l_weight != 0 {
                            let padded = self.null_pad(&l_row);
                            output.changes.push((padded, l_weight));
                        }
                        l_idx += 1;
                        *outer = EvalState::Antijoin(Box::new(AntijoinEvalState::ProcessLDelta {
                            deltas,
                            output,
                            right_count_deltas,
                            l_idx,
                            sub: Box::new(LRowResolve::Idle),
                        }));
                        Ok(IOResult::Done(None))
                    }
                    LRowResolve::Done => Ok(IOResult::Done(None)),
                }
            }
            AntijoinEvalState::Done { output } => Ok(IOResult::Done(Some(output))),
        }
    }
}

impl IncrementalOperator for AntijoinOperator {
    fn eval(
        &mut self,
        state: &mut EvalState,
        cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Delta>> {
        let delta = return_if_io!(self.eval_internal(state, cursors));
        Ok(IOResult::Done(delta))
    }

    fn commit(
        &mut self,
        mut deltas: DeltaPair,
        cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Delta>> {
        deltas.left.consolidate();
        deltas.right.consolidate();
        loop {
            let mut state = std::mem::replace(&mut self.commit_state, AntijoinCommitState::Invalid);
            match &mut state {
                AntijoinCommitState::Idle => {
                    self.commit_state = AntijoinCommitState::Eval {
                        eval_state: deltas.clone().into(),
                    };
                }
                AntijoinCommitState::Eval { ref mut eval_state } => {
                    let output = return_and_restore_if_io!(
                        &mut self.commit_state,
                        state,
                        self.eval(eval_state, cursors)
                    );
                    let key_deltas = self.compute_key_deltas(&deltas.right);
                    self.commit_state = AntijoinCommitState::PersistRCount {
                        deltas: deltas.clone(),
                        output,
                        key_deltas,
                        idx: 0,
                        write_row: WriteRow::new(),
                    };
                }
                AntijoinCommitState::PersistRCount {
                    deltas,
                    output,
                    key_deltas,
                    idx,
                    ref mut write_row,
                } => {
                    if *idx >= key_deltas.len() {
                        self.commit_state = AntijoinCommitState::PersistLIndex {
                            deltas: std::mem::take(deltas),
                            output: std::mem::take(output),
                            idx: 0,
                            write_row: WriteRow::new(),
                        };
                        continue;
                    }

                    let (key, delta_count) = key_deltas[*idx].clone();
                    let key_hash = key.cached_hash();
                    let sentinel = Hash128::new(0, 0);
                    let storage_id = self.r_count_storage_id();
                    let index_key = vec![
                        Value::from_i64(storage_id),
                        key_hash.to_value()?,
                        sentinel.to_value()?,
                    ];
                    let record_values = vec![
                        Value::from_i64(storage_id),
                        key_hash.to_value()?,
                        sentinel.to_value()?,
                        Value::Null,
                    ];

                    let next_idx = *idx + 1;
                    return_and_restore_if_io!(
                        &mut self.commit_state,
                        state,
                        write_row.write_row(cursors, index_key, record_values, delta_count)
                    );
                    self.commit_state = AntijoinCommitState::PersistRCount {
                        deltas: std::mem::take(deltas),
                        output: std::mem::take(output),
                        key_deltas: std::mem::take(key_deltas),
                        idx: next_idx,
                        write_row: WriteRow::new(),
                    };
                }
                AntijoinCommitState::PersistLIndex {
                    deltas,
                    output,
                    idx,
                    ref mut write_row,
                } => {
                    if *idx >= deltas.left.changes.len() {
                        self.commit_state = AntijoinCommitState::Idle;
                        return Ok(IOResult::Done(std::mem::take(output)));
                    }

                    let (l_row, l_weight) = &deltas.left.changes[*idx];
                    let l_key = self.extract_key(&l_row.values, &self.left_key_indices);
                    let l_key_hash = l_key.cached_hash();
                    let l_row_hash = l_row.cached_hash();
                    let storage_id = self.l_index_storage_id();

                    let index_key = vec![
                        Value::from_i64(storage_id),
                        l_key_hash.to_value()?,
                        l_row_hash.to_value()?,
                    ];
                    let row_blob = serialize_l_row(l_row)?;
                    let record_values = vec![
                        Value::from_i64(storage_id),
                        l_key_hash.to_value()?,
                        l_row_hash.to_value()?,
                        Value::Blob(row_blob),
                    ];

                    let next_idx = *idx + 1;
                    return_and_restore_if_io!(
                        &mut self.commit_state,
                        state,
                        write_row.write_row(cursors, index_key, record_values, *l_weight)
                    );
                    self.commit_state = AntijoinCommitState::PersistLIndex {
                        deltas: std::mem::take(deltas),
                        output: std::mem::take(output),
                        idx: next_idx,
                        write_row: WriteRow::new(),
                    };
                }
                AntijoinCommitState::Invalid => {
                    tracing::warn!(
                        "[AntijoinOperator::commit] Recovering from Invalid state. \
                         Resetting to Idle."
                    );
                    self.commit_state = AntijoinCommitState::Idle;
                }
            }
        }
    }

    fn set_tracker(&mut self, tracker: Arc<Mutex<ComputationTracker>>) {
        self.tracker = Some(tracker);
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
