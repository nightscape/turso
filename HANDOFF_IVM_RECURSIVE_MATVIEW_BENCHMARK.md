# Handoff: Recursive MatView Benchmarks + Bug Fixes

## Goal

1. **Fix the bugs** preventing recursive CTE matviews from working correctly (see Bug section below)
2. **Benchmark** recursive CTE matviews at scale: creation time, query time, CDC propagation time
3. Report numbers at sizes: **100, 1K, 10K, 100K** rows in the source table

## Bugs to Fix First

The benchmarks below will only produce meaningful results once these bugs are resolved. Fix them first, then run benchmarks.

### Bug A: Recursive counter matview produces only 1 row

There's already a test for this: `ivm-recursive-counter-union-all` in `testing/runner/tests/ivm-chained-matview.sqltest`. It likely fails currently.

```sql
CREATE MATERIALIZED VIEW gen AS
WITH RECURSIVE gen(n) AS (
  SELECT 1 UNION ALL SELECT n+1 FROM gen WHERE n < 50
)
SELECT n FROM gen;

SELECT COUNT(*) FROM gen;
-- Expected: 50
-- Actual: 1 (recursive step never fires)
```

This is the simplest reproducer. The recursive CTE works fine as a regular query but not as a matview.

### Bug B: Recursive CTE matview over upstream UNION ALL matview — depth capped at 1

Test exists: `ivm-chained-recursive-cte-over-union-matview` in `ivm-chained-matview.sqltest`. That test has only 2 levels of depth so it may pass. The real issue shows at depth 3+.

Add this test:

```
test ivm-chained-recursive-depth-3 {
    CREATE TABLE items (id TEXT PRIMARY KEY, name TEXT, parent_id TEXT);
    INSERT INTO items VALUES
      ('a', 'Root', NULL),
      ('b', 'Child', 'a'),
      ('c', 'Grandchild', 'b'),
      ('d', 'Great-grandchild', 'c');

    CREATE MATERIALIZED VIEW flat_items AS
    SELECT id, parent_id, name FROM items;

    CREATE MATERIALIZED VIEW tree AS
    WITH RECURSIVE paths AS (
        SELECT id, parent_id, name, '/' || id AS path, 0 AS depth
        FROM flat_items WHERE parent_id IS NULL
        UNION ALL
        SELECT c.id, c.parent_id, c.name, p.path || '/' || c.id, p.depth + 1
        FROM flat_items c
        JOIN paths p ON c.parent_id = p.id
        WHERE p.depth < 20
    )
    SELECT * FROM paths;

    SELECT id, depth FROM tree ORDER BY path;
}
expect {
    a|0
    b|1
    c|2
    d|3
}
```

Observed behavior: only rows at depth 0 and 1 appear. Depth 2+ is silently dropped.

### Bug C: LIKE filter in base case of recursive CTE matview over upstream matview silently drops rows

```
test ivm-chained-recursive-like-base-case {
    CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT, content TEXT);
    INSERT INTO blocks VALUES
      ('b1', 'doc:abc', 'Root block'),
      ('b2', 'b1', 'Child block'),
      ('b3', 'b2', 'Grandchild block');

    CREATE MATERIALIZED VIEW all_blocks AS
    SELECT id, parent_id, content FROM blocks;

    CREATE MATERIALIZED VIEW block_paths AS
    WITH RECURSIVE paths AS (
        SELECT id, parent_id, content, '/' || id AS path, 0 AS depth
        FROM all_blocks WHERE parent_id LIKE 'doc:%'
        UNION ALL
        SELECT c.id, c.parent_id, c.content, p.path || '/' || c.id, p.depth + 1
        FROM all_blocks c
        JOIN paths p ON c.parent_id = p.id
        WHERE p.depth < 20
    )
    SELECT * FROM paths;

    SELECT id, depth FROM block_paths ORDER BY path;
}
expect {
    b1|0
    b2|1
    b3|2
}
```

Observed behavior: 0 rows. The `LIKE 'doc:%'` filter produces no matches even though `SELECT ... FROM all_blocks WHERE parent_id LIKE 'doc:%'` returns rows correctly as a standalone query.

### Bug D: Inline subquery UNION in recursive CTE matview — column resolution fails

```
test ivm-recursive-inline-union-subquery {
    CREATE TABLE t1 (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT);
    CREATE TABLE t2 (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT);
    INSERT INTO t1 VALUES ('a', NULL, 'A'), ('b', 'a', 'B');
    INSERT INTO t2 VALUES ('x', NULL, 'X'), ('y', 'x', 'Y');

    CREATE MATERIALIZED VIEW combined_tree AS
    WITH RECURSIVE paths AS (
        SELECT id, parent_id, name, '/' || id AS path
        FROM (SELECT id, parent_id, name FROM t1 UNION ALL SELECT id, parent_id, name FROM t2) AS src
        WHERE parent_id IS NULL
        UNION ALL
        SELECT c.id, c.parent_id, c.name, p.path || '/' || c.id
        FROM (SELECT id, parent_id, name FROM t1 UNION ALL SELECT id, parent_id, name FROM t2) AS c
        JOIN paths p ON c.parent_id = p.id
    )
    SELECT * FROM paths;

    SELECT id FROM combined_tree ORDER BY path;
}
expect {
    a
    b
    x
    y
}
```

