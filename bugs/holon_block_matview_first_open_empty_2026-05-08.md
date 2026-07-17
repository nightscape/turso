# Matview first-open returns partial result; second open returns full result

**Status:** OPEN, reproducible standalone in pure Turso.
**Date:** 2026-05-08
**Trigger:** `CREATE MATERIALIZED VIEW v AS <dual LEFT JOIN + json_group_array + GROUP BY>`,
populate the underlying base table via CDC inserts, then run two identical
`SELECT COUNT(*) FROM v;` statements back-to-back.

## TL;DR

The first `SELECT COUNT(*) FROM v` against a freshly-populated matview
returns a small subset of rows. The second identical `SELECT COUNT(*)` on
the same connection returns the full count. All subsequent reads also
return the full count.

This is **not** an ingest race — `SELECT COUNT(*) FROM block_raw` (the
base table) returns the correct count *before* the first matview read,
and the matview cursor remains stuck on the partial count even when the
two reads are issued back-to-back with zero writes in between.

## Holon symptom

Holon promotes the `block` table to a matview hydrated from junction
tables (`block_tags`, `block_requires`) — see
`crates/holon/sql/schema/block_matview.sql`. The MCP server's first
matview-touching query (`execute_source_block` against the now-query, or
plain `execute_query` / `execute_raw_sql`) returns 0 rows immediately
after MCP startup. The second identical call returns 3 rows. Repro is at
`devlog/2026-05-08-014627-handoff-mcp-first-query-empty-matview.md`.

Pre-existing 25-second `OrgSyncIdleSignal::wait_quiescent` gate confirms
the matview is fully populated (968 rows in `block_raw` and `block` both)
before the first MCP query. The bug is the read-side cursor, not the
write side.

## Repro

**Public-API standalone reproducer** (preferred for upstream filing):
`bindings/rust/examples/matview_first_open_partial.rs` in the Turso
worktree, registered in `bindings/rust/Cargo.toml`. Run with
`cargo run --example matview_first_open_partial -p turso`. Uses only
the public `turso::Builder::experimental_materialized_views(true)` API.

Two extra repros from the holon side:
- `crates/holon/examples/turso_ivm_matview_first_open_empty_repro.rs`
  (uses `turso_core` + `turso_sdk_kit::rsapi::TursoConnection` like
  holon does in production — reproduces in *all four* scenarios
  including same-connection reads).
- `bigdata/turso/bugs/holon_block_matview_first_open_empty_2026-05-08.sql`
  (SQL trace).

The reproducers each run four scenarios, varying:
- whether the matview is created BEFORE or AFTER the base inserts;
- whether the read happens on the same connection as the writes, or on a
  fresh connection.

**Public-API repro:** ONLY the "fresh connection for reads" scenarios
(B, D) reproduce. Same-connection reads (A, C) are correct.
**Holon-stack repro (turso_sdk_kit wrapper):** ALL FOUR scenarios
reproduce, including same-connection reads.

The delta between the two suggests the holon connection wrapper goes
through a code path that hits the bug regardless of whether the
connection is freshly opened — but the underlying bug surfaces in the
plain `turso::Builder` API too, just only on fresh connections. Sample
output from the public-API repro:

```
===== A. matview-first, single-connection =====
[setup] CREATE MATERIALIZED VIEW block (on empty block_raw)
[setup] INSERT 1000 rows into block_raw
    [post-insert] count(*) FROM block_raw = 1000
    [FIRST  read] count(*) FROM block = 54     <-- BUG
    [SECOND read] count(*) FROM block = 1000
    [THIRD  read] count(*) FROM block = 1000
    [FIRST  filtered] count(*) FROM block WHERE TODO+G1 = 29
    [SECOND filtered] count(*) FROM block WHERE TODO+G1 = 29

===== B. matview-first, fresh-conn-for-reads =====
    [FIRST  read] count(*) FROM block = 28     <-- BUG
    [SECOND read] count(*) FROM block = 1000

===== C. matview-after-inserts, single-connection =====
    [FIRST  read] count(*) FROM block = 28     <-- BUG
    [SECOND read] count(*) FROM block = 1000

===== D. matview-after-inserts, fresh-conn-for-reads =====
    [FIRST  read] count(*) FROM block = 28     <-- BUG
    [SECOND read] count(*) FROM block = 1000
```

