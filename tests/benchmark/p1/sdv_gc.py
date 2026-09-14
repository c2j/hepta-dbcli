#!/usr/bin/env python3
"""SynMeter plugin for the SDV Gaussian-copula baseline."""

import json
import importlib
from pathlib import Path

pd = importlib.import_module("pandas")
SingleTableMetadata = importlib.import_module("sdv.metadata").SingleTableMetadata
GaussianCopulaSynthesizer = importlib.import_module(
    "sdv.single_table"
).GaussianCopulaSynthesizer


def _discrete_columns(meta_path):
    with open(meta_path, encoding="utf-8") as handle:
        return [
            column["name"]
            for column in json.load(handle)["columns"]
            if column["type"] == "discrete"
        ]


def train(config, cuda, seed=0):
    """Fit the fixed normal-distribution SDV comparison model."""
    del cuda, seed
    paths = config["path_params"]
    data = pd.concat(
        [pd.read_csv(paths["train_data"]), pd.read_csv(paths["val_data"])],
        ignore_index=True,
    )
    metadata = SingleTableMetadata()
    metadata.detect_from_dataframe(data)
    for column in data.columns:
        sdtype = "categorical" if column in _discrete_columns(paths["meta_data"]) else "numerical"
        metadata.update_column(column_name=column, sdtype=sdtype)
    synthesizer = GaussianCopulaSynthesizer(
        metadata,
        default_distribution="norm",
        enforce_min_max_values=True,
        enforce_rounding=True,
    )
    synthesizer.fit(data)
    output = Path(paths["out_model"])
    output.parent.mkdir(parents=True, exist_ok=True)
    synthesizer.save(filepath=str(output))


def sample(config, n_samples=0, seed=0):
    """Sample deterministically and preserve metadata column order and types."""
    paths = config["path_params"]
    if n_samples <= 0:
        n_samples = sum(
            len(pd.read_csv(paths[name])) for name in ("train_data", "val_data")
        )
    synthesizer = GaussianCopulaSynthesizer.load(filepath=paths["out_model"])
    synthesizer._set_random_state(seed)
    data = synthesizer.sample(num_rows=n_samples)
    with open(paths["meta_data"], encoding="utf-8") as handle:
        meta = json.load(handle)
    columns = [column["name"] for column in meta["columns"]]
    for column in _discrete_columns(paths["meta_data"]):
        data[column] = data[column].astype(str)
    output = Path(paths["out_data"])
    output.parent.mkdir(parents=True, exist_ok=True)
    data.loc[:, columns].to_csv(output, index=False)


def tune(config, cuda, dataset, seed=0):
    """P1 deliberately performs no hyperparameter search."""
    del cuda, dataset, seed
    return config
