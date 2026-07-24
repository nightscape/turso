use super::compiler::{DbspCircuit, DbspCompiler, DeltaSet};
use super::dbsp::{Delta, RowValues};
use super::operator::ComputationTracker;
use crate::numeric::Numeric;
use crate::schema::{BTreeTable, Column, Schema, Table};
use crate::storage::btree::CursorTrait;
use crate::sync::Arc;
use crate::sync::Mutex;
use crate::translate::logical::LogicalPlanBuilder;
use crate::types::{IOResult, Value};
use crate::util::{extract_view_columns, ViewColumnSchema};
use crate::vtab::VirtualTable;
use crate::{return_if_io, LimboError, Pager, Result, Statement};
use parking_lot::Mutex as ParkingLotMutex;
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::fmt;
use std::sync::Arc as StdArc;
use turso_parser::ast;
use turso_parser::{
    ast::{Cmd, Stmt},
    parser::Parser,
};

/// Parsed ORDER BY clause for a materialized view.
/// Each entry is (output_column_index, sort_order, explicit_nulls_order).
#[derive(Debug, Clone, Default)]
pub struct MatviewOrderBy {
    pub columns: Vec<(usize, ast::SortOrder, Option<ast::NullsOrder>)>,
}

impl MatviewOrderBy {
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Number of key parts for IndexInfo: sort columns + 1 (rowid tiebreaker).
    pub fn num_key_parts(&self) -> usize {
        self.columns.len() + 1
    }

    /// Permute a storage-order data tuple back into logical column order.
    ///
    /// Storage layout (after rowid + weight have been stripped): the first
    /// `N` positions hold the user-specified sort columns in `ORDER BY`
    /// order; the remaining positions hold non-sort columns in
    /// logical-index-ascending order.
    ///
    /// `storage` MUST have length equal to the number of *logical* output
    /// data columns (i.e. `output_schema.columns.len()`).
    pub fn permute_storage_to_logical(&self, storage: &[Value]) -> Vec<Value> {
        let num_data_cols = storage.len();
        let mut logical = vec![Value::Null; num_data_cols];
        let sort_set: HashSet<usize> = self.columns.iter().map(|(i, _, _)| *i).collect();
        // Sort columns: storage[i] → logical[order_by.columns[i].0]
        for (storage_pos, (logical_pos, _, _)) in self.columns.iter().enumerate() {
            logical[*logical_pos] = storage[storage_pos].clone();
        }
        // Non-sort columns: storage positions [N..) hold logical positions in
        // ascending order, skipping the sort-column positions.
        let mut j = self.columns.len();
        for l in 0..num_data_cols {
            if !sort_set.contains(&l) {
                logical[l] = storage[j].clone();
                j += 1;
            }
        }
        logical
    }

    /// Build the synthetic IndexInfo describing the on-disk record layout.
    ///
    /// Layout: `[sort_v_1, ..., sort_v_N, rowid, non_sort_data_cols..., weight]`
    ///
    /// `has_rowid` is `false` because `BTreeCursor::get_index_rowid_from_record`
    /// reads the LAST record value as rowid — that would be the weight column,
    /// not the rowid. Rowid extraction for matview cursors is done manually at
    /// `record.values[num_sort_cols]`.
    pub fn to_index_info(&self) -> crate::types::IndexInfo {
        use crate::translate::collate::CollationSeq;
        use crate::types::{IndexInfo, KeyInfo};
        let mut key_info = Vec::with_capacity(self.columns.len() + 1);
        for (_, sort_order, nulls_order) in &self.columns {
            key_info.push(KeyInfo {
                sort_order: *sort_order,
                collation: CollationSeq::Binary, // COLLATE in ORDER BY rejected at DDL
                nulls_order: *nulls_order,
            });
        }
        // rowid tiebreaker — matches IndexInfo::new_from_index for indexes with rowid
        key_info.push(KeyInfo {
            sort_order: ast::SortOrder::Asc,
            collation: CollationSeq::Binary,
            nulls_order: None,
        });
        IndexInfo {
            key_info,
            has_rowid: false,
            num_cols: self.columns.len() + 1,
            is_unique: true,
        }
    }
}

/// State machine for populating a view from its source table
pub enum PopulateState {
    /// Initial state - need to prepare the query
    Start,
    /// All tables that need to be populated
    ProcessingAllTables {
        queries: Vec<String>,
        current_idx: usize,
    },
    /// Actively processing rows from the query
    ProcessingOneTable {
        queries: Vec<String>,
        current_idx: usize,
        stmt: Box<Statement>,
        rows_processed: usize,
        /// If we're in the middle of processing a row (merge_delta returned I/O)
        pending_row: Option<(i64, Vec<Value>)>, // (rowid, values)
    },
    /// Collecting data from source tables for recursive CTE (must batch all data first)
    CollectingRecursiveCteData {
        /// Names of tables still to be processed
        remaining_tables: Vec<String>,
        /// Current statement if we're in the middle of reading a table
        current_stmt: Option<(String, Box<Statement>)>, // (table_name, stmt)
        /// Accumulated deltas from tables already processed
        accumulated_deltas: DeltaSet,
    },
    /// Executing the recursive circuit after all data has been collected.
    /// On first entry, input_map contains the data to pass to the circuit.
    /// On resume (after I/O yield), input_map is None and the circuit uses its internal state.
    ExecutingRecursiveCircuit {
        /// Input data for the circuit (Some on first entry, None on resume)
        input_map: Option<HashMap<String, Delta>>,
    },
    /// Population complete
    Done,
}

// SAFETY: This needs to be audited for thread safety.
// See: https://github.com/tursodatabase/turso/issues/1552
unsafe impl Send for PopulateState {}
unsafe impl Sync for PopulateState {}
crate::assert::assert_send_sync!(PopulateState);

/// State machine for merge_delta to handle I/O operations
impl fmt::Debug for PopulateState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PopulateState::Start => write!(f, "Start"),
            PopulateState::ProcessingAllTables {
                current_idx,
                queries,
            } => f
                .debug_struct("ProcessingAllTables")
                .field("current_idx", current_idx)
                .field("num_queries", &queries.len())
                .finish(),
            PopulateState::ProcessingOneTable {
                current_idx,
                rows_processed,
                pending_row,
                queries,
                ..
            } => f
                .debug_struct("ProcessingOneTable")
                .field("current_idx", current_idx)
                .field("rows_processed", rows_processed)
                .field("has_pending", &pending_row.is_some())
                .field("total_queries", &queries.len())
                .finish(),
            PopulateState::CollectingRecursiveCteData {
                remaining_tables,
                current_stmt,
                ..
            } => f
                .debug_struct("CollectingRecursiveCteData")
                .field("remaining_tables", &remaining_tables.len())
                .field("has_current_stmt", &current_stmt.is_some())
                .finish(),
            PopulateState::ExecutingRecursiveCircuit { input_map } => f
                .debug_struct("ExecutingRecursiveCircuit")
                .field("has_input", &input_map.is_some())
                .finish(),
            PopulateState::Done => write!(f, "Done"),
        }
    }
}

/// Per-connection transaction state for incremental views.
/// Uses Mutex instead of RefCell for thread safety.
#[derive(Debug, Default)]
pub struct ViewTransactionState {
    // Per-table deltas for uncommitted changes (input to the view)
    // Maps table_name -> Delta for that table
    table_deltas: ParkingLotMutex<HashMap<String, Delta>>,
    // Output delta for the view (the actual changes to the view's result set)
    // Computed when merge_delta is called
    output_delta: ParkingLotMutex<Option<Delta>>,
}

impl Clone for ViewTransactionState {
    fn clone(&self) -> Self {
        Self {
            table_deltas: ParkingLotMutex::new(self.table_deltas.lock().clone()),
            output_delta: ParkingLotMutex::new(self.output_delta.lock().clone()),
        }
    }
}

impl ViewTransactionState {
    /// Create a new transaction state
    pub fn new() -> Self {
        Self {
            table_deltas: ParkingLotMutex::new(HashMap::default()),
            output_delta: ParkingLotMutex::new(None),
        }
    }

    /// Insert a row into the delta for a specific table
    pub fn insert(&self, table_name: &str, key: i64, values: impl Into<RowValues>) {
        let mut deltas = self.table_deltas.lock();
        let delta = deltas.entry(table_name.to_string()).or_default();
        delta.insert(key, values);
    }

    /// Delete a row from the delta for a specific table
    pub fn delete(&self, table_name: &str, key: i64, values: impl Into<RowValues>) {
        let mut deltas = self.table_deltas.lock();
        let delta = deltas.entry(table_name.to_string()).or_default();
        delta.delete(key, values);
    }

    /// Clear all changes in the delta
    pub fn clear(&self) {
        self.table_deltas.lock().clear();
        *self.output_delta.lock() = None;
    }

    /// Get deltas organized by table (input deltas)
    pub fn get_table_deltas(&self) -> HashMap<String, Delta> {
        self.table_deltas.lock().clone()
    }

    /// Set the output delta (the actual changes to the view's result set)
    pub fn set_output_delta(&self, delta: Delta) {
        *self.output_delta.lock() = Some(delta);
    }

    /// Get the output delta (the actual changes to the view's result set)
    pub fn get_output_delta(&self) -> Option<Delta> {
        self.output_delta.lock().clone()
    }

    /// Check if the delta is empty
    pub fn is_empty(&self) -> bool {
        self.table_deltas.lock().values().all(|d| d.is_empty())
    }

    /// Returns how many elements exist in the delta.
    pub fn len(&self) -> usize {
        self.table_deltas.lock().values().map(|d| d.len()).sum()
    }

    /// Capture per-table delta lengths for later statement-level rollback.
    /// Returns a snapshot that records how many `changes` each table delta has
    /// at the moment of the call. Pass to `rollback_to` to discard any deltas
    /// appended after the snapshot was taken.
    pub fn snapshot_lengths(&self) -> HashMap<String, usize> {
        self.table_deltas
            .lock()
            .iter()
            .map(|(table, delta)| (table.clone(), delta.changes.len()))
            .collect()
    }

    /// Truncate each table delta back to the lengths recorded in `snapshot`.
    /// Tables absent from the snapshot are dropped entirely (they were created
    /// after the snapshot). The output delta is cleared since it is rebuilt
    /// from input deltas at commit time.
    pub fn rollback_to(&self, snapshot: &HashMap<String, usize>) {
        let mut deltas = self.table_deltas.lock();
        deltas.retain(|table, delta| match snapshot.get(table) {
            Some(&len) => {
                delta.changes.truncate(len);
                !delta.changes.is_empty() || len > 0
            }
            None => false,
        });
        *self.output_delta.lock() = None;
    }
}

/// Container for all view transaction states within a connection.
/// Thread-safe implementation using parking_lot::Mutex.
#[derive(Debug, Default)]
pub struct AllViewsTxState {
    states: StdArc<ParkingLotMutex<HashMap<String, StdArc<ViewTransactionState>>>>,
}

impl Clone for AllViewsTxState {
    fn clone(&self) -> Self {
        Self {
            states: self.states.clone(),
        }
    }
}

impl AllViewsTxState {
    /// Create a new container for view transaction states
    pub fn new() -> Self {
        Self {
            states: StdArc::new(ParkingLotMutex::new(HashMap::default())),
        }
    }

    /// Get or create a transaction state for a view
    pub fn get_or_create(&self, view_name: &str) -> StdArc<ViewTransactionState> {
        let mut states = self.states.lock();
        states
            .entry(view_name.to_string())
            .or_insert_with(|| StdArc::new(ViewTransactionState::new()))
            .clone()
    }

    /// Get a transaction state for a view if it exists
    pub fn get(&self, view_name: &str) -> Option<StdArc<ViewTransactionState>> {
        self.states.lock().get(view_name).cloned()
    }

    /// Clear all transaction states
    pub fn clear(&self) {
        self.states.lock().clear();
    }

    /// Check if there are no transaction states
    pub fn is_empty(&self) -> bool {
        self.states.lock().is_empty()
    }

    /// Get all view names that have transaction states
    pub fn get_view_names(&self) -> Vec<String> {
        self.states.lock().keys().cloned().collect()
    }

    /// Snapshot the current per-view, per-table delta lengths so that a
    /// failed statement can roll back its partial deltas via `rollback_to`.
    pub fn snapshot_lengths(&self) -> HashMap<String, HashMap<String, usize>> {
        self.states
            .lock()
            .iter()
            .map(|(view, state)| (view.clone(), state.snapshot_lengths()))
            .collect()
    }

    /// Restore each view's deltas to the lengths captured in `snapshot`.
    /// Views created after the snapshot are removed entirely.
    pub fn rollback_to(&self, snapshot: &HashMap<String, HashMap<String, usize>>) {
        let mut states = self.states.lock();
        states.retain(|view, state| match snapshot.get(view) {
            Some(view_snapshot) => {
                state.rollback_to(view_snapshot);
                true
            }
            None => false,
        });
    }
}

/// Lightweight representation of a table referenced by a materialized view.
/// Works for both BTree tables and virtual/foreign tables.
#[derive(Debug, Clone)]
pub struct ReferencedTable {
    pub name: String,
    pub columns: Vec<Column>,
    pub has_rowid: bool,
    rowid_alias_index: Option<usize>,
    /// Virtual/foreign tables don't expose `rowid` in SQL — need synthetic rowids.
    is_virtual: bool,
}

impl ReferencedTable {
    /// Build from any table type in the schema.
    /// Returns None for FromClauseSubquery (not a real table).
    pub fn from_schema(schema: &Schema, table_name: &str) -> Option<Self> {
        match schema.get_table(table_name)?.as_ref() {
            Table::BTree(btree) => Some(Self::from_btree(btree)),
            Table::Virtual(vtab) => Some(Self::from_virtual(vtab)),
            Table::FromClauseSubquery(_) | Table::RecursiveCte(_) => None,
        }
    }

