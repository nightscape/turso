# Differential Fuzzer Enhancement: Matview Coverage Gaps

## Context

A bug was found where a recursive CTE materialized view reading from an upstream UNION ALL materialized view crashes with an index-out-of-bounds panic (`core/vdbe/execute.rs:1796`). The differential fuzzer with `--matview -g sql-gen-prop` cannot catch this class of bug due to three independent gaps in SQL generation.

Reproducer: `testing/runner/tests/ivm-chained-matview.sqltest` — test `ivm-chained-recursive-cte-over-union-matview-star`.

## Gap 1: No UNION ALL in matview SELECTs

`sql_gen_prop/view.rs` — `ViewSelectKind` only supports:
- `Star` — `SELECT * FROM table`
- `FilteredColumns` — `SELECT col1, col2 FROM table WHERE expr`
- `Aggregate` — `SELECT col, COUNT(*) ... GROUP BY col`
- `Join` — `SELECT t1.col, t2.col FROM t1 JOIN t2 ON ...`
- `QualifiedStarJoin` — `SELECT t1.* FROM t1 JOIN t2 ON ... WHERE ...`

Missing: a `UnionAll` variant that generates `SELECT ... FROM t1 UNION ALL SELECT ... FROM t2`.

This also requires `SelectStatement` in `sql_gen_prop/select.rs` to support compound queries — currently it only represents single-table SELECTs.

## Gap 2: No recursive CTEs

`sql_gen_prop/cte.rs` line 4 explicitly states: "Only non-recursive CTEs are supported."

The `WITH RECURSIVE ... UNION ALL` pattern that walks a tree is never generated. Recursive CTEs are also disabled for nested queries (line 318 — `cte_profile.disabled()`).

## Gap 3: No matview-to-matview references

`sql_gen_prop/view.rs` `create_view_inner()` line 135 selects source tables from `schema.tables` only. It never reads from `schema.materialized_views` or `schema.views`.

Matview names are tracked in `fuzzer/generate.rs` (line 254) but never fed back into the view generator as possible source relations.

## What Would Need to Change

All three gaps must be closed to catch this specific bug class:

1. **Add `UnionAll` to `ViewSelectKind`** — Generate `SELECT cols FROM t1 UNION ALL SELECT cols FROM t2` with matching column counts/types. Extend `SelectStatement` to support compound queries.

2. **Add recursive CTE generation** — Generate `WITH RECURSIVE tree AS (base UNION ALL recursive_step) SELECT ... FROM tree`. The base case selects from a table, the recursive step joins `tree` back to a table with a depth guard.

3. **Allow matviews as source relations** — In `create_view_inner()`, sample from both `schema.tables` and `schema.materialized_views` when picking source relations. This enables chained matviews that trigger CDC propagation bugs.

## Priority

Gap 3 (chained matviews) is the highest-value change — it's the simplest to implement and would catch CDC propagation bugs between matviews even without UNION ALL or recursive CTEs. Gap 1 (UNION ALL) is medium effort. Gap 2 (recursive CTEs) is the most complex but also targets the most interesting IVM bugs.
