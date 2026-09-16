#!/usr/bin/env python3
"""Assert the #72 child-cardinality behaviour on artifacts from run_m4_cardinality.sh.

Usage: verify_cardinality.py <output-dir>

Reads the trained child model (learned distribution), the modeled CSV and the
exact-rows CSV. The checks are the "real database" half of issue #72's ACs.
"""

import csv
import json
import pathlib
import sys

PARENT_ROWS = 100
TRAINING = {0: 0.5, 1: 0.3, 2: 0.2}
EXACT_ROWS = 100

failures = []


def check(condition, message):
    if condition:
        print(f"  PASS {message}")
    else:
        print(f"  FAIL {message}")
        failures.append(message)


def read_rows(path):
    with path.open(newline="") as handle:
        return list(csv.DictReader(handle))


def main():
    out = pathlib.Path(sys.argv[1])

    model = json.loads((out / "models" / "card_child.model.json").read_text())
    dist = model.get("fk_cardinality", {}).get("parent_id")
    check(dist is not None, "the child model stores a learned parent_id distribution")
    if dist:
        counts = {int(k): v for k, v in dist["counts"].items()}
        check(
            abs(counts.get(0, 0.0) - 0.5) < 1e-9
            and abs(counts.get(1, 0.0) - 0.3) < 1e-9
            and abs(counts.get(2, 0.0) - 0.2) < 1e-9,
            f"learned distribution is exactly {{0:0.5, 1:0.3, 2:0.2}}, got {counts}",
        )
        check(dist.get("null_share", 0.0) == 0.0, "no NULL FK share is learned")

    parent = read_rows(out / "modeled" / "card_parent.csv")
    child = read_rows(out / "modeled" / "card_child.csv")
    check(len(parent) == PARENT_ROWS, f"modeled parent rows == {PARENT_ROWS}, got {len(parent)}")

    parent_keys = {row["id"] for row in parent}
    child_keys = [row["parent_id"] for row in child if row["parent_id"] != ""]
    check(
        all(key in parent_keys for key in child_keys),
        "every non-NULL child key exists in the generated parent keys",
    )

    per_parent = {}
    for key in child_keys:
        per_parent[key] = per_parent.get(key, 0) + 1
    zero = len(parent_keys) - len(per_parent)
    buckets = {}
    for count in per_parent.values():
        buckets[count] = buckets.get(count, 0) + 1
    buckets[0] = zero
    total = len(parent_keys)
    shares = {bucket: n / total for bucket, n in buckets.items()}

    average = len(child) / total
    check(0.4 <= average <= 1.0, f"average children per parent {average:.2f} in [0.4, 1.0]")
    check(0.4 <= shares.get(0, 0.0) <= 0.6, f"zero-children share {shares.get(0, 0.0):.2f} in [0.4, 0.6]")

    # Total variation against the training distribution (AC4): the trained
    # buckets plus everything we still generate.
    support = set(TRAINING) | set(shares)
    tv = 0.5 * sum(abs(shares.get(b, 0.0) - TRAINING.get(b, 0.0)) for b in support)
    check(tv < 0.1, f"cardinality TV distance {tv:.3f} < 0.1")

    exact = read_rows(out / "exact" / "card_child.csv")
    check(
        len(exact) == EXACT_ROWS,
        f"without `modeled` the child keeps --rows ({EXACT_ROWS}), got {len(exact)}",
    )

    report = json.loads((out / "report.json").read_text())
    tvs = [
        item["cardinality_tv"]
        for table in report["tables"]
        for item in (table.get("fk", {}).get("items") or [])
        if item.get("cardinality_tv") is not None
    ]
    check(bool(tvs), "synth report carries a cardinality TV for the modeled edge")
    check(all(tv < 0.1 for tv in tvs), f"report cardinality TV < 0.1, got {tvs}")

    if failures:
        print(f"\n{len(failures)} cardinality check(s) failed")
        return 1
    print("\ncardinality assertions: learned distribution, shape, FK integrity, exact-rows default OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
