# synth M1/M2 verification (`tests/synth-verify`)

End-to-end acceptance checks for the synth M1 work (issues #62 datetime columns,
#63 NULL reproduction, #64 decimal scale, #65 robustness bundle) and the M2 work
(#66 ECDF auto-selection, #67 `synth report`). These run against a **real
MySQL** and assert the artifacts a user actually gets, which is the half of the
acceptance criteria that the Rust unit tests cannot cover.

## Run it

```bash
HEPTA_DBCLI_TEST_URL=mysql://user:pass@127.0.0.1:3306/testdb \
  bash tests/synth-verify/run_m1.sh
```

The script loads `fixture_mysql.sql` into the database named in the URL, then
runs `synth train` / `rules-draft` / `generate` (csv, sql, sql with
`--no-schema-qualifier`, and a second csv pass for determinism) and finally
asserts the results with `verify_m1.py`. Exit code is 0 only when every check
passes.

| Variable | Meaning |
|---|---|
| `HEPTA_DBCLI_TEST_URL` | required; `mysql://user:pass@host:port/db`. The database name is used as the schema for `--schema` and the qualifier assertions |
| `HEPTA_BIN` | binary under test (default `target/debug/hepta_dbcli`, built if missing) |
| `SYNTH_E2E_MYSQL_CMD` | command that reads SQL on stdin and applies it. Use this when there is no `mysql` client (CI passes `docker exec -i <cid> mysql -uroot -ptestpass -D testdb`) |
| `SYNTH_E2E_OUT` | output directory (default: fresh `mktemp -d`) |
| `SYNTH_E2E_KEEP` | `1` keeps the output directory after a pass |

The fixture is dropped and recreated on every run; it only touches its own
`m1_verify_*` tables. The connection is passed through `HEPTA_DBCLI_URL` rather
than a config file, so no password is written to disk or to the OS keychain.

## M2 run

```bash
HEPTA_DBCLI_TEST_URL=mysql://user:pass@127.0.0.1:3306/testdb \
  bash tests/synth-verify/run_m2.sh
```

It reuses the same fixture and environment variables as the M1 run. After
`train` (which now also writes `<table>.report-baseline.json`) it generates the
same data twice, then runs `synth report` four ways: offline, against the live
key pools (`--against-db`), on a degraded copy, and on a models directory with
no baseline. `verify_m2.py` asserts:

| Issue | Check |
|---|---|
| #66 | the trained model is not a blanket Normal: `m1_verify_parent.{id,cjje,whole_dec}` are `uniform`/`ecdf`/`gamma`/`beta`, while the formatted `trade_time` keeps its Normal epoch marginal |
| #67 AC1 | the report carries `shapes` / `pairs` / `fk` sections and an overall score in `(0, 1)` |
| #67 AC2 | shifting `cjje` by +10000 drops that column below 0.8 (>= 0.15 below the clean run) and fails `--min-score` with a non-zero exit code |
| #67 AC4 | offline, the FK edge is scored at rate 1.0 against the **generated parent keys** (`source: "generated"`); with `--against-db` it is scored against the live pool (`source: "database"`) and still 1.0 with no warn |
| #67 AC3 | a models directory without a baseline yields `status: skipped` with a reason and exit code 0; `--strict` fails. A model with no generated data is listed as a skipped table (not silently absent) and also fails `--strict` |
| #67 AC5 | two runs over the same inputs produce byte-identical report JSON (and the same seed regenerates byte-identical data) |
| #67 AC6 | baseline files contain only aggregate payloads (numeric knots or `[value, frequency]` pairs), never a row record |

## What is asserted

| Issue | Check |
|---|---|
| #62 | `TIMESTAMP` column trains as `datetime` with an inferred primary format and epoch min/max; every generated value re-parses with that exact format and stays inside the trained epoch range |
| #62 | `DATETIME(6)` (fixed-width microseconds, including `…00.000000`) infers a `%.6f` format and keeps six digits on output |
| #63 | trained `null_rate` is reproduced within ±3pp; non-NULL values stay in the trained value space; NULLs also appear in a nullable child-table column |
| #64 | `DECIMAL(18,4)` / `DECIMAL(4,2)` scales are learned and every generated value sits on that scale (no `…9999` binary tail), each against its own digit pattern |
| #64 | a `DECIMAL(18,4)` column holding only whole values keeps `decimal_scale: 4` (and no integer rounding) instead of degrading to i64 |
| #65a | a 120-level dictionary column survives training intact under `--categorical-top-k full` |
| #65b/c | `rules-draft` discovers the FK; every generated child key exists in the generated parent pool; a non-unique FK reuses the pool instead of inventing keys |
| #65d | SQL export is schema qualified by default; `--no-schema-qualifier` restores the legacy statement |
| #65f | the resolved schema is recorded in the model; a full non-Oracle sample is not flagged `truncated` |
| — | same-seed re-generation is byte-identical |

`#65e` (Oracle 100-row truncation) needs Oracle, so it is covered by unit tests
plus a manual check against the `gvenzl/oracle-free` container: training a
200-row table prints the truncation WARNING and writes
`"truncated": true` into the model's `provenance`.

Timezone-aware columns (`timestamptz`) have no equivalent MySQL type, so the
UTC-normalisation path is covered by `synth::datetime` unit tests and by the
`fixture_mysql.sql` microsecond column for the format half of the behaviour.

## CI

`.github/workflows/ci.yml` runs both scripts in the `test` job after the
integration suite, against the job's `mysql:8` service container.
