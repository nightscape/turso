CREATE TABLE block (id TEXT PRIMARY KEY, content TEXT);
CREATE TABLE block_tags (
  block_id TEXT NOT NULL, tag TEXT NOT NULL,
  PRIMARY KEY (block_id, tag),
  FOREIGN KEY (block_id) REFERENCES block (id) ON DELETE CASCADE
);
CREATE TABLE task_blockers (
  blocked_id TEXT NOT NULL, blocker_id TEXT NOT NULL,
  PRIMARY KEY (blocked_id, blocker_id),
  FOREIGN KEY (blocked_id) REFERENCES block (id) ON DELETE CASCADE,
  FOREIGN KEY (blocker_id) REFERENCES block (id) ON DELETE CASCADE
);

INSERT INTO block VALUES ('block:a','alpha'),('block:b','bravo'),('block:f','foxtrot');
INSERT INTO block_tags VALUES ('block:a','urgent');
INSERT INTO task_blockers VALUES ('block:b','block:a');

SELECT '--- plain SELECT (no matview) — does block:f appear? ---' AS marker;
SELECT b.id, bt.tag, tb.blocker_id
FROM block b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
ORDER BY b.id;

SELECT '--- plain SELECT with GROUP BY + aggregate ---' AS marker;
SELECT b.id,
  COALESCE(json_group_array(bt.tag) FILTER (WHERE bt.tag IS NOT NULL), '[]') AS tags,
  COALESCE(json_group_array(tb.blocker_id) FILTER (WHERE tb.blocker_id IS NOT NULL), '[]') AS blocked_by
FROM block b
LEFT OUTER JOIN block_tags    bt ON bt.block_id   = b.id
LEFT OUTER JOIN task_blockers tb ON tb.blocked_id = b.id
GROUP BY b.id
ORDER BY b.id;
