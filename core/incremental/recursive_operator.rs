//! Recursive operator for DBSP fixed-point computation
//!
//! This operator wraps a recursive sub-circuit and iterates until a fixed-point is reached.
//! It implements the semantics described in the DBSP paper for incremental evaluation of
//! recursive queries.
//!
//! The fixed-point algorithm:
//! 1. Execute the base case to get initial values
//! 2. Initialize the delay operator with base case results
//! 3. Iterate:
//!    a. Execute the recursive step (reads from delay operator)
//!    b. If the result is empty, we've reached a fixed-point
//!    c. Otherwise, accumulate results and update delay operator
//!    d. Repeat until fixed-point or max iterations

use crate::incremental::dbsp::{Delta, DeltaPair, Hash128, HashableRow};
use crate::incremental::operator::{ComputationTracker, DbspStateCursors, EvalState};
use crate::sync::Mutex;
use crate::types::IOResult;
use crate::Result;
use std::collections::HashMap;
use std::fmt::{self, Debug, Display};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Process-global count of times a UNION-ALL retraction failed to find the rowid
/// its value was originally inserted under, forcing the operator to invent a
/// fresh rowid whose `-1` cancels nothing. This is an invariant violation: a
/// retraction of a row the operator itself emitted must always match a tracked
/// rowid. Tests read/reset it via `take_recursive_restore_fallback_count()` to
/// assert that a reopen + retraction never falls through to an invented rowid.
static RECURSIVE_RETRACT_FALLBACK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Read and reset the process-global count of invariant-violating retraction
/// fallbacks in the recursive UNION-ALL operator (see
/// `RECURSIVE_RETRACT_FALLBACK_COUNT`).
pub fn take_recursive_restore_fallback_count() -> u64 {
    RECURSIVE_RETRACT_FALLBACK_COUNT.swap(0, Ordering::SeqCst)
}

use super::operator::IncrementalOperator;

/// State machine for recursive fixed-point iteration
#[derive(Debug, Clone, PartialEq)]
pub enum RecursiveState {
    /// Starting a new fixed-point computation
    Init,
    /// Base case has been executed, ready for recursive iterations
    BaseComplete,
    /// Running recursive step iteration
    Iterating { iteration: usize },
    /// Fixed-point reached or max iterations hit
    Done,
}

/// Configuration for the recursive operator
#[derive(Debug, Clone)]
pub struct RecursiveConfig {
    /// Maximum number of iterations (prevents infinite loops)
    pub max_iterations: usize,
    /// Whether using UNION ALL (no deduplication) or UNION (distinct)
    pub union_all: bool,
    /// Maximum number of rows to track in deduplication state (0 = unlimited)
    /// When exceeded, returns an error rather than potentially corrupting data.
    pub max_dedup_rows: usize,
}

impl Default for RecursiveConfig {
    fn default() -> Self {
        Self {
            max_iterations: 100,
            union_all: false, // UNION (distinct) is safer default for recursion
            max_dedup_rows: 1_000_000, // 1M rows default limit
        }
    }
}

/// Recursive operator that computes the fixed-point of a recursive query
///
/// This operator coordinates the fixed-point iteration:
/// - Executes the base case once
/// - Iterates the recursive step until no new values are produced
/// - Accumulates all results into the final output
pub struct RecursiveOperator {
    operator_id: i64,
    /// Name of the recursive CTE, used to identify it in error messages
    name: String,
    /// Configuration for the recursion
    config: RecursiveConfig,
    /// Current state of the recursion
    state: RecursiveState,
    /// Accumulated output from all iterations
    accumulated_output: Delta,
    /// Value hash -> canonical rowid for UNION distinct recursion
    seen_rows: HashMap<u64, i64>,
    /// Value hash -> net multiplicity for UNION distinct recursion
    seen_counts: HashMap<u64, isize>,
    /// Value hash -> stack of assigned rowids for UNION ALL mode.
    /// Inserts push, deletes pop — ensures delete deltas reference the same rowid
    /// that was assigned when the row was originally inserted.
    union_all_rowids: HashMap<u64, Vec<i64>>,
    /// Next rowid to assign for canonicalized rows
    next_rowid: i64,
    /// True once this operator's maps were reloaded from a persisted state blob
    /// (see `restore_from_persisted_state`). When true, the rowid bookkeeping is
    /// authoritative: a retraction that fails to find its rowid is a genuine
    /// invariant violation and is raised as an error rather than silently
    /// inventing a rowid. When false (fresh in-process operator, or a matview
    /// created before persistence existed) a miss is downgraded to a logged
    /// counter so pre-existing behavior is preserved.
    restored_from_persisted: bool,
}

