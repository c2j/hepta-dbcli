\timing on
SET statement_timeout = 120000;

-- 1) COUNT day
EXPLAIN (ANALYZE, BUFFERS, VERBOSE)
SELECT COUNT(*) FROM gaussdb.dat_fund_cjqs_bak WHERE bcrq = '20251215';

-- 2) batch checksum (exact keyeddiff SQL, one side)
EXPLAIN (ANALYZE, BUFFERS)
SELECT MOD(('x' || SUBSTR(kh, 1, 8))::bit(32)::bigint, 5) AS bkt,
  COUNT(*) AS cnt,
  MOD(SUM(('x' || SUBSTR(h,  1, 8))::bit(32)::bigint), 18446744073709551616) AS s1,
  MOD(SUM(('x' || SUBSTR(h,  9, 8))::bit(32)::bigint), 18446744073709551616) AS s2,
  MOD(SUM(('x' || SUBSTR(h, 17, 8))::bit(32)::bigint), 18446744073709551616) AS s3,
  MOD(SUM(('x' || SUBSTR(h, 25, 8))::bit(32)::bigint), 18446744073709551616) AS s4
FROM (
  SELECT MD5(concat_ws('#', "xwdm", "security_id", "scdm", "fund_code", "trade_type", "bs", "pay_type", "stock_kind", "bcrq", "etf_flag", "gddm", "gddmzm", "check_type", "mom_fund")) AS kh,
         MD5(concat_ws('#', "xwdm", "security_id", "scdm", "fund_code", "trade_type", "bs", "pay_type", "stock_kind", "bcrq", "etf_flag", "gddm", "gddmzm", "check_type", "mom_fund", COALESCE("cjsl"::text, 'N'), COALESCE("cjje"::text, 'N'), "yhs"::text, "jsf"::text, "jiesf"::text, "zgf"::text, "ghf"::text, "yj"::text, "fxj"::text, "accrual"::text, COALESCE("mrcb"::text, 'N'), "main_xwdm", "ghf_dealer"::text, COALESCE("accural_tax"::text, 'N'), COALESCE("inst_num"::text, 'N'), COALESCE("inst_data_date", 'N'), COALESCE("account_flag", 'N'), "gh_type", COALESCE("cjbz", 'N'), COALESCE("sxf"::text, 'N'))) AS h
  FROM gaussdb.dat_fund_cjqs_bak
  WHERE bcrq = '20251215'
) t
GROUP BY MOD(('x' || SUBSTR(kh, 1, 8))::bit(32)::bigint, 5);

-- 3) key-only MD5 checksum (probe: skip value columns)
EXPLAIN (ANALYZE, BUFFERS)
SELECT MOD(('x' || SUBSTR(MD5(concat_ws('#', xwdm, security_id, scdm, fund_code, trade_type, bs, pay_type, stock_kind, bcrq, etf_flag, gddm, gddmzm, check_type, mom_fund)), 1, 8))::bit(32)::bigint, 5) AS bkt,
       COUNT(*),
       MOD(SUM(('x' || SUBSTR(MD5(concat_ws('#', xwdm, security_id, cjje::text)), 1, 8))::bit(32)::bigint), 18446744073709551616)
FROM gaussdb.dat_fund_cjqs_bak
WHERE bcrq = '20251215'
GROUP BY 1;

-- 4) dirty-bucket pull page 1 (MD5 predicate + COLLATE C sort)
EXPLAIN (ANALYZE, BUFFERS)
SELECT xwdm, security_id, scdm, fund_code, trade_type, bs, pay_type, stock_kind, bcrq, etf_flag, gddm, gddmzm, check_type, mom_fund, cjje
FROM gaussdb.dat_fund_cjqs_bak
WHERE bcrq = '20251215'
  AND MOD(('x' || SUBSTR(MD5(concat_ws('#', xwdm, security_id, scdm, fund_code, trade_type, bs, pay_type, stock_kind, bcrq, etf_flag, gddm, gddmzm, check_type, mom_fund)), 1, 8))::bit(32)::bigint, 5) = 0
ORDER BY xwdm COLLATE "C", security_id COLLATE "C", scdm COLLATE "C", fund_code COLLATE "C",
         trade_type COLLATE "C", bs COLLATE "C", pay_type COLLATE "C", stock_kind COLLATE "C",
         bcrq COLLATE "C", etf_flag COLLATE "C", gddm COLLATE "C", gddmzm COLLATE "C",
         check_type COLLATE "C", mom_fund COLLATE "C"
LIMIT 8192;

-- 5) FETCH_ALL page 1 with COLLATE C (current keyeddiff)
EXPLAIN (ANALYZE, BUFFERS)
SELECT *
FROM gaussdb.dat_fund_cjqs_bak
WHERE bcrq = '20251215'
ORDER BY xwdm COLLATE "C", security_id COLLATE "C", scdm COLLATE "C", fund_code COLLATE "C",
         trade_type COLLATE "C", bs COLLATE "C", pay_type COLLATE "C", stock_kind COLLATE "C",
         bcrq COLLATE "C", etf_flag COLLATE "C", gddm COLLATE "C", gddmzm COLLATE "C",
         check_type COLLATE "C", mom_fund COLLATE "C"
LIMIT 8192;

-- 6) FETCH_ALL page 1 WITHOUT COLLATE (can use PK)
EXPLAIN (ANALYZE, BUFFERS)
SELECT *
FROM gaussdb.dat_fund_cjqs_bak
WHERE bcrq = '20251215'
ORDER BY xwdm, security_id, scdm, fund_code, trade_type, bs, pay_type, stock_kind,
         bcrq, etf_flag, gddm, gddmzm, check_type, mom_fund
LIMIT 8192;

-- 7) seq scan whole table COUNT (no where)
EXPLAIN (ANALYZE, BUFFERS)
SELECT COUNT(*) FROM gaussdb.dat_fund_cjqs_bak;
