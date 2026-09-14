#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH_ROOT="$(cd "${ROOT}/.." && pwd)"
REPO_ROOT="$(cd "${BENCH_ROOT}/../.." && pwd)"
VENV="${BENCH_ROOT}/.venv-p1"
SYNMETER_ROOT="${VENV}/synmeter"
SYNMETER_COMMIT="074d7baf28dc49e1a00cd384c0dd8be6bedb7c9c"
DATA_DIR="${ROOT}/data/adult"
RESULTS_DIR="${ROOT}/results"
WORK_DIR="${ROOT}/work"

if [[ -n "${HEPTA_BIN:-}" ]]; then
    if [[ ! -x "${HEPTA_BIN}" ]]; then
        printf 'HEPTA_BIN is not executable: %s\n' "${HEPTA_BIN}" >&2
        exit 2
    fi
else
    printf 'Building hepta_dbcli (release + duckdb)...\n'
    cargo build --release --features duckdb --manifest-path "${REPO_ROOT}/Cargo.toml"
    HEPTA_BIN="${REPO_ROOT}/target/release/hepta_dbcli"
fi
export HEPTA_BIN
export HEPTA_P1_WORKDIR="${WORK_DIR}"

if [[ ! -x "${VENV}/bin/python" ]]; then
    python3 -m venv "${VENV}"
fi
"${VENV}/bin/pip" install --upgrade pip
"${VENV}/bin/pip" install -r "${BENCH_ROOT}/requirements.txt"

if [[ ! -d "${SYNMETER_ROOT}/.git" ]]; then
    git clone --depth 1 https://github.com/yuntaod/SynMeter "${SYNMETER_ROOT}"
    if ! git -C "${SYNMETER_ROOT}" checkout "${SYNMETER_COMMIT}"; then
        rm -rf "${SYNMETER_ROOT}"
        git clone https://github.com/yuntaod/SynMeter "${SYNMETER_ROOT}"
        git -C "${SYNMETER_ROOT}" checkout "${SYNMETER_COMMIT}"
    fi
else
    if ! git -C "${SYNMETER_ROOT}" checkout "${SYNMETER_COMMIT}"; then
        git -C "${SYNMETER_ROOT}" fetch origin "${SYNMETER_COMMIT}"
        git -C "${SYNMETER_ROOT}" checkout "${SYNMETER_COMMIT}"
    fi
fi
cp "${ROOT}/hepta_synmeter.py" "${SYNMETER_ROOT}/synthesizer/hepta.py"
cp "${ROOT}/sdv_gc.py" "${SYNMETER_ROOT}/synthesizer/sdv_gc.py"

mkdir -p "${RESULTS_DIR}" "${WORK_DIR}"
"${VENV}/bin/python" "${ROOT}/prep_adult.py" --output-dir "${DATA_DIR}"

for model in hepta sdv_gc; do
    "${VENV}/bin/python" "${ROOT}/eval_fidelity_cpu.py" \
        --synmeter-root "${SYNMETER_ROOT}" \
        --model "${model}" \
        --data-dir "${DATA_DIR}" \
        --output-dir "${RESULTS_DIR}"
    "${VENV}/bin/python" "${ROOT}/eval_utility_cpu.py" \
        --synmeter-root "${SYNMETER_ROOT}" \
        --model "${model}" \
        --data-dir "${DATA_DIR}" \
        --output-dir "${RESULTS_DIR}"
done

"${VENV}/bin/python" "${ROOT}/gates.py" --results-dir "${RESULTS_DIR}" --output-dir "${ROOT}"
