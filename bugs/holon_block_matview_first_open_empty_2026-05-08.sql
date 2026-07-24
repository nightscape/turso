-- Standalone SQL repro for the matview-first-open bug seen in holon.
--
-- Setup:
--   - block_raw is a base table with 1000 rows.
--   - block_tags / block_requires are sparse junction tables.
--   - `block` is a materialised view (dual LEFT JOIN + json_group_array
--     + GROUP BY) that hydrates the edge-typed fields back onto block_raw.
--
-- Bug:
--   - First `SELECT COUNT(*) FROM block;` returns a small subset (28-54
--     rows in repro runs).
--   - Second `SELECT COUNT(*) FROM block;` (identical SQL, same connection)
--     returns the full 1000 rows.
--   - All subsequent reads also return 1000.
--
-- Reproduces in pure Turso (no holon code on top) — see the example at
-- `crates/holon/examples/turso_ivm_matview_first_open_empty_repro.rs`.
-- All four scenarios reproduce the bug:
--   A. matview-on-empty-base, single-connection: first=54, second=1000
--   B. matview-on-empty-base, fresh-connection : first=28, second=1000
--   C. matview-on-populated-base, single-conn  : first=28, second=1000
--   D. matview-on-populated-base, fresh-conn   : first=28, second=1000
--
-- Filtered queries (e.g. WHERE json_extract(properties, '$.gate') = 'G1')
-- happen to return consistent counts on first and second reads in this
-- repro (29==29), but holon's MCP server sees the same 0-then-N pattern
-- on a more complex filtered query (NOT EXISTS subqueries against the
-- matview itself); the underlying matview cursor laziness is the same.
--
-- Adjacent fixes that did NOT cover this case:
--   - 7cf0a2e68a3a (Turso): MaterializedViewCursor::ensure_tx_changes_computed
--     walking upstream matview deltas — only fires inside an open txn,
--     this repro is autocommit.
--   - 05c326752ff (nightscape@holon): IVM LEFT JOIN drops null-padded row
--     on redundant UPDATE — fixed but unrelated.

CREATE TABLE block_raw (
    id TEXT PRIMARY KEY,
    parent_id TEXT,
    depth INTEGER NOT NULL DEFAULT 0,
    sort_key TEXT NOT NULL DEFAULT 'A0',
    content TEXT NOT NULL DEFAULT '',
    content_type TEXT NOT NULL DEFAULT 'text',
    source_language TEXT,
    source_name TEXT,
    properties TEXT,
    marks TEXT,
    collapsed INTEGER NOT NULL DEFAULT 0,
    completed INTEGER NOT NULL DEFAULT 0,
    block_type TEXT NOT NULL DEFAULT 'text',
    created_at INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT 0,
    _change_origin TEXT
);

CREATE TABLE block_tags (
    block_id TEXT NOT NULL,
    tag TEXT NOT NULL,
    PRIMARY KEY (block_id, tag)
);

CREATE TABLE block_requires (
    block_id TEXT NOT NULL,
    required_id TEXT NOT NULL,
    PRIMARY KEY (block_id, required_id)
);

-- Matview created on EMPTY base tables, mirroring the holon DI bootstrap.
CREATE MATERIALIZED VIEW block AS
    SELECT
        b.id, b.parent_id, b.depth, b.sort_key, b.content, b.content_type,
        b.source_language, b.source_name, b.properties, b.marks, b.collapsed,
        b.completed, b.block_type, b.created_at, b.updated_at, b._change_origin,
        COALESCE(json_group_array(bt.tag)         FILTER (WHERE bt.tag         IS NOT NULL), '[]') AS tags,
        COALESCE(json_group_array(br.required_id) FILTER (WHERE br.required_id IS NOT NULL), '[]') AS requires
    FROM block_raw b
    LEFT OUTER JOIN block_tags     bt ON bt.block_id = b.id
    LEFT OUTER JOIN block_requires br ON br.block_id = b.id
    GROUP BY
        b.id, b.parent_id, b.depth, b.sort_key, b.content, b.content_type,
        b.source_language, b.source_name, b.properties, b.marks, b.collapsed,
        b.completed, b.block_type, b.created_at, b.updated_at, b._change_origin;

-- Now CDC-populate the matview by inserting into the base table.
-- (In the .rs reproducer this is 1000 rows; the bug reproduces with as
-- few as ~50, but a larger N gives a more obvious "first-call partial,
-- second-call full" delta.)
INSERT INTO block_raw (id, parent_id, content, properties) VALUES ('block:row-00000', 'doc:root', 'C0', '{"task_state":"TODO","gate":"G1","priority":1,"effort":1}');
INSERT INTO block_raw (id, parent_id, content, properties) VALUES ('block:row-00001', 'block:row-00000', 'C1', '{"task_state":"DONE","gate":"G0","priority":1,"effort":1}');
INSERT INTO block_raw (id, parent_id, content, properties) VALUES ('block:row-00002', 'block:row-00000', 'C2', '{"task_state":"DONE","gate":"G0","priority":1,"effort":1}');
-- ... (extend to 1000 rows in the repro binary; this header is enough to
-- demonstrate; real repro needs 50+ rows for the gap to be observable)
-- See the .rs example for the complete loop.

-- Sanity check: base table is fully populated.
SELECT COUNT(*) FROM block_raw;       -- 1000 in the .rs repro

-- BUG: first SELECT against the matview returns a partial subset.
SELECT COUNT(*) FROM block;           -- expected 1000, returns ~28-54

-- Second SELECT against the matview, no other writes in between,
-- returns the full count.
SELECT COUNT(*) FROM block;           -- returns 1000

-- Workaround on the holon side: issue a `SELECT 1 FROM block LIMIT 0`
-- as a startup warmup *after* schema reconcile + initial ingest, so the
-- first user-visible matview cursor open is never the cold one.
