# Turso IVM Bug: Recursive CTE + External JOIN Drops Rows in Batch Transactions

## Bug Description

Materialized views using recursive CTEs combined with external JOINs produce fewer rows than the equivalent direct query **when base table rows are inserted via batch transactions (BEGIN/COMMIT)**. The IVM incremental delta computation is lossy — it silently drops rows.

## Root Cause Analysis (Corrected)

### NOT Concurrency

The application has a **fully serialized database actor** — all SQL (reads, writes, DDL) flows through a single `tokio::mpsc` channel processed sequentially on ONE connection. No concurrent database access is possible. The original "concurrency" diagnosis was incorrect.

### The Actual Trigger: Batch Transactions

The application inserts rows via explicit transactions:

```
BEGIN TRANSACTION
  INSERT INTO block (...) VALUES (...);   -- row 1
  INSERT INTO block (...) VALUES (...);   -- row 2
  ... (50-200 rows per batch)
COMMIT
```

IVM processes the batch delta at COMMIT time. For **recursive CTE + external JOIN** matviews, this batch delta computation is lossy — some rows are dropped from the matview.

### Why Sequential Replay Doesn't Reproduce

The SQL trace file (`/tmp/replay.sql`) was extracted from application tracing logs. Two critical issues:

1. **Transaction boundaries are lost**: The trace captures individual `INSERT` statements (tagged `transaction_stmt`) but NOT the `BEGIN TRANSACTION`/`COMMIT` wrapper. The replay executes each INSERT as an individual auto-committed statement. IVM processes one row at a time → works correctly.

2. **Different session state**: The trace was captured from a warm restart where blocks were already in the database. The bug occurs during cold starts when blocks are being bulk-inserted for the first time.

Evidence: 13,191 `transaction_stmt` entries in the trace, zero `BEGIN`/`COMMIT` statements. Batches of ~1000 statements share the same sub-second timestamp (proof they were in the same transaction).

## The Query

```sql
CREATE MATERIALIZED VIEW watch_view_eb3125ab79aead8f AS
WITH RECURSIVE _vl2 AS (
  SELECT _v1.id AS node_id, _v1.id AS source_id, 0 AS depth,
         CAST(_v1.id AS TEXT) AS visited
  FROM block AS _v1
  UNION ALL
  SELECT _fk.id, _vl2.source_id, _vl2.depth + 1,
         _vl2.visited || ',' || CAST(_fk.id AS TEXT)
  FROM _vl2
  JOIN block _fk ON _fk.parent_id = _vl2.node_id
  WHERE _vl2.depth < 20
    AND ',' || _vl2.visited || ',' NOT LIKE '%,' || CAST(_fk.id AS TEXT) || ',%'
)
SELECT _v3.*, json_extract(_v3."properties", '$.sequence') AS "sequence",
       'focus_roots' AS entity_name
FROM focus_roots AS _v0
JOIN block AS _v1 ON _v1."id" = _v0."root_id"
JOIN _vl2 ON _vl2.source_id = _v1.id
JOIN block AS _v3 ON _v3.id = _vl2.node_id
WHERE _v0."region" = 'main'
  AND _v3."content_type" <> 'source'
  AND _vl2.depth >= 0 AND _vl2.depth <= 20
```

## Observed Behavior

| Scenario | Row Count |
|----------|-----------|
| Direct query (no matview) | 232 |
| Matview (session 1, batch transactions in live app) | 186 |
| Matview (session 2, batch transactions in live app) | 168 |
| Matview (replay with individual auto-commits) | 232 (correct) |

Row counts vary between sessions because the bug depends on which batch transactions arrive before vs after matview creation.

## Key Observation: Simpler Recursive CTE Works

A simpler recursive CTE matview over the SAME data, under the SAME batch transaction workload, is always consistent:

```sql
CREATE MATERIALIZED VIEW block_with_path AS
WITH RECURSIVE paths AS (
  SELECT id, parent_id, ..., '/' || id AS path, id AS root_id
  FROM block WHERE parent_id LIKE 'doc:%' OR parent_id LIKE 'sentinel:%'
  UNION ALL
  SELECT b.id, b.parent_id, ..., p.path || '/' || b.id, p.root_id
  FROM block b INNER JOIN paths p ON b.parent_id = p.id
)
SELECT * FROM paths;
```

This always produces 266 rows (matview = direct query), even with batch transactions.

## What Differs

