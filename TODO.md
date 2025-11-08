# COMPLETED: Improve View Change Callback API

## Implementation Status

**✅ IMPLEMENTED** - Option 1 (Add Schema to Callback Signature) has been successfully implemented.

### What Was Done

1. **Added `RelationChangeEvent` struct** (`core/types.rs:2563-2573`)
   - Contains `relation_name`, `columns`, and `changes`
   - Provides column names with each batch of changes

2. **Updated callback signature** (`core/lib.rs:2377-2379`)
   - Changed from `Fn(&str, &[DatabaseChange])` to `Fn(&RelationChangeEvent)`
   - Added comprehensive documentation

3. **Implemented column name extraction** (`core/vdbe/mod.rs:864-891`)
   - Queries `IncrementalView.column_schema` when invoking callbacks
   - Extracts column names via `flat_columns()` and filters by `name`
   - Includes fallback to generic `col_N` names if schema unavailable

4. **Updated Rust bindings** (`bindings/rust/src/lib.rs`)
   - Re-exported `RelationChangeEvent`, `DatabaseChange`, `DatabaseChangeType`
   - Updated wrapper to match new signature

5. **Tests passing** (`bindings/rust/tests/integration_tests.rs`)
   - All existing tests updated
   - New test `test_view_change_callback_column_names` verifies column names are correct

### How It Works

When a transaction commits with view changes:
1. For each affected view, the callback invocation code in `core/vdbe/mod.rs:864` queries the view's schema
2. It locks `connection.schema` (RwLock) and then `IncrementalView` (Mutex) to extract column names
3. Column names are passed in `RelationChangeEvent` alongside the changes
4. Fallback mechanism provides generic names if schema is unavailable

### Performance Notes

- Adds two lock acquisitions per callback invocation (RwLock read + Mutex lock)
- Overhead is minimal relative to transaction commit work
- If profiling shows issues, can migrate to a cached approach

---

## Original Problem Statement

The current materialized view change callback provides `DatabaseChange` objects that lack critical metadata:

1. **Missing column names**: Only provides raw `Vec<Value>` without column names
2. **No schema information**: Consumers can't map value indices to column names
3. **Limited metadata**: Only provides view name, not enough context for general use

This makes it difficult to:
- Build proper change events with named fields (currently forced to use `col_0`, `col_1`, etc.)
- Create a unified API that works for both table CDC and materialized view changes
- Support general materialized views (not just simple `SELECT *` views)

## Current API (After Database-Wide Callbacks)

```rust
// Connection API
pub fn set_view_change_callback<F>(&mut self, callback: F) -> CallbackId
where F: Fn(&str, &[DatabaseChange]) + Send + Sync + 'static

// DatabaseChange (from types.rs)
pub struct DatabaseChange {
    pub change_id: i64,
    pub change_time: i64,
    pub change: DatabaseChangeType,  // Insert/Update/Delete with bin_record
    pub table_name: String,          // Actually the view name
    pub id: i64,                     // ROWID in the view
}
```

## Proposed Solution

Enhance the view change callback to provide schema metadata alongside the changes.

### Option 1: Add Schema to Callback Signature (Recommended)

```rust
// New callback signature with schema metadata
pub fn set_relation_change_callback<F>(&mut self, callback: F) -> CallbackId
where F: Fn(&RelationChangeEvent) + Send + Sync + 'static

pub struct RelationChangeEvent {
    pub relation_name: String,           // View name
    pub columns: Vec<String>,            // Column names in order
    pub changes: Vec<DatabaseChange>,    // The actual changes
}
```

**Pros**:
- Schema provided with each batch of changes
- No need to query schema separately
- Works for dynamic views

**Cons**:
- Slightly more data passed per callback
- Breaking change to callback signature

### Option 2: Add Method to Query View Schema (only if Option 1 is not feasible)

```rust
// Keep existing callback signature, add schema query
impl Connection {
    pub fn get_view_schema(&self, view_name: &str) -> Result<Vec<ColumnInfo>> { ... }
}

pub struct ColumnInfo {
    pub name: String,
    pub type_name: String,
}
```

**Pros**:
- Non-breaking change
- Schema queried only once when setting up view stream
- Separation of concerns

**Cons**:
- Two-step API (query schema, then get changes)
- Schema might be stale if view definition changes

## Implementation Tasks

### 1. Add Column Schema to ViewTransactionState

**File**: `core/incremental/view.rs`

