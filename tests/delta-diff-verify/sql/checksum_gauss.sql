SELECT 'full' AS case_name;
SELECT COUNT(*) AS cnt,
  MOD(SUM(('x' || SUBSTR(h,  1, 8))::bit(32)::bigint), 18446744073709551616) AS s1,
  MOD(SUM(('x' || SUBSTR(h,  9, 8))::bit(32)::bigint), 18446744073709551616) AS s2,
  MOD(SUM(('x' || SUBSTR(h, 17, 8))::bit(32)::bigint), 18446744073709551616) AS s3,
  MOD(SUM(('x' || SUBSTR(h, 25, 8))::bit(32)::bigint), 18446744073709551616) AS s4
FROM (
  SELECT MD5(concat_ws('#',
    COALESCE(id::text, '␀NULL␀'),
    COALESCE(c_int::text, '␀NULL␀'),
    COALESCE(c_dec::text, '␀NULL␀'),
    COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '␀NULL␀'),
    COALESCE(c_vc, '␀NULL␀'),
    COALESCE(c_bool::int::text, '␀NULL␀'),
    COALESCE(c_null::text, '␀NULL␀')
  )) AS h
  FROM verify_t
  
) t;
SELECT 'range-1-501' AS case_name;
SELECT COUNT(*) AS cnt,
  MOD(SUM(('x' || SUBSTR(h,  1, 8))::bit(32)::bigint), 18446744073709551616) AS s1,
  MOD(SUM(('x' || SUBSTR(h,  9, 8))::bit(32)::bigint), 18446744073709551616) AS s2,
  MOD(SUM(('x' || SUBSTR(h, 17, 8))::bit(32)::bigint), 18446744073709551616) AS s3,
  MOD(SUM(('x' || SUBSTR(h, 25, 8))::bit(32)::bigint), 18446744073709551616) AS s4
FROM (
  SELECT MD5(concat_ws('#',
    COALESCE(id::text, '␀NULL␀'),
    COALESCE(c_int::text, '␀NULL␀'),
    COALESCE(c_dec::text, '␀NULL␀'),
    COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '␀NULL␀'),
    COALESCE(c_vc, '␀NULL␀'),
    COALESCE(c_bool::int::text, '␀NULL␀'),
    COALESCE(c_null::text, '␀NULL␀')
  )) AS h
  FROM verify_t
  WHERE id >= 1 AND id < 501
) t;
SELECT 'range-501-1001' AS case_name;
SELECT COUNT(*) AS cnt,
  MOD(SUM(('x' || SUBSTR(h,  1, 8))::bit(32)::bigint), 18446744073709551616) AS s1,
  MOD(SUM(('x' || SUBSTR(h,  9, 8))::bit(32)::bigint), 18446744073709551616) AS s2,
  MOD(SUM(('x' || SUBSTR(h, 17, 8))::bit(32)::bigint), 18446744073709551616) AS s3,
  MOD(SUM(('x' || SUBSTR(h, 25, 8))::bit(32)::bigint), 18446744073709551616) AS s4
FROM (
  SELECT MD5(concat_ws('#',
    COALESCE(id::text, '␀NULL␀'),
    COALESCE(c_int::text, '␀NULL␀'),
    COALESCE(c_dec::text, '␀NULL␀'),
    COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '␀NULL␀'),
    COALESCE(c_vc, '␀NULL␀'),
    COALESCE(c_bool::int::text, '␀NULL␀'),
    COALESCE(c_null::text, '␀NULL␀')
  )) AS h
  FROM verify_t
  WHERE id >= 501 AND id < 1001
) t;
SELECT 'range-1001-1501' AS case_name;
SELECT COUNT(*) AS cnt,
  MOD(SUM(('x' || SUBSTR(h,  1, 8))::bit(32)::bigint), 18446744073709551616) AS s1,
  MOD(SUM(('x' || SUBSTR(h,  9, 8))::bit(32)::bigint), 18446744073709551616) AS s2,
  MOD(SUM(('x' || SUBSTR(h, 17, 8))::bit(32)::bigint), 18446744073709551616) AS s3,
  MOD(SUM(('x' || SUBSTR(h, 25, 8))::bit(32)::bigint), 18446744073709551616) AS s4
FROM (
  SELECT MD5(concat_ws('#',
    COALESCE(id::text, '␀NULL␀'),
    COALESCE(c_int::text, '␀NULL␀'),
    COALESCE(c_dec::text, '␀NULL␀'),
    COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '␀NULL␀'),
    COALESCE(c_vc, '␀NULL␀'),
    COALESCE(c_bool::int::text, '␀NULL␀'),
    COALESCE(c_null::text, '␀NULL␀')
  )) AS h
  FROM verify_t
  WHERE id >= 1001 AND id < 1501
) t;
SELECT 'range-1501-2001' AS case_name;
SELECT COUNT(*) AS cnt,
  MOD(SUM(('x' || SUBSTR(h,  1, 8))::bit(32)::bigint), 18446744073709551616) AS s1,
  MOD(SUM(('x' || SUBSTR(h,  9, 8))::bit(32)::bigint), 18446744073709551616) AS s2,
  MOD(SUM(('x' || SUBSTR(h, 17, 8))::bit(32)::bigint), 18446744073709551616) AS s3,
  MOD(SUM(('x' || SUBSTR(h, 25, 8))::bit(32)::bigint), 18446744073709551616) AS s4
