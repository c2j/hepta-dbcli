#!/usr/bin/env bash
# M1 synth end-to-end verification (issues #62-#65) against a real MySQL.
#
#   HEPTA_DBCLI_TEST_URL=mysql://user:pass@127.0.0.1:3306/testdb \
#     bash tests/synth-verify/run_m1.sh
#
# What it does: loads tests/synth-verify/fixture_mysql.sql into the target
# database, runs `synth train` / `rules-draft` / `generate` (csv, sql and sql
# with --no-schema-qualifier) and then asserts the artifacts with
# tests/synth-verify/verify_m1.py.
#
# Environment:
#   HEPTA_DBCLI_TEST_URL   required; the database to create the fixture in
#   HEPTA_BIN              binary under test (default target/debug/hepta_dbcli)
#   SYNTH_E2E_MYSQL_CMD    command that reads SQL on stdin and applies it
#                          (default: the `mysql` client built from the URL)
#                          CI passes e.g. "docker exec -i <cid> mysql -uroot -ptestpass -D testdb"
#   SYNTH_E2E_OUT          output directory (default: a fresh mktemp -d)
#   SYNTH_E2E_KEEP         set to 1 to keep the output directory on failure
#
# The fixture uses only the target database's own schema; the schema qualifier
# assertions use the database name from HEPTA_DBCLI_TEST_URL.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${ROOT}/../.." && pwd)"

URL="${HEPTA_DBCLI_TEST_URL:?set HEPTA_DBCLI_TEST_URL to a mysql:// URL}"
HEPTA_BIN="${HEPTA_BIN:-${REPO_ROOT}/target/debug/hepta_dbcli}"
OUT="${SYNTH_E2E_OUT:-$(mktemp -d "${TMPDIR:-/tmp}/synth-m1-verify.XXXXXX")}"

# mysql://user:pass@host:port/db -> the four parts we need.
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
fail=0
run() {
    echo "+ $*"
    "$@" || fail=1
    if [[ "${fail}" -ne 0 ]]; then
        echo "command failed, stopping" >&2
        exit 1
    fi
}

echo "== synth train =="
# `rules_m1_keep.yaml` pins `m1_verify_parent.email` to `sdtype: keep`: since
# issue #71 an `email` column is anonymized by default, which would replace the
# trained value space this suite asserts on.
run "${HEPTA_BIN}" synth train --tables "${TABLES}" --schema "${SCHEMA}" \
    --output "${OUT}" --sample 10000 --categorical-top-k full \
    --rules "${ROOT}/rules_m1_keep.yaml"

echo "== synth rules-draft =="
run "${HEPTA_BIN}" synth rules-draft --tables "${TABLES}" --schema "${SCHEMA}" \
    --models "${OUT}" --output "${OUT}/rules.yaml"

echo "== synth generate (csv) =="
run "${HEPTA_BIN}" synth generate --models "${OUT}" --rules "${OUT}/rules.yaml" \
    --output "${OUT}/csv" --rows 1000 --seed 42 --format csv

echo "== synth generate (csv, same seed again, determinism) =="
run "${HEPTA_BIN}" synth generate --models "${OUT}" --rules "${OUT}/rules.yaml" \
    --output "${OUT}/csv-again" --rows 1000 --seed 42 --format csv

echo "== synth generate (sql, schema qualified) =="
run "${HEPTA_BIN}" synth generate --models "${OUT}" --rules "${OUT}/rules.yaml" \
    --output "${OUT}/sql" --rows 1000 --seed 42 --format sql

echo "== synth generate (sql, --no-schema-qualifier) =="
run "${HEPTA_BIN}" synth generate --models "${OUT}" --rules "${OUT}/rules.yaml" \
    --output "${OUT}/sql-legacy" --rows 1000 --seed 42 --format sql --no-schema-qualifier

echo
echo "== verifying artifacts in ${OUT} =="
if python3 "${ROOT}/verify_m1.py" "${OUT}" "${SCHEMA}"; then
    echo "M1 end-to-end verification PASSED"
    if [[ "${SYNTH_E2E_KEEP:-0}" != "1" ]]; then
        rm -rf "${OUT}"
    fi
    exit 0
fi

echo "M1 end-to-end verification FAILED (artifacts kept in ${OUT})" >&2
exit 1
