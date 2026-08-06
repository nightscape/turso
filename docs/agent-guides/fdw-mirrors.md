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
- **Duplicate identity refused at CREATE and at REFRESH**, with an error naming
  the table and columns. CREATE is refused by the mirror's own PRIMARY KEY
  (`MirrorSync::identity_violation` splits the NOT NULL and PRIMARY KEY failures
  apart, because they are different broken promises); REFRESH is refused by the
  sweep's guard, since its `ON CONFLICT … DO UPDATE` would otherwise collapse
  duplicates silently, last-scanned-wins over an order no driver promises.

- **One folded spelling, asserted.** A mirror is found by name, and missing it is
  silent — the view would then read the foreign table directly and stage no
  deltas — so every name on the path is normalized at its source rather than at
  its uses. `ReferencedTable::name` is normalized by both constructors (a btree
  table is already stored folded, but a virtual table keeps the spelling it was
  declared with); `MirrorSpec::source_table` is normalized in
  `mirror_specs_for_view`; and `mirror_table_name` folds *both* the view name and
  the source name, because `CREATE` and the load path mint that name
  independently. `rewrite_sources_to_mirrors` asserts its redirect map is keyed
  by folded names, since a raw key there is a key that never matches
  (`a_source_spelled_in_another_case_is_still_redirected`).

`CsvFdw` accepts an `identity` table option so the feature is provable in-repo:
`CREATE FOREIGN TABLE … OPTIONS (path '…', identity 'uuid')`.

## The REFRESH sweep

`translate_refresh_materialized_view` branches: no foreign source, or a foreign
source with no declared identity, gets today's byte-identical clear-and-rebuild.
An identity-declaring foreign source instead emits `Insn::SyncFdwMirrors` and
does nothing else — the sync's own DML drives the view through the normal commit
path. `sync_fdw_mirrors` in `core/vdbe/execute.rs` picks the SQL by
`MirrorSyncMode`: `Rebuild` (a mirror-fed view being populated from scratch)
runs `rebuild_sql`, `Sweep(&RefreshScope)` runs `sweep_sql(scope)`.

