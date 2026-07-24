-- Turso IVM: redundant UPDATE on a base table drops the row from a
-- hydrating LEFT-JOIN + GROUP BY matview.
--
-- Setup is the holon `block` matview shape (case2b_two_left_agg + GROUP BY).
-- Bug: a second UPDATE that does not change any value still propagates a
-- delta through the matview's GROUP BY and removes the entire group.
--
-- Run with:
--   target/release/tursodb --experimental-views /tmp/redundant_update.db \
--       < bugs/holon_block_redundant_update_2026-05-07.sql
--
-- Pinned Turso revision when first reproduced: 7cf0a2e68a3a (the chained
-- matview-read-in-txn fix; bug remains after that fix).
--
-- Holon symptom: PBT panic at sut.rs:4002 reports
--   "Region 'main' focus_roots mismatch after navigation.
--    block:<id>: block_raw=✓ block=✗ focus_roots=false"
-- The chained focus_roots matview is the surface; the actual stale-row
-- drop is one level deeper, in the `block` matview that focus_roots JOINs.
-- Captured under autocommit (no BEGIN/COMMIT around the UPDATEs).

CREATE TABLE block_raw (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    content TEXT NOT NULL DEFAULT '',
    sort_key TEXT NOT NULL DEFAULT 'A0'
);

CREATE TABLE block_tags (
    block_id TEXT NOT NULL,
    tag      TEXT NOT NULL,
    PRIMARY KEY (block_id, tag),
    FOREIGN KEY (block_id) REFERENCES block_raw(id) ON DELETE CASCADE
);

CREATE TABLE task_blockers (
    blocked_id TEXT NOT NULL,
    blocker_id TEXT NOT NULL,
    PRIMARY KEY (blocked_id, blocker_id),
    FOREIGN KEY (blocked_id) REFERENCES block_raw(id) ON DELETE CASCADE,
    FOREIGN KEY (blocker_id) REFERENCES block_raw(id) ON DELETE CASCADE
);

CREATE MATERIALIZED VIEW block AS
SELECT
    b.id, b.parent_id, b.content, b.sort_key,
    COALESCE(json_group_array(bt.tag)        FILTER (WHERE bt.tag        IS NOT NULL), '[]') AS tags,
    COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by
FROM block_raw b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
GROUP BY b.id, b.parent_id, b.content, b.sort_key;

-- step 1: insert a row.
INSERT INTO block_raw (id, parent_id, content, sort_key)
VALUES ('block:victim', 'block:doc', 'Dple6 lJaGjrHy3 4b', 'B0');

SELECT '--- after INSERT (expect block_raw=1, block=1) ---' AS marker;
SELECT 'block_raw' AS src, count(*) AS n FROM block_raw WHERE id = 'block:victim'
UNION ALL
SELECT 'block'     AS src, count(*)      FROM block     WHERE id = 'block:victim';

-- step 2: real UPDATE — content changes from 'Dple6 lJaGjrHy3 4b' to 'D'.
UPDATE block_raw SET content = 'D' WHERE id = 'block:victim';

SELECT '--- after UPDATE #1 [value changed] (expect block_raw=1, block=1) ---' AS marker;
SELECT 'block_raw' AS src, count(*) AS n FROM block_raw WHERE id = 'block:victim'
UNION ALL
SELECT 'block'     AS src, count(*)      FROM block     WHERE id = 'block:victim';

-- step 3: redundant UPDATE — content already equals 'D'. No value change.
UPDATE block_raw SET content = 'D' WHERE id = 'block:victim';

SELECT '--- after UPDATE #2 [no value change] (expect block_raw=1, block=1; ACTUAL block=0 — BUG) ---' AS marker;
SELECT 'block_raw' AS src, count(*) AS n FROM block_raw WHERE id = 'block:victim'
UNION ALL
SELECT 'block'     AS src, count(*)      FROM block     WHERE id = 'block:victim';

-- Same projection, but evaluated fresh (no IVM): proves the matview's
-- own SELECT body produces 1 row; only the matview's incremental state
-- has 0 rows.
SELECT '--- fresh re-evaluation of the matview SELECT (expect 1 row) ---' AS marker;
SELECT count(*) AS fresh_block_count
FROM (
  SELECT b.id, b.parent_id, b.content, b.sort_key,
         COALESCE(json_group_array(bt.tag)        FILTER (WHERE bt.tag        IS NOT NULL), '[]') AS tags,
         COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by
  FROM block_raw b
  LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
  LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
  GROUP BY b.id, b.parent_id, b.content, b.sort_key
) WHERE id = 'block:victim';
