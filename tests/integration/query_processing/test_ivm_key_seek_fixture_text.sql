-- The statements that wrote test_ivm_key_seek_fixture_text.db, run by the engine at commit
-- ac81f0251738 (nightscape/turso) through TempDatabase with views and custom
-- types enabled, after PRAGMA page_size = 512 (keeps the file small), then
-- PRAGMA wal_checkpoint(TRUNCATE).
CREATE TABLE probe (x);
CREATE INDEX probe_x ON probe (x);
CREATE TABLE node(id TEXT PRIMARY KEY, parent TEXT, label TEXT);
CREATE INDEX node_parent ON node(parent);
INSERT INTO node VALUES ('b0', NULL, 'a');
INSERT INTO node VALUES ('b1', NULL, 'a');
INSERT INTO node VALUES ('b2', 'b0', 'a');
INSERT INTO node VALUES ('b3', 'b0', 'a');
INSERT INTO node VALUES ('b4', 'b1', 'a');
INSERT INTO node VALUES ('b5', 'b1', 'a');
CREATE MATERIALIZED VIEW rv AS WITH RECURSIVE paths AS ( SELECT id, parent, label, '/' || id AS path, 0 AS depth FROM node WHERE parent IS NULL UNION ALL SELECT b.id, b.parent, b.label, p.path || '/' || b.id, p.depth + 1 FROM node b JOIN paths p ON b.parent = p.id WHERE p.depth < 3) SELECT * FROM paths;
CREATE MATERIALIZED VIEW mm_in AS SELECT id, parent, label FROM node WHERE label IS NOT 'x';
CREATE MATERIALIZED VIEW mm AS SELECT c.id AS child, p.id AS up, p.label FROM mm_in c JOIN mm_in p ON c.parent = p.id;
CREATE INDEX mm_in_parent ON mm_in(parent);
CREATE INDEX mm_in_id ON mm_in(id);
CREATE MATERIALIZED VIEW mr_in AS WITH RECURSIVE paths AS ( SELECT id, parent, label, '/' || id AS path, 0 AS depth FROM node WHERE parent IS NULL UNION ALL SELECT b.id, b.parent, b.label, p.path || '/' || b.id, p.depth + 1 FROM node b JOIN paths p ON b.parent = p.id WHERE p.depth < 3) SELECT * FROM paths;
CREATE MATERIALIZED VIEW mr AS SELECT c.id AS child, p.path AS up_path FROM mr_in c JOIN mr_in p ON c.parent = p.id;
CREATE INDEX mr_in_parent ON mr_in(parent);
CREATE INDEX mr_in_id ON mr_in(id);
CREATE MATERIALIZED VIEW pj AS SELECT a.id AS child, b.id AS up, b.label FROM node a JOIN node b ON a.parent = b.id;
CREATE MATERIALIZED VIEW rm_in AS SELECT id, parent, label FROM node WHERE label IS NOT 'x';
CREATE MATERIALIZED VIEW rm AS WITH RECURSIVE paths AS ( SELECT id, parent, label, '/' || id AS path, 0 AS depth FROM rm_in WHERE parent IS NULL UNION ALL SELECT b.id, b.parent, b.label, p.path || '/' || b.id, p.depth + 1 FROM rm_in b JOIN paths p ON b.parent = p.id WHERE p.depth < 3) SELECT * FROM paths;
CREATE INDEX rm_in_parent ON rm_in(parent);
CREATE INDEX rm_in_id ON rm_in(id);
CREATE TABLE dnode(id numeric(10,2), parent numeric(10,2), label TEXT) STRICT;
INSERT INTO dnode VALUES (0.25, NULL, 'n0');
INSERT INTO dnode VALUES (1.25, NULL, 'n1');
INSERT INTO dnode VALUES (2.25, 0.25, 'n2');
INSERT INTO dnode VALUES (3.25, 0.25, 'n3');
INSERT INTO dnode VALUES (4.25, 1.25, 'n4');
INSERT INTO dnode VALUES (5.25, 1.25, 'n5');
CREATE MATERIALIZED VIEW dv AS SELECT a.label AS child, b.label AS up FROM dnode a JOIN dnode b ON a.parent = b.id;