## Observations

1. **Connection identity doesn't matter.** Scenarios B and D both use a
   fresh connection for the first read; the bug still reproduces. This
   rules out connection-local prepared-statement-cache theories.

2. **Matview-creation timing doesn't matter.** Scenarios C and D create
   the matview *after* the base inserts (so the matview's initial
   incremental compute should already see all 1000 rows). The bug still
   reproduces — and the partial count (28) matches scenarios B/A,
   suggesting the cursor consistently reads the same incomplete prefix
   on first open.

3. **Filtered queries (with a WHERE on `json_extract(properties, …)`)
   return consistent counts** between the first and second read in this
   repro (29 == 29). Holon's actual now-query has more complex filters
   including `NOT EXISTS` subqueries against the matview itself, and
   *does* return 0-then-3 in production — so the filtered path is not
   immune in general; this minimal repro just doesn't hit the same
   sub-pattern.

4. **The first-open count is small but stable** across runs (28
   consistently across scenarios B/C/D). This isn't a random subset —
   the cursor seems to be reading a deterministic partial state.

## Adjacent fixes that did NOT cover this

- `7cf0a2e68a3a` —
  `MaterializedViewCursor::ensure_tx_changes_computed` walking upstream
  matview deltas. Only fires inside an open transaction; this repro is
  pure autocommit.
- `05c326752ff` (nightscape@holon) — IVM LEFT JOIN drops null-padded row
  on redundant UPDATE. Different shape, fixed.

## Strong suspect

`MaterializedViewCursor::next` (or whatever the cursor entry point is)
in autocommit mode: on the first open after the matview's initial
DBSP/incremental state is built, the cursor returns the rows it has
already finalised in memory but doesn't drain the still-pending
incremental delta. The second open sees the now-flushed delta and
returns the full result.

If this theory is right, the fix is symmetric to `7cf0a2e68a3a`: the
autocommit cursor open path needs the same upstream-delta walk that the
in-txn path got.

## Suggested next steps for upstream

1. Add tracing inside `MaterializedViewCursor::next` /
   `ensure_tx_changes_computed` (or its autocommit counterpart) to
   compare cursor state on first vs. second open.
2. Run the `.rs` reproducer with `RUST_LOG=turso_core::matview=trace`
   (or whichever target name the cursor lives under) and see whether
   the second-call path takes a code branch that the first-call path
   skips.
3. Confirm the fix by running the repro and observing that
   `first == second == 1000` in all four scenarios.

## Holon-side workaround

Add a one-shot warmup at MCP startup, after schema reconcile:

```rust
// Warm the `block` matview cursor so the first user-visible query is
// never the cold-cursor open. See
// bigdata/turso/bugs/holon_block_matview_first_open_empty_2026-05-08.md
// for the upstream bug.
engine.execute_query("SELECT COUNT(*) FROM block".into(), Default::default(), None).await?;
```

**The query shape matters.** A `SELECT 1 FROM block LIMIT 1` warmup
short-circuits the cursor without forcing a full DBSP materialisation,
so the user's later `SELECT COUNT(*)` still hits a cold cursor. A
`COUNT(*)` warmup walks every row, fully realises the matview's
incremental state, and persists the warmth across subsequent prepared
statements.

Verified empirically on a populated `holon-pkm` org-mode database: with
`LIMIT 1` warmup, the first user `COUNT(*)` returned 11; with `COUNT(*)`
warmup, the first user `COUNT(*)` returned the full 968. This pattern
also matches the standalone repro: in scenario A, the first three
unfiltered `COUNT(*)` calls go 54 → 1000 → 1000, and the *next* query
(filtered, totally different SQL) returns the correct count on its very
first call — meaning the warmth set by the first full scan is shared
across prepared statements at the matview level.

This is much cheaper than the OrgSync `wait_quiescent` gate previously
attempted (which didn't fix the bug — see the original handoff at
`devlog/2026-05-08-014627-handoff-mcp-first-query-empty-matview.md`).
Once the upstream fix lands the warmup can come out.