`MirrorSync::sweep_sql` is three ordinary statements (so IO-yield resumability
is the engine's existing behaviour, not a bespoke state machine):

1. the guard: a `SELECT` over `(<scan>)` grouped by the identity, emitting a row
   only to refuse — `'null'` if any identity column is NULL, `'duplicate'` if any
   group holds more than one row
2. `INSERT INTO <mirror> SELECT * FROM (<scan>) WHERE true
   ON CONFLICT(<identity>) DO UPDATE SET … WHERE <any value IS NOT excluded.…>`
3. `DELETE FROM <mirror> WHERE (<identity>) NOT IN (SELECT <identity> FROM (<scan>))`

The guard runs first, so a refusal costs the sweep nothing but the scan: no
mirror row is written, no rowid moves, no delta is staged. It groups rather than
counting `DISTINCT` for two reasons — `count(DISTINCT …)` ignores NULLs, so a
lone NULL identity would be reported as a duplicate that is not there, and
SQLite has no `count(DISTINCT a, b)` for a composite identity. `MirrorSync`
carries an internal `DuplicatePolicy`, hard-wired to `Refuse` at its one
construction site; `LastWins` is exactly the absence of the guard and nothing
constructs it, so it is the seam a driver-facing knob would use rather than a
live second code path.

Two properties carry the design:

- **An unchanged row produces no DML and therefore no delta**, by construction —
  the `DO UPDATE` is guarded by a value comparison (`IS NOT`, so NULLs compare).
  A no-change REFRESH does not churn view state or spam CDC.
- **A changed row keeps its rowid** (upsert, not `INSERT OR REPLACE`, which churns
  rowids), so its retraction carries the identity its insertion had.

This costs **three foreign scans per sweep** — guard, upsert, anti-join. The
guard cannot ride along on the other two: a scan named once and read twice is
scanned twice (`test_scan_named_once_and_read_twice`), and the `rows_read`
counter that could have substituted counts rows *before* the predicate, so a
guard built on it would refuse any view the driver cannot push down
(`test_scan_row_count_metric_is_pre_predicate`). The alternative — materialising
the first scan into a staging table — trades the repeats for a second copy of
the data plus per-sync DDL; see `core/incremental/fdw_mirror.rs` and
`tests/integration/query_processing/test_fdw_scan_cost.rs`.

## Scoped REFRESH

`REFRESH MATERIALIZED VIEW <mv> WHERE <predicate>` sweeps a source the scan
speaks for only in part. A driver that can afford "rows changed since the
watermark" but not a full scan cannot witness a deletion at all — a deleted row
has no attributes — so an unscoped sweep over such a scan retracts every row the
scan did not happen to cover.

The scope is one predicate used twice: ANDed into the source's own query block
(so the qualifier reaches the driver — `test_the_scope_is_pushed_down_to_the_driver`)
and prefixed to the anti-join's `WHERE` (so absence retracts only inside it).
`ScanQuery` keeps the scan in parts for exactly this: the view's predicate may be
a disjunction, and appending `AND …` to the rendered text would bind to its last
branch alone. Everything else — the guard, the upsert, `upsert_tail` — is what a
full sweep uses, byte for byte.

"The scan returned nothing" and "no scope was given" are distinct values, not an
empty predicate: `ast::RefreshScope::{Full, Scoped}`, chosen by the parser. An
empty scoped scan therefore retracts exactly its scope, which is the failure mode
that sank three driver-side attempts at the same bound.

The bound is evaluated against the mirror, so `MirrorSync::validate_scope`
refuses at translate time a scope naming a column the source does not have, a
qualified name (the scan and the mirror are two different tables), a parameter
(the sweep binds nothing), or a subquery. A scope on a view with no mirror is
refused too: the rebuild path is authoritative over its whole scan and has
nothing to bound.

## REFRESH with dependents

A matview other matviews are defined over can be refreshed; nothing refuses it.
The clear-and-rebuild path maintains the dependents through the ordinary IVM
path rather than a bespoke one, by emitting `Insn::PopulateMaterializedViews`
with `cascade: PopulateCascade::ToDependents`. `CREATE` emits
`PopulateCascade::None` — nothing can be defined over a view that does not exist
yet, so its population owes no dependent anything.

The two halves have to be one delta for this to be correct. The clear loop
deletes every row the view held, staging one retraction each into the
dependents' per-table delta; `populate_from_table` then collects the rebuilt
rows into `populate_output` and stages one insertion each, keyed the same way
and under the same view name. An unchanged row therefore cancels, and a
dependent sees exactly the difference the refresh made. A negative net would
mean the rebuild retracted something the clear did not, and is asserted against.

`op_delete` truncates the trailing multiset weight before capturing the
retraction. A matview's btree row is its output columns plus a weight written by
`Circuit::commit`; the weight is a property of the stored row, not part of the
row the circuit is keyed on, so carrying it into the delta would key the
retraction differently from the insertion and defeat the cancellation. It is
also not a repeat count: the clear loop stages one retraction per btree row and
the rebuild one insertion per row, on either side.

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
  identity-violation errors, `rewrite_sources_to_mirrors`
- `core/incremental/fdw_mirror_scope_tests.rs` — the scoped-sweep SQL unit tests
- `core/incremental/view.rs` — `ReferencedTable`,
  `redirect_foreign_sources_to_mirrors`, `mirror_syncs`, `PopulateCascade`
- `core/translate/view.rs` — `translate_refresh_materialized_view`
- `core/vdbe/execute.rs` — `MirrorSyncMode`, `sync_fdw_mirrors`,
  `op_populate_materialized_views`, the `op_delete` weight truncation
- `sqlite/parser/src/{ast.rs,parser.rs}` — `RefreshScope`
- `core/foreign.rs` — `identity_columns`, `FdwChange`, `StreamingForeignData`
- `core/connection.rs` — `inject_fdw_changes`, `drain_fdw_stream`
- `core/schema.rs` — `FDW_MIRROR_TABLE_PREFIX`, `mirror_table_names_for_view`
- `tests/integration/query_processing/test_fdw_*.rs` — mirror lifecycle, sweep
  hazards, yield injection, push, required-key, scan cost, scoped refresh,
  integer/null identity, CSV quoted columns
- `tests/integration/query_processing/test_ivm_refresh_fdw_dependent.rs` — the
  dependent cascade
