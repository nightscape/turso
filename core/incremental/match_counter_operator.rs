
//! MatchCounterOperator — produces null-padded rows for LEFT JOIN.
//!
//! DBSP semantics: `L ⟕ R = (L ⋈ R) ⊎ { (l, NULL_R) : count_R[l.k] == 0 }`.
//! This operator emits ONLY the antijoin half. The matched half is produced
//! by the existing `JoinOperator` (in `JoinType::Inner` mode) wired as a
//! sibling. A `MergeOperator` (`UnionMode::All`) UNION ALLs the two outputs.
//!
//! ## Storage
//!
//! Two btree-backed stores, identified by `column_index` in
//! `generate_storage_id(operator_id, column_index, JOIN_TYPE_BIT)`:
//!
//! - **`L_PRESENCE`** (`column_index = 0`): multiset of L rows, indexed by
//!   join key. Btree key `(storage_id_LP, l_join_key_hash, l_row_hash)`,
//!   value blob = serialized L row, weight = L multiplicity.
//! - **`R_COUNT`** (`column_index = 1`): per-join-key R multiplicity.
//!   Btree key `(storage_id_RC, l_join_key_hash, sentinel)`, weight =
//!   `count_R[k]` (uses `WriteRow`'s `weight += δ` semantics).
//!
//! No `R_PRESENCE` store: all R-side info needed (old/new join key) arrives
//! in the input delta; we don't need to re-scan persisted R rows.
//!
//! ## State machine (one `return_if_io!` per state)
//!
//! See MEMORY.md "IVM State Index Duplicate Entry Bug" for why this matters.
//! Each btree operation yields IO at most once; states with seek+mutate use
//! a `sought: bool` flag to route re-entry to the mutate phase.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

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

