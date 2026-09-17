-- DAT_FUND_CJQS_bak: same 14-col PK as production, no partitions, a few days.
-- 80_000 rows/day × 3 days = 240_000. Day under test: BCRQ='20251215'.

DROP TABLE IF EXISTS gaussdb.dat_fund_cjqs_bak_r;
DROP TABLE IF EXISTS gaussdb.dat_fund_cjqs_bak;

CREATE TABLE gaussdb.dat_fund_cjqs_bak (
    xwdm          VARCHAR(7)  NOT NULL,
    security_id   VARCHAR(19) NOT NULL,
    bs            VARCHAR(2)  NOT NULL,
    trade_type    VARCHAR(2)  NOT NULL DEFAULT '0',
    bcrq          VARCHAR(8)  NOT NULL,
    cjsl          NUMERIC(15,2),
    cjje          NUMERIC(15,3),
    yhs           NUMERIC(15,2) NOT NULL DEFAULT 0,
    jsf           NUMERIC(15,2) NOT NULL DEFAULT 0,
    jiesf         NUMERIC(15,2) NOT NULL DEFAULT 0,
    zgf           NUMERIC(15,2) NOT NULL DEFAULT 0,
    ghf           NUMERIC(15,2) NOT NULL DEFAULT 0,
    yj            NUMERIC(20,8) NOT NULL DEFAULT 0,
    fxj           NUMERIC(20,2) NOT NULL DEFAULT 0,
    accrual       NUMERIC(20,8) NOT NULL DEFAULT 0,
    pay_type      VARCHAR(2)  NOT NULL DEFAULT '0',
    fund_code     VARCHAR(8)  NOT NULL,
    stock_kind    VARCHAR(4)  NOT NULL,
    mrcb          NUMERIC(15,2),
    scdm          VARCHAR(3)  NOT NULL,
    main_xwdm     VARCHAR(6)  NOT NULL,
    ghf_dealer    NUMERIC(20,8) NOT NULL DEFAULT 0,
    accural_tax   NUMERIC(20,8),
    etf_flag      VARCHAR(1)  NOT NULL DEFAULT '0',
    gddm          VARCHAR(16) NOT NULL DEFAULT '0',
    gddmzm        VARCHAR(12) NOT NULL DEFAULT '0',
    inst_num      NUMERIC(24,0),
    inst_data_date VARCHAR(8),
    account_flag  VARCHAR(1),
    check_type    VARCHAR(4)  NOT NULL DEFAULT '0',
    gh_type       VARCHAR(1)  NOT NULL DEFAULT '0',
    cjbz          VARCHAR(3),
    sxf           NUMERIC(15,2),
    mom_fund      VARCHAR(8)  NOT NULL DEFAULT '0',
    CONSTRAINT pk_fund_cjqs_bak PRIMARY KEY (
        xwdm, security_id, scdm, fund_code, trade_type, bs, pay_type,
        stock_kind, bcrq, etf_flag, gddm, gddmzm, check_type, mom_fund
    )
);

INSERT INTO gaussdb.dat_fund_cjqs_bak (
    xwdm, security_id, bs, trade_type, bcrq,
    cjsl, cjje, yhs, jsf, jiesf, zgf, ghf, yj, fxj, accrual,
    pay_type, fund_code, stock_kind, mrcb, scdm, main_xwdm,
    ghf_dealer, accural_tax, etf_flag, gddm, gddmzm, inst_num,
    inst_data_date, account_flag, check_type, gh_type, cjbz, sxf, mom_fund
)
SELECT
    lpad((i % 100)::text, 7, '0'),
    lpad(i::text, 19, '0'),
    CASE WHEN i % 2 = 0 THEN 'B' ELSE 'S' END,
    '01',
    d.bcrq,
    ((i % 1000) + 0.10)::numeric(15,2),
    ((i % 10000) * 0.123)::numeric(15,3),
    ((i % 50) * 0.01)::numeric(15,2),
    ((i % 30) * 0.02)::numeric(15,2),
    ((i % 20) * 0.03)::numeric(15,2),
    ((i % 10) * 0.04)::numeric(15,2),
    ((i % 15) * 0.05)::numeric(15,2),
    ((i % 100) * 0.00000012)::numeric(20,8),
    ((i % 200) * 0.01)::numeric(20,2),
    ((i % 80) * 0.00000034)::numeric(20,8),
    '01',
    lpad((i % 20)::text, 8, '0'),
    'A',
    ((i % 500) * 0.5)::numeric(15,2),
    '001',
    lpad((i % 50)::text, 6, '0'),
    ((i % 7) * 0.00000011)::numeric(20,8),
    ((i % 9) * 0.00000022)::numeric(20,8),
    '0',
    lpad((i % 500)::text, 16, '0'),
    lpad((i % 80)::text, 12, '0'),
    (i % 100000)::numeric(24,0),
    d.bcrq,
    '1',
    '0000',
    '0',
    'CNY',
    ((i % 25) * 0.06)::numeric(15,2),
    '0'
FROM (VALUES ('20251215'), ('20251216'), ('20251217')) AS d(bcrq)
CROSS JOIN generate_series(1, 80000) AS i;

CREATE TABLE gaussdb.dat_fund_cjqs_bak_r (LIKE gaussdb.dat_fund_cjqs_bak INCLUDING ALL);
INSERT INTO gaussdb.dat_fund_cjqs_bak_r SELECT * FROM gaussdb.dat_fund_cjqs_bak;

ANALYZE gaussdb.dat_fund_cjqs_bak;
ANALYZE gaussdb.dat_fund_cjqs_bak_r;

SELECT 'bak' AS side, bcrq, COUNT(*) FROM gaussdb.dat_fund_cjqs_bak GROUP BY bcrq ORDER BY 2;
SELECT 'bak_r' AS side, bcrq, COUNT(*) FROM gaussdb.dat_fund_cjqs_bak_r GROUP BY bcrq ORDER BY 2;
