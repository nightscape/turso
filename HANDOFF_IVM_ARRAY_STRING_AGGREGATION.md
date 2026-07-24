# Handoff: IVM Array/String Aggregation in Materialized Views

## Problem

`CREATE MATERIALIZED VIEW` accepts the scalar aggregations
(`COUNT`, `MIN`, `MAX`, `SUM`, `AVG`) but rejects the array/string
aggregations (`json_group_array`, `group_concat`) at view-creation time.
The same aggregation shape works in a regular (non-incremental) `VIEW`,
so this is an IVM operator-graph gap, not a SQL parser/planner gap.

```sql
-- ✅ Works
CREATE MATERIALIZED VIEW spike_tag_summary AS
  SELECT block_id, COUNT(*) AS n, MIN(tag) AS first
  FROM block_tags
  GROUP BY block_id;

-- ❌ Fails at CREATE
CREATE MATERIALIZED VIEW spike_tags_json AS
  SELECT block_id, json_group_array(tag) AS tags
  FROM block_tags
  GROUP BY block_id;

CREATE MATERIALIZED VIEW spike_tags_concat AS
  SELECT block_id, group_concat(tag, ',') AS tags
  FROM block_tags
  GROUP BY block_id;

-- ✅ Works (regular view, non-incremental)
CREATE VIEW spike_block_hydrated AS
  SELECT b.*,
    (SELECT json_group_array(tag) FROM block_tags WHERE block_id = b.id) AS tags
  FROM block b;
```

Spike confirmed against the running engine on 2026-05-04 via
holon-direct.

## Why holon needs this

Holon stores edge-typed fields (e.g., a block's `tags`,
`blocked_by`) in junction tables (`block_tags`, `task_blockers`). The
domain model — `Block` — wants a single row per block with those edges
materialized as JSON-array columns. Today consumers either query the
junction tables manually (which leaks the storage split into every
reader) or read base-table `block` rows that arrive missing the edge
data.

The clean fix in holon is to promote `block` to a materialized view that
LEFT JOINs the junction tables with `json_group_array(...)` aggregation,
then renames the current `block` table to `block_raw`. All consumers
keep reading `block` and start receiving correctly hydrated rows with
zero call-site changes. **That refactor is currently blocked on the IVM
side: without array aggregation, we can't fold N junction rows into one
materialized block row.**

