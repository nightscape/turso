# Handoff: IVM "Index points to non-existent table row" + "Freelist trunk page is not loaded"

## Task

Find and fix the root cause of BTree/freelist corruption during IVM (Incremental View Maintenance) cascades. Two distinct panics manifest from what is likely the same underlying pager bug:

1. `core/incremental/persistence.rs:152` — "Index points to non-existent table row"
2. `core/storage/pager.rs:4488` — "Freelist trunk page is not loaded"

## How to Reproduce

The bug is non-deterministic and timing-dependent. It has never been reproduced in isolation in the Turso repo, only via the holon app's PBT (property-based test), which exercises a wider state space per run.

**Best current reproduction method:**

```bash
cd /Users/martin/Workspaces/pkm/holon
cargo test -p holon-integration-tests --test general_e2e_pbt -- general_e2e_pbt_sql_only
```

This runs proptest with stored regression seeds. The regression seed `9f86ffcce358a60a925bb894e527755cf4e75441696844b1a07a17928a99b0ea` has triggered the bug. It does NOT reproduce 100% of the time — the PBT creates a fresh in-memory DB for each run and the bug depends on page layout / allocation timing.

**Observed reproduction rate:** In 5 consecutive runs of the PBT with the stored regression seed, the test failed every time, but with varying symptoms:

- Runs hitting "Index points to non-existent table row" on `set_field` (UPDATE)
- Runs hitting "Freelist trunk page is not loaded | page_id=262" during startup INSERT cascade
- Runs hitting both in the same run

The bug fires ~100% of the time for this seed, but which of the two panics you get varies.

**Existing Turso-repo tests that attempt to reproduce but DON'T trigger the bug:**

- `tests/integration/query_processing/test_ivm_join_cursor_corruption.rs` — passes
- `tests/integration/query_processing/test_ivm_dirty_pages.rs` — passes

These tests have the right schema shape but lack the exact data volume + operation interleaving that the holon PBT produces. The holon PBT runs 3–20 random transitions including DDL/DML interleaving, pre-startup file sync, todoist DDL race, and CDC callbacks — all of which increase IVM cascade pressure.

## What the Triggering Sequence Does

The PBT regression seed does this:

1. **Pre-startup**: Write 4+ org files to disk (creates ~14-28 blocks worth of data)
2. **StartApp** (all on a single actor connection):
   - CREATE TABLE block, events, navigation_history, navigation_cursor, etc.
   - CREATE MATERIALIZED VIEW blocks_with_paths (recursive CTE + INNER JOIN)
   - CREATE MATERIALIZED VIEW current_focus (JOIN navigation_cursor ↔ navigation_history)
   - CREATE MATERIALIZED VIEW events_view_block (filter on events)
   - CREATE MATERIALIZED VIEW watch_view_* (dynamically, JOINs with current_focus — **chained matviews**)
   - File watcher syncs org files → bulk INSERT into blocks table → IVM cascades through ALL matviews
   - Todoist module concurrently does DDL (CREATE TABLE todoist_tasks + matview) while syncing
   - CDC callbacks registered and firing throughout
3. **After startup**: A single `UPDATE blocks SET content = '...' WHERE id = '...'` triggers the crash

The critical factor is the **chained matview dependency** (watch_view → current_focus → navigation tables), combined with the recursive CTE matview (blocks_with_paths), combined with CDC callbacks, all getting hit by cascading IVM updates from a single write.

## Schema (Exact Production Shape)

```sql
-- Base tables
CREATE TABLE block (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    document_id TEXT,
    content TEXT NOT NULL DEFAULT '',
    content_type TEXT NOT NULL DEFAULT 'text',
    source_language TEXT,
    properties TEXT,
    created_at INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_block_parent_id ON block(parent_id);
CREATE INDEX idx_block_document_id ON block(document_id);

CREATE TABLE events (
    id TEXT PRIMARY KEY,
    event_type TEXT NOT NULL,
    aggregate_type TEXT NOT NULL,
    aggregate_id TEXT NOT NULL,
    origin TEXT NOT NULL,
    status TEXT DEFAULT 'confirmed',
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    processed_by_loro INTEGER DEFAULT 0
);

CREATE TABLE navigation_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    region TEXT NOT NULL,
    block_id TEXT
);
CREATE TABLE navigation_cursor (
    region TEXT PRIMARY KEY,
    history_id INTEGER REFERENCES navigation_history(id)
);
INSERT INTO navigation_cursor (region, history_id) VALUES ('main', NULL);

-- Level 1 matviews (depend only on base tables)
CREATE MATERIALIZED VIEW blocks_with_paths AS
WITH RECURSIVE paths AS (
    SELECT id, parent_id, content, content_type, source_language, properties,
           '/' || id as path
    FROM block WHERE parent_id LIKE 'doc:%'
    UNION ALL
    SELECT b.id, b.parent_id, b.content, b.content_type, b.source_language,
           b.properties, p.path || '/' || b.id as path
    FROM block b INNER JOIN paths p ON b.parent_id = p.id
) SELECT * FROM paths;

CREATE MATERIALIZED VIEW events_view_block AS
SELECT * FROM events WHERE status = 'confirmed' AND aggregate_type = 'block';

CREATE MATERIALIZED VIEW current_focus AS
SELECT nc.region, nh.block_id
FROM navigation_cursor nc JOIN navigation_history nh ON nc.history_id = nh.id;

-- Level 2 matviews (CHAINED: depend on current_focus matview)
-- These are created dynamically during startup and are the critical ingredient
CREATE MATERIALIZED VIEW watch_view_main AS
SELECT b.id, b.content FROM block b
INNER JOIN current_focus cf ON b.parent_id = cf.block_id
WHERE cf.region = 'main';
```

