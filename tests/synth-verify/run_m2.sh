#!/usr/bin/env bash
# M2 synth end-to-end verification (issues #66 ECDF auto-selection, #67 report)
# against a real MySQL.
#
#   HEPTA_DBCLI_TEST_URL=mysql://user:pass@127.0.0.1:3306/testdb \
#     bash tests/synth-verify/run_m2.sh
#
# What it does: loads tests/synth-verify/fixture_mysql.sql into the target
# database, runs `synth train` (which now records a holdout report baseline),
# `rules-draft`, `generate` (jsonl, twice for determinism) and `synth report`
# offline, against the live database, on a degraded copy, and without a
# baseline (honest skip + --strict + --min-score). Assertions live in
# verify_m2.py.
#
# Environment:
#   HEPTA_DBCLI_TEST_URL   required; the database to create the fixture in
#   HEPTA_BIN              binary under test (default target/debug/hepta_dbcli)
#   SYNTH_E2E_MYSQL_CMD    command that reads SQL on stdin and applies it
#   SYNTH_E2E_OUT          output directory (default: a fresh mktemp -d)
#   SYNTH_E2E_KEEP         set to 1 to keep the output directory on failure

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${ROOT}/../.." && pwd)"

URL="${HEPTA_DBCLI_TEST_URL:?set HEPTA_DBCLI_TEST_URL to a mysql:// URL}"
HEPTA_BIN="${HEPTA_BIN:-${REPO_ROOT}/target/debug/hepta_dbcli}"
OUT="${SYNTH_E2E_OUT:-$(mktemp -d "${TMPDIR:-/tmp}/synth-m2-verify.XXXXXX")}"

# mysql://user:pass@host:port/db -> the parts we need.
proto="${URL%%://*}"
if [[ "${proto}" != "mysql" ]]; then
    echo "this verifier expects a mysql:// URL, got '${proto}://'" >&2
    exit 2
fi
rest="${URL#*://}"
creds="${rest%%@*}"
hostpart="${rest#*@}"
db_and_query="${hostpart#*/}"
SCHEMA="${db_and_query%%\?*}"
DBUSER="${creds%%:*}"
DBPASS="${creds#*:}"
hostport="${hostpart%%/*}"
DBHOST="${hostport%%:*}"
if [[ "${hostport}" == *:* ]]; then
    DBPORT="${hostport##*:}"
else
    DBPORT=3306
fi
if [[ -z "${SCHEMA}" || "${SCHEMA}" == "${hostpart}" ]]; then
    echo "HEPTA_DBCLI_TEST_URL must include a database name (schema)" >&2
    exit 2
fi

if [[ ! -x "${HEPTA_BIN}" ]]; then
    echo "== building ${HEPTA_BIN} =="
    (cd "${REPO_ROOT}" && cargo build)
fi

echo "== loading fixture into ${DBHOST}:${DBPORT}/${SCHEMA} =="
if [[ -n "${SYNTH_E2E_MYSQL_CMD:-}" ]]; then
    # shellcheck disable=SC2086 # deliberate word splitting: the command is a list
    ${SYNTH_E2E_MYSQL_CMD} < "${ROOT}/fixture_mysql.sql"
else
    if ! command -v mysql >/dev/null 2>&1; then
        echo "no 'mysql' client found; set SYNTH_E2E_MYSQL_CMD to load the fixture" >&2
        exit 2
    fi
    MYSQL_PWD="${DBPASS}" mysql -h "${DBHOST}" -P "${DBPORT}" -u "${DBUSER}" \
        "${SCHEMA}" < "${ROOT}/fixture_mysql.sql"
fi

# The env-var connection path avoids writing a config file (and therefore
# avoids the plaintext-password migration into the OS keychain).
export HEPTA_DBCLI_URL="${URL}"

TABLES="m1_verify_parent,m1_verify_child"
# Measured on this fixture: ~0.74. The gate is margin, not a fidelity claim.
GOOD_MIN_SCORE=0.7
DEGRADED_MIN_SCORE=0.95

run() {
    echo "+ $*"
    "$@" || {
        echo "command failed, stopping" >&2
        exit 1
    }
}

mkdir -p "${OUT}/models" "${OUT}/nobase" "${OUT}/degraded"

echo "== synth train (writes models + report baselines) =="
run "${HEPTA_BIN}" synth train --tables "${TABLES}" --schema "${SCHEMA}" \
    --output "${OUT}/models" --sample 2000 --categorical-top-k full

