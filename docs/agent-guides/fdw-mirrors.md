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
  removal could never propagate. A sole identity column typed `INTEGER` is
  redeclared `INT` (same affinity, not a rowid alias), or the `PRIMARY KEY` would
  alias the rowid — which both hands NULLs a generated identity and creates no
  automatic index for the one the mirror's DDL writes.
- **Duplicate identity refused at CREATE**, with an error naming the table and
  columns (`MirrorSpec::identity_violation` splits the NOT NULL and PRIMARY KEY
  constraint failures apart, because they are different broken promises).
  **OPEN:** the REFRESH sweep does *not* refuse duplicates — its
  `ON CONFLICT … DO UPDATE` collapses them silently. Refusing would cost a third
  foreign scan. Pinned by an `#[ignore]`d test in
  `tests/integration/query_processing/test_fdw_sweep_hazards.rs`.

`CsvFdw` accepts an `identity` table option so the feature is provable in-repo:
`CREATE FOREIGN TABLE … OPTIONS (path '…', identity 'uuid')`.

## The REFRESH sweep

`translate_refresh_materialized_view` branches: no foreign source, or a foreign
source with no declared identity, gets today's byte-identical clear-and-rebuild.
An identity-declaring foreign source instead syncs its mirrors and does nothing
else — the sync's own DML drives the view through the normal commit path.

`MirrorSpec::sweep_sql` is two ordinary statements (so IO-yield resumability is
the engine's existing behaviour, not a bespoke state machine):

1. `INSERT INTO <mirror> SELECT * FROM (<scan>) WHERE true
   ON CONFLICT(<identity>) DO UPDATE SET … WHERE <any value IS NOT excluded.…>`
2. `DELETE FROM <mirror> WHERE (<identity>) NOT IN (SELECT <identity> FROM (<scan>))`

Two properties carry the design:

- **An unchanged row produces no DML and therefore no delta**, by construction —
  the `DO UPDATE` is guarded by a value comparison (`IS NOT`, so NULLs compare).
  A no-change REFRESH does not churn view state or spam CDC.
- **A changed row keeps its rowid** (upsert, not `INSERT OR REPLACE`, which churns
  rowids), so its retraction carries the identity its insertion had.

This costs **two foreign scans per sweep**. The alternative — materialising the
first scan into a staging table — trades that for a second copy of the data plus
per-sync DDL; see `core/incremental/fdw_mirror.rs` and
`tests/integration/query_processing/test_fdw_scan_cost.rs`.

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

- `core/incremental/fdw_mirror.rs` — `MirrorSpec` / `MirrorSync`: naming, DDL,
  `sweep_sql`, `rebuild_sql`, push SQL, the identity-violation errors
- `core/foreign.rs` — `identity_columns`, `FdwChange`, `StreamingForeignData`
- `core/connection.rs` — `inject_fdw_changes`, `drain_fdw_stream`
- `core/schema.rs` — `FDW_MIRROR_TABLE_PREFIX`, `mirror_table_names_for_view`
- `tests/integration/query_processing/test_fdw_*.rs` — mirror lifecycle, sweep
  hazards, yield injection, push, required-key, scan cost
