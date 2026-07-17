# `WHERE col IS NOT NULL` fails to filter NULL rows in matview with aliased projection (2026-05-08)

**Status:** reproduced on upstream `tursodb` CLI v0.6.0-pre.23 with `--experimental-views`. Minimal SQL repro at [`bugs/holon_focus_roots_null_filter_2026-05-08.sql`](holon_focus_roots_null_filter_2026-05-08.sql).
**Holon symptom:** `LiveData<FocusRoot>` panics on home rows (`block_id IS NULL`) because the watcher sees them streaming through what is supposed to be an `IS NOT NULL`-filtered matview. Test-side workaround at `crates/holon-integration-tests/src/pbt/sut.rs:3207` (`SELECT region, root_id FROM focus_roots WHERE root_id IS NOT NULL`) sidesteps the bug — production GQL does the same via a `JOIN block ON root.id = fr.root_id`.

## TL;DR

For a matview that combines:
- column aliases (`SELECT block_id AS root_id, ...`)
- a compound WHERE with `IS NOT NULL` on a nullable column

the matview's incremental state includes rows where the filtered column is NULL. The same query as a plain `SELECT` (not a matview) correctly excludes the NULL rows. Two transitions both leak:

1. **Insert with NULL value** → row appears in matview despite `WHERE col IS NOT NULL`.
2. **UPDATE value → NULL** on an already-materialized row → row stays in matview instead of being removed.

A minimal matview without aliases (`SELECT id, payload FROM t WHERE payload IS NOT NULL`) handles the same INSERTs correctly. The bug needs the alias-shaped projection.

## Minimal reproducer

```sql
CREATE TABLE navigation_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    region TEXT NOT NULL,
    block_id TEXT,
    timestamp INTEGER NOT NULL,
    closed_at TEXT
);

CREATE MATERIALIZED VIEW focus_roots AS
SELECT region, block_id AS root_id, timestamp AS added_ts, id AS history_id
FROM navigation_history
WHERE closed_at IS NULL AND block_id IS NOT NULL;

INSERT INTO navigation_history (region, block_id, timestamp) VALUES ('main', NULL,      1000);
INSERT INTO navigation_history (region, block_id, timestamp) VALUES ('main', 'block:a', 1001);
INSERT INTO navigation_history (region, block_id, timestamp) VALUES ('main', 'block:b', 1002);

SELECT COUNT(*), COUNT(*) - COUNT(root_id) AS null_leaked FROM focus_roots;
-- expected: 2 rows, 0 null_leaked
-- actual:   3 rows, 1 null_leaked
```

## Why this matters for Holon

Holon's `focus_roots` matview projects open `navigation_history` rows for the navigation system. Home rows (where the user navigated to "no specific block") have `block_id = NULL`. We want them excluded from the matview because consumers do `JOIN block ON block.id = focus_roots.root_id` and a NULL key never joins.

The current matview is:

```sql
-- crates/holon/sql/schema/matview_focus_roots.sql
SELECT region, block_id AS root_id, timestamp AS added_ts, id AS history_id
FROM navigation_history
WHERE closed_at IS NULL
```

The natural fix is to add `AND block_id IS NOT NULL` to the WHERE — but that's the exact pattern that triggers this bug. So we keep the looser matview and filter at the consumer layer:

- **Production GQL**: `JOIN block ON block.id = fr.root_id` — drops NULL rows naturally.
- **PBT LiveData watcher**: explicit `WHERE root_id IS NOT NULL` in the watch SQL.

## Workaround (in place)

`crates/holon-integration-tests/src/pbt/sut.rs:3207` issues `SELECT region, root_id FROM focus_roots WHERE root_id IS NOT NULL` for the LiveData<FocusRoot> mirror. Sidesteps the bug; matches production GQL behavior.

## Notes

- A simpler matview without aliases (`SELECT id, payload FROM rows WHERE payload IS NOT NULL`) handles the same INSERTs correctly. So the bug appears to require the column-renaming projection shape.
- A separate failure mode (matview returns 0 rows when expected 1) was observed on the holon-pinned Turso fork via the Rust `turso` crate but did NOT reproduce on the upstream `tursodb` CLI — likely a fork or API-path-specific bug, separately tracked at `crates/holon/examples/turso_ivm_focus_roots_null_filter.rs::test_no_other_where_clauses`.
- Repro example at `crates/holon/examples/turso_ivm_focus_roots_null_filter.rs` (run with `cargo run --example turso_ivm_focus_roots_null_filter`).