echo "== synth rules-draft =="
run "${HEPTA_BIN}" synth rules-draft --tables "${TABLES}" --schema "${SCHEMA}" \
    --models "${OUT}/models" --output "${OUT}/rules.yaml"

echo "== synth generate (jsonl, seed 7) =="
run "${HEPTA_BIN}" synth generate --models "${OUT}/models" --rules "${OUT}/rules.yaml" \
    --output "${OUT}/data" --rows 2000 --seed 7 --format jsonl

echo "== synth generate (jsonl, seed 7 again) =="
run "${HEPTA_BIN}" synth generate --models "${OUT}/models" --rules "${OUT}/rules.yaml" \
    --output "${OUT}/data2" --rows 2000 --seed 7 --format jsonl

echo "== sibling model dir without a baseline (for --strict) =="
cp "${OUT}/models/"*.model.json "${OUT}/nobase/"

echo "== degraded copy: shift cjje far outside the trained range =="
python3 - "${OUT}" <<'PY'
import json
import sys

out = sys.argv[1]
with open(f"{out}/data/m1_verify_parent.jsonl") as src, open(
    f"{out}/degraded/m1_verify_parent.jsonl", "w"
) as dst:
    for line in src:
        row = json.loads(line)
        row["cjje"] = float(row["cjje"]) + 10000.0
        dst.write(json.dumps(row) + "\n")
with open(f"{out}/data/m1_verify_child.jsonl") as src, open(
    f"{out}/degraded/m1_verify_child.jsonl", "w"
) as dst:
    dst.write(src.read())
PY

echo "== synth report (offline fk, --min-score ${GOOD_MIN_SCORE}) =="
set +e
"${HEPTA_BIN}" synth report --models "${OUT}/models" --data "${OUT}/data" \
    --rules "${OUT}/rules.yaml" --output "${OUT}/report.json" \
    --min-score "${GOOD_MIN_SCORE}"
good_code=$?

echo "== synth report again (determinism) =="
"${HEPTA_BIN}" synth report --models "${OUT}/models" --data "${OUT}/data" \
    --rules "${OUT}/rules.yaml" --output "${OUT}/report2.json" >/dev/null
second_code=$?

echo "== synth report (degraded, --min-score ${DEGRADED_MIN_SCORE}) =="
"${HEPTA_BIN}" synth report --models "${OUT}/models" --data "${OUT}/degraded" \
    --rules "${OUT}/rules.yaml" --output "${OUT}/report_degraded.json" \
    --min-score "${DEGRADED_MIN_SCORE}" >/dev/null
degraded_code=$?

echo "== synth report (--against-db, real key pools) =="
"${HEPTA_BIN}" synth report --models "${OUT}/models" --data "${OUT}/data" \
    --rules "${OUT}/rules.yaml" --against-db --output "${OUT}/report_db.json" >/dev/null
fk_db_code=$?

echo "== synth report (no baseline, honest skip) =="
"${HEPTA_BIN}" synth report --models "${OUT}/nobase" --data "${OUT}/data" \
    --output "${OUT}/report_nobase.json" >/dev/null 2>&1
lenient_code=$?

echo "== synth report (no baseline, --strict) =="
"${HEPTA_BIN}" synth report --models "${OUT}/nobase" --data "${OUT}/data" \
    --strict >/dev/null 2>&1
strict_code=$?

echo "== synth report (no baseline, --min-score: every table must be scored) =="
"${HEPTA_BIN}" synth report --models "${OUT}/nobase" --data "${OUT}/data" \
    --min-score 0.5 >/dev/null 2>&1
unscored_gate_code=$?
set -e

echo
echo "== verifying artifacts in ${OUT} =="
if python3 "${ROOT}/verify_m2.py" "${OUT}" "${SCHEMA}" \
    "${good_code}" "${second_code}" "${degraded_code}" "${fk_db_code}" \
    "${lenient_code}" "${strict_code}" "${unscored_gate_code}"; then
    echo "M2 end-to-end verification PASSED"
    if [[ "${SYNTH_E2E_KEEP:-0}" != "1" ]]; then
        rm -rf "${OUT}"
    fi
    exit 0
fi

echo "M2 end-to-end verification FAILED (artifacts kept in ${OUT})" >&2
exit 1
