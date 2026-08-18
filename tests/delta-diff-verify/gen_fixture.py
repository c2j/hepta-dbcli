#!/usr/bin/env python3
"""生成 delta-diff Phase 0 验证用 fixture：
- 三方言 DDL + 确定性 INSERT（2000 行，含 NULL/负数/小数/Unicode）
- expected.json：python 侧按 §九规范化矩阵 + §十位切片聚合计算的期望五元组
  （全表 / 4 个 id 分段 / 8 桶分桶），供 verify.py 与各库 SQL 输出比对
"""
import hashlib
import json
import os

HERE = os.path.dirname(os.path.abspath(__file__))
SQL_DIR = os.path.join(HERE, "sql")
ROWS = 2000
NULL_SENTINEL = "␀NULL␀"

MYSQL_CHECKSUM_SQL = """SELECT COUNT(*) AS cnt,
  MOD(SUM(CONV(SUBSTRING(h,  1, 8), 16, 10)), 18446744073709551616) AS s1,
  MOD(SUM(CONV(SUBSTRING(h,  9, 8), 16, 10)), 18446744073709551616) AS s2,
  MOD(SUM(CONV(SUBSTRING(h, 17, 8), 16, 10)), 18446744073709551616) AS s3,
  MOD(SUM(CONV(SUBSTRING(h, 25, 8), 16, 10)), 18446744073709551616) AS s4
FROM (
  SELECT MD5(CONCAT_WS('#',
    COALESCE(CAST(id AS CHAR), '{sentinel}'),
    COALESCE(CAST(c_int AS CHAR), '{sentinel}'),
    COALESCE(CAST(c_dec AS CHAR), '{sentinel}'),
    COALESCE(DATE_FORMAT(c_dt, '%Y-%m-%d %H:%i:%s.%f'), '{sentinel}'),
    COALESCE(c_vc, '{sentinel}'),
    COALESCE(CAST(c_bool AS CHAR), '{sentinel}'),
    COALESCE(CAST(c_null AS CHAR), '{sentinel}')
  )) AS h
  FROM verify_t
  {where}
) t;"""

GAUSS_CHECKSUM_SQL = """SELECT COUNT(*) AS cnt,
  MOD(SUM(('x' || SUBSTR(h,  1, 8))::bit(32)::bigint), 18446744073709551616) AS s1,
  MOD(SUM(('x' || SUBSTR(h,  9, 8))::bit(32)::bigint), 18446744073709551616) AS s2,
  MOD(SUM(('x' || SUBSTR(h, 17, 8))::bit(32)::bigint), 18446744073709551616) AS s3,
  MOD(SUM(('x' || SUBSTR(h, 25, 8))::bit(32)::bigint), 18446744073709551616) AS s4
FROM (
  SELECT MD5(concat_ws('#',
    COALESCE(id::text, '{sentinel}'),
    COALESCE(c_int::text, '{sentinel}'),
    COALESCE(c_dec::text, '{sentinel}'),
    COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '{sentinel}'),
    COALESCE(c_vc, '{sentinel}'),
    COALESCE(c_bool::int::text, '{sentinel}'),
    COALESCE(c_null::text, '{sentinel}')
  )) AS h
  FROM verify_t
  {where}
) t;"""


def lcg(seed):
    state = seed
    while True:
        state = (state * 6364136223846793005 + 1442695040888963407) % (1 << 64)
        yield state >> 16


def fmt_decimal(micros):
    sign = "-" if micros < 0 else ""
    v = abs(micros)
    return f"{sign}{v // 1_000_000}.{v % 1_000_000:06d}"


def fmt_dt(day_offset, sec_of_day):
    import datetime as dt

    base = dt.datetime(2023, 1, 1) + dt.timedelta(days=day_offset, seconds=sec_of_day)
    return base.strftime("%Y-%m-%d %H:%M:%S") + ".000000"


