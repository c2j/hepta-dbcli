-- 10 modified rows on the right side, same day under test.
UPDATE gaussdb.dat_fund_cjqs_bak_r
SET cjje = COALESCE(cjje, 0) + 1
WHERE ctid IN (
    SELECT ctid FROM gaussdb.dat_fund_cjqs_bak_r
    WHERE bcrq = '20251215'
    LIMIT 10
);
ANALYZE gaussdb.dat_fund_cjqs_bak_r;
SELECT COUNT(*) AS touched FROM gaussdb.dat_fund_cjqs_bak_r r
JOIN gaussdb.dat_fund_cjqs_bak l USING (
    xwdm, security_id, scdm, fund_code, trade_type, bs, pay_type,
    stock_kind, bcrq, etf_flag, gddm, gddmzm, check_type, mom_fund
)
WHERE r.bcrq = '20251215' AND r.cjje IS DISTINCT FROM l.cjje;
