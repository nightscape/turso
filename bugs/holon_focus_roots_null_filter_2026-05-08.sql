-- Repro: WHERE col IS NOT NULL fails to filter NULL rows in matview projection.
--
-- Two failure modes, both reproduce on tursodb CLI 0.6.0-pre.23 with --experimental-views.
--
-- Run with:
--   rm -f /tmp/focus_roots_repro.db
--   tursodb --experimental-views /tmp/focus_roots_repro.db < holon_focus_roots_null_filter_2026-05-08.sql

-- ─────────────────────────────────────────────────────────────────────
-- Mode 1: column aliases + compound WHERE — NULL rows LEAK INTO matview
-- ─────────────────────────────────────────────────────────────────────
-- Mirrors holon's `focus_roots` matview shape (production query).

CREATE TABLE navigation_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    region TEXT NOT NULL,
    block_id TEXT,
    timestamp INTEGER NOT NULL,
    closed_at TEXT
);

CREATE MATERIALIZED VIEW focus_roots AS
SELECT
    region,
    block_id AS root_id,
    timestamp AS added_ts,
    id AS history_id
FROM navigation_history
WHERE closed_at IS NULL AND block_id IS NOT NULL;

INSERT INTO navigation_history (region, block_id, timestamp) VALUES ('main', NULL,      1000);
INSERT INTO navigation_history (region, block_id, timestamp) VALUES ('main', 'block:a', 1001);
INSERT INTO navigation_history (region, block_id, timestamp) VALUES ('main', 'block:b', 1002);
INSERT INTO navigation_history (region, block_id, timestamp) VALUES ('right', NULL,     1003);
INSERT INTO navigation_history (region, block_id, timestamp) VALUES ('right', 'block:c',1004);

SELECT '=== Mode 1: alias + compound WHERE — expected 3 rows, 0 null_leaked ===' AS label;
SELECT COUNT(*) AS row_count, COUNT(*) - COUNT(root_id) AS null_leaked FROM focus_roots;
-- ACTUAL: row_count = 5, null_leaked = 2

-- ─────────────────────────────────────────────────────────────────────
-- Mode 2: UPDATE value → NULL fails to remove row from matview
-- ─────────────────────────────────────────────────────────────────────

CREATE TABLE nav2 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    region TEXT NOT NULL,
    block_id TEXT,
    timestamp INTEGER NOT NULL,
    closed_at TEXT
);
CREATE MATERIALIZED VIEW focus_roots2 AS
SELECT region, block_id AS root_id, timestamp, id AS history_id
FROM nav2
WHERE closed_at IS NULL AND block_id IS NOT NULL;

INSERT INTO nav2 (region, block_id, timestamp) VALUES ('main', 'block:a', 1000);

SELECT '=== Mode 2: UPDATE value→NULL — before UPDATE expected 1 row ===' AS label;
SELECT COUNT(*) AS before_update FROM focus_roots2;

UPDATE nav2 SET block_id = NULL WHERE region = 'main';

SELECT '=== Mode 2: after UPDATE expected 0 rows (block_id is now NULL → IS NOT NULL filter excludes) ===' AS label;
SELECT COUNT(*) AS after_update, COUNT(*) - COUNT(root_id) AS null_leaked FROM focus_roots2;
-- ACTUAL: after_update = 1, null_leaked = 1 (row should have been removed)

-- ─────────────────────────────────────────────────────────────────────
-- Control: minimal `IS NOT NULL` matview without aliases — works fine
-- ─────────────────────────────────────────────────────────────────────
-- Demonstrates that the bug needs the column-alias projection. Without
-- aliases, a simple `SELECT col FROM t WHERE col IS NOT NULL` correctly
-- excludes NULL rows.

CREATE TABLE rows3 (id INTEGER PRIMARY KEY, payload TEXT);
CREATE MATERIALIZED VIEW non_null AS
SELECT id, payload FROM rows3 WHERE payload IS NOT NULL;

INSERT INTO rows3 (id, payload) VALUES (1, NULL);
INSERT INTO rows3 (id, payload) VALUES (2, 'real');
INSERT INTO rows3 (id, payload) VALUES (3, NULL);

SELECT '=== Control: no aliases, simple IS NOT NULL — expected 1 row ===' AS label;
SELECT COUNT(*) AS row_count FROM non_null;
-- ACTUAL: row_count = 1 ✓ (the bug needs the alias-shaped projection)
