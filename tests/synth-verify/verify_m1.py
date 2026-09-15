#!/usr/bin/env python3
"""Assert the M1 acceptance behaviour on artifacts produced by run_m1.sh.

Usage: verify_m1.py <output-dir> <schema>

Everything here is derived from the generated artifacts (model JSON, CSV, SQL)
plus the fixture definition in fixture_mysql.sql. The checks are deliberately
independent of the Rust unit tests: they are the "real database" half of the
acceptance criteria in issues #62, #63, #64 and #65.
"""

import csv
import datetime
import json
import pathlib
import re
import sys

# Must match fixture_mysql.sql / run_m1.sh.
GENERATED_ROWS = 1000  # run_m1.sh passes --rows 1000
CHILD_DISTINCT_PARENTS = 700  # parents reused by the fixture's FK data
DICTIONARY_LEVELS = 120
DATETIME_RE = re.compile(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}")
DECIMAL_RE = re.compile(r"\d+\.\d{1,4}")


def epoch_text(epoch: float, fmt: str) -> str:
    """Render an epoch the same way the generator does (naive/UTC)."""
    moment = datetime.datetime.fromtimestamp(epoch, datetime.timezone.utc)
    return moment.strftime(fmt.replace("%:z", "").strip())

failures: list[str] = []
checks = 0


def check(ok: bool, label: str, detail: str = "") -> None:
    global checks
    checks += 1
    if ok:
        print(f"  PASS {label}")
    else:
        print(f"  FAIL {label}{' :: ' + detail if detail else ''}")
        failures.append(label)


def read_rows(path: pathlib.Path) -> list[dict[str, str]]:
    with path.open() as handle:
        return list(csv.DictReader(handle))


