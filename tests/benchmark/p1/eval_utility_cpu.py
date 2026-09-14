#!/usr/bin/env python3
"""CPU-only SynMeter utility driver excluding tab_transformer."""

import argparse
import importlib
import importlib.util
import json
import sys
import types
from pathlib import Path

N_EXPS = 5
EVALUATORS = ("lr", "rf", "mlp", "tree", "svm", "xgboost", "cat_boost")


def _install_cpu_import_shims():
    if importlib.util.find_spec("torch") is None:
        torch = types.ModuleType("torch")
        setattr(torch, "manual_seed", lambda seed: None)
        setattr(torch, "Tensor", type("Tensor", (), {}))
        nn = types.ModuleType("torch.nn")
        setattr(nn, "Module", type("Module", (), {}))
        functional = types.ModuleType("torch.nn.functional")
        setattr(torch, "nn", nn)
        sys.modules["torch"] = torch
        sys.modules["torch.nn"] = nn
        sys.modules["torch.nn.functional"] = functional
    tab = types.ModuleType("evaluator.utility.tab_transformer")

    def forbidden(*args, **kwargs):
        raise AssertionError("tab_transformer must not run in the CPU benchmark")

    setattr(tab, "train_tab_transformer", forbidden)
    sys.modules["evaluator.utility.tab_transformer"] = tab


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
            "utility_result": str(output_dir / f"utility_{model}.json"),
        },
        "model_params": {},
        "sample_params": {"num_samples": 0},
    }


def _cpu_train_and_test(helper, data, task_type, dataset, n_class, cuda, seed, tune=False):
    np = importlib.import_module("numpy")
    if cuda != "cpu":
        raise ValueError("P1 utility evaluation requires device 'cpu'")
    if tune:
        train_data, test = data
    else:
        train, val, test = data
        train_data = [
            np.concatenate([train[0], val[0]], axis=0),
            np.concatenate([train[1], val[1]], axis=0),
        ]
    trainers = {
        "lr": helper.train_lr,
        "rf": helper.train_rf,
        "mlp": helper.train_mlp,
        "tree": helper.train_tree,
        "svm": helper.train_svm,
        "xgboost": helper.train_xgb,
        "cat_boost": helper.train_catboost,
    }
    results = {}
    for name, trainer in trainers.items():
        params = helper.load_config(
            str(Path(helper.TUNED_PARAMS_PATH) / "evaluators" / name / f"{dataset}.toml")
        )
        _, results[name] = trainer(params, train_data, test, task_type, n_class)
    return results


def _ml_view(config, output_dir):
    """Adapt Adult's canonical `income` label to SynMeter's hardcoded `label`."""
    pd = importlib.import_module("pandas")
    paths = config["path_params"]
    view = output_dir / "synmeter-ml-view"
    view.mkdir(parents=True, exist_ok=True)
    copied = dict(paths)
    for key in ("train_data", "val_data", "test_data", "out_data"):
        target = view / f"{key}.csv"
        pd.read_csv(paths[key]).rename(columns={"income": "label"}).to_csv(target, index=False)
        copied[key] = str(target)
    meta = json.loads(Path(paths["meta_data"]).read_text(encoding="utf-8"))
    for column in meta["columns"]:
        if column["name"] == "income":
            column["name"] = "label"
    meta["label"] = "label"
    meta_path = view / "meta.json"
    meta_path.write_text(json.dumps(meta), encoding="utf-8")
    copied["meta_data"] = str(meta_path)
    return {**config, "path_params": copied}


def _relative_losses(ml_results):
    np = importlib.import_module("numpy")
    synthetic, real = ml_results
    losses = {}
    for model in EVALUATORS:
        per_run = []
        for metric, syn_values in synthetic[model].items():
            real_values = real[model][metric]
            for syn_value, real_value in zip(syn_values, real_values):
                denominator = max(abs(float(real_value)), 1e-12)
                if metric in ("rmse", "mse"):
                    loss = max(0.0, (float(syn_value) - float(real_value)) / denominator)
                else:
                    loss = max(0.0, (float(real_value) - float(syn_value)) / denominator)
                per_run.append(loss)
        losses[model] = {
            "mean": float(np.mean(per_run)),
            "std": float(np.std(per_run)),
        }
    return losses


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--synmeter-root", required=True, type=Path)
    parser.add_argument("--model", required=True, choices=("hepta", "sdv_gc"))
    parser.add_argument("--data-dir", required=True, type=Path)
    parser.add_argument("--output-dir", required=True, type=Path)
    args = parser.parse_args()
    pd = importlib.import_module("pandas")

    root = args.synmeter_root.resolve()
    sys.path.insert(0, str(root))
    _install_cpu_import_shims()
    info = _import_from_synmeter("lib.info")

    setattr(info, "ROOT_DIR", str(root))
    setattr(info, "TUNED_PARAMS_PATH", str(root / "exp"))
    setattr(info, "N_EXPS", N_EXPS)
    helper = _import_from_synmeter("evaluator.utility.eval_helper")

    setattr(helper, "TUNED_PARAMS_PATH", str(root / "exp"))
    setattr(
        helper,
        "train_and_test",
        lambda data, task, dataset, classes, device, seed, tune=False: _cpu_train_and_test(
            helper, data, task, dataset, classes, device, seed, tune
        ),
    )

    config = _config(args.model, args.data_dir.resolve(), args.output_dir.resolve())
    if not Path(config["path_params"]["out_model"]).exists():
        raise FileNotFoundError("run eval_fidelity_cpu.py first to train the synthesizer")
    synthesizer = importlib.import_module("synthesizer." + args.model)
    sample_count = sum(
        pd.read_csv(config["path_params"][name]).shape[0]
        for name in ("train_data", "val_data")
    )
    ml_results = [{}, {}]
    query_results = {}
    for seed in range(N_EXPS):
        synthesizer.sample(config, sample_count, seed)
        ml_config = _ml_view(config, args.output_dir.resolve() / args.model)
        ml_results = helper.ml_evaluation(
            ml_config, "adult", "cpu", seed, ml_results
        )
        query_results = helper.query_evaluation(
            config, query_results, n_samples=1000, seed=seed
        )
    relative_losses = _relative_losses(ml_results)
    output = Path(config["path_params"]["utility_result"])
    helper.save_utility_results(ml_results, query_results, str(output))
    payload = json.loads(output.read_text(encoding="utf-8"))
    payload["mla_relative_loss"] = relative_losses
    payload["evaluators"] = list(EVALUATORS)
    output.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
