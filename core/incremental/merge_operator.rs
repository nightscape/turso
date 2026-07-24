// Merge operator for DBSP - combines two delta streams
// Used in recursive CTEs and UNION operations

use crate::incremental::dbsp::{Delta, DeltaPair, Hash128, HashableRow};
use crate::incremental::operator::{
    ComputationTracker, DbspStateCursors, EvalState, IncrementalOperator,
};
use crate::sync::Arc;
use crate::sync::Mutex;
use crate::types::IOResult;
use crate::{Result, Value};
use std::fmt::{self, Display};

/// How the merge operator should handle rowids when combining deltas
#[derive(Debug, Clone)]
pub enum UnionMode {
    /// For UNION (distinct) - hash values only to merge duplicates
    Distinct,
    /// For UNION ALL - include source table name in hash to keep duplicates separate
    All {
        left_table: String,
        right_table: String,
    },
}

/// Merge operator that combines two input deltas into one output delta
/// Handles both recursive CTEs and UNION/UNION ALL operations
///
/// Output rowids MUST be a pure function of the input row (and, for UNION
/// ALL, its source), never of session history. Merge outputs feed matview
/// btrees keyed by rowid, and those btrees outlive the process: a
/// retraction issued after a database reopen must serialize to the exact
/// rowid persisted before the reopen, or WriteRowView silently no-ops the
/// delete (leaving a duplicate) or decrements a DIFFERENT row that happens
/// to occupy the stale sequential rowid (losing it). The previous
/// implementation assigned rowids from an in-memory `seen_rows` map plus a
/// `next_rowid` counter, which reset on reopen and caused exactly that
/// corruption (holon BugFunnel row 90).
#[derive(Debug)]
pub struct MergeOperator {
    operator_id: i64,
    union_mode: UnionMode,
}

impl MergeOperator {
    /// Create a new merge operator with specified union mode
    pub fn new(operator_id: i64, mode: UnionMode) -> Self {
        Self {
            operator_id,
            union_mode: mode,
        }
    }

    /// Transform a delta's rowids based on the union mode.
    ///
    /// Rowid derivation is stateless and deterministic (Hash128 is SHA-1
    /// based, byte-stable across processes):
    /// - UNION (distinct): rowid = hash of the row VALUES, so identical
    ///   rows from either side collapse onto one rowid.
    /// - UNION ALL: rowid = hash of (source table tag, original rowid), so
    ///   equal rows from different sources (or distinct rows of one
    ///   source) stay separate, while insert/retract pairs for the same
    ///   source row always meet on the same output rowid.
    fn transform_delta(&mut self, delta: Delta, is_left: bool) -> Delta {
        match &self.union_mode {
            UnionMode::Distinct => {
                let mut output = Delta::new();
                for (row, weight) in delta.changes {
                    // Hash only the values (not rowid) for deduplication
                    let temp_row = HashableRow::new(0, row.values.clone());
                    let assigned_rowid = temp_row.cached_hash().as_i64();
                    let final_row = HashableRow::new(assigned_rowid, temp_row.values);
                    output.changes.push((final_row, weight));
                }
                output
            }
            UnionMode::All {
                left_table,
                right_table,
            } => {
                let table = if is_left { left_table } else { right_table };
                let source_tag = Value::from_text(table.clone());

                let mut output = Delta::new();
                for (row, weight) in delta.changes {
                    let assigned_rowid =
                        Hash128::hash_values(&[source_tag.clone(), Value::from_i64(row.rowid)])
                            .as_i64();
                    let final_row = HashableRow::new(assigned_rowid, row.values.clone());
                    output.changes.push((final_row, weight));
                }
                output
            }
        }
    }
}

impl Display for MergeOperator {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match &self.union_mode {
            UnionMode::Distinct => write!(f, "MergeOperator({}, UNION)", self.operator_id),
            UnionMode::All { .. } => write!(f, "MergeOperator({}, UNION ALL)", self.operator_id),
        }
    }
}

impl IncrementalOperator for MergeOperator {
    fn eval(
        &mut self,
        input: &mut EvalState,
        _cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Delta>> {
        match input {
            EvalState::Init { deltas } => {
                // Extract deltas from the evaluation state
                let delta_pair = std::mem::take(deltas);

                // Transform deltas based on union mode (with state tracking)
                let left_transformed = self.transform_delta(delta_pair.left, true);
                let right_transformed = self.transform_delta(delta_pair.right, false);

                // Merge the transformed deltas
                let mut output = Delta::new();
                output.merge(&left_transformed);
                output.merge(&right_transformed);

                // Move to Done state
                *input = EvalState::Done;

                Ok(IOResult::Done(output))
            }
            EvalState::Aggregate(_)
            | EvalState::Join(_)
            | EvalState::Antijoin(_)
            | EvalState::Uninitialized => {
                // Merge operator only handles Init state
                unreachable!("MergeOperator only handles Init state")
            }
            EvalState::Done => {
                // Already evaluated
                Ok(IOResult::Done(Delta::new()))
            }
        }
    }

    fn commit(
        &mut self,
        deltas: DeltaPair,
        _cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Delta>> {
        // Transform deltas based on union mode
        let left_transformed = self.transform_delta(deltas.left, true);
        let right_transformed = self.transform_delta(deltas.right, false);

        // Merge the transformed deltas
        let mut output = Delta::new();
        output.merge(&left_transformed);
        output.merge(&right_transformed);

        Ok(IOResult::Done(output))
    }

    fn set_tracker(&mut self, _tracker: Arc<Mutex<ComputationTracker>>) {
        // Merge operator doesn't need tracking for now
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}
