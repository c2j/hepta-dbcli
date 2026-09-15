-- M1 synth end-to-end fixture (MySQL 8+).
--
-- Two tables with a foreign key, covering every column class the M1 issues
-- touch: datetimes, fixed-scale decimals, nullable columns, a low-cardinality
-- label, a 120-level dictionary column and a non-unique FK.
--
-- Loaded by tests/synth-verify/run_m1.sh. Safe to re-run: it drops its own
-- tables first. The numbers here must stay in sync with run_m1.sh and
-- verify_m1.py (see the constants block in each).

SET SESSION cte_max_recursion_depth = 10000;

DROP TABLE IF EXISTS m1_verify_child;
DROP TABLE IF EXISTS m1_verify_parent;

CREATE TABLE m1_verify_parent (
  id INT PRIMARY KEY,
  trade_time TIMESTAMP NOT NULL,
  cjje DECIMAL(18,4) NOT NULL,
  discount_rate DECIMAL(4,2) NULL,
  email VARCHAR(64) NULL,
  status VARCHAR(16) NOT NULL,
  code VARCHAR(16) NOT NULL
);

CREATE TABLE m1_verify_child (
  id INT PRIMARY KEY,
  parent_id INT NOT NULL,
  note VARCHAR(32) NULL,
  CONSTRAINT fk_m1_verify_child_parent
    FOREIGN KEY (parent_id) REFERENCES m1_verify_parent(id)
);

-- 2000 parent rows: trade_time advances 60000s per row (trading days are not
-- modelled; the point is a monotonic timestamp range), cjje is 4-place and
-- trends with time, discount_rate is 2-place, email is NULL every 5th row,
-- status cycles 5 levels, code cycles 120 dictionary levels.
INSERT INTO m1_verify_parent (id, trade_time, cjje, discount_rate, email, status, code)
WITH RECURSIVE seq(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM seq WHERE i < 1999)
SELECT i + 1,
       TIMESTAMPADD(SECOND, i * 60000, '2020-01-01 00:00:00'),
       ROUND(1000 + i * 0.7 + (i % 97) * 0.13, 4),
       (i % 100) / 100.0,
       IF(i % 5 = 0, NULL, CONCAT('user', i, '@example.com')),
       ELT(1 + (i % 5), 'open', 'closed', 'pending', 'cancelled', 'draft'),
       CONCAT('C', LPAD(i % 120, 3, '0'))
FROM seq;

-- 1000 child rows referencing only 700 distinct parents on purpose: the FK is
-- NOT unique, so rules-draft must draft unique: false and generation must
-- sample with replacement from the parent pool.
INSERT INTO m1_verify_child (id, parent_id, note)
WITH RECURSIVE seq(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM seq WHERE i < 999)
SELECT i + 1, 1 + (i % 700), IF(i % 7 = 0, NULL, CONCAT('n', i))
FROM seq;