/// op_type bits for `generate_storage_id` identifying a recursive operator's
/// persisted-state row. Distinct from the aggregate op-types (0b00/0b01/0b10);
/// operator_ids are unique per circuit node so this never collides with another
/// operator's rows regardless, but a dedicated code keeps the namespace clear.
pub const RECURSIVE_STATE_TYPE: u8 = 0b11;

/// Version tag for the serialized recursive-operator state blob.
const RECURSIVE_STATE_BLOB_VERSION: u8 = 1;

/// Minimal little-endian byte reader for deserializing the state blob. Every read
/// is bounds-checked and a short/corrupt blob surfaces as an `InternalError`
/// rather than a panic.
struct ByteReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.buf.len());
        match end {
            Some(end) => {
                let slice = &self.buf[self.pos..end];
                self.pos = end;
                Ok(slice)
            }
            None => Err(crate::LimboError::InternalError(
                "recursive CTE persisted state blob is truncated".to_string(),
            )),
        }
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn read_u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn read_i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

/// Result of processing a recursive step
#[derive(Debug)]
pub struct RecursiveStepResult {
    pub done: bool,
    pub delta_for_delay: Delta,
}

/// Snapshot of RecursiveOperator state for save/restore around read-only execution.
pub struct RecursiveOperatorSnapshot {
    state: RecursiveState,
    accumulated_output: Delta,
    seen_rows: HashMap<u64, i64>,
    seen_counts: HashMap<u64, isize>,
    union_all_rowids: HashMap<u64, Vec<i64>>,
    next_rowid: i64,
    restored_from_persisted: bool,
}

impl RecursiveOperator {
    pub fn save_snapshot(&self) -> RecursiveOperatorSnapshot {
        RecursiveOperatorSnapshot {
            state: self.state.clone(),
            accumulated_output: self.accumulated_output.clone(),
            seen_rows: self.seen_rows.clone(),
            seen_counts: self.seen_counts.clone(),
            union_all_rowids: self.union_all_rowids.clone(),
            next_rowid: self.next_rowid,
            restored_from_persisted: self.restored_from_persisted,
        }
    }

    pub fn restore_snapshot(&mut self, snapshot: RecursiveOperatorSnapshot) {
        self.state = snapshot.state;
        self.accumulated_output = snapshot.accumulated_output;
        self.seen_rows = snapshot.seen_rows;
        self.seen_counts = snapshot.seen_counts;
        self.union_all_rowids = snapshot.union_all_rowids;
        self.next_rowid = snapshot.next_rowid;
        self.restored_from_persisted = snapshot.restored_from_persisted;
    }

    pub fn new(
        operator_id: i64,
        name: impl Into<String>,
        max_iterations: usize,
        union_all: bool,
    ) -> Self {
        Self {
            operator_id,
            name: name.into(),
            config: RecursiveConfig {
                max_iterations,
                union_all,
                ..Default::default()
            },
            state: RecursiveState::Init,
            accumulated_output: Delta::new(),
            seen_rows: HashMap::default(),
            seen_counts: HashMap::default(),
            union_all_rowids: HashMap::default(),
            next_rowid: 1,
            restored_from_persisted: false,
        }
    }

    /// Create a new recursive operator with custom memory limits
    #[allow(dead_code)]
    pub fn with_limits(
        operator_id: i64,
        name: impl Into<String>,
        max_iterations: usize,
        union_all: bool,
        max_dedup_rows: usize,
    ) -> Self {
        Self {
            operator_id,
            name: name.into(),
            config: RecursiveConfig {
                max_iterations,
                union_all,
                max_dedup_rows,
            },
            state: RecursiveState::Init,
            accumulated_output: Delta::new(),
            seen_rows: HashMap::default(),
            seen_counts: HashMap::default(),
            union_all_rowids: HashMap::default(),
            next_rowid: 1,
            restored_from_persisted: false,
        }
    }