This handoff is the upstream piece. Once it lands, holon's matview
hydration architecture unblocks (tracked as task #3 there).

## Where the work is

`core/incremental/aggregate_operator.rs` — the `AggregateFunction` enum
(line 98) enumerates currently supported aggregations:

```rust
pub enum AggregateFunction {
    Count,
    CountDistinct(usize),
    Sum(usize),
    SumDistinct(usize),
    Avg(usize),
    AvgDistinct(usize),
    Min(usize),
    Max(usize),
}
```

Routing from SQL `AggFunc` to this enum happens at line ~310
(`AggregateFunction::from_aggfunc` or equivalent — the function that
matches `AggFunc::Count | AggFunc::Sum | ...` returns
`Option<AggregateFunction>`). Anything not enumerated returns `None`,
which is what's surfacing the CREATE failure.

New variants to add (minimum viable set):

```rust
GroupConcat { col: usize, separator: String },
GroupConcatDistinct { col: usize, separator: String },
JsonGroupArray(usize),
JsonGroupArrayDistinct(usize),
// Optional, lower priority:
JsonGroupObject { key_col: usize, value_col: usize },
```

## Why these are tractable for IVM

`group_concat` and `json_group_array` are **commutative + invertible
on a multiset** representation, which is what IVM needs:

- The output of the aggregation is a deterministic function of the
  multiset of input values per group (with optional ordering applied at
  emit time).
- Insert delta on group G: increment multiplicity of value `v` in G's
  multiset; re-render the output column for G.
- Delete delta on group G: decrement multiplicity; if it drops to 0,
  remove the entry; re-render.
- DISTINCT variants: track presence/absence, not multiplicity (set, not
  multiset).

This matches the existing IVM commit-state machine for the Min/Max
operators, which already maintain per-group structures and re-emit on
delta. Min/Max use `MinMaxDeltas: HashMap<String, HashMap<(usize,
HashableRow), isize>>` (line 337); array aggregations want a similar
shape but keyed by aggregated-value rather than by row.

### Suggested per-group state

```rust
/// Sorted multiset of aggregated values per group.
/// BTreeMap because we want stable, deterministic output ordering
/// without paying for an explicit sort on every emit.
pub type AggArrayState = HashMap<HashableRow /* group key */,
                                 BTreeMap<Value /* element */, isize /* count */>>;
```

For DISTINCT variants the inner map collapses to `BTreeSet<Value>` and
deltas are presence flips rather than count adjustments.

### NULL handling

SQLite semantics differ between the two functions:
- `group_concat(x, sep)` **skips** NULL elements.
- `json_group_array(x)` **includes** NULL elements (emits `null` in the
  array).

Both must be respected exactly — holon's tests round-trip arrays and
will catch divergence. Mirror the upstream SQLite tokenizer/aggregator
behavior; cross-check against `core/incremental/aggregate_operator.rs`'s
existing Avg/Sum NULL handling for the project's local conventions.

### Empty groups

- `group_concat([])` → NULL
- `json_group_array([])` → `'[]'`

For LEFT JOINs that produce a row with no matching junction rows, the
aggregation must still emit a row — with `'[]'` for json_group_array,
NULL for group_concat. This is the holon use case (a block with zero
tags).

### Output rendering

On emit per group:
- `json_group_array`: serialize via the same path
  `core/json/...` uses for `json()` and `json_array()` builtins. The
  existing scalar function will already produce the right escaping; the
  IVM operator just needs to feed it the values in order. The
  `BTreeMap<Value, count>`'s natural iteration order gives stable
  output across runs — even though SQL `json_group_array` doesn't
  guarantee order, stable output is preferable for diffing CDC and for
  test determinism.
- `group_concat`: serialize each value via the existing
  `Value::to_string()` and join with the literal separator. SQLite's
  default separator is `","` when one isn't specified — match that.

## Reproduction

Drop into a Turso REPL or fixture:

```sql
CREATE TABLE t (g INT, v TEXT);
INSERT INTO t VALUES (1, 'a'), (1, 'b'), (2, 'c');

CREATE MATERIALIZED VIEW agg_array AS
  SELECT g, json_group_array(v) AS arr FROM t GROUP BY g;
-- expect: ✅ created
SELECT * FROM agg_array ORDER BY g;
-- expect: 1 | ["a","b"]
--         2 | ["c"]

INSERT INTO t VALUES (1, 'x');
SELECT * FROM agg_array WHERE g = 1;
-- expect: 1 | ["a","b","x"]    (incremental update, no full recompute)

DELETE FROM t WHERE v = 'a';
SELECT * FROM agg_array WHERE g = 1;
-- expect: 1 | ["b","x"]

CREATE MATERIALIZED VIEW agg_concat AS
  SELECT g, group_concat(v, ',') AS s FROM t GROUP BY g;
-- expect: ✅ created
SELECT * FROM agg_concat ORDER BY g;
-- expect: 1 | "b,x"
--         2 | "c"
```

Today line 1 fails:
`Failed to execute raw SQL` from `CREATE MATERIALIZED VIEW`.

## Tests to add

1. **Aggregate-operator unit tests** alongside the existing scalar
   tests: per-aggregate insert/delete/update delta application; verify
   per-group state is correctly maintained and that emitted rows match
   a from-scratch SQL evaluation.
2. **Matview integration tests**: the reproduction scenario above
   wrapped in the existing `tests/integration/` harness. Cover:
   - Single-group insert/delete/update
   - Multi-group with cross-group deletes (group becomes empty → row
     should still emit `'[]'` / NULL respectively if the group still
     exists via a LEFT JOIN on another base table; should disappear if
     the group was only present because of those rows)
   - DISTINCT variants
   - NULL-element handling (skip for group_concat, include for
     json_group_array)
   - Custom separator for group_concat
3. **Holon-side validation hook**: after the implementation lands, the
   holon side has a regression PBT (tracked there as task #2) that
   round-trips Block instances through Turso. That PBT will exercise
   the new aggregations under realistic load — a nice cross-repo smoke
   test once unblocked.

## Out of scope (suggested follow-ups)

- **`json_group_object(key, value)`** for hydrating
  `properties`-style key/value JSON objects. Same operator-graph
  shape, slightly more complex per-group state. Lower priority — holon
  doesn't need it for the immediate matview-hydration unblock.
- **`ORDER BY` clauses inside aggregations** (e.g.,
  `json_group_array(v ORDER BY u)`). Useful but out-of-scope for the
  initial implementation; stable BTreeMap iteration order is enough
  for holon's needs.
- **`window`-clause aggregations**. Different operator class entirely;
  not blocking holon.

## Cross-repo coordination

- Holon side, this work unblocks task #3 ("Spike + decide: promote
  `block` to a matview joining block_raw + block_tags +
  task_blockers"). Reference for the holon-side architecture
  decision: holon-pkm/Projects/Holon/Now.org and the memory file at
  `~/.claude/projects/-Users-martin-Workspaces-pkm-holon/memory/turso_ivm_no_array_aggregation_in_matviews.md`.
- Holon will ship task #5 (CacheBlockReader hydration via correlated-
  subquery view) as the interim production fix while this lands. Once
  this ships, the holon team can replace #5's regular VIEW with the
  new matview-backed `block` and delete the BlockReader-side hydration
  entirely.
- Validation: the holon round-trip PBTs (task #2) are the integration
  test for this work landing successfully — once they pass with the
  block-as-matview architecture, the unblock is real.

## File pointers (current tree)

- IVM aggregate operator: `core/incremental/aggregate_operator.rs:98`
  (enum), `~310` (SQL→IVM dispatch)
- IVM operator graph: `core/incremental/operator.rs`,
  `core/incremental/dbsp.rs`
- Existing matview persistence (state schema for new BTreeMap-backed
  per-group state): `core/incremental/persistence.rs`
- Json builtin reference (for output serialization parity):
  `core/json/...` (jsonb encoder)
- group_concat reference: scalar/aggregate implementation already
  exists for non-IVM SELECT paths — find via `rg 'GroupConcat'` in
  `core/`. Reuse its rendering primitives rather than reimplementing.
