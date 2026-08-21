#!/usr/bin/env python3
"""P0-3 GaussDB 逐位奇偶 SUM 形态断言（Addendum A v1.1 §3.2 前置验证）。

openGauss 5.0.0 无 bit_xor 聚合，IBLT 摘要的 XOR 字段改用逐位奇偶：
  XOR 第 i 位 = SUM((val >> i) & 1) mod 2
本脚本以 k=8 小桶在 verify_t 上验证该形态：
  校验字段 = key（id, 64 bit）+ val 第 1 切片（32 bit），共 96 个奇偶列；
SQL 输出逐 cell 的奇偶位，python 拼回 64/32-bit XOR 值并与期望值比对。
"""
import subprocess
import sys
import time

sys.path.insert(0, ".")
from gen_fixture import NULL_SENTINEL, build_rows, row_hash_mysql_norm  # noqa: E402

K = 8
KEY_BITS = 64
VAL_BITS = 32


def expected():
    cells = {}
    for r in build_rows():
        h = row_hash_mysql_norm(r)
        c = int(h[:8], 16) % K
        cnt, kx, v1 = cells.get(c, (0, 0, 0))
        cells[c] = (cnt + 1, kx ^ r[0], v1 ^ int(h[0:8], 16))
    return {c: cells.get(c, (0, 0, 0)) for c in range(K)}


def build_sql():
    key_bits = ",\n".join(
        f"MOD(SUM(((id)::bigint >> {b}) & 1), 2) AS kx_{b}" for b in range(KEY_BITS)
    )
    val_bits = ",\n".join(
        f"MOD(SUM(((('x' || SUBSTR(h, 1, 8))::bit(32)::bigint >> {b}) & 1)), 2) AS vx_{b}"
        for b in range(VAL_BITS)
    )
    return f"""
SELECT MOD(('x' || SUBSTR(h, 1, 8))::bit(32)::bigint, {K}) AS cell,
       COUNT(*) AS cnt,
       {key_bits},
       {val_bits}
FROM (
  SELECT MD5(concat_ws('#',
    COALESCE(id::text, '{NULL_SENTINEL}'), COALESCE(c_int::text, '{NULL_SENTINEL}'),
    COALESCE(c_dec::text, '{NULL_SENTINEL}'),
    COALESCE(to_char(c_dt, 'YYYY-MM-DD HH24:MI:SS.US'), '{NULL_SENTINEL}'),
    COALESCE(c_vc, '{NULL_SENTINEL}'), COALESCE(c_bool::int::text, '{NULL_SENTINEL}'),
    COALESCE(c_null::text, '{NULL_SENTINEL}'))) AS h, id
  FROM verify_t
) t
GROUP BY cell ORDER BY cell;
"""


def main():
    sql = build_sql()
    cmd = ("export LD_LIBRARY_PATH=/usr/local/opengauss/lib && "
           "/usr/local/opengauss/bin/gsql -U gaussdb -W Verify@123 "
           "-d verify -t -A -q")
    t0 = time.time()
    p = subprocess.run(["docker", "exec", "-i", "ddverify-opengauss", "bash", "-lc", cmd],
                       input=sql.encode(), capture_output=True)
    elapsed = time.time() - t0
    out, err = p.stdout.decode(), p.stderr.decode()
    if p.returncode != 0 or "ERROR" in out:
        print(f"[FAIL] query error: {(err + out).strip()[:300]}")
        sys.exit(1)

    want = expected()
    fails = 0
    for line in out.strip().split("\n"):
        if "|" not in line:
            continue
        parts = line.split("|")
        cell = int(parts[0])
        cnt = int(parts[1])
        kx_bits = [int(x) for x in parts[2:2 + KEY_BITS]]
        vx_bits = [int(x) for x in parts[2 + KEY_BITS:2 + KEY_BITS + VAL_BITS]]
        kx = sum(b << i for i, b in enumerate(kx_bits))
        vx = sum(b << i for i, b in enumerate(vx_bits))
        w = want[cell]
        if (cnt, kx, vx) != w:
            print(f"[FAIL] cell {cell}: got=({cnt},{kx},{vx}) want={w}")
            fails += 1
    if fails == 0:
        print(f"[PASS] gaussdb parity-SUM iblt form: {K}/{K} cells match "
              f"(cnt + key_xor 64b + val_xor 32b); query={elapsed:.2f}s on 2000 rows")
    else:
        print(f"[FAIL] {fails} cells mismatched")
        sys.exit(1)


if __name__ == "__main__":
    main()
