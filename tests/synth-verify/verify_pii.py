#!/usr/bin/env python3
"""Assert the #71 PII behaviour on artifacts from run_m4_pii.sh.

Usage: verify_pii.py <output-dir>
"""

import csv
import json
import pathlib
import re
import sys

EMAIL_RE = re.compile(r"^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}$")
PHONE_RE = re.compile(r"^\+?[0-9][0-9 ()-]{6,}$")

TRAINING_EMAILS = {f"user{i}@corp-example.cn" for i in range(50)}
TRAINING_NAMES = {f"Person{i}" for i in range(30)}

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

    models = out / "models"
    model_text = (models / "pii_users.model.json").read_text()
    model = json.loads(model_text)
    profile = json.loads((models / "pii_users.profile.json").read_text())

    check(
        model["columns"]["email"].get("pii") == "email",
        "the trained model marks `email` as PII",
    )
    check(
        model["columns"]["full_name"].get("pii") == "name",
        "the recognizer marks `full_name` as a name",
    )
    check(
        all(email not in model_text for email in TRAINING_EMAILS),
        "no training email survives anywhere in the model JSON",
    )
    check(
        all(name not in model_text for name in TRAINING_NAMES),
        "no training name survives anywhere in the model JSON",
    )
    check(
        profile["columns"]["email"].get("top_values") is None,
        "the profile drops `email` top_values (leak surface)",
    )
    check(
        profile["columns"]["full_name"].get("top_values") is None,
        "the profile drops `full_name` top_values",
    )

    baseline_text = (models / "pii_users.report-baseline.json").read_text()
    check(
        all(email not in baseline_text for email in TRAINING_EMAILS),
        "no training email appears in the holdout baseline JSON",
    )
    check(
        all(name not in baseline_text for name in TRAINING_NAMES),
        "no training name appears in the holdout baseline JSON",
    )
    baseline = json.loads(baseline_text)
    check(
        "email" not in baseline["columns"] and "full_name" not in baseline["columns"],
        "PII columns are absent from the holdout baseline",
    )

    masked = read_rows(out / "masked" / "pii_users.csv")
    emails = [row["email"] for row in masked if row["email"]]
    phones = [row["phone"] for row in masked if row["phone"]]
    names = [row["full_name"] for row in masked if row["full_name"]]

    check(len(masked) == 200, f"generated 200 rows, got {len(masked)}")
    check(
        all(EMAIL_RE.match(value) for value in emails),
        "every generated email matches the email format",
    )
    check(
        all(PHONE_RE.match(value) for value in phones),
        "every generated phone matches the phone format",
    )
    check(
        not (set(emails) & TRAINING_EMAILS),
        "generated emails do not intersect the training values (AC1)",
    )
    check(
        not (set(phones) & {f"+86-139{i:08d}" for i in range(200)}),
        "generated phones do not intersect the training values",
    )
    check(
        not (set(names) & TRAINING_NAMES),
        "generated names do not intersect the training values",
    )

    check(
        (out / "masked" / "pii_users.csv").read_bytes()
        == (out / "masked2" / "pii_users.csv").read_bytes(),
        "same seed reproduces byte-identical output",
    )

    stable = read_rows(out / "stable" / "pii_users.csv")
    stable_emails = {row["email"] for row in stable if row["email"]}
    check(
        1 <= len(stable_emails) <= len(TRAINING_EMAILS),
        f"stable mapping reuses one fake per training value, got {len(stable_emails)}",
    )

    kept = read_rows(out / "keep" / "pii_users.csv")
    kept_emails = {row["email"] for row in kept if row["email"]}
    check(
        bool(kept_emails & TRAINING_EMAILS),
        "`sdtype: keep` keeps the trained value space (AC4)",
    )
    keep_model = json.loads((out / "models_keep" / "pii_users.model.json").read_text())
    check(
        keep_model["columns"]["email"].get("pii") is None,
        "`sdtype: keep` leaves the model column unmarked",
    )

    # Natural-key PII parent (email PRIMARY KEY) referenced by a child: training
    # must anonymize the key, keep it unique, and keep the child referencable.
    account_model_text = (out / "natural_models" / "pii_accounts.model.json").read_text()
    account_model = json.loads(account_model_text)
    check(
        account_model["columns"]["email"].get("pii") == "email",
        "a trained email primary key is marked PII",
    )
    check(
        all(f"owner{i}@corp-example.cn" not in account_model_text for i in range(40)),
        "the email primary key keeps no observed value in the model",
    )
    accounts = read_rows(out / "natural" / "pii_accounts.csv")
    account_emails = [row["email"] for row in accounts if row["email"]]
    check(
        len(set(account_emails)) == len(account_emails) == 40,
        f"the anonymized email key stays unique ({len(set(account_emails))}/{len(account_emails)})",
    )
    check(
        all(EMAIL_RE.match(value) for value in account_emails),
        "anonymized primary-key emails are format valid",
    )
    logins = read_rows(out / "natural" / "pii_logins.csv")
    known = set(account_emails)
    check(
        all(row["account_email"] in known for row in logins if row["account_email"]),
        "the child still references generated parent keys",
    )

    if failures:
        print(f"\n{len(failures)} PII check(s) failed")
        return 1
    print("\nPII assertions: recognition, leak surfaces, formats, zero-reproduction, determinism, stable mapping, keep OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
