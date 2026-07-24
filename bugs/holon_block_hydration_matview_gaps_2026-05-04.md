# Holon block-hydration matview — ground truth (2026-05-04)

## tl;dr (post-fix, 2026-05-04 PM; G4 added 2026-05-06)

**Holon Task #3 is unblocked.** G0 and G1 both fixed in the latest
update; only G2 (correlated scalar subqueries in matview SELECT)
remains, and it now ships with a self-describing error that points at
the working workaround. G2 doesn't gate any holon work since the dual
LEFT OUTER + GROUP BY shape (which Turso suggests as the workaround)
is exactly what holon will use.

| Gap | Status | Pattern |
|---|---|---|
| ~~**G0**~~ | ✅ **fixed** | dual `LEFT OUTER JOIN` + per-join `json_group_array` + `GROUP BY` — rows with empty junctions on both sides now appear correctly |
| ~~G1~~ | ✅ **fixed** | `json_group_array(DISTINCT col) FILTER (WHERE …)` |
| G2 | Open, non-blocking | Correlated `(SELECT json_group_array(c) FROM j WHERE fk = b.id)` in matview SELECT list. Now rejects with a **clear, actionable** error: *"Correlated scalar subqueries in materialized view SELECT lists are not yet supported by the IVM compiler. Rewrite as a LEFT OUTER JOIN with GROUP BY…"* |
| ~~**G4**~~ | ✅ **fixed in `7cf0a2e68a3a`** | Chained matview reads inside an open transaction missed uncommitted upstream deltas. `apply_view_deltas` ran the topological cascade at COMMIT; the read path didn't, so `focus_roots` saw stale rows until COMMIT (and produced duplicates after). Fix walks transitively-upstream matview names DFS, feeds each `execute_with_uncommitted` with table_deltas + already-computed upstream outputs, injects results into the local circuit input keyed by upstream view name. Cache invalidation now sums `tx_state.len()` across the upstream chain. Found by the differential fuzzer (seed 12160730966503765661, `--batch-probability`); tests `chained-matview-uncommitted-insert-propagates` + `chained-matview-uncommitted-holon-shape` in `matview-on-matview.sqltest`. Repro: `bugs/holon_chained_matview_update_2026-05-06.sql`. |

## How this was verified

Self-contained reproducer in `bugs/holon_block_hydration_repro.sql`.
Run with the freshly built CLI:

```sh
cargo build --release -p turso_cli
target/release/tursodb --experimental-views /tmp/holon_repro.db \
  < bugs/holon_block_hydration_repro.sql
```

Built and run against current `@` (`feat: IVM FILTER (WHERE ...) clause
for aggregates`) — same commit holon's `Cargo.lock` pins as
`044a8c41a86de91158ef271363ba5c9b7680eb26`. Output captured at
`/tmp/holon_repro3.out`.

> **Note on prior misreport**: an earlier draft of this handoff listed
> `case1` (single junction on `task_blockers`) and `case2*` (dual LEFT
> JOIN) as failing. That was based on `holon-direct` MCP "Failed to
> execute raw SQL" responses driven against the live holon DB, which
> turn out to be transient false negatives during concurrent app
> mutations. The CLI repro is authoritative; ignore the earlier draft.

## Working target shape for holon's `block` matview (post-fix)

```sql
CREATE MATERIALIZED VIEW block_hydrated AS
SELECT b.id, b.parent_id, b.content,
  COALESCE(json_group_array(bt.tag)        FILTER (WHERE bt.tag        IS NOT NULL), '[]') AS tags,
  COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by
FROM block b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
GROUP BY b.id, b.parent_id, b.content;
```

Output (6 blocks, 4 tags, 2 blockers, post-fix):

```
block:a | ["review","urgent"] | []
block:b | ["review"]          | []
block:c | ["archived"]        | []
block:d | []                  | ["block:a"]
block:e | []                  | ["block:a"]
block:f | []                  | []          ← present (G0 fixed)
```

