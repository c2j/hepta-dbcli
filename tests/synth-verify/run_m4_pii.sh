#!/usr/bin/env bash
# Issue #71 acceptance: PII recognition and anonymized generation.
#
#   HEPTA_DBCLI_TEST_URL=mysql://user:pass@127.0.0.1:3306/testdb \
#     bash tests/synth-verify/run_m4_pii.sh
#
# Loads tests/synth-verify/fixture_pii.sql, then trains/generates four ways:
#   * default            -> email/phone/full_name are anonymized;
#   * default again      -> same seed reproduces byte-identical output;
#   * `pii_stable_mapping` -> equal training values map to equal fakes;
#   * `sdtype: keep`     -> the column keeps its trained value space.
# tests/synth-verify/verify_pii.py asserts all of it plus the profile/model leak
# checks.
#
# Environment: same as run_m1.sh.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${ROOT}/../.." && pwd)"

URL="${HEPTA_DBCLI_TEST_URL:?set HEPTA_DBCLI_TEST_URL to a mysql:// URL}"
HEPTA_BIN="${HEPTA_BIN:-${REPO_ROOT}/target/debug/hepta_dbcli}"
OUT="${SYNTH_E2E_OUT:-$(mktemp -d "${TMPDIR:-/tmp}/synth-pii-verify.XXXXXX")}"

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
mysql_stdin < "${ROOT}/fixture_pii.sql"

export HEPTA_DBCLI_URL="${URL}"

echo "== synth train (default: anonymize) =="
"${HEPTA_BIN}" synth train --tables pii_users --schema "${SCHEMA}" \
    --output "${OUT}/models" --sample 10000

echo "== synth train (sdtype: keep) =="
"${HEPTA_BIN}" synth train --tables pii_users --schema "${SCHEMA}" \
    --output "${OUT}/models_keep" --sample 10000 --rules "${ROOT}/rules_pii_keep.yaml"

echo "== synth generate (default) =="
"${HEPTA_BIN}" synth generate --models "${OUT}/models" --rules "${ROOT}/rules_pii_plain.yaml" \
    --output "${OUT}/masked" --rows 200 --seed 7 --format csv

echo "== synth generate (same seed again) =="
"${HEPTA_BIN}" synth generate --models "${OUT}/models" --rules "${ROOT}/rules_pii_plain.yaml" \
    --output "${OUT}/masked2" --rows 200 --seed 7 --format csv

echo "== synth generate (stable_mapping) =="
"${HEPTA_BIN}" synth generate --models "${OUT}/models" --rules "${ROOT}/rules_pii_stable.yaml" \
    --output "${OUT}/stable" --rows 200 --seed 7 --format csv

echo "== synth generate (keep) =="
"${HEPTA_BIN}" synth generate --models "${OUT}/models_keep" --rules "${ROOT}/rules_pii_keep.yaml" \
    --output "${OUT}/keep" --rows 200 --seed 7 --format csv

echo "== synth train + generate (natural-key PII parent + FK child) =="
"${HEPTA_BIN}" synth train --tables pii_accounts,pii_logins --schema "${SCHEMA}" \
    --output "${OUT}/natural_models" --sample 10000 2> "${OUT}/natural_train.err"
"${HEPTA_BIN}" synth generate --models "${OUT}/natural_models" \
    --rules "${ROOT}/rules_pii_natural.yaml" \
    --output "${OUT}/natural" --rows 40 --seed 7 --format csv

echo
echo "== verifying =="
if python3 "${ROOT}/verify_pii.py" "${OUT}"; then
    echo "M4 PII verification PASSED"
    if [[ "${SYNTH_E2E_KEEP:-0}" != "1" ]]; then
        rm -rf "${OUT}"
    fi
    exit 0
fi

echo "M4 PII verification FAILED (artifacts kept in ${OUT})" >&2
exit 1
