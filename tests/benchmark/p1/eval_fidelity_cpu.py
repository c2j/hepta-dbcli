#!/usr/bin/env python3
"""CPU-only five-run fidelity driver using SynMeter's evaluator."""

import argparse
import importlib
import importlib.util
import sys
import types
from pathlib import Path

N_EXPS = 5


def _install_torch_stub():
    if importlib.util.find_spec("torch") is not None:
        return
    torch = types.ModuleType("torch")
    setattr(torch, "manual_seed", lambda seed: None)
    setattr(torch, "Tensor", type("Tensor", (), {}))
    nn = types.ModuleType("torch.nn")
    setattr(nn, "Module", type("Module", (), {}))
    setattr(torch, "nn", nn)
    sys.modules["torch"] = torch
    sys.modules["torch.nn"] = nn


def _import_from_synmeter(module_name):
    return importlib.import_module(module_name)


def _config(model, data_dir, output_dir):
    model_dir = output_dir / model
    model_suffix = "json" if model == "hepta" else "pkl"
    return {
        "path_params": {
            "meta_data": str(data_dir / "meta.json"),
            "train_data": str(data_dir / "train.csv"),
            "val_data": str(data_dir / "val.csv"),
            "test_data": str(data_dir / "test.csv"),
            "out_model": str(model_dir / f"model.{model_suffix}"),
            "out_data": str(model_dir / "adult.csv"),
            "fidelity_result": str(output_dir / f"fidelity_{model}.json"),
        },
        "model_params": {},
        "sample_params": {"num_samples": 0},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--synmeter-root", required=True, type=Path)
    parser.add_argument("--model", required=True, choices=("hepta", "sdv_gc"))
    parser.add_argument("--data-dir", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    args = parser.parse_args()

    root = args.synmeter_root.resolve()
    sys.path.insert(0, str(root))
    _install_torch_stub()
    info = _import_from_synmeter("lib.info")

    setattr(info, "ROOT_DIR", str(root))
    setattr(info, "TUNED_PARAMS_PATH", str(root / "exp"))
    setattr(info, "N_EXPS", N_EXPS)
    helper = _import_from_synmeter("evaluator.fidelity.eval_helper")

    config = _config(args.model, args.data_dir.resolve(), args.output_dir.resolve())
    synthesizer = importlib.import_module("synthesizer." + args.model)
    synthesizer.train(config, "cpu", 0)
    sample_count = sum(
        __import__("pandas").read_csv(config["path_params"][name]).shape[0]
        for name in ("train_data", "val_data")
    )
    results = {}
    for seed in range(N_EXPS):
        synthesizer.sample(config, sample_count, seed)
        current = helper.fidelity_evaluation(config, seed, eval_type="test")
        results = helper.add_fidelity_results(current, results)
    helper.save_fidelity_results(results, config["path_params"]["fidelity_result"])


if __name__ == "__main__":
    main()
