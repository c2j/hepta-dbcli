-- Issue #82 fixture: primary-key uniqueness on generated data (MySQL 8+).
--
-- Three tables whose primary keys cannot simply be redrawn for the row counts
-- used by run_m4_pk.sh:
--   * pk_verify_single.id    -> 12 integer levels; scale-up extrapolates ids
--   * pk_verify_composite    -> 20 x 20 = 400 tuples (each member repeats alone)
--   * pk_verify_text.code    -> 3 non-numeric levels; cannot be extended, so
--                               generating more rows than that must fail
--
-- Loaded by tests/synth-verify/run_m4_pk.sh, which drops and recreates these
-- tables. Safe to re-run.

DROP TABLE IF EXISTS pk_verify_text;
DROP TABLE IF EXISTS pk_verify_composite;
DROP TABLE IF EXISTS pk_verify_single;

CREATE TABLE pk_verify_single (
  id INT PRIMARY KEY,
  payload VARCHAR(16) NOT NULL
);

CREATE TABLE pk_verify_composite (
  region VARCHAR(2) NOT NULL,
  slot INT NOT NULL,
  payload VARCHAR(16) NOT NULL,
  PRIMARY KEY (region, slot)
);

CREATE TABLE pk_verify_text (
  code VARCHAR(8) PRIMARY KEY,
  payload VARCHAR(16) NOT NULL
);

INSERT INTO pk_verify_single (id, payload) VALUES
  (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e'), (6, 'f'),
  (7, 'g'), (8, 'h'), (9, 'i'), (10, 'j'), (11, 'k'), (12, 'l');

-- 400 tuples: 'A'..'T' x 1..20, so each member repeats heavily on its own.
INSERT INTO pk_verify_composite (region, slot, payload)
WITH RECURSIVE seq(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM seq WHERE i < 399)
SELECT CHAR(65 + (i DIV 20)), 1 + (i % 20), CONCAT('p', i)
FROM seq;

INSERT INTO pk_verify_text (code, payload) VALUES
  ('alpha', 'a'), ('beta', 'b'), ('gamma', 'c');
