-- Issue #72 fixture: child-cardinality modeling (MySQL 8+).
--
-- card_parent: 100 keys.
-- card_child : 70 rows whose per-parent counts are exactly
--              {0: 0.5, 1: 0.3, 2: 0.2}  (50 / 30 / 20 parents).
--
-- Loaded by tests/synth-verify/run_m4_cardinality.sh. Safe to re-run.

DROP TABLE IF EXISTS card_child;
DROP TABLE IF EXISTS card_parent;

CREATE TABLE card_parent (
  id INT PRIMARY KEY,
  payload VARCHAR(16) NOT NULL
);

CREATE TABLE card_child (
  id INT PRIMARY KEY,
  parent_id INT NULL,
  payload VARCHAR(16) NOT NULL,
  CONSTRAINT fk_card_child_parent FOREIGN KEY (parent_id) REFERENCES card_parent (id)
);

INSERT INTO card_parent (id, payload)
WITH RECURSIVE seq(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM seq WHERE i < 99)
SELECT i + 1, CONCAT('p', i + 1) FROM seq;

-- 30 parents (1..30) with exactly one child: ids 1..30.
INSERT INTO card_child (id, parent_id, payload)
WITH RECURSIVE seq(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM seq WHERE i < 29)
SELECT i + 1, i + 1, CONCAT('c', i + 1) FROM seq;

-- 20 parents (31..50) with exactly two children: ids 31..70.
INSERT INTO card_child (id, parent_id, payload)
WITH RECURSIVE seq(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM seq WHERE i < 39)
SELECT 31 + i, 31 + (i DIV 2), CONCAT('c', 31 + i) FROM seq;