    /// Serialize the operator's rowid-bookkeeping state to a byte blob for
    /// persistence in the DBSP internal-state btree.
    ///
    /// This is the ONLY faithful source for restore across a reopen: the maps are
    /// keyed by the hash of the operator's INTERNAL recursive tuple, which is not
    /// recoverable from the matview's OUTER-projection output btree (different
    /// arity and columns). Encoding is deterministic — hash-keyed maps are emitted
    /// in ascending key order — and, crucially, the per-hash rowid vectors keep
    /// their exact order, because that order is pop-order (stack) semantics for
    /// UNION ALL retraction matching, not set membership.
    pub fn serialize_state(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(RECURSIVE_STATE_BLOB_VERSION);
        buf.push(u8::from(self.config.union_all));
        buf.extend_from_slice(&self.next_rowid.to_le_bytes());

        let mut union_entries: Vec<(&u64, &Vec<i64>)> = self.union_all_rowids.iter().collect();
        union_entries.sort_by_key(|(h, _)| **h);
        buf.extend_from_slice(&(union_entries.len() as u64).to_le_bytes());
        for (hash, rowids) in union_entries {
            buf.extend_from_slice(&hash.to_le_bytes());
            buf.extend_from_slice(&(rowids.len() as u64).to_le_bytes());
            for rowid in rowids {
                buf.extend_from_slice(&rowid.to_le_bytes());
            }
        }

        let mut seen_rows: Vec<(&u64, &i64)> = self.seen_rows.iter().collect();
        seen_rows.sort_by_key(|(h, _)| **h);
        buf.extend_from_slice(&(seen_rows.len() as u64).to_le_bytes());
        for (hash, rowid) in seen_rows {
            buf.extend_from_slice(&hash.to_le_bytes());
            buf.extend_from_slice(&rowid.to_le_bytes());
        }

        let mut seen_counts: Vec<(&u64, &isize)> = self.seen_counts.iter().collect();
        seen_counts.sort_by_key(|(h, _)| **h);
        buf.extend_from_slice(&(seen_counts.len() as u64).to_le_bytes());
        for (hash, count) in seen_counts {
            buf.extend_from_slice(&hash.to_le_bytes());
            buf.extend_from_slice(&(*count as i64).to_le_bytes());
        }

        buf
    }

    /// Restore the operator's rowid-bookkeeping state from a blob produced by
    /// `serialize_state`. Sets `restored_from_persisted` so a subsequent
    /// retraction miss is treated as a hard invariant violation.
    pub fn restore_from_persisted_state(&mut self, blob: &[u8]) -> Result<()> {
        let mut cur = ByteReader::new(blob);
        let version = cur.read_u8()?;
        if version != RECURSIVE_STATE_BLOB_VERSION {
            return Err(crate::LimboError::InternalError(format!(
                "recursive CTE '{}' persisted state has unsupported version {version}",
                self.name
            )));
        }
        let _union_all = cur.read_u8()? != 0;
        self.next_rowid = cur.read_i64()?;

        self.union_all_rowids.clear();
        let n_union = cur.read_u64()?;
        for _ in 0..n_union {
            let hash = cur.read_u64()?;
            let len = cur.read_u64()?;
            let mut rowids = Vec::with_capacity(len as usize);
            for _ in 0..len {
                rowids.push(cur.read_i64()?);
            }
            self.union_all_rowids.insert(hash, rowids);
        }

        self.seen_rows.clear();
        let n_seen = cur.read_u64()?;
        for _ in 0..n_seen {
            let hash = cur.read_u64()?;
            let rowid = cur.read_i64()?;
            self.seen_rows.insert(hash, rowid);
        }

        self.seen_counts.clear();
        let n_counts = cur.read_u64()?;
        for _ in 0..n_counts {
            let hash = cur.read_u64()?;
            let count = cur.read_i64()? as isize;
            self.seen_counts.insert(hash, count);
        }

        self.restored_from_persisted = true;
        Ok(())
    }

    pub fn needs_state_restore(&self) -> bool {
        // If next_rowid is 1 and state is Init, the operator hasn't been used yet
        // and may need restoration from btree data if the view was loaded from disk
        self.next_rowid == 1
            && matches!(self.state, RecursiveState::Init)
            && self.union_all_rowids.is_empty()
            && self.seen_rows.is_empty()
    }

    /// Reset state for a new transaction/computation.
    /// Clears all accumulated state while preserving configuration.
    #[allow(dead_code)]
    pub fn reset_for_new_transaction(&mut self) {
        self.state = RecursiveState::Init;
        self.accumulated_output = Delta::new();
        self.seen_rows.clear();
        self.seen_counts.clear();
        self.union_all_rowids.clear();
        self.next_rowid = 1;
        // Cleared maps must be reloaded from the persisted blob before they are
        // authoritative again; drop the flag so `needs_state_restore()` fires and
        // a miss before the reload is not mistaken for an invariant violation.
        self.restored_from_persisted = false;
    }

    /// Get the current state
    pub fn state(&self) -> &RecursiveState {
        &self.state
    }

