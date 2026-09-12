#!/usr/bin/env python3
"""
SDV 端：GaussianCopulaSynthesizer(norm) 拟合 + 采样 + 评测
输入：real.csv
输出：sdv_out.csv, sdv_metrics.json
"""
import json
import time
import warnings
from pathlib import Path

import numpy as np
import pandas as pd
from scipy.stats import ks_2samp

# SDV
from sdv.metadata import SingleTableMetadata
from sdv.single_table import GaussianCopulaSynthesizer
from sdmetrics.reports.single_table import QualityReport

ROOT = Path(__file__).parent
REAL_CSV = ROOT / "real.csv"
OUT_CSV = ROOT / "sdv_out.csv"
OUT_METRICS = ROOT / "sdv_metrics.json"

warnings.filterwarnings("ignore", category=FutureWarning)

def cat_tv(real: pd.Series, syn: pd.Series) -> float:
    pa = real.value_counts(normalize=True)
    pb = syn.value_counts(normalize=True)
    keys = sorted(set(pa.index) | set(pb.index))
    return 0.5 * sum(abs(pa.get(k, 0) - pb.get(k, 0)) for k in keys)

def main():
    # 1. 读真实数据
    real = pd.read_csv(REAL_CSV)
    print(f"Real rows: {len(real)}")

    # 2. 构建 metadata 并锁定 norm
    metadata = SingleTableMetadata()
    metadata.detect_from_dataframe(real)

    # 强制数值列用 norm（与 hepta 对齐）
    num_cols = real.select_dtypes(include=[np.number]).columns.tolist()
    num_dist = {c: "norm" for c in num_cols}

    # 3. 拟合
    t0 = time.perf_counter()
    syn = GaussianCopulaSynthesizer(
        metadata,
        default_distribution="norm",
        numerical_distributions=num_dist,
        enforce_min_max_values=True,
        enforce_rounding=True,
    )
    syn.fit(real)
    fit_s = time.perf_counter() - t0
    print(f"SDV fit: {fit_s:.3f}s")

    # 4. 采样
    t1 = time.perf_counter()
    sdv_out = syn.sample(num_rows=len(real), random_state=42)
    sample_s = time.perf_counter() - t1
    print(f"SDV sample: {sample_s:.3f}s")

    sdv_out.to_csv(OUT_CSV, index=False)
    print(f"Saved to {OUT_CSV}")

    # 5. 自定义指标
    real_corr = float(real["qty"].corr(real["amount"]))
    sdv_corr = float(sdv_out["qty"].corr(sdv_out["amount"]))

    metrics = {
        "n": len(real),
        "real_corr": real_corr,
        "sdv_corr": sdv_corr,
        "corr_error": abs(sdv_corr - real_corr),
        "ks_qty": float(ks_2samp(real["qty"], sdv_out["qty"]).statistic),
        "ks_amount": float(ks_2samp(real["amount"], sdv_out["amount"]).statistic),
        "tv_category": float(cat_tv(real["category"], sdv_out["category"])),
        "tv_region": float(cat_tv(real["region"], sdv_out["region"])),
        "qty_min": float(sdv_out["qty"].min()),
        "qty_max": float(sdv_out["qty"].max()),
        "qty_neg_rate": float((sdv_out["qty"] < 0).mean()),
        "amount_min": float(sdv_out["amount"].min()),
        "amount_max": float(sdv_out["amount"].max()),
        "fit_s": fit_s,
        "sample_s": sample_s,
    }

    # 6. SDMetrics QualityReport
    md = metadata.to_dict()
    report = QualityReport()
    report.generate(real, sdv_out, md, verbose=False)
    metrics["sdmetrics_quality"] = float(report.get_score())
    props = report.get_properties()
    for _, row in props.iterrows():
        metrics[f"sdmetrics_{row['Property'].replace(' ', '_').lower()}"] = float(row['Score'])

    OUT_METRICS.write_text(json.dumps(metrics, indent=2))
    print(json.dumps(metrics, indent=2))

if __name__ == "__main__":
    main()