//! DBSP Compiler: Converts Logical Plans to DBSP Circuits
//!
//! This module implements compilation from SQL logical plans to DBSP circuits.
//! The initial version supports only filter and projection operators.
//!
//! Based on the DBSP paper: "DBSP: Automatic Incremental View Maintenance for Rich Query Languages"

use crate::incremental::aggregate_operator::AggregateOperator;
use crate::incremental::dbsp::{Delta, DeltaPair};
use crate::incremental::expr_compiler::{
    CompiledExpression, ExpressionExecutor, TrivialExpression,
};
use crate::incremental::literal_operator::LiteralOperator;
use crate::incremental::operator::{
    create_dbsp_state_index, AntijoinOperator, DbspStateCursors, EvalState, FilterOperator,
    FilterPredicate, IncrementalOperator, InputOperator, JoinOperator, JoinType, ProjectOperator,
};
use crate::incremental::recursive_operator::{RecursiveOperator, RecursiveState};
use crate::schema::Type;
use crate::storage::btree::{BTreeCursor, BTreeKey, CursorTrait};
use crate::SqliteDialect;
// Note: logical module must be made pub(crate) in translate/mod.rs
use crate::numeric::Numeric;
use crate::sync::{atomic::Ordering, Arc};
use crate::translate::logical::{
    BinaryOperator, Column, ColumnInfo, JoinType as LogicalJoinType, LogicalExpr, LogicalPlan,
    LogicalSchema, RecursiveCTE, SchemaRef, DEFAULT_RECURSIVE_MAX_ITERATIONS,
};
use crate::types::{IOResult, ImmutableRecord, SeekKey, SeekOp, SeekResult, Value};
use crate::Pager;

use crate::util::IOExt;
use crate::{return_and_restore_if_io, return_if_io, LimboError, Result};
use rustc_hash::FxHashMap as HashMap;

/// Which side of a join a filter expression references.
enum FilterSide {
    LeftOnly,
    RightOnly,
    Cross,
}
use std::fmt::{self, Display, Formatter};

// The state table has 5 columns: operator_id, zset_id, element_id, value, weight
const OPERATOR_COLUMNS: usize = 5;

/// State machine for writing rows to simple materialized views (table-only, no index)
#[derive(Debug, Default)]
pub enum WriteRowView {
    #[default]
    GetRecord,
    Delete,
    /// Seek completed, delete operation in progress
    Deleting,
    Insert {
        final_weight: isize,
    },
    /// Seek completed, insert operation in progress
    Inserting {
        final_weight: isize,
    },
    Done,
}

impl WriteRowView {
    pub fn new() -> Self {
        Self::default()
    }

    /// Write a row with weight management for table-only storage.
    ///
    /// # Arguments
    /// * `cursor` - BTree cursor for the storage
    /// * `key` - The key to seek (TableRowId)
    /// * `build_record` - Function that builds the record values to insert.
    ///   Takes the final_weight and returns the complete record values.
    /// * `weight` - The weight delta to apply
    pub fn write_row(
        &mut self,
        cursor: &mut BTreeCursor,
        key: SeekKey,
        build_record: impl Fn(isize) -> Vec<Value>,
        weight: isize,
    ) -> Result<IOResult<()>> {
        loop {
            match self {
                WriteRowView::GetRecord => {
                    let res = return_if_io!(cursor.seek(key.clone(), SeekOp::GE { eq_only: true }));
                    if !matches!(res, SeekResult::Found) {
                        if weight <= 0 {
                            *self = WriteRowView::Done;
                        } else {
                            *self = WriteRowView::Insert {
                                final_weight: weight,
                            };
                        }
                    } else {
                        let existing_record = return_if_io!(cursor.record());
                        let r = existing_record.ok_or_else(|| {
                            LimboError::InternalError(format!(
                                "Found key {key:?} in storage but could not read record"
                            ))
                        })?;
                        let last = r.iter()?.last();

                        // Weight is always the last value
                        let existing_weight = match last {
                            Some(val) => match val?.to_owned()? {
                                Value::Numeric(Numeric::Integer(w)) => w as isize,
                                _ => {
                                    return Err(LimboError::InternalError(format!(
                                        "Invalid weight value in storage for key {key:?}"
                                    )));
                                }
                            },
                            None => {
                                return Err(LimboError::InternalError(format!(
                                    "No weight value found in storage for key {key:?}"
                                )));
                            }
                        };

                        let final_weight = existing_weight + weight;
                        if final_weight <= 0 {
                            *self = WriteRowView::Delete
                        } else {
                            *self = WriteRowView::Insert { final_weight }
                        }
                    }
                }
                WriteRowView::Delete => {
                    // Transition to Deleting state before the delete operation
                    // so we can resume if I/O occurs during delete/balance
                    *self = WriteRowView::Deleting;
                }
                WriteRowView::Deleting => {
                    return_if_io!(cursor.delete());
                    *self = WriteRowView::Done;
                }
                WriteRowView::Insert { final_weight } => {
                    return_if_io!(cursor.seek(key.clone(), SeekOp::GE { eq_only: true }));

                    // Transition to Inserting state after seek completes
                    // so we can resume the insert if I/O occurs during insert/balance
                    *self = WriteRowView::Inserting {
                        final_weight: *final_weight,
                    };
                }
                WriteRowView::Inserting { final_weight } => {
                    // Extract the row ID from the key
                    let key_i64 = match key {
                        SeekKey::TableRowId(id) => id,
                        _ => {
                            return Err(LimboError::InternalError(
                                "Expected TableRowId for storage".to_string(),
                            ));
                        }
                    };

                    // Build the record values using the provided function
                    let record_values = build_record(*final_weight);

                    // Create an ImmutableRecord from the values
                    let immutable_record =
                        ImmutableRecord::from_values(&record_values, record_values.len())?;
                    let btree_key = BTreeKey::new_table_rowid(key_i64, Some(&immutable_record));

                    return_if_io!(cursor.insert(&btree_key));
                    *self = WriteRowView::Done;
                }
                WriteRowView::Done => {
                    return Ok(IOResult::Done(()));
                }
            }
        }
    }
}

/// State machine for writing rows to index-organized materialized views
/// (used when ORDER BY is present). The key is a composite record:
/// `[sort_col_vals..., rowid, remaining_col_vals..., weight]`.
///
/// Each I/O operation is split into two states with `sought: bool` to make
/// re-entry safe — when the seek state transitions BEFORE the I/O operation,
/// re-entry picks up at the mutation state.
#[derive(Debug)]
pub enum WriteRowViewIndex {
    /// Seek for an existing row by composite key (eq_only=true).
    GetRecord {
        sought: bool,
    },
    /// Existing row found, weight will be decremented or removed.
    Deleting {
        sought: bool,
    },
    /// New or updated row will be inserted.
    Inserting {
        sought: bool,
        final_weight: isize,
        /// Full record to insert: [sort_vals..., rowid, data_cols..., weight]
        full_record: ImmutableRecord,
    },
    Done,
}

impl Default for WriteRowViewIndex {
    fn default() -> Self {
        Self::GetRecord { sought: false }
    }
}

impl WriteRowViewIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Write a row with weight management for index-organized storage.
    ///
    /// # Arguments
    /// * `cursor` - BTree cursor with index_info set for the sort columns
    /// * `composite_seek_key` - ImmutableRecord of [sort_vals..., rowid] for seeking
    /// * `full_record` - ImmutableRecord of [sort_vals..., rowid, data_cols..., weight] for insert
    /// * `weight` - The weight delta to apply
    pub fn write_row(
        &mut self,
        cursor: &mut BTreeCursor,
        composite_seek_key: &ImmutableRecord,
        full_record: &ImmutableRecord,
        weight: isize,
    ) -> Result<IOResult<()>> {
        let seek_key = SeekKey::IndexKey(composite_seek_key.as_record_ref());
        loop {
            match self {
                WriteRowViewIndex::GetRecord { sought } => {
                    if !*sought {
                        *self = WriteRowViewIndex::GetRecord { sought: true };
                    }
                    let res =
                        return_if_io!(cursor.seek(seek_key.clone(), SeekOp::GE { eq_only: true }));
                    match res {
                        SeekResult::Found => {
                            let existing_record = return_if_io!(cursor.record());
                            let r = existing_record.ok_or_else(|| {
                                LimboError::InternalError(
                                    "Found composite key in storage but could not read record"
                                        .to_string(),
                                )
                            })?;
                            let last = r.iter()?.last();
                            let existing_weight = match last {
                                Some(val) => match val?.to_owned()? {
                                    Value::Numeric(Numeric::Integer(w)) => w as isize,
                                    _ => {
                                        return Err(LimboError::InternalError(
                                            "Invalid weight value in storage for index key"
                                                .to_string(),
                                        ));
                                    }
                                },
                                None => {
                                    return Err(LimboError::InternalError(
                                        "No weight value found in storage for index key"
                                            .to_string(),
                                    ));
                                }
                            };

                            let final_weight = existing_weight + weight;
                            if final_weight <= 0 {
                                *self = WriteRowViewIndex::Deleting { sought: false };
                            } else {
                                // Build full record with final weight
                                let mut vals = full_record.get_values_owned()?;
                                // Replace the last value (weight) with final_weight
                                let last_idx = vals.len() - 1;
                                vals[last_idx] = Value::from_i64(final_weight as i64);
                                let new_full_record =
                                    ImmutableRecord::from_values(&vals, vals.len())?;
                                *self = WriteRowViewIndex::Inserting {
                                    sought: false,
                                    final_weight,
                                    full_record: new_full_record,
                                };
                            }
                        }
                        SeekResult::TryAdvance => {
                            // At a leaf boundary — advance and re-check.
                            // Pattern matches cursor.rs SeekState::Advancing.
                            return_if_io!(cursor.next());
                            let rowid = return_if_io!(cursor.rowid());
                            if rowid.is_none() {
                                // Past end — key not found
                                if weight <= 0 {
                                    *self = WriteRowViewIndex::Done;
                                } else {
                                    let record_for_insert = full_record.clone();
                                    *self = WriteRowViewIndex::Inserting {
                                        sought: false,
                                        final_weight: weight,
                                        full_record: record_for_insert,
                                    };
                                }
                            } else {
                                // Advanced to a new entry — re-evaluate at GetRecord
                                *self = WriteRowViewIndex::GetRecord { sought: false };
                            }
                        }
                        SeekResult::NotFound => {
                            if weight <= 0 {
                                *self = WriteRowViewIndex::Done;
                            } else {
                                *self = WriteRowViewIndex::Inserting {
                                    sought: false,
                                    final_weight: weight,
                                    full_record: full_record.clone(),
                                };
                            }
                        }
                    }
                }
                WriteRowViewIndex::Deleting { sought } => {
                    if !*sought {
                        *self = WriteRowViewIndex::Deleting { sought: true };
                    }
                    return_if_io!(cursor.delete());
                    *self = WriteRowViewIndex::Done;
                }
                WriteRowViewIndex::Inserting {
                    sought,
                    final_weight,
                    full_record,
                } => {
                    let record_to_insert = full_record.clone();
                    let weight_to_store = *final_weight;
                    let already_sought = *sought;
                    if !already_sought {
                        *self = WriteRowViewIndex::Inserting {
                            sought: true,
                            final_weight: weight_to_store,
                            full_record: record_to_insert.clone(),
                        };
                    }
                    let btree_key = BTreeKey::new_index_key(record_to_insert.as_record_ref());
                    return_if_io!(cursor.insert(&btree_key));
                    *self = WriteRowViewIndex::Done;
                }
                WriteRowViewIndex::Done => {
                    return Ok(IOResult::Done(()));
                }
            }
        }
    }
}

/// State machine for commit operations
pub enum CommitState {
    /// Initial state - ready to start commit
    Init,

    /// Running circuit with commit_operators flag set to true
    CommitOperators {
        /// Execute state for running the circuit
        execute_state: Box<ExecuteState>,
        /// Persistent cursors for operator state (table and index)
        state_cursors: Box<DbspStateCursors>,
    },

    /// Updating the materialized view with the delta
    UpdateView {
        /// Delta to write to the view
        delta: Delta,
        /// Current index in delta.changes being processed
        current_index: usize,
        /// State for writing individual rows (table btree)
        write_row_state: WriteRowView,
        /// State for writing individual rows (index btree, used when ORDER BY is present)
        write_row_index_state: WriteRowViewIndex,
        /// Cursor for view data btree - created fresh for each row
        view_cursor: Box<BTreeCursor>,
        /// Whether this view uses index-organized storage (ORDER BY)
        is_index_organized: bool,
        /// Number of columns in the output (including weight)
        num_columns: usize,
        /// ORDER BY info copied from circuit for building composite keys
        view_order_by: super::view::MatviewOrderBy,
    },
}

impl std::fmt::Debug for CommitState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Init => write!(f, "Init"),
            Self::CommitOperators { execute_state, .. } => f
                .debug_struct("CommitOperators")
                .field("execute_state", execute_state)
                .field("has_state_table_cursor", &true)
                .field("has_state_index_cursor", &true)
                .finish(),
            Self::UpdateView {
                delta,
                current_index,
                write_row_state,
                ..
            } => f
                .debug_struct("UpdateView")
                .field("delta", delta)
                .field("current_index", current_index)
                .field("write_row_state", write_row_state)
                .field("has_view_cursor", &true)
                .finish(),
        }
    }
}

/// State machine for circuit execution across I/O operations
/// Similar to EvalState but for tracking execution state through the circuit
#[derive(Debug)]
pub enum ExecuteState {
    /// Empty state so we can allocate the space without executing
    Uninitialized,

    /// Initial state - starting circuit execution
    Init {
        /// Input deltas to process
        input_data: InputDeltas,
    },

    /// Processing multiple inputs (for recursive node processing)
    ProcessingInputs {
        /// Collection of (node_id, state) pairs to process
        input_states: Vec<(i64, ExecuteState)>,
        /// Current index being processed
        current_index: usize,
        /// Collected deltas from processed inputs
        input_deltas: Vec<Delta>,
        /// Cursors persisted across I/O yields so operator state stays consistent
        temp_cursors: Option<Box<DbspStateCursors>>,
    },

    /// Processing a specific node in the circuit
    ProcessingNode {
        /// Node's evaluation state (includes the delta in its Init state)
        eval_state: Box<EvalState>,
    },

    /// Processing a recursive fixed-point iteration
    ProcessingRecursive {
        /// ID of the recursive operator node
        node_id: i64,
        /// Base case node ID
        base_case_id: i64,
        /// Recursive step node ID
        recursive_step_id: i64,
        /// Name of the recursive CTE (used for delay key in input_data)
        delay_name: String,
        /// State for the current sub-execution
        sub_state: Box<ExecuteState>,
        /// Input data (for sub-executions)
        input_data: InputDeltas,
        /// Cursors persisted across I/O yields so operator state stays consistent
        temp_cursors: Option<Box<DbspStateCursors>>,
    },
}

/// A set of deltas for multiple tables/operators
/// This provides a cleaner API for passing deltas through circuit execution
#[derive(Debug, Clone, Default)]
pub struct DeltaSet {
    /// Deltas keyed by table/operator name
    deltas: HashMap<String, Delta>,
}

impl DeltaSet {
    /// Create a new empty delta set
    pub fn new() -> Self {
        Self {
            deltas: HashMap::default(),
        }
    }

    /// Create an empty delta set (more semantic for "no changes")
    pub fn empty() -> Self {
        Self {
            deltas: HashMap::default(),
        }
    }

    /// Create a DeltaSet from a HashMap
    pub fn from_map(deltas: HashMap<String, Delta>) -> Self {
        Self { deltas }
    }

    /// Add a delta for a table
    pub fn insert(&mut self, table_name: String, delta: Delta) {
        self.deltas.insert(table_name, delta);
    }

    /// Get delta for a table, returns empty delta if not found
    pub fn get(&self, table_name: &str) -> Delta {
        self.deltas
            .get(table_name)
            .cloned()
            .unwrap_or_else(Delta::new)
    }

    /// Convert DeltaSet into the underlying HashMap
    pub fn into_map(self) -> HashMap<String, Delta> {
        self.deltas
    }

    /// Check if all deltas in the set are empty
    pub fn is_empty(&self) -> bool {
        self.deltas.values().all(|d| d.is_empty())
    }
}

/// Overlay view of input deltas used during execution.
/// Keeps base deltas shared and allows a small per-iteration override set.
#[derive(Debug, Clone)]
pub struct InputDeltas {
    base: Arc<DeltaSet>,
    overlay: DeltaSet,
}

impl InputDeltas {
    pub fn from_base(base: Arc<DeltaSet>) -> Self {
        Self {
            base,
            overlay: DeltaSet::new(),
        }
    }

    pub fn from_delta_set(delta_set: DeltaSet) -> Self {
        Self::from_base(Arc::new(delta_set))
    }

    pub fn from_map(deltas: HashMap<String, Delta>) -> Self {
        Self::from_delta_set(DeltaSet::from_map(deltas))
    }

    /// Get delta for a table, overlay first, then base.
    pub fn get(&self, table_name: &str) -> Delta {
        self.overlay
            .deltas
            .get(table_name)
            .cloned()
            .unwrap_or_else(|| self.base.get(table_name))
    }

    /// Insert/replace an overlay delta.
    pub fn insert(&mut self, table_name: String, delta: Delta) {
        self.overlay.insert(table_name, delta);
    }

    /// Retain only overlay entries that match predicate.
    pub fn retain_overlay<F>(&mut self, f: F)
    where
        F: FnMut(&String, &mut Delta) -> bool,
    {
        self.overlay.deltas.retain(f);
    }

    /// Drop the shared base deltas (used after the first recursive iteration).
    pub fn clear_base(&mut self) {
        self.base = Arc::new(DeltaSet::new());
    }
}

impl Default for InputDeltas {
    fn default() -> Self {
        Self::from_base(Arc::new(DeltaSet::new()))
    }
}

/// Represents a DBSP operator in the compiled circuit
#[derive(Debug, Clone, PartialEq)]
pub enum DbspOperator {
    /// Filter operator (σ) - filters records based on a predicate
    Filter { predicate: DbspExpr },
    /// Projection operator (π) - projects specific columns
    Projection {
        exprs: Vec<DbspExpr>,
        schema: SchemaRef,
    },
    /// Aggregate operator (γ) - performs grouping and aggregation
    Aggregate {
        group_exprs: Vec<DbspExpr>,
        aggr_exprs: Vec<crate::incremental::operator::AggregateFunction>,
        schema: SchemaRef,
    },
    /// Join operator (⋈) - joins two relations
    Join {
        join_type: JoinType,
        on_exprs: Vec<(DbspExpr, DbspExpr)>,
        schema: SchemaRef,
    },
    /// Input operator - source of data
    Input { name: String, schema: SchemaRef },
    /// Literal operator - produces constant rows (for EmptyRelation and VALUES)
    Literal { schema: SchemaRef },
    /// Merge operator for combining streams (used in recursive CTEs and UNION)
    Merge { schema: SchemaRef },
    /// Antijoin operator (LEFT JOIN's null-padded unmatched half).
    /// Wired in parallel with a JoinOperator(Inner) and UNION-ALL'd via Merge.
    Antijoin {
        schema: SchemaRef,
        right_column_count: usize,
    },
    /// Distinct operator - removes duplicates
    Distinct { schema: SchemaRef },
    /// Recursive operator - container for fixed-point computation
    Recursive {
        /// Name of the recursive CTE
        name: String,
        /// Maximum iterations allowed
        max_iterations: usize,
        /// Whether to use UNION ALL semantics
        union_all: bool,
        /// Schema of the output
        schema: SchemaRef,
    },
}

/// Represents an expression in DBSP
#[derive(Debug, Clone, PartialEq)]
pub enum DbspExpr {
    /// Column reference
    Column(String),
    /// Literal value
    Literal(Value),
    /// Binary expression
    BinaryExpr {
        left: Box<DbspExpr>,
        op: BinaryOperator,
        right: Box<DbspExpr>,
    },
}

/// A node in the DBSP circuit DAG
pub struct DbspNode {
    /// Unique identifier for this node
    pub id: i64,
    /// The operator metadata
    pub operator: DbspOperator,
    /// Input nodes (edges in the DAG)
    pub inputs: Vec<i64>,
    /// The actual executable operator
    pub executable: Box<dyn IncrementalOperator>,
}

// SAFETY: This needs to be audited for thread safety.
// See: https://github.com/tursodatabase/turso/issues/1552
unsafe impl Send for DbspNode {}
unsafe impl Sync for DbspNode {}
crate::assert::assert_send_sync!(DbspNode);

impl std::fmt::Debug for DbspNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbspNode")
            .field("id", &self.id)
            .field("operator", &self.operator)
            .field("inputs", &self.inputs)
            .field("has_executable", &true)
            .finish()
    }
}

impl DbspNode {
    fn process_node(
        &mut self,
        eval_state: &mut EvalState,
        commit_operators: bool,
        cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Delta>> {
        // Process delta using the executable operator
        let op = &mut self.executable;

        let state = if commit_operators {
            // Clone the deltas from eval_state - don't extract them
            // in case we need to re-execute due to I/O
            let deltas = match eval_state {
                EvalState::Init { deltas } => deltas.clone(),
                _ => panic!("commit can only be called when eval_state is in Init state"),
            };
            let result = return_if_io!(op.commit(deltas, cursors));
            // After successful commit, move state to Done
            *eval_state = EvalState::Done;
            result
        } else {
            return_if_io!(op.eval(eval_state, cursors))
        };
        Ok(IOResult::Done(state))
    }
}

/// Version number for the DBSP circuit format
/// This should be incremented when the circuit structure changes
pub const DBSP_CIRCUIT_VERSION: u32 = 1;

/// Represents a complete DBSP circuit (DAG of operators)
#[derive(Debug)]
pub struct DbspCircuit {
    /// All nodes in the circuit, indexed by their ID
    pub(super) nodes: HashMap<i64, DbspNode>,
    /// Counter for generating unique node IDs
    next_id: i64,
    /// Root node ID (the final output)
    pub(super) root: Option<i64>,
    /// Output schema of the circuit (schema of the root node)
    pub(super) output_schema: SchemaRef,

    /// State machine for commit operation
    commit_state: CommitState,

    /// Root page for the main materialized view data
    pub(super) main_data_root: i64,
    /// Root page for internal DBSP state table
    pub(super) internal_state_root: i64,
    /// Root page for the DBSP state table's primary key index
    pub(super) internal_state_index_root: i64,

    /// ORDER BY columns (empty if no ORDER BY)
    pub order_by: super::view::MatviewOrderBy,
    /// LIMIT clause (None if no LIMIT)
    pub limit: Option<i64>,

    /// Per-circuit-run memo of node outputs.
    ///
    /// In a diamond DAG (e.g. dual `LEFT OUTER JOIN`, where the first LJ's
    /// merge feeds both the second LJ's inner-join and antijoin
    /// sub-operators), `execute_node` would otherwise visit the shared
    /// upstream subtree once per consumer. For stateful operators
    /// (`JoinOperator`, `AntijoinOperator`, `AggregateOperator`)
    /// `commit` mutates btree state, so the second visit observes the
    /// first visit's writes and returns a *different* delta — leaving
    /// downstream consumers with inconsistent left/right inputs.
    /// Memoising the first delta and reusing it for every later consumer
    /// in the same `run_circuit` pass makes shared subtrees behave like
    /// a true DAG instead of a tree.
    ///
    /// Cleared at the start of each `commit()` / `execute()` so it never
    /// leaks across circuit runs.
    exec_node_cache: HashMap<i64, Delta>,
}

// SAFETY: This needs to be audited for thread safety.
// See: https://github.com/tursodatabase/turso/issues/1552
unsafe impl Send for DbspCircuit {}
unsafe impl Sync for DbspCircuit {}
crate::assert::assert_send_sync!(DbspCircuit);

impl DbspCircuit {
    /// Create a new empty circuit with initial empty schema
    /// The actual output schema will be set when the root node is established
    pub fn new(
        main_data_root: i64,
        internal_state_root: i64,
        internal_state_index_root: i64,
        order_by: super::view::MatviewOrderBy,
        limit: Option<i64>,
    ) -> Self {
        // Start with an empty schema - will be updated when root is set
        let empty_schema = Arc::new(LogicalSchema::new(vec![]));
        Self {
            nodes: HashMap::default(),
            next_id: 1, // Start from 1 to reserve 0 for metadata
            root: None,
            output_schema: empty_schema,
            commit_state: CommitState::Init,
            main_data_root,
            internal_state_root,
            internal_state_index_root,
            order_by,
            limit,
            exec_node_cache: HashMap::default(),
        }
    }

    /// Convenience constructor for tests and internal use where ORDER BY is not needed.
    pub fn new_table_only(
        main_data_root: i64,
        internal_state_root: i64,
        internal_state_index_root: i64,
    ) -> Self {
        Self::new(
            main_data_root,
            internal_state_root,
            internal_state_index_root,
            super::view::MatviewOrderBy::default(),
            None,
        )
    }

    /// Check if this circuit is running in one-shot mode (no btree storage).
    /// In one-shot mode, all root pages are 0, meaning there's no persistent
    /// storage for intermediate state. This affects how we handle base data
    /// during recursive CTE execution.
    fn is_one_shot(&self) -> bool {
        self.internal_state_root == 0 && self.internal_state_index_root == 0
    }

