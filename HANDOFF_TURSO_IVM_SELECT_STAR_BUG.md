# Turso IVM Bug: SELECT *, derived_columns in Materialized Views

## Bug Summary

When a `CREATE MATERIALIZED VIEW` uses `SELECT *, <derived_expressions> AS <alias>` (i.e., star expansion combined with additional derived columns), IVM:

1. **Drops all columns from `SELECT *`** — only the explicitly aliased derived columns appear in the view schema
2. **Maps derived column values to wrong positional offsets** — the derived columns receive values from the first N columns of the `*` expansion instead of evaluating their expressions

## Minimal Reproduction

```sql
-- Setup
CREATE TABLE items (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    sort_key TEXT NOT NULL DEFAULT 'a0',
    depth INTEGER NOT NULL DEFAULT 0,
    parent_id TEXT,
    properties TEXT NOT NULL DEFAULT '{}'
);

INSERT INTO items (id, name, parent_id, properties) VALUES
    ('item1', 'First Item', 'root', '{"sequence": "2", "color": "red"}'),
    ('item2', 'Second Item', 'root', '{"sequence": "1", "color": "blue"}'),
    ('item3', 'Third Item', 'other', '{"sequence": "3", "color": "green"}');

-- This works correctly as a regular query:
WITH children AS (SELECT * FROM items WHERE parent_id = 'root')
SELECT *,
    json_extract(properties, '$.sequence') AS seq,
    json_extract(properties, '$.color') AS color_val
FROM children;
-- Returns: id, name, sort_key, depth, parent_id, properties, seq="2", color_val="red" (etc.)

-- BUG: Same query as materialized view:
CREATE MATERIALIZED VIEW broken_view AS
WITH children AS (SELECT * FROM items WHERE parent_id = 'root')
SELECT *,
    json_extract(properties, '$.sequence') AS seq,
    json_extract(properties, '$.color') AS color_val
FROM children;

-- Check schema:
PRAGMA table_info(broken_view);
-- ACTUAL: only 2 columns: seq, color_val
-- EXPECTED: 8 columns: id, name, sort_key, depth, parent_id, properties, seq, color_val

-- Check data:
SELECT * FROM broken_view;
-- ACTUAL: seq="item1", color_val="First Item"  (these are id and name, not the json_extract results!)
-- EXPECTED: seq="2", color_val="red"
```

## What's Happening

The DBSP/IVM pipeline processes the `SELECT *` expansion but when building the materialized view schema, it only registers the explicitly named derived columns. The derived columns then get their values from column positions 0, 1, 2, ... of the underlying row instead of from the correct expression evaluation.

In our production case:
- `SELECT *` expands to 15 columns from the `blocks` table
- 4 derived columns are appended: `seq`, `collapse_to`, `ideal_width`, `priority`
- The matview schema only has those 4 columns
- Their values map to columns 0-3 of the `*` expansion: `id`, `parent_id`, `sort_key`, `depth`

## Where to Look

The bug is likely in the matview DDL processing — specifically where the `SELECT` list is analyzed to build the view's column schema. The `*` expansion needs to be resolved before the schema is built.

Relevant code areas (guesses based on the symptom):
- Materialized view creation DDL handler (where it determines the output column list)
- The DBSP graph setup for IVM (where it maps source columns → output columns)
- Star expansion in the query planner when used inside CTE + matview context

## Workaround

Explicitly list all columns instead of using `*`:
```sql
CREATE MATERIALIZED VIEW working_view AS
WITH children AS (SELECT * FROM items WHERE parent_id = 'root')
SELECT id, name, sort_key, depth, parent_id, properties,
    json_extract(properties, '$.sequence') AS seq,
    json_extract(properties, '$.color') AS color_val
FROM children;
```

## Impact

This blocks the Holon app's entire layout rendering. The root layout query uses `from children` (a CTE) with `derive` (which compiles to `SELECT *, derived_expr AS alias`), and the materialized view returns garbage data causing the Flutter frontend to show nothing.

## Environment

- Turso fork (libSQL with IVM/DBSP extensions)
- SQLite compatibility mode
- Bug reproduces with both `WITH ... AS` CTEs and direct `SELECT *, expr FROM table`