def main() -> int:
    out = pathlib.Path(sys.argv[1])
    schema = sys.argv[2]

    parent_model = json.loads((out / "m1_verify_parent.model.json").read_text())
    child_model = json.loads((out / "m1_verify_child.model.json").read_text())
    parent_cols = parent_model["columns"]
    parent_rows = read_rows(out / "csv" / "m1_verify_parent.csv")
    child_rows = read_rows(out / "csv" / "m1_verify_child.csv")

    print("#62 datetime columns")
    trade_time = parent_cols["trade_time"]
    check(
        trade_time["logical_type"] == "datetime",
        "trade_time is modelled as datetime",
        str(trade_time["logical_type"]),
    )
    check(
        trade_time.get("datetime_format") == "%Y-%m-%d %H:%M:%S",
        "primary datetime format inferred",
        str(trade_time.get("datetime_format")),
    )
    check(
        trade_time.get("datetime_epoch") is True,
        "datetime modelled in epoch space",
        str(trade_time.get("datetime_epoch")),
    )
    check(
        trade_time.get("min") is not None and trade_time.get("max") is not None,
        "epoch min/max recorded",
    )
    values = [row["trade_time"] for row in parent_rows]
    check(
        all(DATETIME_RE.fullmatch(value) for value in values),
        "generated datetime matches the source format exactly",
    )
    lo = epoch_text(trade_time["min"], trade_time["datetime_format"])
    hi = epoch_text(trade_time["max"], trade_time["datetime_format"])
    check(
        all(lo <= value <= hi for value in values),
        "generated datetime stays inside the trained epoch range",
        f"values {min(values)}..{max(values)} vs model {lo}..{hi}",
    )
    check(
        min(values) <= "2021-06-01 00:00:00" and max(values) >= "2023-01-01 00:00:00",
        "generated datetimes spread over the trained range instead of collapsing",
        f"{min(values)}..{max(values)}",
    )

    print("#63 NULL reproduction")
    check(
        abs(parent_cols["email"]["null_rate"] - 0.2) < 1e-9,
        "trained null_rate learned for email",
        str(parent_cols["email"]["null_rate"]),
    )
    null_share = sum(1 for row in parent_rows if row["email"] == "") / len(parent_rows)
    check(
        0.17 <= null_share <= 0.23,
        "generated NULL share reproduces the training rate",
        f"{null_share:.4f}",
    )
    check(
        all("user" in row["email"] for row in parent_rows if row["email"]),
        "non-NULL values stay in the trained value space",
    )
    check(
        sum(1 for row in child_rows if row["note"] == "") > 0,
        "nullable FK-table column also reproduces NULLs",
    )

    print("#64 decimal scale")
    check(
        parent_cols["cjje"].get("decimal_scale") == 4,
        "DECIMAL(18,4) scale learned",
        str(parent_cols["cjje"].get("decimal_scale")),
    )
    check(
        parent_cols["discount_rate"].get("decimal_scale") == 2,
        "DECIMAL(4,2) scale learned",
        str(parent_cols["discount_rate"].get("decimal_scale")),
    )
    check(
        all(DECIMAL_RE.fullmatch(row["cjje"]) for row in parent_rows)
        and all(DECIMAL_RE.fullmatch(row["discount_rate"]) for row in parent_rows),
        "generated decimals sit on the learned scale (no binary tail digits)",
    )

    print("#65a --categorical-top-k full")
    levels = parent_cols["code"]["marginal"]["values"]
    check(
        len(levels) == DICTIONARY_LEVELS,
        "all dictionary levels survive training under --categorical-top-k full",
        f"{len(levels)} of {DICTIONARY_LEVELS}",
    )
    check(
        {row["code"] for row in parent_rows} <= set(levels),
        "generated dictionary values come from the observed levels",
    )

    print("#65d SQL schema qualifier")
    parent_sql = (out / "sql" / "m1_verify_parent.sql").read_text().splitlines()[0]
    child_sql = (out / "sql" / "m1_verify_child.sql").read_text().splitlines()[0]
    check(
        parent_sql.startswith(f"INSERT INTO `{schema}`.`m1_verify_parent`"),
        "SQL export is schema qualified by default",
        parent_sql[:60],
    )
    check(
        child_sql.startswith(f"INSERT INTO `{schema}`.`m1_verify_child`"),
        "child SQL export is schema qualified too",
        child_sql[:60],
    )
    legacy_sql = (out / "sql-legacy" / "m1_verify_parent.sql").read_text().splitlines()[0]
    check(
        legacy_sql.startswith("INSERT INTO `m1_verify_parent`"),
        "--no-schema-qualifier restores the legacy statement",
        legacy_sql[:60],
    )

    print("#65b/c FK integrity and rules-draft")
    rules = (out / "rules.yaml").read_text()
    check(
        "references:" in rules and "m1_verify_parent.id" in rules,
        "rules-draft discovered the foreign key",
    )
    parent_ids = {row["id"] for row in parent_rows}
    check(
        all(row["parent_id"] in parent_ids for row in child_rows),
        "every child key exists in the generated parent pool",
    )
    distinct = len({row["parent_id"] for row in child_rows})
    check(
        distinct <= CHILD_DISTINCT_PARENTS,
        "non-unique FK reuses the parent pool instead of inventing keys",
        f"{distinct} distinct",
    )

    print("#65f schema default")
    check(
        parent_model.get("schema") == schema,
        "train records the resolved schema in the model",
        str(parent_model.get("schema")),
    )
    check(
        parent_model["provenance"].get("truncated") in (None, False),
        "a non-Oracle sample of the full row count is not flagged truncated",
    )

    print("#62/#63 model completeness")
    check(
        len(parent_rows) == GENERATED_ROWS and len(child_rows) == GENERATED_ROWS,
        "requested row counts produced",
        f"parent {len(parent_rows)}, child {len(child_rows)}",
    )
    check(
        set(child_model["columns"]) == {"id", "parent_id", "note"},
        "no column silently dropped from the child model",
        str(sorted(child_model["columns"])),
    )

    print("seed determinism")
    for table in ("m1_verify_parent", "m1_verify_child"):
        first = (out / "csv" / f"{table}.csv").read_bytes()
        again = (out / "csv-again" / f"{table}.csv").read_bytes()
        check(first == again, f"{table} re-generated byte-identically for the same seed")

    print()
    if failures:
        print(f"{len(failures)} of {checks} checks FAILED:")
        for label in failures:
            print(f"  - {label}")
        return 1
    print(f"all {checks} checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
