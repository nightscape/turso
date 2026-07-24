# Redundant UPDATE on a base table panics IVM with `set_null_flag on unexpected cursor type` (2026-05-07)

**Status:** reproduced on upstream `tursodb` CLI; minimal SQL repro committed at
[`bugs/holon_block_redundant_update_2026-05-07.sql`](holon_block_redundant_update_2026-05-07.sql).
**Pinned revision when reproduced:** `7cf0a2e68a3a` (the chained-matview-read-in-txn fix
landed; this bug is independent and remains).
**Holon symptom:** PBT panic at `crates/holon-integration-tests/src/pbt/sut.rs:4002` —
`Region 'main' focus_roots mismatch after navigation. block:<id>: block_raw=✓ block=✗ focus_roots=false`.

## TL;DR

For the LEFT-OUTER-JOIN + `json_group_array(...) FILTER` + `GROUP BY` matview
shape (case2b_two_left_agg from the existing matview test suite), any
`UPDATE base_table SET col = <same-value>` issued after a `SELECT` against the
matview either:

- **panics** the `tursodb` CLI at `core/types.rs:2773:17` (`set_null_flag on
  unexpected cursor type`), or
- **silently drops the row** from the matview when the same statement is
  dispatched in-process via `turso::Connection::execute` (Holon's path).

The same `UPDATE` issued *before* any matview SELECT does not panic but still
drops the row from the matview's incremental state. A simple matview
(`SELECT id, content FROM block_raw`, no joins, no GROUP BY) handles the same
no-op UPDATE correctly — the bug needs the LEFT JOIN + GROUP BY shape.

## Minimal reproducer

```sql
CREATE TABLE block_raw (
    id TEXT PRIMARY KEY, parent_id TEXT,
    content TEXT NOT NULL DEFAULT '', sort_key TEXT NOT NULL DEFAULT 'A0'
);
CREATE TABLE block_tags    (block_id TEXT, tag TEXT, PRIMARY KEY (block_id, tag));
CREATE TABLE task_blockers (blocked_id TEXT, blocker_id TEXT, PRIMARY KEY (blocked_id, blocker_id));

CREATE MATERIALIZED VIEW block AS
SELECT b.id, b.parent_id, b.content, b.sort_key,
    COALESCE(json_group_array(bt.tag)        FILTER (WHERE bt.tag        IS NOT NULL), '[]') AS tags,
    COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by
FROM block_raw b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
GROUP BY b.id, b.parent_id, b.content, b.sort_key;

INSERT INTO block_raw (id, parent_id, content, sort_key) VALUES ('block:v', 'doc', 'D', 'B0');
SELECT count(*) FROM block;            -- 1
UPDATE block_raw SET content = 'D' WHERE id = 'block:v';
                                       -- ↑ panics: set_null_flag on unexpected cursor type
```

The full self-contained SQL is in
[`bugs/holon_block_redundant_update_2026-05-07.sql`](holon_block_redundant_update_2026-05-07.sql).

## Reproduction

```sh
cd /path/to/turso  # checkout where 7cf0a2e68a3a is reachable
cargo build --release -p turso_cli
rm -f /tmp/redundant_update.db
target/release/tursodb --experimental-views /tmp/redundant_update.db \
    < bugs/holon_block_redundant_update_2026-05-07.sql
```

Expected output:
```
thread 'main' panicked at core/types.rs:2773:17:
set_null_flag on unexpected cursor type
```

To see the silent in-process variant (no panic, just data loss):

```sh
cd /path/to/holon
cargo build --release -p holon-tools --bin turso-sql-replay
./target/release/turso-sql-replay replay devlog/2026-05-07-turso-ivm-focus-roots-minimal.sql \
    --check-after-each --no-break-on-inconsistency
```
Exits non-zero with `INCONSISTENCY in block: matview=0, fresh=1, missing=1`.

## Why this matters

This shape — `LEFT OUTER JOIN <junction>` × N + `json_group_array(... ) FILTER`
+ `GROUP BY <every base column>` — is the canonical Turso-recommended workaround
for hydrating an entity table with edge data, after the correlated-scalar
`json_group_array(...)` matview rejection (G2 in
`bugs/holon_block_hydration_matview_gaps_2026-05-04.md`). Holon adopted exactly
this pattern across all read-path code, so any application that ever issues a
no-op UPDATE on a base table — common in any "save unchanged" flow, idempotent
sync, or value-equal Loro reconcile — will silently lose matview rows or panic.

## Trigger matrix (observed against `7cf0a2e68a3a`)

The base sequence in every row is: `INSERT base` → `SELECT count(*) FROM mv` → `UPDATE base SET content = <same-value>` → `SELECT count(*) FROM mv`.

| Matview shape                                                             | Outcome              |
| ------------------------------------------------------------------------- | -------------------- |
| no JOIN: `SELECT id, content FROM base`                                   | OK (mv=1)            |
| no JOIN, GROUP BY: `SELECT id, content, count(*) FROM base GROUP BY ...`  | OK (mv=1)            |
| INNER JOIN to junction (junction row present)                             | OK (mv=1)            |
| **LEFT OUTER JOIN, junction PK = `(base_id)`, project base cols only**    | silent drop (mv=0)   |
| **LEFT OUTER JOIN, junction PK = `(base_id)`, project junction col**      | silent drop (mv=0)   |
| **LEFT OUTER JOIN, junction PK = `(base_id, tag)`, project junction col** | **panic** at 2773    |
| **LEFT OUTER JOIN + GROUP BY + `count(j.col)`**                           | **panic** at 2773    |
| **LEFT OUTER JOIN + GROUP BY + `json_group_array` (FILTER or not)**       | **panic** at 2773    |
| **Dual LEFT OUTER JOIN + GROUP BY + 2× `json_group_array` FILTER (the holon shape)** | **panic** at 2773 |

Common root: a matview with at least one `LEFT OUTER JOIN` is corrupted by an
`UPDATE` that does not change a projected base column. The panic-vs-silent-drop
split correlates with junction PK shape and/or aggregate presence — happy to
narrow further if useful for triage, but the simpler "silent drop" case is
already enough to lose data in production.

The trigger is specifically a *value-equal* UPDATE. `UPDATE base SET col =
<same-value>` corrupts the matview whether `col` is projected or not. `UPDATE
base SET col = <new-value>` works correctly. So the bug isn't about
"any UPDATE"; it's about the IVM mishandling a delta whose old and new
projections are identical.

## Suspected mechanism

The IVM compiler emits a delta for the no-op UPDATE that, after passing through
the LEFT-OUTER-JOIN legs and the `FILTER`-d aggregate, produces a
`(-1 group, +0 group)` — i.e. negative without a matching positive — which the
GROUP BY operator interprets as "delete the group" and the matview cursor for
the join-leg side ends up in a variant the `set_null_flag` dispatch in
`core/types.rs:2767` doesn't handle (cf. `panic!("set_null_flag on unexpected
cursor type")` at line 2773).

The previously-fixed `set_null_flag on unexpected cursor type` panic
(commit `81cef68c`) was on a related but different code path — that one was
reproducible without UPDATEs at all. This one needs an UPDATE that changes no
projected value.

## Notes for triage

- Reproduces under autocommit; no transaction needed.
- Reproduces with both junctions empty (no `block_tags` / `task_blockers` rows).
- Doesn't require chained matviews — `block` is the root.
- Doesn't require recursion (cf. `tui_split_block_cdc_drop`); plain JOIN.
- An `UPDATE` that *does* change a projected value works correctly.
- Removing either `FILTER (WHERE ... IS NOT NULL)` or the dual-LEFT shape
  hasn't been tested — narrowing to the smallest-required schema would be a
  good follow-up if this isn't immediately diagnosable from the existing repro.