    /// Save all RecursiveOperator snapshots so execute_with_uncommitted
    /// can run without corrupting the circuit's persistent state.
    pub fn save_recursive_snapshots(
        &self,
    ) -> Vec<(i64, super::recursive_operator::RecursiveOperatorSnapshot)> {
        self.nodes
            .iter()
            .filter_map(|(&id, node)| {
                if let DbspOperator::Recursive { .. } = &node.operator {
                    node.executable
                        .as_any()
                        .downcast_ref::<RecursiveOperator>()
                        .map(|op| (id, op.save_snapshot()))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Restore RecursiveOperator snapshots after read-only execution.
    pub fn restore_recursive_snapshots(
        &mut self,
        snapshots: Vec<(i64, super::recursive_operator::RecursiveOperatorSnapshot)>,
    ) {
        for (id, snapshot) in snapshots {
            if let Some(node) = self.nodes.get_mut(&id) {
                if let Some(op) = node
                    .executable
                    .as_any_mut()
                    .downcast_mut::<RecursiveOperator>()
                {
                    op.restore_snapshot(snapshot);
                }
            }
        }
    }

    /// Reset all recursive operators so their state will be rebuilt from the
    /// btree on the next `execute()` or `commit()` call.  Used after ROLLBACK
    /// to bring the in-memory DBSP state back in sync with the (rolled-back)
    /// matview btree.
    pub fn reset_recursive_operators_for_rollback(&mut self) {
        for node in self.nodes.values_mut() {
            if let DbspOperator::Recursive { .. } = &node.operator {
                if let Some(op) = node
                    .executable
                    .as_any_mut()
                    .downcast_mut::<RecursiveOperator>()
                {
                    op.reset_for_new_transaction();
                }
            }
        }
    }

    /// Set the root node and update the output schema
    fn set_root(&mut self, root_id: i64, schema: SchemaRef) {
        self.root = Some(root_id);
        self.output_schema = schema;
    }

    /// Get the current materialized state by reading from btree
    /// Add a node to the circuit
    fn add_node(
        &mut self,
        operator: DbspOperator,
        inputs: Vec<i64>,
        executable: Box<dyn IncrementalOperator>,
    ) -> i64 {
        let id = self.next_id;
        self.next_id += 1;

        let node = DbspNode {
            id,
            operator,
            inputs,
            executable,
        };

        self.nodes.insert(id, node);
        id
    }

    pub fn run_circuit(
        &mut self,
        execute_state: &mut ExecuteState,
        pager: &Arc<Pager>,
        state_cursors: &mut DbspStateCursors,
        commit_operators: bool,
    ) -> Result<IOResult<Delta>> {
        if let Some(root_id) = self.root {
            self.execute_node(
                root_id,
                pager.clone(),
                execute_state,
                commit_operators,
                state_cursors,
            )
        } else {
            Err(LimboError::ParseError(
                "Circuit has no root node".to_string(),
            ))
        }
    }

    fn new_state_cursors(&self, pager: Arc<Pager>) -> Result<DbspStateCursors> {
        let table_cursor =
            BTreeCursor::new_table(pager.clone(), self.internal_state_root, OPERATOR_COLUMNS);
        let index_def = create_dbsp_state_index(self.internal_state_index_root);
        let index_cursor =
            BTreeCursor::new_index(pager, self.internal_state_index_root, &index_def, 3)?;
        Ok(DbspStateCursors::new(table_cursor, index_cursor))
    }

    /// Execute the circuit with incremental input data (deltas).
    ///
    /// # Arguments
    /// * `pager` - Pager for btree access
    /// * `context` - Execution context for tracking operator states
    /// * `execute_state` - State machine containing input deltas and tracking execution progress
    pub fn execute(
        &mut self,
        pager: Arc<Pager>,
        execute_state: &mut ExecuteState,
    ) -> Result<IOResult<Delta>> {
        if let Some(root_id) = self.root {
            self.restore_recursive_operators_if_needed(&pager)?;
            // Create temporary cursors for execute (non-commit) operations
            let mut cursors = self.new_state_cursors(pager.clone())?;
            // Fresh per-circuit-run memo (see DbspCircuit::exec_node_cache).
            // For non-commit `eval`, double-execution would still produce
            // duplicate work; for commit it would corrupt operator state.
            // Either way, every public entry point starts with an empty memo.
            if matches!(execute_state, ExecuteState::Init { .. }) {
                self.exec_node_cache.clear();
            }
            self.execute_node(root_id, pager, execute_state, false, &mut cursors)
        } else {
            Err(LimboError::ParseError(
                "Circuit has no root node".to_string(),
            ))
        }
    }

    /// Restore RecursiveOperator state from the matview btree when the view
    /// was loaded from disk. Without this, incremental updates after DB reopen
    /// assign fresh rowids that collide with existing btree entries.
    fn restore_recursive_operators_if_needed(&mut self, pager: &Arc<Pager>) -> Result<()> {
        if self.main_data_root == 0 {
            return Ok(());
        }

        let needs_restore: Vec<i64> = self
            .nodes
            .iter()
            .filter_map(|(&id, node)| {
                if let DbspOperator::Recursive { .. } = &node.operator {
                    let op = node
                        .executable
                        .as_any()
                        .downcast_ref::<RecursiveOperator>()?;
                    if op.needs_state_restore() {
                        return Some(id);
                    }
                }
                None
            })
            .collect();

        if needs_restore.is_empty() {
            return Ok(());
        }

        let num_columns = self.output_schema.columns.len() + 1;
        let rows = Self::read_btree_rows(pager, self.main_data_root, num_columns, &self.order_by)?;

        if rows.is_empty() {
            return Ok(());
        }

        for node_id in needs_restore {
            if let Some(node) = self.nodes.get_mut(&node_id) {
                if let Some(op) = node
                    .executable
                    .as_any_mut()
                    .downcast_mut::<RecursiveOperator>()
                {
                    op.restore_state_from_btree_data(&rows);
                }
            }
        }

        Ok(())
    }

    /// Read all (rowid, values) pairs from a btree.
    ///
    /// For ORDER BY (index-organized) views, the on-disk record layout is
    /// `[sort_v_1, ..., sort_v_N, rowid, non_sort_data_cols..., weight]`, so
    /// the cursor must be opened as an index cursor and the rowid extracted
    /// manually from `record.values[num_sort_cols]`. For table-organized
    /// views, the layout is `[data_cols..., weight]` and the rowid is the
    /// btree key.
    fn read_btree_rows(
        pager: &Arc<Pager>,
        root_page: i64,
        num_columns: usize,
        order_by: &super::view::MatviewOrderBy,
    ) -> Result<Vec<(i64, Vec<Value>)>> {
        let mut rows = Vec::new();
        let mut btree_cursor = if order_by.is_empty() {
            BTreeCursor::new_table(pager.clone(), root_page, num_columns)
        } else {
            BTreeCursor::new_index_with_index_info(
                pager.clone(),
                root_page,
                order_by.to_index_info(),
                num_columns,
            )
        };

        pager.io.block(|| btree_cursor.rewind())?;

        let num_sort_cols = order_by.len();
        let is_index_organized = !order_by.is_empty();

        loop {
            if btree_cursor.is_empty() {
                break;
            }

            let record = loop {
                match btree_cursor.record()? {
                    IOResult::Done(r) => break r,
                    IOResult::IO(io) => io.wait(&*pager.io)?,
                }
            }
            .expect("cursor not empty")
            .to_owned();

            let column_count = record.column_count();
            // Last column is the weight, skip it
            let num_data_columns = column_count - 1;

            if is_index_organized {
                // Index layout: [sort_v_1..N, rowid, non_sort_data..., weight].
                // Extract rowid from position `num_sort_cols`; reconstruct
                // logical-order data values.
                let mut all_vals = Vec::with_capacity(column_count);
                let mut iter = record.iter()?;
                for _ in 0..column_count {
                    all_vals.push(iter.next().expect("checked bounds")?.to_owned()?);
                }
                // weight is last
                all_vals.pop();
                // rowid is at position num_sort_cols
                let rowid_val = all_vals.remove(num_sort_cols);
                let rowid = match rowid_val {
                    Value::Numeric(Numeric::Integer(r)) => r,
                    _ => {
                        return Err(LimboError::InternalError(format!(
                            "expected integer rowid in matview index record at position {num_sort_cols}, found {rowid_val:?}"
                        )));
                    }
                };
                // all_vals now holds storage-order data values (sort cols first,
                // then non-sort cols in input order). For restore_state_from_btree_data
                // the values just need to round-trip through the recursive operator;
                // logical column order isn't strictly required here. But to keep the
                // contract uniform we permute back to logical order using the
                // sort-column index map.
                let logical = order_by.permute_storage_to_logical(&all_vals);
                rows.push((rowid, logical));
            } else {
                let rowid = pager
                    .io
                    .block(|| btree_cursor.rowid())?
                    .expect("cursor not empty");
                let mut values = Vec::with_capacity(num_data_columns);
                let mut values_iter = record.iter()?;
                for _ in 0..num_data_columns {
                    let value = values_iter.next().expect("checked bounds")?;
                    values.push(value.to_owned()?);
                }
                rows.push((rowid, values));
            }

            pager.io.block(|| btree_cursor.next())?;
        }

        Ok(rows)
    }

    /// Commit deltas to the circuit, updating internal operator state and persisting to btree.
    /// This should be called after execute() when you want to make changes permanent.
    ///
    /// # Arguments
    /// * `input_data` - The deltas to commit (same as what was passed to execute)
    /// * `pager` - Pager for creating cursors to the btrees
    pub fn commit(
        &mut self,
        input_data: HashMap<String, Delta>,
        pager: Arc<Pager>,
    ) -> Result<IOResult<Delta>> {
        // No root means nothing to commit
        if self.root.is_none() {
            return Ok(IOResult::Done(Delta::new()));
        }

        self.restore_recursive_operators_if_needed(&pager)?;

        // Get btree root pages
        let main_data_root = self.main_data_root;

        // Add 1 for the weight column that we store in the btree
        let num_columns = self.output_schema.columns.len() + 1;

        // Convert input_data to DeltaSet once, outside the loop
        let input_delta_set = Arc::new(DeltaSet::from_map(input_data));

        loop {
            // Take ownership of the state for processing, to avoid borrow checker issues (we have
            // to call run_circuit, which takes &mut self. Because of that, cannot use
            // return_if_io. We have to use the version that restores the state before returning.
            let mut state = std::mem::replace(&mut self.commit_state, CommitState::Init);
            match &mut state {
                CommitState::Init => {
                    // Create state cursors when entering CommitOperators state
                    let state_cursors = Box::new(self.new_state_cursors(pager.clone())?);

                    // Fresh per-commit memo (see DbspCircuit::exec_node_cache).
                    self.exec_node_cache.clear();

                    self.commit_state = CommitState::CommitOperators {
                        execute_state: Box::new(ExecuteState::Init {
                            input_data: InputDeltas::from_base(input_delta_set.clone()),
                        }),
                        state_cursors,
                    };
                }
                CommitState::CommitOperators {
                    ref mut execute_state,
                    ref mut state_cursors,
                } => {
                    let delta = return_and_restore_if_io!(
                        &mut self.commit_state,
                        state,
                        self.run_circuit(execute_state, &pager, state_cursors, true,)
                    );

                    // Consolidate the delta before writing to the BTree.
                    // The DBSP three-way join (δL⋈δR + δL⋈R_prev + L_prev⋈δR)
                    // can produce multiple entries for the same rowid from
                    // different join phases. Summing their weights eliminates
                    // redundant insert/retract pairs (net weight = 0).
                    let mut delta = delta;
                    delta.consolidate();

                    // Sort: deletes (weight < 0) before inserts (weight > 0),
                    // breaking ties by rowid. Two callers depend on this:
                    //
                    //   1. WriteRowView. Same-rowid UPDATEs produce a delete of
                    //      the old row and an insert of the new row at the same
                    //      rowid. Insert-first would bump the existing weight to
                    //      2; the delete then re-inserts with OLD values at
                    //      weight 1, reverting the update.
                    //
                    //   2. CDC consumers. Recursive UNION ALL matviews emit the
                    //      retraction of an old projected row and the insertion
                    //      of the new one with DIFFERENT rowids (the recursive
                    //      operator pops the existing rowid for the delete and
                    //      assigns a fresh `next_rowid` for the insert). Sorting
                    //      by rowid first leaks Insert-then-Delete to the change
                    //      callback whenever the fresh rowid happens to be lower
                    //      than the popped one — which order-aware coalescers
                    //      (DELETE→INSERT folds to UPDATE; INSERT→DELETE folds to
                    //      no-op) silently misinterpret as a transient row,
                    //      dropping the user's edit. Weight-first ordering puts
                    //      every retraction before every insertion in the batch.
                    delta.changes.sort_by(|(a_row, a_w), (b_row, b_w)| {
                        a_w.cmp(b_w).then(a_row.rowid.cmp(&b_row.rowid))
                    });

                    let is_index_organized = !self.order_by.is_empty();

                    // Create view cursor when entering UpdateView state.
                    // ORDER BY views use an index btree (composite-keyed by
                    // sort cols + rowid); plain matviews use a table btree
                    // keyed by rowid.
                    let view_cursor: Box<BTreeCursor> = if is_index_organized {
                        Box::new(BTreeCursor::new_index_with_index_info(
                            pager.clone(),
                            main_data_root,
                            self.order_by.to_index_info(),
                            num_columns,
                        ))
                    } else {
                        Box::new(BTreeCursor::new_table(
                            pager.clone(),
                            main_data_root,
                            num_columns,
                        ))
                    };

                    self.commit_state = CommitState::UpdateView {
                        delta,
                        current_index: 0,
                        write_row_state: WriteRowView::new(),
                        write_row_index_state: WriteRowViewIndex::new(),
                        view_cursor,
                        is_index_organized,
                        num_columns,
                        view_order_by: self.order_by.clone(),
                    };
                }
                CommitState::UpdateView {
                    delta,
                    current_index,
                    write_row_state,
                    write_row_index_state,
                    view_cursor,
                    is_index_organized,
                    num_columns,
                    view_order_by,
                } => {
                    if *current_index >= delta.changes.len() {
                        self.commit_state = CommitState::Init;
                        let delta = std::mem::take(delta);
                        return Ok(IOResult::Done(delta));
                    } else {
                        let (row, weight) = delta.changes[*current_index].clone();
                        let nc = *num_columns;
                        let is_index = *is_index_organized;

                        // If we're starting a new row, we need a fresh cursor
                        // due to btree cursor state machine limitations
                        let needs_fresh = if is_index {
                            matches!(write_row_index_state, WriteRowViewIndex::GetRecord { .. })
                        } else {
                            matches!(write_row_state, WriteRowView::GetRecord)
                        };
                        if needs_fresh {
                            *view_cursor = if is_index {
                                Box::new(BTreeCursor::new_index_with_index_info(
                                    pager.clone(),
                                    main_data_root,
                                    view_order_by.to_index_info(),
                                    nc,
                                ))
                            } else {
                                Box::new(BTreeCursor::new_table(pager.clone(), main_data_root, nc))
                            };
                        }

                        if is_index {
                            let (composite_seek_key, full_record) = Self::build_composite_keys(
                                &row.values,
                                row.rowid,
                                nc,
                                &view_order_by.columns,
                                weight,
                            )?;
                            return_and_restore_if_io!(
                                &mut self.commit_state,
                                state,
                                write_row_index_state.write_row(
                                    view_cursor,
                                    &composite_seek_key,
                                    &full_record,
                                    weight,
                                )
                            );
                        } else {
                            // Build the view row format: row values + weight
                            let key = SeekKey::TableRowId(row.rowid);
                            let row_values = row.values.clone();
                            let build_fn = move |final_weight: isize| -> Vec<Value> {
                                let mut values = row_values.to_vec();
                                values.push(Value::from_i64(final_weight as i64));
                                values
                            };

                            return_and_restore_if_io!(
                                &mut self.commit_state,
                                state,
                                write_row_state.write_row(view_cursor, key, build_fn, weight)
                            );
                        }

                        // Move to next row
                        let delta = std::mem::take(delta);
                        // Take ownership of view_cursor - we'll create a new one for next row if needed.
                        // The replacement must match the btree page format.
                        let placeholder: Box<BTreeCursor> = if is_index {
                            Box::new(BTreeCursor::new_index_with_index_info(
                                pager.clone(),
                                main_data_root,
                                view_order_by.to_index_info(),
                                nc,
                            ))
                        } else {
                            Box::new(BTreeCursor::new_table(pager.clone(), main_data_root, nc))
                        };
                        let view_cursor = std::mem::replace(view_cursor, placeholder);

                        self.commit_state = CommitState::UpdateView {
                            delta,
                            current_index: *current_index + 1,
                            write_row_state: WriteRowView::new(),
                            write_row_index_state: WriteRowViewIndex::new(),
                            view_cursor,
                            is_index_organized: is_index,
                            num_columns: nc,
                            view_order_by: view_order_by.clone(),
                        };
                    }
                }
            }
        }
    }

    /// Build composite keys for index-organized matview writes.
    ///
    /// Returns (seek_key_record, full_record) where:
    /// - seek_key_record: [sort_val1, ..., sort_valN, rowid] — used for seeking
    /// - full_record: [sort_val1, ..., sort_valN, rowid, remaining_cols..., weight] — used for insert
    fn build_composite_keys(
        row_values: &[Value],
        rowid: i64,
        num_output_columns: usize,
        order_by_columns: &[(
            usize,
            turso_parser::ast::SortOrder,
            Option<turso_parser::ast::NullsOrder>,
        )],
        weight: isize,
    ) -> Result<(ImmutableRecord, ImmutableRecord)> {
        let num_sort_cols = order_by_columns.len();
        let num_data_cols = num_output_columns - 1; // minus the weight column
        let mut seek_vals = Vec::with_capacity(num_sort_cols + 1);
        let mut full_vals = Vec::with_capacity(num_output_columns);

        // Add sort column values
        for &(col_idx, _, _) in order_by_columns {
            let val = row_values.get(col_idx).cloned().unwrap_or(Value::Null);
            seek_vals.push(val.clone());
            full_vals.push(val);
        }

        // Add rowid
        seek_vals.push(Value::from_i64(rowid));
        full_vals.push(Value::from_i64(rowid));

        // Add remaining non-sort data columns
        let sort_indices: std::collections::HashSet<usize> =
            order_by_columns.iter().map(|&(i, _, _)| i).collect();
        for (i, val) in row_values.iter().enumerate() {
            if i < num_data_cols && !sort_indices.contains(&i) {
                full_vals.push(val.clone());
            }
        }

        // Add weight
        full_vals.push(Value::from_i64(weight as i64));

        let seek_record = ImmutableRecord::from_values(&seek_vals, seek_vals.len())?;
        let full_record = ImmutableRecord::from_values(&full_vals, full_vals.len())?;
        Ok((seek_record, full_record))
    }

    /// Execute a specific node in the circuit
    fn execute_node(
        &mut self,
        node_id: i64,
        pager: Arc<Pager>,
        execute_state: &mut ExecuteState,
        commit_operators: bool,
        cursors: &mut DbspStateCursors,
    ) -> Result<IOResult<Delta>> {
        loop {
            match execute_state {
                ExecuteState::Uninitialized => {
                    panic!("Trying to execute an uninitialized ExecuteState state machine");
                }
                ExecuteState::Init { input_data } => {
                    let node = self
                        .nodes
                        .get(&node_id)
                        .ok_or_else(|| LimboError::ParseError("Node not found".to_string()))?;

                    // Check for special node types
                    match &node.operator {
                        DbspOperator::Input { name, .. } => {
                            // Input nodes get their delta directly from input_data
                            let delta = input_data.get(name);
                            *execute_state = ExecuteState::ProcessingNode {
                                eval_state: Box::new(EvalState::Init {
                                    deltas: delta.into(),
                                }),
                            };
                        }
                        DbspOperator::Literal { .. } => {
                            // Literal nodes generate their own data, no external input needed
                            *execute_state = ExecuteState::ProcessingNode {
                                eval_state: Box::new(EvalState::Init {
                                    deltas: DeltaPair::default(),
                                }),
                            };
                        }
                        DbspOperator::Recursive { name, .. } => {
                            // Recursive nodes need special fixed-point execution
                            // inputs = [base_case_id, recursive_step_id, delay_id]
                            let inputs = node.inputs.clone();
                            if inputs.len() != 3 {
                                return Err(LimboError::ParseError(format!(
                                    "Recursive node '{}' must have exactly 3 inputs, found {}",
                                    name,
                                    inputs.len()
                                )));
                            }
                            let base_case_id = inputs[0];
                            let recursive_step_id = inputs[1];
                            // delay_input_id is inputs[2] but we don't need it - we use delay_name

                            let input_data = std::mem::take(input_data);
                            *execute_state = ExecuteState::ProcessingRecursive {
                                node_id,
                                base_case_id,
                                recursive_step_id,
                                delay_name: name.clone(),
                                sub_state: Box::new(ExecuteState::Init {
                                    input_data: input_data.clone(),
                                }),
                                input_data,
                                temp_cursors: None,
                            };
                        }
                        _ => {
                            // Non-input nodes need to process their inputs
                            let input_data = std::mem::take(input_data);
                            let input_node_ids = node.inputs.clone();

                            let input_states: Vec<(i64, ExecuteState)> = input_node_ids
                                .iter()
                                .map(|&input_id| {
                                    (
                                        input_id,
                                        ExecuteState::Init {
                                            input_data: input_data.clone(),
                                        },
                                    )
                                })
                                .collect();

                            *execute_state = ExecuteState::ProcessingInputs {
                                input_states,
                                current_index: 0,
                                input_deltas: Vec::new(),
                                temp_cursors: None,
                            };
                        }
                    }
                }
                ExecuteState::ProcessingInputs {
                    input_states,
                    current_index,
                    input_deltas,
                    temp_cursors,
                } => {
                    if *current_index >= input_states.len() {
                        // All inputs processed
                        let left_delta = input_deltas.first().cloned().unwrap_or_else(Delta::new);
                        let right_delta = input_deltas.get(1).cloned().unwrap_or_else(Delta::new);

                        *execute_state = ExecuteState::ProcessingNode {
                            eval_state: Box::new(EvalState::Init {
                                deltas: DeltaPair::new(left_delta, right_delta),
                            }),
                        };
                    } else {
                        // Get the (node_id, state) pair for the current index
                        let (input_node_id, input_state) = &mut input_states[*current_index];

                        // Diamond-DAG memo: if a sibling consumer already
                        // evaluated this subtree in this run, reuse the delta
                        // instead of re-running stateful operators (which would
                        // observe their own first-run side effects on btree
                        // state and emit a *different* delta).
                        if let Some(cached) = self.exec_node_cache.get(input_node_id) {
                            input_deltas.push(cached.clone());
                            *current_index += 1;
                            *temp_cursors = None;
                            continue;
                        }

                        // Reuse persisted cursors across I/O yields so operator
                        // state (e.g. WriteRow::InsertIndex { sought: true })
                        // stays consistent with the cursor's page stack position.
                        if temp_cursors.is_none() {
                            *temp_cursors = Some(Box::new(self.new_state_cursors(pager.clone())?));
                        }
                        let tc = temp_cursors.as_mut().unwrap();

                        let delta = return_if_io!(self.execute_node(
                            *input_node_id,
                            pager.clone(),
                            input_state,
                            commit_operators,
                            tc
                        ));
                        input_deltas.push(delta);
                        *current_index += 1;
                        // Reset cursors for next input's subtree
                        *temp_cursors = None;
                    }
                }
                ExecuteState::ProcessingNode { eval_state } => {
                    // Get mutable reference to node for eval
                    let node = self
                        .nodes
                        .get_mut(&node_id)
                        .ok_or_else(|| LimboError::ParseError("Node not found".to_string()))?;

                    let output_delta =
                        return_if_io!(node.process_node(eval_state, commit_operators, cursors));
                    // Memoise so sibling consumers in the same circuit run
                    // see the *same* delta — see DbspCircuit::exec_node_cache.
                    self.exec_node_cache.insert(node_id, output_delta.clone());
                    return Ok(IOResult::Done(output_delta));
                }
                ExecuteState::ProcessingRecursive {
                    node_id,
                    base_case_id,
                    recursive_step_id,
                    delay_name,
                    sub_state,
                    input_data,
                    temp_cursors,
                } => {
                    let delay_key = format!("__delay_{delay_name}");
                    let nid = *node_id;

                    let state = {
                        let node = self
                            .nodes
                            .get(&nid)
                            .ok_or_else(|| LimboError::ParseError("Node not found".to_string()))?;
                        let recursive_op = node
                            .executable
                            .as_any()
                            .downcast_ref::<RecursiveOperator>()
                            .ok_or_else(|| {
                                LimboError::ParseError("Expected RecursiveOperator".to_string())
                            })?;
                        recursive_op.state().clone()
                    };

                    // If the operator was previously Done, reset to Init for re-execution.
                    // This is intentional for incremental updates: when source tables change,
                    // we re-run fixed-point iteration with the new deltas. The key insight is
                    // that RecursiveOperator preserves seen_counts/seen_rows hash maps across
                    // runs, so filter_new_rows() only emits deltas for values whose multiplicity
                    // changed (e.g., new inserts or deletions). This prevents re-computing the
                    // entire transitive closure and ensures we only propagate the incremental
                    // changes through the recursion. Note: accumulated_output is reset by
                    // initialize_with_base(), so finalize() having taken it is fine.
                    let state = if matches!(state, RecursiveState::Done) {
                        RecursiveState::Init
                    } else {
                        state
                    };

                    match state {
                        RecursiveState::Init => {
                            if temp_cursors.is_none() {
                                *temp_cursors =
                                    Some(Box::new(self.new_state_cursors(pager.clone())?));
                            }
                            let tc = temp_cursors.as_mut().unwrap();

                            // Each recursive iteration is a *separate pass*
                            // through the body, so the diamond-DAG memo must
                            // not leak deltas from one iteration into another.
                            // Clear before the base case (first pass) and again
                            // before each recursive step.
                            self.exec_node_cache.clear();

                            // Execute the base case
                            let base_delta = return_if_io!(self.execute_node(
                                *base_case_id,
                                pager.clone(),
                                sub_state,
                                commit_operators,
                                tc
                            ));
                            *temp_cursors = None;

                            // Get the operator and initialize it
                            let node = self.nodes.get_mut(&nid).ok_or_else(|| {
                                LimboError::ParseError("Node not found".to_string())
                            })?;
                            let recursive_op = node
                                .executable
                                .as_any_mut()
                                .downcast_mut::<RecursiveOperator>()
                                .ok_or_else(|| {
                                    LimboError::ParseError("Expected RecursiveOperator".to_string())
                                })?;

                            let delay_delta = recursive_op.initialize_with_base(base_delta)?;
                            recursive_op.start_iteration();

                            // Set the delay value in input_data for the recursive step
                            input_data.insert(delay_key, delay_delta);

                            // Move to recursive step
                            **sub_state = ExecuteState::Init {
                                input_data: input_data.clone(),
                            };
                        }
                        RecursiveState::BaseComplete => {
                            let node = self.nodes.get_mut(&nid).ok_or_else(|| {
                                LimboError::ParseError("Node not found".to_string())
                            })?;
                            let recursive_op = node
                                .executable
                                .as_any_mut()
                                .downcast_mut::<RecursiveOperator>()
                                .ok_or_else(|| {
                                    LimboError::ParseError("Expected RecursiveOperator".to_string())
                                })?;
                            recursive_op.start_iteration();
                            **sub_state = ExecuteState::Init {
                                input_data: input_data.clone(),
                            };
                        }
                        RecursiveState::Iterating { iteration } => {
                            if temp_cursors.is_none() {
                                *temp_cursors =
                                    Some(Box::new(self.new_state_cursors(pager.clone())?));
                            }
                            let tc = temp_cursors.as_mut().unwrap();

                            // Fresh memo per recursive iteration — see the
                            // matching clear in `RecursiveState::Init`.
                            self.exec_node_cache.clear();

                            let step_delta = return_if_io!(self.execute_node(
                                *recursive_step_id,
                                pager.clone(),
                                sub_state,
                                commit_operators,
                                tc
                            ));
                            *temp_cursors = None;

                            // Get the operator and process iteration result
                            let node = self.nodes.get_mut(&nid).ok_or_else(|| {
                                LimboError::ParseError("Node not found".to_string())
                            })?;
                            let recursive_op = node
                                .executable
                                .as_any_mut()
                                .downcast_mut::<RecursiveOperator>()
                                .ok_or_else(|| {
                                    LimboError::ParseError("Expected RecursiveOperator".to_string())
                                })?;

                            let step_result = recursive_op.process_iteration_result(step_delta)?;

                            if step_result.done {
                                return Ok(IOResult::Done(recursive_op.finalize()));
                            }

                            // Update delay value for next iteration
                            // The delay should contain the NEW values from this iteration
                            input_data.insert(delay_key.clone(), step_result.delta_for_delay);
                            // After the first iteration, clear base data from input_data.
                            // In the commit path, subsequent iterations read from btree stored
                            // state. Clearing base data avoids keeping unnecessary state.
                            //
                            // EXCEPTIONS — base data must be preserved when:
                            // (a) One-shot mode: no btree storage, JoinOperator needs base data.
                            // (b) Execute (non-commit) path: used by MaterializedViewCursor for
                            //     uncommitted transaction reads. The btree doesn't contain
                            //     uncommitted data, so the JOIN needs the full delta for every
                            //     iteration. Per DBSP theory (Budiu et al., VLDB 2023 §5.3),
                            //     the input stream σ must be available to ALL fixed-point
                            //     iterations, not just the first.
                            if iteration == 1 && !self.is_one_shot() && commit_operators {
                                input_data.clear_base();
                            }

                            // Move to next iteration
                            **sub_state = ExecuteState::Init {
                                input_data: input_data.clone(),
                            };
                        }
                        RecursiveState::Done => {
                            let node = self.nodes.get_mut(&nid).ok_or_else(|| {
                                LimboError::ParseError("Node not found".to_string())
                            })?;
                            let recursive_op = node
                                .executable
                                .as_any_mut()
                                .downcast_mut::<RecursiveOperator>()
                                .ok_or_else(|| {
                                    LimboError::ParseError("Expected RecursiveOperator".to_string())
                                })?;
                            return Ok(IOResult::Done(recursive_op.finalize()));
                        }
                    }
                }
            }
        }
    }
}

impl Display for DbspCircuit {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        writeln!(f, "DBSP Circuit:")?;
        if let Some(root_id) = self.root {
            self.fmt_node(f, root_id, 0)?;
        }
        Ok(())
    }
}

impl DbspCircuit {
    fn fmt_node(&self, f: &mut Formatter, node_id: i64, depth: usize) -> fmt::Result {
        let indent = "  ".repeat(depth);
        if let Some(node) = self.nodes.get(&node_id) {
            match &node.operator {
                DbspOperator::Filter { predicate } => {
                    writeln!(f, "{indent}Filter[{node_id}]: {predicate:?}")?;
                }
                DbspOperator::Projection { exprs, .. } => {
                    writeln!(f, "{indent}Projection[{node_id}]: {exprs:?}")?;
                }
                DbspOperator::Aggregate {
                    group_exprs,
                    aggr_exprs,
                    ..
                } => {
                    writeln!(
                        f,
                        "{indent}Aggregate[{node_id}]: GROUP BY {group_exprs:?}, AGGR {aggr_exprs:?}"
                    )?;
                }
                DbspOperator::Join {
                    join_type,
                    on_exprs,
                    ..
                } => {
                    writeln!(f, "{indent}Join[{node_id}]: {join_type:?} ON {on_exprs:?}")?;
                }
                DbspOperator::Input { name, .. } => {
                    writeln!(f, "{indent}Input[{node_id}]: {name}")?;
                }
                DbspOperator::Literal { schema } => {
                    writeln!(
                        f,
                        "{indent}Literal[{node_id}]: {} columns",
                        schema.columns.len()
                    )?;
                }
                DbspOperator::Merge { schema } => {
                    writeln!(
                        f,
                        "{indent}Merge[{node_id}]: UNION/Recursive (schema: {} columns)",
                        schema.columns.len()
                    )?;
                }
                DbspOperator::Antijoin {
                    schema,
                    right_column_count,
                } => {
                    writeln!(
                        f,
                        "{indent}Antijoin[{node_id}]: LEFT JOIN antijoin half (R cols: {}, schema: {})",
                        right_column_count,
                        schema.columns.len()
                    )?;
                }
                DbspOperator::Distinct { schema } => {
                    writeln!(
                        f,
                        "{indent}Distinct[{node_id}]: (schema: {} columns)",
                        schema.columns.len()
                    )?;
                }
                DbspOperator::Recursive {
                    name,
                    max_iterations,
                    union_all,
                    schema,
                } => {
                    let union_mode_str = if *union_all { "UNION ALL" } else { "UNION" };
                    writeln!(
                        f,
                        "{indent}Recursive[{node_id}]: {} (max_iter={}, mode={}, {} columns)",
                        name,
                        max_iterations,
                        union_mode_str,
                        schema.columns.len()
                    )?;
                }
            }

            for input_id in &node.inputs {
                self.fmt_node(f, *input_id, depth + 1)?;
            }
        }
        Ok(())
    }
}

/// Compiler from LogicalPlan to DBSP Circuit
pub struct DbspCompiler {
    circuit: DbspCircuit,
    /// Maps recursive CTE names to their delay input node IDs
    /// Used during compilation to resolve RecursiveCTERef nodes
    recursive_cte_refs: HashMap<String, i64>,
}

impl DbspCompiler {
    /// Create a new DBSP compiler
    pub fn new(
        main_data_root: i64,
        internal_state_root: i64,
        internal_state_index_root: i64,
        order_by: super::view::MatviewOrderBy,
        limit: Option<i64>,
    ) -> Self {
        Self {
            circuit: DbspCircuit::new(
                main_data_root,
                internal_state_root,
                internal_state_index_root,
                order_by,
                limit,
            ),
            recursive_cte_refs: HashMap::default(),
        }
    }

    /// Resolve join condition columns to determine which side each column belongs to.
    ///
    /// Split a LogicalExpr into AND-conjuncts.
    fn split_conjuncts(expr: &LogicalExpr) -> Vec<LogicalExpr> {
        match expr {
            LogicalExpr::BinaryExpr {
                left,
                op: BinaryOperator::And,
                right,
            } => {
                let mut result = Self::split_conjuncts(left);
                result.extend(Self::split_conjuncts(right));
                result
            }
            other => vec![other.clone()],
        }
    }

    /// Collect all Column references from a LogicalExpr.
    fn collect_column_refs(expr: &LogicalExpr) -> Vec<&Column> {
        match expr {
            LogicalExpr::Column(col) => vec![col],
            LogicalExpr::BinaryExpr { left, right, .. } => {
                let mut refs = Self::collect_column_refs(left);
                refs.extend(Self::collect_column_refs(right));
                refs
            }
            LogicalExpr::UnaryExpr { expr, .. } => Self::collect_column_refs(expr),
            LogicalExpr::IsNull { expr, .. } => Self::collect_column_refs(expr),
            LogicalExpr::Like { expr, pattern, .. } => {
                let mut refs = Self::collect_column_refs(expr);
                refs.extend(Self::collect_column_refs(pattern));
                refs
            }
            LogicalExpr::Between {
                expr, low, high, ..
            } => {
                let mut refs = Self::collect_column_refs(expr);
                refs.extend(Self::collect_column_refs(low));
                refs.extend(Self::collect_column_refs(high));
                refs
            }
            LogicalExpr::InList { expr, list, .. } => {
                let mut refs = Self::collect_column_refs(expr);
                for item in list {
                    refs.extend(Self::collect_column_refs(item));
                }
                refs
            }
            LogicalExpr::ScalarFunction { args, .. } => {
                let mut refs = Vec::new();
                for arg in args {
                    refs.extend(Self::collect_column_refs(arg));
                }
                refs
            }
            LogicalExpr::Cast { expr, .. } | LogicalExpr::Alias { expr, .. } => {
                Self::collect_column_refs(expr)
            }
            LogicalExpr::Case {
                expr,
                when_then,
                else_expr,
            } => {
                let mut refs = Vec::new();
                if let Some(e) = expr {
                    refs.extend(Self::collect_column_refs(e));
                }
                for (w, t) in when_then {
                    refs.extend(Self::collect_column_refs(w));
                    refs.extend(Self::collect_column_refs(t));
                }
                if let Some(e) = else_expr {
                    refs.extend(Self::collect_column_refs(e));
                }
                refs
            }
            LogicalExpr::AggregateFunction { args, .. } => {
                let mut refs = Vec::new();
                for arg in args {
                    refs.extend(Self::collect_column_refs(arg));
                }
                refs
            }
            LogicalExpr::Literal(_)
            | LogicalExpr::InSubquery { .. }
            | LogicalExpr::Exists { .. }
            | LogicalExpr::ScalarSubquery(_) => vec![],
        }
    }

    /// Classify which side of a join a filter expression references.
    fn classify_filter_side(
        expr: &LogicalExpr,
        left_schema: &LogicalSchema,
        right_schema: &LogicalSchema,
    ) -> FilterSide {
        let cols = Self::collect_column_refs(expr);
        let mut refs_left = false;
        let mut refs_right = false;

        for col in &cols {
            let in_left = left_schema
                .find_column(&col.name, col.table.as_deref())
                .is_some();
            let in_right = right_schema
                .find_column(&col.name, col.table.as_deref())
                .is_some();
            if in_left {
                refs_left = true;
            }
            if in_right {
                refs_right = true;
            }
        }

        match (refs_left, refs_right) {
            (true, false) => FilterSide::LeftOnly,
            (false, true) => FilterSide::RightOnly,
            _ => FilterSide::Cross,
        }
    }

    /// Add a filter node on top of an existing node.
    fn add_filter_node(
        &mut self,
        input_id: i64,
        predicate: &LogicalExpr,
        schema: &LogicalSchema,
    ) -> Result<i64> {
        let dbsp_predicate = Self::compile_expr(predicate)?;
        let filter_predicate = Self::compile_filter_predicate(predicate, schema)?;
        let executable: Box<dyn IncrementalOperator> =
            Box::new(FilterOperator::new(filter_predicate));
        let node_id = self.circuit.add_node(
            DbspOperator::Filter {
                predicate: dbsp_predicate,
            },
            vec![input_id],
            executable,
        );
        Ok(node_id)
    }

    /// Returns (left_column, left_index, right_column, right_index) where:
    /// - left_column/right_column are the Column references
    /// - left_index/right_index are the column indices in their respective schemas
    ///
    /// Handles cases where:
    /// - Columns are in normal order (left table column = right table column)
    /// - Columns are swapped (right table column = left table column)
    /// - One or both columns have table qualifiers
    /// - Column names exist in both tables but are disambiguated by qualifiers
    fn resolve_join_columns(
        first_col: &Column,
        second_col: &Column,
        left_schema: &LogicalSchema,
        right_schema: &LogicalSchema,
    ) -> Result<(Column, usize, Column, usize)> {
        // Check all four possibilities to handle ambiguous column names
        let first_in_left = left_schema.find_column(&first_col.name, first_col.table.as_deref());
        let first_in_right = right_schema.find_column(&first_col.name, first_col.table.as_deref());
        let second_in_left = left_schema.find_column(&second_col.name, second_col.table.as_deref());
        let second_in_right =
            right_schema.find_column(&second_col.name, second_col.table.as_deref());

        // Determine the correct pairing: one column must be from left, one from right
        if first_in_left.is_some() && second_in_right.is_some() {
            // first is from left, second is from right
            let (left_idx, _) = first_in_left.ok_or_else(|| {
                LimboError::InternalError("first_in_left should exist".to_string())
            })?;
            let (right_idx, _) = second_in_right.ok_or_else(|| {
                LimboError::InternalError("second_in_right should exist".to_string())
            })?;
            Ok((first_col.clone(), left_idx, second_col.clone(), right_idx))
        } else if first_in_right.is_some() && second_in_left.is_some() {
            // first is from right, second is from left
            let (left_idx, _) = second_in_left.ok_or_else(|| {
                LimboError::InternalError("second_in_left should exist".to_string())
            })?;
            let (right_idx, _) = first_in_right.ok_or_else(|| {
                LimboError::InternalError("first_in_right should exist".to_string())
            })?;
            Ok((second_col.clone(), left_idx, first_col.clone(), right_idx))
        } else {
            // Provide specific error messages for different failure cases
            if first_in_left.is_none() && first_in_right.is_none() {
                Err(LimboError::ParseError(format!(
                    "Join condition column '{}' not found in either input",
                    first_col.name
                )))
            } else if second_in_left.is_none() && second_in_right.is_none() {
                Err(LimboError::ParseError(format!(
                    "Join condition column '{}' not found in either input",
                    second_col.name
                )))
            } else {
                Err(LimboError::ParseError(format!(
                    "Join condition columns '{}' and '{}' must come from different input tables",
                    first_col.name, second_col.name
                )))
            }
        }
    }

    /// Compile a logical plan to a DBSP circuit
    pub fn compile(mut self, plan: &LogicalPlan) -> Result<DbspCircuit> {
        // First, inline any CTEs in the plan
        let inlined_plan = plan.inline_ctes()?;
        let root_id = self.compile_plan(&inlined_plan)?;
        let output_schema = inlined_plan.schema().clone();
        self.circuit.set_root(root_id, output_schema);
        Ok(self.circuit)
    }

    /// Recursively compile a logical plan node
    fn compile_plan(&mut self, plan: &LogicalPlan) -> Result<i64> {
        match plan {
            LogicalPlan::Projection(proj) => {
                // Compile the input first
                let input_id = self.compile_plan(&proj.input)?;

                // Get input column names for the ProjectOperator
                let input_schema = proj.input.schema();
                let input_column_names: Vec<String> = input_schema
                    .columns
                    .iter()
                    .map(|col| col.name.clone())
                    .collect();

                // Convert logical expressions to DBSP expressions
                let dbsp_exprs = proj
                    .exprs
                    .iter()
                    .map(Self::compile_expr)
                    .collect::<Result<Vec<_>>>()?;

                // Compile logical expressions to CompiledExpressions
                let mut compiled_exprs = Vec::new();
                let mut aliases = Vec::new();
                for expr in &proj.exprs {
                    let (compiled, alias) = Self::compile_expression(expr, input_schema)?;
                    compiled_exprs.push(compiled);
                    aliases.push(alias);
                }

                // Get output column names from the projection schema
                let output_column_names: Vec<String> = proj
                    .schema
                    .columns
                    .iter()
                    .map(|col| col.name.clone())
                    .collect();

                // Create the ProjectOperator
                let executable: Box<dyn IncrementalOperator> =
                    Box::new(ProjectOperator::from_compiled(
                        compiled_exprs,
                        aliases,
                        input_column_names,
                        output_column_names,
                    )?);

                // Create projection node
                let node_id = self.circuit.add_node(
                    DbspOperator::Projection {
                        exprs: dbsp_exprs,
                        schema: proj.schema.clone(),
                    },
                    vec![input_id],
                    executable,
                );
                Ok(node_id)
            }
            LogicalPlan::Filter(filter) => {
                // Compile the input first
                let input_id = self.compile_plan(&filter.input)?;

                // Get input schema for column resolution
                let input_schema = filter.input.schema();

                // Check if the predicate contains expressions that need to be computed
                if Self::predicate_needs_projection(&filter.predicate) {
                    // Complex expression in WHERE clause - need to add projection first
                    // 1. Create projection that adds the computed expression as a new column

                    // First, get all existing columns
                    let mut dbsp_exprs = Vec::new();
                    for col in &input_schema.columns {
                        dbsp_exprs.push(DbspExpr::Column(col.name.clone()));
                    }

                    // Now add the expression as a computed column
                    let temp_column_name = "__temp_filter_expr";
                    let computed_expr = Self::extract_expression_from_predicate(&filter.predicate)?;

                    // Compile the projection expressions.
                    //
                    // The passthrough part is a pure 1:1 copy of
                    // `input_schema.columns`, so bind it POSITIONALLY. Rebuilding
                    // it as unqualified `LogicalExpr::Column`s and re-resolving
                    // those by name is lossy: when this filter's input is a join
                    // whose two sides carry columns with the same bare name (a
                    // self-join, or a recursive CTE mirroring its base table),
                    // every such reference resolves to the FIRST match, silently
                    // wiring the output column to the wrong side.
                    let input_len = input_schema.columns.len();
                    let mut compiled_exprs = Vec::new();
                    let mut aliases = Vec::new();
                    let mut output_names = Vec::new();
                    for (i, col) in input_schema.columns.iter().enumerate() {
                        compiled_exprs.push(CompiledExpression {
                            executor: ExpressionExecutor::Trivial(TrivialExpression::Column(i)),
                            input_count: input_len,
                        });
                        aliases.push(None);
                        output_names.push(col.name.clone());
                    }
                    let (compiled_computed, _alias) =
                        Self::compile_expression(&computed_expr, input_schema)?;
                    compiled_exprs.push(compiled_computed);
                    aliases.push(Some(temp_column_name.to_string()));
                    output_names.push(temp_column_name.to_string());

                    // Get input column names for ProjectOperator
                    let input_column_names: Vec<String> = input_schema
                        .columns
                        .iter()
                        .map(|col| col.name.clone())
                        .collect();

                    // Create projection operator
                    let proj_executable: Box<dyn IncrementalOperator> =
                        Box::new(ProjectOperator::from_compiled(
                            compiled_exprs.clone(),
                            aliases.clone(),
                            input_column_names,
                            output_names.clone(),
                        )?);

                    // Create updated schema for the projection output
                    let mut proj_schema_columns = input_schema.columns.clone();
                    proj_schema_columns.push(ColumnInfo {
                        name: temp_column_name.to_string(),
                        table: None,
                        database: None,
                        table_alias: None,
                        ty: Type::Integer, // Computed expressions default to Integer
                    });
                    let proj_schema = SchemaRef::new(LogicalSchema {
                        columns: proj_schema_columns,
                    });

                    // Add projection node
                    let proj_id = self.circuit.add_node(
                        DbspOperator::Projection {
                            exprs: dbsp_exprs.clone(),
                            schema: proj_schema.clone(),
                        },
                        vec![input_id],
                        proj_executable,
                    );

                    // Now create a filter that replaces the complex expression with the temp column
                    // but keeps all other conditions intact
                    let replaced_predicate =
                        Self::replace_complex_with_temp(&filter.predicate, temp_column_name)?;
                    let filter_predicate =
                        Self::compile_filter_predicate(&replaced_predicate, &proj_schema)?;

                    let filter_executable: Box<dyn IncrementalOperator> =
                        Box::new(FilterOperator::new(filter_predicate));

                    // Create filter node
                    let filter_id = self.circuit.add_node(
                        DbspOperator::Filter {
                            predicate: Self::compile_expr(&replaced_predicate)?,
                        },
                        vec![proj_id],
                        filter_executable,
                    );

                    // Finally, project again to remove the temporary column
                    let mut final_exprs = Vec::new();
                    let mut final_aliases = Vec::new();
                    let mut final_names = Vec::new();
                    let mut final_dbsp_exprs = Vec::new();

                    for (i, column) in input_schema.columns.iter().enumerate() {
                        let col_name = &column.name;
                        final_exprs.push(compiled_exprs[i].clone());
                        final_aliases.push(None);
                        final_names.push(col_name.clone());
                        final_dbsp_exprs.push(DbspExpr::Column(col_name.clone()));
                    }

                    // Input names for the final projection include the temp column
                    let filter_output_names = output_names.clone();

                    let final_proj_executable: Box<dyn IncrementalOperator> =
                        Box::new(ProjectOperator::from_compiled(
                            final_exprs,
                            final_aliases,
                            filter_output_names,
                            final_names.clone(),
                        )?);

                    let final_id = self.circuit.add_node(
                        DbspOperator::Projection {
                            exprs: final_dbsp_exprs,
                            schema: input_schema.clone(), // Back to original schema
                        },
                        vec![filter_id],
                        final_proj_executable,
                    );

                    Ok(final_id)
                } else {
                    // Simple filter - use existing implementation
                    // Convert predicate to DBSP expression
                    let dbsp_predicate = Self::compile_expr(&filter.predicate)?;

                    // Convert to FilterPredicate
                    let filter_predicate =
                        Self::compile_filter_predicate(&filter.predicate, input_schema)?;

                    // Create executable operator
                    let executable: Box<dyn IncrementalOperator> =
                        Box::new(FilterOperator::new(filter_predicate));

                    // Create filter node
                    let node_id = self.circuit.add_node(
                        DbspOperator::Filter {
                            predicate: dbsp_predicate,
                        },
                        vec![input_id],
                        executable,
                    );
                    Ok(node_id)
                }
            }
            LogicalPlan::Aggregate(agg) => {
                // Compile the input first
                let input_id = self.compile_plan(&agg.input)?;

                // Get input column names
                let input_schema = agg.input.schema();
                let input_column_names: Vec<String> = input_schema
                    .columns
                    .iter()
                    .map(|col| col.name.clone())
                    .collect();

                // Compile group by expressions to column indices
                let mut group_by_indices = Vec::new();
                let mut dbsp_group_exprs = Vec::new();
                for expr in &agg.group_expr {
                    // For now, only support simple column references in GROUP BY
                    if let LogicalExpr::Column(col) = expr {
                        // Find the column index in the input schema using qualified lookup
                        let (col_idx, _) = input_schema
                            .find_column(&col.name, col.table.as_deref())
                            .ok_or_else(|| {
                                LimboError::ParseError(format!(
                                    "GROUP BY column '{}' not found in input",
                                    col.name
                                ))
                            })?;
                        group_by_indices.push(col_idx);
                        dbsp_group_exprs.push(DbspExpr::Column(col.name.clone()));
                    } else {
                        return Err(LimboError::ParseError(
                            "Only column references are supported in GROUP BY for incremental views".to_string()
                        ));
                    }
                }

                // Compile aggregate expressions (both DISTINCT and regular).
                // `aggregate_filters` is a parallel vec, one entry per
                // aggregate, recording the optional FILTER predicate.
                let mut aggregate_functions = Vec::new();
                let mut aggregate_filters: Vec<Option<FilterPredicate>> = Vec::new();
                for expr in &agg.aggr_expr {
                    if let LogicalExpr::AggregateFunction {
                        fun,
                        args,
                        distinct,
                        filter,
                    } = expr
                    {
                        use crate::function::AggFunc;
                        use crate::incremental::aggregate_operator::AggregateFunction;

                        // v1 limitation: reject FILTER on aggregates whose
                        // state machinery cannot honour the per-aggregate
                        // predicate. Two distinct cases:
                        //
                        //   (a) Count/Min/Max — `count` tracks group existence
                        //       and Min/Max are recomputed from the persisted
                        //       index, neither of which sees `filter_passes`.
                        //   (b) Count/Sum/Avg DISTINCT — distinct transitions
                        //       are computed from raw row values in
                        //       `detect_distinct_transitions`, before the
                        //       filter is consulted, so a row that fails the
                        //       filter would still flip distinct counts.
                        //
                        // GroupConcat[Distinct] and JsonGroupArray[Distinct]
                        // *do* honour `filter_passes` in apply_delta (see
                        // aggregate_operator.rs:1442-1469), so FILTER works
                        // for those even with DISTINCT. The C1 duplicate-
                        // column check below still catches divergent-filter
                        // collisions on the same column.
                        if filter.is_some() {
                            let always_unsupported = matches!(
                                fun,
                                AggFunc::Count | AggFunc::Count0 | AggFunc::Min | AggFunc::Max
                            );
                            let distinct_set_aggregate = *distinct
                                && !matches!(
                                    fun,
                                    AggFunc::JsonGroupArray
                                        | AggFunc::JsonbGroupArray
                                        | AggFunc::GroupConcat
                                        | AggFunc::StringAgg
                                );
                            if always_unsupported || distinct_set_aggregate {
                                return Err(LimboError::ParseError(format!(
                                    "FILTER not supported with {fun:?}{} in incremental views (v1 limitation)",
                                    if *distinct { " DISTINCT" } else { "" }
                                )));
                            }
                        }

                        match fun {
                            AggFunc::Count | AggFunc::Count0 => {
                                if *distinct {
                                    // COUNT(DISTINCT col)
                                    if args.is_empty() {
                                        return Err(LimboError::ParseError(
                                            "COUNT(DISTINCT) requires an argument".to_string(),
                                        ));
                                    }
                                    if let LogicalExpr::Column(col) = &args[0] {
                                        let (col_idx, _) = input_schema.find_column(&col.name, col.table.as_deref())
                                            .ok_or_else(|| LimboError::ParseError(
                                                format!("COUNT(DISTINCT) column '{}' not found in input", col.name)
                                            ))?;
                                        aggregate_functions
                                            .push(AggregateFunction::CountDistinct(col_idx));
                                    } else {
                                        return Err(LimboError::ParseError(
                                            "Only column references are supported in aggregate functions for incremental views".to_string()
                                        ));
                                    }
                                } else {
                                    aggregate_functions.push(AggregateFunction::Count);
                                }
                            }
                            AggFunc::Sum => {
                                if args.is_empty() {
                                    return Err(LimboError::ParseError(
                                        "SUM requires an argument".to_string(),
                                    ));
                                }
                                // Extract column index from the argument
                                if let LogicalExpr::Column(col) = &args[0] {
                                    let (col_idx, _) = input_schema
                                        .find_column(&col.name, col.table.as_deref())
                                        .ok_or_else(|| {
                                            LimboError::ParseError(format!(
                                                "SUM column '{}' not found in input",
                                                col.name
                                            ))
                                        })?;
                                    if *distinct {
                                        aggregate_functions
                                            .push(AggregateFunction::SumDistinct(col_idx));
                                    } else {
                                        aggregate_functions.push(AggregateFunction::Sum(col_idx));
                                    }
                                } else {
                                    return Err(LimboError::ParseError(
                                        "Only column references are supported in aggregate functions for incremental views".to_string()
                                    ));
                                }
                            }
                            AggFunc::Avg => {
                                if args.is_empty() {
                                    return Err(LimboError::ParseError(
                                        "AVG requires an argument".to_string(),
                                    ));
                                }
                                if let LogicalExpr::Column(col) = &args[0] {
                                    let (col_idx, _) = input_schema
                                        .find_column(&col.name, col.table.as_deref())
                                        .ok_or_else(|| {
                                            LimboError::ParseError(format!(
                                                "AVG column '{}' not found in input",
                                                col.name
                                            ))
                                        })?;
                                    if *distinct {
                                        aggregate_functions
                                            .push(AggregateFunction::AvgDistinct(col_idx));
                                    } else {
                                        aggregate_functions.push(AggregateFunction::Avg(col_idx));
                                    }
                                } else {
                                    return Err(LimboError::ParseError(
                                        "Only column references are supported in aggregate functions for incremental views".to_string()
                                    ));
                                }
                            }
                            AggFunc::Min => {
                                if args.is_empty() {
                                    return Err(LimboError::ParseError(
                                        "MIN requires an argument".to_string(),
                                    ));
                                }
                                if let LogicalExpr::Column(col) = &args[0] {
                                    let (col_idx, _) = input_schema
                                        .find_column(&col.name, col.table.as_deref())
                                        .ok_or_else(|| {
                                            LimboError::ParseError(format!(
                                                "MIN column '{}' not found in input",
                                                col.name
                                            ))
                                        })?;
                                    aggregate_functions.push(AggregateFunction::Min(col_idx));
                                } else {
                                    return Err(LimboError::ParseError(
                                        "Only column references are supported in MIN for incremental views".to_string()
                                    ));
                                }
                            }
                            AggFunc::Max => {
                                if args.is_empty() {
                                    return Err(LimboError::ParseError(
                                        "MAX requires an argument".to_string(),
                                    ));
                                }
                                if let LogicalExpr::Column(col) = &args[0] {
                                    let (col_idx, _) = input_schema
                                        .find_column(&col.name, col.table.as_deref())
                                        .ok_or_else(|| {
                                            LimboError::ParseError(format!(
                                                "MAX column '{}' not found in input",
                                                col.name
                                            ))
                                        })?;
                                    aggregate_functions.push(AggregateFunction::Max(col_idx));
                                } else {
                                    return Err(LimboError::ParseError(
                                        "Only column references are supported in MAX for incremental views".to_string()
                                    ));
                                }
                            }
                            AggFunc::GroupConcat | AggFunc::StringAgg => {
                                if args.is_empty() {
                                    return Err(LimboError::ParseError(
                                        "group_concat requires an argument".to_string(),
                                    ));
                                }
                                let col_idx = if let LogicalExpr::Column(col) = &args[0] {
                                    let (idx, _) = input_schema
                                        .find_column(&col.name, col.table.as_deref())
                                        .ok_or_else(|| {
                                            LimboError::ParseError(format!(
                                                "group_concat column '{}' not found in input",
                                                col.name
                                            ))
                                        })?;
                                    idx
                                } else {
                                    return Err(LimboError::ParseError(
                                        "Only column references are supported in aggregate functions for incremental views".to_string()
                                    ));
                                };
                                let separator = match args.get(1) {
                                    None => ",".to_string(),
                                    Some(LogicalExpr::Literal(crate::Value::Text(t))) => {
                                        t.as_str().to_string()
                                    }
                                    Some(LogicalExpr::Literal(crate::Value::Null)) => {
                                        return Err(LimboError::ParseError(
                                            "group_concat separator must be a non-NULL string literal in incremental views".to_string()
                                        ));
                                    }
                                    Some(_) => {
                                        return Err(LimboError::ParseError(
                                            "group_concat separator must be a string literal in incremental views".to_string()
                                        ));
                                    }
                                };
                                aggregate_functions.push(if *distinct {
                                    AggregateFunction::GroupConcatDistinct {
                                        col: col_idx,
                                        separator,
                                    }
                                } else {
                                    AggregateFunction::GroupConcat {
                                        col: col_idx,
                                        separator,
                                    }
                                });
                            }
                            #[cfg(feature = "json")]
                            AggFunc::JsonGroupArray | AggFunc::JsonbGroupArray => {
                                if args.is_empty() {
                                    return Err(LimboError::ParseError(
                                        "json_group_array requires an argument".to_string(),
                                    ));
                                }
                                let col_idx = if let LogicalExpr::Column(col) = &args[0] {
                                    let (idx, _) = input_schema
                                        .find_column(&col.name, col.table.as_deref())
                                        .ok_or_else(|| {
                                            LimboError::ParseError(format!(
                                                "json_group_array column '{}' not found in input",
                                                col.name
                                            ))
                                        })?;
                                    idx
                                } else {
                                    return Err(LimboError::ParseError(
                                        "Only column references are supported in aggregate functions for incremental views".to_string()
                                    ));
                                };
                                aggregate_functions.push(if *distinct {
                                    AggregateFunction::JsonGroupArrayDistinct(col_idx)
                                } else {
                                    AggregateFunction::JsonGroupArray(col_idx)
                                });
                            }
                            AggFunc::ArrayAgg => {
                                return Err(LimboError::ParseError(
                                    "array_agg is not supported in incremental views; use json_group_array instead".to_string()
                                ));
                            }
                            _ => {
                                return Err(LimboError::ParseError(format!(
                                    "Unsupported aggregate function in DBSP compiler: {fun:?}"
                                )));
                            }
                        }

                        // Compile FILTER predicate, if present, against the
                        // aggregate's input schema (post-pre-projection).
                        // `compile_filter_predicate` rejects predicate shapes
                        // it doesn't support with a clear error pointing at
                        // the FILTER clause.
                        let compiled_filter = if let Some(f) = filter {
                            Some(
                                Self::compile_filter_predicate(f, input_schema).map_err(|e| {
                                    LimboError::ParseError(format!(
                                        "FILTER expression on aggregate is unsupported: {e}"
                                    ))
                                })?,
                            )
                        } else {
                            None
                        };
                        aggregate_filters.push(compiled_filter);
                    } else {
                        return Err(LimboError::ParseError(
                            "Expected aggregate function in aggregate expressions".to_string(),
                        ));
                    }
                }

                // Reject duplicate column with mismatched filter (C1).
                // AggregateState is keyed by column index, so two aggregates
                // over the same column with different filters would silently
                // overwrite each other's state. Group aggregate indices by
                // their column index and bail if any group has divergent
                // filter shapes (None vs Some, or two Somes that differ).
                debug_assert_eq!(aggregate_functions.len(), aggregate_filters.len());
                let mut col_to_filter: std::collections::HashMap<
                    usize,
                    &Option<FilterPredicate>,
                > = std::collections::HashMap::new();
                let agg_col_idx = |af: &crate::incremental::aggregate_operator::AggregateFunction| -> Option<usize> {
                    use crate::incremental::aggregate_operator::AggregateFunction;
                    match af {
                        AggregateFunction::Count => None,
                        AggregateFunction::CountDistinct(c)
                        | AggregateFunction::Sum(c)
                        | AggregateFunction::SumDistinct(c)
                        | AggregateFunction::Avg(c)
                        | AggregateFunction::AvgDistinct(c)
                        | AggregateFunction::Min(c)
                        | AggregateFunction::Max(c)
                        | AggregateFunction::JsonGroupArray(c)
                        | AggregateFunction::JsonGroupArrayDistinct(c) => Some(*c),
                        AggregateFunction::GroupConcat { col, .. }
                        | AggregateFunction::GroupConcatDistinct { col, .. } => Some(*col),
                    }
                };
                for (af, filt) in aggregate_functions.iter().zip(aggregate_filters.iter()) {
                    if let Some(col_idx) = agg_col_idx(af) {
                        if let Some(existing) = col_to_filter.get(&col_idx) {
                            if *existing != filt {
                                return Err(LimboError::ParseError(format!(
                                    "FILTER on aggregate over column index {col_idx} \
                                     conflicts with another aggregate over the same column \
                                     with a different (or no) FILTER (v1 limitation)"
                                )));
                            }
                        } else {
                            col_to_filter.insert(col_idx, filt);
                        }
                    }
                }

                let operator_id = self.circuit.next_id;

                use crate::incremental::aggregate_operator::AggregateOperator;
                let executable: Box<dyn IncrementalOperator> = Box::new(AggregateOperator::new(
                    operator_id,
                    group_by_indices.clone(),
                    aggregate_functions.clone(),
                    input_column_names,
                    aggregate_filters,
                )?);

                let result_node_id = self.circuit.add_node(
                    DbspOperator::Aggregate {
                        group_exprs: dbsp_group_exprs,
                        aggr_exprs: aggregate_functions,
                        schema: agg.schema.clone(),
                    },
                    vec![input_id],
                    executable,
                );

                Ok(result_node_id)
            }
            LogicalPlan::Join(join) => {
                // Compile left and right inputs
                let mut left_id = self.compile_plan(&join.left)?;
                let mut right_id = self.compile_plan(&join.right)?;

                // Get schemas from inputs
                let left_schema = join.left.schema();
                let right_schema = join.right.schema();

                // Handle join filter conditions by pushing them down.
                // For LEFT JOIN this must be join-type-aware: pushing a LeftOnly
                // conjunct to the left input would drop L rows that should
                // null-pad. Only RightOnly is safe to push (equivalent to
                // LEFT JOIN (R WHERE p)). LeftOnly and Cross stay above the
                // join (applied to the merged output).
                let is_left_outer = matches!(join.join_type, LogicalJoinType::Left);
                let mut cross_side_filters = Vec::new();
                if let Some(ref filter) = join.filter {
                    let conjuncts = Self::split_conjuncts(filter);
                    for conjunct in conjuncts {
                        match Self::classify_filter_side(&conjunct, left_schema, right_schema) {
                            FilterSide::LeftOnly => {
                                if is_left_outer {
                                    // Pushing a LeftOnly ON-conjunct to L would
                                    // drop L rows that should still appear with
                                    // NULL R columns. Applying it post-merge
                                    // would *also* drop the null-padded rows
                                    // (they have non-NULL L columns and would
                                    // also fail the predicate). Correct
                                    // handling requires teaching Antijoin
                                    // to treat LeftOnly-failing L rows as
                                    // forced-unmatched. That's not in scope
                                    // for this PR — reject to keep semantics
                                    // safe.
                                    return Err(LimboError::ParseError(
                                        "LEFT JOIN with non-equijoin ON-conjuncts \
                                         referencing only the left side is not yet \
                                         supported in incremental views"
                                            .to_string(),
                                    ));
                                } else {
                                    left_id =
                                        self.add_filter_node(left_id, &conjunct, left_schema)?;
                                }
                            }
                            FilterSide::RightOnly => {
                                right_id =
                                    self.add_filter_node(right_id, &conjunct, right_schema)?;
                            }
                            FilterSide::Cross => {
                                if is_left_outer {
                                    return Err(LimboError::ParseError(
                                        "LEFT JOIN with non-equijoin ON-conjuncts \
                                         spanning both sides is not yet supported \
                                         in incremental views"
                                            .to_string(),
                                    ));
                                }
                                cross_side_filters.push(conjunct);
                            }
                        }
                    }
                }

                // Get column names from left and right
                let left_columns: Vec<String> = left_schema
                    .columns
                    .iter()
                    .map(|col| col.name.clone())
                    .collect();
                let right_columns: Vec<String> = right_schema
                    .columns
                    .iter()
                    .map(|col| col.name.clone())
                    .collect();
                let right_column_count = right_columns.len();

                // Check if we have at least one equijoin condition
                if join.on.is_empty() {
                    return Err(LimboError::ParseError(
                        "Joins in materialized views must have at least one equality condition."
                            .to_string(),
                    ));
                }

                // Extract join key indices from join conditions
                let mut left_key_indices = Vec::new();
                let mut right_key_indices = Vec::new();
                let mut dbsp_on_exprs = Vec::new();

                for (left_expr, right_expr) in &join.on {
                    if let (LogicalExpr::Column(first_col), LogicalExpr::Column(second_col)) =
                        (left_expr, right_expr)
                    {
                        let (actual_left_col, actual_left_idx, actual_right_col, actual_right_idx) =
                            Self::resolve_join_columns(
                                first_col,
                                second_col,
                                left_schema,
                                right_schema,
                            )?;

                        left_key_indices.push(actual_left_idx);
                        right_key_indices.push(actual_right_idx);

                        dbsp_on_exprs.push((
                            DbspExpr::Column(actual_left_col.name.clone()),
                            DbspExpr::Column(actual_right_col.name.clone()),
                        ));
                    } else {
                        return Err(LimboError::ParseError(
                            "Only simple column references are supported in join conditions for incremental views".to_string()
                        ));
                    }
                }

                // For LEFT JOIN, build a 3-operator subgraph:
                //   InnerJoinOperator (matched rows) ─┐
                //                                     ├─► MergeOperator (UNION ALL)
                //   AntijoinOperator (null-pad) ─────┘
                // Both children consume [left_id, right_id]. The MergeOperator's
                // output replaces the single Join node that an INNER JOIN would
                // produce.
                let mut node_id = match join.join_type {
                    LogicalJoinType::Left => {
                        // 1. Inner join sub-component (matched rows).
                        let inner_op_id = self.circuit.next_id;
                        let inner_executable: Box<dyn IncrementalOperator> =
                            Box::new(JoinOperator::new(
                                inner_op_id,
                                JoinType::Inner,
                                left_key_indices.clone(),
                                right_key_indices.clone(),
                                left_columns,
                                right_columns,
                            )?);
                        let inner_id = self.circuit.add_node(
                            DbspOperator::Join {
                                join_type: JoinType::Inner,
                                on_exprs: dbsp_on_exprs.clone(),
                                schema: join.schema.clone(),
                            },
                            vec![left_id, right_id],
                            inner_executable,
                        );

                        // 2. Antijoin operator (null-padded unmatched half).
                        let aj_op_id = self.circuit.next_id;
                        let aj_executable: Box<dyn IncrementalOperator> =
                            Box::new(AntijoinOperator::new(
                                aj_op_id,
                                left_key_indices.clone(),
                                right_key_indices.clone(),
                                right_column_count,
                            ));
                        let mc_id = self.circuit.add_node(
                            DbspOperator::Antijoin {
                                schema: join.schema.clone(),
                                right_column_count,
                            },
                            vec![left_id, right_id],
                            aj_executable,
                        );

                        // 3. UNION ALL the two outputs.
                        use crate::incremental::merge_operator::{MergeOperator, UnionMode};
                        let merge_op_id = self.circuit.next_id;
                        let merge_executable: Box<dyn IncrementalOperator> =
                            Box::new(MergeOperator::new(
                                merge_op_id,
                                UnionMode::All {
                                    left_table: format!("_inner_join_{inner_id}"),
                                    right_table: format!("_antijoin_{mc_id}"),
                                },
                            ));
                        self.circuit.add_node(
                            DbspOperator::Merge {
                                schema: join.schema.clone(),
                            },
                            vec![inner_id, mc_id],
                            merge_executable,
                        )
                    }
                    LogicalJoinType::Inner
                    | LogicalJoinType::Right
                    | LogicalJoinType::Full
                    | LogicalJoinType::Cross => {
                        let operator_join_type = match join.join_type {
                            LogicalJoinType::Inner => JoinType::Inner,
                            LogicalJoinType::Left => unreachable!(),
                            LogicalJoinType::Right => JoinType::Right,
                            LogicalJoinType::Full => JoinType::Full,
                            LogicalJoinType::Cross => JoinType::Cross,
                        };
                        let operator_id = self.circuit.next_id;
                        let executable: Box<dyn IncrementalOperator> = Box::new(JoinOperator::new(
                            operator_id,
                            operator_join_type.clone(),
                            left_key_indices,
                            right_key_indices,
                            left_columns,
                            right_columns,
                        )?);
                        self.circuit.add_node(
                            DbspOperator::Join {
                                join_type: operator_join_type,
                                on_exprs: dbsp_on_exprs,
                                schema: join.schema.clone(),
                            },
                            vec![left_id, right_id],
                            executable,
                        )
                    }
                };

                // Apply cross-side filters as a post-join filter
                if !cross_side_filters.is_empty() {
                    let combined = cross_side_filters
                        .into_iter()
                        .reduce(|a, b| LogicalExpr::BinaryExpr {
                            left: Box::new(a),
                            op: BinaryOperator::And,
                            right: Box::new(b),
                        })
                        .unwrap();
                    node_id = self.add_filter_node(node_id, &combined, &join.schema)?;
                }

                Ok(node_id)
            }
            LogicalPlan::TableScan(scan) => {
                // Create input node with InputOperator for uniform handling
                let executable: Box<dyn IncrementalOperator> =
                    Box::new(InputOperator::new(scan.table_name.clone()));

                let node_id = self.circuit.add_node(
                    DbspOperator::Input {
                        name: scan.table_name.clone(),
                        schema: scan.schema.clone(),
                    },
                    vec![],
                    executable,
                );
                Ok(node_id)
            }
            LogicalPlan::Union(union) => {
                // Handle UNION and UNION ALL
                self.compile_union(union)
            }
            LogicalPlan::Distinct(distinct) => {
                // DISTINCT is implemented as GROUP BY all columns with a special aggregate
                let input_id = self.compile_plan(&distinct.input)?;
                let input_schema = distinct.input.schema();

                // Create GROUP BY indices for all columns
                let group_by: Vec<usize> = (0..input_schema.columns.len()).collect();

                // Column names for the operator
                let input_column_names: Vec<String> = input_schema
                    .columns
                    .iter()
                    .map(|col| col.name.clone())
                    .collect();

                // Create the aggregate operator with DISTINCT mode
                let operator_id = self.circuit.next_id;
                let executable: Box<dyn IncrementalOperator> = Box::new(AggregateOperator::new(
                    operator_id,
                    group_by,
                    vec![], // Empty aggregates indicates plain DISTINCT
                    input_column_names,
                    vec![], // No FILTERs on plain DISTINCT
                )?);

                // Add the node to the circuit
                let node_id = self.circuit.add_node(
                    DbspOperator::Distinct {
                        schema: input_schema.clone(),
                    },
                    vec![input_id],
                    executable,
                );

                Ok(node_id)
            }
            LogicalPlan::RecursiveCTE(recursive) => self.compile_recursive_cte(recursive),
            LogicalPlan::RecursiveCTERef(cte_ref) => {
                // Look up the delay input node for this recursive CTE
                match self.recursive_cte_refs.get(&cte_ref.name) {
                    Some(&delay_node_id) => Ok(delay_node_id),
                    None => Err(LimboError::ParseError(format!(
                        "Recursive CTE reference '{}' not found in scope",
                        cte_ref.name
                    ))),
                }
            }
            LogicalPlan::EmptyRelation(empty) => {
                // EmptyRelation produces either a single empty row or no rows
                let executable: Box<dyn IncrementalOperator> = if empty.produce_one_row {
                    Box::new(LiteralOperator::single_empty_row())
                } else {
                    Box::new(LiteralOperator::new(vec![]))
                };

                let node_id = self.circuit.add_node(
                    DbspOperator::Literal {
                        schema: empty.schema.clone(),
                    },
                    vec![],
                    executable,
                );
                Ok(node_id)
            }
            LogicalPlan::Sort(sort) => {
                // ORDER BY for materialized views is handled by the storage
                // layer (the matview's output btree is index-organized when
                // the SELECT has ORDER BY). The DBSP circuit itself works on
                // unordered Z-sets, so we just pass through to the input.
                self.compile_plan(&sort.input)
            }
            LogicalPlan::Limit(limit) => {
                // LIMIT for materialized views is enforced at the cursor
                // level (the matview stores every row but the cursor stops
                // walking after N). DBSP doesn't model LIMIT directly.
                self.compile_plan(&limit.input)
            }
            LogicalPlan::Values(values) => {
                // Evaluate literal expressions to get concrete values
                let rows: Result<Vec<Vec<Value>>> = values
                    .rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(|expr| match expr {
                                LogicalExpr::Literal(v) => Ok(v.clone()),
                                _ => Err(LimboError::ParseError(
                                    "VALUES expressions must be literals".to_string(),
                                )),
                            })
                            .collect()
                    })
                    .collect();

                let executable: Box<dyn IncrementalOperator> =
                    Box::new(LiteralOperator::new(rows?));

                let node_id = self.circuit.add_node(
                    DbspOperator::Literal {
                        schema: values.schema.clone(),
                    },
                    vec![],
                    executable,
                );
                Ok(node_id)
            }
            _ => Err(LimboError::ParseError(format!(
                "Unsupported operator in DBSP compiler: only Filter, Projection, Join, Aggregate, Union, EmptyRelation, and Values are supported, got: {:?}",
                match plan {
                    LogicalPlan::Sort(_) => "Sort",
                    LogicalPlan::Limit(_) => "Limit",
                    LogicalPlan::WithCTE(_) => "WithCTE",
                    LogicalPlan::CTERef(_) => "CTERef",
                    _ => "Unknown",
                }
            ))),
        }
    }

    /// Compile a recursive CTE to a DBSP circuit with feedback loop
    ///
    /// The circuit structure for recursion:
    /// 1. Base case: compiled normally, produces initial rows
    /// 2. Delay input: provides z^-1 feedback via a reserved `__delay_<name>` input
    /// 3. Recursive step: references delay via RecursiveCTERef, joins with source tables
    /// 4. Recursive container: orchestrates fixed-point iteration
    ///
    /// The Recursive node has exactly 3 inputs:
    /// - inputs[0]: base_case_id - compiled base case subgraph
    /// - inputs[1]: recursive_step_id - compiled recursive step subgraph
    /// - inputs[2]: delay_id - delay input node for feedback loop
    ///
    /// During execution, the executor runs base case once, then iterates the
    /// recursive step (feeding previous output via delay input) until fixed-point.
    fn compile_recursive_cte(&mut self, recursive: &RecursiveCTE) -> Result<i64> {
        let schema = recursive.schema.clone();
        let max_iterations = recursive
            .max_iterations
            .unwrap_or(DEFAULT_RECURSIVE_MAX_ITERATIONS);
        let recursive_name = recursive.name.clone();

        // 1. Create a delay input (provides z^-1 feedback).
        // This must be created first so RecursiveCTERef can resolve to it.
        let delay_input_name = format!("__delay_{}", recursive.name);
        let delay_executable: Box<dyn IncrementalOperator> =
            Box::new(InputOperator::new(delay_input_name.clone()));
        let delay_id = self.circuit.add_node(
            DbspOperator::Input {
                name: delay_input_name,
                schema: schema.clone(),
            },
            vec![],
            delay_executable,
        );

        // Register the delay input node for RecursiveCTERef resolution
        self.recursive_cte_refs
            .insert(recursive_name.clone(), delay_id);

        let result = (|| -> Result<i64> {
            // 2. Compile the base case
            let base_case_id = self.compile_plan(&recursive.base_case)?;

            // 3. Compile the recursive step (will resolve RecursiveCTERef to delay_id)
            let recursive_step_id = self.compile_plan(&recursive.recursive_step)?;

            // 4. Create the recursive container operator
            let recursive_executable: Box<dyn IncrementalOperator> =
                Box::new(RecursiveOperator::new(
                    self.circuit.next_id,
                    recursive_name.clone(),
                    max_iterations,
                    recursive.union_all,
                ));

            let recursive_id = self.circuit.add_node(
                DbspOperator::Recursive {
                    name: recursive.name.clone(),
                    max_iterations,
                    union_all: recursive.union_all,
                    schema: schema.clone(),
                },
                vec![base_case_id, recursive_step_id, delay_id],
                recursive_executable,
            );

            Ok(recursive_id)
        })();

        // 5. Clean up the recursive CTE reference even if compilation failed
        self.recursive_cte_refs.remove(&recursive_name);

        result
    }

    /// Extract a representative table name from a logical plan (for UNION ALL identification)
    /// Returns a string that uniquely identifies the source of the data
    fn extract_source_identifier(plan: &LogicalPlan) -> String {
        match plan {
            LogicalPlan::TableScan(scan) => {
                // Direct table scan - use the table name
                scan.table_name.clone()
            }
            LogicalPlan::Projection(proj) => {
                // Pass through to input
                Self::extract_source_identifier(&proj.input)
            }
            LogicalPlan::Filter(filter) => {
                // Pass through to input
                Self::extract_source_identifier(&filter.input)
            }
            LogicalPlan::Aggregate(agg) => {
                // Aggregate of a table
                format!("agg_{}", Self::extract_source_identifier(&agg.input))
            }
            LogicalPlan::Sort(sort) => {
                // Pass through to input
                Self::extract_source_identifier(&sort.input)
            }
            LogicalPlan::Limit(limit) => {
                // Pass through to input
                Self::extract_source_identifier(&limit.input)
            }
            LogicalPlan::Join(join) => {
                // Join of two sources - combine their identifiers
                let left_id = Self::extract_source_identifier(&join.left);
                let right_id = Self::extract_source_identifier(&join.right);
                format!("join_{left_id}_{right_id}")
            }
            LogicalPlan::Union(union) => {
                // Union of multiple sources
                if union.inputs.is_empty() {
                    "union_empty".to_string()
                } else {
                    let identifiers: Vec<String> = union
                        .inputs
                        .iter()
                        .map(|input| Self::extract_source_identifier(input))
                        .collect();
                    format!("union_{}", identifiers.join("_"))
                }
            }
            LogicalPlan::Distinct(distinct) => {
                // Distinct of a source
                format!(
                    "distinct_{}",
                    Self::extract_source_identifier(&distinct.input)
                )
            }
            LogicalPlan::WithCTE(with_cte) => {
                // CTE body
                Self::extract_source_identifier(&with_cte.body)
            }
            LogicalPlan::CTERef(cte_ref) => {
                // CTE reference - use the CTE name
                format!("cte_{}", cte_ref.name)
            }
            LogicalPlan::RecursiveCTE(recursive) => {
                // Recursive CTE - use the CTE name
                format!("recursive_{}", recursive.name)
            }
            LogicalPlan::RecursiveCTERef(cte_ref) => {
                // Recursive CTE reference - use the CTE name
                format!("recursive_ref_{}", cte_ref.name)
            }
            LogicalPlan::EmptyRelation(_) => "empty".to_string(),
            LogicalPlan::Values(_) => "values".to_string(),
        }
    }

    /// Compile a UNION operator
    fn compile_union(&mut self, union: &crate::translate::logical::Union) -> Result<i64> {
        if union.inputs.len() != 2 {
            return Err(LimboError::ParseError(format!(
                "UNION requires exactly 2 inputs, got {}",
                union.inputs.len()
            )));
        }

        // Extract source identifiers from each input (for UNION ALL)
        let left_source = Self::extract_source_identifier(&union.inputs[0]);
        let right_source = Self::extract_source_identifier(&union.inputs[1]);

        // Compile left and right inputs
        let left_id = self.compile_plan(&union.inputs[0])?;
        let right_id = self.compile_plan(&union.inputs[1])?;

        use crate::incremental::merge_operator::{MergeOperator, UnionMode};

        // Create a merge operator that handles the rowid transformation
        let operator_id = self.circuit.next_id;
        let mode = if union.all {
            // For UNION ALL, pass the source identifiers
            UnionMode::All {
                left_table: left_source,
                right_table: right_source,
            }
        } else {
            UnionMode::Distinct
        };
        let merge_operator = Box::new(MergeOperator::new(operator_id, mode));

        let merge_id = self.circuit.add_node(
            DbspOperator::Merge {
                schema: union.schema.clone(),
            },
            vec![left_id, right_id],
            merge_operator,
        );

        Ok(merge_id)
    }

    /// Convert a logical expression to a DBSP expression
    fn compile_expr(expr: &LogicalExpr) -> Result<DbspExpr> {
        match expr {
            LogicalExpr::Column(col) => Ok(DbspExpr::Column(col.name.clone())),

            LogicalExpr::Literal(val) => Ok(DbspExpr::Literal(val.clone())),

            LogicalExpr::BinaryExpr { left, op, right } => {
                let left_expr = Self::compile_expr(left)?;
                let right_expr = Self::compile_expr(right)?;

                Ok(DbspExpr::BinaryExpr {
                    left: Box::new(left_expr),
                    op: *op,
                    right: Box::new(right_expr),
                })
            }

            LogicalExpr::Alias { expr, .. } => {
                // For aliases, compile the underlying expression
                Self::compile_expr(expr)
            }

            // For complex expressions (functions, etc), we can't represent them as DbspExpr
            // but that's OK - they'll be handled by the ProjectOperator's VDBE compilation
            // For now, just use a placeholder
            _ => {
                // Use a literal null as placeholder - the actual execution will use the compiled VDBE
                Ok(DbspExpr::Literal(Value::Null))
            }
        }
    }

    /// Compile a logical expression to a CompiledExpression and optional alias
    fn compile_expression(
        expr: &LogicalExpr,
        input_schema: &LogicalSchema,
    ) -> Result<(CompiledExpression, Option<String>)> {
        // Check for alias first
        if let LogicalExpr::Alias { expr, alias } = expr {
            // For aliases, compile the underlying expression and return with alias
            let (compiled, _) = Self::compile_expression(expr, input_schema)?;
            return Ok((compiled, Some(alias.clone())));
        }

        // Convert LogicalExpr to AST Expr with proper column resolution
        let ast_expr = Self::logical_to_ast_expr_with_schema(expr, input_schema)?;

        // Extract column names from schema for CompiledExpression::compile
        let input_column_names: Vec<String> = input_schema
            .columns
            .iter()
            .map(|col| col.name.clone())
            .collect();

        // For all expressions (simple or complex), use CompiledExpression::compile
        // This handles both trivial cases and complex VDBE compilation
        // We need to set up the necessary context
        use crate::sync::Arc;
        use crate::{Database, MemoryIO, SymbolTable};

        // Create an internal connection for expression compilation
        let io = Arc::new(MemoryIO::new());
        let db = Database::open_file(io, ":memory:", Arc::new(SqliteDialect))?;
        let internal_conn = db.connect()?;
        internal_conn.set_query_only(true);
        internal_conn.auto_commit.store(false, Ordering::SeqCst);

        // Create temporary symbol table
        let temp_syms = SymbolTable::new();

        // Get a minimal schema for compilation (we don't need the full schema for expressions)
        let schema = crate::schema::Schema::new();

        // Compile the expression using the existing CompiledExpression::compile
        let compiled = CompiledExpression::compile(
            &ast_expr,
            &input_column_names,
            &schema,
            &temp_syms,
            internal_conn,
        )?;

        Ok((compiled, None))
    }

    /// Convert LogicalExpr to AST Expr with qualified column resolution
    fn logical_to_ast_expr_with_schema(
        expr: &LogicalExpr,
        schema: &LogicalSchema,
    ) -> Result<turso_parser::ast::Expr> {
        use turso_parser::ast;

        match expr {
            LogicalExpr::Column(col) => {
                // Find the column index using qualified lookup
                let (idx, _) = schema
                    .find_column(&col.name, col.table.as_deref())
                    .ok_or_else(|| {
                        LimboError::ParseError(format!(
                            "Column '{}' with table {:?} not found in schema",
                            col.name, col.table
                        ))
                    })?;
                // Return a Register expression with the correct index
                Ok(ast::Expr::Register(idx))
            }
            LogicalExpr::Literal(val) => {
                let lit = match val {
                    Value::Numeric(Numeric::Integer(i)) => ast::Literal::Numeric(i.to_string()),
                    Value::Numeric(Numeric::Float(f)) => {
                        // Rust renders whole-valued floats without a decimal
                        // point ("3.0" -> "3"); the downstream expression
                        // compiler re-parses that as an INTEGER (parse::<i64>()
                        // succeeds first), dropping REAL affinity. Preserve
                        // float-ness precisely when the rendered form would
                        // otherwise round-trip to an integer.
                        let s = f64::from(*f).to_string();
                        let s = if s.parse::<i64>().is_ok() {
                            format!("{s}.0")
                        } else {
                            s
                        };
                        ast::Literal::Numeric(s)
                    }
                    Value::Text(t) => {
                        // Add quotes for string literals as translate_expr expects them
                        // Also escape any single quotes in the string
                        let escaped = t.to_string().replace('\'', "''");
                        ast::Literal::String(format!("'{escaped}'"))
                    }
                    Value::Blob(b) => ast::Literal::Blob(format!("X'{}'", hex::encode(b))),
                    Value::Null => ast::Literal::Null,
                };
                Ok(ast::Expr::Literal(lit))
            }
            LogicalExpr::BinaryExpr { left, op, right } => {
                let left_expr = Self::logical_to_ast_expr_with_schema(left, schema)?;
                let right_expr = Self::logical_to_ast_expr_with_schema(right, schema)?;
                Ok(ast::Expr::Binary(
                    Box::new(left_expr),
                    *op,
                    Box::new(right_expr),
                ))
            }
            LogicalExpr::ScalarFunction { fun, args } => {
                let ast_args: Result<Vec<_>> = args
                    .iter()
                    .map(|arg| Self::logical_to_ast_expr_with_schema(arg, schema))
                    .collect();
                let ast_args: Vec<Box<ast::Expr>> = ast_args?.into_iter().map(Box::new).collect();
                Ok(ast::Expr::FunctionCall {
                    name: ast::Name::exact(fun.clone()),
                    distinctness: None,
                    args: ast_args,
                    order_by: Vec::new(),
                    within_group: vec![],
                    filter_over: ast::FunctionTail {
                        filter_clause: None,
                        over_clause: None,
                    },
                })
            }
            LogicalExpr::Alias { expr, .. } => {
                // For conversion to AST, ignore the alias and convert the inner expression
                Self::logical_to_ast_expr_with_schema(expr, schema)
            }
            LogicalExpr::AggregateFunction {
                fun,
                args,
                distinct,
                // FILTER is consumed by the IVM aggregate operator at line
                // ~2257; this AST round-trip is only used by code paths that
                // operate on the aggregate arg expressions (e.g. projection
                // pre-compilation), not on the filter.
                filter: _,
            } => {
                // Convert aggregate function to AST
                let ast_args: Result<Vec<_>> = args
                    .iter()
                    .map(|arg| Self::logical_to_ast_expr_with_schema(arg, schema))
                    .collect();
                let ast_args: Vec<Box<ast::Expr>> = ast_args?.into_iter().map(Box::new).collect();

                // Get the function name based on the aggregate type
                let func_name = match fun {
                    crate::function::AggFunc::Count => "COUNT",
                    crate::function::AggFunc::Sum => "SUM",
                    crate::function::AggFunc::Avg => "AVG",
                    crate::function::AggFunc::Min => "MIN",
                    crate::function::AggFunc::Max => "MAX",
                    _ => {
                        return Err(LimboError::ParseError(format!(
                            "Unsupported aggregate function: {fun:?}"
                        )));
                    }
                };

                Ok(ast::Expr::FunctionCall {
                    name: ast::Name::exact(func_name.to_string()),
                    distinctness: if *distinct {
                        Some(ast::Distinctness::Distinct)
                    } else {
                        None
                    },
                    args: ast_args,
                    order_by: Vec::new(),
                    within_group: vec![],
                    filter_over: ast::FunctionTail {
                        filter_clause: None,
                        over_clause: None,
                    },
                })
            }
            LogicalExpr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                // BETWEEN x AND y is rewritten as (expr >= x AND expr <= y)
                // NOT BETWEEN x AND y is rewritten as (expr < x OR expr > y)
                let expr_ast = Self::logical_to_ast_expr_with_schema(expr, schema)?;
                let low_ast = Self::logical_to_ast_expr_with_schema(low, schema)?;
                let high_ast = Self::logical_to_ast_expr_with_schema(high, schema)?;

                if *negated {
                    // NOT BETWEEN: (expr < low OR expr > high)
                    Ok(ast::Expr::Binary(
                        Box::new(ast::Expr::Binary(
                            Box::new(expr_ast.clone()),
                            ast::Operator::Less,
                            Box::new(low_ast),
                        )),
                        ast::Operator::Or,
                        Box::new(ast::Expr::Binary(
                            Box::new(expr_ast),
                            ast::Operator::Greater,
                            Box::new(high_ast),
                        )),
                    ))
                } else {
                    // BETWEEN: (expr >= low AND expr <= high)
                    Ok(ast::Expr::Binary(
                        Box::new(ast::Expr::Binary(
                            Box::new(expr_ast.clone()),
                            ast::Operator::GreaterEquals,
                            Box::new(low_ast),
                        )),
                        ast::Operator::And,
                        Box::new(ast::Expr::Binary(
                            Box::new(expr_ast),
                            ast::Operator::LessEquals,
                            Box::new(high_ast),
                        )),
                    ))
                }
            }
            LogicalExpr::InList {
                expr,
                list,
                negated,
            } => {
                let lhs = Box::new(Self::logical_to_ast_expr_with_schema(expr, schema)?);
                let values: Result<Vec<_>> = list
                    .iter()
                    .map(|item| {
                        let ast_expr = Self::logical_to_ast_expr_with_schema(item, schema)?;
                        Ok(Box::new(ast_expr))
                    })
                    .collect();
                Ok(ast::Expr::InList {
                    lhs,
                    not: *negated,
                    rhs: values?,
                })
            }
            LogicalExpr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => {
                let lhs = Box::new(Self::logical_to_ast_expr_with_schema(expr, schema)?);
                let rhs = Box::new(Self::logical_to_ast_expr_with_schema(pattern, schema)?);
                let escape_expr = escape
                    .map(|c| Box::new(ast::Expr::Literal(ast::Literal::String(c.to_string()))));
                Ok(ast::Expr::Like {
                    lhs,
                    not: *negated,
                    op: ast::LikeOperator::Like,
                    rhs,
                    escape: escape_expr,
                })
            }
            LogicalExpr::IsNull { expr, negated } => {
                let inner_expr = Box::new(Self::logical_to_ast_expr_with_schema(expr, schema)?);
                if *negated {
                    // IS NOT NULL needs to be represented differently
                    Ok(ast::Expr::Unary(
                        ast::UnaryOperator::Not,
                        Box::new(ast::Expr::IsNull(inner_expr)),
                    ))
                } else {
                    Ok(ast::Expr::IsNull(inner_expr))
                }
            }
            LogicalExpr::Cast { expr, type_name } => {
                let inner_expr = Box::new(Self::logical_to_ast_expr_with_schema(expr, schema)?);
                Ok(ast::Expr::Cast {
                    expr: inner_expr,
                    type_name: type_name.clone(),
                })
            }
            LogicalExpr::ScalarSubquery(_) => Err(LimboError::ParseError(
                "Correlated scalar subqueries in materialized view SELECT lists \
                 are not yet supported by the IVM compiler. Rewrite as a LEFT \
                 OUTER JOIN with GROUP BY for the same hydrated-row semantics, \
                 e.g. `(SELECT json_group_array(t.tag) FROM tags t WHERE t.fk = b.id)` \
                 → `LEFT OUTER JOIN tags t ON t.fk = b.id` + \
                 `json_group_array(t.tag) FILTER (WHERE t.tag IS NOT NULL)` + \
                 `GROUP BY b.id`."
                    .to_string(),
            )),
            _ => Err(LimboError::ParseError(format!(
                "Cannot convert LogicalExpr to AST Expr: {expr:?}"
            ))),
        }
    }

