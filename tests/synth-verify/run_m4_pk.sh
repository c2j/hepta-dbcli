#!/usr/bin/env bash
# Issue #82 acceptance: generated primary keys stay unique and loadable.
#
#   HEPTA_DBCLI_TEST_URL=mysql://user:pass@127.0.0.1:3306/testdb \
#     bash tests/synth-verify/run_m4_pk.sh
#
# What it does:
#   1. loads tests/synth-verify/fixture_pk.sql;
#   2. trains the three tables and drafts per-table rules;
#   3. `pk_verify_text` (3 non-numeric keys) must FAIL to generate 10 rows and
#      name the column and row count;
#   4. `pk_verify_single` (12 integer keys) and `pk_verify_composite` (400
#      tuples) must both generate 200 rows and round-trip SQL export -> MySQL
#      load with 200 distinct single keys and 200 distinct composite tuples.
#
# Environment:
#   HEPTA_DBCLI_TEST_URL   required
#   HEPTA_BIN              binary under test (default target/debug/hepta_dbcli)
#   SYNTH_E2E_MYSQL_CMD    command that reads SQL on stdin (e.g. CI's
#                          "docker exec -i <cid> mysql -uroot -ptestpass -D testdb")
#   SYNTH_E2E_KEEP         set to 1 to keep the output directory

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${ROOT}/../.." && pwd)"

URL="${HEPTA_DBCLI_TEST_URL:?set HEPTA_DBCLI_TEST_URL to a mysql:// URL}"
HEPTA_BIN="${HEPTA_BIN:-${REPO_ROOT}/target/debug/hepta_dbcli}"
OUT="${SYNTH_E2E_OUT:-$(mktemp -d "${TMPDIR:-/tmp}/synth-pk-verify.XXXXXX")}"

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

# Feed SQL on stdin through the configured client, mirroring run_m1.sh.
mysql_stdin() {
    if [[ -n "${SYNTH_E2E_MYSQL_CMD:-}" ]]; then
        # shellcheck disable=SC2086 # deliberate word splitting: the command is a list
        ${SYNTH_E2E_MYSQL_CMD}
    else
        if ! command -v mysql >/dev/null 2>&1; then
            echo "no 'mysql' client found; set SYNTH_E2E_MYSQL_CMD to load SQL" >&2
            exit 2
        fi
        MYSQL_PWD="${DBPASS}" mysql -h "${DBHOST}" -P "${DBPORT}" -u "${DBUSER}" "${SCHEMA}"
    fi
}

# Same, but requesting tab-separated bare values for assertions.
mysql_query() {
    if [[ -n "${SYNTH_E2E_MYSQL_CMD:-}" ]]; then
        # shellcheck disable=SC2086 # deliberate word splitting: the command is a list
        ${SYNTH_E2E_MYSQL_CMD} -N -B
    else
        MYSQL_PWD="${DBPASS}" mysql -h "${DBHOST}" -P "${DBPORT}" -u "${DBUSER}" \
            -N -B "${SCHEMA}"
    fi
}

echo "== loading fixture =="
mysql_stdin < "${ROOT}/fixture_pk.sql"

export HEPTA_DBCLI_URL="${URL}"

TABLES="pk_verify_single,pk_verify_composite,pk_verify_text"

echo "== synth train =="
"${HEPTA_BIN}" synth train --tables "${TABLES}" --schema "${SCHEMA}" \
    --output "${OUT}" --sample 10000 --categorical-top-k full

# Rules are drafted per table: the fixtures are unrelated, and a combined draft
# invents implicit foreign keys between them (which then look cyclic).
for table in pk_verify_single pk_verify_composite pk_verify_text; do
    "${HEPTA_BIN}" synth rules-draft --tables "${table}" --schema "${SCHEMA}" \
        --models "${OUT}" --output "${OUT}/rules_${table}.yaml"
done

echo "== pk_verify_text: 10 rows from 3 non-numeric keys must fail =="
if "${HEPTA_BIN}" synth generate --models "${OUT}" --rules "${OUT}/rules_pk_verify_text.yaml" \
    --output "${OUT}/over_text" --rows 10 --seed 7 --format sql \
    2> "${OUT}/over_text.err"; then
    echo "generate --rows 10 unexpectedly succeeded; duplicate primary keys would be exported" >&2
    exit 1
fi
if ! grep -q "primary key column 'pk_verify_text.code'" "${OUT}/over_text.err"; then
    echo "failure did not name the primary key: $(cat "${OUT}/over_text.err")" >&2
    exit 1
fi
if ! grep -q "10 unique values" "${OUT}/over_text.err"; then
    echo "failure did not name the row count: $(cat "${OUT}/over_text.err")" >&2
    exit 1
fi

# Scale-up: 200 rows from 12 integer keys (extrapolated) and from 400 tuples.
for table in pk_verify_single pk_verify_composite; do
    echo "== synth generate ${table} --rows 200 =="
    "${HEPTA_BIN}" synth generate --models "${OUT}" --rules "${OUT}/rules_${table}.yaml" \
        --output "${OUT}/sql_${table}" --rows 200 --seed 7 --format sql
done

echo "== loading generated SQL back into MySQL =="
mysql_stdin <<SQL
TRUNCATE TABLE pk_verify_single;
TRUNCATE TABLE pk_verify_composite;
SQL
cat "${OUT}/sql_pk_verify_single/pk_verify_single.sql" \
    "${OUT}/sql_pk_verify_composite/pk_verify_composite.sql" | mysql_stdin

read_count() {
    local query="$1"
    echo "${query}" | mysql_query
}

single="$(read_count "SELECT COUNT(*), COUNT(DISTINCT id) FROM pk_verify_single;")"
composite="$(read_count "SELECT COUNT(*), COUNT(DISTINCT region, slot) FROM pk_verify_composite;")"

echo "pk_verify_single    (rows, distinct id):    ${single}"
echo "pk_verify_composite (rows, distinct tuple): ${composite}"

fail=0
[[ "${single}" == $'200\t200' ]] || { echo "single pk mismatch: ${single}" >&2; fail=1; }
[[ "${composite}" == $'200\t200' ]] || { echo "composite pk mismatch: ${composite}" >&2; fail=1; }

if [[ "${fail}" -eq 0 ]]; then
    echo "M4 pk-uniqueness verification PASSED"
    if [[ "${SYNTH_E2E_KEEP:-0}" != "1" ]]; then
        rm -rf "${OUT}"
    fi
    exit 0
fi

echo "M4 pk-uniqueness verification FAILED (artifacts kept in ${OUT})" >&2
exit 1
