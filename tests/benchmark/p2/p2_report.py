#!/usr/bin/env python3
"""Evaluate and render the P2 multi-table benchmark gates."""

import argparse
import json
import math
import os
import subprocess
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parent
TABLES = ("customer", "rental", "payment")
EXPECTED_ROWS = {"customer": 599, "rental": 16044, "payment": 16049}
NUMERIC_TYPES = {
    "smallint", "integer", "bigint", "decimal", "numeric", "real",
    "double precision", "smallserial", "serial", "bigserial",
}

# All database assertions and metric extracts are defined here.
ROW_COUNTS_SQL = "SELECT '{table}', count(*) FROM staging.{table};"
REAL_COUNTS_SQL = "SELECT count(*) FROM {schema}.{table};"
ORPHAN_SQL = {
    "payment.customer_id->customer.customer_id": """SELECT count(*) FROM staging.payment p LEFT JOIN staging.customer c ON p.customer_id = c.customer_id WHERE p.customer_id IS NOT NULL AND c.customer_id IS NULL;""",
    "payment.rental_id->rental.rental_id": """SELECT count(*) FROM staging.payment p LEFT JOIN staging.rental r ON p.rental_id = r.rental_id WHERE p.rental_id IS NOT NULL AND r.rental_id IS NULL;""",
    "rental.customer_id->customer.customer_id": """SELECT count(*) FROM staging.rental r LEFT JOIN staging.customer c ON r.customer_id = c.customer_id WHERE r.customer_id IS NOT NULL AND c.customer_id IS NULL;""",
}
FANOUT_SQL = "SELECT count(*) FROM {schema}.rental GROUP BY customer_id ORDER BY customer_id;"
COLUMNS_SQL = """SELECT table_name, column_name, data_type FROM information_schema.columns WHERE table_schema = '{schema}' AND table_name IN ('customer','rental','payment') ORDER BY table_name, ordinal_position;"""
COLUMN_SQL = 'SELECT "{column}" FROM "{schema}"."{table}" WHERE "{column}" IS NOT NULL;'
STORE_SPEND_SQL = """SELECT c.store_id, avg(p.amount) FROM {schema}.payment p JOIN {schema}.customer c ON p.customer_id = c.customer_id GROUP BY c.customer_id, c.store_id;"""
AMOUNT_LEVELS_SQL = "SELECT DISTINCT amount FROM {schema}.payment WHERE amount IS NOT NULL;"
AMOUNT_VALUES_SQL = "SELECT amount FROM staging.payment WHERE amount IS NOT NULL;"
TABLE_SQL = 'SELECT * FROM "{schema}"."{table}";'


def gsql(sql: str):
    result = subprocess.run(
        ["docker", "exec", "pagila", "gsql-pagila", "-t", "-A", "-F", "\t", "-c", sql],
        check=True,
        capture_output=True,
        text=True,
    )
    return [line.split("\t") for line in result.stdout.splitlines() if line.strip()]


def scalar(sql: str) -> int:
    rows = gsql(sql)
    if len(rows) != 1 or len(rows[0]) != 1:
        raise RuntimeError(f"expected one scalar row, got {rows!r}")
    return int(rows[0][0])


def floats(sql: str):
    return [float(row[0]) for row in gsql(sql)]


def generated_rows(path: Path) -> int:
    with path.open(encoding="utf-8") as stream:
        return sum(1 for line in stream if line.lstrip().startswith("INSERT INTO"))


def ks_statistic(left, right):
    """Use scipy when installed; retain a DB-free stdlib path for --self-test."""
    try:
        from scipy.stats import ks_2samp

        return float(ks_2samp(left, right).statistic)
    except ImportError:
        if not left or not right:
            raise ValueError("KS samples must be non-empty")
        left_sorted = sorted(left)
        right_sorted = sorted(right)
        points = sorted(set(left_sorted) | set(right_sorted))
        return max(
            abs(
                sum(value <= point for value in left_sorted) / len(left_sorted)
                - sum(value <= point for value in right_sorted) / len(right_sorted)
            )
            for point in points
        )


def collect_gate_inputs(generated_dir: Path, source_schema: str, load_succeeded: bool):
    db_counts = {
        table: int(gsql(ROW_COUNTS_SQL.format(table=table))[0][1]) for table in TABLES
    }
    expected_counts = {
        table: scalar(REAL_COUNTS_SQL.format(schema=source_schema, table=table))
        for table in TABLES
    }
    file_counts = {
        table: generated_rows(generated_dir / f"{table}.sql") for table in TABLES
    }
    return {
        "load_succeeded": load_succeeded,
        "expected_counts": expected_counts,
        "db_counts": db_counts,
        "file_counts": file_counts,
        "orphans": {name: scalar(sql) for name, sql in ORPHAN_SQL.items()},
        "real_fanout": floats(FANOUT_SQL.format(schema=source_schema)),
        "synthetic_fanout": floats(FANOUT_SQL.format(schema="staging")),
        "amount_on_grid_ratio": amount_on_grid(source_schema),
    }


