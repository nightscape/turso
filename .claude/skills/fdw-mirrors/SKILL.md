---
name: fdw-mirrors
description: FDW-sourced materialized views - mirror btrees, the identity contract, the REFRESH sweep, and the push path
---
# FDW Mirrors (IVM over foreign tables)

How a materialized view over a foreign table is maintained incrementally.

## Why a mirror

A foreign table has no write path (`VirtualTable::readonly()` is unconditionally
`true`), so it produces no btree DML and therefore no deltas. The obvious
alternative — diff a fresh scan against the matview — does not work either: a DBSP
retraction needs the **full old row values**, and the matview stores query
*results*, not source rows.

So each view keeps a **mirror**: an internal btree table shadowing the foreign rows
that view reads. Syncing the mirror is ordinary DML, so everything downstream — the
circuit, chained views, CDC, the commit-time `apply_view_deltas` path — is the
existing, tested machinery. Nothing new reaches into the circuit.

```
foreign source ──scan (REFRESH)──┐
                                 ├──► mirror btree ──► view_transaction_states
foreign source ──push (inject)───┘    (+ identity PK)         │
                                                              ▼
                                              apply_view_deltas at COMMIT
                                                → DBSP circuit → matview,
                                                  chained matviews, CDC
```

Cost: the mirror duplicates the foreign rows the view reads. That is the price of
incrementality and it is paid per view, scoped to the view's pushed-down predicate
(a driver with a `required: true` key column cannot be scanned unqualified at all,
so a table-wide mirror is impossible for exactly the API-backed drivers this
targets).

## The identity contract

`ForeignDataWrapper::identity_columns() -> Option<&[u32]>` (`core/foreign.rs`).
Zero-based column indices whose combined values recognise a row across scans.

- **Explicit.** It is *not* `key_columns()`: that declares which operators the
  source evaluates server-side and carries no uniqueness claim (`session_id` is a
  fine key column and a terrible identity).
- **No declaration ⇒ snapshot semantics.** The default `None` keeps today's
  behaviour exactly: synthetic scan-ordinal rowids, `REFRESH` = full rebuild. No
  mirror is created. Drivers that decline the contract are unaffected.
- **NULL identity refused.** The mirror declares the identity columns `NOT NULL`;
  a row with no identity could never be matched on a later scan, so its update or
  removal could never propagate. A sole `INTEGER` identity column is redeclared
  `INT` (`MirrorSpec::declared_type`) so the PRIMARY KEY does not alias the
  rowid.
- **Duplicate identity refused at CREATE and at REFRESH**, with an error naming
  the table and columns. CREATE is refused by the mirror's own PRIMARY KEY
  (`MirrorSync::identity_violation` splits the NOT NULL and PRIMARY KEY failures
  apart, because they are different broken promises); REFRESH is refused by the
  sweep's guard statement.
- **Names are folded, everywhere.** `ReferencedTable::name` is normalized by both
  constructors, `MirrorSpec::source_table` is normalized, and
  `mirror_table_name` folds *both* the view and the source before minting the
  mirror's name — so `CREATE` and the load path agree on one spelling. The
  redirect map is keyed by the folded source name and asserts it
  (`fdw_mirror.rs`, `mirror_specs_for_view` / `rewrite_sources_to_mirrors`);
  missing a redirect is silent, so the assert is the guard.

`CsvFdw` accepts an `identity` table option so the feature is provable in-repo:
`CREATE FOREIGN TABLE … OPTIONS (path '…', identity 'uuid')`.

## The REFRESH sweep

`translate_refresh_materialized_view` branches: no foreign source, or a foreign
source with no declared identity, gets today's byte-identical clear-and-rebuild.
An identity-declaring foreign source instead syncs its mirrors and does nothing
else — the sync's own DML drives the view through the normal commit path.