    /// Check if a predicate contains expressions that need projection
    fn predicate_needs_projection(expr: &LogicalExpr) -> bool {
        match expr {
            LogicalExpr::BinaryExpr { left, op, right } => {
                // Only these specific simple patterns DON'T need projection
                match (left.as_ref(), right.as_ref()) {
                    // Simple column to literal comparisons
                    (LogicalExpr::Column(_), LogicalExpr::Literal(_))
                        if matches!(
                            op,
                            BinaryOperator::Equals
                                | BinaryOperator::NotEquals
                                | BinaryOperator::Greater
                                | BinaryOperator::GreaterEquals
                                | BinaryOperator::Less
                                | BinaryOperator::LessEquals
                        ) =>
                    {
                        false
                    }

                    // Simple column to column comparisons
                    (LogicalExpr::Column(_), LogicalExpr::Column(_))
                        if matches!(
                            op,
                            BinaryOperator::Equals
                                | BinaryOperator::NotEquals
                                | BinaryOperator::Greater
                                | BinaryOperator::GreaterEquals
                                | BinaryOperator::Less
                                | BinaryOperator::LessEquals
                        ) =>
                    {
                        false
                    }

                    // AND/OR of simple expressions - check recursively
                    _ if matches!(op, BinaryOperator::And | BinaryOperator::Or) => {
                        Self::predicate_needs_projection(left)
                            || Self::predicate_needs_projection(right)
                    }

                    // Everything else needs projection
                    _ => true,
                }
            }
            // These simple cases don't need projection
            LogicalExpr::Column(_) | LogicalExpr::Literal(_) => false,

            // `<col> IS [NOT] NULL` is handled natively by `compile_filter_predicate`
            // as `FilterPredicate::IsNull`/`IsNotNull`. Routing it through the
            // projection-rewrite path is wrong: that path only carries a single
            // complex sub-expression as a temp column and then rewrites *both*
            // sides of an AND/OR to reference that one temp column — silently
            // dropping every other null-check predicate in a compound WHERE.
            LogicalExpr::IsNull { expr, .. } if matches!(expr.as_ref(), LogicalExpr::Column(_)) => {
                false
            }

            // Default: assume we need projection for safety
            // This includes: Between, InList, Like, IsNull, Cast, ScalarFunction, Case,
            // InSubquery, Exists, ScalarSubquery, and any future expression types
            _ => true,
        }
    }

