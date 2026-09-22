# Handoff: Make Chained Materialized Views Reactive

## Problem

When a materialized view (MatView B) is built on top of another materialized view (MatView A), changes to the underlying tables that cause MatView A to update do NOT propagate to MatView B.

## Reproduction

```sql
-- Base tables
CREATE TABLE navigation_cursor (region TEXT PRIMARY KEY, history_id INTEGER);
CREATE TABLE navigation_history (id INTEGER PRIMARY KEY, region TEXT, block_id TEXT, timestamp TEXT);
CREATE TABLE blocks (id TEXT PRIMARY KEY, parent_id TEXT, content TEXT);

-- First MatView: joins cursor and history
CREATE MATERIALIZED VIEW nav_focus AS
SELECT nc.region, nh.block_id, nh.timestamp
FROM navigation_cursor nc
JOIN navigation_history nh ON nc.history_id = nh.id;

-- Second MatView: joins blocks with first MatView
CREATE MATERIALIZED VIEW main_blocks AS
SELECT b.id, b.content, b.parent_id
FROM blocks b
JOIN nav_focus nf ON b.parent_id = nf.block_id
WHERE nf.region = 'main';

-- Insert test data
INSERT INTO navigation_history (id, region, block_id) VALUES (1, 'main', 'doc-A');
UPDATE navigation_cursor SET history_id = 1 WHERE region = 'main';
INSERT INTO blocks (id, parent_id, content) VALUES ('block-1', 'doc-A', 'Content A');
INSERT INTO blocks (id, parent_id, content) VALUES ('block-2', 'doc-B', 'Content B');

-- At this point:
-- nav_focus shows: region='main', block_id='doc-A'
-- main_blocks shows: block-1

-- Now change navigation
INSERT INTO navigation_history (id, region, block_id) VALUES (2, 'main', 'doc-B');
UPDATE navigation_cursor SET history_id = 2 WHERE region = 'main';

-- EXPECTED:
-- nav_focus shows: region='main', block_id='doc-B'  ✓ (works)
-- main_blocks shows: block-2                        ✗ (still shows block-1)

-- ACTUAL:
-- nav_focus updates correctly
-- main_blocks does NOT update - still shows old data
```

## Expected Behavior

When the base tables change and MatView A updates, MatView B should also update to reflect the new data from MatView A.

## Technical Context

- This is Turso's IVM (Incremental View Materialization) feature
- MatViews are implemented using DBSP (Database Stream Processing)
- The issue is that the dependency chain between MatViews isn't being tracked/propagated
- Relevant code likely in `core/` directory, particularly around:
  - `translate/view.rs` - MatView translation
  - DBSP integration for incremental updates

## Use Case

We're building a PKM (Personal Knowledge Management) app where:
1. `navigation_cursor` + `navigation_history` track what document the user is viewing
2. `nav_focus` MatView provides the current focus per UI region
3. `main_blocks` MatView shows blocks belonging to the currently focused document

When the user clicks to navigate to a different document, we need the main view to reactively update to show that document's content.

## Success Criteria

1. Changes to base tables propagate through the entire MatView dependency chain
2. MatView B updates when MatView A updates (not just when base tables change)
3. No manual refresh/recreation of dependent MatViews required