def build_rows():
    rnd = lcg(20260817)
    rows = []
    for i in range(1, ROWS + 1):
        r = next(rnd)
        c_int = None if i % 11 == 0 else (r % 200000) - 100000
        c_dec = None if i % 13 == 0 else fmt_decimal(((r >> 8) % 4_000_000_000_000) - 2_000_000_000_000)
        c_dt = None if i % 17 == 0 else fmt_dt(i % 365, (r >> 20) % 86400)
        if i == 1:
            c_vc = "unicode-汉字-🙂"
        elif i % 19 == 0:
            c_vc = None
        else:
            c_vc = f"vc-{r % 1_000_000:06d}-x"
        c_bool = None if i % 23 == 0 else (1 if (r >> 30) % 2 == 0 else 0)
        c_null = None if i % 7 == 0 else (r >> 12) % 100000
        rows.append((i, c_int, c_dec, c_dt, c_vc, c_bool, c_null))
    return rows


def row_hash_mysql_norm(row):
    id_, c_int, c_dec, c_dt, c_vc, c_bool, c_null = row
    parts = [
        str(id_),
        NULL_SENTINEL if c_int is None else str(c_int),
        NULL_SENTINEL if c_dec is None else c_dec,
        NULL_SENTINEL if c_dt is None else c_dt,
        NULL_SENTINEL if c_vc is None else c_vc,
        NULL_SENTINEL if c_bool is None else str(c_bool),
        NULL_SENTINEL if c_null is None else str(c_null),
    ]
    return hashlib.md5("#".join(parts).encode("utf-8")).hexdigest()


def checksum(hashes):
    s = [0, 0, 0, 0]
    for h in hashes:
        for k in range(4):
            s[k] = (s[k] + int(h[k * 8:(k + 1) * 8], 16)) % (1 << 64)
    return [len(hashes), *s]


def mysql_bucket_expr(n, b):
    return f"MOD(CONV(SUBSTRING(MD5(CONCAT_WS('#', COALESCE(CAST(id AS CHAR), '{NULL_SENTINEL}'), COALESCE(CAST(c_int AS CHAR), '{NULL_SENTINEL}'), COALESCE(CAST(c_dec AS CHAR), '{NULL_SENTINEL}'), COALESCE(DATE_FORMAT(c_dt, '%Y-%m-%d %H:%i:%s.%f'), '{NULL_SENTINEL}'), COALESCE(c_vc, '{NULL_SENTINEL}'), COALESCE(CAST(c_bool AS CHAR), '{NULL_SENTINEL}'), COALESCE(CAST(c_null AS CHAR), '{NULL_SENTINEL}'))), 1, 8), 16, 10), {n}) = {b}"


def gauss_bucket_expr(n, b):
    return f"MOD(('x' || SUBSTR(MD5(concat_ws('#', COALESCE(id::text, '{NULL_SENTINEL}'), COALESCE(c_int::text, '{NULL_SENTINEL}'), COALESCE(c_dec::text, '{NULL_SENTINEL}'), COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '{NULL_SENTINEL}'), COALESCE(c_vc, '{NULL_SENTINEL}'), COALESCE(c_bool::int::text, '{NULL_SENTINEL}'), COALESCE(c_null::text, '{NULL_SENTINEL}'))), 1, 8))::bit(32)::bigint, {n}) = {b}"


def sql_lit(v):
    if v is None:
        return "NULL"
    return "'" + str(v).replace("'", "''") + "'"


