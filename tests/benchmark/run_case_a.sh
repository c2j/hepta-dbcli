#!/bin/bash
# Case A 一键复现脚本
# 用法：./run_case_a.sh
# 要求：Docker(pagila)、hepta_dbcli (release+synth)、Python venv(sdv+sdmetrics)

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HEPTA_BIN="/Users/c2j/Projects/Desktop_Projects/DB/GaussDB_Heptadecagon/lib/hepta-dbcli/target/release/hepta_dbcli"
VENV="${ROOT}/.venv-sdv"

echo "=== Case A Benchmark Runner ==="
echo "Root: ${ROOT}"

# 1. 检查 hepta 二进制
if [[ ! -x "${HEPTA_BIN}" ]]; then
    echo "Building hepta_dbcli (release + synth)..."
    cd /Users/c2j/Projects/Desktop_Projects/DB/GaussDB_Heptadecagon/lib/hepta-dbcli
    cargo build --features synth --release
    HEPTA_BIN="/Users/c2j/Projects/Desktop_Projects/DB/GaussDB_Heptadecagon/lib/hepta-dbcli/target/release/hepta_dbcli"
fi
echo "hepta: ${HEPTA_BIN}"

# 2. 启动 pagila
echo "Starting pagila..."
cd /Users/c2j/Projects/Desktop_Projects/DB/GaussDB_Heptadecagon/lib/ogagila
docker-compose up -d
echo "Waiting for pagila..."
for i in {1..30}; do
    if docker exec pagila gsql-pagila -c "SELECT 1;" >/dev/null 2>&1; then
        echo "pagila ready"
        break
    fi
    sleep 2
done

# 3. Python venv
if [[ ! -d "${VENV}" ]]; then
    echo "Creating Python venv..."
    python3 -m venv "${VENV}"
    "${VENV}/bin/pip" install -U pip
    "${VENV}/bin/pip" install 'sdv>=1.17,<2' sdmetrics pandas scipy numpy
fi
PY="${VENV}/bin/python"

# 4. 生成真实数据
echo "=== Generating real data ==="
cd "${ROOT}"
"${PY}" case_a_real_data.py

# 5. SDV 跑
echo "=== Running SDV ==="
cd "${ROOT}"
"${PY}" case_a_sdv.py

# 6. hepta 跑
echo "=== Running hepta ==="
"${PY}" case_a_hepta.py

# 7. 生成报告
echo "=== Generating report ==="
cd "${ROOT}"
"${PY}" case_a_report.py

# 7. 展示报告
echo "=== Report ==="
cat /tmp/case_a_report.md

echo "=== Done ==="
echo "Artifacts in /tmp/: case_a_report.json, case_a_report.md, *_metrics.json"