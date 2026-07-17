-- Bug: MatchCounterOperator::eval called with Uninitialized state
-- Turso commit:  81cef68c (branch nightscape@holon)
-- File:          core/incremental/match_counter_operator.rs:378
--
-- Self-contained reproducer. Run with:
--    cargo run -p turso_core --bin tursodb -- /tmp/match_counter_repro.db < match_counter_uninitialized_repro.sql
-- (or the equivalent for your local Turso CLI build)
--
-- See match_counter_uninitialized_repro.md for the analysis + stack trace.
--
-- The bug surfaces when a LEFT JOIN matview's IVM eval path suspends on
-- I/O (`return_if_io!`) inside `process_match_counter_state` — the inner
-- arms at match_counter_operator.rs:484 (read_r_count) and 531
-- (read_next_join_row) return without restoring `*outer`, so the next
-- call sees `EvalState::Uninitialized` and panics.
--
-- The shape below mirrors holon's `block` matview: ONE entity table +
-- TWO LEFT-JOINed junction tables + json_group_array + GROUP BY.
-- Each LEFT JOIN spawns its own MatchCounterOperator; the dual-LEFT
-- shape doubles the chance of hitting an I/O suspension during a single
-- commit cycle.

-- =====================================================================
-- Schema
-- =====================================================================

CREATE TABLE block_raw (
  id      TEXT PRIMARY KEY,
  parent_id TEXT,
  content TEXT NOT NULL DEFAULT ''
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

-- Dual LEFT JOIN matview — each LEFT JOIN spawns one MatchCounterOperator.
CREATE MATERIALIZED VIEW block_matview AS
SELECT
  b.id,
  b.parent_id,
  b.content,
  COALESCE(json_group_array(bt.tag)        FILTER (WHERE bt.tag        IS NOT NULL), '[]') AS tags,
  COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by
FROM block_raw b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
GROUP BY b.id, b.parent_id, b.content;

-- =====================================================================
-- Trigger: many small CDC cycles, each one a chance to suspend on I/O
-- mid-eval. Each separate INSERT/DELETE statement is its own commit,
-- so the matview re-runs MatchCounterOperator::eval per statement.
-- =====================================================================

INSERT INTO block_raw (id, content) VALUES ('a', 'a-content');
INSERT INTO block_raw (id, content) VALUES ('b', 'b-content');
INSERT INTO block_raw (id, content) VALUES ('c', 'c-content');
INSERT INTO block_raw (id, content) VALUES ('d', 'd-content');
INSERT INTO block_raw (id, content) VALUES ('e', 'e-content');

INSERT INTO block_tags (block_id, tag) VALUES ('a', 'page');
INSERT INTO block_tags (block_id, tag) VALUES ('a', 'wip');
INSERT INTO block_tags (block_id, tag) VALUES ('b', 'page');
INSERT INTO block_tags (block_id, tag) VALUES ('c', 'page');
INSERT INTO block_tags (block_id, tag) VALUES ('d', 'archive');

INSERT INTO task_blockers (blocked_id, blocker_id) VALUES ('a', 'b');
INSERT INTO task_blockers (blocked_id, blocker_id) VALUES ('a', 'c');
INSERT INTO task_blockers (blocked_id, blocker_id) VALUES ('b', 'c');

-- These DELETEs are the most reliable trigger: they cause the per-key
-- R_COUNT to cross zero, forcing MatchCounterOperator into the
-- ScanningL phase (the second `return_if_io!` site at line 531).
DELETE FROM block_tags    WHERE block_id   = 'a';
DELETE FROM block_tags    WHERE block_id   = 'b';
DELETE FROM task_blockers WHERE blocked_id = 'a';
DELETE FROM task_blockers WHERE blocked_id = 'b';

-- Re-insert + re-delete to push more cycles through the operator.
INSERT INTO block_tags (block_id, tag) VALUES ('a', 'page');
DELETE FROM block_tags WHERE block_id = 'a';
INSERT INTO block_tags (block_id, tag) VALUES ('a', 'page');
DELETE FROM block_tags WHERE block_id = 'a';

-- Read it back — at this point the matview's IVM state should be
-- corrupted if the bug fires. If the view returns the wrong rows here
-- (or panicked above), the bug is reproduced.
SELECT id, tags, blocked_by FROM block_matview ORDER BY id;
