#!/usr/bin/env python3
"""Evaluate P1 gates, write report.json/REPORT.md, and return gate status."""

import argparse
import json
import math
import sys
from pathlib import Path

FIDELITY_KEYS = (
    "cont_error",
    "cat_error",
    "cont_cont_error",
    "cat_cont_error",
    "cat_cat_error",
)
ML_MODELS = ("lr", "rf", "mlp", "tree", "svm", "xgboost", "cat_boost")


def _metric_mean(value):
    if isinstance(value, dict):
        value = value["mean"]
    if isinstance(value, list):
        value = sum(value) / len(value)
    result = float(value)
    if not math.isfinite(result):
        raise ValueError(f"metric is not finite: {result}")
    return result


def _ratio(hepta, sdv):
    if sdv == 0.0:
        return 1.0 if hepta == 0.0 else math.inf
    return hepta / sdv


def _relative_gate(gate_id, metric, hepta, sdv):
    ratio = _ratio(hepta, sdv)
    return {
        "id": gate_id,
        "metric": metric,
        "hepta_mean": hepta,
        "sdv_mean": sdv,
        "ratio": ratio,
        "threshold": 1.15,
        "passed": ratio <= 1.15,
    }


def evaluate(fidelity, utility):
    hf, sf = fidelity["hepta"], fidelity["sdv_gc"]
    hu, su = utility["hepta"], utility["sdv_gc"]
    gates = [
        _relative_gate("G1", "cont_error", _metric_mean(hf["cont_error"]), _metric_mean(sf["cont_error"])),
        _relative_gate("G2", "cat_error", _metric_mean(hf["cat_error"]), _metric_mean(sf["cat_error"])),
        _relative_gate(
            "G3",
            "fidelity_5_key_mean",
            sum(_metric_mean(hf[key]) for key in FIDELITY_KEYS) / len(FIDELITY_KEYS),
            sum(_metric_mean(sf[key]) for key in FIDELITY_KEYS) / len(FIDELITY_KEYS),
        ),
        _relative_gate(
            "G4",
            "mla_7_model_relative_loss_mean",
            sum(_metric_mean(hu["mla_relative_loss"][key]) for key in ML_MODELS) / len(ML_MODELS),
            sum(_metric_mean(su["mla_relative_loss"][key]) for key in ML_MODELS) / len(ML_MODELS),
        ),
        _relative_gate(
            "G5",
            "query_error_3_way",
            _metric_mean(hu["range_query"]["3_way_range"]),
            _metric_mean(su["range_query"]["3_way_range"]),
        ),
    ]
    # P1 is Adult-only; the payment.amount on-grid gate lives in the P2
    # harness (run_p2.sh), the only place payment flows end to end.
    return {"gates": gates}


def _markdown(report):
    lines = [
        "# P1 SynMeter Benchmark Report",
        "",
        "UCI Adult; deterministic seed-42 70/15/15 split; five repetitions (seeds 0–4).",
        "MLA uses lr/rf/mlp/tree/svm/xgboost/cat_boost on CPU, synthetic train/val and real test.",
        "",
        "| Gate | Metric | hepta | SDV-GC | Ratio | Threshold | Result |",
        "|---|---|---:|---:|---:|---:|---|",
    ]
    for gate in report["gates"]:
        values = (
            f"{gate['hepta_mean']:.6g}",
            f"{gate['sdv_mean']:.6g}",
            f"{gate['ratio']:.6g}",
            f"<= {gate['threshold']:.2f}",
            "PASS" if gate["passed"] else "FAIL",
        )
        lines.append(f"| {gate['id']} | {gate['metric']} | " + " | ".join(values) + " |")
    return "\n".join(lines) + "\n"


def _print_table(report):
    print("gate metric                              ratio threshold result")
    for gate in report["gates"]:
        result = "PASS" if gate["passed"] else "FAIL"
        print(f"{gate['id']:<4} {gate['metric']:<35} {gate['ratio']:>6.3f} {gate['threshold']:>9.2f} {result}")


def _self_test(expect):
    baseline_fidelity = {key: {"mean": 1.0, "std": 0.0} for key in FIDELITY_KEYS}
    factor = 1.0 if expect == "good" else 1.20
    candidate_fidelity = {key: {"mean": factor, "std": 0.0} for key in FIDELITY_KEYS}
    baseline_utility = {
        "mla_relative_loss": {key: {"mean": 1.0} for key in ML_MODELS},
        "range_query": {"3_way_range": {"mean": 1.0}},
    }
    candidate_utility = {
        "mla_relative_loss": {key: {"mean": factor} for key in ML_MODELS},
        "range_query": {"3_way_range": {"mean": factor}},
    }
    report = evaluate(
        {"hepta": candidate_fidelity, "sdv_gc": baseline_fidelity},
        {"hepta": candidate_utility, "sdv_gc": baseline_utility},
    )
    _print_table(report)
    return 0 if all(g["passed"] for g in report["gates"]) else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results-dir", type=Path)
    parser.add_argument("--output-dir", type=Path, default=Path(__file__).resolve().parent)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--self-test-good", action="store_true")
    args = parser.parse_args()
    if args.self_test or args.self_test_good:
        return _self_test("good" if args.self_test_good else "bad")
    if args.results_dir is None:
        parser.error("--results-dir is required unless --self-test is used")
    results = args.results_dir.resolve()
    fidelity = {
        model: json.loads((results / f"fidelity_{model}.json").read_text(encoding="utf-8"))
        for model in ("hepta", "sdv_gc")
    }
    utility = {
        model: json.loads((results / f"utility_{model}.json").read_text(encoding="utf-8"))
        for model in ("hepta", "sdv_gc")
    }
    report = evaluate(fidelity, utility)
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    (output / "report.json").write_text(
        json.dumps(report, indent=2, sort_keys=True, allow_nan=False) + "\n", encoding="utf-8"
    )
    (output / "REPORT.md").write_text(_markdown(report), encoding="utf-8")
    _print_table(report)
    return 0 if all(gate["passed"] for gate in report["gates"]) else 1


if __name__ == "__main__":
    sys.exit(main())
