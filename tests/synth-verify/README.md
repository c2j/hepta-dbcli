# synth M1 verification (`tests/synth-verify`)

End-to-end acceptance checks for the synth M1 work (issues #62 datetime columns,
#63 NULL reproduction, #64 decimal scale, #65 robustness bundle). These run
against a **real MySQL** and assert the artifacts a user actually gets, which is
the half of the acceptance criteria that the Rust unit tests cannot cover.

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

## What is asserted

| Issue | Check |
|---|---|
| #62 | `TIMESTAMP` column trains as `datetime` with an inferred primary format and epoch min/max; every generated value re-parses with that exact format and stays inside the trained epoch range |
| #63 | trained `null_rate` is reproduced within ±3pp; non-NULL values stay in the trained value space; NULLs also appear in a nullable child-table column |
| #64 | `DECIMAL(18,4)` / `DECIMAL(4,2)` scales are learned and every generated value sits on that scale (no `…9999` binary tail) |
| #65a | a 120-level dictionary column survives training intact under `--categorical-top-k full` |
| #65b/c | `rules-draft` discovers the FK; every generated child key exists in the generated parent pool; a non-unique FK reuses the pool instead of inventing keys |
| #65d | SQL export is schema qualified by default; `--no-schema-qualifier` restores the legacy statement |
| #65f | the resolved schema is recorded in the model; a full non-Oracle sample is not flagged `truncated` |
| — | same-seed re-generation is byte-identical |

`#65e` (Oracle 100-row truncation) needs Oracle, so it is covered by unit tests
plus a manual check against the `gvenzl/oracle-free` container: training a
200-row table prints the truncation WARNING and writes
`"truncated": true` into the model's `provenance`.

## CI

`.github/workflows/ci.yml` runs this script in the `test` job after the
integration suite, against the job's `mysql:8` service container.