    pub fn from_btree(table: &BTreeTable) -> Self {
        let rowid_alias_index = table.get_rowid_alias_column().map(|(idx, _)| idx);
        Self {
            name: table.name.clone(),
            columns: table.columns().to_vec(),
            has_rowid: table.has_rowid,
            rowid_alias_index,
            is_virtual: false,
        }
    }

    pub fn from_virtual(table: &VirtualTable) -> Self {
        Self {
            name: table.name.clone(),
            columns: table.columns.clone(),
            has_rowid: true,
            rowid_alias_index: None,
            is_virtual: true,
        }
    }

    pub fn get_rowid_alias_column(&self) -> Option<usize> {
        self.rowid_alias_index
    }
}

/// Incremental view that maintains its state through a DBSP circuit
///
/// This version keeps everything in-memory. This is acceptable for small views, since DBSP
/// doesn't have to track the history of changes. Still for very large views (think of the result
/// of create view v as select * from tbl where x > 1; and that having 1B values.
///
/// We should have a version of this that materializes the results. Materializing will also be good
/// for large aggregations, because then we don't have to re-compute when opening the database
/// again.
///
/// Uses DBSP circuits for incremental computation.
#[derive(Debug)]
pub struct IncrementalView {
    name: String,
    // The SELECT statement that defines how to transform input data
    pub select_stmt: ast::Select,

    // DBSP circuit that encapsulates the computation
    circuit: DbspCircuit,

    // All tables referenced by this view (from FROM clause and JOINs)
    referenced_tables: Vec<ReferencedTable>,
    // Mapping from table aliases to actual table names (e.g., "c" -> "customers")
    table_aliases: HashMap<String, String>,
    // Mapping from table name to fully qualified name (e.g., "customers" -> "main.customers")
    // This preserves database qualification from the original query
    qualified_table_names: HashMap<String, String>,
    // WHERE conditions for each table (accumulated from all occurrences)
    // Multiple conditions from UNION branches or duplicate references are stored as a vector
    table_conditions: HashMap<String, Vec<Option<ast::Expr>>>,
    // The view's column schema with table relationships
    pub column_schema: ViewColumnSchema,
    // State machine for population
    populate_state: PopulateState,
    // Computation tracker for statistics
    // We will use this one day to export rows_read, but for now, will just test that we're doing the expected amount of compute
    #[cfg_attr(not(test), allow(dead_code))]
    pub tracker: Arc<Mutex<ComputationTracker>>,
    // Root page of the btree storing the materialized state (0 for unmaterialized)
    root_page: i64,
    // Whether this view contains a WITH RECURSIVE clause
    has_recursive_cte: bool,
    // ORDER BY columns (empty vec if no ORDER BY)
    pub order_by: MatviewOrderBy,
    // LIMIT clause (None if no LIMIT)
    pub limit: Option<i64>,
    // Whether to store data in an index btree (true when ORDER BY is present)
    pub has_order_by: bool,
}

// SAFETY: This needs to be audited for thread safety.
// See: https://github.com/tursodatabase/turso/issues/1552
unsafe impl Send for IncrementalView {}
unsafe impl Sync for IncrementalView {}
crate::assert::assert_send_sync!(IncrementalView);

impl IncrementalView {
    /// Try to compile the SELECT statement into a DBSP circuit
    fn try_compile_circuit(
        select: &ast::Select,
        schema: &Schema,
        main_data_root: i64,
        internal_state_root: i64,
        internal_state_index_root: i64,
        order_by: &MatviewOrderBy,
        limit: Option<i64>,
    ) -> Result<DbspCircuit> {
        // Build the logical plan from the SELECT statement.
        //
        // We strip ORDER BY and LIMIT from the SELECT before logical-plan
        // building because both are enforced by the storage layer (the
        // matview's index btree gives sort order; the cursor enforces
        // LIMIT). The general logical-plan `build_limit` rejects negative
        // LIMITs, so passing through the user's `LIMIT -1` (= unlimited)
        // would fail there. The original `select_stmt` keeps ORDER BY/LIMIT
        // for SQL round-trips (recursive-CTE full-result mode etc.).
        let mut select_for_plan = select.clone();
        select_for_plan.order_by.clear();
        select_for_plan.limit = None;
        let mut builder = LogicalPlanBuilder::new(schema);
        let stmt = ast::Stmt::Select(select_for_plan);
        let logical_plan = builder.build_statement(&stmt)?;

        // Compile the logical plan to a DBSP circuit with the storage roots
        let compiler = DbspCompiler::new(
            main_data_root,
            internal_state_root,
            internal_state_index_root,
            order_by.clone(),
            limit,
        );
        let circuit = compiler.compile(&logical_plan)?;

        Ok(circuit)
    }

