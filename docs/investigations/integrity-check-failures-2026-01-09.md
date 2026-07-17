# Integrity Check Failures Investigation

**Date:** 2026-01-09
**Seeds:** 5, 10
**Error:** "Page X: never used"
**Status:** Root cause identified, fix pending

---

## Executive Summary

The differential simulator tests report integrity check failures where pages are marked as "never used" despite being allocated. Investigation revealed this is an **in-memory state corruption** issue, not actual disk corruption. The persisted database files pass SQLite's integrity check.

The bug appears to be related to how `database_size` in the header is tracked across multiple pagers (main database + ephemeral databases) during differential testing with auto-vacuum enabled.

---

## Reproduction

```bash
# Basic reproduction
cargo run -p limbo_sim --release -- -l 5 --differential

# With debug tracing
RUST_LOG=turso_core::storage::pager=debug cargo run -p limbo_sim --release -- -l 5 --differential 2>&1 | grep -E "allocate_page\(database_size|rollback"

# Verify bug happens without REOPEN_DATABASE fault
cargo run -p limbo_sim --release -- -l 5 --differential --disable-reopen-database

# Keep database files for inspection
cargo run -p limbo_sim --release -- -l 5 --differential --keep-files
```

---

## Key Findings

### 1. Bug is NOT related to WAL recovery

Initially suspected the REOPEN_DATABASE fault injection (which simulates crash recovery) was causing the issue. However, the bug reproduces with `--disable-reopen-database`:

```
$ cargo run -p limbo_sim --release -- -l 5 --differential --disable-reopen-database
Error: Integrity Check Failed: Page 9: never used
```

### 2. Persisted database is valid

After running with `--keep-files`, inspecting the saved database shows it's actually fine:

```bash
$ sqlite3 .bugbase/5/diff.db "PRAGMA page_count; PRAGMA freelist_count; PRAGMA integrity_check;"
3
0
ok
```

SQLite reports only 3 pages with 0 freelist pages, and integrity check passes. This proves the **disk state is correct** - the bug is in Turso's in-memory view.

### 3. In-memory header shows wrong database_size

The integrity check reads `database_size` from the in-memory header:

```rust
// core/vdbe/execute.rs:8692-8698
let (freelist_trunk_page, db_size) =
    return_if_io!(with_header(pager, mv_store.as_ref(), program, |header| (
        header.freelist_trunk_page.get(),
        header.database_size.get()  // <-- Returns 9 when it should be 3
    )));
```

The integrity check then iterates pages 2 through `db_size` and reports any that aren't referenced by B-trees or freelist as "never used".

### 4. Page allocation trace shows the pattern

```
DEBUG allocate_page: database_size=1   <-- Ephemeral pager
DEBUG allocate_page: database_size=1   <-- Ephemeral pager
DEBUG allocate_page: database_size=2
DEBUG allocate_page: database_size=3
...
DEBUG allocate_page: database_size=7
DEBUG allocate_page: database_size=8   <-- After this, database_size=9
Error: Integrity Check Failed: Page 9: never used
```

Note the many `database_size=1` entries - these are **ephemeral pagers** (used for sorting, temporary tables) starting fresh. The main database's allocations interleave with these.

### 5. Auto-vacuum is enabled

The simulator creates databases with auto-vacuum:

```rust
// simulator/runner/env.rs:975
let mut db_opts = turso_core::DatabaseOpts::new().with_autovacuum(true);
```

With auto-vacuum enabled:
- Page 1: Database header/schema
- Page 2: First pointer map page
- Page 3+: Actual data pages

The integrity check has special handling for pointer map pages (`core/vdbe/execute.rs:8757-8770`), but there may be an issue with how this interacts with multiple pagers.

---

## Root Cause Hypotheses

### Hypothesis 1: Ephemeral pager interference (MOST LIKELY)

The differential test creates ephemeral pagers for temporary operations. These pagers call `allocate_page()` which logs `database_size=1`.

**Theory:** The logging or some shared state is being affected by ephemeral pager operations, causing the main database's integrity check to see an incorrect `database_size`.

**Evidence:**
- Many `allocate_page(database_size=1)` in trace
- Ephemeral pagers created via `op_open_ephemeral` in `core/vdbe/execute.rs:8230-8392`

### Hypothesis 2: Transaction rollback state leak

When a transaction allocates pages then rolls back:

```rust
// core/storage/pager.rs:4143-4165
pub fn rollback(&self, schema_did_change: bool, connection: &Connection, is_write: bool) {
    self.clear_page_cache(is_write);  // Clears cached pages
    if is_write {
        self.dirty_pages.write().clear();
    }
    self.reset_internal_states();
    // ... but database_size in header might not be restored
}
```

The rollback clears the page cache (which includes the header page), but doesn't explicitly restore `database_size` to its pre-transaction value. The next header read should get the correct value from WAL/disk, but there may be a race condition.

### Hypothesis 3: Multiple connection shared state corruption