FROM (
  SELECT MD5(concat_ws('#',
    COALESCE(id::text, '␀NULL␀'),
    COALESCE(c_int::text, '␀NULL␀'),
    COALESCE(c_dec::text, '␀NULL␀'),
    COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '␀NULL␀'),
    COALESCE(c_vc, '␀NULL␀'),
    COALESCE(c_bool::int::text, '␀NULL␀'),
    COALESCE(c_null::text, '␀NULL␀')
  )) AS h
  FROM verify_t
  WHERE id >= 1501 AND id < 2001
) t;
SELECT 'bucket-8-0' AS case_name;
SELECT COUNT(*) AS cnt,
  MOD(SUM(('x' || SUBSTR(h,  1, 8))::bit(32)::bigint), 18446744073709551616) AS s1,
  MOD(SUM(('x' || SUBSTR(h,  9, 8))::bit(32)::bigint), 18446744073709551616) AS s2,
  MOD(SUM(('x' || SUBSTR(h, 17, 8))::bit(32)::bigint), 18446744073709551616) AS s3,
  MOD(SUM(('x' || SUBSTR(h, 25, 8))::bit(32)::bigint), 18446744073709551616) AS s4
FROM (
  SELECT MD5(concat_ws('#',
    COALESCE(id::text, '␀NULL␀'),
    COALESCE(c_int::text, '␀NULL␀'),
    COALESCE(c_dec::text, '␀NULL␀'),
    COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '␀NULL␀'),
    COALESCE(c_vc, '␀NULL␀'),
    COALESCE(c_bool::int::text, '␀NULL␀'),
    COALESCE(c_null::text, '␀NULL␀')
  )) AS h
  FROM verify_t
  WHERE MOD(('x' || SUBSTR(MD5(concat_ws('#', COALESCE(id::text, '␀NULL␀'), COALESCE(c_int::text, '␀NULL␀'), COALESCE(c_dec::text, '␀NULL␀'), COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '␀NULL␀'), COALESCE(c_vc, '␀NULL␀'), COALESCE(c_bool::int::text, '␀NULL␀'), COALESCE(c_null::text, '␀NULL␀'))), 1, 8))::bit(32)::bigint, 8) = 0
) t;
SELECT 'bucket-8-3' AS case_name;
SELECT COUNT(*) AS cnt,
  MOD(SUM(('x' || SUBSTR(h,  1, 8))::bit(32)::bigint), 18446744073709551616) AS s1,
  MOD(SUM(('x' || SUBSTR(h,  9, 8))::bit(32)::bigint), 18446744073709551616) AS s2,
  MOD(SUM(('x' || SUBSTR(h, 17, 8))::bit(32)::bigint), 18446744073709551616) AS s3,
  MOD(SUM(('x' || SUBSTR(h, 25, 8))::bit(32)::bigint), 18446744073709551616) AS s4
FROM (
  SELECT MD5(concat_ws('#',
    COALESCE(id::text, '␀NULL␀'),
    COALESCE(c_int::text, '␀NULL␀'),
    COALESCE(c_dec::text, '␀NULL␀'),
    COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '␀NULL␀'),
    COALESCE(c_vc, '␀NULL␀'),
    COALESCE(c_bool::int::text, '␀NULL␀'),
    COALESCE(c_null::text, '␀NULL␀')
  )) AS h
  FROM verify_t
  WHERE MOD(('x' || SUBSTR(MD5(concat_ws('#', COALESCE(id::text, '␀NULL␀'), COALESCE(c_int::text, '␀NULL␀'), COALESCE(c_dec::text, '␀NULL␀'), COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '␀NULL␀'), COALESCE(c_vc, '␀NULL␀'), COALESCE(c_bool::int::text, '␀NULL␀'), COALESCE(c_null::text, '␀NULL␀'))), 1, 8))::bit(32)::bigint, 8) = 3
) t;
SELECT 'bucket-8-7' AS case_name;
SELECT COUNT(*) AS cnt,
  MOD(SUM(('x' || SUBSTR(h,  1, 8))::bit(32)::bigint), 18446744073709551616) AS s1,
  MOD(SUM(('x' || SUBSTR(h,  9, 8))::bit(32)::bigint), 18446744073709551616) AS s2,
  MOD(SUM(('x' || SUBSTR(h, 17, 8))::bit(32)::bigint), 18446744073709551616) AS s3,
  MOD(SUM(('x' || SUBSTR(h, 25, 8))::bit(32)::bigint), 18446744073709551616) AS s4
FROM (
  SELECT MD5(concat_ws('#',
    COALESCE(id::text, '␀NULL␀'),
    COALESCE(c_int::text, '␀NULL␀'),
    COALESCE(c_dec::text, '␀NULL␀'),
    COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '␀NULL␀'),
    COALESCE(c_vc, '␀NULL␀'),
    COALESCE(c_bool::int::text, '␀NULL␀'),
    COALESCE(c_null::text, '␀NULL␀')
  )) AS h
  FROM verify_t
  WHERE MOD(('x' || SUBSTR(MD5(concat_ws('#', COALESCE(id::text, '␀NULL␀'), COALESCE(c_int::text, '␀NULL␀'), COALESCE(c_dec::text, '␀NULL␀'), COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '␀NULL␀'), COALESCE(c_vc, '␀NULL␀'), COALESCE(c_bool::int::text, '␀NULL␀'), COALESCE(c_null::text, '␀NULL␀'))), 1, 8))::bit(32)::bigint, 8) = 7
) t;