def main():
    os.makedirs(SQL_DIR, exist_ok=True)
    rows = build_rows()
    hashes = {r[0]: row_hash_mysql_norm(r) for r in rows}

    expected = {"full": checksum(list(hashes.values()))}
    ranges = [(1, 501), (501, 1001), (1001, 1501), (1501, 2001)]
    expected["ranges"] = {
        f"{lo}-{hi}": checksum([h for i, h in hashes.items() if lo <= i < hi])
        for lo, hi in ranges
    }
    expected["buckets"] = {}
    for n, b in [(8, 0), (8, 3), (8, 7)]:
        sel = [h for h in hashes.values() if int(h[:8], 16) % n == b]
        expected["buckets"][f"{n}-{b}"] = checksum(sel)

    with open(os.path.join(HERE, "expected.json"), "w", encoding="utf-8") as f:
        json.dump(expected, f, ensure_ascii=False, indent=1)

    mysql = ["DROP TABLE IF EXISTS verify_t;", """
CREATE TABLE verify_t (
  id INT PRIMARY KEY,
  c_int INT NULL,
  c_dec DECIMAL(20,6) NULL,
  c_dt DATETIME NULL,
  c_vc VARCHAR(64) NULL,
  c_bool TINYINT(1) NULL,
  c_null INT NULL
);""", "SET NAMES utf8mb4;"]
    gauss = ["DROP TABLE IF EXISTS verify_t;", """
CREATE TABLE verify_t (
  id INT PRIMARY KEY,
  c_int INT NULL,
  c_dec NUMERIC(20,6) NULL,
  c_dt TIMESTAMP NULL,
  c_vc VARCHAR(64) NULL,
  c_bool BOOLEAN NULL,
  c_null INT NULL
);"""]

    batch = []
    for r in rows:
        id_, c_int, c_dec, c_dt, c_vc, c_bool, c_null = r
        dt_sql = sql_lit(c_dt.removesuffix(".000000") if c_dt else None)
        batch.append(
            f"({id_}, {c_int if c_int is not None else 'NULL'}, "
            f"{c_dec if c_dec is not None else 'NULL'}, {dt_sql}, {sql_lit(c_vc)}, "
            f"{c_bool if c_bool is not None else 'NULL'}, {c_null if c_null is not None else 'NULL'})"
        )
        if len(batch) == 200:
            vals = ",\n".join(batch)
            mysql.append(f"INSERT INTO verify_t VALUES\n{vals};")
            gauss.append(f"INSERT INTO verify_t VALUES\n{vals};")
            batch = []

    with open(os.path.join(SQL_DIR, "fixture_mysql.sql"), "w", encoding="utf-8") as f:
        f.write("\n".join(mysql) + "\n")
    with open(os.path.join(SQL_DIR, "fixture_gauss.sql"), "w", encoding="utf-8") as f:
        f.write("\n".join(gauss) + "\n")

    queries = {"full": ""}
    for lo, hi in ranges:
        queries[f"range-{lo}-{hi}"] = f"WHERE id >= {lo} AND id < {hi}"
    for n, b in [(8, 0), (8, 3), (8, 7)]:
        queries[f"bucket-{n}-{b}"] = None

    with open(os.path.join(SQL_DIR, "checksum_mysql.sql"), "w", encoding="utf-8") as f:
        for name, where in queries.items():
            if name.startswith("bucket-"):
                n, b = name.split("-")[1:3]
                where = f"WHERE {mysql_bucket_expr(int(n), int(b))}"
            f.write(f"SELECT '{name}' AS case_name;\n")
            f.write(MYSQL_CHECKSUM_SQL.format(sentinel=NULL_SENTINEL, where=where) + "\n")

    with open(os.path.join(SQL_DIR, "checksum_gauss.sql"), "w", encoding="utf-8") as f:
        for name, where in queries.items():
            if name.startswith("bucket-"):
                n, b = name.split("-")[1:3]
                where = f"WHERE {gauss_bucket_expr(int(n), int(b))}"
            f.write(f"SELECT '{name}' AS case_name;\n")
            f.write(GAUSS_CHECKSUM_SQL.format(sentinel=NULL_SENTINEL, where=where) + "\n")

    print(f"rows={ROWS} expected full={expected['full']}")
    print("generated: sql/fixture_mysql.sql sql/fixture_gauss.sql "
          "sql/checksum_mysql.sql sql/checksum_gauss.sql expected.json")


if __name__ == "__main__":
    main()