## Crash Paths

### Path 1: "Index points to non-existent table row"
```
UPDATE block SET content = '...' WHERE id = '...'
  → commit_txn
    → apply_view_deltas
      → IncrementalView::merge_delta
        → ReadRow::read_row (persistence.rs:146-154)
          → index_cursor.seek → finds rowid
          → table_cursor.seek(rowid) → SeekResult NOT Found
          → ERROR: "Index points to non-existent table row"
```

The BTree index for the matview's internal DBSP state table has a rowid that no longer exists in the table. The index is stale/corrupt.

### Path 2: "Freelist trunk page is not loaded"
```
INSERT INTO block VALUES (...)
  → commit_txn → apply_view_deltas → ... → pager needs to allocate a page
    → AllocatePageState::SearchAvailableFreeListLeaf
      → trunk_page.is_loaded() == false
      → PANIC at pager.rs:4488
```

The freelist trunk page was evicted from the page cache between being pinned and being accessed. Suggests a pin/unpin lifecycle issue during deep IVM cascades.

## Hypotheses (Ordered by Probability)

1. **Page eviction during IVM cascade** (HIGH): During deeply nested `execute_node` calls for chained matviews, the pager's page cache evicts pages that are still in use by outer IVM operators. The recursive nature of the IVM cascade (blocks_with_paths recurse + chained watch_view → current_focus) creates deep call stacks where multiple BTree cursors are active simultaneously. If the page cache has a size limit, inner operations can evict pages needed by outer operations.

2. **Cursor sharing across IVM levels** (MEDIUM): BTree cursors from one matview's commit are accessed by a different matview's IVM operator during the cascade. The `apply_view_deltas` function collects all transitively dependent views and processes them in BFS order, but cursor state from one node's processing may leak to another.

3. **Write-read interference in single-connection IVM** (MEDIUM): IVM computes deltas by reading old state and comparing with new state. On a single connection, the write transaction's dirty pages are visible to the read sub-operations. If the read path encounters a dirty page that was freed/reallocated by the write path (e.g., a BTree split), it reads stale data.

## Investigation Strategy

1. **Add tracing to persistence.rs:150**: When `SeekResult::Found` fails, log the rowid, the table name, and dump the index cursor's current state. This will tell us whether the rowid is garbage or a valid-but-deleted rowid.

2. **Add tracing to pager.rs:4486**: Log the trunk page's pin count, last access time, and the page cache size when the assertion fires. This will tell us if it's an eviction race.

3. **Run the holon PBT with `RUST_BACKTRACE=1`**: The backtrace will show the exact IVM cascade depth and which matview operations are on the stack when the panic fires.

4. **Instrument `apply_view_deltas`**: Log the order of matview processing, the number of BTree cursor operations per view, and the page cache hit/miss rate. The bug likely correlates with high cascade depth or specific view ordering.

## Files to Focus On

- `core/incremental/persistence.rs` — ReadRow/WriteRow, where index↔table consistency is checked
- `core/storage/pager.rs` — Page allocation, freelist management, pin/unpin lifecycle
- `core/storage/btree.rs` — BTree cursor page stack (PageStack::current, push, clear)
- `core/incremental/compiler.rs` — `execute_node` recursion, `apply_view_deltas` BFS ordering
- `core/incremental/join_operator.rs` — JoinOperator commit/state machine, the `Invalid` sentinel

## Related Prior Work

- `HANDOFF_TURSO_IVM_JOIN_PANIC.md` in holon repo — documents a related `current_page=-1` BTree corruption in JoinOperator that was discovered earlier. Same schema pattern, same chained matview dependency. That bug manifested as `PageStack::current` panic; the bugs in this handoff manifest as index corruption and freelist corruption. All three are likely the same underlying pager issue.
- `tests/integration/query_processing/test_ivm_join_cursor_corruption.rs` — existing test (passes)
- `tests/integration/query_processing/test_ivm_dirty_pages.rs` — existing test (passes)
- Holon examples: `turso_ivm_dirty_pages_repro.rs`, `turso_ivm_register_mismatch_repro.rs`
