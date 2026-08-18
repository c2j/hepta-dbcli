#!/usr/bin/env python3
"""delta-diff Phase 0 技术可行性验证脚本。

对三个容器实例（ddverify-mysql / ddverify-opengauss / ddverify-polardbx）执行：
  V1-V3  位切片 checksum SQL 跨库一致性（与 expected.json 的 python 期望值比对）
  V4     MySQL SUM() 溢出语义实证（v2.0 CAST 饱和 vs v2.1 MOD 正确）
  V5     openGauss hash_any_extended / hashtextextended 可用性探测
  V6     快照事务语法（三库）
  V7     CRC32 链表达式可行性（mysql / polardbx）
  V8     1M 行 checksum 吞吐实测（R_scan 数据点）

输出：终端 PASS/FAIL 汇总 + results.txt（原始记录，供设计文档引用）。
前置：gen_fixture.py 已执行；三个容器已启动（tests/delta-diff-verify/docker-compose.yml）。
"""
import json
import os
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from gen_fixture import (  # noqa: E402
    GAUSS_CHECKSUM_SQL,
    MYSQL_CHECKSUM_SQL,
    NULL_SENTINEL,
    gauss_bucket_expr,
    mysql_bucket_expr,
)

RESULTS = []


def run(cmd, stdin_file=None, timeout=300):
    stdin = open(stdin_file, "rb") if stdin_file else subprocess.DEVNULL
    try:
        p = subprocess.run(cmd, stdin=stdin, capture_output=True, timeout=timeout)
        return p.returncode, p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")
    finally:
        if stdin_file:
            stdin.close()


def mysql(sql, db="verify"):
    return run(["docker", "exec", "-i", "ddverify-mysql", "mysql",
                "-uroot", "-pverify123", "--batch", "--raw", "-N",
                "--default-character-set=utf8mb4", db, "-e", sql])


GSQL = ("export LD_LIBRARY_PATH=/usr/local/opengauss/lib && "
        "/usr/local/opengauss/bin/gsql -U gaussdb -W Verify@123 -d {db} -t -A -c {sql}")


def gauss(sql, db="verify"):
    import shlex

    cmd = GSQL.format(db=db, sql=shlex.quote(sql))
    return run(["docker", "exec", "-i", "ddverify-opengauss", "bash", "-lc", cmd])


def polardbx(sql, db="verify"):
    return run(["docker", "exec", "-i", "ddverify-polardbx", "mysql",
                "-h127.0.0.1", "-P8527", "-upolardbx_root",
                "--batch", "--raw", "-N", "--default-character-set=utf8mb4", db, "-e", sql])


def load_mysql(path):
    return run(["docker", "exec", "-i", "ddverify-mysql", "mysql",
                "-uroot", "-pverify123", "--default-character-set=utf8mb4", "verify"],
               stdin_file=path)


def load_gauss(path):
    return run(["docker", "exec", "-i", "ddverify-opengauss", "bash", "-lc",
                "export LD_LIBRARY_PATH=/usr/local/opengauss/lib && "
                "/usr/local/opengauss/bin/gsql -U gaussdb -W Verify@123 -d verify -q"],
               stdin_file=path)


def load_polardbx(path):
    return run(["docker", "exec", "-i", "ddverify-polardbx", "mysql",
                "-h127.0.0.1", "-P8527", "-upolardbx_root",
                "--default-character-set=utf8mb4", "verify"],
               stdin_file=path)


def record(item, engine, status, detail):
    line = f"[{status}] {item} | {engine} | {detail}"
    RESULTS.append(line)
    print(line)


def parse_tuple(out):
    vals = out.strip().split("\n")[-1].replace("|", "\t").split("\t")
    vals = [v.strip() for v in vals if v.strip() != ""]
    return [int(float(v)) for v in vals[:5]]


def wait_health(name, timeout=600):
    deadline = time.time() + timeout
    while time.time() < deadline:
        rc, out, _ = run(["docker", "inspect", "-f", "{{.State.Health.Status}}", name])
        if rc == 0 and out.strip() == "healthy":
            return True
        time.sleep(5)
    return False


def cases():
    yield "full", "", "full"
    for lo, hi in [(1, 501), (501, 1001), (1001, 1501), (1501, 2001)]:
        yield f"range-{lo}-{hi}", f"WHERE id >= {lo} AND id < {hi}", f"{lo}-{hi}"
    for n, b in [(8, 0), (8, 3), (8, 7)]:
        yield f"bucket-{n}-{b}", None, f"{n}-{b}"


def expected_map(exp):
    m = {"full": exp["full"]}
    m.update(exp["ranges"])
    m.update(exp["buckets"])
    return m


