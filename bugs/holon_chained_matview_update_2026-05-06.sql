-- G4 reproducer: chained matview reads inside an open transaction miss
-- uncommitted upstream deltas. Pre-fix (≤ commit f682709866bb), the
-- focus_roots projection inside the txn does not see the post-UPDATE
-- state of block_mv. Fixed in commit 7cf0a2e68a3a (".. IVM chained
-- matview reads inside open transactions"); tests
-- chained-matview-uncommitted-insert-propagates and
-- chained-matview-uncommitted-holon-shape in matview-on-matview.sqltest.
--
-- Original problem report: bugs/holon_block_hydration_matview_gaps_2026-05-04.md.
-- Holon symptom: split_block within a single PBT transition issues
--   INSERT new_block + UPDATE block_raw.content for the original;
-- the focus_roots row for the original block disappears, propagating
-- to the LiveData<FocusRoot> mirror and then to user-visible
-- editable_text widgets that race with the mirror.
--
-- This repro wraps the four splits in a single explicit BEGIN/COMMIT
-- so the chained read inside the txn fires the bug. The autocommit
-- variant (each split = its own txn boundary) does *not* trigger.
--
-- Run with:  tursodb --experimental-views /tmp/holon_split_repro.db
--   < bugs/holon_chained_matview_update_2026-05-06.sql

CREATE TABLE block_raw (
  id TEXT PRIMARY KEY,
  parent_id TEXT,
  content TEXT NOT NULL DEFAULT '',
  sort_key TEXT NOT NULL DEFAULT 'A0'
);
CREATE TABLE block_tags (
  block_id TEXT NOT NULL,
  tag      TEXT NOT NULL,
  PRIMARY KEY (block_id, tag)
);
CREATE TABLE task_blockers (
  blocked_id TEXT NOT NULL,
  blocker_id TEXT NOT NULL,
  PRIMARY KEY (blocked_id, blocker_id)
);
CREATE TABLE nav (region TEXT NOT NULL, doc_id TEXT NOT NULL, PRIMARY KEY (region));

INSERT INTO block_raw (id, parent_id, content, sort_key) VALUES
  ('block:doc', '',          'Doc',      'A0'),
  ('block:1-4', 'block:doc', 'aaaa',     'B0'),
  ('block:tay', 'block:doc', 'tau yota', 'C0');
INSERT INTO block_tags (block_id, tag) VALUES ('block:1-4', 'urgent');
INSERT INTO nav (region, doc_id) VALUES ('main', 'block:doc');

-- L1: holon's `block` matview shape (case2b_two_left_agg + sort_key).
CREATE MATERIALIZED VIEW block_mv AS
SELECT b.id, b.parent_id, b.content, b.sort_key,
  COALESCE(json_group_array(bt.tag)        FILTER (WHERE bt.tag        IS NOT NULL), '[]') AS tags,
  COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by
FROM block_raw b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
GROUP BY b.id, b.parent_id, b.content, b.sort_key;

-- L2: holon's focus_roots matview shape (matview JOIN base table).
CREATE MATERIALIZED VIEW focus_roots AS
SELECT n.region, b.id AS root_id
FROM nav n
JOIN block_mv b ON b.parent_id = n.doc_id;

SELECT '--- INITIAL focus_roots (expect 2 rows: 1-4, tay) ---' AS marker;
SELECT region, root_id FROM focus_roots ORDER BY root_id;

-- The bug fires when the SELECT below runs INSIDE the same transaction
-- that did the INSERT+UPDATE. Pre-fix, focus_roots' chained read of
-- block_mv misses block_mv's uncommitted output delta — so the row for
-- block:1-4 (and any other UPDATEd row) appears stale or missing.
BEGIN;

INSERT INTO block_raw (id, parent_id, content, sort_key) VALUES ('block:s1', 'block:doc', 'aaa', 'B5');
UPDATE block_raw SET content = 'a' WHERE id = 'block:1-4';

SELECT '--- inside txn after SPLIT #1 (expect 3 rows incl. block:1-4) ---' AS marker;
SELECT region, root_id FROM focus_roots ORDER BY root_id;

INSERT INTO block_raw (id, parent_id, content, sort_key) VALUES ('block:s2', 'block:doc', 'a', 'B7');
UPDATE block_raw SET content = '' WHERE id = 'block:1-4';

SELECT '--- inside txn after SPLIT #2 (expect 4 rows incl. block:1-4) ---' AS marker;
SELECT region, root_id FROM focus_roots ORDER BY root_id;

INSERT INTO block_raw (id, parent_id, content, sort_key) VALUES ('block:s3', 'block:doc', 'aa', 'B6');
UPDATE block_raw SET content = '' WHERE id = 'block:s1';

SELECT '--- inside txn after SPLIT #3 (expect 5 rows incl. block:1-4 and block:s1) ---' AS marker;
SELECT region, root_id FROM focus_roots ORDER BY root_id;

INSERT INTO block_raw (id, parent_id, content, sort_key) VALUES ('block:s4', 'block:doc', 'final', 'B8');
UPDATE block_raw SET content = 'fff' WHERE id = 'block:1-4';

SELECT '--- inside txn after SPLIT #4 (expect 6 rows: 1-4, tay, s1, s2, s3, s4) ---' AS marker;
SELECT region, root_id FROM focus_roots ORDER BY root_id;

COMMIT;

SELECT '--- post-COMMIT focus_roots (expect 6 rows) ---' AS marker;
SELECT region, root_id FROM focus_roots ORDER BY root_id;
