#!/usr/bin/env python3
"""
聚合双边指标，输出 JSON + Markdown 报告
"""
import json
from pathlib import Path

ROOT = Path(__file__).parent
SDV_M = ROOT / "sdv_metrics.json"
HEPTA_M = ROOT / "hepta_metrics.json"
OUT_JSON = ROOT / "case_a_report.json"
OUT_MD = ROOT / "case_a_report.md"

def load(p):
    if p.exists():
        return json.loads(p.read_text())
    return {}

def main():
    sdv = load(Path("/tmp/sdv_metrics.json"))
    hepta = load(Path("/tmp/hepta_metrics.json"))

    # 兼容直接放在 benchmark 目录
    if not sdv:
        sdv = load(ROOT / "sdv_metrics.json")
    if not hepta:
        hepta = load(ROOT / "hepta_metrics.json")

    report = {
        "sdv": sdv,
        "hepta": hepta,
        "comparison": {},
    }

    if sdv and hepta:
        report["comparison"] = {
            "corr_error_delta": hepta.get("corr_error", 999) - sdv.get("corr_error", 999),
            "ks_qty_delta": hepta.get("ks_qty", 999) - sdv.get("ks_qty", 999),
            "ks_amount_delta": hepta.get("ks_amount", 999) - sdv.get("ks_amount", 999),
            "tv_category_delta": hepta.get("tv_category", 999) - sdv.get("tv_category", 999),
            "tv_region_delta": hepta.get("tv_region", 999) - sdv.get("tv_region", 999),
            "qty_neg_rate_hepta": hepta.get("qty_neg_rate", -1),
            "qty_neg_rate_sdv": sdv.get("qty_neg_rate", -1),
            "qty_range_hepta": [hepta.get("qty_min"), hepta.get("qty_max")],
            "qty_range_sdv": [sdv.get("qty_min"), sdv.get("qty_max")],
            "quality_delta": hepta.get("sdmetrics_quality", 0) - sdv.get("sdmetrics_quality", 0),
        }

    # 写 JSON
    Path("/tmp/case_a_report.json").write_text(json.dumps(report, indent=2, ensure_ascii=False))
    print("Report JSON saved to /tmp/case_a_report.json")

    # 生成 Markdown
    md = []
    md.append("# Case A Benchmark Report\n")
    md.append(f"Generated: {__import__('datetime').datetime.now().isoformat()}\n")

    md.append("## Summary\n")
    if sdv and hepta:
        md.append(f"- **hepta corr error**: {hepta.get('corr_error', 'N/A'):.4f} vs SDV {sdv.get('corr_error', 'N/A'):.4f}")
        md.append(f"- **QualityScore**: hepta {hepta.get('sdmetrics_quality', 'N/A'):.4f} vs SDV {sdv.get('sdmetrics_quality', 'N/A'):.4f}")
        md.append(f"- **qty neg rate**: hepta {hepta.get('qty_neg_rate', 0)*100:.1f}% vs SDV {sdv.get('qty_neg_rate', 0)*100:.1f}%")
        md.append(f"- **qty range**: hepta [{hepta.get('qty_min')}, {hepta.get('qty_max')}] vs SDV [{sdv.get('qty_min')}, {sdv.get('qty_max')}]\n")

    md.append("## SDV Metrics\n")
    for k, v in sdv.items():
        md.append(f"- **{k}**: {v}")

    md.append("\n## Hepta Metrics\n")
    for k, v in hepta.items():
        md.append(f"- **{k}**: {v}")

    if sdv and hepta:
        md.append("\n## Comparison (hepta - SDV)\n")
        for k, v in report["comparison"].items():
            md.append(f"- **{k}**: {v:+.4f}")

    Path("/tmp/case_a_report.md").write_text("\n".join(md))
    print("Report MD saved to /tmp/case_a_report.md")

if __name__ == "__main__":
    main()