`MirrorSync::sweep_sql(&scope)` is three ordinary statements (so IO-yield
resumability is the engine's existing behaviour, not a bespoke state machine):

1. the guard: a `SELECT` over `(<scan>)` grouped by the identity, emitting a row
   only to refuse — `'null'` if any identity column is NULL, `'duplicate'` if a
   group holds more than one row
2. `INSERT INTO <mirror> SELECT * FROM (<scan>) WHERE true
   ON CONFLICT(<identity>) DO UPDATE SET … WHERE <any value IS NOT excluded.…>`
3. `DELETE FROM <mirror> WHERE (<identity>) NOT IN (SELECT <identity> FROM (<scan>))`

The guard runs first, so a refusal costs nothing but the scan: no mirror row is
written, no rowid moves, no delta is staged.

Two properties carry the design:

- **An unchanged row produces no DML and therefore no delta**, by construction —
  the `DO UPDATE` is guarded by a value comparison (`IS NOT`, so NULLs compare).
  A no-change REFRESH does not churn view state or spam CDC.
- **A changed row keeps its rowid** (upsert, not `INSERT OR REPLACE`, which churns
  rowids), so its retraction carries the identity its insertion had.

This costs **three foreign scans per sweep** — guard, upsert, anti-join. The
guard cannot ride along on the other two; see the guide for why, and
`tests/integration/query_processing/test_fdw_scan_cost.rs`.

## Scoped REFRESH

`REFRESH MATERIALIZED VIEW <mv> WHERE <predicate>` sweeps a source the scan
speaks for only in part — a driver that can afford "rows changed since the
watermark" but not a full scan cannot witness a deletion, so an unscoped sweep
over such a scan retracts every row the scan did not cover.

The scope is `ast::RefreshScope::{Full, Scoped}` from the parser, carried on
`Insn::SyncFdwMirrors` and dispatched as `MirrorSyncMode::{Rebuild, Sweep(scope)}`.
It is one predicate used twice: ANDed into the source's own query block (so the
qualifier reaches the driver — `ScanQuery` keeps `head` and `predicate` apart for
exactly this) and prefixed to the anti-join's `WHERE`, so absence retracts only
inside the scope. `MirrorSync::validate_scope` refuses at translate time a scope
naming an unknown column, a qualified name, a parameter, or a subquery; a scope
on a view with no mirror is refused too (the rebuild path is authoritative over
its whole scan and has nothing to bound).

## REFRESH with dependents

REFRESH of a matview that other matviews are defined over is *not* refused: the
rebuild path emits `Insn::PopulateMaterializedViews { cascade:
PopulateCascade::ToDependents }`, so the clear loop's retraction of every held
row and the repopulation's insertions are staged into the dependents' delta as
the two halves of one delta — an unchanged row cancels. `CREATE` emits
`PopulateCascade::None`, since nothing can yet be defined over the new view.

For that cancellation to work, `op_delete` truncates the trailing multiset
weight off a matview btree row before capturing the retraction: the stored row is
its output columns plus a weight, and the weight is not part of the row the
circuit is keyed on (`core/vdbe/execute.rs`, the delete path's
`values.truncate(num_columns)`).

## The push path

`Connection::inject_fdw_changes(foreign_table, &[FdwChange])` — REFRESH without the
rescan. `FdwChange { values, weight }`: `+1` insert, `-1` delete, update = both.
`values` is positional against `schema_sql()` and must carry one value per declared
column, **deletes included** (a delete's identity is read out of it by index);
wrong-width batches are refused before anything is applied.

- **Batch-atomic.** The whole batch lands or none of it does; a reader never sees a
  torn push. With no transaction to join it takes its own (`BEGIN IMMEDIATE`);
  inside a caller's transaction it takes a savepoint, so a failure retracts the
  batch without disturbing the caller's writes or ending its transaction.
- **Refused, not queued.** Nothing buffers. If another writer holds the write lock
  (e.g. a suspended sweep), the push fails busy and the caller retries — see
  `test_push_during_a_suspended_sweep_is_refused_then_applies`.
- **Over-approximation is free.** A push may carry a row the view's predicate
  excludes; the mirror is predicate-scoped but the compiled circuit re-applies the
  predicate anyway, so such a push costs storage, never correctness.
- Never call it from inside a running statement (asserted).

`Connection::drain_fdw_stream(table, &Receiver<FdwChange>)` drains a
`StreamingForeignData` subscription and applies it as one batch. **Detection stays
the caller's job** — nothing in the engine polls or schedules. What the engine owns
is that a known change costs only that change, applied atomically.

CSV is **REFRESH-only by ruling**: a file cannot say it changed, and reading it on
every query would make a `SELECT` take the write lock and fire CDC mid-read. Pinned
by `test_refresh_matview_on_fdw`.

## Key files

- `core/incremental/fdw_mirror.rs` — `MirrorSpec` / `MirrorSync` / `ScanQuery`:
  naming, DDL, `sweep_sql`, `rebuild_sql`, `validate_scope`, push SQL, the
  identity-violation errors, the source→mirror redirect
- `core/incremental/view.rs` — `ReferencedTable`,
  `redirect_foreign_sources_to_mirrors`, `PopulateCascade`
- `core/translate/view.rs` — `translate_refresh_materialized_view`: mirror-fed
  sweep vs. clear-and-rebuild, scope validation
- `core/vdbe/execute.rs` — `MirrorSyncMode`, `sync_fdw_mirrors`,
  `op_populate_materialized_views`, the `op_delete` weight truncation
- `core/foreign.rs` — `identity_columns`, `FdwChange`, `StreamingForeignData`
- `core/connection.rs` — `inject_fdw_changes`, `drain_fdw_stream`
- `core/schema.rs` — `FDW_MIRROR_TABLE_PREFIX`, `mirror_table_names_for_view`
- `tests/integration/query_processing/test_fdw_*.rs` — mirror lifecycle, sweep
  hazards, yield injection, push, required-key, scan cost, scoped refresh,
  integer/null identity; `test_ivm_refresh_fdw_dependent.rs` for the cascade
- `docs/agent-guides/fdw-mirrors.md` — the deep reference for all of the above
