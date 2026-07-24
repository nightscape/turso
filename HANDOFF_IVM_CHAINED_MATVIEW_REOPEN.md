# Handoff: IVM Chained Matview Dependency Ordering on DB Reopen

## Problem

When reopening an existing database that contains materialized views with inter-matview dependencies, `Database::open_file_with_flags` fails with:

```
Parse error: Table 'focus_roots' not found in schema
```

This happens because `populate_materialized_views()` in `core/schema.rs:1214` iterates a `HashMap<String, (String, i64)>`, which has **arbitrary iteration order**. If matview B depends on matview A, but B is processed before A, then `IncrementalView::from_sql()` calls `schema.get_btree_table("A")` which returns `None` (A hasn't been registered yet), causing the error at `core/incremental/view.rs:538`.

## Root Cause (Exact Code Path)

1. `core/util.rs:193` calls `schema.populate_materialized_views(materialized_view_info, ...)` where `materialized_view_info` is a `HashMap`
2. `core/schema.rs:1220` iterates the HashMap: `for (view_name, (sql, main_root)) in materialized_view_info`
3. `core/schema.rs:1258` calls `IncrementalView::from_sql(&sql, self, ...)` — `self` is the Schema
4. `core/incremental/view.rs:400` calls `extract_all_tables()` → `process_table_reference()`
5. `core/incremental/view.rs:526`: `schema.get_btree_table(table_name)` returns `None` for a matview that hasn't been processed yet
6. `core/incremental/view.rs:538`: returns `Err(ParseError("Table '{table_name}' not found in schema"))`

## Reproduction Scenario

The dependency chain in the production DB:

```
current_focus (matview)  — depends on: navigation_cursor, navigation_history (regular tables)
     ↓
focus_roots (matview)    — depends on: current_focus (matview!), block (regular table)
     ↓
watch_view_eb3125... (matview) — depends on: focus_roots (matview!), block (regular table)
```

In `sqlite_master`, `focus_roots` (offset 400) appears BEFORE `current_focus` (offset 1165). HashMap iteration doesn't follow row order either, so any permutation can occur.

## Fix

In `populate_materialized_views()` (`core/schema.rs:1213-1293`), replace the single-pass HashMap iteration with multi-pass processing:

1. Collect all matviews into a `Vec`
2. On each pass: try processing each remaining matview. If `from_sql()` fails with "not found in schema", defer it to the next pass. If it succeeds, the matview is now registered as a BTreeTable and available for subsequent views.
3. If a full pass makes zero progress (no new views succeed), return an error — there's a circular dependency or a genuinely missing table.

### Pseudocode

```rust
pub fn populate_materialized_views(&mut self, ...) -> Result<()> {
    let mut pending: Vec<(String, String, i64)> = materialized_view_info
        .into_iter()
        .map(|(name, (sql, root))| (name, sql, root))
        .collect();

    let mut last_count = pending.len() + 1;
    while !pending.is_empty() && pending.len() < last_count {
        last_count = pending.len();
        let mut deferred = Vec::new();
        for (view_name, sql, main_root) in pending {
            // ... existing DBSP state root lookup + index registration ...
            match IncrementalView::from_sql(&sql, self, main_root, dbsp_root, dbsp_idx_root) {
                Ok(incremental_view) => {
                    // ... existing BTreeTable creation + add_materialized_view ...
                }
                Err(e) if e.to_string().contains("not found in schema") => {
                    deferred.push((view_name, sql, main_root));
                }
                Err(e) => return Err(e),
            }
        }
        pending = deferred;
    }

    if !pending.is_empty() {
        // Unresolvable dependencies — return meaningful error
        return Err(...);
    }
    Ok(())
}
```

Note: Extract the per-view body (DBSP lookup + IncrementalView::from_sql + BTreeTable + add_materialized_view + dependency registration) into a helper method to keep the multi-pass loop clean.

## Tests

### 1. sqltest: Reopen DB with chained matviews

The sqltest runner currently only uses `:memory:` databases. This test needs a **file-backed DB** that is closed and reopened. If the sqltest runner doesn't support `@reopen` or `@close`/`@open`, use an integration test instead.

**Test in `testing/runner/tests/ivm-chained-matview.sqltest`** (if runner supports reopen):

```
test ivm-chained-matview-reopen {
    CREATE TABLE base (id INTEGER PRIMARY KEY, val TEXT);
    CREATE TABLE meta (id INTEGER PRIMARY KEY, base_id INTEGER);
    INSERT INTO base VALUES (1, 'a'), (2, 'b');
    INSERT INTO meta VALUES (10, 1);

    CREATE MATERIALIZED VIEW mv_base AS SELECT id, val FROM base;
    CREATE MATERIALIZED VIEW mv_chain AS SELECT m.id, b.val FROM meta m JOIN mv_base b ON m.base_id = b.id;

    SELECT * FROM mv_chain;
}
expect {
    10|a
}
-- @reopen
-- SELECT * FROM mv_chain;
-- expect { 10|a }
```

### 2. Integration test: File-backed reopen with 3-level chain

Add to `tests/integration/query_processing/test_materialized_subquery.rs` or a new file:

```rust
#[test]
fn test_chained_matview_reopen_dependency_order() {
    // Level 0: regular tables
    // Level 1: mv_level1 depends on regular tables only
    // Level 2: mv_level2 depends on mv_level1
    // Level 3: mv_level3 depends on mv_level2
    //
    // Create all, insert data, verify, close DB, reopen, verify again.

    let tmp = TempDatabaseBuilder::new()
        .with_opts(DatabaseOpts::default().with_views(true))
        .with_db_name("chained_matview_reopen.db")
        .build();

    let conn = tmp.connect_limbo();
    // Create schema + matviews + insert data
    // ...
    // Verify queries work
    // ...
    drop(conn);

    // Reopen the same file
    let db2 = TempDatabase::new_with_existent_with_opts(
        &tmp.path,
        DatabaseOpts::default().with_views(true),
    );
    let conn2 = db2.connect_limbo();
    // Verify the same queries still work
}
```

### 3. Unit test: populate_materialized_views with scrambled order

Test that `populate_materialized_views` handles a HashMap where dependent views come before their dependencies. This can be tested by constructing a `materialized_view_info` HashMap with known entries and verifying it doesn't error.

### 4. Edge cases to test

- **Circular dependency**: mv_a depends on mv_b, mv_b depends on mv_a → should produce a clear error, not infinite loop
- **Missing base table**: matview references a table that doesn't exist at all → should fail with clear error
- **Self-referential CTE**: matview with recursive CTE referencing itself → should work (CTEs are excluded from table lookup at `view.rs:525`)
- **Mixed**: some matviews resolve in pass 1, others in pass 2, some in pass 3

## Files to Change

| File | Change |
|------|--------|
| `core/schema.rs:1213-1293` | Multi-pass logic in `populate_materialized_views()` |
| `core/schema.rs` (new helper) | Extract per-view processing into `populate_one_materialized_view()` |
| `testing/runner/tests/ivm-chained-matview.sqltest` | Add reopen test (if runner supports it) |
| `tests/integration/query_processing/` | New integration test for file-backed reopen |

## Verification

After the fix, this command should work on the holon production DB:

```bash
tursodb --experimental-views ~/Library/Application\ Support/space.holon/holon.db "SELECT COUNT(*) FROM focus_roots;"
```

Currently it fails with `Parse error: Table 'focus_roots' not found in schema`.