    /// Extract the expression part from a predicate that needs to be computed
    fn extract_expression_from_predicate(expr: &LogicalExpr) -> Result<LogicalExpr> {
        match expr {
            LogicalExpr::BinaryExpr { left, op, right } => {
                // Handle AND/OR - recursively find the complex expression
                if matches!(op, BinaryOperator::And | BinaryOperator::Or) {
                    // Check left side first
                    if Self::predicate_needs_projection(left) {
                        return Self::extract_expression_from_predicate(left);
                    }
                    // Then check right side
                    if Self::predicate_needs_projection(right) {
                        return Self::extract_expression_from_predicate(right);
                    }
                    // Neither side needs projection (shouldn't happen if predicate_needs_projection was true)
                    return Ok(expr.clone());
                }

                // For comparison expressions, check if we need to extract a subexpression
                if matches!(
                    op,
                    BinaryOperator::Greater
                        | BinaryOperator::GreaterEquals
                        | BinaryOperator::Less
                        | BinaryOperator::LessEquals
                        | BinaryOperator::Equals
                        | BinaryOperator::NotEquals
                ) {
                    // If the left side is complex (not a column), extract it
                    if !matches!(
                        left.as_ref(),
                        LogicalExpr::Column(_) | LogicalExpr::Literal(_)
                    ) {
                        return Ok((**left).clone());
                    }
                    // If the right side is complex (not a literal), extract it
                    if !matches!(
                        right.as_ref(),
                        LogicalExpr::Column(_) | LogicalExpr::Literal(_)
                    ) {
                        return Ok((**right).clone());
                    }
                    // Both sides are simple but the expression as a whole might need projection
                    // (e.g., for arithmetic operations)
                    Ok(expr.clone())
                } else {
                    // For other binary operators (arithmetic, etc.), return the whole expression
                    Ok(expr.clone())
                }
            }
            // For non-binary expressions (BETWEEN, IN, LIKE, functions, etc.),
            // we need to compute the whole expression as a boolean
            _ => Ok(expr.clone()),
        }
    }

