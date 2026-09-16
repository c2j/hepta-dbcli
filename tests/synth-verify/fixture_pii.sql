-- Issue #71 fixture: PII columns (MySQL 8+).
--
-- 200 rows over 50 distinct emails, 200 distinct phones and 30 distinct names,
-- so "masked values already existed" and "distinct fake count" are both easy
-- to assert. Loaded by tests/synth-verify/run_m4_pii.sh. Safe to re-run.

DROP TABLE IF EXISTS pii_users;
DROP TABLE IF EXISTS pii_logins;
DROP TABLE IF EXISTS pii_accounts;

CREATE TABLE pii_users (
  id INT PRIMARY KEY,
  email VARCHAR(64) NULL,
  phone VARCHAR(24) NULL,
  full_name VARCHAR(64) NULL,
  note VARCHAR(16) NOT NULL
);

INSERT INTO pii_users (id, email, phone, full_name, note)
WITH RECURSIVE seq(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM seq WHERE i < 199)
SELECT i + 1,
       CONCAT('user', i % 50, '@corp-example.cn'),
       CONCAT('+86-139', LPAD(i % 200, 8, '0')),
       CONCAT('Person', i % 30),
       CONCAT('n', i)
FROM seq;

-- A natural-key PII parent: `email` is the primary key and is referenced by a
-- child, so anonymization has to keep it unique and referencable.
CREATE TABLE pii_accounts (
  email VARCHAR(64) PRIMARY KEY,
  note VARCHAR(16) NOT NULL
);

CREATE TABLE pii_logins (
  id INT PRIMARY KEY,
  account_email VARCHAR(64) NULL,
  CONSTRAINT fk_pii_logins_account
    FOREIGN KEY (account_email) REFERENCES pii_accounts (email)
);

INSERT INTO pii_accounts (email, note)
WITH RECURSIVE seq(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM seq WHERE i < 39)
SELECT CONCAT('owner', i, '@corp-example.cn'), CONCAT('a', i)
FROM seq;

INSERT INTO pii_logins (id, account_email)
WITH RECURSIVE seq(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM seq WHERE i < 79)
SELECT i + 1, CONCAT('owner', i % 40, '@corp-example.cn')
FROM seq;