    /// The operator's stable id, used to key its persisted state row in the
    /// DBSP internal-state btree (see `serialize_state`).
    pub fn operator_id(&self) -> i64 {
        self.operator_id
    }

    /// Initialize with base case result.
    /// Returns an error if memory limits are exceeded.
    pub fn initialize_with_base(&mut self, base_delta: Delta) -> Result<Delta> {
        self.state = RecursiveState::Init;
        self.accumulated_output = Delta::new();

        let normalized = self.normalize_delta(base_delta)?;

        let mut filtered = self.filter_new_rows(normalized);
        if !self.config.union_all {
            filtered.consolidate();
        }

        // Accumulate base case results
        self.accumulated_output = filtered.clone();

        self.state = RecursiveState::BaseComplete;
        Ok(filtered)
    }

    /// Process the result of a recursive step iteration.
    /// Returns an error if memory limits are exceeded.
    /// Returns Ok(RecursiveStepResult) with done=true if fixed-point was reached or max iterations hit.
    pub fn process_iteration_result(&mut self, step_delta: Delta) -> Result<RecursiveStepResult> {
        let iteration = match &self.state {
            RecursiveState::Iterating { iteration } => *iteration,
            _ => 1,
        };

        // Breaching the guard means the recursion did not converge. Reporting `done` here would
        // hand `finalize()` to the caller, which writes the rows accumulated so far into the
        // matview's btree as though the fixed point had been reached -- a durable wrong answer
        // that no downstream reader can distinguish from a converged one. Raise instead, matching
        // the runaway-recursion error the non-incremental path emits, so the guard only decides
        // when we give up, never whether the view's contents are correct.
        //
        // The reset drops the partial accumulation of this failed run. Leaving the operator
        // mid-`Iterating` would make the next execution resume from the breached iteration and
        // fail forever; from `Init` it re-runs the base case, and `needs_state_restore()` becomes
        // true so the committed btree state is reloaded first.
        if iteration >= self.config.max_iterations {
            let max_iterations = self.config.max_iterations;
            let name = self.name.clone();
            self.reset_for_new_transaction();
            return Err(crate::LimboError::InternalError(format!(
                "recursive CTE '{name}' did not converge within {max_iterations} iterations"
            )));
        }

        let normalized = self.normalize_delta(step_delta)?;

        // Consolidate new delta before merging to avoid redundant work
        let mut filtered = self.filter_new_rows(normalized);
        if !self.config.union_all {
            filtered.consolidate();
        }

        // Check for fixed-point (empty delta after consolidation)
        if filtered.is_empty() {
            self.state = RecursiveState::Done;
            return Ok(RecursiveStepResult {
                done: true,
                delta_for_delay: Delta::new(),
            });
        }

        // Accumulate new results - merge only the consolidated delta
        self.accumulated_output.merge(&filtered);

        // Move to next iteration
        self.state = RecursiveState::Iterating {
            iteration: iteration + 1,
        };

        Ok(RecursiveStepResult {
            done: false,
            delta_for_delay: filtered,
        })
    }

    /// Start iteration phase (after base case is complete)
    pub fn start_iteration(&mut self) {
        assert!(matches!(self.state, RecursiveState::BaseComplete));
        self.state = RecursiveState::Iterating { iteration: 1 };
    }

    /// Finalize the result
    /// Consolidates if not UNION ALL
    pub fn finalize(&mut self) -> Delta {
        let mut result = std::mem::take(&mut self.accumulated_output);
        if !self.config.union_all {
            result.consolidate();
        }
        self.state = RecursiveState::Done;
        result
    }