| Working (`block_with_path`) | Broken (`watch_view`) |
|---|---|
| Pure recursive CTE, no external joins after | Recursive CTE + JOIN `focus_roots` + JOIN `block` |
| Final SELECT is `SELECT * FROM paths` | Final SELECT joins CTE result with external tables |
| No `visited` cycle-detection string | Uses `visited` concatenation for cycle detection |

## Reproduction Steps

```sql
-- 1. Setup
CREATE TABLE block (
  id TEXT PRIMARY KEY,
  parent_id TEXT NOT NULL,
  content TEXT DEFAULT '',
  content_type TEXT DEFAULT 'text',
  properties TEXT DEFAULT '{}'
);
CREATE TABLE focus_roots (
  region TEXT PRIMARY KEY,
  root_id TEXT NOT NULL
);

-- 2. Insert seed data (a root block + focus_roots entry)
INSERT INTO focus_roots (region, root_id) VALUES ('main', 'block:root');
INSERT INTO block (id, parent_id, content, content_type) VALUES ('block:root', 'doc:1', 'Root', 'text');

-- 3. Create the recursive CTE + external JOIN matview
CREATE MATERIALIZED VIEW watch_view AS
WITH RECURSIVE _vl2 AS (
  SELECT b.id AS node_id, b.id AS source_id, 0 AS depth, CAST(b.id AS TEXT) AS visited
  FROM block AS b
  UNION ALL
  SELECT child.id, _vl2.source_id, _vl2.depth + 1,
         _vl2.visited || ',' || CAST(child.id AS TEXT)
  FROM _vl2
  JOIN block child ON child.parent_id = _vl2.node_id
  WHERE _vl2.depth < 20
    AND ',' || _vl2.visited || ',' NOT LIKE '%,' || CAST(child.id AS TEXT) || ',%'
)
SELECT b2.*
FROM focus_roots AS fr
JOIN block AS b1 ON b1.id = fr.root_id
JOIN _vl2 ON _vl2.source_id = b1.id
JOIN block AS b2 ON b2.id = _vl2.node_id
WHERE fr.region = 'main' AND b2.content_type <> 'source';

-- 4. Insert children in a BATCH TRANSACTION (this triggers the bug)
BEGIN TRANSACTION;
  INSERT INTO block VALUES ('block:child-1', 'block:root', 'Child 1', 'text', '{}');
  INSERT INTO block VALUES ('block:child-2', 'block:root', 'Child 2', 'text', '{}');
  INSERT INTO block VALUES ('block:child-3', 'block:root', 'Child 3', 'text', '{}');
  INSERT INTO block VALUES ('block:grandchild-1', 'block:child-1', 'GC 1', 'text', '{}');
  INSERT INTO block VALUES ('block:grandchild-2', 'block:child-1', 'GC 2', 'text', '{}');
  INSERT INTO block VALUES ('block:grandchild-3', 'block:child-2', 'GC 3', 'text', '{}');
COMMIT;

-- 5. Compare matview vs direct query
SELECT count(*) FROM watch_view;
-- Expected: 7 (root + 3 children + 3 grandchildren)
-- Bug: returns fewer rows

SELECT count(*) FROM (
  -- re-evaluate the matview query directly
  WITH RECURSIVE _vl2 AS (...)
  SELECT ...
);
-- Returns: 7 (correct)
```

**Key**: Step 4 MUST use `BEGIN TRANSACTION`/`COMMIT`. If you insert rows individually (auto-commit), the bug does NOT manifest.

Try varying the batch size (10, 50, 200 rows) and tree depth to find the minimal reproduction.

## Reproduction Test (Rust)

There is a ready-to-run reproduction test at:
```
cargo run --example ivm_batch_repro
```

This runs 7 scenarios (auto-commit vs batch, with/without other matviews, various batch sizes and tree depths) and compares matview row counts against fresh re-evaluations.

## Hypothesis

IVM's incremental delta computation for recursive CTEs has a bug when:
1. Multiple rows are committed in a single transaction, AND
2. The recursive CTE result is JOINed with external tables in the final SELECT

The batch delta likely miscalculates which CTE expansions need to include the newly-inserted rows when they form parent-child chains across the same transaction.

## Environment

- Turso with IVM (local embedded mode, `experimental_materialized_views(true)`)
- ~270 blocks in the `block` table
- 1 row in `focus_roots` (region='main')
- Single-connection, serialized access (no concurrency)

## Acceptance Criteria

- [ ] Materialized view with recursive CTE + external JOINs returns same row count as direct query after batch transaction INSERTs
- [ ] Existing IVM tests pass
- [ ] New test covers: batch INSERT (in transaction) followed by matview consistency check for recursive CTE + external JOIN pattern
