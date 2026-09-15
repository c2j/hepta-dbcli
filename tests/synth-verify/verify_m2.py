#!/usr/bin/env python3
"""Assertions for the M2 synth end-to-end run (tests/synth-verify/run_m2.sh).

Usage: verify_m2.py OUT SCHEMA GOOD_CODE SECOND_CODE DEGRADED_CODE FK_DB_CODE \
                    LENIENT_CODE STRICT_CODE

Checks the artifacts produced by run_m2.sh:
  * `train` wrote a holdout baseline per table, and it holds aggregate
    summaries only (no raw rows);
  * #66 auto-selection is active on the trained model (the fixture's numeric
    columns are not a blanket Normal);
  * `synth report` scored shapes, pairs and the FK edge offline (against the
    generated parent keys), deterministically, and its JSON stays
    machine-readable;
  * a degraded numeric column is detected (score < 0.8 and >= 0.15 lower than
    the clean run) and trips --min-score (exit code != 0);
  * `--against-db` scores the foreign key against the live key pool at 1.0;
  * a missing baseline is an honest skip (exit 0) unless --strict is given.
"""

import json
import os
import sys

TABLES = ("m1_verify_parent", "m1_verify_child")
# Numeric columns whose model marginal must not be a blanket Normal: the
# fixture's arithmetic progressions fit Uniform/ECDF (issue #66).
AUTO_SELECTED_COLUMNS = ("id", "cjje", "whole_dec")
PARAMETRIC_FAMILIES = {"uniform", "ecdf", "gamma", "beta"}
ALLOWED_BASELINE_KEYS = {"schema_version", "table", "holdout_rows", "columns", "pairs"}
SHIFTED_COLUMN = "cjje"


class Failure(Exception):
    pass


def load_json(path):
    with open(path) as handle:
        return json.load(handle)


def check(condition, message):
    if not condition:
        raise Failure(message)


def check_marginals(out):
    """The trained model must show auto-selection at work (#66)."""
    model = load_json(os.path.join(out, "models", "m1_verify_parent.model.json"))
    for name in AUTO_SELECTED_COLUMNS:
        family = model["columns"][name]["marginal"]["name"]
        check(
            family in PARAMETRIC_FAMILIES,
            f"{name}: marginal '{family}' outside {sorted(PARAMETRIC_FAMILIES)}; "
            "auto-selection regressed to a blanket normal",
        )
    # Formatted datetimes keep Normal (auto-selection on the epoch axis is not
    # part of #66).
    check(
        model["columns"]["trade_time"]["marginal"]["name"] == "norm",
        "formatted datetime must keep the Normal epoch marginal",
    )


def check_baselines(out):
    for table in TABLES:
        path = os.path.join(out, "models", f"{table}.report-baseline.json")
        check(os.path.isfile(path), f"missing baseline {path}")
        baseline = load_json(path)
        check(baseline["schema_version"] == 1, f"{table}: schema_version")
        check(baseline["holdout_rows"] > 0, f"{table}: empty holdout")
        check(baseline["table"] == table, f"{table}: table name")
        extra = set(baseline) - ALLOWED_BASELINE_KEYS
        check(not extra, f"{table}: unexpected baseline keys {extra}")
        for name, column in baseline["columns"].items():
            keys = set(column)
            check(
                keys == {"kind", "knots"} or keys == {"kind", "values"},
                f"{table}.{name}: baseline column shape {keys}",
            )
            check(
                isinstance(column["knots"], list)
                if column["kind"] == "numerical"
                else isinstance(column["values"], list),
                f"{table}.{name}: malformed baseline payload",
            )
        # Structural privacy: every payload is an aggregate of a fixed shape,
        # so no entry can be a stored row.
        for name, column in baseline["columns"].items():
            check_payload(column, f"{table}.{name}")
        for pair in baseline.get("pairs", []):
            keys = set(pair)
            check(
                keys == {"kind", "left", "right", "pearson"}
                or keys == {"kind", "left", "right", "joint"},
                f"{table}: unexpected pair keys {keys}",
            )


def check_payload(column, path):
    """A column payload is either numeric knots (list of numbers) or value
    frequencies (list of [value, frequency]) - never a row."""
    if column["kind"] == "numerical":
        check(
            all(isinstance(knot, (int, float)) for knot in column["knots"]),
            f"{path}: non-numeric knot",
        )
        return
    for entry in column["values"]:
        check(
            isinstance(entry, list)
            and len(entry) == 2
            and isinstance(entry[0], str)
            and isinstance(entry[1], (int, float)),
            f"{path}: malformed frequency entry {entry!r}",
        )


def table_by_name(report, name):
    for table in report["tables"]:
        if table["table"] == name:
            return table
    raise Failure(f"report has no table {name}")


def column_scores(table):
    shapes = table["shapes"]
    check(shapes["status"] == "scored", f"{table['table']}: shapes not scored")
    check(0.0 <= shapes["score"] <= 1.0, f"{table['table']}: shape score range")
    return {item["name"]: item for item in shapes["items"]}