def evaluate_gates(inputs):
    count_details = {}
    p20 = bool(inputs["load_succeeded"])
    expected_counts = inputs.get("expected_counts", EXPECTED_ROWS)
    for table, expected in expected_counts.items():
        file_count = inputs["file_counts"].get(table)
        db_count = inputs["db_counts"].get(table)
        passed = file_count == expected and db_count == expected and db_count == file_count
        count_details[table] = {
            "expected": expected, "generated_file": file_count, "staging": db_count, "passed": passed
        }
        p20 = p20 and passed

    orphan_passed = all(value == 0 for value in inputs["orphans"].values())
    ks = ks_statistic(inputs["real_fanout"], inputs["synthetic_fanout"])
    ratio = inputs["amount_on_grid_ratio"]
    ongrid_passed = ratio >= 0.95
    # P2-2 is recorded, not gated (#55 review adjudication): uniform/zipf FK
    # pools are not empirical fan-out, so the KS value is informational.
    return {
        "passed": bool(p20 and orphan_passed and ongrid_passed),
        "P2-0": {"passed": bool(p20), "load_succeeded": bool(inputs["load_succeeded"]), "tables": count_details},
        "P2-1": {"passed": orphan_passed, "orphan_counts": inputs["orphans"]},
        "on_grid": {"passed": bool(ongrid_passed), "ratio": ratio, "threshold": 0.95},
        "P2-2": {"gate": False, "ks_statistic": ks, "within_threshold": bool(ks < 0.15), "threshold": 0.15},
    }


def total_variation(real, synthetic):
    if not real or not synthetic:
        return None
    real_counts = Counter(real)
    synthetic_counts = Counter(synthetic)
    levels = set(real_counts) | set(synthetic_counts)
    return 0.5 * sum(
        abs(
            real_counts[level] / len(real) - synthetic_counts[level] / len(synthetic)
        )
        for level in levels
    )


def marginal_metrics(source_schema: str):
    columns = gsql(COLUMNS_SQL.format(schema=source_schema))
    metrics = {}
    for table, column, data_type in columns:
        real = [row[0] for row in gsql(COLUMN_SQL.format(schema=source_schema, table=table, column=column))]
        synthetic = [row[0] for row in gsql(COLUMN_SQL.format(schema="staging", table=table, column=column))]
        key = f"{table}.{column}"
        if data_type in NUMERIC_TYPES:
            value = ks_statistic(list(map(float, real)), list(map(float, synthetic))) if real and synthetic else None
            metrics[key] = {"kind": "ks", "value": value}
        else:
            metrics[key] = {"kind": "tv", "value": total_variation(real, synthetic)}
    return metrics


def pearson(left, right):
    if len(left) != len(right) or len(left) < 2:
        return None
    left_mean = sum(left) / len(left)
    right_mean = sum(right) / len(right)
    numerator = sum((x - left_mean) * (y - right_mean) for x, y in zip(left, right))
    denominator = math.sqrt(
        sum((x - left_mean) ** 2 for x in left)
        * sum((y - right_mean) ** 2 for y in right)
    )
    value = numerator / denominator if denominator else float("nan")
    return None if math.isnan(value) else value


def store_spend_pearson(schema):
    """Pearson(customer.store_id, per-customer average payment.amount):
    a business-meaningful 1-hop signal, unlike surrogate-key co-monotonicity."""
    pairs = gsql(STORE_SPEND_SQL.format(schema=schema))
    if len(pairs) < 2:
        return None
    return pearson([float(row[0]) for row in pairs], [float(row[1]) for row in pairs])


def amount_on_grid(source_schema: str):
    observed = set(floats(AMOUNT_LEVELS_SQL.format(schema=source_schema)))
    generated = floats(AMOUNT_VALUES_SQL)
    return sum(value in observed for value in generated) / len(generated) if generated else 0.0