The pager is shared across connections:

```rust
// Pager contains shared state including:
// - page_cache: RwLock<PageCache>
// - dirty_pages: RwLock<RoaringBitmap>
// - header accessed via with_header()
```

If connection A allocates pages (modifying shared header), then connection B runs an integrity check before A's rollback is visible, B would see the wrong `database_size`.

### Hypothesis 4: Pointer map page tracking bug

With auto-vacuum, `is_ptrmap_page()` determines which pages are pointer map pages:

```rust
// core/storage/pager.rs:4442-4451
pub fn is_ptrmap_page(db_page_no: u32, page_size: usize) -> bool {
    if db_page_no == 1 { return false; }
    if db_page_no == FIRST_PTRMAP_PAGE_NO { return true; }  // Page 2
    get_ptrmap_page_no_for_db_page(db_page_no, page_size) == db_page_no
}
```

If the integrity check's auto-vacuum mode detection (`pager.get_auto_vacuum_mode()`) returns wrong value, pointer map pages won't be marked as "used".

---

## Code Paths Involved

### Integrity Check
- Entry: `PRAGMA integrity_check`
- Translation: `core/translate/integrity_check.rs`
- VM execution: `core/vdbe/execute.rs:8667-8808` (`op_integrity_ck`)
- State machine: `IntegrityCheckState` with `page_reference` HashMap

### Page Allocation
- Entry: `pager.allocate_page()` or `pager.btree_create()`
- Implementation: `core/storage/pager.rs:3880-4116`
- Key line: `header.database_size = new_db_size.into();` (line 4110)

### Transaction Rollback
- Entry: `pager.rollback_tx()`
- Implementation: `core/storage/pager.rs:2227-2253`
- Calls: `clear_page_cache()`, `rollback()`, `wal.rollback()`

### Ephemeral Pager
- Entry: `op_open_ephemeral` instruction
- Implementation: `core/vdbe/execute.rs:8230-8392`
- Creates new pager with own page cache and header

---

## Recommended Fix Approaches

### Approach 1: Add assertions in integrity check

Add debugging to log what integrity check sees:

```rust
// In op_integrity_ck
tracing::debug!(
    "Integrity check: db_size={}, freelist_trunk={}, auto_vacuum={:?}",
    db_size, freelist_trunk_page, pager.get_auto_vacuum_mode()
);
```

### Approach 2: Verify pager isolation

Ensure ephemeral pagers don't share state with main database pager. Check that:
- Each ephemeral pager has its own header page
- `with_header()` reads from the correct pager's page cache

### Approach 3: Track database_size across rollback

In `rollback()`, explicitly save/restore `database_size`:

```rust
// In open_savepoint():
let db_size = header.database_size.get();
savepoint.db_size.store(db_size, Ordering::SeqCst);

// In rollback_to_savepoint():
header.database_size = savepoint.db_size.load(Ordering::SeqCst).into();
```

### Approach 4: Add page allocation tracking

Add debug assertions to track page lifecycle:

```rust
#[cfg(debug_assertions)]
static ALLOCATED_PAGES: Mutex<HashSet<u32>> = Mutex::new(HashSet::new());

fn allocate_page(...) {
    // ... allocation logic ...
    #[cfg(debug_assertions)]
    ALLOCATED_PAGES.lock().insert(page_id);
}

fn free_page(...) {
    #[cfg(debug_assertions)]
    assert!(ALLOCATED_PAGES.lock().remove(&page_id), "freeing unallocated page");
}
```

---

## Files of Interest

| File | Relevance |
|------|-----------|
| `core/storage/pager.rs` | Page allocation, freelist, rollback, header access |
| `core/vdbe/execute.rs` | Integrity check VM instruction, ephemeral pager creation |
| `core/storage/pager/ptrmap.rs` | Pointer map page handling for auto-vacuum |
| `simulator/runner/differential.rs` | Differential test orchestration |
| `simulator/runner/execution.rs` | `limbo_integrity_check()` function |

---

## Test Artifacts

| Path | Description |
|------|-------------|
| `.bugbase/5/shrunk.sql` | Minimized SQL reproducer (414 lines) |
| `.bugbase/5/runs.json` | Test run metadata |
| `.bugbase/5/diff.db` | Database file at failure (passes SQLite integrity check) |
| `.bugbase/10/` | Similar artifacts for seed 10 |

---

## Related Issues

- The bug may be related to other simulator failures involving multiple connections
- CDC (Change Data Capture) appears in many failing seeds but may be coincidental
- Auto-vacuum pointer map handling is a complex area with potential for subtle bugs

---

## Conclusion

This is a **high-priority bug** because integrity check failures indicate potential data corruption (even if in this case the disk data is fine). The bug appears to be in how the in-memory `database_size` is tracked when multiple pagers are involved.

**Recommended next step:** Add detailed logging in the integrity check to capture exactly what `database_size` value it sees and which pager it's reading from. This will confirm whether the issue is pager confusion or header state corruption.
