#!/usr/bin/env python3
"""
hepta-dbcli 端：synth train | generate + 评测
输入：real.csv（已灌库）
输出：hepta_out.csv, hepta_metrics.json
"""
import json
import os
import subprocess
import time
from pathlib import Path

import pandas as pd
from scipy.stats import ks_2samp

ROOT = Path(__file__).parent
REPO_ROOT = ROOT.parent.parent
REAL_CSV = ROOT / "real.csv"
OUT_CSV = ROOT / "hepta_out.csv"
OUT_METRICS = ROOT / "hepta_metrics.json"

HEPTA_BIN = os.environ.get(
    "HEPTA_BIN", str(REPO_ROOT / "target" / "release" / "hepta_dbcli")
)
CFG = ROOT / "pagila.toml"
MODEL_DIR = ROOT / "hepta_models"
OUT_DIR = ROOT / "hepta_out"
RULES_YAML = ROOT / "synth-rules.yaml"

def cat_tv(real, syn):
    pa = real.value_counts(normalize=True)
    pb = syn.value_counts(normalize=True)
    keys = set(pa.index) | set(pb.index)
    return 0.5 * sum(abs(pa.get(k, 0) - pb.get(k, 0)) for k in keys)

def run_cmd(cmd, cwd=None):
    print(f"$ {cmd}")
    r = subprocess.run(cmd, shell=True, cwd=cwd, capture_output=True, text=True)
    if r.returncode != 0:
        print(f"STDERR: {r.stderr}")
    return r

def main():
    # 1. 读真实数据
    real = pd.read_csv(REAL_CSV)
    print(f"Real rows: {len(real)}")

    # 2. 灌库
    print("Loading data to pagila...")
    docker_cp = f"docker cp {REAL_CSV} pagila:/tmp/real.csv"
    subprocess.run(docker_cp, shell=True, check=True)
    sql = """DROP TABLE IF EXISTS gaussdb.bakeoff_t;
CREATE TABLE gaussdb.bakeoff_t (qty INT, amount NUMERIC(12,2), category VARCHAR(16), region VARCHAR(16));
COPY gaussdb.bakeoff_t FROM STDIN WITH CSV HEADER;"""
    subprocess.run(f"docker exec -i pagila gsql-pagila -c \"{sql}\" < {REAL_CSV}", shell=True, check=True)
    print("Data loaded")

    # 2. 训练
    for d in ["/tmp/hepta_models", "/tmp/hepta_out"]:
        subprocess.run(f"rm -rf {d}", shell=True)
        subprocess.run(f"mkdir -p {d}", shell=True)

    t0 = time.perf_counter()
    r = run_cmd(f"{HEPTA_BIN} --config {CFG} synth train --name pagila --tables bakeoff_t --schema gaussdb --output /tmp/hepta_models --sample 2000")
    if r.returncode != 0:
        raise RuntimeError(f"hepta train failed: {r.stderr}")
    train_s = time.perf_counter() - t0
    print(f"hepta train: {train_s:.3f}s")

    # rules yaml
    with open("/tmp/synth-rules.yaml", "w") as f:
        f.write("""version: "1"
tables:
  - name: bakeoff_t
    strategy: uniform
    relationships: []
""")

    # 3. 生成
    t1 = time.perf_counter()
    r = run_cmd(f"{HEPTA_BIN} --config {CFG} synth generate --models /tmp/hepta_models --rules /tmp/synth-rules.yaml --output /tmp/hepta_out --rows 2000 --seed 42 --format csv")
    if r.returncode != 0:
        raise RuntimeError(f"hepta generate failed: {r.stderr}")
    gen_s = time.perf_counter() - t1
    print(f"hepta generate: {gen_s:.3f}s")

    # 4. 读生成结果
    hepta = pd.read_csv("/tmp/hepta_out/bakeoff_t.csv")
    print(f"Hepta rows: {len(hepta)}")

    # 5. 真实数据（用于对比）
    real = pd.read_csv(REAL_CSV)

    # 6. 指标
    from scipy.stats import ks_2samp
    real_corr = float(real["qty"].corr(real["amount"]))
    hepta_corr = float(hepta["qty"].corr(hepta["amount"]))

    def cat_tv(r, s):
        pa = r.value_counts(normalize=True)
        pb = s.value_counts(normalize=True)
        keys = set(pa.index) | set(pb.index)
        return 0.5 * sum(abs(pa.get(k, 0) - pb.get(k, 0)) for k in keys)

    metrics = {
        "n": len(real),
        "real_corr": float(real["qty"].corr(real["amount"])),
        "hepta_corr": hepta_corr,
        "corr_error": abs(hepta_corr - real_corr),
        "ks_qty": float(ks_2samp(real["qty"], hepta["qty"]).statistic),
        "ks_amount": float(ks_2samp(real["amount"], hepta["amount"]).statistic),
        "tv_category": float(cat_tv(real["category"], hepta["category"])),
        "tv_region": float(cat_tv(real["region"], hepta["region"])),
        "qty_min": float(hepta["qty"].min()),
        "qty_max": float(hepta["qty"].max()),
        "qty_neg_rate": float((hepta["qty"] < 0).mean()),
        "amount_min": float(hepta["amount"].min()),
        "amount_max": float(hepta["amount"].max()),
        "train_s": train_s,
        "gen_s": gen_s,
    }

    # 保存
    with open("/tmp/hepta_metrics.json", "w") as f:
        json.dump(metrics, f, indent=2)
    print(json.dumps(metrics, indent=2))

if __name__ == "__main__":
    main()