def optional_sdv_reference(source_schema: str):
    if os.environ.get("P2_SDV_REF") != "1":
        return {"enabled": False}
    try:
        import pandas as pd
        from sdv.metadata import Metadata
        from sdv.single_table import GaussianCopulaSynthesizer
    except ImportError as error:
        return {"enabled": True, "available": False, "error": str(error)}

    scores = {}
    column_types = gsql(COLUMNS_SQL.format(schema=source_schema))
    for table in TABLES:
        rows = gsql(TABLE_SQL.format(schema=source_schema, table=table))
        columns = [column for found_table, column, _ in column_types if found_table == table]
        types = {column: data_type for found_table, column, data_type in column_types if found_table == table}
        real = pd.DataFrame(rows, columns=columns)
        for column in columns:
            if types[column] in NUMERIC_TYPES:
                real[column] = pd.to_numeric(real[column], errors="coerce")
        metadata = Metadata.detect_from_dataframe(real)
        model = GaussianCopulaSynthesizer(metadata)
        model.fit(real)
        sampled = model.sample(num_rows=EXPECTED_ROWS[table])
        scores[table] = {}
        for column in columns:
            if types[column] in NUMERIC_TYPES:
                real_values = pd.to_numeric(real[column], errors="coerce").dropna().tolist()
                sampled_values = pd.to_numeric(sampled[column], errors="coerce").dropna().tolist()
                value = ks_statistic(real_values, sampled_values) if real_values and sampled_values else None
                scores[table][column] = {"kind": "ks", "value": value}
            else:
                scores[table][column] = {"kind": "tv", "value": total_variation(real[column], sampled[column])}
    return {"enabled": True, "available": True, "marginals": scores}


def self_test_inputs():
    return {
        "load_succeeded": True,
        "db_counts": dict(EXPECTED_ROWS),
        "file_counts": dict(EXPECTED_ROWS),
        "orphans": {
            "payment.customer_id->customer.customer_id": 1,
            "payment.rental_id->rental.rental_id": 0,
            "rental.customer_id->customer.customer_id": 0,
        },
        "real_fanout": [0.0] * 100,
        "synthetic_fanout": [1.0] * 100,
        "amount_on_grid_ratio": 0.9,
    }


def write_report(report, output_json: Path, output_md: Path):
    output_json.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    lines = ["# P2 Multi-table FK Benchmark", "", f"Overall: **{'PASS' if report['passed'] else 'FAIL'}**", ""]
    for gate in ("P2-0", "P2-1", "on_grid"):
        lines.extend([f"## {gate}: {'PASS' if report[gate]['passed'] else 'FAIL'}", "", "```json", json.dumps(report[gate], indent=2, sort_keys=True), "```", ""])
    lines.extend([
        "## P2-2 fan-out KS (record only, not gated)", "", "```json", json.dumps(report.get("P2-2", {}), indent=2, sort_keys=True), "```", "",
        "## P2-3 (record only)", "", "```json", json.dumps(report.get("P2-3", {}), indent=2, sort_keys=True), "```", "",
        "## P2-4 (record only)", "", "```json", json.dumps(report.get("P2-4", {}), indent=2, sort_keys=True), "```", "",
    ])
    output_md.write_text("\n".join(lines), encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--generated-dir", type=Path)
    parser.add_argument("--schema", default=os.environ.get("P2_SCHEMA", "public"))
    parser.add_argument("--attempt", default="uniform")
    parser.add_argument("--load-status", type=int, default=0)
    parser.add_argument("--attempts-file", type=Path, default=ROOT / "p2_attempts.json")
    parser.add_argument("--output-json", type=Path, default=ROOT / "p2_report.json")
    parser.add_argument("--output-md", type=Path, default=ROOT / "REPORT.md")
    args = parser.parse_args()

    if args.self_test:
        report = evaluate_gates(self_test_inputs())
        print(json.dumps(report, indent=2, sort_keys=True))
        return 0 if report["passed"] else 1
    if not args.generated_dir:
        parser.error("--generated-dir is required outside --self-test")

    report = evaluate_gates(
        collect_gate_inputs(args.generated_dir, args.schema, args.load_status == 0)
    )
    report["attempt"] = args.attempt
    report["P2-3"] = {
        "gate": False,
        "marginals": marginal_metrics(args.schema),
        "sdv_gc_reference": optional_sdv_reference(args.schema),
    }
    real_corr = store_spend_pearson(args.schema)
    synthetic_corr = store_spend_pearson("staging")
    report["P2-4"] = {
        "gate": False,
        "store_id_vs_per_customer_avg_amount_pearson": {
            "real": real_corr,
            "synthetic": synthetic_corr,
            "absolute_error": abs(real_corr - synthetic_corr) if real_corr is not None and synthetic_corr is not None else None,
        },
    }

    attempts = []
    if args.attempts_file.exists():
        attempts = json.loads(args.attempts_file.read_text(encoding="utf-8"))
    attempts = [item for item in attempts if item.get("attempt") != args.attempt]
    # Snapshot, not a live reference: report["attempts"] below would otherwise
    # make `report` contain itself and break json.dumps in write_report.
    attempts.append(json.loads(json.dumps(report)))
    args.attempts_file.write_text(json.dumps(attempts, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    report["attempts"] = attempts
    write_report(report, args.output_json, args.output_md)
    for gate in ("P2-0", "P2-1", "on_grid"):
        print(f"{gate} {'PASS' if report[gate]['passed'] else 'FAIL'}")
    print(f"P2-2 recorded: KS {report['P2-2']['ks_statistic']:.4f} (not gated)")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