    /// Replace complex expressions in the predicate with references to the temp column
    fn replace_complex_with_temp(
        expr: &LogicalExpr,
        temp_column_name: &str,
    ) -> Result<LogicalExpr> {
        match expr {
            LogicalExpr::BinaryExpr { left, op, right } => {
                // Handle AND/OR - recursively process both sides
                if matches!(op, BinaryOperator::And | BinaryOperator::Or) {
                    let new_left = Self::replace_complex_with_temp(left, temp_column_name)?;
                    let new_right = Self::replace_complex_with_temp(right, temp_column_name)?;
                    return Ok(LogicalExpr::BinaryExpr {
                        left: Box::new(new_left),
                        op: *op,
                        right: Box::new(new_right),
                    });
                }

                // Check if this is a complex comparison that needs replacement
                if Self::predicate_needs_projection(expr) {
                    // Determine which side is complex and needs replacement
                    let left_is_simple = matches!(
                        left.as_ref(),
                        LogicalExpr::Column(_) | LogicalExpr::Literal(_)
                    );
                    let right_is_simple = matches!(
                        right.as_ref(),
                        LogicalExpr::Column(_) | LogicalExpr::Literal(_)
                    );

                    if !left_is_simple {
                        // Left side is complex - replace it with temp column
                        return Ok(LogicalExpr::BinaryExpr {
                            left: Box::new(LogicalExpr::Column(Column {
                                name: temp_column_name.to_string(),
                                table: None,
                            })),
                            op: *op,
                            right: right.clone(),
                        });
                    } else if !right_is_simple {
                        // Right side is complex - replace it with temp column
                        return Ok(LogicalExpr::BinaryExpr {
                            left: left.clone(),
                            op: *op,
                            right: Box::new(LogicalExpr::Column(Column {
                                name: temp_column_name.to_string(),
                                table: None,
                            })),
                        });
                    } else {
                        // Both sides are simple, but the expression as a whole needs projection
                        // This shouldn't happen normally, but keep the expression as-is
                        return Ok(expr.clone());
                    }
                }

                // Simple comparison - keep as is
                Ok(expr.clone())
            }
            // For non-binary expressions that need projection (BETWEEN, IN, etc.),
            // replace the whole expression with a column reference to the temp column
            // The temp column will hold the boolean result of evaluating the expression
            _ if Self::predicate_needs_projection(expr) => {
                // The complex expression result is in the temp column
                // We need to check if it's true (non-zero)
                Ok(LogicalExpr::BinaryExpr {
                    left: Box::new(LogicalExpr::Column(Column {
                        name: temp_column_name.to_string(),
                        table: None,
                    })),
                    op: BinaryOperator::Equals,
                    right: Box::new(LogicalExpr::Literal(Value::from_i64(1))), // true = 1 in SQL
                })
            }
            _ => Ok(expr.clone()),
        }
    }

    /// Compile a logical expression to a FilterPredicate for execution
    fn compile_filter_predicate(
        expr: &LogicalExpr,
        schema: &LogicalSchema,
    ) -> Result<FilterPredicate> {
        match expr {
            LogicalExpr::BinaryExpr { left, op, right } => {
                // Extract column name and value for simple predicates
                // First check for column-to-column comparisons
                if let (LogicalExpr::Column(left_col), LogicalExpr::Column(right_col)) =
                    (left.as_ref(), right.as_ref())
                {
                    // Resolve both column names to indices
                    let left_idx = Self::resolve_column_index(left_col, schema)?;
                    let right_idx = Self::resolve_column_index(right_col, schema)?;

                    match op {
                        BinaryOperator::Equals => Ok(FilterPredicate::ColumnEquals {
                            left_idx,
                            right_idx,
                        }),
                        BinaryOperator::NotEquals => Ok(FilterPredicate::ColumnNotEquals {
                            left_idx,
                            right_idx,
                        }),
                        BinaryOperator::Greater => Ok(FilterPredicate::ColumnGreaterThan {
                            left_idx,
                            right_idx,
                        }),
                        BinaryOperator::GreaterEquals => {
                            Ok(FilterPredicate::ColumnGreaterThanOrEqual {
                                left_idx,
                                right_idx,
                            })
                        }
                        BinaryOperator::Less => Ok(FilterPredicate::ColumnLessThan {
                            left_idx,
                            right_idx,
                        }),
                        BinaryOperator::LessEquals => Ok(FilterPredicate::ColumnLessThanOrEqual {
                            left_idx,
                            right_idx,
                        }),
                        BinaryOperator::And | BinaryOperator::Or => {
                            // Handle logical operators recursively
                            let left_pred = Self::compile_filter_predicate(left, schema)?;
                            let right_pred = Self::compile_filter_predicate(right, schema)?;
                            match op {
                                BinaryOperator::And => Ok(FilterPredicate::And(
                                    Box::new(left_pred),
                                    Box::new(right_pred),
                                )),
                                BinaryOperator::Or => Ok(FilterPredicate::Or(
                                    Box::new(left_pred),
                                    Box::new(right_pred),
                                )),
                                _ => unreachable!(),
                            }
                        }
                        _ => Err(LimboError::ParseError(format!(
                            "Unsupported operator in filter: {op:?}"
                        ))),
                    }
                } else if let (LogicalExpr::Column(col), LogicalExpr::Literal(val)) =
                    (left.as_ref(), right.as_ref())
                {
                    // Column-to-literal comparisons
                    let column_idx = Self::resolve_column_index(col, schema)?;

                    match op {
                        BinaryOperator::Equals => Ok(FilterPredicate::Equals {
                            column_idx,
                            value: val.clone(),
                        }),
                        BinaryOperator::NotEquals => Ok(FilterPredicate::NotEquals {
                            column_idx,
                            value: val.clone(),
                        }),
                        BinaryOperator::Greater => Ok(FilterPredicate::GreaterThan {
                            column_idx,
                            value: val.clone(),
                        }),
                        BinaryOperator::GreaterEquals => Ok(FilterPredicate::GreaterThanOrEqual {
                            column_idx,
                            value: val.clone(),
                        }),
                        BinaryOperator::Less => Ok(FilterPredicate::LessThan {
                            column_idx,
                            value: val.clone(),
                        }),
                        BinaryOperator::LessEquals => Ok(FilterPredicate::LessThanOrEqual {
                            column_idx,
                            value: val.clone(),
                        }),
                        BinaryOperator::And => {
                            // Handle AND of two predicates
                            let left_pred = Self::compile_filter_predicate(left, schema)?;
                            let right_pred = Self::compile_filter_predicate(right, schema)?;
                            Ok(FilterPredicate::And(
                                Box::new(left_pred),
                                Box::new(right_pred),
                            ))
                        }
                        BinaryOperator::Or => {
                            // Handle OR of two predicates
                            let left_pred = Self::compile_filter_predicate(left, schema)?;
                            let right_pred = Self::compile_filter_predicate(right, schema)?;
                            Ok(FilterPredicate::Or(
                                Box::new(left_pred),
                                Box::new(right_pred),
                            ))
                        }
                        _ => Err(LimboError::ParseError(format!(
                            "Unsupported operator in filter: {op:?}"
                        ))),
                    }
                } else if matches!(op, BinaryOperator::And | BinaryOperator::Or) {
                    // Handle logical operators
                    let left_pred = Self::compile_filter_predicate(left, schema)?;
                    let right_pred = Self::compile_filter_predicate(right, schema)?;
                    match op {
                        BinaryOperator::And => Ok(FilterPredicate::And(
                            Box::new(left_pred),
                            Box::new(right_pred),
                        )),
                        BinaryOperator::Or => Ok(FilterPredicate::Or(
                            Box::new(left_pred),
                            Box::new(right_pred),
                        )),
                        _ => unreachable!(),
                    }
                } else {
                    Err(LimboError::ParseError(
                        "Filter predicate must be column op value or column op column".to_string(),
                    ))
                }
            }
            LogicalExpr::IsNull { expr, negated } => {
                // Extract column index from the inner expression
                if let LogicalExpr::Column(col) = expr.as_ref() {
                    let column_idx = Self::resolve_column_index(col, schema)?;

                    if *negated {
                        Ok(FilterPredicate::IsNotNull { column_idx })
                    } else {
                        Ok(FilterPredicate::IsNull { column_idx })
                    }
                } else {
                    Err(LimboError::ParseError(
                        "IS NULL/IS NOT NULL expects a column reference".to_string(),
                    ))
                }
            }
            _ => Err(LimboError::ParseError(format!(
                "Unsupported filter expression: {expr:?}"
            ))),
        }
    }