```rust
pub struct ViewTransactionState {
    // Existing fields...
    table_deltas: RefCell<HashMap<String, Delta>>,
    output_delta: RefCell<Option<Delta>>,

    // NEW: Store column schema
    column_names: Vec<String>,  // Column names from the view definition
}
```

The schema should be captured when the view is created and stored with the state.

### 2. Extract Column Names from View Definition

**File**: `core/incremental/view.rs`

In `IncrementalView::new()` or wherever the view is created from the SQL definition:
- Parse the SELECT columns from the view definition
- For `SELECT *`, resolve to actual column names from referenced tables
- Store in `ViewTransactionState`

### 3. Pass Column Names to Callback

**File**: `core/vdbe/mod.rs` around line 240-283

Modify the callback invocation to include column names:

```rust
if let Some(callback) = &self.connection.view_change_callback {
    for view_name in views.iter() {
        if let Some(state) = self.connection.view_transaction_states.get(view_name) {
            let column_names = state.get_column_names();  // NEW
            let mut changes = Vec::new();

            if let Some(output_delta) = state.get_output_delta() {
                // Build DatabaseChange objects...
            }

            // NEW: Pass schema with changes
            callback(&RelationChangeEvent {
                relation_name: view_name.clone(),
                columns: column_names,
                changes,
            });
        }
    }
}
```

### 4. Update DatabaseChange Documentation

**File**: `core/types.rs`

Clarify that for view changes:
- `table_name` contains the **view name**, not base table name
- `id` is the **ROWID** in the view's storage (internal SQLite identifier)
- Consumers should use column names from `RelationChangeEvent` to interpret `bin_record`

## Testing Requirements

### Test 1: ROWID Stability
Create a test that verifies ROWID remains stable when other rows are deleted:

```rust
#[test]
fn test_rowid_stability() {
    // Insert rows with id 1, 2, 3 (ROWIDs will be 1, 2, 3)
    // Delete row with id 1
    // Update row with id 2 - ROWID should still be 2
    // Insert new row with id 4 - ROWID should be 4 (not reusing 1)
}
```

### Test 2: Column Names Match Data
```rust
#[test]
fn test_view_column_names() {
    // Create view: SELECT id, value, computed_col FROM ...
    // Set callback and collect column names
    // Verify names match: ["id", "value", "computed_col"]
    // Verify bin_record has 3 values in that order
}
```

### Test 3: SELECT * Column Resolution
```rust
#[test]
fn test_select_star_columns() {
    // Create table with columns: id, name, age
    // Create view: SELECT * FROM table
    // Verify callback provides: ["id", "name", "age"]
}
```

## Files to Modify

1. **core/incremental/view.rs**
   - Add `column_names` field to `ViewTransactionState`
   - Extract column names during view creation
   - Add `get_column_names()` method

2. **core/vdbe/mod.rs**
   - Update callback invocation to pass column schema (lines ~240-283)

3. **core/types.rs**
   - Add `RelationChangeEvent` struct (if using Option 1)
   - Add `ColumnInfo` struct (if using Option 2)
   - Document `DatabaseChange` fields for view context

4. **bindings/rust/src/lib.rs**
   - Update wrapper to expose new API
   - Add `get_view_schema()` if using Option 2

## Alternative: Keep Current API, Fix Downstream

If modifying Turso is too invasive, we could instead:

1. Query view schema once when creating the view stream in `turso.rs`
2. Store column names in a HashMap: `view_name -> Vec<String>`
3. Use stored schema to map indices to names in `parse_row_values()`

This is a **workaround** that works for our use case but doesn't solve the general problem.

## Recommendation

Implement **Option 1** (add schema to callback) because:
- It's the cleanest API for consumers
- Schema is always in sync with the data
- Works for all view types (not just `SELECT *`)
- Minimal performance impact (column names sent once per batch)

Start with the testing requirements to validate ROWID stability and column name extraction, then implement the schema passing.

---

## Context

This TODO was created after discovering that view change callbacks don't provide column names, forcing consumers to use generic `col_0`, `col_1` names. This makes it impossible to create a proper unified API for table CDC and materialized view changes.

Property-based tests in `rusty-knowledge/crates/rusty-knowledge/src/storage/turso_pbt_tests.rs` expect changes like:
```rust
ViewChange {
    relation_name: "test_entity_view",
    change: Insert {
        rowid: 2,
        data: {"id": "a", "value": "xyz"}  // Named columns, not col_0/col_1
    }
}
```

Related commits:
- `7fb1c20e5` - Initial view callback implementation
- Database-wide callbacks implementation (latest)