CDC verified: after `INSERT INTO block_tags ('block:f','late-add')` and
`DELETE FROM task_blockers WHERE blocked_id='block:d'`:

```
block:d | []           | []   ← preserved through delete (G0 fix)
block:f | ["late-add"] | []   ← reflected through insert
```

## Gap details (historical — kept for context; G0 and G1 are fixed)

### G0 — dual LEFT OUTER + GROUP BY drops rows that match neither junction *(fixed)*

**This is the holon blocker.** Production has thousands of plain-text
blocks that have no tags and no blockers; they would silently vanish
from the hydrated matview.

#### Initial state — `block:f` missing

After the matview is created over the seed data:

```
SELECT id, tags, blocked_by FROM case2b_two_left_agg ORDER BY id;
-- block:a | ["review","urgent"] | []
-- block:b | ["review"]          | []
-- block:c | ["archived"]        | []
-- block:d | []                  | ["block:a"]
-- block:e | []                  | ["block:a"]
-- (block:f absent — has no tags, no blockers)
```

Single-junction matviews (`baseline_one_junction`, `case1_blockers_only`)
correctly include `block:f` with `[]`. So the bug requires both LEFTs
in the same view.

#### CDC — row also drops when its last junction entry is deleted

```sql
DELETE FROM task_blockers WHERE blocked_id = 'block:d';
SELECT id, tags, blocked_by FROM case2b_two_left_agg WHERE id = 'block:d';
-- expected: block:d  []  []
-- actual:   (no rows)
```

Symmetrically: an `INSERT INTO block_tags VALUES ('block:f', 'late-add')`
*does* surface `block:f` in the matview, with the new tag. So the
operator-side state is responding to changes correctly; it just refuses
to materialise rows whose left side has nothing on the right of either
join.

#### Isolation: matview-specific, not a general SQL bug

The same query as a plain `SELECT` against the same data produces all
three rows correctly:

```sql
-- regular SELECT, both forms work fine — block:f appears with NULL/[].
SELECT b.id, bt.tag, tb.blocker_id
FROM block b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id;
-- block:a, urgent, NULL
-- block:b, NULL,   block:a
-- block:f, NULL,   NULL    ← present in regular SELECT

SELECT b.id,
  COALESCE(json_group_array(bt.tag)        FILTER (WHERE bt.tag        IS NOT NULL), '[]'),
  COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]')
FROM block b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
GROUP BY b.id;
-- block:a, ["urgent"], []
-- block:b, [],         ["block:a"]
-- block:f, [],         []          ← present in regular SELECT with aggregation
```

So the regular-SELECT planner preserves both LEFT OUTER row-preservation
guarantees correctly. The bug lives strictly in the IVM operator graph.
The minimal repro is `bugs/holon_block_hydration_g0_isolation.sql`
(same SQL, only 3 blocks); it's directly liftable into a unit test in
`core/incremental/`.

#### Hypothesis

In the IVM operator graph for two LEFT OUTER JOINs over the same left
table feeding a `GROUP BY` aggregator, the row preservation that LEFT
OUTER guarantees is being lost when *both* right-side joins produce
zero matches. Probably the cartesian intermediate is being computed
first, then the `GROUP BY` runs over an empty group key, producing
nothing — instead of emitting one row per left-side key with NULLs.

A SQL-side workaround for holon would be to chain via subquery:

```sql
SELECT b.id,
  ( … aggregate over block_tags    … ) AS tags,
  ( … aggregate over task_blockers … ) AS blocked_by
FROM block b;
```

…but that's exactly G2, which also rejects. So the holon path forward
needs at least one of {G0, G2} fixed.

#### Suggested check

If `case1_blockers_only` produces all 6 rows (which it does — see the
captured run output), the fix likely lives at the dual-join composition
layer rather than in the join operator itself. Possibly somewhere in
how `JoinOperator::commit` interacts with the upstream of the next
`JoinOperator` when the second join is also LEFT OUTER over an
unrelated junction.