    /// Resolve a column reference to its index in the schema, considering table
    /// qualifiers to disambiguate columns with the same name (e.g. self-joins).
    fn resolve_column_index(col: &Column, schema: &LogicalSchema) -> Result<usize> {
        if let Some(ref table) = col.table {
            // Strip database prefix if present (e.g. "main.customers" -> "customers")
            let table_name = table
                .rsplit_once('.')
                .map(|(_, t)| t)
                .unwrap_or(table.as_str());
            // Qualified: match on both table_alias (or table) and name
            schema
                .columns
                .iter()
                .position(|c| {
                    c.name == col.name
                        && (c.table_alias.as_deref() == Some(table_name)
                            || c.table.as_deref() == Some(table_name))
                })
                .ok_or_else(|| {
                    LimboError::ParseError(format!(
                        "Column '{}.{}' not found in schema for filter",
                        table, col.name
                    ))
                })
        } else {
            schema
                .columns
                .iter()
                .position(|c| c.name == col.name)
                .ok_or_else(|| {
                    LimboError::ParseError(format!(
                        "Column '{}' not found in schema for filter",
                        col.name
                    ))
                })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::incremental::dbsp::Delta;
    use crate::incremental::operator::{FilterOperator, FilterPredicate};
    use crate::schema::{
        BTreeCharacteristics, BTreeTable, ColDef, Column as SchemaColumn, Schema, Type,
    };
    use crate::storage::pager::CreateBTreeFlags;
    use crate::sync::Arc;
    use crate::translate::logical::{ColumnInfo, LogicalPlanBuilder, LogicalSchema};
    use crate::util::IOExt;
    use crate::SqliteDialect;
    use crate::{Database, MemoryIO, Pager, IO};
    use rustc_hash::FxHashSet as HashSet;
    use turso_parser::ast;
    use turso_parser::parser::Parser;

    // Macro to create a test schema with a users table
    macro_rules! test_schema {
        () => {{
            let mut schema = Schema::new();
            let columns = crate::alloc::vec![
                SchemaColumn::new(
                    Some("id".to_string()),
                    "INTEGER".to_string(),
                    None,
                    None,
                    Type::Integer,
                    None,
                    ColDef {
                        primary_key: true,
                        rowid_alias: true,
                        notnull: true,
                        ..Default::default()
                    },
                ),
                SchemaColumn::new_default_text(Some("name".to_string()), "TEXT".to_string(), None),
                SchemaColumn::new_default_integer(
                    Some("age".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
            ];
            let users_table = BTreeTable::new(
                2,
                "users".to_string(),
                crate::alloc::vec![("id".to_string(), turso_parser::ast::SortOrder::Asc)],
                columns,
                BTreeCharacteristics::HAS_ROWID,
                crate::alloc::vec![],
                crate::alloc::vec![],
                crate::alloc::vec![],
                None,
            );
            schema
                .add_btree_table(Arc::new(users_table))
                .expect("Test setup: failed to add users table");

            // Add products table for join tests
            let columns = crate::alloc::vec![
                SchemaColumn::new(
                    Some("product_id".to_string()),
                    "INTEGER".to_string(),
                    None,
                    None,
                    Type::Integer,
                    None,
                    ColDef {
                        primary_key: true,
                        rowid_alias: true,
                        notnull: true,
                        ..Default::default()
                    },
                ),
                SchemaColumn::new_default_text(
                    Some("product_name".to_string()),
                    "TEXT".to_string(),
                    None,
                ),
                SchemaColumn::new_default_integer(
                    Some("price".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
            ];
            let products_table = BTreeTable::new(
                3,
                "products".to_string(),
                crate::alloc::vec![("product_id".to_string(), turso_parser::ast::SortOrder::Asc)],
                columns,
                BTreeCharacteristics::HAS_ROWID,
                crate::alloc::vec![],
                crate::alloc::vec![],
                crate::alloc::vec![],
                None,
            );
            schema
                .add_btree_table(Arc::new(products_table))
                .expect("Test setup: failed to add products table");

            // Add orders table for join tests
            let columns = crate::alloc::vec![
                SchemaColumn::new(
                    Some("order_id".to_string()),
                    "INTEGER".to_string(),
                    None,
                    None,
                    Type::Integer,
                    None,
                    ColDef {
                        primary_key: true,
                        rowid_alias: true,
                        notnull: true,
                        ..Default::default()
                    },
                ),
                SchemaColumn::new_default_integer(
                    Some("user_id".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
                SchemaColumn::new_default_integer(
                    Some("product_id".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
                SchemaColumn::new_default_integer(
                    Some("quantity".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
            ];
            let orders_table = BTreeTable::new(
                4,
                "orders".to_string(),
                crate::alloc::vec![("order_id".to_string(), turso_parser::ast::SortOrder::Asc)],
                columns,
                BTreeCharacteristics::HAS_ROWID,
                crate::alloc::vec![],
                crate::alloc::vec![],
                crate::alloc::vec![],
                None,
            );
            schema
                .add_btree_table(Arc::new(orders_table))
                .expect("Test setup: failed to add orders table");

            // Add customers table with id and name for testing column ambiguity
            let columns = crate::alloc::vec![
                SchemaColumn::new(
                    Some("id".to_string()),
                    "INTEGER".to_string(),
                    None,
                    None,
                    Type::Integer,
                    None,
                    ColDef {
                        primary_key: true,
                        rowid_alias: true,
                        notnull: true,
                        ..Default::default()
                    },
                ),
                SchemaColumn::new_default_text(Some("name".to_string()), "TEXT".to_string(), None),
            ];
            let customers_table = BTreeTable::new(
                6,
                "customers".to_string(),
                crate::alloc::vec![("id".to_string(), turso_parser::ast::SortOrder::Asc)],
                columns,
                BTreeCharacteristics::HAS_ROWID,
                crate::alloc::vec![],
                crate::alloc::vec![],
                crate::alloc::vec![],
                None,
            );
            schema
                .add_btree_table(Arc::new(customers_table))
                .expect("Test setup: failed to add customers table");

            // Add purchases table (junction table for three-way join)
            let columns = crate::alloc::vec![
                SchemaColumn::new(
                    Some("id".to_string()),
                    "INTEGER".to_string(),
                    None,
                    None,
                    Type::Integer,
                    None,
                    ColDef {
                        primary_key: true,
                        rowid_alias: true,
                        notnull: true,
                        ..Default::default()
                    },
                ),
                SchemaColumn::new_default_integer(
                    Some("customer_id".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
                SchemaColumn::new_default_integer(
                    Some("vendor_id".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
                SchemaColumn::new_default_integer(
                    Some("quantity".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
            ];
            let purchases_table = BTreeTable::new(
                7,
                "purchases".to_string(),
                crate::alloc::vec![("id".to_string(), turso_parser::ast::SortOrder::Asc)],
                columns,
                BTreeCharacteristics::HAS_ROWID,
                crate::alloc::vec![],
                crate::alloc::vec![],
                crate::alloc::vec![],
                None,
            );
            schema
                .add_btree_table(Arc::new(purchases_table))
                .expect("Test setup: failed to add purchases table");

            // Add vendors table with id, name, and price (ambiguous columns with customers)
            let columns = crate::alloc::vec![
                SchemaColumn::new(
                    Some("id".to_string()),
                    "INTEGER".to_string(),
                    None,
                    None,
                    Type::Integer,
                    None,
                    ColDef {
                        primary_key: true,
                        rowid_alias: true,
                        notnull: true,
                        ..Default::default()
                    },
                ),
                SchemaColumn::new_default_text(Some("name".to_string()), "TEXT".to_string(), None),
                SchemaColumn::new_default_integer(
                    Some("price".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
            ];
            let vendors_table = BTreeTable::new(
                8,
                "vendors".to_string(),
                crate::alloc::vec![("id".to_string(), turso_parser::ast::SortOrder::Asc)],
                columns,
                BTreeCharacteristics::HAS_ROWID,
                crate::alloc::vec![],
                crate::alloc::vec![],
                crate::alloc::vec![],
                None,
            );
            schema
                .add_btree_table(Arc::new(vendors_table))
                .expect("Test setup: failed to add vendors table");

            let columns = crate::alloc::vec![
                SchemaColumn::new_default_integer(
                    Some("product_id".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
                SchemaColumn::new_default_integer(
                    Some("amount".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
            ];
            let sales_table = BTreeTable::new(
                2,
                "sales".to_string(),
                crate::alloc::vec![],
                columns,
                BTreeCharacteristics::HAS_ROWID,
                crate::alloc::vec![],
                crate::alloc::vec![],
                crate::alloc::vec![],
                None,
            );
            schema
                .add_btree_table(Arc::new(sales_table))
                .expect("Test setup: failed to add sales table");

            // Add edges table for recursive CTE tests (transitive closure)
            let edges_columns = vec![
                SchemaColumn::new_default_integer(
                    Some("src".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
                SchemaColumn::new_default_integer(
                    Some("dst".to_string()),
                    "INTEGER".to_string(),
                    None,
                ),
            ];
            let edges_table = BTreeTable::new(
                9,
                "edges".to_string(),
                vec![],
                edges_columns,
                BTreeCharacteristics::HAS_ROWID,
                vec![],
                vec![],
                vec![],
                None,
            );
            schema
                .add_btree_table(Arc::new(edges_table))
                .expect("Test setup: failed to add edges table");

            schema
        }};
    }

    fn setup_btree_for_circuit() -> (Arc<Pager>, i64, i64, i64) {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = Database::open_file(io.clone(), ":memory:", Arc::new(SqliteDialect)).unwrap();
        let conn = db.connect().unwrap();
        let pager = conn.pager.load().clone();

        let _ = pager.io.block(|| pager.allocate_page1()).unwrap();

        let main_root_page = pager
            .io
            .block(|| pager.btree_create(&CreateBTreeFlags::new_table()))
            .unwrap() as i64;

        let dbsp_state_page = pager
            .io
            .block(|| pager.btree_create(&CreateBTreeFlags::new_table()))
            .unwrap() as i64;

        let dbsp_state_index_page = pager
            .io
            .block(|| pager.btree_create(&CreateBTreeFlags::new_index()))
            .unwrap() as i64;

        (
            pager,
            main_root_page,
            dbsp_state_page,
            dbsp_state_index_page,
        )
    }

    // Macro to compile SQL to DBSP circuit
    macro_rules! compile_sql {
        ($sql:expr) => {{
            let (pager, main_root_page, dbsp_state_page, dbsp_state_index_page) =
                setup_btree_for_circuit();
            let schema = test_schema!();
            let mut parser = Parser::new($sql.as_bytes());
            let cmd = parser
                .next()
                .unwrap() // This returns Option<Result<Cmd, Error>>
                .unwrap(); // This unwraps the Result

            match cmd {
                ast::Cmd::Stmt(stmt) => {
                    let mut builder = LogicalPlanBuilder::new(&schema);
                    let logical_plan = builder.build_statement(&stmt).unwrap();
                    (
                        DbspCompiler::new(
                            main_root_page,
                            dbsp_state_page,
                            dbsp_state_index_page,
                            crate::incremental::view::MatviewOrderBy::default(),
                            None,
                        )
                        .compile(&logical_plan)
                        .unwrap(),
                        pager,
                    )
                }
                _ => panic!("Only SQL statements are supported"),
            }
        }};
    }

    // Macro to assert circuit structure
    macro_rules! assert_circuit {
        ($circuit:expr, depth: $depth:expr, root: $root_type:ident) => {
            assert_eq!($circuit.nodes.len(), $depth);
            let node = get_node_at_level(&$circuit, 0);
            assert!(matches!(node.operator, DbspOperator::$root_type { .. }));
        };
    }

    // Macro to assert operator properties
    macro_rules! assert_operator {
        ($circuit:expr, $level:expr, Input { name: $name:expr }) => {{
            let node = get_node_at_level(&$circuit, $level);
            match &node.operator {
                DbspOperator::Input { name, .. } => assert_eq!(name, $name),
                _ => panic!("Expected Input operator at level {}", $level),
            }
        }};
        ($circuit:expr, $level:expr, Filter) => {{
            let node = get_node_at_level(&$circuit, $level);
            assert!(matches!(node.operator, DbspOperator::Filter { .. }));
        }};
        ($circuit:expr, $level:expr, Projection { columns: [$($col:expr),*] }) => {{
            let node = get_node_at_level(&$circuit, $level);
            match &node.operator {
                DbspOperator::Projection { exprs, .. } => {
                    let expected_cols = vec![$($col),*];
                    let actual_cols: Vec<String> = exprs.iter().map(|e| {
                        match e {
                            DbspExpr::Column(name) => name.clone(),
                            _ => "expr".to_string(),
                        }
                    }).collect();
                    assert_eq!(actual_cols, expected_cols);
                }
                _ => panic!("Expected Projection operator at level {}", $level),
            }
        }};
    }

    // Macro to assert filter predicate
    macro_rules! assert_filter_predicate {
        ($circuit:expr, $level:expr, $col:literal > $val:literal) => {{
            let node = get_node_at_level(&$circuit, $level);
            match &node.operator {
                DbspOperator::Filter { predicate } => match predicate {
                    DbspExpr::BinaryExpr { left, op, right } => {
                        assert!(matches!(op, ast::Operator::Greater));
                        assert!(matches!(&**left, DbspExpr::Column(name) if name == $col));
                        assert!(matches!(&**right, DbspExpr::Literal(Value::Numeric(Numeric::Integer($val)))));
                    }
                    _ => panic!("Expected binary expression in filter"),
                },
                _ => panic!("Expected Filter operator at level {}", $level),
            }
        }};
        ($circuit:expr, $level:expr, $col:literal < $val:literal) => {{
            let node = get_node_at_level(&$circuit, $level);
            match &node.operator {
                DbspOperator::Filter { predicate } => match predicate {
                    DbspExpr::BinaryExpr { left, op, right } => {
                        assert!(matches!(op, ast::Operator::Less));
                        assert!(matches!(&**left, DbspExpr::Column(name) if name == $col));
                        assert!(matches!(&**right, DbspExpr::Literal(Value::Numeric(Numeric::Integer($val)))));
                    }
                    _ => panic!("Expected binary expression in filter"),
                },
                _ => panic!("Expected Filter operator at level {}", $level),
            }
        }};
        ($circuit:expr, $level:expr, $col:literal = $val:literal) => {{
            let node = get_node_at_level(&$circuit, $level);
            match &node.operator {
                DbspOperator::Filter { predicate } => match predicate {
                    DbspExpr::BinaryExpr { left, op, right } => {
                        assert!(matches!(op, ast::Operator::Equals));
                        assert!(matches!(&**left, DbspExpr::Column(name) if name == $col));
                        assert!(matches!(&**right, DbspExpr::Literal(Value::Numeric(Numeric::Integer($val)))));
                    }
                    _ => panic!("Expected binary expression in filter"),
                },
                _ => panic!("Expected Filter operator at level {}", $level),
            }
        }};
    }

    // Helper to get node at specific level from root
    fn get_node_at_level(circuit: &DbspCircuit, level: usize) -> &DbspNode {
        let mut current_id = circuit.root.expect("Circuit has no root");
        for _ in 0..level {
            let node = circuit.nodes.get(&current_id).expect("Node not found");
            if node.inputs.is_empty() {
                panic!("No more levels available, requested level {level}");
            }
            current_id = node.inputs[0];
        }
        circuit.nodes.get(&current_id).expect("Node not found")
    }

    // Helper function for tests to execute circuit and extract the Delta result
    #[cfg(test)]
    fn test_execute(
        circuit: &mut DbspCircuit,
        inputs: HashMap<String, Delta>,
        pager: Arc<Pager>,
    ) -> Result<Delta> {
        let mut execute_state = ExecuteState::Init {
            input_data: InputDeltas::from_map(inputs),
        };
        match circuit.execute(pager, &mut execute_state)? {
            IOResult::Done(delta) => Ok(delta),
            IOResult::IO(_) => panic!("Unexpected I/O in test"),
        }
    }

    // Helper to get the committed BTree state from main_data_root
    // This reads the actual persisted data from the BTree
    #[cfg(test)]
    fn get_current_state(pager: Arc<Pager>, circuit: &DbspCircuit) -> Result<Delta> {
        use crate::storage::btree::CursorTrait;

        let mut delta = Delta::new();

        let main_data_root = circuit.main_data_root;
        let num_columns = circuit.output_schema.columns.len() + 1;

        // Create a cursor to read the btree
        let mut btree_cursor = BTreeCursor::new_table(pager.clone(), main_data_root, num_columns);

        // Rewind to the beginning
        pager.io.block(|| btree_cursor.rewind())?;

        // Read all rows from the BTree
        loop {
            // Check if cursor is empty (no more rows)
            if btree_cursor.is_empty() {
                break;
            }

            // Get the rowid
            let rowid = pager.io.block(|| btree_cursor.rowid()).unwrap().unwrap();

            // Get the record at this position
            let record = loop {
                match btree_cursor.record().unwrap() {
                    IOResult::Done(r) => break r,
                    IOResult::IO(io) => io.wait(&*pager.io).unwrap(),
                }
            }
            .unwrap()
            .to_owned();

            let num_data_columns = record.column_count() - 1;

            let mut values = Vec::with_capacity(num_data_columns);
            let mut values_iter = record.iter()?;

            for _ in 0..num_data_columns {
                let value = values_iter.next().expect("we already checked bounds")?;
                values.push(value.to_owned()?);
            }

            delta.insert(rowid, values);
            pager.io.block(|| btree_cursor.next()).unwrap();
        }
        Ok(delta)
    }

    #[test]
    fn test_simple_projection() {
        let (circuit, _) = compile_sql!("SELECT name FROM users");

        // Circuit has 2 nodes with Projection at root
        assert_circuit!(circuit, depth: 2, root: Projection);

        // Verify operators at each level
        assert_operator!(circuit, 0, Projection { columns: ["name"] });
        assert_operator!(circuit, 1, Input { name: "users" });
    }

    #[test]
    fn test_filter_with_projection() {
        let (circuit, _) = compile_sql!("SELECT name FROM users WHERE age > 18");

        // Circuit has 3 nodes with Projection at root
        assert_circuit!(circuit, depth: 3, root: Projection);

        // Verify operators at each level
        assert_operator!(circuit, 0, Projection { columns: ["name"] });
        assert_operator!(circuit, 1, Filter);
        assert_filter_predicate!(circuit, 1, "age" > 18);
        assert_operator!(circuit, 2, Input { name: "users" });
    }

    #[test]
    fn test_select_star() {
        let (mut circuit, pager) = compile_sql!("SELECT * FROM users");

        // Create test data
        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        input_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(17),
            ],
        );

        // Create input map
        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, inputs.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(inputs.clone(), pager.clone()))
            .unwrap();

        // Should have all rows with all columns
        assert_eq!(result.changes.len(), 2);

        // Verify both rows are present with all columns
        for (row, weight) in &result.changes {
            assert_eq!(*weight, 1);
            assert_eq!(row.values.len(), 3); // id, name, age
        }
    }

    #[test]
    fn test_execute_filter() {
        let (mut circuit, pager) = compile_sql!("SELECT * FROM users WHERE age > 18");

        // Create test data
        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        input_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(17),
            ],
        );
        input_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(30),
            ],
        );

        // Create input map
        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, inputs.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(inputs.clone(), pager.clone()))
            .unwrap();

        // Should only have Alice and Charlie (age > 18)
        assert_eq!(
            result.changes.len(),
            2,
            "Expected 2 rows after filtering, got {}",
            result.changes.len()
        );

        // Check that the filtered rows are correct
        let names: Vec<String> = result
            .changes
            .iter()
            .filter_map(|(row, weight)| {
                if *weight > 0 && row.values.len() > 1 {
                    if let Value::Text(name) = &row.values[1] {
                        Some(name.to_string())
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect();

        assert!(
            names.contains(&"Alice".to_string()),
            "Alice should be in results"
        );
        assert!(
            names.contains(&"Charlie".to_string()),
            "Charlie should be in results"
        );
        assert!(
            !names.contains(&"Bob".to_string()),
            "Bob should not be in results"
        );
    }

    #[test]
    fn test_simple_column_projection() {
        let (mut circuit, pager) = compile_sql!("SELECT name, age FROM users");

        // Create test data
        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        input_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(17),
            ],
        );

        // Create input map
        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, inputs.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(inputs.clone(), pager.clone()))
            .unwrap();

        // Should have all rows but only 2 columns (name, age)
        assert_eq!(result.changes.len(), 2);

        for (row, _) in &result.changes {
            assert_eq!(row.values.len(), 2); // Only name and age
                                             // First value should be name (Text)
            assert!(matches!(&row.values[0], Value::Text(_)));
            // Second value should be age (Integer)
            assert!(matches!(
                &row.values[1],
                Value::Numeric(Numeric::Integer(_))
            ));
        }
    }

    #[test]
    fn test_simple_aggregation() {
        // Test COUNT(*) with GROUP BY
        let (mut circuit, pager) = compile_sql!("SELECT age, COUNT(*) FROM users GROUP BY age");

        // Create test data
        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        input_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(25),
            ],
        );
        input_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(30),
            ],
        );

        // Create input map
        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, inputs.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(inputs.clone(), pager.clone()))
            .unwrap();

        // Should have 2 groups: age 25 with count 2, age 30 with count 1
        assert_eq!(result.changes.len(), 2);

        // Check the results
        let mut found_25 = false;
        let mut found_30 = false;

        for (row, weight) in &result.changes {
            assert_eq!(*weight, 1);
            assert_eq!(row.values.len(), 2); // age, count

            if let (
                Value::Numeric(Numeric::Integer(age)),
                Value::Numeric(Numeric::Integer(count)),
            ) = (&row.values[0], &row.values[1])
            {
                if *age == 25 {
                    assert_eq!(*count, 2, "Age 25 should have count 2");
                    found_25 = true;
                } else if *age == 30 {
                    assert_eq!(*count, 1, "Age 30 should have count 1");
                    found_30 = true;
                }
            }
        }

        assert!(found_25, "Should have group for age 25");
        assert!(found_30, "Should have group for age 30");
    }

    #[test]
    fn test_sum_aggregation() {
        // Test SUM with GROUP BY
        let (mut circuit, pager) = compile_sql!("SELECT name, SUM(age) FROM users GROUP BY name");

        // Create test data - some names appear multiple times
        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        input_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Alice".into()),
                Value::from_i64(30),
            ],
        );
        input_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Bob".into()),
                Value::from_i64(20),
            ],
        );

        // Create input map
        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, inputs.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(inputs.clone(), pager.clone()))
            .unwrap();

        // Should have 2 groups: Alice with sum 55, Bob with sum 20
        assert_eq!(result.changes.len(), 2);

        for (row, weight) in &result.changes {
            assert_eq!(*weight, 1);
            assert_eq!(row.values.len(), 2); // name, sum

            if let (Value::Text(name), Value::Numeric(Numeric::Float(sum))) =
                (&row.values[0], &row.values[1])
            {
                if name.as_str() == "Alice" {
                    assert_eq!(*sum, 55.0, "Alice should have sum 55");
                } else if name.as_str() == "Bob" {
                    assert_eq!(*sum, 20.0, "Bob should have sum 20");
                }
            }
        }
    }

    #[test]
    fn test_aggregation_without_group_by() {
        // Test aggregation without GROUP BY - should produce a single row
        let (mut circuit, pager) = compile_sql!("SELECT COUNT(*), SUM(age), AVG(age) FROM users");

        // Create test data
        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        input_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );
        input_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(20),
            ],
        );

        // Create input map
        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, inputs.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(inputs.clone(), pager.clone()))
            .unwrap();

        // Should have exactly 1 row with all aggregates
        assert_eq!(
            result.changes.len(),
            1,
            "Should have exactly one result row"
        );

        let (row, weight) = result.changes.first().unwrap();
        assert_eq!(*weight, 1);
        assert_eq!(row.values.len(), 3); // count, sum, avg

        // Check aggregate results
        // COUNT should be Integer
        if let Value::Numeric(Numeric::Integer(count)) = &row.values[0] {
            assert_eq!(*count, 3, "COUNT(*) should be 3");
        } else {
            panic!("COUNT should be Integer, got {:?}", row.values[0]);
        }

        // SUM can be Integer (if whole number) or Float
        match &row.values[1] {
            Value::Numeric(Numeric::Integer(sum)) => assert_eq!(*sum, 75, "SUM(age) should be 75"),
            Value::Numeric(Numeric::Float(sum)) => {
                assert_eq!(f64::from(*sum), 75.0, "SUM(age) should be 75.0")
            }
            other => panic!("SUM should be Integer or Float, got {other:?}"),
        }

        // AVG should be Float
        if let Value::Numeric(Numeric::Float(avg)) = &row.values[2] {
            assert_eq!(f64::from(*avg), 25.0, "AVG(age) should be 25.0");
        } else {
            panic!("AVG should be Float, got {:?}", row.values[2]);
        }
    }

    #[test]
    fn test_expression_projection_execution() {
        // Test that complex expressions work through VDBE compilation
        let (mut circuit, pager) = compile_sql!("SELECT hex(id) FROM users");

        // Create test data
        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        input_delta.insert(
            2,
            vec![
                Value::from_i64(255),
                Value::Text("Bob".into()),
                Value::from_i64(17),
            ],
        );

        // Create input map
        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, inputs.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(inputs.clone(), pager.clone()))
            .unwrap();

        assert_eq!(result.changes.len(), 2);

        let hex_values: HashMap<i64, String> = result
            .changes
            .iter()
            .map(|(row, _)| {
                let rowid = row.rowid;
                if let Value::Text(text) = &row.values[0] {
                    (rowid, text.to_string())
                } else {
                    panic!("Expected Text value for hex() result");
                }
            })
            .collect();

        assert_eq!(
            hex_values.get(&1).unwrap(),
            "31",
            "hex(1) should return '31' (hex of ASCII '1')"
        );

        assert_eq!(
            hex_values.get(&2).unwrap(),
            "323535",
            "hex(255) should return '323535' (hex of ASCII '2', '5', '5')"
        );
    }

    #[test]
    fn test_matview_whole_float_literal_keeps_real_affinity() {
        // Regression: a whole-number float literal (3.0, 2.0, 1.0, ...) in a
        // materialized-view projection lost its REAL affinity and was computed
        // with integer semantics — e.g. (3.0 / 2.0) maintained as INTEGER 1
        // instead of REAL 1.5. Root cause: logical_to_ast_expr_with_schema
        // lowered a Float literal back to AST text via f64::to_string(), which
        // renders 3.0 as "3", so the VDBE re-parsed it as an integer. Fractional
        // literals (0.001) and genuine REAL columns were unaffected — asserted
        // here as contrast so the fix stays narrow.
        let (mut circuit, pager) = compile_sql!(
            "SELECT id, \
                (3.0 / 2.0) AS a, \
                (age / 2.0) AS b, \
                (3.0 * 1.0) AS c, \
                typeof(3.0 / 2.0) AS ta, \
                (0.001 * age) AS frac \
             FROM users"
        );

        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(9),
            ],
        );

        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, inputs.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(inputs.clone(), pager.clone()))
            .unwrap();

        assert_eq!(result.changes.len(), 1);
        let (row, _) = result.changes.iter().next().unwrap();
        let values = &row.values;

        let expect_real = |v: &Value, expected: f64, label: &str| match v {
            Value::Numeric(Numeric::Float(f)) => {
                assert!(
                    (f64::from(*f) - expected).abs() < 1e-9,
                    "{label}: expected REAL {expected}, got {}",
                    f64::from(*f)
                );
            }
            other => panic!("{label}: expected REAL {expected}, got {other:?}"),
        };

        expect_real(&values[1], 1.5, "3.0 / 2.0");
        expect_real(&values[2], 4.5, "age(9) / 2.0");
        expect_real(&values[3], 3.0, "3.0 * 1.0");
        match &values[4] {
            Value::Text(t) => assert_eq!(t.to_string(), "real", "typeof(3.0 / 2.0) must be 'real'"),
            other => panic!("typeof(3.0 / 2.0): expected 'real', got {other:?}"),
        }
        expect_real(&values[5], 0.009, "0.001 * age(9) [fractional contrast]");
    }

    // TODO: This test currently fails on incremental updates.
    // The initial execution works correctly, but incremental updates produce
    // incorrect results (3 changes instead of 2, with wrong values).
    // This tests that the aggregate operator correctly handles incremental
    // updates when it's sandwiched between projection operators.
    #[test]
    fn test_projection_aggregation_projection_pattern() {
        // Test pattern: projection -> aggregation -> projection
        // Query: SELECT HEX(SUM(age + 2)) FROM users
        let (mut circuit, pager) = compile_sql!("SELECT HEX(SUM(age + 2)) FROM users");

        // Initial input data
        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".to_string().into()),
                Value::from_i64(25),
            ],
        );
        input_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".to_string().into()),
                Value::from_i64(30),
            ],
        );
        input_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".to_string().into()),
                Value::from_i64(35),
            ],
        );

        let mut input_data = HashMap::default();
        input_data.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, input_data.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(input_data.clone(), pager.clone()))
            .unwrap();

        // Expected: SUM(age + 2) = (25+2) + (30+2) + (35+2) = 27 + 32 + 37 = 96
        // HEX(96) should be the hex representation of the string "96" = "3936"
        assert_eq!(result.changes.len(), 1);
        let (row, _weight) = &result.changes[0];
        assert_eq!(row.values.len(), 1);

        // The hex function converts the number to string first, then to hex
        // SUM now returns Float, so 96.0 as string is "96.0", which in hex is "39362E30"
        // (hex of ASCII '9', '6', '.', '0')
        assert_eq!(
            row.values[0],
            Value::Text("39362E30".to_string().into()),
            "HEX(SUM(age + 2)) should return '39362E30' for sum of 96.0"
        );

        // Test incremental update: add a new user
        let mut input_delta = Delta::new();
        input_delta.insert(
            4,
            vec![
                Value::from_i64(4),
                Value::Text("David".to_string().into()),
                Value::from_i64(40),
            ],
        );

        let mut input_data = HashMap::default();
        input_data.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, input_data, pager).unwrap();

        // Expected: new SUM(age + 2) = 96.0 + (40+2) = 138.0
        // HEX(138.0) = hex of "138.0" = "3133382E30"
        assert_eq!(result.changes.len(), 2);

        // First change: remove old aggregate (96.0)
        let (row, weight) = &result.changes[0];
        assert_eq!(*weight, -1);
        assert_eq!(row.values[0], Value::Text("39362E30".to_string().into()));

        // Second change: add new aggregate (138.0)
        let (row, weight) = &result.changes[1];
        assert_eq!(*weight, 1);
        assert_eq!(
            row.values[0],
            Value::Text("3133382E30".to_string().into()),
            "HEX(SUM(age + 2)) should return '3133382E30' for sum of 138.0"
        );
    }

    #[test]
    fn test_nested_projection_with_groupby() {
        // Test pattern: projection -> aggregation with GROUP BY -> projection
        // Query: SELECT name, HEX(SUM(age * 2)) FROM users GROUP BY name
        let (mut circuit, pager) =
            compile_sql!("SELECT name, HEX(SUM(age * 2)) FROM users GROUP BY name");

        // Initial input data
        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".to_string().into()),
                Value::from_i64(25),
            ],
        );
        input_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".to_string().into()),
                Value::from_i64(30),
            ],
        );
        input_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Alice".to_string().into()),
                Value::from_i64(35),
            ],
        );

        let mut input_data = HashMap::default();
        input_data.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, input_data.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(input_data.clone(), pager.clone()))
            .unwrap();

        // Expected results:
        // Alice: SUM(25*2 + 35*2) = 50 + 70 = 120.0, HEX("120.0") = "3132302E30"
        // Bob: SUM(30*2) = 60.0, HEX("60.0") = "36302E30"
        assert_eq!(result.changes.len(), 2);

        let results: HashMap<String, String> = result
            .changes
            .iter()
            .map(|(row, _weight)| {
                let name = match &row.values[0] {
                    Value::Text(t) => t.to_string(),
                    _ => panic!("Expected text for name"),
                };
                let hex_sum = match &row.values[1] {
                    Value::Text(t) => t.to_string(),
                    _ => panic!("Expected text for hex value"),
                };
                (name, hex_sum)
            })
            .collect();

        assert_eq!(
            results.get("Alice").unwrap(),
            "3132302E30",
            "Alice's HEX(SUM(age * 2)) should be '3132302E30' (120.0)"
        );
        assert_eq!(
            results.get("Bob").unwrap(),
            "36302E30",
            "Bob's HEX(SUM(age * 2)) should be '36302E30' (60.0)"
        );
    }

    #[test]
    fn test_transaction_context() {
        // Test that uncommitted changes are visible within a transaction
        // but don't affect the operator's internal state
        let (mut circuit, pager) = compile_sql!("SELECT * FROM users WHERE age > 18");

        // Initialize with some data
        let mut init_data = HashMap::default();
        let mut delta = Delta::new();
        delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(17),
            ],
        );
        init_data.insert("users".to_string(), delta);

        let _ = test_execute(&mut circuit, init_data.clone(), pager.clone()).unwrap();
        let state = pager
            .io
            .block(|| circuit.commit(init_data.clone(), pager.clone()))
            .unwrap();

        // Verify initial delta : only Alice (age > 18)
        assert_eq!(state.changes.len(), 1);
        assert_eq!(state.changes[0].0.values[1], Value::Text("Alice".into()));

        // Create uncommitted changes that would be visible in a transaction
        let mut uncommitted = HashMap::default();
        let mut uncommitted_delta = Delta::new();
        // Add Charlie (age 30) - should be visible in transaction
        uncommitted_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(30),
            ],
        );
        // Add David (age 15) - should NOT be visible (filtered out)
        uncommitted_delta.insert(
            4,
            vec![
                Value::from_i64(4),
                Value::Text("David".into()),
                Value::from_i64(15),
            ],
        );
        uncommitted.insert("users".to_string(), uncommitted_delta);

        // Execute with uncommitted data - this simulates processing the uncommitted changes
        // through the circuit to see what would be visible
        let tx_result = test_execute(&mut circuit, uncommitted.clone(), pager.clone()).unwrap();

        // The result should show Charlie being added (passes filter, age > 18)
        // David is filtered out (age 15 < 18)
        assert_eq!(tx_result.changes.len(), 1, "Should see Charlie added");
        assert_eq!(
            tx_result.changes[0].0.values[1],
            Value::Text("Charlie".into())
        );

        // Now actually commit Charlie (without uncommitted context)
        let mut commit_data = HashMap::default();
        let mut commit_delta = Delta::new();
        commit_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(30),
            ],
        );
        commit_data.insert("users".to_string(), commit_delta);

        let commit_result = test_execute(&mut circuit, commit_data.clone(), pager.clone()).unwrap();

        // The commit result should show Charlie being added
        assert_eq!(commit_result.changes.len(), 1, "Should see Charlie added");
        assert_eq!(
            commit_result.changes[0].0.values[1],
            Value::Text("Charlie".into())
        );

        // Commit the change to make it permanent
        pager
            .io
            .block(|| circuit.commit(commit_data.clone(), pager.clone()))
            .unwrap();

        // Now if we execute again with no changes, we should see no delta
        let empty_result = test_execute(&mut circuit, HashMap::default(), pager).unwrap();
        assert_eq!(empty_result.changes.len(), 0, "No changes when no new data");
    }

    #[test]
    fn test_uncommitted_delete() {
        // Test that uncommitted deletes are handled correctly without affecting operator state
        let (mut circuit, pager) = compile_sql!("SELECT * FROM users WHERE age > 18");

        // Initialize with some data
        let mut init_data = HashMap::default();
        let mut delta = Delta::new();
        delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );
        delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(20),
            ],
        );
        init_data.insert("users".to_string(), delta);

        let _ = test_execute(&mut circuit, init_data.clone(), pager.clone()).unwrap();
        let state = pager
            .io
            .block(|| circuit.commit(init_data.clone(), pager.clone()))
            .unwrap();

        // Verify initial delta: Alice, Bob, Charlie (all age > 18)
        assert_eq!(state.changes.len(), 3);

        // Create uncommitted delete for Bob
        let mut uncommitted = HashMap::default();
        let mut uncommitted_delta = Delta::new();
        uncommitted_delta.delete(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );
        uncommitted.insert("users".to_string(), uncommitted_delta);

        // Execute with uncommitted delete
        let tx_result = test_execute(&mut circuit, uncommitted.clone(), pager.clone()).unwrap();

        // Result should show the deleted row that passed the filter
        assert_eq!(
            tx_result.changes.len(),
            1,
            "Should see the uncommitted delete"
        );

        // Verify operator's internal state is unchanged (still has all 3 users)
        let state_after = get_current_state(pager.clone(), &circuit).unwrap();
        assert_eq!(
            state_after.changes.len(),
            3,
            "Internal state should still have all 3 users"
        );

        // Now actually commit the delete
        let mut commit_data = HashMap::default();
        let mut commit_delta = Delta::new();
        commit_delta.delete(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );
        commit_data.insert("users".to_string(), commit_delta);

        let commit_result = test_execute(&mut circuit, commit_data.clone(), pager.clone()).unwrap();

        // Actually commit the delete to update operator state
        pager
            .io
            .block(|| circuit.commit(commit_data.clone(), pager.clone()))
            .unwrap();

        // The commit result should show Bob being deleted
        assert_eq!(commit_result.changes.len(), 1, "Should see Bob deleted");
        assert_eq!(
            commit_result.changes[0].1, -1,
            "Delete should have weight -1"
        );
        assert_eq!(
            commit_result.changes[0].0.values[1],
            Value::Text("Bob".into())
        );

        // After commit, internal state should have only Alice and Charlie
        let final_state = get_current_state(pager, &circuit).unwrap();
        assert_eq!(
            final_state.changes.len(),
            2,
            "After commit, should have Alice and Charlie"
        );

        let names: Vec<String> = final_state
            .changes
            .iter()
            .map(|(row, _)| {
                if let Value::Text(name) = &row.values[1] {
                    name.to_string()
                } else {
                    panic!("Expected text value");
                }
            })
            .collect();
        assert!(names.contains(&"Alice".to_string()));
        assert!(names.contains(&"Charlie".to_string()));
        assert!(!names.contains(&"Bob".to_string()));
    }

    #[test]
    fn test_uncommitted_update() {
        // Test that uncommitted updates (delete + insert) are handled correctly
        let (mut circuit, pager) = compile_sql!("SELECT * FROM users WHERE age > 18");

        // Initialize with some data
        let mut init_data = HashMap::default();
        let mut delta = Delta::new();
        delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(17),
            ],
        ); // Bob is 17, filtered out
        init_data.insert("users".to_string(), delta);

        let _ = test_execute(&mut circuit, init_data.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(init_data.clone(), pager.clone()))
            .unwrap();

        // Create uncommitted update: Bob turns 19 (update from 17 to 19)
        // This is modeled as delete + insert
        let mut uncommitted = HashMap::default();
        let mut uncommitted_delta = Delta::new();
        uncommitted_delta.delete(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(17),
            ],
        );
        uncommitted_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(19),
            ],
        );
        uncommitted.insert("users".to_string(), uncommitted_delta);

        // Execute with uncommitted update
        let tx_result = test_execute(&mut circuit, uncommitted.clone(), pager.clone()).unwrap();

        // Bob should now appear in the result (age 19 > 18)
        // Consolidate to see the final state
        let mut final_result = tx_result;
        final_result.consolidate();

        assert_eq!(final_result.changes.len(), 1, "Bob should now be in view");
        assert_eq!(
            final_result.changes[0].0.values[1],
            Value::Text("Bob".into())
        );
        assert_eq!(final_result.changes[0].0.values[2], Value::from_i64(19));

        // Now actually commit the update
        let mut commit_data = HashMap::default();
        let mut commit_delta = Delta::new();
        commit_delta.delete(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(17),
            ],
        );
        commit_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(19),
            ],
        );
        commit_data.insert("users".to_string(), commit_delta);

        // Commit the update
        pager
            .io
            .block(|| circuit.commit(commit_data.clone(), pager.clone()))
            .unwrap();

        // After committing, Bob should be in the view's state
        let state = get_current_state(pager, &circuit).unwrap();
        let mut consolidated_state = state;
        consolidated_state.consolidate();

        // Should have both Alice and Bob now
        assert_eq!(
            consolidated_state.changes.len(),
            2,
            "Should have Alice and Bob"
        );

        let names: Vec<String> = consolidated_state
            .changes
            .iter()
            .map(|(row, _)| {
                if let Value::Text(name) = &row.values[1] {
                    name.as_str().to_string()
                } else {
                    panic!("Expected text value");
                }
            })
            .collect();
        assert!(names.contains(&"Alice".to_string()));
        assert!(names.contains(&"Bob".to_string()));
    }

    #[test]
    fn test_uncommitted_filtered_delete() {
        // Test deleting a row that doesn't pass the filter
        let (mut circuit, pager) = compile_sql!("SELECT * FROM users WHERE age > 18");

        // Initialize with mixed data
        let mut init_data = HashMap::default();
        let mut delta = Delta::new();
        delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(15),
            ],
        ); // Bob doesn't pass filter
        init_data.insert("users".to_string(), delta);

        let _ = test_execute(&mut circuit, init_data.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(init_data.clone(), pager.clone()))
            .unwrap();

        // Create uncommitted delete for Bob (who isn't in the view because age=15)
        let mut uncommitted = HashMap::default();
        let mut uncommitted_delta = Delta::new();
        uncommitted_delta.delete(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(15),
            ],
        );
        uncommitted.insert("users".to_string(), uncommitted_delta);

        // Execute with uncommitted delete - should produce no output changes
        let tx_result = test_execute(&mut circuit, uncommitted, pager.clone()).unwrap();

        // Bob wasn't in the view, so deleting him produces no output
        assert_eq!(
            tx_result.changes.len(),
            0,
            "Deleting filtered row produces no changes"
        );

        // The view state should still only have Alice
        let state = get_current_state(pager, &circuit).unwrap();
        assert_eq!(state.changes.len(), 1, "View still has only Alice");
        assert_eq!(state.changes[0].0.values[1], Value::Text("Alice".into()));
    }

    #[test]
    fn test_uncommitted_mixed_operations() {
        // Test multiple uncommitted operations together
        let (mut circuit, pager) = compile_sql!("SELECT * FROM users WHERE age > 18");

        // Initialize with some data
        let mut init_data = HashMap::default();
        let mut delta = Delta::new();
        delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );
        init_data.insert("users".to_string(), delta);

        let _ = test_execute(&mut circuit, init_data.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(init_data.clone(), pager.clone()))
            .unwrap();

        // Verify initial state
        let state = get_current_state(pager.clone(), &circuit).unwrap();
        assert_eq!(state.changes.len(), 2);

        // Create uncommitted changes:
        // - Delete Alice
        // - Update Bob's age to 35
        // - Insert Charlie (age 40)
        // - Insert David (age 16, filtered out)
        let mut uncommitted = HashMap::default();
        let mut uncommitted_delta = Delta::new();
        // Delete Alice
        uncommitted_delta.delete(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        // Update Bob (delete + insert)
        uncommitted_delta.delete(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );
        uncommitted_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(35),
            ],
        );
        // Insert Charlie
        uncommitted_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(40),
            ],
        );
        // Insert David (will be filtered)
        uncommitted_delta.insert(
            4,
            vec![
                Value::from_i64(4),
                Value::Text("David".into()),
                Value::from_i64(16),
            ],
        );
        uncommitted.insert("users".to_string(), uncommitted_delta);

        // Execute with uncommitted changes
        let tx_result = test_execute(&mut circuit, uncommitted.clone(), pager.clone()).unwrap();

        // Result should show all changes: delete Alice, update Bob, insert Charlie and David
        assert_eq!(
            tx_result.changes.len(),
            4,
            "Should see all uncommitted mixed operations"
        );

        // Verify operator's internal state is unchanged
        let state_after = get_current_state(pager.clone(), &circuit).unwrap();
        assert_eq!(state_after.changes.len(), 2, "Still has Alice and Bob");

        // Commit all changes
        let mut commit_data = HashMap::default();
        let mut commit_delta = Delta::new();
        commit_delta.delete(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        commit_delta.delete(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );
        commit_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(35),
            ],
        );
        commit_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(40),
            ],
        );
        commit_delta.insert(
            4,
            vec![
                Value::from_i64(4),
                Value::Text("David".into()),
                Value::from_i64(16),
            ],
        );
        commit_data.insert("users".to_string(), commit_delta);

        let commit_result = test_execute(&mut circuit, commit_data.clone(), pager.clone()).unwrap();

        // Should see: Alice deleted, Bob deleted, Bob inserted, Charlie inserted
        // (David filtered out)
        assert_eq!(commit_result.changes.len(), 4, "Should see 4 changes");

        // Actually commit the changes to update operator state
        pager
            .io
            .block(|| circuit.commit(commit_data.clone(), pager.clone()))
            .unwrap();

        // After all commits, execute with no changes should return empty delta
        let empty_result = test_execute(&mut circuit, HashMap::default(), pager).unwrap();
        assert_eq!(empty_result.changes.len(), 0, "No changes when no new data");
    }

    #[test]
    fn test_uncommitted_aggregation() {
        // Test that aggregations work correctly with uncommitted changes
        // This tests the specific scenario where a transaction adds new data
        // and we need to see correct aggregation results within the transaction

        // Create a sales table schema for testing
        let _ = test_schema!();

        let (mut circuit, pager) = compile_sql!(
            "SELECT product_id, SUM(amount) as total, COUNT(*) as cnt FROM sales GROUP BY product_id"
        );

        // Initialize with base data: (1, 100), (1, 200), (2, 150), (2, 250)
        let mut init_data = HashMap::default();
        let mut delta = Delta::new();
        delta.insert(1, vec![Value::from_i64(1), Value::from_i64(100)]);
        delta.insert(2, vec![Value::from_i64(1), Value::from_i64(200)]);
        delta.insert(3, vec![Value::from_i64(2), Value::from_i64(150)]);
        delta.insert(4, vec![Value::from_i64(2), Value::from_i64(250)]);
        init_data.insert("sales".to_string(), delta);

        let _ = test_execute(&mut circuit, init_data.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(init_data.clone(), pager.clone()))
            .unwrap();

        // Verify initial state: product 1 total=300, product 2 total=400
        let state = get_current_state(pager.clone(), &circuit).unwrap();
        assert_eq!(state.changes.len(), 2, "Should have 2 product groups");

        // Build a map of product_id -> (total, count)
        let initial_results: HashMap<i64, (i64, i64)> = state
            .changes
            .iter()
            .map(|(row, _)| {
                // SUM might return Integer or Float, COUNT returns Integer
                let product_id = match &row.values[0] {
                    Value::Numeric(Numeric::Integer(id)) => *id,
                    _ => panic!("Product ID should be Integer, got {:?}", row.values[0]),
                };

                let total = match &row.values[1] {
                    Value::Numeric(Numeric::Integer(t)) => *t,
                    Value::Numeric(Numeric::Float(t)) => f64::from(*t) as i64,
                    _ => panic!("Total should be numeric, got {:?}", row.values[1]),
                };

                let count = match &row.values[2] {
                    Value::Numeric(Numeric::Integer(c)) => *c,
                    _ => panic!("Count should be Integer, got {:?}", row.values[2]),
                };

                (product_id, (total, count))
            })
            .collect();

        assert_eq!(
            initial_results.get(&1).unwrap(),
            &(300, 2),
            "Product 1 should have total=300, count=2"
        );
        assert_eq!(
            initial_results.get(&2).unwrap(),
            &(400, 2),
            "Product 2 should have total=400, count=2"
        );

        // Create uncommitted changes: INSERT (1, 50), (3, 300)
        let mut uncommitted = HashMap::default();
        let mut uncommitted_delta = Delta::new();
        uncommitted_delta.insert(5, vec![Value::from_i64(1), Value::from_i64(50)]); // Add to product 1
        uncommitted_delta.insert(6, vec![Value::from_i64(3), Value::from_i64(300)]); // New product 3
        uncommitted.insert("sales".to_string(), uncommitted_delta);

        // Execute with uncommitted data - simulating a read within transaction
        let tx_result = test_execute(&mut circuit, uncommitted.clone(), pager.clone()).unwrap();

        // Result should show the aggregate changes from uncommitted data
        // Product 1: retraction of (300, 2) and insertion of (350, 3)
        // Product 3: insertion of (300, 1) - new product
        assert_eq!(
            tx_result.changes.len(),
            3,
            "Should see aggregate changes from uncommitted data"
        );

        // IMPORTANT: Verify operator's internal state is unchanged
        let state_after = get_current_state(pager.clone(), &circuit).unwrap();
        assert_eq!(
            state_after.changes.len(),
            2,
            "Internal state should still have 2 groups"
        );

        // Verify the internal state still has original values
        let state_results: HashMap<i64, (i64, i64)> = state_after
            .changes
            .iter()
            .map(|(row, _)| {
                let product_id = match &row.values[0] {
                    Value::Numeric(Numeric::Integer(id)) => *id,
                    _ => panic!("Product ID should be Integer"),
                };

                let total = match &row.values[1] {
                    Value::Numeric(Numeric::Integer(t)) => *t,
                    Value::Numeric(Numeric::Float(t)) => f64::from(*t) as i64,
                    _ => panic!("Total should be numeric"),
                };

                let count = match &row.values[2] {
                    Value::Numeric(Numeric::Integer(c)) => *c,
                    _ => panic!("Count should be Integer"),
                };

                (product_id, (total, count))
            })
            .collect();

        assert_eq!(
            state_results.get(&1).unwrap(),
            &(300, 2),
            "Product 1 unchanged"
        );
        assert_eq!(
            state_results.get(&2).unwrap(),
            &(400, 2),
            "Product 2 unchanged"
        );
        assert!(
            !state_results.contains_key(&3),
            "Product 3 should not be in committed state"
        );

        // Now actually commit the changes
        let mut commit_data = HashMap::default();
        let mut commit_delta = Delta::new();
        commit_delta.insert(5, vec![Value::from_i64(1), Value::from_i64(50)]);
        commit_delta.insert(6, vec![Value::from_i64(3), Value::from_i64(300)]);
        commit_data.insert("sales".to_string(), commit_delta);

        let commit_result = test_execute(&mut circuit, commit_data.clone(), pager.clone()).unwrap();

        // Should see changes for product 1 (updated) and product 3 (new)
        assert_eq!(
            commit_result.changes.len(),
            3,
            "Should see 3 changes (delete old product 1, insert new product 1, insert product 3)"
        );

        // Actually commit the changes to update operator state
        pager
            .io
            .block(|| circuit.commit(commit_data.clone(), pager.clone()))
            .unwrap();

        // After commit, verify final state
        let final_state = get_current_state(pager, &circuit).unwrap();
        assert_eq!(
            final_state.changes.len(),
            3,
            "Should have 3 product groups after commit"
        );

        let final_results: HashMap<i64, (i64, i64)> = final_state
            .changes
            .iter()
            .map(|(row, _)| {
                let product_id = match &row.values[0] {
                    Value::Numeric(Numeric::Integer(id)) => *id,
                    _ => panic!("Product ID should be Integer"),
                };

                let total = match &row.values[1] {
                    Value::Numeric(Numeric::Integer(t)) => *t,
                    Value::Numeric(Numeric::Float(t)) => f64::from(*t) as i64,
                    _ => panic!("Total should be numeric"),
                };

                let count = match &row.values[2] {
                    Value::Numeric(Numeric::Integer(c)) => *c,
                    _ => panic!("Count should be Integer"),
                };

                (product_id, (total, count))
            })
            .collect();

        assert_eq!(
            final_results.get(&1).unwrap(),
            &(350, 3),
            "Product 1 should have total=350, count=3"
        );
        assert_eq!(
            final_results.get(&2).unwrap(),
            &(400, 2),
            "Product 2 should have total=400, count=2"
        );
        assert_eq!(
            final_results.get(&3).unwrap(),
            &(300, 1),
            "Product 3 should have total=300, count=1"
        );
    }

    #[test]
    fn test_uncommitted_data_visible_in_transaction() {
        // Test that uncommitted INSERTs are visible within the same transaction
        // This simulates: BEGIN; INSERT ...; SELECT * FROM view; COMMIT;

        let (mut circuit, pager) = compile_sql!("SELECT * FROM users WHERE age > 18");

        // Initialize with some data - need to match the schema (id, name, age)
        let mut init_data = HashMap::default();
        let mut delta = Delta::new();
        delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );
        init_data.insert("users".to_string(), delta);

        let _ = test_execute(&mut circuit, init_data.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(init_data.clone(), pager.clone()))
            .unwrap();

        // Verify initial state
        let state = get_current_state(pager.clone(), &circuit).unwrap();
        assert_eq!(
            state.len(),
            2,
            "Should have 2 users initially (both pass age > 18 filter)"
        );

        // Simulate a transaction: INSERT new users that pass the filter - match schema (id, name, age)
        let mut uncommitted = HashMap::default();
        let mut tx_delta = Delta::new();
        tx_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(35),
            ],
        );
        tx_delta.insert(
            4,
            vec![
                Value::from_i64(4),
                Value::Text("David".into()),
                Value::from_i64(20),
            ],
        );
        uncommitted.insert("users".to_string(), tx_delta);

        // Execute with uncommitted data - this should return the uncommitted changes
        // that passed through the filter (age > 18)
        let tx_result = test_execute(&mut circuit, uncommitted.clone(), pager.clone()).unwrap();

        // IMPORTANT: tx_result should contain the filtered uncommitted changes!
        // Both Charlie (35) and David (20) should pass the age > 18 filter
        assert_eq!(
            tx_result.len(),
            2,
            "Should see 2 uncommitted rows that pass filter"
        );

        // Verify the uncommitted results contain the expected rows
        let has_charlie = tx_result.changes.iter().any(|(row, _)| row.rowid == 3);
        assert!(
            has_charlie,
            "Should find Charlie (rowid=3) in uncommitted results"
        );

        let has_david = tx_result.changes.iter().any(|(row, _)| row.rowid == 4);
        assert!(
            has_david,
            "Should find David (rowid=4) in uncommitted results"
        );

        // CRITICAL: Verify the operator state wasn't modified by uncommitted execution
        let state_after_uncommitted = get_current_state(pager, &circuit).unwrap();
        assert_eq!(
            state_after_uncommitted.len(),
            2,
            "State should STILL be 2 after uncommitted execution - only Alice and Bob"
        );

        // The state should not contain Charlie or David
        let has_charlie_in_state = state_after_uncommitted
            .changes
            .iter()
            .any(|(row, _)| row.rowid == 3);
        let has_david_in_state = state_after_uncommitted
            .changes
            .iter()
            .any(|(row, _)| row.rowid == 4);
        assert!(
            !has_charlie_in_state,
            "Charlie should NOT be in operator state (uncommitted)"
        );
        assert!(
            !has_david_in_state,
            "David should NOT be in operator state (uncommitted)"
        );
    }

    #[test]
    fn test_uncommitted_aggregation_with_rollback() {
        // Test that rollback properly discards uncommitted aggregation changes
        // Similar to test_uncommitted_aggregation but explicitly tests rollback semantics

        // Create a simple aggregation circuit
        let (mut circuit, pager) =
            compile_sql!("SELECT age, COUNT(*) as cnt FROM users GROUP BY age");

        // Initialize with some data
        let mut init_data = HashMap::default();
        let mut delta = Delta::new();
        delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );
        delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(25),
            ],
        );
        delta.insert(
            4,
            vec![
                Value::from_i64(4),
                Value::Text("David".into()),
                Value::from_i64(30),
            ],
        );
        init_data.insert("users".to_string(), delta);

        let _ = test_execute(&mut circuit, init_data.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(init_data.clone(), pager.clone()))
            .unwrap();

        // Verify initial state: age 25 count=2, age 30 count=2
        let state = get_current_state(pager.clone(), &circuit).unwrap();
        assert_eq!(state.changes.len(), 2);

        let initial_counts: HashMap<i64, i64> = state
            .changes
            .iter()
            .map(|(row, _)| {
                if let (
                    Value::Numeric(Numeric::Integer(age)),
                    Value::Numeric(Numeric::Integer(count)),
                ) = (&row.values[0], &row.values[1])
                {
                    (*age, *count)
                } else {
                    panic!("Unexpected value types");
                }
            })
            .collect();

        assert_eq!(initial_counts.get(&25).unwrap(), &2);
        assert_eq!(initial_counts.get(&30).unwrap(), &2);

        // Create uncommitted changes that would affect aggregations
        let mut uncommitted = HashMap::default();
        let mut uncommitted_delta = Delta::new();
        // Add more people aged 25
        uncommitted_delta.insert(
            5,
            vec![
                Value::from_i64(5),
                Value::Text("Eve".into()),
                Value::from_i64(25),
            ],
        );
        uncommitted_delta.insert(
            6,
            vec![
                Value::from_i64(6),
                Value::Text("Frank".into()),
                Value::from_i64(25),
            ],
        );
        // Add person aged 35 (new group)
        uncommitted_delta.insert(
            7,
            vec![
                Value::from_i64(7),
                Value::Text("Grace".into()),
                Value::from_i64(35),
            ],
        );
        // Delete Bob (age 30)
        uncommitted_delta.delete(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );
        uncommitted.insert("users".to_string(), uncommitted_delta);

        // Execute with uncommitted changes
        let tx_result = test_execute(&mut circuit, uncommitted.clone(), pager.clone()).unwrap();

        // Should see the aggregate changes from uncommitted data
        // Age 25: retraction of count 1 and insertion of count 2
        // Age 30: insertion of count 1 (Bob is new for age 30)
        assert!(
            !tx_result.changes.is_empty(),
            "Should see aggregate changes from uncommitted data"
        );

        // Verify internal state is unchanged (simulating rollback by not committing)
        let state_after_rollback = get_current_state(pager, &circuit).unwrap();
        assert_eq!(
            state_after_rollback.changes.len(),
            2,
            "Should still have 2 age groups"
        );

        let rollback_counts: HashMap<i64, i64> = state_after_rollback
            .changes
            .iter()
            .map(|(row, _)| {
                if let (
                    Value::Numeric(Numeric::Integer(age)),
                    Value::Numeric(Numeric::Integer(count)),
                ) = (&row.values[0], &row.values[1])
                {
                    (*age, *count)
                } else {
                    panic!("Unexpected value types");
                }
            })
            .collect();

        // Verify counts are unchanged after rollback
        assert_eq!(
            rollback_counts.get(&25).unwrap(),
            &2,
            "Age 25 count unchanged"
        );
        assert_eq!(
            rollback_counts.get(&30).unwrap(),
            &2,
            "Age 30 count unchanged"
        );
        assert!(
            !rollback_counts.contains_key(&35),
            "Age 35 should not exist"
        );
    }

    #[test]
    fn test_circuit_rowid_update_consolidation() {
        let (pager, p1, p2, p3) = setup_btree_for_circuit();

        // Test that circuit properly consolidates state when rowid changes
        let mut circuit = DbspCircuit::new_table_only(p1, p2, p3);

        // Create a simple filter node
        let schema = Arc::new(LogicalSchema::new(vec![
            ColumnInfo {
                name: "id".to_string(),
                ty: Type::Integer,
                database: None,
                table: None,
                table_alias: None,
            },
            ColumnInfo {
                name: "value".to_string(),
                ty: Type::Integer,
                database: None,
                table: None,
                table_alias: None,
            },
        ]));

        // First create an input node with InputOperator
        let input_id = circuit.add_node(
            DbspOperator::Input {
                name: "test".to_string(),
                schema: schema.clone(),
            },
            vec![],
            Box::new(InputOperator::new("test".to_string())),
        );

        let filter_op = FilterOperator::new(FilterPredicate::GreaterThan {
            column_idx: 1, // "value" is at index 1
            value: Value::from_i64(10),
        });

        // Create the filter predicate using DbspExpr
        let predicate = DbspExpr::BinaryExpr {
            left: Box::new(DbspExpr::Column("value".to_string())),
            op: ast::Operator::Greater,
            right: Box::new(DbspExpr::Literal(Value::from_i64(10))),
        };

        let filter_id = circuit.add_node(
            DbspOperator::Filter { predicate },
            vec![input_id], // Filter takes input from the input node
            Box::new(filter_op),
        );

        circuit.set_root(filter_id, schema);

        // Initialize with a row
        let mut init_data = HashMap::default();
        let mut delta = Delta::new();
        delta.insert(5, vec![Value::from_i64(5), Value::from_i64(20)]);
        init_data.insert("test".to_string(), delta);

        let _ = test_execute(&mut circuit, init_data.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(init_data.clone(), pager.clone()))
            .unwrap();

        // Verify initial state
        let state = get_current_state(pager.clone(), &circuit).unwrap();
        assert_eq!(state.changes.len(), 1);
        assert_eq!(state.changes[0].0.rowid, 5);

        // Now update the rowid from 5 to 3
        let mut update_data = HashMap::default();
        let mut update_delta = Delta::new();
        update_delta.delete(5, vec![Value::from_i64(5), Value::from_i64(20)]);
        update_delta.insert(3, vec![Value::from_i64(3), Value::from_i64(20)]);
        update_data.insert("test".to_string(), update_delta);

        test_execute(&mut circuit, update_data.clone(), pager.clone()).unwrap();

        // Commit the changes to update operator state
        pager
            .io
            .block(|| circuit.commit(update_data.clone(), pager.clone()))
            .unwrap();

        // The circuit should consolidate the state properly
        let final_state = get_current_state(pager, &circuit).unwrap();
        assert_eq!(
            final_state.changes.len(),
            1,
            "Circuit should consolidate to single row"
        );
        assert_eq!(final_state.changes[0].0.rowid, 3);
        assert_eq!(
            &final_state.changes[0].0.values[..],
            &[Value::from_i64(3), Value::from_i64(20)]
        );
        assert_eq!(final_state.changes[0].1, 1);
    }

    #[test]
    fn test_circuit_respects_multiplicities() {
        let (mut circuit, pager) = compile_sql!("SELECT * from users");

        // Insert same row twice (multiplicity 2)
        let mut delta = Delta::new();
        delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );

        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), delta);
        test_execute(&mut circuit, inputs.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(inputs.clone(), pager.clone()))
            .unwrap();

        // Delete once (should leave multiplicity 1)
        let mut delete_one = Delta::new();
        delete_one.delete(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );

        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), delete_one);
        test_execute(&mut circuit, inputs.clone(), pager.clone()).unwrap();
        pager
            .io
            .block(|| circuit.commit(inputs.clone(), pager.clone()))
            .unwrap();

        // With proper DBSP: row still exists (weight 2 - 1 = 1)
        let state = get_current_state(pager, &circuit).unwrap();
        let mut consolidated = state;
        consolidated.consolidate();
        assert_eq!(
            consolidated.len(),
            1,
            "Row should still exist with multiplicity 1"
        );
    }

    #[test]
    fn test_join_with_aggregation() {
        // Test join followed by aggregation - verifying actual output
        let (mut circuit, pager) = compile_sql!(
            "SELECT u.name, SUM(o.quantity) as total_quantity
             FROM users u
             JOIN orders o ON u.id = o.user_id
             GROUP BY u.name"
        );

        // Create test data for users
        let mut users_delta = Delta::new();
        users_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(30),
            ],
        );
        users_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(25),
            ],
        );

        // Create test data for orders (order_id, user_id, product_id, quantity)
        let mut orders_delta = Delta::new();
        orders_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::from_i64(1),
                Value::from_i64(101),
                Value::from_i64(5),
            ],
        ); // Alice: 5
        orders_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::from_i64(1),
                Value::from_i64(102),
                Value::from_i64(3),
            ],
        ); // Alice: 3
        orders_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::from_i64(2),
                Value::from_i64(101),
                Value::from_i64(7),
            ],
        ); // Bob: 7
        orders_delta.insert(
            4,
            vec![
                Value::from_i64(4),
                Value::from_i64(1),
                Value::from_i64(103),
                Value::from_i64(2),
            ],
        ); // Alice: 2
        let inputs = HashMap::from_iter([
            ("users".to_string(), users_delta),
            ("orders".to_string(), orders_delta),
        ]);

        let result = test_execute(&mut circuit, inputs, pager).unwrap();

        // Should have 2 results: Alice with total 10, Bob with total 7
        assert_eq!(
            result.len(),
            2,
            "Should have aggregated results for Alice and Bob"
        );

        // Check the results
        let mut results_map: HashMap<String, f64> = HashMap::default();
        for (row, weight) in result.changes {
            assert_eq!(weight, 1);
            assert_eq!(row.values.len(), 2); // name and total_quantity

            if let (Value::Text(name), Value::Numeric(Numeric::Float(total))) =
                (&row.values[0], &row.values[1])
            {
                results_map.insert(name.to_string(), f64::from(*total));
            } else {
                panic!("Unexpected value types in result");
            }
        }

        assert_eq!(
            results_map.get("Alice"),
            Some(&10.0),
            "Alice should have total quantity 10"
        );
        assert_eq!(
            results_map.get("Bob"),
            Some(&7.0),
            "Bob should have total quantity 7"
        );
    }

    #[test]
    fn test_join_aggregate_with_filter() {
        // Test complex query with join, filter, and aggregation - verifying output
        let (mut circuit, pager) = compile_sql!(
            "SELECT u.name, SUM(o.quantity) as total
             FROM users u
             JOIN orders o ON u.id = o.user_id
             WHERE u.age > 18
             GROUP BY u.name"
        );

        // Create test data for users
        let mut users_delta = Delta::new();
        users_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(30),
            ],
        ); // age > 18
        users_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(17),
            ],
        ); // age <= 18
        users_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(25),
            ],
        ); // age > 18

        // Create test data for orders (order_id, user_id, product_id, quantity)
        let mut orders_delta = Delta::new();
        orders_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::from_i64(1),
                Value::from_i64(101),
                Value::from_i64(5),
            ],
        ); // Alice: 5
        orders_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::from_i64(2),
                Value::from_i64(102),
                Value::from_i64(10),
            ],
        ); // Bob: 10 (should be filtered)
        orders_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::from_i64(3),
                Value::from_i64(101),
                Value::from_i64(7),
            ],
        ); // Charlie: 7
        orders_delta.insert(
            4,
            vec![
                Value::from_i64(4),
                Value::from_i64(1),
                Value::from_i64(103),
                Value::from_i64(3),
            ],
        ); // Alice: 3

        let inputs = HashMap::from_iter([
            ("users".to_string(), users_delta),
            ("orders".to_string(), orders_delta),
        ]);

        let result = test_execute(&mut circuit, inputs, pager).unwrap();

        // Should only have results for Alice and Charlie (Bob filtered out due to age <= 18)
        assert_eq!(
            result.len(),
            2,
            "Should only have results for users with age > 18"
        );

        // Check the results
        let mut results_map: HashMap<String, f64> = HashMap::default();
        for (row, weight) in result.changes {
            assert_eq!(weight, 1);
            assert_eq!(row.values.len(), 2); // name and total

            if let (Value::Text(name), Value::Numeric(Numeric::Float(total))) =
                (&row.values[0], &row.values[1])
            {
                results_map.insert(name.to_string(), f64::from(*total));
            }
        }

        assert_eq!(
            results_map.get("Alice"),
            Some(&8.0),
            "Alice should have total 8"
        );
        assert_eq!(
            results_map.get("Charlie"),
            Some(&7.0),
            "Charlie should have total 7"
        );
        assert_eq!(results_map.get("Bob"), None, "Bob should be filtered out");
    }

    #[test]
    fn test_three_way_join_execution() {
        // Test executing a 3-way join with aggregation
        let (mut circuit, pager) = compile_sql!(
            "SELECT u.name, p.product_name, SUM(o.quantity) as total
             FROM users u
             JOIN orders o ON u.id = o.user_id
             JOIN products p ON o.product_id = p.product_id
             GROUP BY u.name, p.product_name"
        );

        // Create test data for users
        let mut users_delta = Delta::new();
        users_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        users_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );

        // Create test data for products
        let mut products_delta = Delta::new();
        products_delta.insert(
            100,
            vec![
                Value::from_i64(100),
                Value::Text("Widget".into()),
                Value::from_i64(50),
            ],
        );
        products_delta.insert(
            101,
            vec![
                Value::from_i64(101),
                Value::Text("Gadget".into()),
                Value::from_i64(75),
            ],
        );
        products_delta.insert(
            102,
            vec![
                Value::from_i64(102),
                Value::Text("Doohickey".into()),
                Value::from_i64(25),
            ],
        );

        // Create test data for orders joining users and products
        let mut orders_delta = Delta::new();
        // Alice orders 5 Widgets
        orders_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::from_i64(1),
                Value::from_i64(100),
                Value::from_i64(5),
            ],
        );
        // Alice orders 3 Gadgets
        orders_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::from_i64(1),
                Value::from_i64(101),
                Value::from_i64(3),
            ],
        );
        // Bob orders 7 Widgets
        orders_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::from_i64(2),
                Value::from_i64(100),
                Value::from_i64(7),
            ],
        );
        // Bob orders 2 Doohickeys
        orders_delta.insert(
            4,
            vec![
                Value::from_i64(4),
                Value::from_i64(2),
                Value::from_i64(102),
                Value::from_i64(2),
            ],
        );
        // Alice orders 4 more Widgets
        orders_delta.insert(
            5,
            vec![
                Value::from_i64(5),
                Value::from_i64(1),
                Value::from_i64(100),
                Value::from_i64(4),
            ],
        );

        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), users_delta);
        inputs.insert("products".to_string(), products_delta);
        inputs.insert("orders".to_string(), orders_delta);

        // Execute the 3-way join with aggregation
        let result = test_execute(&mut circuit, inputs.clone(), pager).unwrap();

        // We should get aggregated results for each user-product combination
        // Expected results:
        // - Alice, Widget: 9 (5 + 4)
        // - Alice, Gadget: 3
        // - Bob, Widget: 7
        // - Bob, Doohickey: 2
        assert_eq!(result.len(), 4, "Should have 4 aggregated results");

        // Verify aggregation results
        let mut found_results = HashSet::default();
        for (row, weight) in result.changes.iter() {
            assert_eq!(*weight, 1);
            // Row should have name, product_name, and sum columns
            assert_eq!(row.values.len(), 3);

            if let (
                Value::Text(name),
                Value::Text(product),
                Value::Numeric(Numeric::Float(total)),
            ) = (&row.values[0], &row.values[1], &row.values[2])
            {
                let key = format!("{}-{}", name.as_ref(), product.as_ref());
                found_results.insert(key.clone());

                match key.as_str() {
                    "Alice-Widget" => {
                        assert_eq!(*total, 9.0, "Alice should have ordered 9 Widgets total")
                    }
                    "Alice-Gadget" => {
                        assert_eq!(*total, 3.0, "Alice should have ordered 3 Gadgets")
                    }
                    "Bob-Widget" => assert_eq!(*total, 7.0, "Bob should have ordered 7 Widgets"),
                    "Bob-Doohickey" => {
                        assert_eq!(*total, 2.0, "Bob should have ordered 2 Doohickeys")
                    }
                    _ => panic!("Unexpected result: {key}"),
                }
            } else {
                panic!("Unexpected value types in result");
            }
        }

        // Ensure we found all expected combinations
        assert!(found_results.contains("Alice-Widget"));
        assert!(found_results.contains("Alice-Gadget"));
        assert!(found_results.contains("Bob-Widget"));
        assert!(found_results.contains("Bob-Doohickey"));
    }

    #[test]
    fn test_join_execution() {
        let (mut circuit, pager) = compile_sql!(
            "SELECT u.name, o.quantity FROM users u JOIN orders o ON u.id = o.user_id"
        );

        // Create test data for users
        let mut users_delta = Delta::new();
        users_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        users_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );

        // Create test data for orders
        let mut orders_delta = Delta::new();
        orders_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::from_i64(1),
                Value::from_i64(100),
                Value::from_i64(5),
            ],
        );
        orders_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::from_i64(1),
                Value::from_i64(101),
                Value::from_i64(3),
            ],
        );
        orders_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::from_i64(2),
                Value::from_i64(102),
                Value::from_i64(7),
            ],
        );

        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), users_delta);
        inputs.insert("orders".to_string(), orders_delta);

        // Execute the join
        let result = test_execute(&mut circuit, inputs.clone(), pager).unwrap();

        // We should get 3 results (2 orders for Alice, 1 for Bob)
        assert_eq!(result.len(), 3, "Should have 3 join results");

        // Verify the join results contain the correct data
        let results: Vec<_> = result.changes.iter().collect();

        // Check that we have the expected joined rows
        for (row, weight) in results {
            assert_eq!(*weight, 1); // All weights should be 1 for insertions
                                    // Row should have name and quantity columns
            assert_eq!(row.values.len(), 2);
        }
    }

    #[test]
    fn test_three_way_join_with_column_ambiguity() {
        // Test three-way join with aggregation where multiple tables have columns with the same name
        // Ensures that column references are correctly resolved to their respective tables
        // Tables: customers(id, name), purchases(id, customer_id, vendor_id, quantity), vendors(id, name, price)
        // Note: both customers and vendors have 'id' and 'name' columns which can cause ambiguity

        let sql = "SELECT c.name as customer_name, v.name as vendor_name,
                          SUM(p.quantity) as total_quantity,
                          SUM(p.quantity * v.price) as total_value
                   FROM customers c
                   JOIN purchases p ON c.id = p.customer_id
                   JOIN vendors v ON p.vendor_id = v.id
                   GROUP BY c.name, v.name";

        let (mut circuit, pager) = compile_sql!(sql);

        // Create test data for customers (id, name)
        let mut customers_delta = Delta::new();
        customers_delta.insert(1, vec![Value::from_i64(1), Value::Text("Alice".into())]);
        customers_delta.insert(2, vec![Value::from_i64(2), Value::Text("Bob".into())]);

        // Create test data for vendors (id, name, price)
        let mut vendors_delta = Delta::new();
        vendors_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Widget Co".into()),
                Value::from_i64(10),
            ],
        );
        vendors_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Gadget Inc".into()),
                Value::from_i64(20),
            ],
        );

        // Create test data for purchases (id, customer_id, vendor_id, quantity)
        let mut purchases_delta = Delta::new();
        // Alice purchases 5 units from Widget Co
        purchases_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::from_i64(1), // customer_id: Alice
                Value::from_i64(1), // vendor_id: Widget Co
                Value::from_i64(5),
            ],
        );
        // Alice purchases 3 units from Gadget Inc
        purchases_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::from_i64(1), // customer_id: Alice
                Value::from_i64(2), // vendor_id: Gadget Inc
                Value::from_i64(3),
            ],
        );
        // Bob purchases 2 units from Widget Co
        purchases_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::from_i64(2), // customer_id: Bob
                Value::from_i64(1), // vendor_id: Widget Co
                Value::from_i64(2),
            ],
        );
        // Alice purchases 4 more units from Widget Co
        purchases_delta.insert(
            4,
            vec![
                Value::from_i64(4),
                Value::from_i64(1), // customer_id: Alice
                Value::from_i64(1), // vendor_id: Widget Co
                Value::from_i64(4),
            ],
        );

        let inputs = HashMap::from_iter([
            ("customers".to_string(), customers_delta),
            ("purchases".to_string(), purchases_delta),
            ("vendors".to_string(), vendors_delta),
        ]);

        let result = test_execute(&mut circuit, inputs, pager).unwrap();

        // Expected results:
        // Alice|Gadget Inc|3|60    (3 units * 20 price = 60)
        // Alice|Widget Co|9|90     (9 units * 10 price = 90)
        // Bob|Widget Co|2|20       (2 units * 10 price = 20)

        assert_eq!(result.len(), 3, "Should have 3 aggregated results");

        // Sort results for consistent testing
        let mut results: Vec<_> = result.changes.into_iter().collect();
        results.sort_by(|a, b| {
            let a_cust = &a.0.values[0];
            let a_vend = &a.0.values[1];
            let b_cust = &b.0.values[0];
            let b_vend = &b.0.values[1];
            (a_cust, a_vend).cmp(&(b_cust, b_vend))
        });

        // Verify Alice's Gadget Inc purchases
        assert_eq!(results[0].0.values[0], Value::Text("Alice".into()));
        assert_eq!(results[0].0.values[1], Value::Text("Gadget Inc".into()));
        assert_eq!(results[0].0.values[2], Value::from_i64(3)); // total_quantity
        assert_eq!(results[0].0.values[3], Value::from_i64(60)); // total_value

        // Verify Alice's Widget Co purchases
        assert_eq!(results[1].0.values[0], Value::Text("Alice".into()));
        assert_eq!(results[1].0.values[1], Value::Text("Widget Co".into()));
        assert_eq!(results[1].0.values[2], Value::from_i64(9)); // total_quantity
        assert_eq!(results[1].0.values[3], Value::from_i64(90)); // total_value

        // Verify Bob's Widget Co purchases
        assert_eq!(results[2].0.values[0], Value::Text("Bob".into()));
        assert_eq!(results[2].0.values[1], Value::Text("Widget Co".into()));
        assert_eq!(results[2].0.values[2], Value::from_i64(2)); // total_quantity
        assert_eq!(results[2].0.values[3], Value::from_i64(20)); // total_value
    }

    #[test]
    fn test_projection_with_function_and_ambiguous_columns() {
        // Test projection with functions operating on potentially ambiguous columns
        // Uses HEX() function on sum of columns from different tables with same names
        // Tables: customers(id, name), vendors(id, name, price), purchases(id, customer_id, vendor_id, quantity)
        // This test ensures column references are correctly resolved to their respective tables

        let sql = "SELECT HEX(c.id + v.id) as hex_sum,
                          UPPER(c.name) as customer_upper,
                          LOWER(v.name) as vendor_lower,
                          c.id * v.price as product_value
                   FROM customers c
                   JOIN vendors v ON c.id = v.id";

        let (mut circuit, pager) = compile_sql!(sql);

        // Create test data for customers (id, name)
        let mut customers_delta = Delta::new();
        customers_delta.insert(1, vec![Value::from_i64(1), Value::Text("Alice".into())]);
        customers_delta.insert(2, vec![Value::from_i64(2), Value::Text("Bob".into())]);
        customers_delta.insert(3, vec![Value::from_i64(3), Value::Text("Charlie".into())]);

        // Create test data for vendors (id, name, price)
        let mut vendors_delta = Delta::new();
        vendors_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Widget Co".into()),
                Value::from_i64(10),
            ],
        );
        vendors_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Gadget Inc".into()),
                Value::from_i64(20),
            ],
        );
        vendors_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Tool Corp".into()),
                Value::from_i64(30),
            ],
        );

        let inputs = HashMap::from_iter([
            ("customers".to_string(), customers_delta),
            ("vendors".to_string(), vendors_delta),
        ]);

        let result = test_execute(&mut circuit, inputs, pager).unwrap();

        // Expected results:
        // For customer 1 (Alice) + vendor 1:
        //   - HEX(1 + 1) = HEX(2) = "32"
        //   - UPPER("Alice") = "ALICE"
        //   - LOWER("Widget Co") = "widget co"
        //   - 1 * 10 = 10
        assert_eq!(result.len(), 3, "Should have 3 join results");

        let mut results = result.changes;
        results.sort_by_key(|(row, _)| {
            // Sort by the product_value column for predictable ordering
            match &row.values[3] {
                Value::Numeric(Numeric::Integer(n)) => *n,
                _ => 0,
            }
        });

        // First result: Alice + Widget Co
        assert_eq!(results[0].0.values[0], Value::Text("32".into())); // HEX(2)
        assert_eq!(results[0].0.values[1], Value::Text("ALICE".into()));
        assert_eq!(results[0].0.values[2], Value::Text("widget co".into()));
        assert_eq!(results[0].0.values[3], Value::from_i64(10)); // 1 * 10

        // Second result: Bob + Gadget Inc
        assert_eq!(results[1].0.values[0], Value::Text("34".into())); // HEX(4)
        assert_eq!(results[1].0.values[1], Value::Text("BOB".into()));
        assert_eq!(results[1].0.values[2], Value::Text("gadget inc".into()));
        assert_eq!(results[1].0.values[3], Value::from_i64(40)); // 2 * 20

        // Third result: Charlie + Tool Corp
        assert_eq!(results[2].0.values[0], Value::Text("36".into())); // HEX(6)
        assert_eq!(results[2].0.values[1], Value::Text("CHARLIE".into()));
        assert_eq!(results[2].0.values[2], Value::Text("tool corp".into()));
        assert_eq!(results[2].0.values[3], Value::from_i64(90)); // 3 * 30
    }

    #[test]
    fn test_projection_column_selection_after_join() {
        // Test selecting specific columns after a join, especially with overlapping column names
        // This ensures the projection correctly picks columns by their qualified references

        let sql = "SELECT c.id as customer_id,
                          c.name as customer_name,
                          o.order_id,
                          o.quantity,
                          p.product_name
                   FROM users c
                   JOIN orders o ON c.id = o.user_id
                   JOIN products p ON o.product_id = p.product_id
                   WHERE o.quantity > 2";

        let (mut circuit, pager) = compile_sql!(sql);

        // Create test data for users (id, name, age)
        let mut users_delta = Delta::new();
        users_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        users_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );

        // Create test data for orders (order_id, user_id, product_id, quantity)
        let mut orders_delta = Delta::new();
        orders_delta.insert(
            1,
            vec![
                Value::from_i64(101),
                Value::from_i64(1),   // Alice
                Value::from_i64(201), // Widget
                Value::from_i64(5),   // quantity > 2
            ],
        );
        orders_delta.insert(
            2,
            vec![
                Value::from_i64(102),
                Value::from_i64(2),   // Bob
                Value::from_i64(202), // Gadget
                Value::from_i64(1),   // quantity <= 2, filtered out
            ],
        );
        orders_delta.insert(
            3,
            vec![
                Value::from_i64(103),
                Value::from_i64(1),   // Alice
                Value::from_i64(202), // Gadget
                Value::from_i64(3),   // quantity > 2
            ],
        );

        // Create test data for products (product_id, product_name, price)
        let mut products_delta = Delta::new();
        products_delta.insert(
            201,
            vec![
                Value::from_i64(201),
                Value::Text("Widget".into()),
                Value::from_i64(10),
            ],
        );
        products_delta.insert(
            202,
            vec![
                Value::from_i64(202),
                Value::Text("Gadget".into()),
                Value::from_i64(20),
            ],
        );

        let inputs = HashMap::from_iter([
            ("users".to_string(), users_delta),
            ("orders".to_string(), orders_delta),
            ("products".to_string(), products_delta),
        ]);

        let result = test_execute(&mut circuit, inputs, pager).unwrap();

        // Should have 2 results (orders with quantity > 2)
        assert_eq!(result.len(), 2, "Should have 2 results after filtering");

        let mut results = result.changes;
        results.sort_by_key(|(row, _)| {
            match &row.values[2] {
                // Sort by order_id
                Value::Numeric(Numeric::Integer(n)) => *n,
                _ => 0,
            }
        });

        // First result: Alice's order 101 for Widget
        assert_eq!(results[0].0.values[0], Value::from_i64(1)); // customer_id
        assert_eq!(results[0].0.values[1], Value::Text("Alice".into())); // customer_name
        assert_eq!(results[0].0.values[2], Value::from_i64(101)); // order_id
        assert_eq!(results[0].0.values[3], Value::from_i64(5)); // quantity
        assert_eq!(results[0].0.values[4], Value::Text("Widget".into())); // product_name

        // Second result: Alice's order 103 for Gadget
        assert_eq!(results[1].0.values[0], Value::from_i64(1)); // customer_id
        assert_eq!(results[1].0.values[1], Value::Text("Alice".into())); // customer_name
        assert_eq!(results[1].0.values[2], Value::from_i64(103)); // order_id
        assert_eq!(results[1].0.values[3], Value::from_i64(3)); // quantity
        assert_eq!(results[1].0.values[4], Value::Text("Gadget".into())); // product_name
    }

    #[test]
    fn test_projection_column_reordering_and_duplication() {
        // Test that projection can reorder columns and select the same column multiple times
        // This is important for views that need specific column arrangements

        let sql = "SELECT o.quantity,
                          u.name,
                          u.id,
                          o.quantity * 2 as double_quantity,
                          u.id as user_id_again
                   FROM users u
                   JOIN orders o ON u.id = o.user_id
                   WHERE u.id = 1";

        let (mut circuit, pager) = compile_sql!(sql);

        // Create test data for users
        let mut users_delta = Delta::new();
        users_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );

        // Create test data for orders
        let mut orders_delta = Delta::new();
        orders_delta.insert(
            1,
            vec![
                Value::from_i64(101),
                Value::from_i64(1),   // user_id
                Value::from_i64(201), // product_id
                Value::from_i64(5),   // quantity
            ],
        );
        orders_delta.insert(
            2,
            vec![
                Value::from_i64(102),
                Value::from_i64(1),   // user_id
                Value::from_i64(202), // product_id
                Value::from_i64(3),   // quantity
            ],
        );

        let inputs = HashMap::from_iter([
            ("users".to_string(), users_delta),
            ("orders".to_string(), orders_delta),
        ]);

        let result = test_execute(&mut circuit, inputs, pager).unwrap();

        assert_eq!(result.len(), 2, "Should have 2 results for user 1");

        // Check that columns are in the right order and values are correct
        for (row, _) in &result.changes {
            // Column 0: o.quantity (5 or 3)
            assert!(matches!(
                row.values[0],
                Value::Numeric(Numeric::Integer(5)) | Value::Numeric(Numeric::Integer(3))
            ));
            // Column 1: u.name
            assert_eq!(row.values[1], Value::Text("Alice".into()));
            // Column 2: u.id
            assert_eq!(row.values[2], Value::from_i64(1));
            // Column 3: o.quantity * 2 (10 or 6)
            assert!(matches!(
                row.values[3],
                Value::Numeric(Numeric::Integer(10)) | Value::Numeric(Numeric::Integer(6))
            ));
            // Column 4: u.id again
            assert_eq!(row.values[4], Value::from_i64(1));
        }
    }

    #[test]
    fn test_join_with_aggregate_execution() {
        let (mut circuit, pager) = compile_sql!(
            "SELECT u.name, SUM(o.quantity) as total_quantity
             FROM users u
             JOIN orders o ON u.id = o.user_id
             GROUP BY u.name"
        );

        // Create test data for users
        let mut users_delta = Delta::new();
        users_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        users_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );

        // Create test data for orders
        let mut orders_delta = Delta::new();
        orders_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::from_i64(1),
                Value::from_i64(100),
                Value::from_i64(5),
            ],
        );
        orders_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::from_i64(1),
                Value::from_i64(101),
                Value::from_i64(3),
            ],
        );
        orders_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::from_i64(2),
                Value::from_i64(102),
                Value::from_i64(7),
            ],
        );

        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), users_delta);
        inputs.insert("orders".to_string(), orders_delta);

        // Execute the join with aggregation
        let result = test_execute(&mut circuit, inputs.clone(), pager).unwrap();

        // We should get 2 aggregated results (one for Alice, one for Bob)
        assert_eq!(result.len(), 2, "Should have 2 aggregated results");

        // Verify aggregation results
        for (row, weight) in result.changes.iter() {
            assert_eq!(*weight, 1);
            // Row should have name and sum columns
            assert_eq!(row.values.len(), 2);

            // Check the aggregated values
            if let Value::Text(name) = &row.values[0] {
                if name.as_ref() == "Alice" {
                    // Alice should have total quantity of 8 (5 + 3)
                    assert_eq!(row.values[1], Value::from_i64(8));
                } else if name.as_ref() == "Bob" {
                    // Bob should have total quantity of 7
                    assert_eq!(row.values[1], Value::from_i64(7));
                }
            }
        }
    }

    #[test]
    fn test_filter_with_qualified_columns_in_join() {
        // Test that filters correctly handle qualified column names in joins
        // when multiple tables have columns with the SAME names.
        // Both users and customers tables have 'id' and 'name' columns which can be ambiguous.

        let (mut circuit, pager) = compile_sql!(
            "SELECT users.id, users.name, customers.id, customers.name
             FROM users
             JOIN customers ON users.id = customers.id
             WHERE users.id > 1 AND customers.id < 100"
        );

        // Create test data
        let mut users_delta = Delta::new();
        let mut customers_delta = Delta::new();

        // Users data: (id, name, age)
        users_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(30),
            ],
        ); // id = 1
        users_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(25),
            ],
        ); // id = 2
        users_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(35),
            ],
        ); // id = 3

        // Customers data: (id, name, email)
        customers_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Customer Alice".into()),
                Value::Text("alice@example.com".into()),
            ],
        ); // id = 1
        customers_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Customer Bob".into()),
                Value::Text("bob@example.com".into()),
            ],
        ); // id = 2
        customers_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Customer Charlie".into()),
                Value::Text("charlie@example.com".into()),
            ],
        ); // id = 3

        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), users_delta);
        inputs.insert("customers".to_string(), customers_delta);

        let result = test_execute(&mut circuit, inputs.clone(), pager).unwrap();

        // Should get rows where users.id > 1 AND customers.id < 100
        // - users.id=2 (> 1) AND customers.id=2 (< 100) ✓
        // - users.id=3 (> 1) AND customers.id=3 (< 100) ✓
        // Alice excluded: users.id=1 (NOT > 1)
        assert_eq!(result.len(), 2, "Should have 2 filtered results");

        let (row, weight) = &result.changes[0];
        assert_eq!(*weight, 1);
        assert_eq!(row.values.len(), 4, "Should have 4 columns");

        // Verify the filter correctly used qualified columns for Bob
        assert_eq!(row.values[0], Value::from_i64(2), "users.id should be 2");
        assert_eq!(
            row.values[1],
            Value::Text("Bob".into()),
            "users.name should be Bob"
        );
        assert_eq!(
            row.values[2],
            Value::from_i64(2),
            "customers.id should be 2"
        );
        assert_eq!(
            row.values[3],
            Value::Text("Customer Bob".into()),
            "customers.name should be Customer Bob"
        );
    }

    #[test]
    fn test_expression_in_where_clause() {
        // Test expressions in WHERE clauses like (quantity * price) >= 400
        let (mut circuit, pager) = compile_sql!("SELECT * FROM users WHERE (age * 2) > 30");

        // Create test data
        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(20), // age * 2 = 40 > 30, should pass
            ],
        );
        input_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(10), // age * 2 = 20 <= 30, should be filtered out
            ],
        );
        input_delta.insert(
            3,
            vec![
                Value::from_i64(3),
                Value::Text("Charlie".into()),
                Value::from_i64(16), // age * 2 = 32 > 30, should pass
            ],
        );

        // Create input map
        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, inputs.clone(), pager).unwrap();

        // Should only have Alice and Charlie (age * 2 > 30)
        assert_eq!(
            result.changes.len(),
            2,
            "Should have 2 rows after filtering"
        );

        // Check Alice
        let alice = result
            .changes
            .iter()
            .find(|(row, _)| row.values[0] == Value::from_i64(1))
            .expect("Alice should be in result");
        assert_eq!(alice.0.values[1], Value::Text("Alice".into()));
        assert_eq!(alice.0.values[2], Value::from_i64(20));

        // Check Charlie
        let charlie = result
            .changes
            .iter()
            .find(|(row, _)| row.values[0] == Value::from_i64(3))
            .expect("Charlie should be in result");
        assert_eq!(charlie.0.values[1], Value::Text("Charlie".into()));
        assert_eq!(charlie.0.values[2], Value::from_i64(16));

        // Bob should not be in result
        let bob = result
            .changes
            .iter()
            .find(|(row, _)| row.values[0] == Value::from_i64(2));
        assert!(bob.is_none(), "Bob should be filtered out");
    }

    fn make_column_info(name: &str, ty: Type, table: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            ty,
            database: None,
            table: Some(table.to_string()),
            table_alias: None,
        }
    }

    #[test]
    fn test_resolve_join_columns_normal_order() {
        // Normal case: left.id = right.id
        let left_schema = LogicalSchema::new(vec![
            ColumnInfo {
                name: "id".to_string(),
                ty: Type::Integer,
                database: None,
                table: Some("left".to_string()),
                table_alias: None,
            },
            ColumnInfo {
                name: "name".to_string(),
                ty: Type::Text,
                database: None,
                table: Some("left".to_string()),
                table_alias: None,
            },
        ]);
        let right_schema = LogicalSchema::new(vec![
            ColumnInfo {
                name: "id".to_string(),
                ty: Type::Integer,
                database: None,
                table: Some("right".to_string()),
                table_alias: None,
            },
            ColumnInfo {
                name: "value".to_string(),
                ty: Type::Integer,
                database: None,
                table: Some("right".to_string()),
                table_alias: None,
            },
        ]);

        let left_col = Column {
            name: "id".to_string(),
            table: Some("left".to_string()),
        };
        let right_col = Column {
            name: "id".to_string(),
            table: Some("right".to_string()),
        };

        let result =
            DbspCompiler::resolve_join_columns(&left_col, &right_col, &left_schema, &right_schema);
        assert!(result.is_ok());
        let (actual_left, left_idx, actual_right, right_idx) = result.unwrap();
        assert_eq!(actual_left.name, "id");
        assert_eq!(actual_left.table, Some("left".to_string()));
        assert_eq!(left_idx, 0);
        assert_eq!(actual_right.name, "id");
        assert_eq!(actual_right.table, Some("right".to_string()));
        assert_eq!(right_idx, 0);
    }

    #[test]
    fn test_resolve_join_columns_swapped_order() {
        // Swapped case: right.id = left.id
        let left_schema = LogicalSchema::new(vec![
            make_column_info("id", Type::Integer, "left"),
            make_column_info("name", Type::Text, "left"),
        ]);
        let right_schema = LogicalSchema::new(vec![
            make_column_info("id", Type::Integer, "right"),
            make_column_info("value", Type::Integer, "right"),
        ]);

        let right_col = Column {
            name: "id".to_string(),
            table: Some("right".to_string()),
        };
        let left_col = Column {
            name: "id".to_string(),
            table: Some("left".to_string()),
        };

        let result =
            DbspCompiler::resolve_join_columns(&right_col, &left_col, &left_schema, &right_schema);
        assert!(result.is_ok());
        let (actual_left, left_idx, actual_right, right_idx) = result.unwrap();
        assert_eq!(actual_left.name, "id");
        assert_eq!(actual_left.table, Some("left".to_string()));
        assert_eq!(left_idx, 0);
        assert_eq!(actual_right.name, "id");
        assert_eq!(actual_right.table, Some("right".to_string()));
        assert_eq!(right_idx, 0);
    }

    #[test]
    fn test_resolve_join_columns_one_ambiguous_one_not() {
        // Both tables have 'id', but only left has 'other_id'
        let left_schema = LogicalSchema::new(vec![
            make_column_info("id", Type::Integer, "left"),
            make_column_info("other_id", Type::Integer, "left"),
        ]);
        let right_schema = LogicalSchema::new(vec![
            make_column_info("id", Type::Integer, "right"),
            make_column_info("value", Type::Integer, "right"),
        ]);

        // Unqualified 'id' with qualified 'left.other_id'
        let id_col = Column {
            name: "id".to_string(),
            table: None,
        };
        let other_id_col = Column {
            name: "other_id".to_string(),
            table: Some("left".to_string()),
        };

        // id from right, other_id from left
        let result =
            DbspCompiler::resolve_join_columns(&id_col, &other_id_col, &left_schema, &right_schema);
        assert!(result.is_ok());
        let (actual_left, left_idx, actual_right, right_idx) = result.unwrap();
        assert_eq!(actual_left.name, "other_id");
        assert_eq!(left_idx, 1);
        assert_eq!(actual_right.name, "id");
        assert_eq!(right_idx, 0);
    }

    #[test]
    fn test_resolve_join_columns_mixed_qualified() {
        // One qualified, one unqualified, column exists on both sides
        let left_schema = LogicalSchema::new(vec![
            make_column_info("id", Type::Integer, "left"),
            make_column_info("name", Type::Text, "left"),
        ]);
        let right_schema = LogicalSchema::new(vec![
            make_column_info("id", Type::Integer, "right"),
            make_column_info("name", Type::Text, "right"),
        ]);

        // Qualified left.id with unqualified name
        let left_id = Column {
            name: "id".to_string(),
            table: Some("left".to_string()),
        };
        let name_unqualified = Column {
            name: "name".to_string(),
            table: None,
        };

        let result = DbspCompiler::resolve_join_columns(
            &left_id,
            &name_unqualified,
            &left_schema,
            &right_schema,
        );
        // left.id is explicitly from left, so unqualified 'name' must be resolved from right
        assert!(result.is_ok());
        let (actual_left, left_idx, actual_right, right_idx) = result.unwrap();
        assert_eq!(actual_left.name, "id");
        assert_eq!(left_idx, 0);
        assert_eq!(actual_right.name, "name");
        assert_eq!(right_idx, 1);
    }

    #[test]
    fn test_resolve_join_columns_both_from_same_side() {
        // Both columns from left table - should fail
        let left_schema = LogicalSchema::new(vec![
            make_column_info("id", Type::Integer, "left"),
            make_column_info("other_id", Type::Integer, "left"),
        ]);
        let right_schema =
            LogicalSchema::new(vec![make_column_info("value", Type::Integer, "right")]);

        let left_id = Column {
            name: "id".to_string(),
            table: Some("left".to_string()),
        };
        let left_other_id = Column {
            name: "other_id".to_string(),
            table: Some("left".to_string()),
        };

        let result = DbspCompiler::resolve_join_columns(
            &left_id,
            &left_other_id,
            &left_schema,
            &right_schema,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must come from different input tables"));
    }

    #[test]
    fn test_resolve_join_columns_nonexistent_column() {
        // Column doesn't exist in either table
        let left_schema = LogicalSchema::new(vec![make_column_info("id", Type::Integer, "left")]);
        let right_schema =
            LogicalSchema::new(vec![make_column_info("value", Type::Integer, "right")]);

        let id_col = Column {
            name: "id".to_string(),
            table: None,
        };
        let nonexistent_col = Column {
            name: "does_not_exist".to_string(),
            table: None,
        };

        let result = DbspCompiler::resolve_join_columns(
            &id_col,
            &nonexistent_col,
            &left_schema,
            &right_schema,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_resolve_join_columns_both_qualified() {
        // Both columns qualified - should work normally
        let left_schema = LogicalSchema::new(vec![
            make_column_info("id", Type::Integer, "left"),
            make_column_info("name", Type::Text, "left"),
        ]);
        let right_schema = LogicalSchema::new(vec![
            make_column_info("id", Type::Integer, "right"),
            make_column_info("value", Type::Integer, "right"),
        ]);

        let left_id = Column {
            name: "id".to_string(),
            table: Some("left".to_string()),
        };
        let right_id = Column {
            name: "id".to_string(),
            table: Some("right".to_string()),
        };

        let result =
            DbspCompiler::resolve_join_columns(&left_id, &right_id, &left_schema, &right_schema);
        assert!(result.is_ok());
        let (actual_left, left_idx, actual_right, right_idx) = result.unwrap();
        assert_eq!(actual_left.name, "id");
        assert_eq!(left_idx, 0);
        assert_eq!(actual_right.name, "id");
        assert_eq!(right_idx, 0);
    }

    #[test]
    fn test_resolve_join_columns_both_unqualified_same_name() {
        // Both columns unqualified with same name existing in both tables - should succeed
        // (first match wins based on order of checking)
        let left_schema = LogicalSchema::new(vec![make_column_info("id", Type::Integer, "left")]);
        let right_schema = LogicalSchema::new(vec![make_column_info("id", Type::Integer, "right")]);

        let id_col1 = Column {
            name: "id".to_string(),
            table: None,
        };
        let id_col2 = Column {
            name: "id".to_string(),
            table: None,
        };

        let result =
            DbspCompiler::resolve_join_columns(&id_col1, &id_col2, &left_schema, &right_schema);
        // Should succeed - unqualified 'id' matches in both schemas
        assert!(result.is_ok());
    }

    #[test]
    fn test_resolve_join_columns_first_not_found() {
        // First column doesn't exist anywhere
        let left_schema = LogicalSchema::new(vec![make_column_info("id", Type::Integer, "left")]);
        let right_schema =
            LogicalSchema::new(vec![make_column_info("value", Type::Integer, "right")]);

        let missing_col = Column {
            name: "missing".to_string(),
            table: None,
        };
        let value_col = Column {
            name: "value".to_string(),
            table: None,
        };

        let result = DbspCompiler::resolve_join_columns(
            &missing_col,
            &value_col,
            &left_schema,
            &right_schema,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not found in either input"));
    }

    #[test]
    fn test_resolve_join_columns_both_unqualified_different_names() {
        // Both unqualified, each exists in only one table
        let left_schema =
            LogicalSchema::new(vec![make_column_info("left_id", Type::Integer, "left")]);
        let right_schema =
            LogicalSchema::new(vec![make_column_info("right_id", Type::Integer, "right")]);

        let left_col = Column {
            name: "left_id".to_string(),
            table: None,
        };
        let right_col = Column {
            name: "right_id".to_string(),
            table: None,
        };

        let result =
            DbspCompiler::resolve_join_columns(&left_col, &right_col, &left_schema, &right_schema);
        assert!(result.is_ok());
        let (actual_left, left_idx, actual_right, right_idx) = result.unwrap();
        assert_eq!(actual_left.name, "left_id");
        assert_eq!(left_idx, 0);
        assert_eq!(actual_right.name, "right_id");
        assert_eq!(right_idx, 0);
    }

    #[test]
    fn test_simple_cte() {
        // Simple CTE: WITH active_users AS (SELECT * FROM users WHERE age > 18) SELECT name FROM active_users
        let (circuit, _) = compile_sql!(
            "WITH active_users AS (SELECT * FROM users WHERE age > 18) SELECT name FROM active_users"
        );

        // After CTE inlining, this should be equivalent to:
        // SELECT name FROM (SELECT * FROM users WHERE age > 18)
        // Which compiles to: Projection (name) -> Projection (*) -> Filter -> Input
        assert_circuit!(circuit, depth: 4, root: Projection);
        assert_operator!(circuit, 0, Projection { columns: ["name"] });
        assert_operator!(
            circuit,
            1,
            Projection {
                columns: ["id", "name", "age"]
            }
        );
        assert_operator!(circuit, 2, Filter);
        assert_operator!(circuit, 3, Input { name: "users" });
    }

    #[test]
    fn test_cte_with_aggregation() {
        // CTE with aggregation in the main query
        let (mut circuit, pager) = compile_sql!(
            "WITH user_ages AS (SELECT age FROM users) SELECT COUNT(*) FROM user_ages"
        );

        // After CTE inlining: Aggregate -> Projection -> Input
        assert_circuit!(circuit, depth: 3, root: Aggregate);

        // Test execution
        let mut input_delta = Delta::new();
        input_delta.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::Text("Alice".into()),
                Value::from_i64(25),
            ],
        );
        input_delta.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::Text("Bob".into()),
                Value::from_i64(30),
            ],
        );

        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), input_delta);

        let result = test_execute(&mut circuit, inputs, pager).unwrap();
        assert_eq!(result.changes.len(), 1);
        // COUNT(*) should be 2
        assert_eq!(result.changes[0].0.values[0], Value::from_i64(2));
    }

    #[test]
    fn test_recursive_cte_transitive_closure() {
        // Test recursive CTE for transitive closure
        let sql = r#"
            WITH RECURSIVE reachable AS (
                SELECT src, dst FROM edges
                UNION
                SELECT reachable.src, edges.dst FROM reachable INNER JOIN edges ON reachable.dst = edges.src
            )
            SELECT src, dst FROM reachable
        "#;

        let (mut circuit, pager) = compile_sql!(sql);

        // Print the circuit structure for debugging
        eprintln!("Circuit structure:\n{circuit}");

        // Verify the circuit has a Recursive node somewhere
        let has_recursive = circuit
            .nodes
            .values()
            .any(|n| matches!(&n.operator, DbspOperator::Recursive { .. }));
        assert!(
            has_recursive,
            "Expected a Recursive operator in the circuit"
        );

        // Create input delta with edges: 1->2, 2->3, 3->4
        let mut input_delta = Delta::new();
        input_delta.insert(1, vec![Value::from_i64(1), Value::from_i64(2)]);
        input_delta.insert(2, vec![Value::from_i64(2), Value::from_i64(3)]);
        input_delta.insert(3, vec![Value::from_i64(3), Value::from_i64(4)]);

        let mut inputs = HashMap::default();
        inputs.insert("edges".to_string(), input_delta);

        // Execute the circuit
        let result = test_execute(&mut circuit, inputs, pager).unwrap();

        // Debug output
        eprintln!("Result has {} rows:", result.changes.len());
        for (row, weight) in &result.changes {
            eprintln!("  {:?} (weight {})", row.values, weight);
        }

        // Expected: 1->2, 1->3, 1->4, 2->3, 2->4, 3->4 (6 edges including transitive)
        assert!(
            result.changes.len() >= 3,
            "Expected at least base case (3 edges), got {}",
            result.changes.len()
        );
    }

    /// Reproduces the Holon split_block staleness bug: an UPDATE on a base
    /// table that changes only a column NOT used in the CTE's intermediate
    /// projection must still emit both retraction and insertion.
    ///
    /// Uses the `users` table from `test_schema!` (id, name, age).
    /// The CTE projects only `id`, stripping `name` (analogous to
    /// `content` in Holon).  The outer select restores `name` via a
    /// self-join.
    #[test]
    fn test_recursive_cte_update_preserves_retraction_and_insertion() {
        let sql = r#"
            WITH RECURSIVE cte AS (
                SELECT id FROM users
                UNION ALL
                SELECT u.id FROM cte JOIN users u ON u.age = cte.id
            )
            SELECT u.* FROM users u JOIN cte ON cte.id = u.id
        "#;

        let (mut circuit, pager) = compile_sql!(sql);

        // Seed: users table has (id, name, age). Seed with two rows.
        let mut seed = Delta::new();
        seed.insert(
            1,
            vec![
                Value::from_i64(1),
                Value::from_text("old content"),
                Value::from_i64(2),
            ],
        );
        seed.insert(
            2,
            vec![
                Value::from_i64(2),
                Value::from_text("parent row"),
                Value::from_i64(0),
            ],
        );

        let mut inputs = HashMap::default();
        inputs.insert("users".to_string(), seed);
        let _ = test_execute(&mut circuit, inputs, pager.clone()).unwrap();

        // UPDATE: only `name` changes (column not in CTE projection).
        // Old row has name="old content", new row has name="new content".
        let mut update_delta = Delta::new();
        update_delta.delete(
            1,
            vec![
                Value::from_i64(1),
                Value::from_text("old content"),
                Value::from_i64(2),
            ],
        );
        update_delta.insert(
            3,
            vec![
                Value::from_i64(1),
                Value::from_text("new content"),
                Value::from_i64(2),
            ],
        );

        let mut update_inputs = HashMap::default();
        update_inputs.insert("users".to_string(), update_delta);
        let result = test_execute(&mut circuit, update_inputs, pager).unwrap();

        let has_retraction = result.changes.iter().any(|(row, w)| {
            *w == -1 && row.values.get(1).and_then(|v| v.to_text()) == Some("old content")
        });
        let has_insertion = result.changes.iter().any(|(row, w)| {
            *w == 1 && row.values.get(1).and_then(|v| v.to_text()) == Some("new content")
        });

        assert!(
            has_retraction,
            "BUG: UPDATE must emit retraction of old content (name='old content')"
        );
        assert!(
            has_insertion,
            "BUG: UPDATE must emit insertion of new content (name='new content')"
        );
    }
}