    fn normalize_delta(&mut self, delta: Delta) -> Result<Delta> {
        if self.config.union_all {
            // For UNION ALL, we must assign unique rowids to inserts while ensuring
            // that delete deltas reference the SAME rowid as the original insert.
            // Without this, incremental updates (UPDATE/DELETE) would create new btree
            // entries with negative weights instead of canceling existing ones.
            //
            // BASE CASE vs RECURSIVE STEP:
            //
            // At the base case (Init state), entries arrive directly from
            // the base table delta via the InputOperator. A -1/+1 pair at
            // the same input rowid represents a real base-table UPDATE
            // whose projected values happen to be identical (the changed
            // column isn't in the CTE projection).  These MUST survive to
            // the outer projection layer, which restores the
            // differentiating columns via the final JOIN+SELECT.
            //
            // At the recursive step (Iterating state), entries arrive from
            // the JoinOperator's three-way join (δL⋈R + L⋈δR + δL⋈δR).
            // This can emit the same logical row from different algebra
            // terms as a +1/-1 pair with the SAME input rowid.
            // Per-input-rowid consolidation cancels these formula
            // duplicates.  A genuine propagated update has its -1 and +1
            // entries at DIFFERENT input rowids (L_prev vs δL), so the
            // grouping leaves them untouched.
            let is_base_case = matches!(self.state, RecursiveState::Init);

            let consolidated: Vec<(i64, super::dbsp::RowValues, isize)>;
            if is_base_case {
                consolidated = delta
                    .changes
                    .into_iter()
                    .map(|(r, w)| (r.rowid, r.values, w))
                    .collect();
            } else {
                // Consolidate while preserving first-occurrence input order.
                // Order matters: the rowid-reuse stack below pops on deletes
                // and pushes on inserts, so a delete must see the state the
                // input ordering implies, not HashMap iteration order.
                let mut entries: Vec<(i64, super::dbsp::RowValues, isize)> = Vec::new();
                let mut index: HashMap<(i64, u64), usize> = HashMap::new();
                for (row, weight) in delta.changes {
                    let vh = Hash128::hash_values(&row.values).as_u64();
                    match index.entry((row.rowid, vh)) {
                        std::collections::hash_map::Entry::Occupied(e) => {
                            entries[*e.get()].2 += weight;
                        }
                        std::collections::hash_map::Entry::Vacant(e) => {
                            e.insert(entries.len());
                            entries.push((row.rowid, row.values, weight));
                        }
                    }
                }
                consolidated = entries.into_iter().filter(|(_, _, w)| *w != 0).collect();
            }

            let mut output = Delta::new();
            for (_input_rowid, values, weight) in consolidated {
                if weight > 0 {
                    for _ in 0..weight {
                        let rowid = self.next_rowid;
                        self.next_rowid += 1;
                        let value_hash = Hash128::hash_values(&values).as_u64();
                        self.union_all_rowids
                            .entry(value_hash)
                            .or_default()
                            .push(rowid);
                        output
                            .changes
                            .push((HashableRow::new(rowid, values.clone()), 1));
                    }
                } else if weight < 0 {
                    for _ in 0..(-weight) {
                        let value_hash = Hash128::hash_values(&values).as_u64();
                        let rowid = if let Some(rowids) = self.union_all_rowids.get_mut(&value_hash)
                        {
                            if let Some(rowid) = rowids.pop() {
                                if rowids.is_empty() {
                                    self.union_all_rowids.remove(&value_hash);
                                }
                                rowid
                            } else {
                                self.on_retract_fallback("empty rowid stack")?;
                                let rowid = self.next_rowid;
                                self.next_rowid += 1;
                                rowid
                            }
                        } else {
                            self.on_retract_fallback("no rowid entry")?;
                            let rowid = self.next_rowid;
                            self.next_rowid += 1;
                            rowid
                        };
                        output
                            .changes
                            .push((HashableRow::new(rowid, values.clone()), -1));
                    }
                }
            }
            return Ok(output);
        }

        let mut output = Delta::new();
        for (row, weight) in delta.changes {
            // We use a 128-bit hash of the row values (truncated to u64) to assign
            // stable row IDs for deduplication in non-UNION-ALL mode.
            //
            // HASH COLLISION RISK: In theory, a hash collision could cause two distinct
            // rows to share the same row ID and thus be incorrectly merged. We accept this
            // trade-off because Hash128 provides a very large hash space (~2^64 after
            // truncation), making collisions extremely unlikely in practice (~1 in 2^64
            // for any pair, birthday paradox reaches 50% collision probability only at
            // ~4 billion rows), while keeping the deduplication state compact.
            let value_hash = Hash128::hash_values(&row.values).as_u64();
            let assigned_rowid = if let Some(&rowid) = self.seen_rows.get(&value_hash) {
                rowid
            } else {
                self.check_memory_limit()?;
                let rowid = self.next_rowid;
                self.next_rowid += 1;
                self.seen_rows.insert(value_hash, rowid);
                rowid
            };
            let final_row = HashableRow::new(assigned_rowid, row.values);
            output.changes.push((final_row, weight));
        }

        Ok(output)
    }