### G1 — `json_group_array(DISTINCT col) FILTER (WHERE …)` *(fixed)*

```sql
CREATE MATERIALIZED VIEW case3_distinct AS
SELECT b.id,
  COALESCE(json_group_array(DISTINCT bt.tag) FILTER (WHERE bt.tag IS NOT NULL), '[]') AS tags
FROM block b
LEFT OUTER JOIN block_tags bt ON bt.block_id = b.id
GROUP BY b.id;
-- Error: Parse error: FILTER not supported with JsonGroupArray DISTINCT
--                    in incremental views (v1 limitation)
```

The error message is explicit and self-describing — looks like a known
v1 carve-out, not an unexpected planner failure. Worth confirming
whether `DISTINCT` without `FILTER` works (didn't test); if so, the
fix surface is just FILTER+DISTINCT composition.

### G2 — correlated `json_group_array` subquery in matview SELECT *(open, non-blocking)*

```sql
CREATE MATERIALIZED VIEW case4_correlated_subq AS
SELECT b.id,
  (SELECT json_group_array(tag)        FROM block_tags    WHERE block_id   = b.id) AS tags,
  (SELECT json_group_array(blocker_id) FROM task_blockers WHERE blocked_id = b.id) AS blocked_by
FROM block b;
-- Error: Parse error: Correlated scalar subqueries in materialized view
--                    SELECT lists are not yet supported by the IVM compiler.
--                    Rewrite as a LEFT OUTER JOIN with GROUP BY for the same
--                    hydrated-row semantics, e.g.
--                    `(SELECT json_group_array(t.tag) FROM tags t WHERE t.fk = b.id)`
--                    → `LEFT OUTER JOIN tags t ON t.fk = b.id`
--                    + `json_group_array(t.tag) FILTER (WHERE t.tag IS NOT NULL)`
--                    + `GROUP BY b.id`.
```

The error message now explicitly describes the working workaround,
which is exactly the shape holon will use. No urgency to land G2 — but
if/when it does, the LogicalExpr → AST lowering for
`ScalarSubquery(Aggregate(...))` is the missing piece.

## Reproducer harness

Two SQL files in `bugs/`:

- `holon_block_hydration_g0_isolation.sql` — **smallest G0 repro**:
  3 blocks, 1 tag, 1 blocker. Runs the same dual-LEFT shape as a plain
  `SELECT` (works) and points out the matview-only divergence.
- `holon_block_hydration_repro.sql` — full surface: 6 blocks, 4 tags,
  2 blockers, all five matview cases plus the CDC test:

| Section | Status (post-fix) |
|---|---|
| `BASELINE` (single LEFT, agg, GROUP BY) | ✅ |
| `CASE_1` (single LEFT on `task_blockers`) | ✅ |
| `CASE_2a` (two LEFTs, no agg) | ✅ — `block:f` present |
| `CASE_2b` (two LEFTs + dual `json_group_array` + GROUP BY) | ✅ — **target shape**, `block:f` present, CDC propagates correctly through delete |
| `CASE_3` (DISTINCT + FILTER) | ✅ — G1 fixed |
| `CASE_4` (correlated subquery) | ❌ G2 — open, non-blocking, error message now actionable |

Each section prints a marker row and (where the matview accepted) the
hydrated rows. Easy to bisect or attach to a unit test.

## CDC verification (run, results above)

The repro file appends an INSERT + DELETE pair after creating the
matviews. Captured output for the `case2b` matview after mutation:

```
INSERT INTO block_tags  VALUES ('block:f', 'late-add');
DELETE FROM task_blockers WHERE blocked_id = 'block:d';

SELECT id, tags, blocked_by FROM case2b_two_left_agg
 WHERE id IN ('block:f', 'block:d') ORDER BY id;
-- block:f | ["late-add"] | []     ← appeared (good)
-- (block:d absent)                ← regressed (G0 — was present before delete)
```

So CDC propagation itself works — the matview reacts to junction-table
changes — but it loses any row whose junctions all empty out. That
matches the steady-state behaviour for `block:f`.

## Suggested order of attack (historical)

1. ~~**G0** — dual LEFT OUTER row-drop.~~ ✅ fixed
2. ~~**G1** — DISTINCT + FILTER.~~ ✅ fixed
3. **G2** — correlated `ScalarSubquery(Aggregate(...))` in matview
   SELECT. Open, non-blocking, low-priority — error message now points
   at the working workaround.

## Holon-side next step (back at `holon/`)

G0 is fixed and verified end-to-end (DDL + initial hydration + CDC
through delete) on the same commit holon's `Cargo.lock` will pin once
the workspace updates. Holon can now execute Task #3 from
`devlog/2026-05-04-block-roundtrip-handoff.md`:

1. Rename `block` → `block_raw`.
2. Create `block` as the `case2b` matview (with parent_id, content,
   etc. — see `block_mv_full` in the reproducer).
3. Replace `CacheBlockReader::load_all_blocks_with_hydration`'s
   correlated subquery with `SELECT * FROM block`.
4. Drop the SUT's `live_block_tags` workaround
   (`crates/holon-integration-tests/src/pbt/sut.rs:97-117`).
5. Re-run `turso_block_round_trip_pbt` as the regression gate.

## Chained-matview preflight (added 2026-05-05)

Before promoting `block` to a matview, holon verified that the 20+
existing matviews that do `FROM block` (block_with_path,
task_blocking_edges, watch_view_*) work as matview-on-matview. Result:
**GREEN** — Turso IVM now supports chained matviews for the shapes
holon uses. The `holon_block_hydration_repro.sql` `CHAIN_*` sections
exercise:

| Shape | Status |
|---|---|
| Simple filter on matview (watch_view_* pattern) | ✅ |
| WITH RECURSIVE on matview (block_with_path: unaliased base + b.-prefixed recursive) | ✅ |
| Matview JOIN base table (task_blocking_edges, focus_roots) | ✅ |
| Matview JOIN matview, multi-base + matview alias (focus_roots-style) | ✅ |
| Two-hop CDC: base mutation → block matview → chained matview | ✅ |

### G3 (open, non-blocking) — aliased base + p.id reference in WITH RECURSIVE

One specific shape fails:

```sql
CREATE MATERIALIZED VIEW chain_paths AS
WITH RECURSIVE paths AS (
  SELECT b.id, b.parent_id, '/' || b.id AS path  -- aliased base case projects raw id
  FROM block_mv b WHERE b.parent_id LIKE 'doc:%'
  UNION ALL
  SELECT c.id, c.parent_id, p.path || '/' || c.id
  FROM block_mv c JOIN paths p ON c.parent_id = p.id  -- references p.id
)
SELECT * FROM paths;
-- Error: Parse error: no such column: id
```

Trigger conditions (all three required):
1. Source is a matview (works fine on a base table).
2. Base case `FROM matview AS alias` projects an unrenamed column
   (e.g. `b.id`, not `b.id AS node_id`).
3. Recursive case references that column via the CTE alias (`p.id`).

Workaround: drop the alias on the base case (`FROM block_mv WHERE
parent_id LIKE 'doc:%'`), or rename the projection (`b.id AS node_id`
+ `p.node_id`).

Holon doesn't trigger this in any current matview:
- `block_with_path` uses unaliased base case.
- GQL/PRQL-generated WITH RECURSIVE views all use AS-renames
  (`_v0.id AS node_id`, then `_vl.node_id` in the recursive case).

Worth a Turso ticket for completeness — the LogicalExpr → AST lowering
in the IVM compiler doesn't propagate the projected column through a
table-aliased CTE base case when the source is itself a matview. Low
priority; documented workaround above.

## G4 — chained matview reads inside open transactions miss uncommitted upstream deltas *(fixed in 7cf0a2e68a3a)*

Surfaced 2026-05-06 by holon's gpui PBT, found 2026-05-07 by the
differential fuzzer (seed `12160730966503765661`,
`--batch-probability`). The Apr-2026 "split_block CDC drop" repro
(`crates/holon/src/storage/turso_ivm_split_block_cdc_drop_repro.rs`)
covered UPDATE through *recursive* matviews; this is the parallel gap
for *non-recursive dual-LEFT + GROUP BY* matviews chained downstream
when reads share a transaction with the writes they depend on.

### Pattern

1. `block_raw` is the source table.
2. `block` is a matview: `LEFT JOIN block_tags + LEFT JOIN
   task_blockers + json_group_array(...) FILTER + GROUP BY` (the G0
   target shape; `case2b_two_left_agg` from the original repro).
3. `focus_roots` is a matview that JOINs `block` against an unrelated
   table (matches the prod focus_roots SQL: `JOIN block AS b ON
   b.parent_id = nh.block_id`).
4. Inside one transaction: `INSERT INTO block_raw …` + `UPDATE
   block_raw SET content = '…' WHERE id = X` + `SELECT … FROM
   focus_roots …`.

### Symptom (pre-fix)

`SELECT FROM focus_roots` inside the transaction misses both the
inserted row and the post-UPDATE state of `X`. Holon's PBT issues
all of split_block's writes inside one transaction and queries
focus_roots-bound watchers within the same window; the watchers see
the stale state and the row appears to "disappear". Post-COMMIT, the
matview also produces duplicate rows for in-flight rows because the
cascade re-runs without correctly subtracting prior in-txn outputs.

### Root cause

`apply_view_deltas` ran the topological cascade at COMMIT, but the
read path through `MaterializedViewCursor::ensure_tx_changes_computed`
didn't walk upstream matview deltas. So a `focus_roots` read inside
the txn fed its circuit input only the local `table_deltas`, not the
chain of `block` matview's uncommitted output deltas it depends on.

### Fix (commit `7cf0a2e68a3a`)

`MaterializedViewCursor::ensure_tx_changes_computed` now walks
transitively-upstream matview names (DFS, deepest first), feeds each
upstream's `execute_with_uncommitted` its own `table_deltas` plus the
already-computed upstream outputs, and injects the resulting output
deltas into the local circuit's input keyed by the upstream view
name. Cache invalidation sums `tx_state.len()` across the whole
upstream chain so a recompute fires when any upstream's tx_state
grows even if the local stays empty.

### Verification

`bugs/holon_chained_matview_update_2026-05-06.sql` now wraps the four
splits in `BEGIN/COMMIT`. On the unfixed binary the inside-txn
`SELECT region, root_id FROM focus_roots` returns the initial 2 rows
throughout, never the expected 3, 4, 5, 6; post-COMMIT it returns 9
rows with duplicates. On the fixed binary all expected counts match.
Upstream tests: `chained-matview-uncommitted-insert-propagates` and
`chained-matview-uncommitted-holon-shape` in
`matview-on-matview.sqltest` (both fail before, pass after).

### Holon-side verification

After bumping holon's pin to `7cf0a2e68a3a`, the gpui PBT no longer
emits `[inv1 WARN] Missing in live_blocks: [block:1-4]`, no
`inv-focus-roots WARN`, no `focus_roots mismatch` panic across 20
unseeded cases. The truth-check pattern at
`crates/holon-integration-tests/src/pbt/sut.rs:~3970` stays useful
as a regression gate: it queries the matview directly and downgrades
LiveData mirror lag, but keeps a panic for matview-state divergence.

### Original (autocommit) repro: doesn't trigger

The first iteration of the repro ran each split as its own
autocommit statement. That doesn't fire the bug because every
statement boundary forces the cascade through `apply_view_deltas`.
The bug only appears when reads share a transaction with the writes
they depend on — which the differential fuzzer eventually
discovered via `--batch-probability`.
