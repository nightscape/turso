-- Holon block-hydration matview gaps — self-contained reproducer.
-- Run with:  tursodb /tmp/holon_repro.db < bugs/holon_block_hydration_repro.sql
--
-- See bugs/holon_block_hydration_matview_gaps_2026-05-04.md for context.
--
-- All schemas, data, and matviews are independent — each CASE_X can be
-- run on its own by copy-pasting just its block (plus the schema +
-- INSERT setup at the top).

-- =====================================================================
-- Schema (mirrors holon's prod shape: one entity table + two junctions,
-- one with single FK, one with dual FK back to the entity)
-- =====================================================================

CREATE TABLE block (
  id TEXT PRIMARY KEY,
  parent_id TEXT,
  content TEXT NOT NULL DEFAULT ''
);

CREATE TABLE block_tags (
  block_id TEXT NOT NULL,
  tag      TEXT NOT NULL,
  PRIMARY KEY (block_id, tag),
  FOREIGN KEY (block_id) REFERENCES block (id) ON DELETE CASCADE
);

CREATE TABLE task_blockers (
  blocked_id TEXT NOT NULL,
  blocker_id TEXT NOT NULL,
  PRIMARY KEY (blocked_id, blocker_id),
  FOREIGN KEY (blocked_id) REFERENCES block (id) ON DELETE CASCADE,
  FOREIGN KEY (blocker_id) REFERENCES block (id) ON DELETE CASCADE
);

INSERT INTO block (id, parent_id, content) VALUES
  ('block:a', 'doc:demo', 'alpha'),
  ('block:b', 'doc:demo', 'bravo'),
  ('block:c', 'doc:demo', 'charlie'),
  ('block:d', 'doc:demo', 'delta'),
  ('block:e', 'doc:demo', 'echo'),
  ('block:f', 'doc:demo', 'foxtrot');

INSERT INTO block_tags (block_id, tag) VALUES
  ('block:a', 'urgent'),
  ('block:a', 'review'),
  ('block:b', 'review'),
  ('block:c', 'archived');

INSERT INTO task_blockers (blocked_id, blocker_id) VALUES
  ('block:d', 'block:a'),
  ('block:e', 'block:a');

-- =====================================================================
-- BASELINE — confirmed working on holon branch as of 2026-05-04
-- =====================================================================

-- Single LEFT OUTER + json_group_array + FILTER + GROUP BY.
-- DDL accepts; matview reflects live writes correctly.
CREATE MATERIALIZED VIEW baseline_one_junction AS
SELECT
  b.id, b.parent_id, b.content,
  COALESCE(json_group_array(bt.tag) FILTER (WHERE bt.tag IS NOT NULL), '[]') AS tags
FROM block b
LEFT OUTER JOIN block_tags bt ON bt.block_id = b.id
GROUP BY b.id, b.parent_id, b.content;

-- Sanity check (expect 6 rows; 'block:a' has ["urgent","review"], etc.)
SELECT id, tags FROM baseline_one_junction ORDER BY id;

-- =====================================================================
-- CASE_1 — second junction with structurally identical SQL
-- The only change vs. baseline: joining task_blockers (dual FK to block)
-- instead of block_tags (single FK).
-- EXPECTED on holon-branch 2026-05-04: REJECTED.
-- =====================================================================

CREATE MATERIALIZED VIEW case1_blockers_only AS
SELECT b.id,
  COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by
FROM block b
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
GROUP BY b.id;

SELECT '--- case1_blockers_only ---' AS marker;
SELECT id, blocked_by FROM case1_blockers_only ORDER BY id;

-- =====================================================================
-- CASE_2a — two LEFT OUTER JOINs, no aggregation
-- EXPECTED: REJECTED.
-- =====================================================================

CREATE MATERIALIZED VIEW case2a_two_left_no_agg AS
SELECT b.id, bt.tag, tb.blocker_id
FROM block b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id;

SELECT '--- case2a_two_left_no_agg ---' AS marker;
SELECT id, tag, blocker_id FROM case2a_two_left_no_agg ORDER BY id, tag, blocker_id;

-- =====================================================================
-- CASE_2b — two LEFT OUTER JOINs with aggregation per junction
-- EXPECTED: REJECTED.
-- This is the *target* shape — holon's `block` matview should hydrate
-- both `tags` and `blocked_by` in one pass.
-- =====================================================================

CREATE MATERIALIZED VIEW case2b_two_left_agg AS
SELECT b.id,
  COALESCE(json_group_array(bt.tag)        FILTER (WHERE bt.tag        IS NOT NULL), '[]') AS tags,
  COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by
FROM block b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
GROUP BY b.id;

SELECT '--- case2b_two_left_agg ---' AS marker;
SELECT id, tags, blocked_by FROM case2b_two_left_agg ORDER BY id;

