-- The statements that wrote test_ivm_key_seek_fixture_array_view_index.db, run by
-- the engine at commit ac81f0251738 (nightscape/turso) through TempDatabase with
-- views and custom types enabled, after PRAGMA page_size = 512, then
-- PRAGMA wal_checkpoint(TRUNCATE).
CREATE TABLE a(id INTEGER PRIMARY KEY, ints INTEGER[], words TEXT[], label TEXT) STRICT;
INSERT INTO a VALUES (1, '{-5,2}', '{b,a}', 'x1');
INSERT INTO a VALUES (2, '{10}', '{a}', 'x2');
INSERT INTO a VALUES (3, '{2}', '{ab}', 'x3');
INSERT INTO a VALUES (4, '{-1}', '{Z}', 'x4');
INSERT INTO a VALUES (5, '{3,1}', '{a,b,c}', 'x5');
INSERT INTO a VALUES (6, '{100,-3}', '{}', 'x6');
INSERT INTO a VALUES (7, '{}', '{zz}', 'x7');
INSERT INTO a VALUES (8, '{7,7,7}', '{ab,b}', 'x8');
CREATE MATERIALIZED VIEW am AS SELECT id, ints, words, label FROM a;
CREATE INDEX am_ints ON am(ints);
CREATE INDEX am_words ON am(words);
