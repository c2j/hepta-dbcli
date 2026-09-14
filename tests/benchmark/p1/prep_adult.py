#!/usr/bin/env python3
"""Download, clean, and deterministically split the UCI Adult dataset."""

import argparse
import importlib
import json
import urllib.request
from pathlib import Path

TRAIN_URL = "https://archive.ics.uci.edu/ml/machine-learning-databases/adult/adult.data"
TEST_URL = "https://archive.ics.uci.edu/ml/machine-learning-databases/adult/adult.test"
COLUMNS = [
    "age",
    "workclass",
    "fnlwgt",
    "education",
    "educational-num",
    "marital-status",
    "occupation",
    "relationship",
    "race",
    "sex",
    "capital-gain",
    "capital-loss",
    "hours-per-week",
    "native-country",
    "income",
]
CONTINUOUS = {
    "age",
    "fnlwgt",
    "educational-num",
    "capital-gain",
    "capital-loss",
    "hours-per-week",
}


def _download(url, destination):
    if not destination.exists():
        destination.parent.mkdir(parents=True, exist_ok=True)
        urllib.request.urlretrieve(url, destination)


def _read(path, skiprows=0):
    pd = importlib.import_module("pandas")

    return pd.read_csv(
        path,
        names=COLUMNS,
        header=None,
        skiprows=skiprows,
        skipinitialspace=True,
        na_values="?",
    )


def prepare(output_dir):
    np = importlib.import_module("numpy")
    pd = importlib.import_module("pandas")

    output_dir.mkdir(parents=True, exist_ok=True)
    raw_dir = output_dir / "raw"
    train_raw = raw_dir / "adult.data"
    test_raw = raw_dir / "adult.test"
    _download(TRAIN_URL, train_raw)
    _download(TEST_URL, test_raw)

    data = pd.concat([_read(train_raw), _read(test_raw, skiprows=1)], ignore_index=True)
    for column in data.select_dtypes(include="object").columns:
        data[column] = data[column].str.strip()
    data["income"] = data["income"].str.removesuffix(".")
    data = data.dropna().reset_index(drop=True)

    permutation = np.random.RandomState(42).permutation(len(data))
    train_end = int(len(data) * 0.70)
    val_end = train_end + int(len(data) * 0.15)
    splits = {
        "train": data.iloc[permutation[:train_end]].reset_index(drop=True),
        "val": data.iloc[permutation[train_end:val_end]].reset_index(drop=True),
        "test": data.iloc[permutation[val_end:]].reset_index(drop=True),
    }
    for name, frame in splits.items():
        frame.to_csv(output_dir / f"{name}.csv", index=False)

    metadata_columns = []
    for name in COLUMNS:
        if name in CONTINUOUS:
            metadata_columns.append(
                {
                    "name": name,
                    "type": "continuous",
                    "i2s": [],
                    "size": 0,
                    "min": float(data[name].min()),
                    "max": float(data[name].max()),
                }
            )
        else:
            values = sorted(data[name].astype(str).unique().tolist())
            metadata_columns.append(
                {
                    "name": name,
                    "type": "discrete",
                    "i2s": values,
                    "size": len(values),
                    "min": None,
                    "max": None,
                }
            )
    metadata = {
        "columns": metadata_columns,
        "task": "binary_classification",
        "train_size": len(splits["train"]),
        "val_size": len(splits["val"]),
        "test_size": len(splits["test"]),
        "label": "income",
    }
    (output_dir / "meta.json").write_text(
        json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(json.dumps({"rows": len(data), **{k: len(v) for k, v in splits.items()}}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path(__file__).resolve().parent / "data" / "adult",
        help="destination for raw files, CSV splits, and meta.json",
    )
    args = parser.parse_args()
    prepare(args.output_dir.resolve())


if __name__ == "__main__":
    main()
