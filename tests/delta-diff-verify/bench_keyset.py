#!/usr/bin/env python3
"""P0-2 keyset 分页吞吐实测（设计文档 §6.2.2 方案验证）。

对 100 万行 perf_t（verify.py V8 已建，id 连续 0..999999）按页 8192 做 keyset
拉取，全部页查询在**单一会话**内顺序执行（消除逐页进程开销，测服务端分页吞吐）：
  SELECT id, c1, c2 FROM perf_t WHERE id > :last ORDER BY id LIMIT 8192
"""
import subprocess
import time

PAGE = 8192
IDS = 1_000_000


def page_statements():
    return "\n".join(
        f"SELECT id, c1, c2 FROM perf_t WHERE id > {last} ORDER BY id LIMIT {PAGE};"
        for last in range(-1, IDS, PAGE)
    )


def bench(name, cmd, count_parser):
    t0 = time.time()
    p = subprocess.run(cmd, input=page_statements().encode(), capture_output=True)
    dt = time.time() - t0
    out, err = p.stdout.decode(), p.stderr.decode()
    if p.returncode != 0:
        print(f"[{name}] FAIL: {err.strip()[:200]}")
        return
    rows = count_parser(out)
    print(f"[{name}] rows={rows} elapsed={dt:.1f}s keyset_rps={rows / dt / 1e6:.2f}M rows/s "
          f"(单会话, {rows // PAGE + 1} 页)")


def main():
    bench("mysql",
          ["docker", "exec", "-i", "ddverify-mysql", "mysql",
           "-uroot", "-pverify123", "verify", "--batch", "--raw", "-N"],
          lambda out: len([l for l in out.split("\n") if l.strip()]))
    bench("opengauss",
          ["docker", "exec", "-i", "ddverify-opengauss", "bash", "-lc",
           "export LD_LIBRARY_PATH=/usr/local/opengauss/lib && "
           "/usr/local/opengauss/bin/gsql -U gaussdb -W Verify@123 -d verify -t -A -q"],
          lambda out: len([l for l in out.split("\n") if "|" in l]))


if __name__ == "__main__":
    main()
