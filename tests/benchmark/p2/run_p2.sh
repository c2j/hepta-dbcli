#!/bin/bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${ROOT}/../../.." && pwd)"
HEPTA_BIN="${HEPTA_BIN:-${REPO_ROOT}/target/release/hepta_dbcli}"
CONFIG="${REPO_ROOT}/tests/benchmark/pagila.toml"
VENV="${REPO_ROOT}/tests/benchmark/.venv-p2"
PY="${VENV}/bin/python"
MODELS="${ROOT}/models"
GENERATED="${ROOT}/generated"
RULES="${ROOT}/p2_rules.yaml"
DDL="${ROOT}/staging.sql"
SCHEMA="${P2_SCHEMA:-public}"
TABLES="customer,rental,payment"

OGAGILA_DIR="${OGAGILA_DIR:?set OGAGILA_DIR to the ogagila checkout}"
export HEPTA_DBCLI_PASSWORD="${HEPTA_DBCLI_PASSWORD:-Enmo@123}"

if [[ ! -x "${HEPTA_BIN}" ]]; then
    (cd "${REPO_ROOT}" && cargo build --release)
fi

(cd "${OGAGILA_DIR}" && docker-compose up -d)
ready=0
for _ in {1..30}; do
    if docker exec pagila gsql-pagila -c "SELECT 1;" >/dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 2
done
if [[ "${ready}" -ne 1 ]]; then
    echo "pagila did not become ready after 60 seconds" >&2
    exit 1
fi

if [[ ! -d "${VENV}" ]]; then
    python3 -m venv "${VENV}"
    "${VENV}/bin/pip" install -U pip
    "${VENV}/bin/pip" install -r "${REPO_ROOT}/tests/benchmark/requirements.txt"
fi

rm -rf "${MODELS}" "${GENERATED}"
rm -f "${RULES}" "${DDL}" "${ROOT}/p2_attempts.json" "${ROOT}/p2_report.json" "${ROOT}/REPORT.md"
mkdir -p "${MODELS}" "${GENERATED}"

"${HEPTA_BIN}" --config "${CONFIG}" synth train --name pagila --tables "${TABLES}" --schema "${SCHEMA}" --output "${MODELS}" --sample 20000
"${HEPTA_BIN}" --config "${CONFIG}" synth rules-draft --name pagila --tables "${TABLES}" --schema "${SCHEMA}" --models "${MODELS}" --output "${RULES}"
real_count() {
    docker exec pagila gsql-pagila -t -A -c "SELECT count(*) FROM ${SCHEMA}.$1;"
}

ROWS_ARGS=()
for table in customer rental payment; do
    ROWS_ARGS+=("--rows" "${table}=$(real_count "${table}")")
done
"${PY}" "${ROOT}/p2_patch_rules.py" "${RULES}" "${ROWS_ARGS[@]}"

load_generated() {
    rm -rf "${GENERATED}"
    mkdir -p "${GENERATED}"
    "${HEPTA_BIN}" synth generate --models "${MODELS}" --rules "${RULES}" --output "${GENERATED}" --seed 42 --format sql || return $?
    "${PY}" "${ROOT}/p2_make_staging.py" --schema "${SCHEMA}" --output "${DDL}" || return $?
    for table in customer rental payment; do
        # gsql executes stdin scripts; the generated INSERTs carry no schema
        # qualifier, so pin the search_path inside the same stdin stream.
        { printf 'SET search_path TO staging;\n'; cat "${GENERATED}/${table}.sql"; } \
            | docker exec -i pagila gsql-pagila || return $?
    done
}

load_status=0
load_generated || load_status=$?
uniform_exit=0
"${PY}" "${ROOT}/p2_report.py" --generated-dir "${GENERATED}" --schema "${SCHEMA}" --load-status "${load_status}" --attempt uniform || uniform_exit=$?

# Zipf retry is data enrichment only: rerun when every gate is green but the
# recorded KS is high. Overall = P2-0 ∧ P2-1 ∧ on_grid; P2-2 never gates it.
gates_green_ks_high() {
    "${PY}" -c 'import json,sys; r=json.load(open(sys.argv[1])); sys.exit(0 if r["P2-0"]["passed"] and r["P2-1"]["passed"] and r["on_grid"]["passed"] and r["P2-2"]["ks_statistic"] >= 0.15 else 1)' "${ROOT}/p2_report.json"
}

if gates_green_ks_high; then
    echo "All gates green; P2-2 KS above threshold (record only); retrying with Zipf for data enrichment"
    "${PY}" "${ROOT}/p2_patch_rules.py" "${RULES}" "${ROWS_ARGS[@]}" --zipf
    load_status=0
    load_generated || load_status=$?
    "${PY}" "${ROOT}/p2_report.py" --generated-dir "${GENERATED}" --schema "${SCHEMA}" --load-status "${load_status}" --attempt zipf
    exit $?
fi

exit "${uniform_exit}"
