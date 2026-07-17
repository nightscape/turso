# Confirming repro: `json_group_array multiset went negative` (2026-05-15)

Fresh capture of the multiset-negative panic on Turso pin
`a01c27da5841f70ead56e0366e312bbd2a754422` (branch `holon` on
`nightscape/turso`). Same root cause as
[`json_group_array_multiset_negative.md`](json_group_array_multiset_negative.md) —
this is a new on-disk trace from a real Holon session, not a regression
of a fixed bug.

## Panic

```
thread 'tokio-rt-worker' panicked at
core/incremental/aggregate_operator.rs:1482:29:
json_group_array multiset went negative for col 19
val Text("block:edge-field-descriptor")
— delta consolidation invariant violated
```

Second panic shape that fired ~5 seconds later in the same session:

```
thread 'tokio-rt-worker' panicked at
core/incremental/aggregate_operator.rs:1482:29:
json_group_array multiset went negative for col 17
val Text("edge-abstraction")
```

Followed by a cascade of `Reached invalid state! State was replaced,
and not replaced back` at `aggregate_operator.rs:2213:21` on every
subsequent command against the wedged matview (10+ occurrences).
Holon's `TursoBackend::Actor` catches via `catch_unwind` so the
process survives, but the matview's IVM state is poisoned for the
rest of the session.

## Affected matview

`block` (definition in trace, ~line 247):

```sql
CREATE MATERIALIZED VIEW block AS
SELECT
    b.id, b.parent_id, b.depth, b.sort_key, b.content, b.content_type,
    b.source_language, b.source_name, b.properties, b.marks,
    b.collapsed, b.completed, b.block_type, b.created_at, b.updated_at,
    b._change_origin,
    COALESCE(json_group_array(bt.tag)         FILTER (WHERE bt.tag         IS NOT NULL), '[]') AS tags,
    COALESCE(json_group_array(br.required_id) FILTER (WHERE br.required_id IS NOT NULL), '[]') AS requires
FROM block_raw b
LEFT OUTER JOIN block_tags     bt ON bt.block_id = b.id
LEFT OUTER JOIN block_requires br ON br.block_id = b.id
GROUP BY b.id, ...;
```

Col 19 = `requires` aggregate (`json_group_array(br.required_id)`).
Col 17 = `tags` aggregate (`json_group_array(bt.tag)`).
Both `json_group_array` aggregates are vulnerable; the bug is per-aggregate.

## Trigger

Holon's org-file watcher re-parses an org file and, per block, emits
the canonical "replace junction rows" pattern:

```sql
BEGIN TRANSACTION;          -- 149 stmts in this batch
...
DELETE FROM block_requires WHERE "block_id" = 'block:sql-provider-edge-partition';
INSERT INTO block_requires ("block_id", "required_id")
   VALUES ('block:sql-provider-edge-partition', 'block:edge-field-descriptor');
...
COMMIT;
```

Important timing detail: **the panic fires ~106 ms AFTER the
COMMIT**, on a tokio worker thread.

```
15:41:44.338096  actor_tx_begin: BEGIN TRANSACTION (149 stmts)
15:41:44.361795  actor_tx_commit: COMMIT
15:41:44.468396  PANIC: json_group_array multiset went negative
```

So the aggregate consolidation that goes negative is the **downstream
matview update applied to the just-committed delta batch**, not
in-transaction processing. This is why a single-threaded sync replay
using `turso-sql-replay` does not reproduce it (see below).

## Repro asset

- **`holon_json_group_array_multiset_negative_2026-05-15.sql`** —
  4,108 statements (1.9 MB), full DDL + data + writes leading up to
  the panic. Extracted from `HOLON_TRACE_SQL=1` log via
  `tools/turso-sql-replay extract --stop-pattern "json_group_array
  multiset went negative" --dedup-ddl`.

### Replay verdict

Replaying through `tools/turso-sql-replay replay <file>` against the
same Turso pin does **not** trigger the panic. The replay completes
all 4,106 statements with `Issues found: 1` — and that single issue
is the pre-existing `focus_roots` matview-version error, not the
multiset bug.

Hypothesis: the panic requires the asynchronous post-commit
consolidation path that Holon's `TursoBackend` actor takes (tokio
worker, async I/O on, concurrent CDC notification). The synchronous
replay harness coalesces those phases and skips the racy window.
Suggested upstream repro path:

1. Run the trace against a Turso instance with the same async setup
   (multi-threaded tokio, `async_io: true` on Builder, real disk
   backing — `/tmp` or similar).
2. Or extend the replay harness with the same post-commit
   notification dispatch that Holon uses.

## Notes for upstream

- Pin: `a01c27da5841f70ead56e0366e312bbd2a754422` (`nightscape/turso@holon`).
- This is the same bug family the existing
  `json_group_array_multiset_negative.md` describes; the value
  triggering it is different per run (`'Page'` previously,
  `'block:edge-field-descriptor'` and `'edge-abstraction'` here).
  The shape — dual LEFT JOIN + `json_group_array` + churn on the R
  sides — is identical.
- The Holon-side band-aid is unattractive: the `DELETE FROM
  junction WHERE block_id = ?; INSERT ...` pattern is the correct
  shape for `set_field` on edge fields and skipping the DELETE when
  unchanged would just push the bug into a different shape.
- Recommend tightening the abort path: today `catch_unwind` keeps
  the actor alive on a corrupted aggregate state, which triggers
  the secondary "Reached invalid state" cascade. A panic in
  aggregate consolidation should poison the specific matview, not
  silently continue processing on a broken multiset.
