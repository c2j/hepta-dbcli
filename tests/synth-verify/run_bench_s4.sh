#!/usr/bin/env bash
# Issue #89 S4④/S4⑤ measurement: cardinality modeling on 100k parent keys
# and PII fake-value throughput. No database required.
#
#   bash tests/synth-verify/run_bench_s4.sh
#
# Runs the two #[ignore]d bench tests in the synth test module with
# --nocapture and summarizes the printed measurements. Both tests also
# carry behavior assertions (round-trip TV <= 0.02; id-card uniqueness
# > 99k/100k), so a quality regression fails the run rather than merely
# changing a number.
#
# Environment: HEPTA_BIN optionally points at a prebuilt binary (unused
# here; the benches run through cargo). SYNTH_E2E_KEEP=1 keeps nothing
# (the benches are pure computation).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${ROOT}/../.." && pwd)"
cd "${REPO_ROOT}"

echo "== issue #89 S4 benches (debug profile; release numbers are ~5-10x faster) =="
cargo test --all --bin hepta_dbcli bench_100k_parent_cardinality -- --ignored --nocapture
cargo test --all --bin hepta_dbcli bench_pii_generation -- --ignored --nocapture

echo
echo "== how to reproduce with release speed =="
echo "  cargo test --all --bin hepta_dbcli --release bench_100k_parent_cardinality -- --ignored --nocapture"
echo "  cargo test --all --bin hepta_dbcli --release bench_pii_generation -- --ignored --nocapture"