def main():
    with open(os.path.join(HERE, "expected.json"), encoding="utf-8") as f:
        exp = expected_map(json.load(f))

    print("== waiting for containers ==")
    for name in ["ddverify-mysql", "ddverify-opengauss", "ddverify-polardbx"]:
        ok = wait_health(name, timeout=900)
        record("health", name, "PASS" if ok else "FAIL", "healthy" if ok else "timeout")
        if not ok and name != "ddverify-polardbx":
            summarize()
            sys.exit(1)

    print("== loading fixtures ==")
    rc, out, err = load_mysql(os.path.join(HERE, "sql", "fixture_mysql.sql"))
    record("fixture-load", "mysql", "PASS" if rc == 0 else "FAIL", err.strip()[:200] or "loaded")
    rc, out, err = load_gauss(os.path.join(HERE, "sql", "fixture_gauss.sql"))
    record("fixture-load", "opengauss", "PASS" if rc == 0 else "FAIL", err.strip()[:200] or "loaded")

    rc, _, err = polardbx("CREATE DATABASE IF NOT EXISTS verify", db="")
    record("create-db", "polardbx", "PASS" if rc == 0 else "FAIL", err.strip()[:200] or "verify")
    rc, out, err = load_polardbx(os.path.join(HERE, "sql", "fixture_mysql.sql"))
    record("fixture-load", "polardbx", "PASS" if rc == 0 else "FAIL", err.strip()[:200] or "loaded")

    print("== V1-V3 checksum cross-db ==")
    engines = [
        ("mysql", mysql, MYSQL_CHECKSUM_SQL, mysql_bucket_expr),
        ("opengauss", gauss, GAUSS_CHECKSUM_SQL, gauss_bucket_expr),
        ("polardbx", polardbx, MYSQL_CHECKSUM_SQL, mysql_bucket_expr),
    ]
    for name, fn, tpl, bucket_fn in engines:
        for case, where, exp_key in cases():
            if case.startswith("bucket-"):
                n, b = case.split("-")[1:3]
                where = f"WHERE {bucket_fn(int(n), int(b))}"
            sql = tpl.format(sentinel=NULL_SENTINEL, where=where)
            rc, out, err = fn(sql)
            if rc != 0:
                record(f"V-checksum {case}", name, "FAIL", err.strip()[:300])
                continue
            try:
                got = parse_tuple(out)
            except Exception:
                record(f"V-checksum {case}", name, "FAIL", f"unparsable: {out.strip()[:200]}")
                continue
            want = exp[exp_key]
            status = "PASS" if got == want else "FAIL"
            record(f"V-checksum {case}", name, status, f"got={got} want={want}")

    print("== V4 SUM overflow semantics ==")
    v4 = ("SELECT SUM(x) AS sum_dec, "
          "CAST(SUM(x) AS UNSIGNED) % 18446744073709551616 AS old_way, "
          "MOD(SUM(x), 18446744073709551616) AS new_way "
          "FROM (SELECT 18446744073709551615 AS x UNION ALL "
          "SELECT 18446744073709551615 UNION ALL SELECT 4294967295) t")
    for name, fn in [("mysql", mysql), ("polardbx", polardbx)]:
        rc, out, err = fn(v4)
        if rc != 0:
            record("V4-overflow", name, "FAIL", err.strip()[:300])
            continue
        record("V4-overflow", name, "INFO", out.strip())
    rc, out, err = gauss(
        "SELECT SUM(x), MOD(SUM(x), 18446744073709551616) FROM "
        "(SELECT 18446744073709551615::numeric AS x UNION ALL "
        "SELECT 18446744073709551615::numeric UNION ALL SELECT 4294967295::numeric) t")
    record("V4-overflow", "opengauss", "INFO" if rc == 0 else "FAIL",
           out.strip() if rc == 0 else err.strip()[:300])

    print("== V5 hash function probing on opengauss ==")
    for func in ["hash_any_extended('abc', 0)", "hashtextextended('abc', 0)",
                 "hashint8extended(42, 0)", "md5('abc')"]:
        rc, out, err = gauss(f"SELECT {func}")
        combined = (out + err).strip()
        missing = "does not exist" in combined
        status = "MISSING" if missing else ("INFO" if out.strip() else "FAIL")
        record("V5-hashfunc", "opengauss", status,
               f"{func} => {combined[:160]}")

    print("== V6 snapshot syntax ==")
    rc, out, err = mysql("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY; "
                         "SELECT COUNT(*) FROM verify_t; COMMIT;")
    record("V6-snapshot", "mysql", "PASS" if rc == 0 else "FAIL",
           out.strip() if rc == 0 else err.strip()[:300])
    rc, out, err = gauss("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY; "
                         "SELECT COUNT(*) FROM verify_t; COMMIT;")
    record("V6-snapshot", "opengauss", "PASS" if rc == 0 else "FAIL",
           out.strip() if rc == 0 else err.strip()[:300])
    rc, out, err = polardbx("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY; "
                            "SELECT COUNT(*) FROM verify_t; COMMIT;")
    record("V6-snapshot-with-consistent", "polardbx", "PASS" if rc == 0 else "FAIL",
           out.strip() if rc == 0 else err.strip()[:300])
    rc, out, err = polardbx("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; "
                            "START TRANSACTION READ ONLY; "
                            "SELECT COUNT(*) FROM verify_t; COMMIT;")
    record("V6-snapshot-rr-ro", "polardbx", "PASS" if rc == 0 else "FAIL",
           out.strip() if rc == 0 else err.strip()[:300])

    print("== V7 CRC32 chain ==")
    for name, fn in [("mysql", mysql), ("polardbx", polardbx)]:
        rc, out, err = fn("SELECT CRC32('delta'), CRC32(CONCAT(CRC32('a'), CRC32('b'))), "
                          "MOD(CONV(SUBSTRING(MD5('x'),1,8),16,10), 8)")
        record("V7-crc32", name, "PASS" if rc == 0 else "FAIL",
               out.strip() if rc == 0 else err.strip()[:300])

    print("== V8 1M-row throughput ==")
    v8_mysql_ddl = ("DROP TABLE IF EXISTS perf_t; "
                    "CREATE TABLE perf_t (id INT PRIMARY KEY, c1 VARCHAR(32), c2 BIGINT);")
    v8_mysql_fill = ("INSERT INTO perf_t "
                     "SELECT n, CONCAT('v', n), n * 7 FROM ("
                     "SELECT a.x + b.x * 10 + c.x * 100 + d.x * 1000 + e.x * 10000 + f.x * 100000 AS n "
                     "FROM (SELECT 0 x UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 "
                     "UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) a,"
                     "(SELECT 0 x UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 "
                     "UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) b,"
                     "(SELECT 0 x UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 "
                     "UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) c,"
                     "(SELECT 0 x UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 "
                     "UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) d,"
                     "(SELECT 0 x UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 "
                     "UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) e,"
                     "(SELECT 0 x UNION SELECT 1 UNION SELECT 2 UNION SELECT 3 UNION SELECT 4 "
                     "UNION SELECT 5 UNION SELECT 6 UNION SELECT 7 UNION SELECT 8 UNION SELECT 9) f"
                     ") t")
    v8_mysql_q = ("SELECT COUNT(*), "
                  "MOD(SUM(CONV(SUBSTRING(h,1,8),16,10)), 18446744073709551616), "
                  "MOD(SUM(CONV(SUBSTRING(h,9,8),16,10)), 18446744073709551616), "
                  "MOD(SUM(CONV(SUBSTRING(h,17,8),16,10)), 18446744073709551616), "
                  "MOD(SUM(CONV(SUBSTRING(h,25,8),16,10)), 18446744073709551616) FROM ("
                  "SELECT MD5(CONCAT_WS('#', CAST(id AS CHAR), c1, CAST(c2 AS CHAR))) AS h "
                  "FROM perf_t) t")
    v8_gauss_ddl = ("DROP TABLE IF EXISTS perf_t; "
                    "CREATE TABLE perf_t (id INT PRIMARY KEY, c1 VARCHAR(32), c2 BIGINT);")
    v8_gauss_fill = ("INSERT INTO perf_t SELECT i, 'v' || i, i * 7 "
                     "FROM generate_series(0, 999999) i")
    v8_gauss_q = ("SELECT COUNT(*), "
                  "MOD(SUM(('x'||SUBSTR(h,1,8))::bit(32)::bigint), 18446744073709551616), "
                  "MOD(SUM(('x'||SUBSTR(h,9,8))::bit(32)::bigint), 18446744073709551616), "
                  "MOD(SUM(('x'||SUBSTR(h,17,8))::bit(32)::bigint), 18446744073709551616), "
                  "MOD(SUM(('x'||SUBSTR(h,25,8))::bit(32)::bigint), 18446744073709551616) FROM ("
                  "SELECT MD5(concat_ws('#', id::text, c1, c2::text)) AS h FROM perf_t) t")

    for name, fn, ddl, fill, q in [
        ("mysql", mysql, v8_mysql_ddl, v8_mysql_fill, v8_mysql_q),
        ("opengauss", gauss, v8_gauss_ddl, v8_gauss_fill, v8_gauss_q),
    ]:
        rc, _, err = fn(ddl)
        if rc != 0:
            record("V8-throughput", name, "FAIL", f"ddl: {err.strip()[:200]}")
            continue
        t0 = time.time()
        rc, _, err = fn(fill)
        fill_s = time.time() - t0
        if rc != 0:
            record("V8-throughput", name, "FAIL", f"fill: {err.strip()[:200]}")
            continue
        fn(q)
        t0 = time.time()
        rc, out, err = fn(q)
        run_s = time.time() - t0
        if rc != 0:
            record("V8-throughput", name, "FAIL", f"query: {err.strip()[:200]}")
            continue
        rows = parse_tuple(out)[0]
        rps = rows / run_s if run_s > 0 else 0
        record("V8-throughput", name, "INFO",
               f"rows={rows} fill={fill_s:.1f}s checksum={run_s:.2f}s "
               f"r_scan={rps/1e6:.2f}M rows/s (含 docker exec 开销, 容器内单会话)")

    with open(os.path.join(HERE, "results.txt"), "w", encoding="utf-8") as f:
        f.write("\n".join(RESULTS) + "\n")
    summarize()


def summarize():
    fails = [l for l in RESULTS if l.startswith("[FAIL]")]
    print(f"\n==== summary: {len(RESULTS)} records, {len(fails)} FAIL ====")
    for l in fails:
        print(l)


if __name__ == "__main__":
    main()
