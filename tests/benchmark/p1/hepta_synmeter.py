#!/usr/bin/env python3
"""SynMeter plugin for the hepta-dbcli synthesizer."""

import json
import importlib
import os
import shutil
import subprocess
from pathlib import Path

pd = importlib.import_module("pandas")
duckdb = importlib.import_module("duckdb")


def _paths(config):
    return config["path_params"]


def _binary():
    value = os.environ.get("HEPTA_BIN")
    if not value:
        raise RuntimeError("HEPTA_BIN is required and must point to hepta_dbcli")
    binary = Path(value).expanduser().resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise RuntimeError(f"HEPTA_BIN is not executable: {binary}")
    return binary


def _workdir():
    value = os.environ.get("HEPTA_P1_WORKDIR")
    if not value:
        raise RuntimeError("HEPTA_P1_WORKDIR is required")
    path = Path(value).expanduser().resolve()
    path.mkdir(parents=True, exist_ok=True)
    return path


def _run(*args):
    subprocess.run([str(_binary()), *map(str, args)], check=True)


def _sql_literal(path):
    return str(path).replace("'", "''")


def train(config, cuda, seed=0):
    """Load train+validation data into DuckDB and train hepta's model."""
    del cuda, seed
    paths = _paths(config)
    workdir = _workdir()
    combined = workdir / "adult_train_val.csv"
    frames = [pd.read_csv(paths["train_data"]), pd.read_csv(paths["val_data"])]
    pd.concat(frames, ignore_index=True).to_csv(combined, index=False)

    database = workdir / "p1.duckdb"
    config_path = workdir / "p1_duckdb.toml"
    config_path.write_text(
        "default_connection = \"p1\"\n\n"
        "[connections.p1]\n"
        "driver = \"duckdb\"\n"
        f'database = "{str(database).replace(chr(34), chr(92) + chr(34))}"\n',
        encoding="utf-8",
    )
    models = workdir / "models"
    rules = workdir / "rules.yaml"
    shutil.rmtree(models, ignore_errors=True)
    database.unlink(missing_ok=True)
    # hepta's DuckDB pool refuses missing/invalid files, so bootstrap a
    # valid empty database file with the pip duckdb package first.
    duckdb.connect(str(database)).close()
    _run(
        "--config",
        config_path,
        "cli",
        "--sql",
        "CREATE OR REPLACE TABLE adult AS SELECT * "
        f"FROM read_csv_auto('{_sql_literal(combined)}', header=true)",
    )
    _run(
        "--config",
        config_path,
        "synth",
        "train",
        "--name",
        "p1",
        "--tables",
        "adult",
        "--output",
        models,
    )
    _run(
        "--config",
        config_path,
        "synth",
        "rules-draft",
        "--name",
        "p1",
        "--tables",
        "adult",
        "--models",
        models,
        "--output",
        rules,
    )
    marker = Path(paths["out_model"])
    marker.parent.mkdir(parents=True, exist_ok=True)
    marker.write_text(
        json.dumps({"models": str(models), "rules": str(rules)}), encoding="utf-8"
    )


def sample(config, n_samples=0, seed=0):
    """Generate a CSV and enforce SynMeter's metadata column contract."""
    paths = _paths(config)
    workdir = _workdir()
    marker = json.loads(Path(paths["out_model"]).read_text(encoding="utf-8"))
    if n_samples <= 0:
        n_samples = sum(
            len(pd.read_csv(paths[name])) for name in ("train_data", "val_data")
        )
    generated = workdir / f"generated-{seed}"
    shutil.rmtree(generated, ignore_errors=True)
    _run(
        "synth",
        "generate",
        "--models",
        marker["models"],
        "--rules",
        marker["rules"],
        "--output",
        generated,
        "--rows",
        str(n_samples),
        "--seed",
        str(seed),
        "--format",
        "csv",
    )
    frame = pd.read_csv(generated / "adult.csv")
    with open(paths["meta_data"], encoding="utf-8") as handle:
        expected = [column["name"] for column in json.load(handle)["columns"]]
    if list(frame.columns) != expected:
        raise RuntimeError(
            f"hepta output columns {list(frame.columns)!r} do not match metadata {expected!r}"
        )
    output = Path(paths["out_data"])
    output.parent.mkdir(parents=True, exist_ok=True)
    frame.to_csv(output, index=False)


def tune(config, cuda, dataset, seed=0):
    """P1 deliberately performs no hyperparameter search."""
    del cuda, dataset, seed
    return config