/// MatchCounterOperator state needed during eval.
#[derive(Debug, Default)]
pub enum MatchCounterEvalState {
    #[default]
    Uninitialized,
    /// Phase B: walk affected R-side keys, scan L_PRESENCE for each, emit
    /// null-pad transitions when the R-count crosses zero.
    EmitRTransitions {
        deltas: DeltaPair,
        output: Delta,
        /// All input keys with their batched count delta — but we only emit
        /// for keys that actually transition zero↔positive.
        affected_keys: Vec<AffectedKey>,
        key_idx: usize,
        sub: Box<RKeyScan>,
        /// Cached_hashes of every L row appearing in δL with non-zero net
        /// weight (after `consolidate`). Phase B must NOT emit null-pad
        /// transitions for L rows in this set — those rows have a joint
        /// (L,R) transition that Phase C handles with the correct pre/post
        /// formula. Without this filter, both phases emit a retraction for
        /// the same null-pad row and the downstream aggregate multiset
        /// goes negative. See bug:
        /// `bugs/holon_json_group_array_multiset_negative_2026-05-15.md`.
        delta_l_rows: HashSet<Hash128>,
    },
    /// Phase C: walk δL, emit null-pad insert/retract for each l based on
    /// pre/post existence and pre/post R-count.
    ProcessLDelta {
        deltas: DeltaPair,
        output: Delta,
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
    /// Sum of δR weights for rows with this key (the count delta).
    pub delta_count: isize,
    /// Filled in once we read R_COUNT — None until then.
    pub c_pre: Option<isize>,
}

/// Sub-state for Phase B: read R_COUNT pre-state, then scan L_PRESENCE for
/// the same key. Each variant has at most ONE `return_if_io!`.
#[derive(Debug, Default)]
pub enum RKeyScan {
    #[default]
    Idle,
    /// Read the existing R_COUNT entry to discover c_pre. We need at most
    /// one `return_if_io!`-yielding operation per state.
    ReadingRCount {
        sought: bool,
    },
    /// We know c_pre and c_post; if the count crossed zero, scan L_PRESENCE
    /// and emit one null-pad row per L found (with the correct weight).
    ScanningL {
        emit_weight: isize,
        last_l_hash: Option<Hash128>,
    },
    Done,
}

/// Sub-state for Phase C: read L_PRESENCE pre-state, then read R_COUNT
/// pre-state, then emit Δ_emit.
#[derive(Debug, Default)]
pub enum LRowResolve {
    #[default]
    Idle,
    ReadingLPre {
        sought: bool,
    },
    ReadingRCount {
        sought: bool,
        l_pre_weight: isize,
    },
    Done,
}

/// MatchCounterOperator — produces only the null-padded "unmatched" half of
/// a LEFT JOIN's output delta. The compiler wires this in parallel with
/// an InnerJoinOperator and UNION-ALLs the two via MergeOperator.
#[derive(Debug)]
pub struct MatchCounterOperator {
    operator_id: i64,
    /// Indices into L row values that form the join key.
    left_key_indices: Vec<usize>,
    /// Indices into R row values that form the join key (used to compute
    /// δR_count from input δR).
    right_key_indices: Vec<usize>,
    /// Number of right-side columns — needed to construct NULL padding.
    right_column_count: usize,
    tracker: Option<Arc<Mutex<ComputationTracker>>>,
    commit_state: MatchCounterCommitState,
}

#[derive(Debug, Default)]
enum MatchCounterCommitState {
    #[default]
    Idle,
    Eval {
        eval_state: EvalState,
    },
    /// Persist L_PRESENCE updates from δL (one btree write per δL row).
    PersistLPresence {
        deltas: DeltaPair,
        output: Delta,
        right_count_deltas: HashMap<Hash128, isize>,
        idx: usize,
        write_row: WriteRow,
    },
    /// Persist R_COUNT updates: one btree write per affected join key.
    PersistRCount {
        deltas: DeltaPair,
        output: Delta,
        key_deltas: Vec<(HashableRow, isize)>, // (join_key, δ_count)
        idx: usize,
        write_row: WriteRow,
    },
    /// Sentinel value used to recover from prior panics during commit.
    Invalid,
}

impl MatchCounterOperator {
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
            commit_state: MatchCounterCommitState::Idle,
        }
    }

    fn l_presence_storage_id(&self) -> i64 {
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

    /// Returns true if any element of the join key is NULL — these never
    /// match in SQL, but they DO produce a null-padded LEFT JOIN row (since
    /// nothing in R matches).
    fn key_has_null(key: &HashableRow) -> bool {
        key.values.iter().any(|v| matches!(v, Value::Null))
    }

    /// Build the null-padded output row: l columns + NULL × right_column_count.
    fn null_pad(&self, l_row: &HashableRow) -> HashableRow {
        let mut values = l_row.values.to_vec();
        values.extend(std::iter::repeat_n(Value::Null, self.right_column_count));
        // Use a hash-based synthetic rowid (matches inner JoinOperator's
        // combine_rows pattern). Downstream MergeOperator reassigns rowids
        // anyway, but a stable hash means duplicate emits collapse.
        let values: super::dbsp::RowValues = values.into();
        let temp = HashableRow::new(0, values.clone());
        HashableRow::new(temp.cached_hash().as_i64(), values)
    }

    /// Build δR_count[key_hash] from input δR.
    fn compute_r_count_deltas(&self, right_delta: &Delta) -> HashMap<Hash128, isize> {
        let mut out: HashMap<Hash128, isize> = HashMap::new();
        for (row, weight) in &right_delta.changes {
            let key = self.extract_key(&row.values, &self.right_key_indices);
            if Self::key_has_null(&key) {
                continue; // NULL-keyed R rows match nothing — they don't affect counts.
            }
            *out.entry(key.cached_hash()).or_default() += *weight;
        }
        out
    }

    /// Build the list of (key, δ_count) for keys whose count actually changed.
    fn compute_key_deltas(&self, right_delta: &Delta) -> Vec<(HashableRow, isize)> {
        // We need both the key (for storage) and the delta. Iterate δR,
        // group by key hash, but keep one full HashableRow per group.
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
/// This is one I/O-yielding state — caller wraps with the `sought: bool`
/// pattern if needed.
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
        join_key_hash.to_value(),
        sentinel.to_value(),
    ];
    let index_record = ImmutableRecord::from_values(&index_key_values, index_key_values.len());
    let res = return_if_io!(cursors.index_cursor.seek(
        SeekKey::IndexKey(&index_record),
        SeekOp::GE { eq_only: true }
    ));
    if !matches!(res, SeekResult::Found) {
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
    let v = r.get_value(4)?.to_owned();
    let weight = match v {
        Value::Numeric(Numeric::Integer(w)) => w as isize,
        _ => 0,
    };
    Ok(IOResult::Done(weight))
}

/// Read L_PRESENCE.weight for a given (join_key, l_row). Returns 0 if not
/// present. Same shape as `read_r_count` but the key includes the row hash.
fn read_l_presence(
    storage_id: i64,
    join_key_hash: Hash128,
    l_row_hash: Hash128,
    cursors: &mut DbspStateCursors,
) -> Result<IOResult<isize>> {
    if cursors.index_cursor.root_page() == 0 {
        return Ok(IOResult::Done(0));
    }
    let index_key_values = vec![
        Value::from_i64(storage_id),
        join_key_hash.to_value(),
        l_row_hash.to_value(),
    ];
    let index_record = ImmutableRecord::from_values(&index_key_values, index_key_values.len());
    let res = return_if_io!(cursors.index_cursor.seek(
        SeekKey::IndexKey(&index_record),
        SeekOp::GE { eq_only: true }
    ));
    if !matches!(res, SeekResult::Found) {
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
    let v = r.get_value(4)?.to_owned();
    let weight = match v {
        Value::Numeric(Numeric::Integer(w)) => w as isize,
        _ => 0,
    };
    Ok(IOResult::Done(weight))
}

fn serialize_l_row(row: &HashableRow) -> Vec<u8> {
    let mut all_values = Vec::with_capacity(row.values.len() + 1);
    all_values.push(Value::from_i64(row.rowid));
    all_values.extend_from_slice(&row.values);
    let record = ImmutableRecord::from_values(&all_values, all_values.len());
    record.as_blob().clone()
}

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

/// Pull `EmitRTransitions` payload back out of `*outer` after a successful
/// I/O. Caller must have just placed an `EmitRTransitions` value there
/// before the read; otherwise this panics.
fn take_emit_r_transitions(
    state: &mut EvalState,
) -> (DeltaPair, Delta, Vec<AffectedKey>, usize, HashSet<Hash128>) {
    match std::mem::replace(state, EvalState::Uninitialized) {
        EvalState::MatchCounter(boxed) => match *boxed {
            MatchCounterEvalState::EmitRTransitions {
                deltas,
                output,
                affected_keys,
                key_idx,
                delta_l_rows,
                ..
            } => (deltas, output, affected_keys, key_idx, delta_l_rows),
            _ => unreachable!("take_emit_r_transitions: expected EmitRTransitions"),
        },
        _ => unreachable!("take_emit_r_transitions: expected MatchCounter"),
    }
}

/// Same as `take_emit_r_transitions` but for `ProcessLDelta`.
fn take_process_l_delta(
    state: &mut EvalState,
) -> (DeltaPair, Delta, HashMap<Hash128, isize>, usize) {
    match std::mem::replace(state, EvalState::Uninitialized) {
        EvalState::MatchCounter(boxed) => match *boxed {
            MatchCounterEvalState::ProcessLDelta {
                deltas,
                output,
                right_count_deltas,
                l_idx,
                ..
            } => (deltas, output, right_count_deltas, l_idx),
            _ => unreachable!("take_process_l_delta: expected ProcessLDelta"),
        },
        _ => unreachable!("take_process_l_delta: expected MatchCounter"),
    }
}

impl MatchCounterOperator {
    /// Drive eval. Reads pre-state from btree as needed; emits null-pad delta.
    fn eval_internal(
        &mut self,
        state: &mut EvalState,
        cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Delta>> {
        loop {
            let loop_state = std::mem::replace(state, EvalState::Uninitialized);
            match loop_state {
                EvalState::Uninitialized => {
                    panic!("MatchCounterOperator::eval called with Uninitialized state");
                }
                EvalState::Init { mut deltas } => {
                    // Consolidate input deltas before eval. The algorithm
                    // reads l_pre / c_pre from storage once and computes
                    // post per-entry, so two entries affecting the same
                    // row cannot see each other's effect — a no-op UPDATE
                    // (del+ins with identical values) would otherwise
                    // emit a spurious null-pad retraction. Mirror the
                    // consolidation in `commit` so eval-only paths
                    // (uncommitted reads) get the same correctness.
                    deltas.left.consolidate();
                    deltas.right.consolidate();
                    // Phase A: aggregate δR_count by join key (in memory).
                    let key_deltas = self.compute_key_deltas(&deltas.right);

                    // Sort key_deltas for determinism (otherwise HashMap order
                    // bleeds into output ordering — see MEMORY.md "IVM Matview
                    // UPDATE Not Propagated — CDC Zero Changes" for similar
                    // class of bugs).
                    let mut affected_keys: Vec<AffectedKey> = key_deltas
                        .into_iter()
                        .map(|(k, d)| AffectedKey {
                            join_key: k,
                            delta_count: d,
                            c_pre: None,
                        })
                        .collect();
                    affected_keys.sort_by_key(|a| a.join_key.cached_hash().as_i64());

                    // δL rows whose joint (L, R) transition is Phase C's
                    // responsibility — Phase B must skip these to avoid
                    // double-emitting null-pad retractions.
                    let delta_l_rows: HashSet<Hash128> = deltas
                        .left
                        .changes
                        .iter()
                        .map(|(row, _)| row.cached_hash())
                        .collect();

                    *state = EvalState::MatchCounter(Box::new(
                        MatchCounterEvalState::EmitRTransitions {
                            deltas,
                            output: Delta::new(),
                            affected_keys,
                            key_idx: 0,
                            sub: Box::new(RKeyScan::Idle),
                            delta_l_rows,
                        },
                    ));
                }
                EvalState::MatchCounter(boxed) => {
                    let inner = *boxed;
                    let result = self.process_match_counter_state(inner, state, cursors)?;
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
                    panic!("MatchCounterOperator received non-MatchCounter EvalState");
                }
            }
        }
    }

    /// Drive one step of the MatchCounter sub-machine. Returns:
    ///   - IO yield: caller stores the IO and returns up.
    ///   - Done(Some(delta)): work complete; deliver delta.
    ///   - Done(None): state was advanced; caller re-enters the outer loop.
    fn process_match_counter_state(
        &mut self,
        inner: MatchCounterEvalState,
        outer: &mut EvalState,
        cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Option<Delta>>> {
        match inner {
            MatchCounterEvalState::Uninitialized => {
                panic!("MatchCounterEvalState::Uninitialized");
            }
            MatchCounterEvalState::EmitRTransitions {
                deltas,
                output,
                affected_keys,
                key_idx,
                sub,
                delta_l_rows,
            } => {
                // Done with all keys → transition to Phase C.
                if key_idx >= affected_keys.len() {
                    let r_count_deltas = self.compute_r_count_deltas(&deltas.right);
                    *outer =
                        EvalState::MatchCounter(Box::new(MatchCounterEvalState::ProcessLDelta {
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
                let lp_storage_id = self.l_presence_storage_id();

                match *sub {
                    RKeyScan::Idle => {
                        // Begin: read R_COUNT for this key.
                        *outer = EvalState::MatchCounter(Box::new(
                            MatchCounterEvalState::EmitRTransitions {
                                deltas,
                                output,
                                affected_keys,
                                key_idx,
                                sub: Box::new(RKeyScan::ReadingRCount { sought: false }),
                                delta_l_rows,
                            },
                        ));
                        Ok(IOResult::Done(None))
                    }
                    RKeyScan::ReadingRCount { .. } => {
                        // Save in-progress state to *outer BEFORE the I/O so
                        // a yield resumes here on re-entry. Without this,
                        // *outer stays Uninitialized (the parent loop's
                        // `mem::replace` cleared it) and the next eval cycle
                        // panics on the Uninitialized arm.
                        *outer = EvalState::MatchCounter(Box::new(
                            MatchCounterEvalState::EmitRTransitions {
                                deltas,
                                output,
                                affected_keys,
                                key_idx,
                                sub: Box::new(RKeyScan::ReadingRCount { sought: true }),
                                delta_l_rows,
                            },
                        ));
                        let c_pre =
                            return_if_io!(read_r_count(r_storage_id, join_key_hash, cursors));
                        let (deltas, output, mut affected_keys, mut key_idx, delta_l_rows) =
                            take_emit_r_transitions(outer);
                        let c_post = c_pre + delta_count;
                        affected_keys[key_idx].c_pre = Some(c_pre);

                        // Determine whether emit is needed.
                        let emit_weight = if c_pre == 0 && c_post > 0 {
                            -1 // retract null-pad rows
                        } else if c_pre > 0 && c_post == 0 {
                            1 // emit null-pad rows
                        } else {
                            0
                        };

                        if emit_weight == 0 {
                            // No transition; advance to next key.
                            key_idx += 1;
                            *outer = EvalState::MatchCounter(Box::new(
                                MatchCounterEvalState::EmitRTransitions {
                                    deltas,
                                    output,
                                    affected_keys,
                                    key_idx,
                                    sub: Box::new(RKeyScan::Idle),
                                    delta_l_rows,
                                },
                            ));
                            return Ok(IOResult::Done(None));
                        }

                        *outer = EvalState::MatchCounter(Box::new(
                            MatchCounterEvalState::EmitRTransitions {
                                deltas,
                                output,
                                affected_keys,
                                key_idx,
                                sub: Box::new(RKeyScan::ScanningL {
                                    emit_weight,
                                    last_l_hash: None,
                                }),
                                delta_l_rows,
                            },
                        ));
                        Ok(IOResult::Done(None))
                    }
                    RKeyScan::ScanningL {
                        emit_weight,
                        last_l_hash,
                    } => {
                        // Save in-progress state before the read so an I/O
                        // yield resumes the same scan position.
                        *outer = EvalState::MatchCounter(Box::new(
                            MatchCounterEvalState::EmitRTransitions {
                                deltas,
                                output,
                                affected_keys,
                                key_idx,
                                sub: Box::new(RKeyScan::ScanningL {
                                    emit_weight,
                                    last_l_hash,
                                }),
                                delta_l_rows,
                            },
                        ));
                        // Read next L row matching this join key.
                        let scan = return_if_io!(read_next_join_row(
                            lp_storage_id,
                            &key_join,
                            last_l_hash,
                            cursors
                        ));
                        let (deltas, mut output, affected_keys, mut key_idx, delta_l_rows) =
                            take_emit_r_transitions(outer);
                        match scan {
                            None => {
                                // No more L rows for this key — advance.
                                key_idx += 1;
                                *outer = EvalState::MatchCounter(Box::new(
                                    MatchCounterEvalState::EmitRTransitions {
                                        deltas,
                                        output,
                                        affected_keys,
                                        key_idx,
                                        sub: Box::new(RKeyScan::Idle),
                                        delta_l_rows,
                                    },
                                ));
                            }
                            Some((l_hash, l_row, _l_weight)) => {
                                // Skip L rows that appear in δL — their joint
                                // (L, R) transition is Phase C's responsibility.
                                // Without this, both phases emit a null-pad
                                // retraction for the same row and the downstream
                                // aggregate multiset goes negative. See bug:
                                // `bugs/holon_json_group_array_multiset_negative_2026-05-15.md`.
                                if !delta_l_rows.contains(&l_row.cached_hash()) {
                                    let padded = self.null_pad(&l_row);
                                    output.changes.push((padded, emit_weight));
                                }
                                *outer = EvalState::MatchCounter(Box::new(
                                    MatchCounterEvalState::EmitRTransitions {
                                        deltas,
                                        output,
                                        affected_keys,
                                        key_idx,
                                        sub: Box::new(RKeyScan::ScanningL {
                                            emit_weight,
                                            last_l_hash: Some(l_hash),
                                        }),
                                        delta_l_rows,
                                    },
                                ));
                            }
                        }
                        Ok(IOResult::Done(None))
                    }
                    RKeyScan::Done => {
                        // Should never happen — we transition to Idle above.
                        Ok(IOResult::Done(None))
                    }
                }
            }
            MatchCounterEvalState::ProcessLDelta {
                deltas,
                mut output,
                right_count_deltas,
                l_idx,
                sub,
            } => {
                if l_idx >= deltas.left.changes.len() {
                    *outer = EvalState::MatchCounter(Box::new(MatchCounterEvalState::Done {
                        output: std::mem::take(&mut output),
                    }));
                    return Ok(IOResult::Done(None));
                }

                let (l_row, l_weight) = deltas.left.changes[l_idx].clone();
                let l_key = self.extract_key(&l_row.values, &self.left_key_indices);
                let l_key_hash = l_key.cached_hash();
                let l_row_hash = l_row.cached_hash();
                let lp_storage_id = self.l_presence_storage_id();
                let r_storage_id = self.r_count_storage_id();

                match *sub {
                    LRowResolve::Idle => {
                        *outer = EvalState::MatchCounter(Box::new(
                            MatchCounterEvalState::ProcessLDelta {
                                deltas,
                                output,
                                right_count_deltas,
                                l_idx,
                                sub: Box::new(LRowResolve::ReadingLPre { sought: false }),
                            },
                        ));
                        Ok(IOResult::Done(None))
                    }
                    LRowResolve::ReadingLPre { .. } => {
                        // Save in-progress state before the read.
                        *outer = EvalState::MatchCounter(Box::new(
                            MatchCounterEvalState::ProcessLDelta {
                                deltas,
                                output,
                                right_count_deltas,
                                l_idx,
                                sub: Box::new(LRowResolve::ReadingLPre { sought: true }),
                            },
                        ));
                        let l_pre_weight = return_if_io!(read_l_presence(
                            lp_storage_id,
                            l_key_hash,
                            l_row_hash,
                            cursors
                        ));
                        let (deltas, output, right_count_deltas, l_idx) =
                            take_process_l_delta(outer);
                        *outer = EvalState::MatchCounter(Box::new(
                            MatchCounterEvalState::ProcessLDelta {
                                deltas,
                                output,
                                right_count_deltas,
                                l_idx,
                                sub: Box::new(LRowResolve::ReadingRCount {
                                    sought: false,
                                    l_pre_weight,
                                }),
                            },
                        ));
                        Ok(IOResult::Done(None))
                    }
                    LRowResolve::ReadingRCount { l_pre_weight, .. } => {
                        // Save in-progress state before the read.
                        *outer = EvalState::MatchCounter(Box::new(
                            MatchCounterEvalState::ProcessLDelta {
                                deltas,
                                output,
                                right_count_deltas,
                                l_idx,
                                sub: Box::new(LRowResolve::ReadingRCount {
                                    sought: true,
                                    l_pre_weight,
                                }),
                            },
                        ));
                        let c_pre = return_if_io!(read_r_count(r_storage_id, l_key_hash, cursors));
                        let (deltas, mut output, right_count_deltas, mut l_idx) =
                            take_process_l_delta(outer);
                        let dr = right_count_deltas.get(&l_key_hash).copied().unwrap_or(0);
                        let c_post = c_pre + dr;
                        // Existence is "weight > 0" in multiset semantics.
                        // For our purposes l_post = l_pre + δl_weight.
                        let l_post_weight = l_pre_weight + l_weight;
                        let pre_is_unmatched = l_pre_weight > 0 && c_pre == 0;
                        let post_is_unmatched = l_post_weight > 0 && c_post == 0;
                        // Δ_emit = post − pre (each contribution is 0 or 1).
                        let mut emit_weight: isize = 0;
                        if pre_is_unmatched {
                            emit_weight -= 1;
                        }
                        if post_is_unmatched {
                            emit_weight += 1;
                        }
                        // Phase B already covered the case "l_pre > 0 stays >0,
                        // count crosses zero" — that emits with the existing
                        // l rows from L_PRESENCE. But here we're processing δL
                        // entries: l rows that are NEW (l_pre_weight == 0) or
                        // BEING REMOVED (l_post_weight == 0). The Phase-B case
                        // (l_pre > 0 and l_post > 0, i.e. δl == 0) cannot
                        // happen here because we only iterate δL entries (δl
                        // is non-zero by construction).
                        if emit_weight != 0 {
                            let padded = self.null_pad(&l_row);
                            output.changes.push((padded, emit_weight));
                        }
                        l_idx += 1;
                        *outer = EvalState::MatchCounter(Box::new(
                            MatchCounterEvalState::ProcessLDelta {
                                deltas,
                                output,
                                right_count_deltas,
                                l_idx,
                                sub: Box::new(LRowResolve::Idle),
                            },
                        ));
                        Ok(IOResult::Done(None))
                    }
                    LRowResolve::Done => Ok(IOResult::Done(None)),
                }
            }
            MatchCounterEvalState::Done { output } => Ok(IOResult::Done(Some(output))),
        }
    }
}

impl IncrementalOperator for MatchCounterOperator {
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
        // Consolidate input deltas. The eval algorithm reads l_pre / c_pre
        // from storage and computes the post state per-entry as
        // pre + this_entry_weight, but storage isn't updated until after
        // eval — so two deltas affecting the same row don't see each
        // other's effect. A no-op UPDATE (delete + insert with identical
        // values) consolidates to an empty delta; without consolidation
        // each entry is processed independently and emits a spurious
        // null-pad retraction. See ivm-left-join.sqltest tests #40-#41.
        deltas.left.consolidate();
        deltas.right.consolidate();
        loop {
            let mut state =
                std::mem::replace(&mut self.commit_state, MatchCounterCommitState::Invalid);
            match &mut state {
                MatchCounterCommitState::Idle => {
                    self.commit_state = MatchCounterCommitState::Eval {
                        eval_state: deltas.clone().into(),
                    };
                }
                MatchCounterCommitState::Eval { ref mut eval_state } => {
                    let output = return_and_restore_if_io!(
                        &mut self.commit_state,
                        state,
                        self.eval(eval_state, cursors)
                    );
                    let r_count_deltas = self.compute_r_count_deltas(&deltas.right);
                    self.commit_state = MatchCounterCommitState::PersistLPresence {
                        deltas: deltas.clone(),
                        output,
                        right_count_deltas: r_count_deltas,
                        idx: 0,
                        write_row: WriteRow::new(),
                    };
                }
                MatchCounterCommitState::PersistLPresence {
                    deltas,
                    output,
                    right_count_deltas,
                    idx,
                    ref mut write_row,
                } => {
                    if *idx >= deltas.left.changes.len() {
                        let key_deltas: Vec<(HashableRow, isize)> = right_count_deltas
                            .iter()
                            .filter_map(|(_h, d)| {
                                // Re-derive the key HashableRow by scanning δR
                                // for one row whose key matches this hash.
                                // Could be cached upfront; correctness is fine
                                // since R rows with the same key produce the
                                // same hash.
                                None::<(HashableRow, isize)>.or_else(|| {
                                    deltas.right.changes.iter().find_map(|(r, _w)| {
                                        let k =
                                            self.extract_key(&r.values, &self.right_key_indices);
                                        if !Self::key_has_null(&k) && k.cached_hash() == *_h {
                                            Some((k, *d))
                                        } else {
                                            None
                                        }
                                    })
                                })
                            })
                            .collect();
                        self.commit_state = MatchCounterCommitState::PersistRCount {
                            deltas: std::mem::take(deltas),
                            output: std::mem::take(output),
                            key_deltas,
                            idx: 0,
                            write_row: WriteRow::new(),
                        };
                        continue;
                    }

                    let (l_row, l_weight) = &deltas.left.changes[*idx];
                    let l_key = self.extract_key(&l_row.values, &self.left_key_indices);
                    let l_key_hash = l_key.cached_hash();
                    let l_row_hash = l_row.cached_hash();
                    let storage_id = self.l_presence_storage_id();

                    let index_key = vec![
                        Value::from_i64(storage_id),
                        l_key_hash.to_value(),
                        l_row_hash.to_value(),
                    ];
                    let row_blob = serialize_l_row(l_row);
                    let record_values = vec![
                        Value::from_i64(storage_id),
                        l_key_hash.to_value(),
                        l_row_hash.to_value(),
                        Value::Blob(row_blob),
                    ];

                    let next_idx = *idx + 1;
                    return_and_restore_if_io!(
                        &mut self.commit_state,
                        state,
                        write_row.write_row(cursors, index_key, record_values, *l_weight)
                    );
                    self.commit_state = MatchCounterCommitState::PersistLPresence {
                        deltas: std::mem::take(deltas),
                        output: std::mem::take(output),
                        right_count_deltas: std::mem::take(right_count_deltas),
                        idx: next_idx,
                        write_row: WriteRow::new(),
                    };
                }
                MatchCounterCommitState::PersistRCount {
                    deltas,
                    output,
                    key_deltas,
                    idx,
                    ref mut write_row,
                } => {
                    if *idx >= key_deltas.len() {
                        self.commit_state = MatchCounterCommitState::Idle;
                        return Ok(IOResult::Done(std::mem::take(output)));
                    }

                    let (key, delta_count) = key_deltas[*idx].clone();
                    let key_hash = key.cached_hash();
                    let sentinel = Hash128::new(0, 0);
                    let storage_id = self.r_count_storage_id();
                    let index_key = vec![
                        Value::from_i64(storage_id),
                        key_hash.to_value(),
                        sentinel.to_value(),
                    ];
                    let record_values = vec![
                        Value::from_i64(storage_id),
                        key_hash.to_value(),
                        sentinel.to_value(),
                        Value::Null,
                    ];

                    let next_idx = *idx + 1;
                    return_and_restore_if_io!(
                        &mut self.commit_state,
                        state,
                        write_row.write_row(cursors, index_key, record_values, delta_count)
                    );
                    self.commit_state = MatchCounterCommitState::PersistRCount {
                        deltas: std::mem::take(deltas),
                        output: std::mem::take(output),
                        key_deltas: std::mem::take(key_deltas),
                        idx: next_idx,
                        write_row: WriteRow::new(),
                    };
                }
                MatchCounterCommitState::Invalid => {
                    // Recover from a prior panic during commit (e.g. BTree
                    // corruption during chained matview cascades). Reset to
                    // Idle so subsequent IVM updates can proceed.
                    tracing::warn!(
                        "[MatchCounterOperator::commit] Recovering from Invalid state. \
                         Resetting to Idle."
                    );
                    self.commit_state = MatchCounterCommitState::Idle;
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

// Silence unused import warning when blob deserialization is unreachable
// in the reduced state machine paths exercised by initial tests.
#[allow(dead_code)]
const _UNUSED: fn(&[u8]) -> Result<HashableRow> = deserialize_l_row;
