# P2 multi-table FK benchmark

This harness trains and generates Pagila `customer`, `rental`, and `payment`
together, loads the generated SQL into an FK-enforced `staging` schema, and
evaluates P2-0/1/2. P2-3/4 and payment amount on-grid are recorded only.

## Prerequisites

- `OGAGILA_DIR` points to an ogagila checkout containing the Pagila
  `docker-compose.yml`.
- Docker and `docker-compose` are available.
- `HEPTA_BIN` optionally points to a release `hepta_dbcli`; otherwise
  `target/release/hepta_dbcli` is built with default features.
- Python 3 with venv support. Dependencies come from `../requirements.txt`.

```bash
export OGAGILA_DIR=/path/to/ogagila
export HEPTA_BIN=/path/to/hepta_dbcli  # optional
./tests/benchmark/p2/run_p2.sh
```

The source schema defaults to `public`; override it with `P2_SCHEMA`. The
runner exports `HEPTA_DBCLI_PASSWORD`, defaulting to the Pagila container
password `Enmo@123`. Set `P2_SDV_REF=1` to record optional per-table SDV-GC
reference metrics when SDV is installed.

Outputs are `p2_report.json` and `REPORT.md`. If uniform FK selection misses
P2-2 (`KS < 0.15`), the runner retries exactly once with Zipf selection on
`rental` and `payment`; both attempts remain in the JSON report.

DB-free gate check (failure is expected and proves the gates can turn red):

```bash
python3 tests/benchmark/p2/p2_report.py --self-test
```