Current error: `Join condition column 'parent_id' not found in either input`. The IVM join resolver can't see through subquery aliases.

## Benchmarks

After fixing the bugs, create a criterion benchmark at `core/benches/matview_benchmark.rs`. Use the same `setup_limbo` / `run_to_completion` pattern from `write_perf_benchmark.rs`.

### What to Measure

For each size N in {100, 1_000, 10_000, 100_000}:

#### 1. MatView Creation Time

Time from `CREATE MATERIALIZED VIEW` to completion, with N rows already in the source table.

```sql
-- Setup: table with N rows, tree depth ~4 (each non-leaf has ~N^(1/4) children)
CREATE TABLE items (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT);
-- ... insert N rows with tree structure ...

-- Benchmark this:
CREATE MATERIALIZED VIEW tree AS
WITH RECURSIVE paths AS (
    SELECT id, parent_id, name, '/' || id AS path, 0 AS depth
    FROM items WHERE parent_id IS NULL
    UNION ALL
    SELECT c.id, c.parent_id, c.name, p.path || '/' || c.id, p.depth + 1
    FROM items c
    JOIN paths p ON c.parent_id = p.id
    WHERE p.depth < 20
)
SELECT * FROM paths;
```

#### 2. MatView Query Time

After matview is created, measure time to:
- `SELECT COUNT(*) FROM tree` (full scan)
- `SELECT * FROM tree WHERE id = ?` (point lookup)
- `SELECT * FROM tree WHERE path LIKE '/root-1/%'` (subtree via path prefix — this is the core query for `from descendants`)
- `SELECT * FROM tree WHERE depth = 0` (filter on computed column)

#### 3. CDC Propagation Time

After matview exists with N rows, measure time for the matview to update after:
- Single INSERT (leaf node)
- Single INSERT (mid-tree node that creates new subtree)
- Single DELETE (leaf)
- Single UPDATE of parent_id (reparent a subtree)

To measure CDC time: time from `INSERT` statement completion to `SELECT COUNT(*) FROM tree` returning the updated count.

#### 4. Chained MatView (UNION + recursive)

Same measurements as above, but with two source tables combined via UNION ALL matview, and the recursive CTE reading from that upstream matview.

```sql
CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT);
CREATE TABLE ext_tasks (id TEXT PRIMARY KEY, parent_id TEXT, name TEXT);
-- ... insert N/2 rows in each ...

CREATE MATERIALIZED VIEW unified AS
SELECT id, parent_id, name FROM blocks
UNION ALL
SELECT id, parent_id, name FROM ext_tasks;

CREATE MATERIALIZED VIEW unified_tree AS
WITH RECURSIVE paths AS (
    SELECT id, parent_id, name, '/' || id AS path, 0 AS depth
    FROM unified WHERE parent_id IS NULL
    UNION ALL
    SELECT c.id, c.parent_id, c.name, p.path || '/' || c.id, p.depth + 1
    FROM unified c
    JOIN paths p ON c.parent_id = p.id
    WHERE p.depth < 20
)
SELECT * FROM paths;
```

Measure creation + query + CDC for this two-layer setup.

### Tree Shape for Benchmarks

Generate a balanced tree with target depth 4. For N items:
- branching_factor = ceil(N^(1/4))  (gives ~4 levels)
- Root nodes: branching_factor items with parent_id = NULL
- Level 1: branching_factor children per root
- Level 2: branching_factor children per level-1 node
- Level 3: remaining items distributed as leaves

For N=100: ~3-4 children per node, depth 4
For N=100K: ~18 children per node, depth 4

### Output Format

Report as a markdown table:

```
| N      | Create (ms) | COUNT(*) (ms) | Point lookup (ms) | Subtree (ms) | INSERT leaf CDC (ms) | Reparent CDC (ms) |
|--------|-------------|---------------|--------------------|--------------|----------------------|-------------------|
| 100    |             |               |                    |              |                      |                   |
| 1,000  |             |               |                    |              |                      |                   |
| 10,000 |             |               |                    |              |                      |                   |
| 100,000|             |               |                    |              |                      |                   |
```

Same table for chained (UNION + recursive) variant.

## Files to Create/Modify

1. **`testing/runner/tests/ivm-chained-matview.sqltest`** — add test cases for Bugs B, C, D above (Bug A tests already exist)
2. **`core/benches/matview_benchmark.rs`** — new criterion benchmark file
3. **`core/Cargo.toml`** — add `[[bench]]` entry for `matview_benchmark`

## Where to Look for Fixes

The IVM implementation is in `core/translate/` — look for how matview queries are compiled and how recursive CTEs are translated to the IVM incremental maintenance plan. The recursive step likely needs to re-evaluate the full fixpoint, not just one iteration, when the source is a matview rather than a table.