def check_report_shape(report):
    check(report["schema_version"] == 1, "report schema_version")
    check(len(report["tables"]) == len(TABLES), "report table count")
    score = report["overall_score"]
    check(score is not None and 0.0 < score < 1.0, f"overall score {score}")
    for table in report["tables"]:
        check(
            set(table) >= {"table", "rows", "shapes", "pairs", "fk", "score"},
            f"{table['table']}: report section keys {sorted(table)}",
        )
        check(table["rows"] > 0, f"{table['table']}: no generated rows")
        check("status" in table["pairs"], f"{table['table']}: pairs section")
        check("status" in table["fk"], f"{table['table']}: fk section")


def main():
    (
        out,
        schema,
        good_code,
        second_code,
        degraded_code,
        fk_db_code,
        lenient_code,
        strict_code,
    ) = sys.argv[1:9]
    good_code = int(good_code)
    second_code = int(second_code)
    degraded_code = int(degraded_code)
    fk_db_code = int(fk_db_code)
    lenient_code = int(lenient_code)
    strict_code = int(strict_code)

    try:
        check_marginals(out)
        check_baselines(out)

        report = load_json(os.path.join(out, "report.json"))
        check_report_shape(report)
        check(good_code == 0, f"clean report should pass --min-score, exit {good_code}")

        second = load_json(os.path.join(out, "report2.json"))
        check(second == report, "two identical reports must match")
        with open(os.path.join(out, "report.json")) as handle:
            first_bytes = handle.read()
        with open(os.path.join(out, "report2.json")) as handle:
            second_bytes = handle.read()
        check(first_bytes == second_bytes, "report JSON must be byte-identical")
        check(second_code == 0, f"second report exit {second_code}")

        # Degradation: the shifted column loses its shape and trips the gate.
        degraded = load_json(os.path.join(out, "report_degraded.json"))
        parent = table_by_name(report, "m1_verify_parent")
        degraded_parent = table_by_name(degraded, "m1_verify_parent")
        clean_score = column_scores(parent)[SHIFTED_COLUMN]["score"]
        shifted = column_scores(degraded_parent)[SHIFTED_COLUMN]
        check(
            shifted["score"] < 0.8,
            f"degraded {SHIFTED_COLUMN} score {shifted['score']} should be < 0.8",
        )
        check(
            clean_score - shifted["score"] > 0.15,
            f"degradation must be visible: clean {clean_score} shifted {shifted['score']}",
        )
        check(
            degraded["overall_score"] < report["overall_score"],
            "degraded overall score must drop",
        )
        check(degraded_code != 0, f"degraded data must fail --min-score, exit {degraded_code}")

        # Foreign keys offline: the pool is the generated parent column, so a
        # correct generator scores 1.0 without --against-db.
        child_offline = table_by_name(report, "m1_verify_child")
        offline_fk = child_offline["fk"]
        check(
            offline_fk["status"] == "scored",
            f"offline fk section must be scored from generated keys: {offline_fk}",
        )
        offline_rate = offline_fk["items"][0]
        check(offline_rate["source"] == "generated", f"fk source {offline_rate['source']}")
        check(
            abs(offline_rate["rate"] - 1.0) < 1e-9,
            f"generated child must reference generated parent keys, rate {offline_rate['rate']}",
        )
        check(not offline_rate["warn"], "offline FK rate 1.0 must not warn")

        # Foreign keys against the live key pool.
        db_report = load_json(os.path.join(out, "report_db.json"))
        check(fk_db_code == 0, f"--against-db exit {fk_db_code}")
        child = table_by_name(db_report, "m1_verify_child")
        fk = child["fk"]
        check(fk["status"] == "scored", f"child fk section not scored: {fk}")
        rates = fk["items"]
        check(len(rates) == 1, f"expected one FK edge, got {rates}")
        rate = rates[0]
        check(rate["source"] == "database", f"fk source {rate['source']}")
        check(rate["parent_table"] == "m1_verify_parent", "fk parent table")
        check(rate["keys"] > 0, "fk key count")
        check(abs(rate["rate"] - 1.0) < 1e-9, f"fk join rate {rate['rate']} (want 1.0)")
        check(not rate["warn"], "fk rate 1.0 must not warn")
        parent_table = table_by_name(db_report, "m1_verify_parent")
        check(
            parent_table["fk"]["status"] == "skipped",
            "parent table owns no FK edge and must be skipped",
        )

        # Missing baseline: skip, exit 0; --strict: error.
        nobase = load_json(os.path.join(out, "report_nobase.json"))
        check(len(nobase["tables"]) == len(TABLES), "no-baseline report tables")
        for table in nobase["tables"]:
            check(table["shapes"]["status"] == "skipped", f"{table['table']}: shapes")
            check("reason" in table["shapes"], f"{table['table']}: skip reason")
        check(len(nobase["tables"][0].get("pairs", {}) or {}) >= 1, "pairs section present")
        check(lenient_code == 0, f"missing baseline must exit 0, got {lenient_code}")
        check(strict_code != 0, f"--strict must exit non-zero, got {strict_code}")
        check(schema and len(schema) > 0, "schema must be non-empty")
    except Failure as error:
        print(f"FAIL: {error}", file=sys.stderr)
        return 1

    print(
        "M2 assertions: marginals, baselines, shapes, pairs, degradation, "
        "fk (generated + database), skip/strict OK"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