-- =====================================================================
-- CASE_3 — DISTINCT inside json_group_array
-- Workaround for the cartesian explosion if CASE_2b stays open.
-- EXPECTED: REJECTED.
-- =====================================================================

CREATE MATERIALIZED VIEW case3_distinct AS
SELECT b.id,
  COALESCE(json_group_array(DISTINCT bt.tag) FILTER (WHERE bt.tag IS NOT NULL), '[]') AS tags
FROM block b
LEFT OUTER JOIN block_tags bt ON bt.block_id = b.id
GROUP BY b.id;

SELECT '--- case3_distinct ---' AS marker;
SELECT id, tags FROM case3_distinct ORDER BY id;

-- =====================================================================
-- CASE_4 — correlated json_group_array subquery in matview SELECT list
-- This shape works as a regular VIEW today (holon's CacheBlockReader
-- uses it). Promoting to a matview is the smallest holon-side change
-- to unblock the architectural collapse.
-- EXPECTED: REJECTED.
-- =====================================================================

CREATE MATERIALIZED VIEW case4_correlated_subq AS
SELECT b.id,
  (SELECT json_group_array(tag)        FROM block_tags    WHERE block_id   = b.id) AS tags,
  (SELECT json_group_array(blocker_id) FROM task_blockers WHERE blocked_id = b.id) AS blocked_by
FROM block b;

-- =====================================================================
-- CDC verification — matview must reflect base-table mutations.
-- Drives the case2b matview (the holon target shape) since it's the
-- only one that hydrates both junctions at once.
-- =====================================================================

SELECT '--- before mutation (block:f, block:d) ---' AS marker;
SELECT id, tags, blocked_by FROM case2b_two_left_agg WHERE id IN ('block:f', 'block:d') ORDER BY id;

INSERT INTO block_tags  (block_id,   tag)        VALUES ('block:f', 'late-add');
DELETE FROM task_blockers WHERE blocked_id = 'block:d';

SELECT '--- after mutation ---' AS marker;
SELECT id, tags, blocked_by FROM case2b_two_left_agg WHERE id IN ('block:f', 'block:d') ORDER BY id;
-- expected:
--   block:d  []           []
--   block:f  ["late-add"] []

-- =====================================================================
-- CHAINED-MATVIEW PREFLIGHT — required for holon Task #3.
--
-- Holon has 20+ existing matviews that do `FROM block` (block_with_path,
-- task_blocking_edges, watch_view_*). When `block` becomes a matview,
-- all of those become matview-on-matview. The Turso IVM compiler
-- historically hung in that configuration (skill:
-- turso-chained-matview-hang — DBSP graph cannot wire matview outputs
-- as inputs to other operators; IncrementalView only tracked
-- source_tables, not source_matviews).
--
-- IMPORTANT: case2b_two_left_agg above only projects (id, tags,
-- blocked_by) so it can't be used as the `block` matview directly —
-- consumers like block_with_path reference parent_id, content, etc.
-- This section uses block_mv_full, which projects all the columns
-- holon's real `block` table has.
--
-- Covers the three shapes holon actually uses on top of `block`:
--
--   CHAIN_1 — simple filter matview (watch_view_* shape).
--   CHAIN_2 — recursive CTE matview (block_with_path shape, the most
--             load-bearing — drives ancestor/descendant queries).
--   CHAIN_3 — JOIN matview (focus_roots-style: matview JOIN other
--             table).
--   CHAIN_CDC — base-table mutations must propagate two hops:
--               block_tags → block_mv_full (the new `block`)
--                          → chain_filter / chain_paths / chain_join.
--
-- Green = proceed with Task #3.
-- Red on any of {DDL hang, missing rows, stale CDC} = file as G3 and
--   pause on Task #3.
-- =====================================================================

-- ---------------------------------------------------------------------
-- block_mv_full — realistic shape of the new `block` matview.
-- Projects all the source-table columns that downstream matviews
-- reference, so chain_* matviews can look like the real holon ones.
-- ---------------------------------------------------------------------

CREATE MATERIALIZED VIEW block_mv_full AS
SELECT b.id, b.parent_id, b.content,
  COALESCE(json_group_array(bt.tag)        FILTER (WHERE bt.tag        IS NOT NULL), '[]') AS tags,
  COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by
FROM block b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
GROUP BY b.id, b.parent_id, b.content;

SELECT '--- block_mv_full (initial) ---' AS marker;
SELECT id, parent_id, tags, blocked_by FROM block_mv_full ORDER BY id;
-- expected: 6 rows; block:f present with [] [] (G0 fix); same data as
-- case2b but with parent_id/content projected too.

