#!/usr/bin/env bash
# Issue #72 acceptance: learned child cardinality (HMA-lite).
#
#   HEPTA_DBCLI_TEST_URL=mysql://user:pass@127.0.0.1:3306/testdb \
#     bash tests/synth-verify/run_m4_cardinality.sh
#
# Loads tests/synth-verify/fixture_cardinality.sql (100 parent keys, 70 child
# rows distributed {0:0.5, 1:0.3, 2:0.2}), trains, then generates twice:
#   * `cardinality: modeled`  -> the child row count follows the distribution;
#   * default exact_rows      -> the child row count stays `--rows`.
# tests/synth-verify/verify_cardinality.py asserts both plus the learned model.
#
# Environment: same as run_m1.sh (HEPTA_DBCLI_TEST_URL, HEPTA_BIN,
# SYNTH_E2E_MYSQL_CMD, SYNTH_E2E_KEEP, SYNTH_E2E_OUT).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${ROOT}/../.." && pwd)"

URL="${HEPTA_DBCLI_TEST_URL:?set HEPTA_DBCLI_TEST_URL to a mysql:// URL}"
HEPTA_BIN="${HEPTA_BIN:-${REPO_ROOT}/target/debug/hepta_dbcli}"
OUT="${SYNTH_E2E_OUT:-$(mktemp -d "${TMPDIR:-/tmp}/synth-card-verify.XXXXXX")}"

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

echo "== loading fixture =="
mysql_stdin < "${ROOT}/fixture_cardinality.sql"

export HEPTA_DBCLI_URL="${URL}"

echo "== synth train =="
"${HEPTA_BIN}" synth train --tables "card_parent,card_child" --schema "${SCHEMA}" \
    --output "${OUT}/models" --sample 10000

# Rules are written by hand: the point is the `cardinality` key, and a hand
# written file is stable across rules-draft output changes.
cat > "${OUT}/rules_modeled.yaml" <<'YAML'
version: "1"
tables:
  - name: card_parent
    relationships: []
  - name: card_child
    relationships:
      - pk: parent_id
        references: [card_parent.id]
        cardinality: modeled
YAML

cat > "${OUT}/rules_exact.yaml" <<'YAML'
version: "1"
tables:
  - name: card_parent
    relationships: []
  - name: card_child
    relationships:
      - pk: parent_id
        references: [card_parent.id]
YAML

echo "== synth generate (cardinality: modeled) =="
"${HEPTA_BIN}" synth generate --models "${OUT}/models" --rules "${OUT}/rules_modeled.yaml" \
    --output "${OUT}/modeled" --rows 100 --seed 7 --format csv

echo "== synth generate (default exact_rows) =="
"${HEPTA_BIN}" synth generate --models "${OUT}/models" --rules "${OUT}/rules_exact.yaml" \
    --output "${OUT}/exact" --rows 100 --seed 7 --format csv

echo "== synth report (cardinality TV) =="
"${HEPTA_BIN}" synth report --models "${OUT}/models" --data "${OUT}/modeled" \
    --rules "${OUT}/rules_modeled.yaml" --output "${OUT}/report.json"

echo
echo "== verifying =="
if python3 "${ROOT}/verify_cardinality.py" "${OUT}"; then
    echo "M4 cardinality verification PASSED"
    if [[ "${SYNTH_E2E_KEEP:-0}" != "1" ]]; then
        rm -rf "${OUT}"
    fi
    exit 0
fi

echo "M4 cardinality verification FAILED (artifacts kept in ${OUT})" >&2
exit 1