    /// Filter rows to only emit new appearances/disappearances for UNION semantics.
    /// Uses the same hash-based deduplication as normalize_delta (see comment there).
    fn filter_new_rows(&mut self, delta: Delta) -> Delta {
        if self.config.union_all {
            return delta;
        }

        let mut output = Delta::new();
        for (row, weight) in delta.changes {
            if weight == 0 {
                continue;
            }
            let value_hash = Hash128::hash_values(&row.values).as_u64();
            let previous = *self.seen_counts.get(&value_hash).unwrap_or(&0);
            let new_count = previous + weight;
            let was_present = previous > 0;
            let is_present = new_count > 0;

            if !was_present && is_present {
                output.changes.push((row, 1));
            } else if was_present && !is_present {
                output.changes.push((row, -1));
            }

            if new_count == 0 {
                self.seen_counts.remove(&value_hash);
                self.seen_rows.remove(&value_hash);
            } else {
                self.seen_counts.insert(value_hash, new_count);
            }
        }

        output
    }

    /// A UNION-ALL retraction could not be matched to the rowid its value was
    /// originally inserted under. This must never happen for a row the operator
    /// itself emitted: it means `union_all_rowids` is out of sync with the rows
    /// already committed downstream.
    ///
    /// When the operator's state was reloaded from a persisted blob
    /// (`restored_from_persisted`), the bookkeeping is authoritative, so a miss is
    /// a genuine invariant violation and is raised as an error — matching the
    /// runaway-recursion error above, and never silently absorbed into a `-1`
    /// that cancels nothing. When the state was NOT restored from a blob (a fresh
    /// in-process operator, where the separately-tracked in-process retraction
    /// bug can rarely trip this, or a matview created before persistence existed)
    /// the miss is downgraded to a logged counter so pre-existing behavior is
    /// preserved. The counter is bumped in both cases for test observability.
    fn on_retract_fallback(&self, reason: &str) -> Result<()> {
        RECURSIVE_RETRACT_FALLBACK_COUNT.fetch_add(1, Ordering::SeqCst);
        if self.restored_from_persisted {
            return Err(crate::LimboError::InternalError(format!(
                "recursive CTE '{}' retraction found no rowid to cancel ({reason}) after \
                 restoring persisted state; union_all_rowids is out of sync with committed rows",
                self.name
            )));
        }
        tracing::error!(
            "recursive CTE '{}' UNION-ALL retraction fell through to an invented \
             rowid ({reason}); the retraction will not cancel its original row. \
             This indicates union_all_rowids is out of sync.",
            self.name
        );
        Ok(())
    }

    /// Check if memory limits would be exceeded by adding more rows
    fn check_memory_limit(&self) -> Result<()> {
        if self.config.max_dedup_rows > 0 && self.seen_rows.len() >= self.config.max_dedup_rows {
            return Err(crate::LimboError::InternalError(format!(
                "Recursive CTE exceeded maximum deduplication row limit ({}). \
                 Consider using UNION ALL or increasing the limit.",
                self.config.max_dedup_rows
            )));
        }
        Ok(())
    }
}

impl Debug for RecursiveOperator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecursiveOperator")
            .field("operator_id", &self.operator_id)
            .field("state", &self.state)
            .field("max_iterations", &self.config.max_iterations)
            .field("union_all", &self.config.union_all)
            .field("accumulated_rows", &self.accumulated_output.changes.len())
            .finish()
    }
}

impl Display for RecursiveOperator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "RecursiveOperator({}, state={:?})",
            self.operator_id, self.state
        )
    }
}