-- ---------------------------------------------------------------------
-- CHAIN_1 — simple filter on top of block_mv_full (watch_view_* pattern).
-- ---------------------------------------------------------------------

CREATE MATERIALIZED VIEW chain_filter AS
SELECT id, parent_id, tags
FROM block_mv_full
WHERE parent_id = 'doc:demo';

SELECT '--- chain_filter (initial) ---' AS marker;
SELECT id, parent_id, tags FROM chain_filter ORDER BY id;
-- expected: 6 rows (block:a..block:f), all with parent_id='doc:demo';
-- block:a carries ["review","urgent"]; block:f carries ["late-add"]
-- (from the earlier CDC INSERT); block:d/e/f carry [] for tags etc.

-- ---------------------------------------------------------------------
-- CHAIN_2 — recursive CTE on top of block_mv_full (block_with_path
-- pattern). This is the most likely-to-hang shape: WITH RECURSIVE base
-- case selects from the matview, recursive case JOINs the matview again.
--
-- Mirrors holon's actual blocks_with_paths.sql:
--   unaliased base case + b.-prefixed recursive case.
--
-- Caveat (G3 candidate, NOT a holon blocker): if you alias the base
-- case (`FROM block_mv_full b WHERE b.parent_id LIKE ...`) AND the
-- recursive case references `p.id` (the projected unrenamed id from
-- the base), DDL fails with `Parse error: no such column: id`. Holon
-- never emits this exact shape — block_with_path uses unaliased base,
-- and GQL/PRQL-generated WITH RECURSIVE views use AS-renames
-- (`_v0.id AS node_id`) so they reference `p.node_id`, not `p.id`.
-- Worth a Turso ticket but doesn't block Task #3.
-- ---------------------------------------------------------------------

CREATE MATERIALIZED VIEW chain_paths AS
WITH RECURSIVE paths AS (
  SELECT id, parent_id, tags, blocked_by,
         '/' || id AS path,
         id AS root_id
  FROM block_mv_full
  WHERE parent_id LIKE 'doc:%'

  UNION ALL

  SELECT b.id, b.parent_id, b.tags, b.blocked_by,
         p.path || '/' || b.id AS path,
         p.root_id
  FROM block_mv_full b
  INNER JOIN paths p ON b.parent_id = p.id
)
SELECT * FROM paths;

SELECT '--- chain_paths (initial) ---' AS marker;
SELECT id, path, tags, blocked_by FROM chain_paths ORDER BY id;
-- expected: 6 rows (all blocks are direct children of doc:demo, so
-- paths are flat: '/block:a' .. '/block:f'). The recursive CTE
-- compilation path on a matview source is what's being validated.

-- ---------------------------------------------------------------------
-- CHAIN_3 — JOIN matview-on-matview with another base table
-- (focus_roots-style: matview JOIN base table).
-- ---------------------------------------------------------------------

CREATE MATERIALIZED VIEW chain_join AS
SELECT b.id, b.tags, tb.blocker_id
FROM block_mv_full b
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id;

SELECT '--- chain_join (initial) ---' AS marker;
SELECT id, tags, blocker_id FROM chain_join ORDER BY id, blocker_id;
-- expected (post earlier mutation that DELETED task_blockers for
-- block:d): 6 rows; only block:e has blocker_id='block:a'.

-- ---------------------------------------------------------------------
-- CHAIN_CDC — mutate base tables, verify two-hop propagation.
-- ---------------------------------------------------------------------

SELECT '--- chain_cdc before tag mutation (block:a) ---' AS marker;
SELECT id, tags FROM chain_filter WHERE id='block:a';
SELECT id, tags FROM chain_paths  WHERE id='block:a';

INSERT INTO block_tags (block_id, tag) VALUES ('block:a', 'chain-cdc');

SELECT '--- chain_cdc after tag mutation (block:a) ---' AS marker;
SELECT id, tags FROM block_mv_full WHERE id='block:a';
SELECT id, tags FROM chain_filter  WHERE id='block:a';
SELECT id, tags FROM chain_paths   WHERE id='block:a';
-- expected: all three show ["chain-cdc","review","urgent"] (or any
-- permutation containing 'chain-cdc'). If chain_filter/chain_paths
-- regress to the pre-insert tag set, two-hop CDC is broken — file G3.

INSERT INTO block (id, parent_id, content) VALUES ('block:g', 'doc:demo', 'golf');

SELECT '--- chain_cdc after new block:g insert ---' AS marker;
SELECT id, parent_id, tags FROM block_mv_full WHERE id='block:g';
SELECT id, parent_id, tags FROM chain_filter  WHERE id='block:g';
SELECT id, path,      tags FROM chain_paths   WHERE id='block:g';
-- expected: all three show block:g with tags=[];
-- chain_paths.path='/block:g'.