    /// Get an iterator over column names, using enumerated naming for unnamed columns
    pub fn column_names(&self) -> impl Iterator<Item = String> + '_ {
        self.column_schema
            .columns
            .iter()
            .enumerate()
            .map(|(i, vc)| {
                vc.column
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("column{}", i + 1))
            })
    }

    /// Check if this view has the same SQL definition as the provided SQL string
    pub fn has_same_sql(&self, sql: &str) -> bool {
        // Parse the SQL to extract just the SELECT statement
        if let Ok(Some(Cmd::Stmt(Stmt::CreateMaterializedView { select, .. }))) =
            Parser::new(sql.as_bytes()).next_cmd()
        {
            // Compare the SELECT statements as SQL strings
            return self.select_stmt == select;
        }
        false
    }

    /// Validate a SELECT statement and extract the columns it would produce
    /// This is used during CREATE MATERIALIZED VIEW to validate the view before storing it
    pub fn validate_and_extract_columns(
        select: &ast::Select,
        schema: &Schema,
    ) -> Result<ViewColumnSchema> {
        crate::util::validate_select_for_unsupported_features(select)?;
        // Use the shared function to extract columns with full table context
        extract_view_columns(select, schema)
    }

    pub fn from_sql(
        sql: &str,
        schema: &Schema,
        main_data_root: i64,
        internal_state_root: i64,
        internal_state_index_root: i64,
    ) -> Result<Self> {
        let mut parser = Parser::new(sql.as_bytes());
        let cmd = parser.next_cmd()?;
        let cmd = cmd.expect("View is an empty statement");
        match cmd {
            Cmd::Stmt(Stmt::CreateMaterializedView {
                if_not_exists: _,
                view_name,
                columns: _,
                select,
            }) => IncrementalView::from_stmt(
                view_name,
                select,
                schema,
                main_data_root,
                internal_state_root,
                internal_state_index_root,
            ),
            _ => Err(LimboError::ParseError(format!(
                "View is not a CREATE MATERIALIZED VIEW statement: {sql}"
            ))),
        }
    }

    pub fn from_stmt(
        view_name: ast::QualifiedName,
        select: ast::Select,
        schema: &Schema,
        main_data_root: i64,
        internal_state_root: i64,
        internal_state_index_root: i64,
    ) -> Result<Self> {
        let name = view_name.name.as_str().to_string();

        // Extract output columns using the shared function
        let column_schema = extract_view_columns(&select, schema)?;

        // Parse ORDER BY and LIMIT from the SELECT
        let (order_by, limit) = Self::parse_order_by_and_limit(&select, &column_schema)?;

        // Check for matview-on-matview upstream LIMIT restriction
        // (will be validated after we check referenced tables below)

        let mut referenced_tables = Vec::new();
        let mut table_aliases = HashMap::default();
        let mut qualified_table_names = HashMap::default();
        let mut table_conditions = HashMap::default();
        Self::extract_all_tables(
            &select,
            schema,
            &mut referenced_tables,
            &mut table_aliases,
            &mut qualified_table_names,
            &mut table_conditions,
        )?;

        // Matview-on-matview restriction: an upstream matview that uses
        // ORDER BY or LIMIT is stored in a different btree shape (index vs
        // table) and may have its row count truncated by LIMIT. Until we
        // make the downstream IVM input adapter cursor-aware (so it sees
        // the LIMIT-applied, sort-ordered rows the same way the user does),
        // refuse the chain at DDL time. ORDER BY is also restricted because
        // raw btree iteration over an upstream index page would crash a
        // table cursor.
        for ref_table in &referenced_tables {
            if let Some(upstream_arc) = schema.get_materialized_view(&ref_table.name) {
                let upstream = upstream_arc.lock();
                if !upstream.order_by.is_empty() || upstream.limit.is_some() {
                    return Err(LimboError::ParseError(format!(
                        "matview-on-matview is not yet supported when the upstream view uses ORDER BY or LIMIT (view '{}')",
                        ref_table.name
                    )));
                }
            }
        }

        Self::new_with_order_by(
            name,
            select.clone(),
            referenced_tables,
            table_aliases,
            qualified_table_names,
            table_conditions,
            column_schema,
            schema,
            main_data_root,
            internal_state_root,
            internal_state_index_root,
            order_by,
            limit,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        select_stmt: ast::Select,
        referenced_tables: Vec<ReferencedTable>,
        table_aliases: HashMap<String, String>,
        qualified_table_names: HashMap<String, String>,
        table_conditions: HashMap<String, Vec<Option<ast::Expr>>>,
        column_schema: ViewColumnSchema,
        schema: &Schema,
        main_data_root: i64,
        internal_state_root: i64,
        internal_state_index_root: i64,
    ) -> Result<Self> {
        Self::new_with_order_by(
            name,
            select_stmt,
            referenced_tables,
            table_aliases,
            qualified_table_names,
            table_conditions,
            column_schema,
            schema,
            main_data_root,
            internal_state_root,
            internal_state_index_root,
            MatviewOrderBy::default(),
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_with_order_by(
        name: String,
        select_stmt: ast::Select,
        referenced_tables: Vec<ReferencedTable>,
        table_aliases: HashMap<String, String>,
        qualified_table_names: HashMap<String, String>,
        table_conditions: HashMap<String, Vec<Option<ast::Expr>>>,
        column_schema: ViewColumnSchema,
        schema: &Schema,
        main_data_root: i64,
        internal_state_root: i64,
        internal_state_index_root: i64,
        order_by: MatviewOrderBy,
        limit: Option<i64>,
    ) -> Result<Self> {
        // Create the tracker that will be shared by all operators
        let tracker = Arc::new(Mutex::new(ComputationTracker::new()));

        // Check if the SELECT statement has a WITH RECURSIVE clause
        let has_recursive_cte = select_stmt.with.as_ref().is_some_and(|w| w.recursive);

        let has_order_by = !order_by.is_empty();

        // Compile the SELECT statement into a DBSP circuit
        let circuit = Self::try_compile_circuit(
            &select_stmt,
            schema,
            main_data_root,
            internal_state_root,
            internal_state_index_root,
            &order_by,
            limit,
        )?;

        Ok(Self {
            name,
            select_stmt,
            circuit,
            referenced_tables,
            table_aliases,
            qualified_table_names,
            table_conditions,
            column_schema,
            populate_state: PopulateState::Start,
            tracker,
            root_page: main_data_root,
            has_recursive_cte,
            order_by,
            limit,
            has_order_by,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Parse ORDER BY and LIMIT from the SELECT statement.
    /// Returns (order_by, limit) where order_by columns are indices into the
    /// expanded column_schema.
    fn parse_order_by_and_limit(
        select: &ast::Select,
        column_schema: &ViewColumnSchema,
    ) -> Result<(MatviewOrderBy, Option<i64>)> {
        let num_output_cols = column_schema.columns.len();

        // Validate ORDER BY columns
        let mut order_columns = Vec::new();
        for sc in &select.order_by {
            let col_idx = resolve_sorted_column_to_index(sc, num_output_cols).map_err(|msg| {
                LimboError::ParseError(format!("ORDER BY in materialized view: {msg}"))
            })?;
            let sort_order = sc.order.unwrap_or(ast::SortOrder::Asc);
            order_columns.push((col_idx, sort_order, sc.nulls));
        }

        // Validate LIMIT
        let limit = if let Some(ref limit_clause) = select.limit {
            // Reject OFFSET
            if limit_clause.offset.is_some() {
                return Err(LimboError::ParseError(
                    "LIMIT ... OFFSET is not yet supported in materialized views".to_string(),
                ));
            }
            let limit_val = extract_integer_literal(&limit_clause.expr).ok_or_else(|| {
                LimboError::ParseError(
                    "LIMIT must be a constant integer in materialized views".to_string(),
                )
            })?;
            // SQLite-compat: `LIMIT -1` means "unlimited" (legacy sentinel).
            // Other negatives are rejected with a message that names the rule.
            if limit_val == -1 {
                None
            } else if limit_val < 0 {
                return Err(LimboError::ParseError(
                    "LIMIT must be non-negative or -1 (unlimited) in materialized views"
                        .to_string(),
                ));
            } else {
                Some(limit_val)
            }
        } else {
            None
        };

        // Reject LIMIT without ORDER BY
        if limit.is_some() && order_columns.is_empty() {
            return Err(LimboError::ParseError(
                "LIMIT without ORDER BY is not supported in materialized views".to_string(),
            ));
        }

        Ok((
            MatviewOrderBy {
                columns: order_columns,
            },
            limit,
        ))
    }

    /// Reset the populate state so the view can be repopulated (used by REFRESH).
    pub fn populate_state_is_done(&self) -> bool {
        matches!(self.populate_state, PopulateState::Done)
    }

    pub fn reset_populate_state(&mut self) {
        self.populate_state = PopulateState::Start;
    }

    /// Reset DBSP circuit state after a ROLLBACK so that recursive operator
    /// state (seen_rows, union_all_rowids, etc.) is rebuilt from the btree on
    /// the next execution.
    pub fn reset_circuit_for_rollback(&mut self) {
        self.circuit.reset_recursive_operators_for_rollback();
    }

    /// Execute the circuit with uncommitted changes to get processed delta
    /// Returns (delta, is_full_result). When is_full_result is true, the delta
    /// contains the COMPLETE matview output (not an incremental change), and the
    /// cursor should use it directly instead of merging with the btree.
    pub fn execute_with_uncommitted(
        &mut self,
        uncommitted: DeltaSet,
        pager: Arc<Pager>,
        execute_state: &mut crate::incremental::compiler::ExecuteState,
        conn: &crate::sync::Arc<crate::Connection>,
    ) -> crate::Result<crate::types::IOResult<(Delta, bool)>> {
        if self.has_recursive_cte {
            // For recursive CTE matviews, the DBSP incremental circuit can't correctly
            // compute uncommitted deltas because:
            // - The recursive output includes computed columns (paths) whose values
            //   differ between commit-time and read-time computations
            // - seen_counts/seen_rows use value hashes that don't match across contexts
            // - This causes either missing rows or massive over-counting
            //
            // Instead, evaluate the matview SQL directly on the parent connection
            // (which sees committed + uncommitted data). Return the full result.
            let sql = self.select_stmt.to_string();
            let mut stmt = conn.prepare(&sql)?;
            let mut delta = super::dbsp::Delta::new();
            let mut rowid = 1i64;
            loop {
                match stmt.step()? {
                    crate::vdbe::StepResult::Row => {
                        if let Some(row) = stmt.row() {
                            let values: Vec<crate::types::Value> =
                                row.get_values().cloned().collect();
                            delta.insert(rowid, values);
                            rowid += 1;
                        }
                    }
                    crate::vdbe::StepResult::Done => break,
                    crate::vdbe::StepResult::IO => {
                        let completion = crate::io::Completion::new_yield();
                        return Ok(crate::types::IOResult::IO(
                            crate::types::IOCompletions::Single(completion),
                        ));
                    }
                    _ => break,
                }
            }
            return Ok(crate::types::IOResult::Done((delta, true)));
        }

        // Non-recursive matviews: use DBSP incremental delta computation
        *execute_state = crate::incremental::compiler::ExecuteState::Init {
            input_data: crate::incremental::compiler::InputDeltas::from_delta_set(uncommitted),
        };
        let result = self.circuit.execute(pager, execute_state)?;
        match result {
            crate::types::IOResult::Done(delta) => Ok(crate::types::IOResult::Done((delta, false))),
            crate::types::IOResult::IO(io) => Ok(crate::types::IOResult::IO(io)),
        }
    }

    /// Get the root page for this materialized view's btree
    pub fn get_root_page(&self) -> i64 {
        self.root_page
    }

    /// Get all table names referenced by this view
    pub fn get_referenced_table_names(&self) -> Vec<String> {
        self.referenced_tables
            .iter()
            .map(|t| t.name.clone())
            .collect()
    }

    /// Get all tables referenced by this view
    pub fn get_referenced_tables(&self) -> &[ReferencedTable] {
        &self.referenced_tables
    }

    /// Process a single table reference from a FROM or JOIN clause
    fn process_table_reference(
        name: &ast::QualifiedName,
        alias: &Option<ast::As>,
        schema: &Schema,
        table_map: &mut HashMap<String, ReferencedTable>,
        aliases: &mut HashMap<String, String>,
        qualified_names: &mut HashMap<String, String>,
        cte_names: &HashSet<String>,
    ) -> Result<()> {
        let table_name = name.name.as_str();

        // Build the fully qualified name
        let qualified_name = if let Some(ref db) = name.db_name {
            format!("{db}.{table_name}")
        } else {
            table_name.to_string()
        };

        // Skip CTEs - they're not real tables
        if !cte_names.contains(table_name) {
            let ref_table = ReferencedTable::from_schema(schema, table_name).ok_or_else(|| {
                LimboError::ParseError(format!("Table '{table_name}' not found in schema"))
            })?;
            table_map.insert(table_name.to_string(), ref_table);
            qualified_names.insert(table_name.to_string(), qualified_name);

            // Store the alias mapping if there is an alias
            if let Some(alias_enum) = alias {
                aliases.insert(
                    alias_enum.name().as_str().to_string(),
                    table_name.to_string(),
                );
            }
        }
        Ok(())
    }

    fn extract_one_statement(
        select: &ast::OneSelect,
        schema: &Schema,
        table_map: &mut HashMap<String, ReferencedTable>,
        aliases: &mut HashMap<String, String>,
        qualified_names: &mut HashMap<String, String>,
        table_conditions: &mut HashMap<String, Vec<Option<ast::Expr>>>,
        cte_names: &HashSet<String>,
    ) -> Result<()> {
        if let ast::OneSelect::Select {
            from: Some(ref from),
            ..
        } = select
        {
            // Get the main table from FROM clause
            if let ast::SelectTable::Table(name, alias, _) = from.select.as_ref() {
                Self::process_table_reference(
                    name,
                    alias,
                    schema,
                    table_map,
                    aliases,
                    qualified_names,
                    cte_names,
                )?;
            }

            // Get all tables from JOIN clauses
            for join in &from.joins {
                if let ast::SelectTable::Table(name, alias, _) = join.table.as_ref() {
                    Self::process_table_reference(
                        name,
                        alias,
                        schema,
                        table_map,
                        aliases,
                        qualified_names,
                        cte_names,
                    )?;
                }
            }
        }
        // Detect self-joined tables (same table under multiple aliases)
        let mut alias_count: HashMap<String, usize> = HashMap::default();
        for (_alias, table_name) in aliases.iter() {
            *alias_count.entry(table_name.clone()).or_insert(0) += 1;
        }
        let self_joined_tables: HashSet<String> = alias_count
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|(name, _)| name)
            .collect();

        // Extract WHERE conditions for this SELECT
        let where_expr = if let ast::OneSelect::Select {
            where_clause: Some(ref where_expr),
            ..
        } = select
        {
            Some(where_expr.as_ref().clone())
        } else {
            None
        };

        // Ensure all tables have an entry in table_conditions (even if empty)
        for table_name in table_map.keys() {
            table_conditions.entry(table_name.clone()).or_default();
        }

        // Extract and store table-specific conditions from the WHERE clause
        if let Some(ref where_expr) = where_expr {
            for table_name in table_map.keys() {
                let conditions = table_conditions.get_mut(table_name).ok_or_else(|| {
                    LimboError::InternalError(
                        "table_conditions should have entry for table_name".to_string(),
                    )
                })?;
                if self_joined_tables.contains(table_name) {
                    // Self-joined table: conditions reference different aliases of the same
                    // table. We must fetch all rows; the circuit handles filtering post-join.
                    conditions.push(None);
                    continue;
                }
                let all_tables: Vec<String> = table_map.keys().cloned().collect();
                let table_specific_condition = Self::extract_conditions_for_table(
                    where_expr,
                    table_name,
                    aliases,
                    &all_tables,
                    schema,
                );
                if let Some(condition) = table_specific_condition {
                    conditions.push(Some(condition));
                }
            }
        } else {
            // No WHERE clause - push None for all tables in this SELECT. It is a way
            // of signaling that we need all rows in the table. It is important we signal this
            // explicitly, because the same table may appear in many conditions - some of which
            // have filters that would otherwise be applied.
            for table_name in table_map.keys() {
                let conditions = table_conditions.get_mut(table_name).ok_or_else(|| {
                    LimboError::InternalError(
                        "table_conditions should have entry for table_name".to_string(),
                    )
                })?;
                conditions.push(None);
            }
        }

        Ok(())
    }

    /// Extract all tables and their aliases from the SELECT statement, handling CTEs
    /// Deduplicates tables and accumulates WHERE conditions
    fn extract_all_tables(
        select: &ast::Select,
        schema: &Schema,
        tables: &mut Vec<ReferencedTable>,
        aliases: &mut HashMap<String, String>,
        qualified_names: &mut HashMap<String, String>,
        table_conditions: &mut HashMap<String, Vec<Option<ast::Expr>>>,
    ) -> Result<()> {
        let mut table_map = HashMap::default();
        Self::extract_all_tables_inner(
            select,
            schema,
            &mut table_map,
            aliases,
            qualified_names,
            table_conditions,
            &HashSet::default(),
        )?;

        // Convert deduplicated table map to vector
        for (_name, table) in table_map {
            tables.push(table);
        }

        Ok(())
    }

    fn extract_all_tables_inner(
        select: &ast::Select,
        schema: &Schema,
        table_map: &mut HashMap<String, ReferencedTable>,
        aliases: &mut HashMap<String, String>,
        qualified_names: &mut HashMap<String, String>,
        table_conditions: &mut HashMap<String, Vec<Option<ast::Expr>>>,
        parent_cte_names: &HashSet<String>,
    ) -> Result<()> {
        let mut cte_names = parent_cte_names.clone();

        // First, collect CTE names and process any CTEs (WITH clauses)
        if let Some(ref with) = select.with {
            // First pass: collect all CTE names (needed for recursive CTEs)
            for cte in &with.ctes {
                cte_names.insert(cte.tbl_name.as_str().to_string());
            }

            // Second pass: extract tables from each CTE's SELECT statement
            for cte in &with.ctes {
                // Recursively extract tables from each CTE's SELECT statement
                Self::extract_all_tables_inner(
                    &cte.select,
                    schema,
                    table_map,
                    aliases,
                    qualified_names,
                    table_conditions,
                    &cte_names,
                )?;
            }
        }

        // Then process the main SELECT body
        Self::extract_one_statement(
            &select.body.select,
            schema,
            table_map,
            aliases,
            qualified_names,
            table_conditions,
            &cte_names,
        )?;

        // Process any compound selects (UNION, etc.)
        for c in &select.body.compounds {
            let ast::CompoundSelect { select, .. } = c;
            Self::extract_one_statement(
                select,
                schema,
                table_map,
                aliases,
                qualified_names,
                table_conditions,
                &cte_names,
            )?;
        }

        Ok(())
    }

    /// Generate SQL queries for populating the view from each source table
    /// Returns a vector of SQL statements, one for each referenced table
    /// Each query includes the WHERE conditions accumulated from all occurrences
    fn sql_for_populate(&self) -> crate::Result<Vec<String>> {
        Self::generate_populate_queries(
            &self.select_stmt,
            &self.referenced_tables,
            &self.table_aliases,
            &self.qualified_table_names,
            &self.table_conditions,
        )
    }

    pub fn generate_populate_queries(
        select_stmt: &ast::Select,
        referenced_tables: &[ReferencedTable],
        table_aliases: &HashMap<String, String>,
        qualified_table_names: &HashMap<String, String>,
        table_conditions: &HashMap<String, Vec<Option<ast::Expr>>>,
    ) -> crate::Result<Vec<String>> {
        if referenced_tables.is_empty() {
            return Err(LimboError::ParseError(
                "No tables to populate from".to_string(),
            ));
        }

        let mut queries = Vec::new();

        for table in referenced_tables {
            // Virtual tables (including foreign tables) don't expose `rowid` in SQL.
            // BTree tables with a rowid alias include it in `*`.
            // Only bare BTree tables without a rowid alias need explicit `, rowid`.
            let select_clause = if table.is_virtual || table.get_rowid_alias_column().is_some() {
                "*".to_string()
            } else {
                "*, rowid".to_string()
            };

            // Get accumulated WHERE conditions for this table
            let where_clause = if let Some(conditions) = table_conditions.get(&table.name) {
                // Combine multiple conditions with OR if there are multiple occurrences
                Self::combine_conditions(
                    select_stmt,
                    conditions,
                    &table.name,
                    referenced_tables,
                    table_aliases,
                )?
            } else {
                String::new()
            };

            // Use the qualified table name if available, otherwise just the table name
            let table_name = qualified_table_names
                .get(&table.name)
                .cloned()
                .unwrap_or_else(|| table.name.clone());

            // Construct the query for this table
            let query = if where_clause.is_empty() {
                format!("SELECT {select_clause} FROM {table_name}")
            } else {
                format!("SELECT {select_clause} FROM {table_name} WHERE {where_clause}")
            };
            tracing::debug!("populating materialized view with `{query}`");
            queries.push(query);
        }

        Ok(queries)
    }

    fn combine_conditions(
        _select_stmt: &ast::Select,
        conditions: &[Option<ast::Expr>],
        table_name: &str,
        _referenced_tables: &[ReferencedTable],
        table_aliases: &HashMap<String, String>,
    ) -> crate::Result<String> {
        // Check if any conditions are None (SELECTs without WHERE)
        let has_none = conditions.iter().any(|c| c.is_none());
        let non_empty: Vec<_> = conditions.iter().filter_map(|c| c.as_ref()).collect();

        // If we have both Some and None conditions, that means in some of the expressions where
        // this table appear we want all rows. So we need to fetch all rows.
        if has_none && !non_empty.is_empty() {
            return Ok(String::new());
        }

        if non_empty.is_empty() {
            return Ok(String::new());
        }

        if non_empty.len() == 1 {
            // Unqualify the expression before converting to string
            let unqualified = Self::unqualify_expression(non_empty[0], table_name, table_aliases);
            return Ok(unqualified.to_string());
        }

        // Multiple conditions - combine with OR
        // This happens in UNION ALL when the same table appears multiple times
        let mut combined_parts = Vec::new();
        for condition in non_empty {
            let unqualified = Self::unqualify_expression(condition, table_name, table_aliases);
            // Wrap each condition in parentheses to preserve precedence
            combined_parts.push(format!("({unqualified})"));
        }

        // Join all conditions with OR
        Ok(combined_parts.join(" OR "))
    }
    /// Resolve a table alias to the actual table name
    /// Check if an expression is a simple comparison that can be safely extracted
    /// This excludes subqueries, CASE expressions, function calls, etc.
    fn is_simple_comparison(expr: &ast::Expr) -> bool {
        match expr {
            // Simple column references and literals are OK
            ast::Expr::Column { .. } | ast::Expr::Literal(_) => true,

            // Simple binary operations between simple expressions are OK
            ast::Expr::Binary(left, op, right) => {
                match op {
                    // Logical operators
                    ast::Operator::And | ast::Operator::Or => {
                        Self::is_simple_comparison(left) && Self::is_simple_comparison(right)
                    }
                    // Comparison operators
                    ast::Operator::Equals
                    | ast::Operator::NotEquals
                    | ast::Operator::Less
                    | ast::Operator::LessEquals
                    | ast::Operator::Greater
                    | ast::Operator::GreaterEquals
                    | ast::Operator::Is
                    | ast::Operator::IsNot => {
                        Self::is_simple_comparison(left) && Self::is_simple_comparison(right)
                    }
                    // String concatenation and other operations are NOT simple
                    ast::Operator::Concat => false,
                    // Arithmetic might be OK if operands are simple
                    ast::Operator::Add
                    | ast::Operator::Subtract
                    | ast::Operator::Multiply
                    | ast::Operator::Divide
                    | ast::Operator::Modulus => {
                        Self::is_simple_comparison(left) && Self::is_simple_comparison(right)
                    }
                    _ => false,
                }
            }

            // Unary operations might be OK
            ast::Expr::Unary(
                ast::UnaryOperator::Not
                | ast::UnaryOperator::Negative
                | ast::UnaryOperator::Positive,
                inner,
            ) => Self::is_simple_comparison(inner),
            ast::Expr::Unary(_, _) => false,

            // Complex expressions are NOT simple
            ast::Expr::Case { .. } => false,
            ast::Expr::Cast { .. } => false,
            ast::Expr::Collate { .. } => false,
            ast::Expr::Exists(_) => false,
            ast::Expr::FunctionCall { .. } => false,
            ast::Expr::InList { .. } => false,
            ast::Expr::InSelect { .. } => false,
            ast::Expr::Like { .. } => false,
            ast::Expr::NotNull(_) => true, // IS NOT NULL is simple enough
            ast::Expr::Parenthesized(exprs) => {
                // Parenthesized expression can contain multiple expressions
                // Only consider it simple if it has exactly one simple expression
                exprs.len() == 1 && Self::is_simple_comparison(&exprs[0])
            }
            ast::Expr::Subquery(_) => false,

            // BETWEEN might be OK if all operands are simple
            ast::Expr::Between { .. } => {
                // BETWEEN has a different structure, for safety just exclude it
                false
            }

            // Qualified references are simple
            ast::Expr::DoublyQualified(..) => true,
            ast::Expr::Qualified(_, _) => true,

            // These are simple
            ast::Expr::Id(_) => true,
            ast::Expr::Name(_) => true,

            // Anything else is not simple
            _ => false,
        }
    }

    /// Extract conditions from a WHERE clause that apply to a specific table
    fn extract_conditions_for_table(
        expr: &ast::Expr,
        table_name: &str,
        aliases: &HashMap<String, String>,
        all_tables: &[String],
        schema: &Schema,
    ) -> Option<ast::Expr> {
        match expr {
            ast::Expr::Binary(left, op, right) => {
                match op {
                    ast::Operator::And => {
                        // For AND, we can extract conditions independently
                        let left_cond = Self::extract_conditions_for_table(
                            left, table_name, aliases, all_tables, schema,
                        );
                        let right_cond = Self::extract_conditions_for_table(
                            right, table_name, aliases, all_tables, schema,
                        );

                        match (left_cond, right_cond) {
                            (Some(l), Some(r)) => Some(ast::Expr::Binary(
                                Box::new(l),
                                ast::Operator::And,
                                Box::new(r),
                            )),
                            (Some(l), None) => Some(l),
                            (None, Some(r)) => Some(r),
                            (None, None) => None,
                        }
                    }
                    ast::Operator::Or => {
                        // For OR, both sides must reference only our table
                        let left_tables =
                            Self::get_tables_in_expr(left, aliases, all_tables, schema);
                        let right_tables =
                            Self::get_tables_in_expr(right, aliases, all_tables, schema);

                        if left_tables.len() == 1
                            && left_tables.contains(&table_name.to_string())
                            && right_tables.len() == 1
                            && right_tables.contains(&table_name.to_string())
                            && Self::is_simple_comparison(expr)
                        {
                            Some(expr.clone())
                        } else {
                            None
                        }
                    }
                    _ => {
                        // For comparison operators, check if this condition only references our table
                        let referenced_tables =
                            Self::get_tables_in_expr(expr, aliases, all_tables, schema);
                        if referenced_tables.len() == 1
                            && referenced_tables.contains(&table_name.to_string())
                            && Self::is_simple_comparison(expr)
                        {
                            Some(expr.clone())
                        } else {
                            None
                        }
                    }
                }
            }
            _ => {
                // For other expressions, check if they only reference our table
                let referenced_tables = Self::get_tables_in_expr(expr, aliases, all_tables, schema);
                if referenced_tables.len() == 1
                    && referenced_tables.contains(&table_name.to_string())
                    && Self::is_simple_comparison(expr)
                {
                    Some(expr.clone())
                } else {
                    None
                }
            }
        }
    }

    /// Unqualify column references in an expression
    /// Removes table/alias prefixes from qualified column names
    fn unqualify_expression(
        expr: &ast::Expr,
        table_name: &str,
        aliases: &HashMap<String, String>,
    ) -> ast::Expr {
        match expr {
            ast::Expr::Binary(left, op, right) => ast::Expr::Binary(
                Box::new(Self::unqualify_expression(left, table_name, aliases)),
                *op,
                Box::new(Self::unqualify_expression(right, table_name, aliases)),
            ),
            ast::Expr::Qualified(table_or_alias, column) => {
                // Check if this qualification refers to our table
                let table_str = table_or_alias.as_str();
                let actual_table = if let Some(actual) = aliases.get(table_str) {
                    actual.clone()
                } else if table_str.contains('.') {
                    // Handle database.table format
                    table_str
                        .split('.')
                        .next_back()
                        .unwrap_or(table_str)
                        .to_string()
                } else {
                    table_str.to_string()
                };

                if actual_table == table_name {
                    // Remove the qualification
                    ast::Expr::Id(column.clone())
                } else {
                    // Keep the qualification (shouldn't happen if extraction worked correctly)
                    expr.clone()
                }
            }
            ast::Expr::DoublyQualified(_database, table, column) => {
                // Check if this refers to our table
                if table.as_str() == table_name {
                    // Remove the qualification, keep just the column
                    ast::Expr::Id(column.clone())
                } else {
                    // Keep the qualification (shouldn't happen if extraction worked correctly)
                    expr.clone()
                }
            }
            ast::Expr::Unary(op, inner) => ast::Expr::Unary(
                *op,
                Box::new(Self::unqualify_expression(inner, table_name, aliases)),
            ),
            ast::Expr::FunctionCall {
                name,
                args,
                distinctness,
                filter_over,
                order_by,
                within_group,
            } => ast::Expr::FunctionCall {
                name: name.clone(),
                args: args
                    .iter()
                    .map(|arg| Box::new(Self::unqualify_expression(arg, table_name, aliases)))
                    .collect(),
                distinctness: *distinctness,
                filter_over: filter_over.clone(),
                order_by: order_by.clone(),
                within_group: within_group.clone(),
            },
            ast::Expr::InList { lhs, not, rhs } => ast::Expr::InList {
                lhs: Box::new(Self::unqualify_expression(lhs, table_name, aliases)),
                not: *not,
                rhs: rhs
                    .iter()
                    .map(|item| Box::new(Self::unqualify_expression(item, table_name, aliases)))
                    .collect(),
            },
            ast::Expr::Between {
                lhs,
                not,
                start,
                end,
            } => ast::Expr::Between {
                lhs: Box::new(Self::unqualify_expression(lhs, table_name, aliases)),
                not: *not,
                start: Box::new(Self::unqualify_expression(start, table_name, aliases)),
                end: Box::new(Self::unqualify_expression(end, table_name, aliases)),
            },
            _ => expr.clone(),
        }
    }

    /// Get all tables referenced in an expression
    fn get_tables_in_expr(
        expr: &ast::Expr,
        aliases: &HashMap<String, String>,
        all_tables: &[String],
        schema: &Schema,
    ) -> Vec<String> {
        let mut tables = Vec::new();
        Self::collect_tables_in_expr(expr, aliases, all_tables, schema, &mut tables);
        tables.sort();
        tables.dedup();
        tables
    }

    /// Recursively collect table references from an expression
    fn collect_tables_in_expr(
        expr: &ast::Expr,
        aliases: &HashMap<String, String>,
        all_tables: &[String],
        schema: &Schema,
        tables: &mut Vec<String>,
    ) {
        match expr {
            ast::Expr::Binary(left, _, right) => {
                Self::collect_tables_in_expr(left, aliases, all_tables, schema, tables);
                Self::collect_tables_in_expr(right, aliases, all_tables, schema, tables);
            }
            ast::Expr::Qualified(table_or_alias, _) => {
                // Handle database.table or just table/alias
                let table_str = table_or_alias.as_str();
                let table_name = if let Some(actual_table) = aliases.get(table_str) {
                    // It's an alias
                    actual_table.clone()
                } else if table_str.contains('.') {
                    // It might be database.table format, extract just the table name
                    table_str
                        .split('.')
                        .next_back()
                        .unwrap_or(table_str)
                        .to_string()
                } else {
                    // It's a direct table name
                    table_str.to_string()
                };
                tables.push(table_name);
            }
            ast::Expr::DoublyQualified(_database, table, _column) => {
                // For database.table.column, extract the table name
                tables.push(table.to_string());
            }
            ast::Expr::Id(column) => {
                // Unqualified column - try to find which table has this column
                if all_tables.len() == 1 {
                    tables.push(all_tables[0].clone());
                } else {
                    // Check which table has this column
                    for table_name in all_tables {
                        if let Some(table) = schema.get_table(table_name) {
                            if table
                                .columns()
                                .iter()
                                .any(|col| col.name.as_deref() == Some(column.as_str()))
                            {
                                tables.push(table_name.clone());
                                break; // Found the table, stop looking
                            }
                        }
                    }
                }
            }
            ast::Expr::FunctionCall { args, .. } => {
                for arg in args {
                    Self::collect_tables_in_expr(arg, aliases, all_tables, schema, tables);
                }
            }
            ast::Expr::InList { lhs, rhs, .. } => {
                Self::collect_tables_in_expr(lhs, aliases, all_tables, schema, tables);
                for item in rhs {
                    Self::collect_tables_in_expr(item, aliases, all_tables, schema, tables);
                }
            }
            ast::Expr::InSelect { lhs, .. } => {
                Self::collect_tables_in_expr(lhs, aliases, all_tables, schema, tables);
            }
            ast::Expr::Between {
                lhs, start, end, ..
            } => {
                Self::collect_tables_in_expr(lhs, aliases, all_tables, schema, tables);
                Self::collect_tables_in_expr(start, aliases, all_tables, schema, tables);
                Self::collect_tables_in_expr(end, aliases, all_tables, schema, tables);
            }
            ast::Expr::Unary(_, expr) => {
                Self::collect_tables_in_expr(expr, aliases, all_tables, schema, tables);
            }
            _ => {
                // Literals, etc. don't reference tables
            }
        }
    }
    /// Populate the view by scanning the source table using a state machine
    /// This can be called multiple times and will resume from where it left off
    /// This method is only for materialized views and will persist data to the btree
    pub fn populate_from_table(
        &mut self,
        conn: &crate::sync::Arc<crate::Connection>,
        pager: &crate::sync::Arc<crate::Pager>,
        _btree_cursor: &mut dyn CursorTrait,
    ) -> crate::Result<IOResult<()>> {
        // Assert that this is a materialized view with a root page
        assert!(
            self.root_page != 0,
            "populate_from_table should only be called for materialized views with root_page"
        );

        // Mark as nested for the duration of this call to prevent inner queries from
        // committing the outer transaction's dirty pages. We increment on every entry
        // and decrement on every exit (including IO yields and errors) so re-entrant
        // calls keep the counter balanced.
        conn.start_nested();
        let result = self.populate_from_table_inner(conn, pager, _btree_cursor);
        conn.end_nested();
        result
    }

    fn populate_from_table_inner(
        &mut self,
        conn: &crate::sync::Arc<crate::Connection>,
        pager: &crate::sync::Arc<crate::Pager>,
        _btree_cursor: &mut dyn CursorTrait,
    ) -> crate::Result<IOResult<()>> {
        'outer: loop {
            match std::mem::replace(&mut self.populate_state, PopulateState::Done) {
                PopulateState::Start => {
                    // For recursive CTEs, we must gather all source table data first,
                    // then run through the circuit in one batch, because fixed-point iteration
                    // requires all base case rows to be available together.
                    if self.has_recursive_cte {
                        // Transition to the collecting state
                        let remaining_tables: Vec<String> = self
                            .referenced_tables
                            .iter()
                            .map(|t| t.name.clone())
                            .collect();
                        self.populate_state = PopulateState::CollectingRecursiveCteData {
                            remaining_tables,
                            current_stmt: None,
                            accumulated_deltas: DeltaSet::new(),
                        };
                        continue 'outer;
                    }

                    // Generate the SQL query for populating the view
                    // It is best to use a standard query than a cursor for two reasons:
                    // 1) Using a sql query will allow us to be much more efficient in cases where we only want
                    //    some rows, in particular for indexed filters
                    // 2) There are two types of cursors: index and table. In some situations (like for example
                    //    if the table has an integer primary key), the key will be exclusively in the index
                    //    btree and not in the table btree. Using cursors would force us to be aware of this
                    //    distinction (and others), and ultimately lead to reimplementing the whole query
                    //    machinery (next step is which index is best to use, etc)
                    let queries = self.sql_for_populate()?;

                    self.populate_state = PopulateState::ProcessingAllTables {
                        queries,
                        current_idx: 0,
                    };
                }

                PopulateState::ProcessingAllTables {
                    queries,
                    current_idx,
                } => {
                    if current_idx >= queries.len() {
                        self.populate_state = PopulateState::Done;
                        return Ok(IOResult::Done(()));
                    }

                    let query = queries[current_idx].clone();
                    // Use the parent connection directly for reading.
                    // We need to use the same connection that has the uncommitted schema changes.
                    // Creating a new connection would cause schema version mismatch issues because
                    // the new connection's schema cookie check would fail (database file has old version).

                    // Prepare the statement using the parent connection
                    let stmt = conn.prepare(&query)?;

                    self.populate_state = PopulateState::ProcessingOneTable {
                        queries,
                        current_idx,
                        stmt: Box::new(stmt),
                        rows_processed: 0,
                        pending_row: None,
                    };
                }

                PopulateState::ProcessingOneTable {
                    queries,
                    current_idx,
                    mut stmt,
                    mut rows_processed,
                    pending_row,
                } => {
                    // If we have a pending row from a previous I/O interruption, process it first
                    if let Some((rowid, values)) = pending_row {
                        match self.process_one_row(
                            rowid,
                            values.clone(),
                            current_idx,
                            pager.clone(),
                        )? {
                            IOResult::Done(_) => {
                                // Row processed successfully, continue to next row
                                rows_processed += 1;
                            }
                            IOResult::IO(io) => {
                                // Still not done, restore state with pending row and return
                                self.populate_state = PopulateState::ProcessingOneTable {
                                    queries,
                                    current_idx,
                                    stmt,
                                    rows_processed,
                                    pending_row: Some((rowid, values)),
                                };
                                return Ok(IOResult::IO(io));
                            }
                        }
                    }

                    // Process rows one at a time - no batching
                    loop {
                        // This step() call resumes from where the statement left off
                        match stmt.step()? {
                            crate::vdbe::StepResult::Row => {
                                // Get the row
                                let row = stmt.row().ok_or_else(|| {
                                    LimboError::InternalError(
                                        "row should exist after StepResult::Row".to_string(),
                                    )
                                })?;

                                // Extract values from the row
                                let all_values: Vec<crate::types::Value> =
                                    row.get_values().cloned().collect();

                                // Extract rowid and values using helper
                                let (rowid, values) = match self.extract_rowid_and_values(
                                    all_values,
                                    current_idx,
                                    rows_processed as i64 + 1,
                                ) {
                                    Some(result) => result,
                                    None => {
                                        // Invalid rowid, skip this row
                                        rows_processed += 1;
                                        continue;
                                    }
                                };

                                // Process this row
                                match self.process_one_row(
                                    rowid,
                                    values.clone(),
                                    current_idx,
                                    pager.clone(),
                                )? {
                                    IOResult::Done(_) => {
                                        // Row processed successfully, continue to next row
                                        rows_processed += 1;
                                    }
                                    IOResult::IO(io) => {
                                        // Save state and return I/O
                                        // We'll resume at the SAME row when called again (don't increment rows_processed)
                                        // The circuit still has unfinished work for this row
                                        self.populate_state = PopulateState::ProcessingOneTable {
                                            queries,
                                            current_idx,
                                            stmt,
                                            rows_processed, // Don't increment - row not done yet!
                                            pending_row: Some((rowid, values)), // Save the row for resumption
                                        };
                                        return Ok(IOResult::IO(io));
                                    }
                                }
                            }

                            crate::vdbe::StepResult::Done => {
                                // All rows processed from this table
                                // Move to next table
                                self.populate_state = PopulateState::ProcessingAllTables {
                                    queries,
                                    current_idx: current_idx + 1,
                                };
                                continue 'outer;
                            }

                            crate::vdbe::StepResult::Interrupt | crate::vdbe::StepResult::Busy => {
                                // Save state before returning error
                                self.populate_state = PopulateState::ProcessingOneTable {
                                    queries,
                                    current_idx,
                                    stmt,
                                    rows_processed,
                                    pending_row: None, // No pending row when interrupted between rows
                                };
                                return Err(LimboError::Busy);
                            }

                            crate::vdbe::StepResult::IO | crate::vdbe::StepResult::Yield => {
                                // Statement needs I/O - save state and return
                                self.populate_state = PopulateState::ProcessingOneTable {
                                    queries,
                                    current_idx,
                                    stmt,
                                    rows_processed,
                                    pending_row: None, // No pending row when interrupted between rows
                                };
                                // TODO: Get the actual I/O completion from the statement
                                let completion = crate::io::Completion::new_yield();
                                return Ok(IOResult::IO(crate::types::IOCompletions(completion)));
                            }
                        }
                    }
                }

                PopulateState::CollectingRecursiveCteData {
                    mut remaining_tables,
                    current_stmt,
                    mut accumulated_deltas,
                } => {
                    // If we have an active statement, continue reading from it
                    if let Some((table_name, mut stmt)) = current_stmt {
                        let mut table_delta = accumulated_deltas.get(&table_name);

                        loop {
                            match stmt.step()? {
                                crate::vdbe::StepResult::Row => {
                                    let row = stmt.row().ok_or_else(|| {
                                        LimboError::InternalError(
                                            "row should exist after StepResult::Row".to_string(),
                                        )
                                    })?;
                                    let all_values: Vec<crate::types::Value> =
                                        row.get_values().cloned().collect();

                                    // Last value is rowid
                                    let rowid = match all_values.last() {
                                        Some(crate::types::Value::Numeric(
                                            crate::numeric::Numeric::Integer(id),
                                        )) => *id,
                                        _ => continue,
                                    };
                                    let values = all_values[..all_values.len() - 1].to_vec();
                                    table_delta.insert(rowid, values);
                                }
                                crate::vdbe::StepResult::Done => {
                                    // Finished this table, save delta and continue to next
                                    accumulated_deltas.insert(table_name.clone(), table_delta);
                                    self.populate_state =
                                        PopulateState::CollectingRecursiveCteData {
                                            remaining_tables,
                                            current_stmt: None,
                                            accumulated_deltas,
                                        };
                                    continue 'outer;
                                }
                                crate::vdbe::StepResult::Interrupt
                                | crate::vdbe::StepResult::Busy => {
                                    // Save state and retry
                                    accumulated_deltas.insert(table_name.clone(), table_delta);
                                    self.populate_state =
                                        PopulateState::CollectingRecursiveCteData {
                                            remaining_tables,
                                            current_stmt: Some((table_name, stmt)),
                                            accumulated_deltas,
                                        };
                                    return Err(LimboError::Busy);
                                }
                                crate::vdbe::StepResult::IO | crate::vdbe::StepResult::Yield => {
                                    // Save state and return I/O
                                    accumulated_deltas.insert(table_name.clone(), table_delta);
                                    self.populate_state =
                                        PopulateState::CollectingRecursiveCteData {
                                            remaining_tables,
                                            current_stmt: Some((table_name, stmt)),
                                            accumulated_deltas,
                                        };
                                    let completion = crate::io::Completion::new_yield();
                                    return Ok(IOResult::IO(crate::types::IOCompletions::Single(
                                        completion,
                                    )));
                                }
                            }
                        }
                    }

                    // No active statement - start reading next table or finish
                    if let Some(table_name) = remaining_tables.pop() {
                        let quoted = table_name.replace('"', "\"\"");
                        let query = format!("SELECT *, rowid FROM \"{quoted}\" ORDER BY rowid");
                        // Use the parent connection to see uncommitted writes
                        // in the current transaction (same fix as ProcessingAllTables).
                        let stmt = conn.prepare(&query)?;

                        self.populate_state = PopulateState::CollectingRecursiveCteData {
                            remaining_tables,
                            current_stmt: Some((table_name, Box::new(stmt))),
                            accumulated_deltas,
                        };
                        continue 'outer;
                    }

                    // All tables collected, transition to circuit execution
                    let input_map = accumulated_deltas.into_map();
                    self.populate_state = PopulateState::ExecutingRecursiveCircuit {
                        input_map: Some(input_map),
                    };
                    continue 'outer;
                }

                PopulateState::ExecutingRecursiveCircuit { input_map } => {
                    // Run circuit with input data (first call) or empty map (resume after I/O)
                    let data = input_map.unwrap_or_default();
                    match self.circuit.commit(data, pager.clone())? {
                        IOResult::Done(_) => {
                            self.populate_state = PopulateState::Done;
                            return Ok(IOResult::Done(()));
                        }
                        IOResult::IO(io) => {
                            // Save state for resume — the circuit stores its commit progress
                            // internally in self.circuit.commit_state, so it resumes correctly
                            // even though we pass None for input_map on re-entry.
                            self.populate_state =
                                PopulateState::ExecutingRecursiveCircuit { input_map: None };
                            return Ok(IOResult::IO(io));
                        }
                    }
                }

                PopulateState::Done => {
                    return Ok(IOResult::Done(()));
                }
            }
        }
    }

    /// Process a single row through the circuit
    fn process_one_row(
        &mut self,
        rowid: i64,
        values: Vec<Value>,
        table_idx: usize,
        pager: Arc<crate::Pager>,
    ) -> crate::Result<IOResult<()>> {
        // Create a single-row delta
        let mut single_row_delta = Delta::new();
        single_row_delta.insert(rowid, values);

        // Create a DeltaSet with this delta for the current table
        let mut delta_set = DeltaSet::new();
        let table_name = self.referenced_tables[table_idx].name.clone();
        delta_set.insert(table_name, single_row_delta);

        // Process through merge_delta - we discard the output delta here since
        // this is used during population, not during transaction commit
        match self.merge_delta(delta_set, pager)? {
            IOResult::Done(_output_delta) => Ok(IOResult::Done(())),
            IOResult::IO(io) => Ok(IOResult::IO(io)),
        }
    }

    /// Extract rowid and values from a row.
    /// `synthetic_rowid` is used for virtual/foreign tables that don't expose rowid in SQL.
    fn extract_rowid_and_values(
        &self,
        all_values: Vec<Value>,
        table_idx: usize,
        synthetic_rowid: i64,
    ) -> Option<(i64, Vec<Value>)> {
        let table = &self.referenced_tables[table_idx];
        if table.is_virtual {
            // Virtual/foreign tables: all values are columns, use synthetic rowid
            Some((synthetic_rowid, all_values))
        } else if let Some(idx) = table.get_rowid_alias_column() {
            // The rowid is the value at the rowid alias column index
            let rowid = match all_values.get(idx) {
                Some(Value::Numeric(Numeric::Integer(id))) => *id,
                _ => return None,
            };
            Some((rowid, all_values))
        } else {
            // The last value is the explicitly selected rowid
            let rowid = match all_values.last() {
                Some(Value::Numeric(Numeric::Integer(id))) => *id,
                _ => return None,
            };
            let values = all_values[..all_values.len() - 1].to_vec();
            Some((rowid, values))
        }
    }

    /// Merge a delta set of changes into the view's current state
    pub fn merge_delta(
        &mut self,
        delta_set: DeltaSet,
        pager: Arc<crate::Pager>,
    ) -> crate::Result<IOResult<Delta>> {
        // Early return if all deltas are empty
        if delta_set.is_empty() {
            return Ok(IOResult::Done(Delta::new()));
        }

        // Use the circuit to process the deltas and write to btree
        let input_data = delta_set.into_map();

        // The circuit handles all btree I/O internally and returns the output delta
        let output_delta = return_if_io!(self.circuit.commit(input_data, pager));
        Ok(IOResult::Done(output_delta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc::vec;
    use crate::schema::{
        BTreeCharacteristics, BTreeTable, ColDef, Column as SchemaColumn, Schema, Type,
    };
    use crate::sync::Arc;
    use turso_parser::ast;
    use turso_parser::parser::Parser;

    // Helper function to create a test schema with multiple tables
    fn create_test_schema() -> Schema {
        let mut schema = Schema::new();

        // Create customers table
        let columns = vec![
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
                    explicit_notnull: false,
                    unique: false,
                    hidden: false,
                    notnull_conflict_clause: None,
                },
            ),
            SchemaColumn::new_default_text(Some("name".to_string()), "TEXT".to_string(), None),
        ];
        let customers_table = BTreeTable::new(
            2,
            "customers".to_string(),
            vec![("id".to_string(), ast::SortOrder::Asc)],
            columns,
            BTreeCharacteristics::HAS_ROWID,
            vec![],
            vec![],
            vec![],
            None,
        );

        // Create orders table
        let columns = vec![
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
                    explicit_notnull: false,
                    unique: false,
                    hidden: false,
                    notnull_conflict_clause: None,
                },
            ),
            SchemaColumn::new(
                Some("customer_id".to_string()),
                "INTEGER".to_string(),
                None,
                None,
                Type::Integer,
                None,
                ColDef::default(),
            ),
            SchemaColumn::new_default_integer(
                Some("total".to_string()),
                "INTEGER".to_string(),
                None,
            ),
        ];
        let orders_table = BTreeTable::new(
            3,
            "orders".to_string(),
            vec![("id".to_string(), ast::SortOrder::Asc)],
            columns,
            BTreeCharacteristics::HAS_ROWID,
            vec![],
            vec![],
            vec![],
            None,
        );

        // Create products table
        let columns = vec![
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
                    explicit_notnull: false,
                    unique: false,
                    hidden: false,
                    notnull_conflict_clause: None,
                },
            ),
            SchemaColumn::new_default_text(Some("name".to_string()), "TEXT".to_string(), None),
            SchemaColumn::new(
                Some("price".to_string()),
                "REAL".to_string(),
                None,
                None,
                Type::Real,
                None,
                ColDef::default(),
            ),
        ];
        let products_table = BTreeTable::new(
            4,
            "products".to_string(),
            vec![("id".to_string(), ast::SortOrder::Asc)],
            columns,
            BTreeCharacteristics::HAS_ROWID,
            vec![],
            vec![],
            vec![],
            None,
        );

        // Create logs table - without a rowid alias (no INTEGER PRIMARY KEY)
        let columns = vec![
            SchemaColumn::new(
                Some("message".to_string()),
                "TEXT".to_string(),
                None,
                None,
                Type::Text,
                None,
                ColDef::default(),
            ),
            SchemaColumn::new_default_integer(
                Some("level".to_string()),
                "INTEGER".to_string(),
                None,
            ),
            SchemaColumn::new_default_integer(
                Some("timestamp".to_string()),
                "INTEGER".to_string(),
                None,
            ),
        ];
        // logs has no primary key (no rowid alias) but does have an implicit rowid.
        let logs_table = BTreeTable::new(
            5,
            "logs".to_string(),
            vec![],
            columns,
            BTreeCharacteristics::HAS_ROWID,
            vec![],
            vec![],
            vec![],
            None,
        );

        schema
            .add_btree_table(Arc::new(customers_table))
            .expect("Test setup: failed to add customers table");

        schema
            .add_btree_table(Arc::new(orders_table))
            .expect("Test setup: failed to add orders table");

        schema
            .add_btree_table(Arc::new(products_table))
            .expect("Test setup: failed to add products table");

        schema
            .add_btree_table(Arc::new(logs_table))
            .expect("Test setup: failed to add logs table");

        schema
    }

    // Helper to parse SQL and extract the SELECT statement
    fn parse_select(sql: &str) -> ast::Select {
        let mut parser = Parser::new(sql.as_bytes());
        let cmd = parser.next().unwrap().unwrap();
        match cmd {
            ast::Cmd::Stmt(ast::Stmt::Select(select)) => select,
            _ => panic!("Expected SELECT statement"),
        }
    }

    // Type alias for the complex return type of extract_all_tables
    type ExtractedTableInfo = (
        Vec<ReferencedTable>,
        HashMap<String, String>,
        HashMap<String, String>,
        HashMap<String, Vec<Option<ast::Expr>>>,
    );

    fn extract_all_tables(select: &ast::Select, schema: &Schema) -> Result<ExtractedTableInfo> {
        let mut referenced_tables = Vec::new();
        let mut table_aliases = HashMap::default();
        let mut qualified_table_names = HashMap::default();
        let mut table_conditions = HashMap::default();
        IncrementalView::extract_all_tables(
            select,
            schema,
            &mut referenced_tables,
            &mut table_aliases,
            &mut qualified_table_names,
            &mut table_conditions,
        )?;
        Ok((
            referenced_tables,
            table_aliases,
            qualified_table_names,
            table_conditions,
        ))
    }

    #[test]
    fn test_extract_single_table() {
        let schema = create_test_schema();
        let select = parse_select("SELECT * FROM customers");

        let (tables, _, _, _table_conditions) = extract_all_tables(&select, &schema).unwrap();

        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name, "customers");
    }

    #[test]
    fn test_tables_from_union() {
        let schema = create_test_schema();
        let select = parse_select("SELECT name FROM customers union SELECT name from products");

        let (tables, _, _, table_conditions) = extract_all_tables(&select, &schema).unwrap();

        assert_eq!(tables.len(), 2);
        assert!(table_conditions.contains_key("customers"));
        assert!(table_conditions.contains_key("products"));
    }

    #[test]
    fn test_extract_tables_from_inner_join() {
        let schema = create_test_schema();
        let select = parse_select(
            "SELECT * FROM customers INNER JOIN orders ON customers.id = orders.customer_id",
        );

        let (tables, _, _, table_conditions) = extract_all_tables(&select, &schema).unwrap();

        assert_eq!(tables.len(), 2);
        assert!(table_conditions.contains_key("customers"));
        assert!(table_conditions.contains_key("orders"));
    }

    #[test]
    fn test_extract_tables_from_multiple_joins() {
        let schema = create_test_schema();
        let select = parse_select(
            "SELECT * FROM customers
             INNER JOIN orders ON customers.id = orders.customer_id
             INNER JOIN products ON orders.id = products.id",
        );

        let (tables, _, _, table_conditions) = extract_all_tables(&select, &schema).unwrap();

        assert_eq!(tables.len(), 3);
        assert!(table_conditions.contains_key("customers"));
        assert!(table_conditions.contains_key("orders"));
        assert!(table_conditions.contains_key("products"));
    }

    #[test]
    fn test_extract_tables_from_left_join() {
        let schema = create_test_schema();
        let select = parse_select(
            "SELECT * FROM customers LEFT JOIN orders ON customers.id = orders.customer_id",
        );

        let (tables, _, _, table_conditions) = extract_all_tables(&select, &schema).unwrap();

        assert_eq!(tables.len(), 2);
        assert!(table_conditions.contains_key("customers"));
        assert!(table_conditions.contains_key("orders"));
    }

    #[test]
    fn test_extract_tables_from_cross_join() {
        let schema = create_test_schema();
        let select = parse_select("SELECT * FROM customers CROSS JOIN orders");

        let (tables, _, _, table_conditions) = extract_all_tables(&select, &schema).unwrap();

        assert_eq!(tables.len(), 2);
        assert!(table_conditions.contains_key("customers"));
        assert!(table_conditions.contains_key("orders"));
    }

    #[test]
    fn test_extract_tables_with_aliases() {
        let schema = create_test_schema();
        let select =
            parse_select("SELECT * FROM customers c INNER JOIN orders o ON c.id = o.customer_id");

        let (tables, aliases, _, _table_conditions) = extract_all_tables(&select, &schema).unwrap();

        // Should still extract the actual table names, not aliases
        assert_eq!(tables.len(), 2);
        let table_names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert!(table_names.contains(&"customers"));
        assert!(table_names.contains(&"orders"));

        // Check that aliases are correctly mapped
        assert_eq!(aliases.get("c"), Some(&"customers".to_string()));
        assert_eq!(aliases.get("o"), Some(&"orders".to_string()));
    }

    #[test]
    fn test_extract_tables_nonexistent_table_error() {
        let schema = create_test_schema();
        let select = parse_select("SELECT * FROM nonexistent");

        let result = extract_all_tables(&select, &schema).map(|(tables, _, _, _)| tables);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Table 'nonexistent' not found"));
    }

    #[test]
    fn test_extract_tables_nonexistent_join_table_error() {
        let schema = create_test_schema();
        let select = parse_select(
            "SELECT * FROM customers INNER JOIN nonexistent ON customers.id = nonexistent.id",
        );

        let result = extract_all_tables(&select, &schema).map(|(tables, _, _, _)| tables);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Table 'nonexistent' not found"));
    }

    #[test]
    fn test_sql_for_populate_simple_query_no_where() {
        // Test simple query with no WHERE clause
        let schema = create_test_schema();
        let select = parse_select("SELECT * FROM customers");

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();
        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 1);
        // customers has id as rowid alias, so no need for explicit rowid
        assert_eq!(queries[0], "SELECT * FROM customers");
    }

    #[test]
    fn test_sql_for_populate_simple_query_with_where() {
        // Test simple query with WHERE clause
        let schema = create_test_schema();
        let select = parse_select("SELECT * FROM customers WHERE id > 10");

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();
        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 1);
        // For single-table queries, we should get the full WHERE clause
        assert_eq!(queries[0], "SELECT * FROM customers WHERE id > 10");
    }

    #[test]
    fn test_sql_for_populate_join_with_where_on_both_tables() {
        // Test JOIN query with WHERE conditions on both tables
        let schema = create_test_schema();
        let select = parse_select(
            "SELECT * FROM customers c \
             JOIN orders o ON c.id = o.customer_id \
             WHERE c.id > 10 AND o.total > 100",
        );

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();
        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 2);

        // With per-table WHERE extraction:
        // - customers table gets: c.id > 10
        // - orders table gets: o.total > 100
        assert!(queries
            .iter()
            .any(|q| q == "SELECT * FROM customers WHERE id > 10"));
        assert!(queries
            .iter()
            .any(|q| q == "SELECT * FROM orders WHERE total > 100"));
    }

    #[test]
    fn test_sql_for_populate_complex_join_with_mixed_conditions() {
        // Test complex JOIN with WHERE conditions mixing both tables
        let schema = create_test_schema();
        let select = parse_select(
            "SELECT * FROM customers c \
             JOIN orders o ON c.id = o.customer_id \
             WHERE c.id > 10 AND o.total > 100 AND c.name = 'John' \
             AND o.customer_id = 5 AND (c.id = 15 OR o.total = 200)",
        );

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();
        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 2);

        // With per-table WHERE extraction:
        // - customers gets: c.id > 10 AND c.name = 'John'
        // - orders gets: o.total > 100 AND o.customer_id = 5
        // Note: The OR condition (c.id = 15 OR o.total = 200) involves both tables,
        // so it cannot be extracted to either table individually
        // Check both queries exist (order doesn't matter)
        assert!(queries
            .contains(&"SELECT * FROM customers WHERE id > 10 AND name = 'John'".to_string()));
        assert!(queries
            .contains(&"SELECT * FROM orders WHERE total > 100 AND customer_id = 5".to_string()));
    }

    #[test]
    fn test_sql_for_populate_table_without_rowid_alias() {
        let schema = create_test_schema();
        let select = parse_select("SELECT * FROM logs WHERE level > 2");

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();
        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 1);
        // logs table has no rowid alias, so we need to explicitly select rowid
        assert_eq!(queries[0], "SELECT *, rowid FROM logs WHERE level > 2");
    }

    #[test]
    fn test_sql_for_populate_join_with_and_without_rowid_alias() {
        // Test JOIN between a table with rowid alias and one without
        let schema = create_test_schema();
        let select = parse_select(
            "SELECT * FROM customers c \
             JOIN logs l ON c.id = l.level \
             WHERE c.id > 10 AND l.level > 2",
        );

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();
        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 2);
        // customers has rowid alias (id), logs doesn't
        assert!(queries.contains(&"SELECT * FROM customers WHERE id > 10".to_string()));
        assert!(queries.contains(&"SELECT *, rowid FROM logs WHERE level > 2".to_string()));
    }

    #[test]
    fn test_sql_for_populate_with_database_qualified_names() {
        // Test that database.table.column references are handled correctly
        // The table name in FROM should keep the database prefix,
        // but column names in WHERE should be unqualified
        let schema = create_test_schema();

        // Test with single table using database qualification
        let select = parse_select("SELECT * FROM main.customers WHERE main.customers.id > 10");

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();
        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 1);
        // The FROM clause should preserve the database qualification,
        // but the WHERE clause should have unqualified column names
        assert_eq!(queries[0], "SELECT * FROM main.customers WHERE id > 10");
    }

    #[test]
    fn test_sql_for_populate_join_with_database_qualified_names() {
        // Test JOIN with database-qualified table and column references
        let schema = create_test_schema();

        let select = parse_select(
            "SELECT * FROM main.customers c \
             JOIN main.orders o ON c.id = o.customer_id \
             WHERE main.customers.id > 10 AND main.orders.total > 100",
        );

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();
        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 2);
        // The FROM clauses should preserve database qualification,
        // but WHERE clauses should have unqualified column names
        assert!(queries.contains(&"SELECT * FROM main.customers WHERE id > 10".to_string()));
        assert!(queries.contains(&"SELECT * FROM main.orders WHERE total > 100".to_string()));
    }

    #[test]
    fn test_where_extraction_for_three_tables_with_aliases() {
        // Test that WHERE clause extraction correctly separates conditions for 3+ tables
        // This addresses the concern about conditions "piling up" as joins increase
        let schema = create_test_schema();
        let select = parse_select(
            "SELECT * FROM customers c
             JOIN orders o ON c.id = o.customer_id
             JOIN products p ON p.id = o.product_id
             WHERE c.id > 10 AND o.total > 100 AND p.price > 50",
        );

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();

        // Verify we extracted all three tables
        assert_eq!(tables.len(), 3);
        let table_names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert!(table_names.contains(&"customers"));
        assert!(table_names.contains(&"orders"));
        assert!(table_names.contains(&"products"));

        // Verify aliases are correctly mapped
        assert_eq!(aliases.get("c"), Some(&"customers".to_string()));
        assert_eq!(aliases.get("o"), Some(&"orders".to_string()));
        assert_eq!(aliases.get("p"), Some(&"products".to_string()));

        // Generate populate queries to verify each table gets its own conditions
        let queries = IncrementalView::generate_populate_queries(
            &select,
            &tables,
            &aliases,
            &qualified_names,
            &table_conditions,
        )
        .unwrap();

        assert_eq!(queries.len(), 3);

        // Verify the exact queries generated for each table
        // The order might vary, so check all possibilities
        let expected_queries = vec![
            "SELECT * FROM customers WHERE id > 10",
            "SELECT * FROM orders WHERE total > 100",
            "SELECT * FROM products WHERE price > 50",
        ];

        for expected in &expected_queries {
            assert!(
                queries.contains(&expected.to_string()),
                "Missing expected query: {expected}. Got: {queries:?}"
            );
        }
    }

    #[test]
    fn test_sql_for_populate_complex_expressions_not_included() {
        // Test that complex expressions (subqueries, CASE, string concat) are NOT included in populate queries
        let schema = create_test_schema();
        let select = parse_select(
            "SELECT * FROM customers
             WHERE id > (SELECT MAX(customer_id) FROM orders)
               AND name || ' Customer' = 'John Customer'
               AND CASE WHEN id > 10 THEN 1 ELSE 0 END = 1
               AND EXISTS (SELECT 1 FROM orders WHERE customer_id = customers.id)",
        );

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();

        let queries = IncrementalView::generate_populate_queries(
            &select,
            &tables,
            &aliases,
            &qualified_names,
            &table_conditions,
        )
        .unwrap();

        assert_eq!(queries.len(), 1);
        // Since customers table has an INTEGER PRIMARY KEY (id), we should get SELECT *
        // without rowid and without WHERE clause (all conditions are complex)
        assert_eq!(queries[0], "SELECT * FROM customers");
    }

    #[test]
    fn test_sql_for_populate_unambiguous_unqualified_column() {
        // Test that unambiguous unqualified columns ARE extracted
        let schema = create_test_schema();
        let select = parse_select(
            "SELECT * FROM customers c \
             JOIN orders o ON c.id = o.customer_id \
             WHERE total > 100", // 'total' only exists in orders table
        );

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();
        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 2);

        // 'total' is unambiguous (only in orders), so it should be extracted
        assert!(queries.contains(&"SELECT * FROM customers".to_string()));
        assert!(queries.contains(&"SELECT * FROM orders WHERE total > 100".to_string()));
    }

    #[test]
    fn test_database_qualified_table_names() {
        let schema = create_test_schema();

        // Test with database-qualified table names
        let select = parse_select(
            "SELECT c.id, c.name, o.id, o.total
             FROM main.customers c
             JOIN main.orders o ON c.id = o.customer_id
             WHERE c.id > 10",
        );

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();

        // Check that qualified names are preserved
        assert!(qualified_names.contains_key("customers"));
        assert_eq!(qualified_names.get("customers").unwrap(), "main.customers");
        assert!(qualified_names.contains_key("orders"));
        assert_eq!(qualified_names.get("orders").unwrap(), "main.orders");

        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 2);

        // The FROM clause should contain the database-qualified name
        // But the WHERE clause should use unqualified column names
        assert!(queries.contains(&"SELECT * FROM main.customers WHERE id > 10".to_string()));
        assert!(queries.contains(&"SELECT * FROM main.orders".to_string()));
    }

    #[test]
    fn test_mixed_qualified_unqualified_tables() {
        let schema = create_test_schema();

        // Test with a mix of qualified and unqualified table names
        let select = parse_select(
            "SELECT c.id, c.name, o.id, o.total
             FROM main.customers c
             JOIN orders o ON c.id = o.customer_id
             WHERE c.id > 10 AND o.total < 1000",
        );

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();

        // Check that qualified names are preserved where specified
        assert_eq!(qualified_names.get("customers").unwrap(), "main.customers");
        // Unqualified tables should not have an entry (or have the bare name)
        assert!(
            !qualified_names.contains_key("orders")
                || qualified_names.get("orders").unwrap() == "orders"
        );

        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 2);

        // The FROM clause should preserve qualification where specified
        assert!(queries.contains(&"SELECT * FROM main.customers WHERE id > 10".to_string()));
        assert!(queries.contains(&"SELECT * FROM orders WHERE total < 1000".to_string()));
    }

    #[test]
    fn test_extract_tables_with_simple_cte() {
        let schema = create_test_schema();
        let select = parse_select(
            "WITH customer_totals AS (
                SELECT c.id, c.name, SUM(o.total) as total_spent
                FROM customers c
                JOIN orders o ON c.id = o.customer_id
                GROUP BY c.id, c.name
            )
            SELECT * FROM customer_totals WHERE total_spent > 1000",
        );

        let (tables, aliases, _qualified_names, _table_conditions) =
            extract_all_tables(&select, &schema).unwrap();

        // Check that we found both tables from the CTE
        assert_eq!(tables.len(), 2);
        let table_names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert!(table_names.contains(&"customers"));
        assert!(table_names.contains(&"orders"));

        // Check aliases from the CTE
        assert_eq!(aliases.get("c"), Some(&"customers".to_string()));
        assert_eq!(aliases.get("o"), Some(&"orders".to_string()));
    }

    #[test]
    fn test_extract_tables_with_multiple_ctes() {
        let schema = create_test_schema();
        let select = parse_select(
            "WITH
            high_value_customers AS (
                SELECT id, name
                FROM customers
                WHERE id IN (SELECT customer_id FROM orders WHERE total > 500)
            ),
            recent_orders AS (
                SELECT id, customer_id, total
                FROM orders
                WHERE id > 100
            )
            SELECT hvc.name, ro.total
            FROM high_value_customers hvc
            JOIN recent_orders ro ON hvc.id = ro.customer_id",
        );

        let (tables, _aliases, _qualified_names, _table_conditions) =
            extract_all_tables(&select, &schema).unwrap();

        // Check that we found both tables from both CTEs
        assert_eq!(tables.len(), 2);
        let table_names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert!(table_names.contains(&"customers"));
        assert!(table_names.contains(&"orders"));
    }

    #[test]
    fn test_sql_for_populate_union_mixed_conditions() {
        // Test UNION where same table appears with and without WHERE clause
        // This should drop ALL conditions to ensure we get all rows
        let schema = create_test_schema();

        let select = parse_select(
            "SELECT * FROM customers WHERE id > 10
             UNION ALL
             SELECT * FROM customers",
        );

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();

        let view = IncrementalView::new(
            "union_view".to_string(),
            select.clone(),
            tables,
            aliases,
            qualified_names,
            table_conditions,
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1, // main_data_root
            2, // internal_state_root
            3, // internal_state_index_root
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 1);
        // When the same table appears with and without WHERE conditions in a UNION,
        // we must fetch ALL rows (no WHERE clause) because the conditions are incompatible
        assert_eq!(
            queries[0], "SELECT * FROM customers",
            "UNION with mixed conditions (some with WHERE, some without) should fetch ALL rows"
        );
    }

    #[test]
    fn test_extract_tables_with_nested_cte() {
        let schema = create_test_schema();
        let select = parse_select(
            "WITH RECURSIVE customer_hierarchy AS (
                SELECT id, name, 0 as level
                FROM customers
                WHERE id = 1
                UNION ALL
                SELECT c.id, c.name, ch.level + 1
                FROM customers c
                JOIN orders o ON c.id = o.customer_id
                JOIN customer_hierarchy ch ON o.customer_id = ch.id
                WHERE ch.level < 3
            )
            SELECT * FROM customer_hierarchy",
        );

        let (tables, _aliases, _qualified_names, _table_conditions) =
            extract_all_tables(&select, &schema).unwrap();

        // Check that we found the tables referenced in the recursive CTE
        let table_names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();

        // We're finding duplicates because "customers" appears twice in the recursive CTE
        // Let's deduplicate
        let unique_tables: HashSet<&str> = table_names.iter().cloned().collect();
        assert_eq!(unique_tables.len(), 2);
        assert!(unique_tables.contains("customers"));
        assert!(unique_tables.contains("orders"));
    }

    #[test]
    fn test_extract_tables_with_cte_and_main_query() {
        let schema = create_test_schema();
        let select = parse_select(
            "WITH customer_stats AS (
                SELECT customer_id, COUNT(*) as order_count
                FROM orders
                GROUP BY customer_id
            )
            SELECT c.name, cs.order_count, p.name as product_name
            FROM customers c
            JOIN customer_stats cs ON c.id = cs.customer_id
            JOIN products p ON p.id = 1",
        );

        let (tables, aliases, _qualified_names, _table_conditions) =
            extract_all_tables(&select, &schema).unwrap();

        // Check that we found tables from both the CTE and the main query
        assert_eq!(tables.len(), 3);
        let table_names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert!(table_names.contains(&"customers"));
        assert!(table_names.contains(&"orders"));
        assert!(table_names.contains(&"products"));

        // Check aliases from main query
        assert_eq!(aliases.get("c"), Some(&"customers".to_string()));
        assert_eq!(aliases.get("p"), Some(&"products".to_string()));
    }

    #[test]
    fn test_sql_for_populate_simple_union() {
        let schema = create_test_schema();
        let select = parse_select(
            "SELECT * FROM orders WHERE total > 1000
             UNION ALL
             SELECT * FROM orders WHERE total < 100",
        );

        let (tables, aliases, qualified_names, table_conditions) =
            extract_all_tables(&select, &schema).unwrap();

        // Generate populate queries
        let queries = IncrementalView::generate_populate_queries(
            &select,
            &tables,
            &aliases,
            &qualified_names,
            &table_conditions,
        )
        .unwrap();

        // We should have deduplicated to a single table
        assert_eq!(tables.len(), 1, "Should have one unique table");
        assert_eq!(tables[0].name, "orders"); // Single table, order doesn't matter

        // Should have collected two conditions
        assert_eq!(table_conditions.get("orders").unwrap().len(), 2);

        // Should combine multiple conditions with OR
        assert_eq!(queries.len(), 1);
        // Conditions are combined with OR
        assert_eq!(
            queries[0],
            "SELECT * FROM orders WHERE (total > 1000) OR (total < 100)"
        );
    }

    #[test]
    fn test_sql_for_populate_with_union_and_filters() {
        let schema = create_test_schema();

        // Test UNION with different WHERE conditions on the same table
        let select = parse_select(
            "SELECT * FROM orders WHERE total > 1000
             UNION ALL
             SELECT * FROM orders WHERE total < 100",
        );

        let view = IncrementalView::from_stmt(
            ast::QualifiedName {
                db_name: None,
                name: ast::Name::exact("test_view".to_string()),
                alias: None,
            },
            select,
            &schema,
            1,
            2,
            3,
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        // We deduplicate tables, so we get 1 query for orders
        assert_eq!(queries.len(), 1);

        // Multiple conditions on the same table are combined with OR
        assert_eq!(
            queries[0],
            "SELECT * FROM orders WHERE (total > 1000) OR (total < 100)"
        );
    }

    #[test]
    fn test_sql_for_populate_with_union_mixed_tables() {
        let schema = create_test_schema();

        // Test UNION with different tables
        let select = parse_select(
            "SELECT id, name FROM customers WHERE id > 10
             UNION ALL
             SELECT customer_id as id, 'Order' as name FROM orders WHERE total > 500",
        );

        let view = IncrementalView::from_stmt(
            ast::QualifiedName {
                db_name: None,
                name: ast::Name::exact("test_view".to_string()),
                alias: None,
            },
            select,
            &schema,
            1,
            2,
            3,
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        assert_eq!(queries.len(), 2, "Should have one query per table");

        // Check that each table gets its appropriate WHERE clause
        let customers_query = queries
            .iter()
            .find(|q| q.contains("FROM customers"))
            .unwrap();
        let orders_query = queries.iter().find(|q| q.contains("FROM orders")).unwrap();

        assert!(customers_query.contains("WHERE id > 10"));
        assert!(orders_query.contains("WHERE total > 500"));
    }

    #[test]
    fn test_sql_for_populate_duplicate_tables_conflicting_filters() {
        // This tests what happens when we have duplicate table references with different filters
        // We need to manually construct a view to simulate what would happen with CTEs
        let schema = create_test_schema();

        // Get the orders table twice (simulating what would happen with CTEs)
        let orders_table = ReferencedTable::from_btree(&schema.get_btree_table("orders").unwrap());

        let referenced_tables = std::vec![orders_table.clone(), orders_table];

        // Create a SELECT that would have conflicting WHERE conditions
        let select = parse_select(
            "SELECT * FROM orders WHERE total > 1000", // This is just for the AST
        );

        let view = IncrementalView::new(
            "test_view".to_string(),
            select.clone(),
            referenced_tables,
            HashMap::default(),
            HashMap::default(),
            HashMap::default(),
            extract_view_columns(&select, &schema).unwrap(),
            &schema,
            1,
            2,
            3,
        )
        .unwrap();

        let queries = view.sql_for_populate().unwrap();

        // With duplicates, we should get 2 identical queries
        assert_eq!(queries.len(), 2);

        // Both should be the same since they're from the same table reference
        assert_eq!(queries[0], queries[1]);
    }

    #[test]
    fn test_table_extraction_with_nested_ctes_complex_conditions() {
        let schema = create_test_schema();
        let select = parse_select(
            "WITH
            customer_orders AS (
                SELECT c.*, o.total
                FROM customers c
                JOIN orders o ON c.id = o.customer_id
                WHERE c.name LIKE 'A%' AND o.total > 100
            ),
            top_customers AS (
                SELECT * FROM customer_orders WHERE total > 500
            )
            SELECT * FROM top_customers",
        );

        // Test table extraction directly without creating a view
        let mut tables = Vec::new();
        let mut aliases = HashMap::default();
        let mut qualified_names = HashMap::default();
        let mut table_conditions = HashMap::default();

        IncrementalView::extract_all_tables(
            &select,
            &schema,
            &mut tables,
            &mut aliases,
            &mut qualified_names,
            &mut table_conditions,
        )
        .unwrap();

        let table_names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();

        // Should have one reference to each table
        assert_eq!(table_names.len(), 2, "Should have 2 table references");
        assert!(table_names.contains(&"customers"));
        assert!(table_names.contains(&"orders"));

        // Check aliases
        assert_eq!(aliases.get("c"), Some(&"customers".to_string()));
        assert_eq!(aliases.get("o"), Some(&"orders".to_string()));
    }

    #[test]
    fn test_union_all_populate_queries() {
        // Test that UNION ALL generates correct populate queries
        let schema = create_test_schema();

        // Create a UNION ALL query that references the same table twice with different WHERE conditions
        let sql = "
            SELECT id, name FROM customers WHERE id < 5
            UNION ALL
            SELECT id, name FROM customers WHERE id > 10
        ";

        let mut parser = Parser::new(sql.as_bytes());
        let cmd = parser.next_cmd().unwrap();
        let select_stmt = match cmd.unwrap() {
            turso_parser::ast::Cmd::Stmt(ast::Stmt::Select(select)) => select,
            _ => panic!("Expected SELECT statement"),
        };

        // Extract tables and conditions
        let (tables, aliases, qualified_names, conditions) =
            extract_all_tables(&select_stmt, &schema).unwrap();

        // Generate populate queries
        let queries = IncrementalView::generate_populate_queries(
            &select_stmt,
            &tables,
            &aliases,
            &qualified_names,
            &conditions,
        )
        .unwrap();

        // Expected query - assuming customers table has INTEGER PRIMARY KEY
        // so we don't need to select rowid separately
        let expected = "SELECT * FROM customers WHERE (id < 5) OR (id > 10)";

        assert_eq!(
            queries.len(),
            1,
            "Should generate exactly 1 query for UNION ALL with same table"
        );
        assert_eq!(queries[0], expected, "Query should match expected format");
    }

    #[test]
    fn test_union_all_different_tables_populate_queries() {
        // Test UNION ALL with different tables
        let schema = create_test_schema();

        let sql = "
            SELECT id, name FROM customers WHERE id < 5
            UNION ALL
            SELECT id, product_name FROM orders WHERE amount > 100
        ";

        let mut parser = Parser::new(sql.as_bytes());
        let cmd = parser.next_cmd().unwrap();
        let select_stmt = match cmd.unwrap() {
            turso_parser::ast::Cmd::Stmt(ast::Stmt::Select(select)) => select,
            _ => panic!("Expected SELECT statement"),
        };

        // Extract tables and conditions
        let (tables, aliases, qualified_names, conditions) =
            extract_all_tables(&select_stmt, &schema).unwrap();

        // Generate populate queries
        let queries = IncrementalView::generate_populate_queries(
            &select_stmt,
            &tables,
            &aliases,
            &qualified_names,
            &conditions,
        )
        .unwrap();

        // Should generate separate queries for each table
        assert_eq!(
            queries.len(),
            2,
            "Should generate 2 queries for different tables"
        );

        // Check we have queries for both tables
        let has_customers = queries.iter().any(|q| q.contains("customers"));
        let has_orders = queries.iter().any(|q| q.contains("orders"));
        assert!(has_customers, "Should have a query for customers table");
        assert!(has_orders, "Should have a query for orders table");

        // Verify the customers query has its WHERE clause
        let customers_query = queries
            .iter()
            .find(|q| q.contains("customers"))
            .expect("Should have customers query");
        assert!(
            customers_query.contains("WHERE"),
            "Customers query should have WHERE clause"
        );
    }
}

/// Tests for concurrent IVM operations.
///
/// These tests verify thread safety of IVM operations under concurrent access.
/// Related to: https://github.com/tursodatabase/turso/issues/1552
#[cfg(all(shuttle, test))]
mod shuttle_ivm_tests {
    use crate::io::MemoryIO;
    use crate::sync::Arc;
    use crate::thread;
    use crate::{Database, DatabaseOpts, OpenFlags};

    fn create_test_db_with_views() -> Arc<Database> {
        let io = Arc::new(MemoryIO::new());
        Database::open_file_with_flags(
            io,
            ":memory:",
            OpenFlags::default(),
            DatabaseOpts::new().with_views(true),
            None,
            Arc::new(crate::dialect::SqliteDialect),
        )
        .unwrap()
    }

    /// Test concurrent INSERTs triggering IVM on the same materialized view.
    ///
    /// This test reproduces the bug described in IVM-CONCURRENCY-BUG.md:
    /// When two concurrent write operations both trigger IVM on the same
    /// materialized view, the B-tree's overflow cell ordering invariant
    /// can be violated.
    ///
    /// The assertion at btree.rs checks that overflow cells are sequential:
    /// ```
    /// turso_assert!(overflow_cell.index + 1 == cell_idx, "multiple overflow cells...")
    /// ```
    ///
    /// This fails when:
    /// 1. Thread A inserts at cell_idx=N, page overflows, creates overflow cell at index N
    /// 2. Thread B inserts at cell_idx=M (where M != N+1), tries to create overflow cell
    /// 3. Assertion fails because M != N+1
    #[test]
    fn concurrent_ivm_inserts_overflow_cell_ordering() {
        shuttle::check_random(
            || {
                let db = create_test_db_with_views();

                // Setup: create table and materialized view
                let setup_conn = db.connect().unwrap();
                setup_conn
                    .execute("CREATE TABLE events (id INTEGER PRIMARY KEY, data TEXT)")
                    .unwrap();
                setup_conn
                    .execute("CREATE MATERIALIZED VIEW events_view AS SELECT id, data FROM events")
                    .unwrap();
                setup_conn.close().unwrap();

                // Spawn multiple threads doing concurrent inserts
                // Each insert triggers IVM on the same materialized view
                // Note: Keep these numbers small for shuttle (deterministic scheduler has limited stack)
                let num_threads = 2;
                let inserts_per_thread = 3;

                let mut handles = vec![];
                for thread_id in 0..num_threads {
                    let db_clone = db.clone();
                    let handle = thread::spawn(move || {
                        let conn = db_clone.connect().unwrap();
                        for i in 0..inserts_per_thread {
                            let id = thread_id * 1000 + i;
                            let sql = format!(
                                "INSERT INTO events (id, data) VALUES ({}, 'thread_{}_item_{}')",
                                id, thread_id, i
                            );
                            // This may fail with BUSY, which is acceptable
                            let _ = conn.execute(&sql);
                        }
                        conn.close().unwrap();
                    });
                    handles.push(handle);
                }

                // Wait for all threads - if the bug manifests, one will panic with:
                // "multiple overflow cells can only occur when a parent overflows..."
                for handle in handles {
                    handle.join().unwrap();
                }

                // Verify data integrity if we get here
                let verify_conn = db.connect().unwrap();
                let mut count = 0i64;
                let mut stmt = verify_conn
                    .query("SELECT COUNT(*) FROM events_view")
                    .unwrap()
                    .unwrap();
                stmt.run_with_row_callback(|row| {
                    count = row.get_value(0).as_int().unwrap();
                    Ok(())
                })
                .unwrap();
                // Some inserts may have failed due to BUSY, but we should have some data
                assert!(count > 0, "Should have inserted some rows");
            },
            100, // Keep iterations low for shuttle's stack limits
        );
    }

    /// Test concurrent INSERTs with a simple view (no aggregation).
    ///
    /// This is a minimal test that should work within shuttle's stack limits.
    #[test]
    fn concurrent_ivm_simple_view() {
        shuttle::check_random(
            || {
                let db = create_test_db_with_views();

                let setup_conn = db.connect().unwrap();
                setup_conn
                    .execute("CREATE TABLE items (id INTEGER PRIMARY KEY, value INTEGER)")
                    .unwrap();
                setup_conn
                    .execute("CREATE MATERIALIZED VIEW items_view AS SELECT id, value FROM items")
                    .unwrap();
                setup_conn.close().unwrap();

                // Minimal workload for shuttle
                let num_threads = 2;
                let inserts_per_thread = 2;

                let mut handles = vec![];
                for thread_id in 0..num_threads {
                    let db_clone = db.clone();
                    let handle = thread::spawn(move || {
                        let conn = db_clone.connect().unwrap();
                        for i in 0..inserts_per_thread {
                            let id = thread_id * 1000 + i;
                            let sql =
                                format!("INSERT INTO items (id, value) VALUES ({}, {})", id, i);
                            let _ = conn.execute(&sql);
                        }
                        conn.close().unwrap();
                    });
                    handles.push(handle);
                }

                for handle in handles {
                    handle.join().unwrap();
                }
            },
            100, // Fewer iterations to avoid stack issues
        );
    }
}

/// Non-shuttle concurrent IVM tests that can run with standard `cargo test`.
///
/// These tests use std::thread directly and are useful for reproducing the
/// IVM concurrency bug without the shuttle infrastructure.
#[cfg(test)]
mod concurrent_ivm_tests {
    use crate::io::MemoryIO;
    use crate::sync::Arc;
    use crate::{Database, DatabaseOpts, OpenFlags};
    use std::thread;

    fn create_test_db_with_views() -> Arc<Database> {
        let io = Arc::new(MemoryIO::new());
        Database::open_file_with_flags(
            io,
            ":memory:",
            OpenFlags::default(),
            DatabaseOpts::new().with_views(true),
            None,
            Arc::new(crate::dialect::SqliteDialect),
        )
        .unwrap()
    }

    /// Test concurrent INSERTs triggering IVM on the same materialized view.
    ///
    /// This test reproduces the bug described in IVM-CONCURRENCY-BUG.md.
    /// Expected behavior:
    /// - In debug builds: panics with "multiple overflow cells can only occur..."
    /// - In release builds: may cause silent data corruption
    ///
    /// If this test passes without panic, the IVM concurrency bug has been fixed.
    #[test]
    #[ignore] // Enable when testing the bug - may panic or hang
    fn test_concurrent_ivm_inserts() {
        let db = create_test_db_with_views();

        // Setup: create table and materialized view
        let setup_conn = db.connect().unwrap();
        setup_conn
            .execute("CREATE TABLE events (id INTEGER PRIMARY KEY, data TEXT)")
            .unwrap();
        setup_conn
            .execute("CREATE MATERIALIZED VIEW events_view AS SELECT id, data FROM events")
            .unwrap();
        setup_conn.close().unwrap();

        // Spawn multiple threads doing concurrent inserts
        let num_threads = 4;
        let inserts_per_thread = 100;

        let mut handles = vec![];
        for thread_id in 0..num_threads {
            let db_clone = db.clone();
            let handle = thread::spawn(move || {
                let conn = db_clone.connect().unwrap();
                for i in 0..inserts_per_thread {
                    let id = thread_id * 1000 + i;
                    let sql = format!(
                        "INSERT INTO events (id, data) VALUES ({id}, 'thread_{thread_id}_item_{i}')"
                    );
                    // Ignore BUSY errors - we're testing concurrent access
                    let _ = conn.execute(&sql);
                }
                conn.close().unwrap();
            });
            handles.push(handle);
        }

        // Wait for all threads
        for handle in handles {
            handle.join().unwrap();
        }

        // Verify we can still query the view
        let verify_conn = db.connect().unwrap();
        let mut count = 0i64;
        let mut stmt = verify_conn
            .query("SELECT COUNT(*) FROM events_view")
            .unwrap()
            .unwrap();
        stmt.run_with_row_callback(|row| {
            count = row.get_value(0).as_int().unwrap();
            Ok(())
        })
        .unwrap();
        assert!(count > 0, "Should have inserted some rows, got {count}");
    }

    /// Stress test with more threads and aggregation to maximize IVM contention.
    #[test]
    #[ignore] // Enable when testing the bug
    fn test_concurrent_ivm_stress() {
        for iteration in 0..10 {
            let db = create_test_db_with_views();

            let setup_conn = db.connect().unwrap();
            setup_conn
                .execute(
                    "CREATE TABLE items (id INTEGER PRIMARY KEY, value INTEGER, category TEXT)",
                )
                .unwrap();
            setup_conn
                .execute(
                    "CREATE MATERIALIZED VIEW items_summary AS \
                     SELECT category, COUNT(*) as cnt FROM items GROUP BY category",
                )
                .unwrap();
            setup_conn.close().unwrap();

            let num_threads = 8;
            let inserts_per_thread = 50;

            let mut handles = vec![];
            for thread_id in 0..num_threads {
                let db_clone = db.clone();
                let handle = thread::spawn(move || {
                    let conn = db_clone.connect().unwrap();
                    for i in 0..inserts_per_thread {
                        let id = thread_id * 1000 + i;
                        let category = format!("cat_{}", thread_id % 3);
                        let value = i * 10;
                        let sql = format!(
                            "INSERT INTO items (id, value, category) VALUES ({id}, {value}, '{category}')"
                        );
                        let _ = conn.execute(&sql);
                    }
                    conn.close().unwrap();
                });
                handles.push(handle);
            }

            for handle in handles {
                handle.join().unwrap();
            }

            println!("Iteration {iteration} completed successfully");
        }
    }
}

/// Resolve a SortedColumn expression to an output column index (0-based).
/// Supports column references by name and column ordinals (e.g., `ORDER BY 2`).
fn resolve_sorted_column_to_index(
    sc: &ast::SortedColumn,
    num_output_cols: usize,
) -> std::result::Result<usize, String> {
    use turso_parser::ast::Expr;

    match sc.expr.as_ref() {
        Expr::Literal(ast::Literal::Numeric(val)) => {
            // Column ordinal: "ORDER BY 2"
            if let Ok(n) = val.parse::<i64>() {
                if n < 1 || n > num_output_cols as i64 {
                    return Err(format!(
                        "column ordinal {} is out of range (view has {} columns)",
                        n, num_output_cols
                    ));
                }
                Ok((n - 1) as usize)
            } else {
                Err(format!("unexpected ORDER BY expression: {val}"))
            }
        }
        Expr::Id(name) => {
            let col_name = crate::util::normalize_ident(name.as_str());
            Err(format!(
                "ORDER BY column reference '{col_name}' is not yet supported; use column ordinals (e.g., ORDER BY 1) instead"
            ))
        }
        Expr::Collate(inner, _) => {
            Err("COLLATE in ORDER BY is not yet supported for materialized views".to_string())
        }
        _ => Err(format!(
            "ORDER BY expression '{}' is not yet supported in materialized views",
            sc.expr
        )),
    }
}

/// Extract a constant integer from a parser expression, if it's a numeric literal.
///
/// Negative integer literals come through as `Unary(Negative, Literal::Numeric(N))`
/// rather than a single signed literal — handle that explicitly so callers can
/// see e.g. `-1` for the SQLite-compat "unlimited" LIMIT sentinel.
fn extract_integer_literal(expr: &ast::Expr) -> Option<i64> {
    use turso_parser::ast::{Expr, UnaryOperator};
    match expr {
        Expr::Literal(ast::Literal::Numeric(val)) => val.parse::<i64>().ok(),
        Expr::Unary(UnaryOperator::Negative, inner) => {
            if let Expr::Literal(ast::Literal::Numeric(val)) = inner.as_ref() {
                val.parse::<i64>().ok().and_then(i64::checked_neg)
            } else {
                None
            }
        }
        Expr::Unary(UnaryOperator::Positive, inner) => {
            if let Expr::Literal(ast::Literal::Numeric(val)) = inner.as_ref() {
                val.parse::<i64>().ok()
            } else {
                None
            }
        }
        _ => None,
    }
}