impl IncrementalOperator for RecursiveOperator {
    fn eval(
        &mut self,
        state: &mut EvalState,
        _cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Delta>> {
        // The RecursiveOperator's eval returns the accumulated output
        // The actual fixed-point iteration is handled by execute_recursive_node()
        match state {
            EvalState::Init { .. } => {
                *state = EvalState::Done;
                Ok(IOResult::Done(self.accumulated_output.clone()))
            }
            EvalState::Done => Ok(IOResult::Done(Delta::new())),
            _ => unreachable!("RecursiveOperator only handles Init state"),
        }
    }

    fn commit(
        &mut self,
        deltas: DeltaPair,
        _cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Delta>> {
        // During commit, we receive the final accumulated output
        // This is called after fixed-point iteration completes
        self.accumulated_output.merge(&deltas.left);
        if !self.config.union_all {
            self.accumulated_output.consolidate();
        }
        Ok(IOResult::Done(self.accumulated_output.clone()))
    }

    fn set_tracker(&mut self, _tracker: Arc<Mutex<ComputationTracker>>) {
        // RecursiveOperator doesn't need computation tracking
        // (the sub-operators handle their own tracking)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::incremental::dbsp::HashableRow;
    use crate::Value;

    #[test]
    fn test_recursive_operator_basic() {
        let mut op = RecursiveOperator::new(1, "walk", 100, false);

        assert!(matches!(op.state(), RecursiveState::Init));

        // Initialize with base case
        let mut base = Delta::new();
        base.changes
            .push((HashableRow::new(1, vec![Value::from_i64(1)]), 1));
        base.changes
            .push((HashableRow::new(2, vec![Value::from_i64(2)]), 1));
        let base_delta = op.initialize_with_base(base).unwrap();

        assert!(matches!(op.state(), RecursiveState::BaseComplete));
        assert_eq!(op.accumulated_output.changes.len(), 2);
        assert_eq!(base_delta.changes.len(), 2);
    }

    #[test]
    fn test_recursive_operator_iteration() {
        let mut op = RecursiveOperator::new(1, "walk", 100, false);

        // Initialize with base case
        let mut base = Delta::new();
        base.changes
            .push((HashableRow::new(1, vec![Value::from_i64(1)]), 1));
        op.initialize_with_base(base).unwrap();

        // Start iteration
        op.start_iteration();
        assert!(matches!(
            op.state(),
            RecursiveState::Iterating { iteration: 1 }
        ));

        // Process first iteration result (not empty)
        let mut step1 = Delta::new();
        step1
            .changes
            .push((HashableRow::new(2, vec![Value::from_i64(2)]), 1));
        let result = op.process_iteration_result(step1).unwrap();
        assert!(!result.done);
        assert!(matches!(
            op.state(),
            RecursiveState::Iterating { iteration: 2 }
        ));
        assert_eq!(op.accumulated_output.changes.len(), 2);

        // Process second iteration result (empty = fixed-point)
        let step2 = Delta::new();
        let result = op.process_iteration_result(step2).unwrap();
        assert!(result.done);
        assert!(matches!(op.state(), RecursiveState::Done));
    }

    /// Breaching the iteration guard must be an error, not a `done` that hands the caller a
    /// truncated `finalize()` to persist. It must also leave the operator resettable so the
    /// next attempt re-runs the base case instead of resuming the breached iteration.
    #[test]
    fn test_recursive_operator_max_iterations_errors_instead_of_truncating() {
        let mut op = RecursiveOperator::new(1, "walk", 3, false);

        let mut base = Delta::new();
        base.changes
            .push((HashableRow::new(1, vec![Value::from_i64(1)]), 1));
        op.initialize_with_base(base).unwrap();

        op.start_iteration();

        // Simulate iterations that never converge
        for i in 1..=3 {
            let mut step = Delta::new();
            step.changes.push((
                HashableRow::new(i as i64 + 1, vec![Value::from_i64(i as i64 + 1)]),
                1,
            ));
            let result = op.process_iteration_result(step);

            if i < 3 {
                assert!(!result.unwrap().done, "Should not be done at iteration {i}");
            } else {
                let err = result.expect_err("guard breach must surface as an error");
                let msg = err.to_string();
                assert!(
                    msg.contains("did not converge") && msg.contains("walk"),
                    "unexpected error message: {msg}"
                );
                assert!(matches!(op.state(), RecursiveState::Init));
                assert!(op.accumulated_output.changes.is_empty());
            }
        }
    }

    #[test]
    fn test_recursive_operator_consolidation() {
        let mut op = RecursiveOperator::new(1, "walk", 100, false);

        // Initialize with base case
        let mut base = Delta::new();
        base.changes
            .push((HashableRow::new(1, vec![Value::from_i64(1)]), 1));
        op.initialize_with_base(base).unwrap();

        op.start_iteration();

        // Add same row again (should consolidate)
        let mut step = Delta::new();
        step.changes
            .push((HashableRow::new(1, vec![Value::from_i64(1)]), 1)); // Duplicate
        step.changes
            .push((HashableRow::new(2, vec![Value::from_i64(2)]), 1));
        op.process_iteration_result(step).unwrap();

        // Finalize with consolidation
        let result = op.finalize();
        assert!(result.changes.len() <= 3); // May have fewer after consolidation
    }

    #[test]
    fn test_recursive_operator_reset() {
        let mut op = RecursiveOperator::new(1, "walk", 100, false);

        // Initialize and do some work
        let mut base = Delta::new();
        base.changes
            .push((HashableRow::new(1, vec![Value::from_i64(1)]), 1));
        op.initialize_with_base(base).unwrap();
        op.start_iteration();

        let mut step = Delta::new();
        step.changes
            .push((HashableRow::new(2, vec![Value::from_i64(2)]), 1));
        op.process_iteration_result(step).unwrap();

        // Reset for new transaction
        op.reset_for_new_transaction();

        assert!(matches!(op.state(), RecursiveState::Init));
        assert!(op.accumulated_output.changes.is_empty());
        assert!(op.seen_rows.is_empty());
        assert!(op.seen_counts.is_empty());
    }

    #[test]
    fn test_recursive_operator_memory_limit() {
        let mut op = RecursiveOperator::with_limits(1, "walk", 100, false, 2);

        // Initialize with one row
        let mut base = Delta::new();
        base.changes
            .push((HashableRow::new(1, vec![Value::from_i64(1)]), 1));
        op.initialize_with_base(base).unwrap();

        op.start_iteration();

        // Add one more row (should succeed, at limit)
        let mut step1 = Delta::new();
        step1
            .changes
            .push((HashableRow::new(2, vec![Value::from_i64(2)]), 1));
        op.process_iteration_result(step1).unwrap();

        // Add another row (should fail, exceeds limit)
        let mut step2 = Delta::new();
        step2
            .changes
            .push((HashableRow::new(3, vec![Value::from_i64(3)]), 1));
        let result = op.process_iteration_result(step2);
        assert!(result.is_err());
    }

    #[test]
    fn test_recursive_operator_seen_rows_cleanup() {
        let mut op = RecursiveOperator::new(1, "walk", 100, false);

        // Initialize with a row
        let mut base = Delta::new();
        base.changes
            .push((HashableRow::new(1, vec![Value::from_i64(42)]), 1));
        op.initialize_with_base(base).unwrap();

        op.start_iteration();

        // Add then remove the same row
        let mut step1 = Delta::new();
        step1
            .changes
            .push((HashableRow::new(1, vec![Value::from_i64(42)]), -1));
        op.process_iteration_result(step1).unwrap();

        // Verify that seen_rows was cleaned up when count reached 0
        let hash = Hash128::hash_values(&[Value::from_i64(42)]).as_u64();
        assert!(
            !op.seen_counts.contains_key(&hash),
            "seen_counts should be cleaned up"
        );
        assert!(
            !op.seen_rows.contains_key(&hash),
            "seen_rows should be cleaned up when count reaches 0"
        );
    }

    /// When a base table row is UPDATEd (content changed, but the columns
    /// projected by the recursive CTE's intermediate layers are unchanged),
    /// the input delta carries a retraction (-1) and insertion (+1) for the
    /// same projected values.  `normalize_delta` must NOT cancel these to
    /// zero — the pair must survive to the final projection layer where the
    /// differentiating columns (e.g. content) are restored.
    ///
    /// This reproduces the Holon split_block staleness bug: pressing Enter
    /// truncates the original block's content in SQL, but the UI's matview
    /// CDC stream never emits an Updated event.
    #[test]
    fn test_normalize_delta_preserves_update_pair_for_union_all() {
        let mut op = RecursiveOperator::new(1, "walk", 100, /* union_all */ true);

        // Seed the operator state as if a previous insert stored this value.
        let mut base = Delta::new();
        base.changes.push((
            HashableRow::new(1, vec![Value::from_text("A"), Value::from_i64(0)]),
            1,
        ));
        let _base_delta = op.initialize_with_base(base).unwrap();

        // Delta after a base-table UPDATE: the old and new versions of the
        // row have the SAME projection values (the CTE layer only sees id
        // and depth, not content), so they hash to the same key.
        let mut update_delta = Delta::new();
        update_delta.changes.push((
            HashableRow::new(10, vec![Value::from_text("A"), Value::from_i64(0)]),
            -1, // retraction of old projected row
        ));
        update_delta.changes.push((
            HashableRow::new(11, vec![Value::from_text("A"), Value::from_i64(0)]),
            1, // insertion of new projected row
        ));

        let normalized = op.normalize_delta(update_delta).unwrap();

        // Both entries must survive: the retraction uses the existing rowid
        // from union_all_rowids, the insertion gets a new one.
        let deletes: Vec<_> = normalized.changes.iter().filter(|(_, w)| *w < 0).collect();
        let inserts: Vec<_> = normalized.changes.iter().filter(|(_, w)| *w > 0).collect();

        assert_eq!(
            deletes.len(),
            1,
            "retraction of the old projected row must survive; \
             got {normalized:?}"
        );
        assert_eq!(
            inserts.len(),
            1,
            "insertion of the new projected row must survive; \
             got {normalized:?}"
        );
        assert_eq!(
            deletes[0].0.rowid, 1,
            "delete must reuse the original rowid from union_all_rowids"
        );
        assert!(inserts[0].0.rowid > 1, "insert must get a fresh rowid");
    }
}
