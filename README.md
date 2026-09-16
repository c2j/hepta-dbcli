# hepta_dbcli

CLI and MCP server for MySQL / PolarDB-X / Oracle / GaussDB / DuckDB database introspection, plus cross-database table comparison (`delta-diff`).

Current version: **0.5.3**.

## Features

- **MCP server** — spawn as a Model Context Protocol server for AI tools (Claude, Cursor, etc.) with per-dialect read-only enforcement
- **Multi-database** — MySQL, PolarDB-X, Oracle, GaussDB (default features: `oracle-rs`, `oracle`, `gaussdb`, `synth`) and DuckDB (optional feature `duckdb`)
- **One-shot CLI** — execute SQL from command line, file, or stdin with `table` / `json` / `csv` / `vertical` output
- **Interactive REPL** — database-aware SQL prompt with multi-line editing, history, and dot commands
- **Layered write control** — CLI/REPL data changes require `--allow-write`; destructive DDL is always refused; MCP stays read-only
- **Cross-DB delta-diff** — compare table data across two named connections (`hashdiff` / `joindiff` / `bucketdiff` / `iblt` / `keyeddiff`); CLI + MCP
- **Synthetic data generation** — train per-column statistical models from real tables, then generate look-alike data with FK integrity (`synth` CLI; in default features since 0.5.0, MCP does not expose it)
- **Multi-connection** — `~/.hepta-dbcli.toml` with per-connection timeouts
- **OS keychain** — passwords stored in macOS Keychain or Linux Secret Service, with automatic migration from plaintext config files
- **Audit log** — separate local JSONL ledger of every executed statement, connection and outcome (independent of `RUST_LOG`)

## Installation

### Download binary

Prebuilt binaries for Linux (x86_64, arm64) and Windows (x86_64) from [GitHub Releases](https://github.com/c2j/hepta-dbcli/releases):

- `hepta_dbcli-{version}-x86_64-unknown-linux-gnu.zip` (glibc 2.28+, e.g. Debian 10 / RHEL 8 / Ubuntu 20.04)
- `hepta_dbcli-{version}-aarch64-unknown-linux-gnu.zip`
- `hepta_dbcli-{version}-x86_64-pc-windows-msvc.zip`

macOS: build from source (no prebuilt artifact yet).

### Build from source

```bash
git clone https://github.com/c2j/hepta-dbcli.git
cd hepta-dbcli
cargo build --release -p polar-mysql
# binary at: target/release/hepta_dbcli

# With DuckDB support (compiles the bundled DuckDB C++ core — first build takes several minutes)
cargo build --release -p polar-mysql --features duckdb
# On memory-constrained machines, limit build parallelism to avoid OOM during the C++ compile:
cargo build --release -p polar-mysql --features duckdb -j 2
```

Default features include Oracle (`oracle-rs` + native fallback), GaussDB, and synthetic data generation (`synth`). Prebuilt GitHub Release binaries match that set from **0.5.0**. Oracle 11g connections fall back to the `oracle` crate and need [Oracle Instant Client](https://www.oracle.com/database/technologies/instant-client.html) on the PATH.

To omit synth: `cargo build --release -p polar-mysql --no-default-features --features "oracle-rs,oracle,gaussdb"`.

DuckDB notes: the `bundled` feature compiles DuckDB from source (C++ toolchain required) and statically links it. The bundled build excludes the ICU extension — date arithmetic like `now() - interval '1 day'` needs `INSTALL icu; LOAD icu;` at runtime (`TIMESTAMPTZ - INTERVAL` fails without it; the delta-diff incremental window casts `NOW()` to naive `TIMESTAMP` first, which yields **UTC wall-clock** in bundled builds — both sides of a diff use the same cutoff, so results stay self-consistent). A `.duckdb` file allows one writer at a time; concurrent readers require `?mode=ro`. delta-diff supports DuckDB on both sides (`BLOB`/`JSON`/`TEXT` columns are excluded from row hashing, and `TIMESTAMPTZ` normalizes to UTC text without ICU). Cross-backend caveat: UUID/BLOB/non-finite-float values may normalize differently than GaussDB/Oracle — such columns are either excluded loudly or may report diffs; verify when comparing across engines.

## Configuration

### Config file

Create `~/.hepta-dbcli.toml` (also reads legacy `~/.polardb-mysql.toml`):

```toml
# Single connection (sections below are the defaults; driver defaults to mysql)
host = "127.0.0.1"
port = 3306
user = "root"
password = "your-password"
database = "mysql"
```

On first successful connection, the password is automatically migrated to your OS keychain and the file is rewritten with `password = "keyring"`.

The connection name is the TOML table key (`[connections.dev]` → `--name dev`). An explicit `name =` field inside the table is ignored.

### Multi-connection

```toml
default_connection = "dev"

[connections.prod]
host = "prod-db.example.com"
user = "readonly"
password = "keyring"
database = "orders"

[connections.dev]
host = "127.0.0.1"
user = "root"
password = "keyring"
database = "orders"

[connections.ora]
driver = "oracle"
host = "oracle.internal"
port = 1521
user = "scott"
password = "keyring"
database = "FREEPDB1"

[connections.gauss]
driver = "gaussdb"
host = "gauss.internal"
port = 5432
user = "gaussdb"
password = "keyring"
database = "testdb"
sslmode = "disable"          # disable | require | verify-ca | verify-full

[connections.duck]
driver = "duckdb"
database = "/data/analytics/shop.duckdb"   # file path, or ":memory:"
```

`database` also accepts the alias `dbname`. Default ports: MySQL `3306`, Oracle `1521`, GaussDB `5432`. DuckDB is embedded — it has no host/port/user/password; `database` holds the file path (or `:memory:`) and password fields are ignored. A missing database file is an error, never silently created. Add `?mode=ro` to open read-only (multiple processes may then read the same file concurrently).

URL form is also accepted:

```toml
[connections.ora]
url = "oracle://scott:tiger@oracle.internal:1521/FREEPDB1"

[connections.gauss]
url = "gaussdb://gaussdb:secret@gauss.internal:5432/testdb?sslmode=disable"

[connections.duck]
url = "duckdb:///data/analytics/shop.duckdb?mode=ro"
```

Special characters in passwords must be percent-encoded in URLs (`@` → `%40`).

### Environment variable

```bash
export HEPTA_DBCLI_URL="mysql://user:password@host:port/database"
export HEPTA_DBCLI_URL="oracle://scott:tiger@host:1521/FREEPDB1"
export HEPTA_DBCLI_URL="gaussdb://gaussdb:secret@host:5432/testdb?sslmode=disable"
export HEPTA_DBCLI_URL="duckdb:///data/analytics/shop.duckdb"
```

When `HEPTA_DBCLI_URL` is set, the connection name is `default` and the OS keychain is not used. Optional `HEPTA_DBCLI_PASSWORD` supplies the password separately.

### Timeout settings

```toml
# Global defaults (per-connection overrides in [connections.NAME])
statement_timeout = "30s"       # Per-query max execution time
connection_max_lifetime = "1h"  # Recycle connection after this duration
```

Supported units: `ms`, `s`, `min`, `h`, or plain seconds.

### SSL/TLS

```toml
# MySQL: field form only enables require
sslmode = "require"
# or via URL:
url = "mysql://user:password@host:3306/db?ssl-mode=REQUIRED"

# GaussDB
sslmode = "disable"       # local / Docker
sslmode = "require"       # encrypt, no cert verify
sslmode = "verify-ca"
sslmode = "verify-full"
```

## Usage

### MCP server (default)

```bash
hepta_dbcli
hepta_dbcli mcp
hepta_dbcli --config /path/to/config.toml
```

Runs on stdio. Intended to be spawned by MCP clients. `execute_query` is read-only:

| Dialect | Allowed prefixes |
|---------|------------------|
| MySQL / PolarDB-X | `SELECT`, `EXPLAIN`, `SHOW`, `DESCRIBE`, `DESC` |
| Oracle / GaussDB | `SELECT`, `EXPLAIN`, `WITH` |
| DuckDB | `SELECT`, `EXPLAIN`, `WITH`, `SHOW`, `DESCRIBE`, `DESC`, `SUMMARIZE` |

### One-shot SQL

```bash
# From command line
hepta_dbcli cli --sql "SELECT version()"

# From file
hepta_dbcli cli --file query.sql

# From stdin
echo "SHOW TABLES" | hepta_dbcli cli

# Custom output format
hepta_dbcli cli --sql "SELECT * FROM users" --format json
hepta_dbcli cli --sql "SELECT * FROM users" --format csv
hepta_dbcli cli --sql "SELECT * FROM users" --format vertical

# Target a specific connection
hepta_dbcli cli --name prod --sql "SELECT count(*) FROM orders"
hepta_dbcli cli --name gauss --sql "SELECT version()"
```

Read-only statements (`SELECT`, `SHOW`, `EXPLAIN`, `DESCRIBE`, transaction control) run as before. Data changes need `--allow-write`, **on every dialect**: MySQL and Oracle CLI sessions used to accept a bare `INSERT` and no longer do.

```bash
hepta_dbcli cli --sql "INSERT INTO t VALUES (1)"                 # refused
hepta_dbcli cli --allow-write --sql "INSERT INTO t VALUES (1)"   # runs, prints "1 rows affected"
hepta_dbcli cli --allow-write --sql "DROP TABLE t"               # always refused
```

| Layer | Statements | CLI/REPL | MCP |
|-------|-----------|----------|-----|
| L1 read-only | `SELECT` / `SHOW` / `EXPLAIN` / `DESCRIBE` | allowed | allowed |
| L2 data change | `INSERT` / `UPDATE` / `DELETE` / `CALL` | needs `--allow-write` | refused |
| L3 destructive | `DROP` / `TRUNCATE` / `ALTER` / `CREATE` / `GRANT` | always refused | refused |

`--allow-write` is a global flag and applies only to the CLI and REPL — `hepta_dbcli --allow-write mcp` exits with an error. It is a guard rail, not a security boundary: pair it with a low-privilege database account. On GaussDB the flag drops the `default_transaction_read_only` session guard; on MySQL/Oracle it opens the client-side gate. Writes are audited fail-closed: if the audit record cannot be written, the statement is refused before it reaches the engine.

### Interactive REPL

```bash
hepta_dbcli cli --interactive
hepta_dbcli cli -i --name dev
```

REPL commands:

- `.help` / `?` — show help
- `.connect [name]` — switch connection
- `.history` — show SQL execution history
- `.output [file]` — redirect SQL output to file
- `.save <file> [format]` — save last result
- `.clear` / `.cls` — clear screen
- `.exit` / `.quit` — exit

Output formats: `table` (default), `json`, `vertical`, `csv`.

End SQL statements with `;` + Enter to execute. Multi-line with incomplete statements is supported.

### Test connection

```bash
hepta_dbcli check
hepta_dbcli check --verbose
hepta_dbcli check --name prod
hepta_dbcli check --name gauss
hepta_dbcli check --name duck
```

MySQL probes three TLS modes (plain / skip-verify / verify). Oracle and GaussDB each make a single connect attempt (Oracle tries pure-Rust `oracle-rs` first, then Instant Client).

### Store password

```bash
hepta_dbcli store-password
hepta_dbcli store-password --name prod
```

Prompts for password and stores it in the OS keychain under service `hepta-dbcli`, account `{connection_name}#{8_hex_chars}`.

### Audit log

Every executed statement is written to a local JSONL ledger that is independent of the `tracing` troubleshooting log and unaffected by `RUST_LOG`. It records who ran what, on which connection, with what outcome.

```
$XDG_DATA_HOME/hepta-dbcli/audit/hepta-dbcli-audit.YYYY-MM-DD.jsonl
# macOS: ~/Library/Application Support/hepta-dbcli/audit/
```

- One JSON object per line (schema `v: 1`): envelope (`ts`, `event_id`, `session_id`, `seq`, `channel`, `actor`, `connection`, `action`, `class`, `decision`) plus action detail (`sql`, `outcome`, `deny_reason`, `detail`).
- `channel` is one of `mcp`, `cli`, `repl`, `delta_diff`, `synth`.
- Enabled by default; the directory and files are created `0700` / `0600`. Passwords and DSN userinfo are stripped, and result rows / EXPLAIN bodies are never written.
- A rejected statement is recorded even though it never reached the engine: MCP's read-only gate as `deny_reason: "prefix"`, the CLI write gate as `write_flag_required` / `destructive_ddl`. Query errors are `decision: "error"` with `error_kind` + `sqlstate`.

Global flags:

| Flag | Meaning |
|------|---------|
| `--audit-dir <path>` | Write the ledger under `<path>` (CI / log shipping) |
| `--audit-meta` | Also record high-noise meta tools (`list_tables`, `get_table_metadata`, `get_database_info`, `list_connections`) |
| `--audit-retention-days <n>` | Delete audit files older than `n` days on startup (default `30`, `0` = keep forever) |

The ledger cannot be switched off: `--audit-dir` only changes where it is written. If the audit directory is unusable (permissions, read-only filesystem) the failure is reported on stderr and read-only queries still run; writes fail closed.

### Cross-database delta-diff

Compare table data on two named connections. Default: `auto` strategy, `snapshot` consistency, diff recheck.

```bash
# Same table name on both sides
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders

# Different table / schema names
hepta_dbcli delta-diff --left mysql_dev --right ora_dev \
  --left-table orders --right-table ORDERS \
  --left-schema shop --right-schema SCOTT

# Filter or incremental window (--where and --update-column are mutually exclusive)
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --where "status = 'PAID'"
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --update-column updated_at --update-since "1 day"

# Preview the plan without comparing
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders --dry-run

# Resume a long run (JSONL checkpoint, format version 2)
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --checkpoint /tmp/orders.ckpt

# Export all diffs (suffix infers csv / jsonl / json / sql)
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --export /tmp/orders.diff.csv

# SQL patch is CLI-only and writes a file — it does not execute DML
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --export /tmp/orders.patch.sql --apply-to right
```

| `--strategy` | When `auto` picks it |
|--------------|----------------------|
| `bucketdiff` | No usable key, or left/right keys cannot be paired |
| `keyeddiff` | Key exists but is not a single integer column (composite / string / …) |
| `joindiff` | Same-connection MySQL-family with a single integer key |
| `iblt` | Cross-connection (or non-MySQL) with a single integer key |
| `hashdiff` | Not chosen by `auto`; pass `--strategy hashdiff` for bisection checksums |

Exit codes (CI contract): `0` identical, `1` differences found, `2` error. `--dry-run` exits `0` on success.

See [UserGuide.md](UserGuide.md) for the full flag list, export formats, and `--rtrim-char-columns`.

### Synthetic data generation

Sample-based generation: train per-column marginals plus a Gaussian copula correlation matrix from real tables, then generate look-alike data with cross-table foreign-key integrity. CLI-only; included in default features since **0.5.0** (prebuilt Release binaries include it). MCP does not expose synth.

```bash
# 1. Train table models (samples tables, fits per-column marginals
#    and the copula correlation matrix)
hepta_dbcli synth train --name dev --tables users,orders --output .synth

# 2. Draft a rules YAML from the database's foreign keys
hepta_dbcli synth rules-draft --name dev --tables users,orders \
  --models .synth --output synth-rules.yaml

# 3. Generate synthetic rows
hepta_dbcli synth generate --models .synth --rules synth-rules.yaml \
  --output synth-out --rows 1000 --seed 42 --format csv   # csv / jsonl / json / sql

# Validate a trained model file
hepta_dbcli synth validate --model .synth/users.model.json

# Score generated data against the holdout baseline `train` recorded
hepta_dbcli synth report --models .synth --data synth-out \
  --rules synth-rules.yaml --min-score 0.85   # --against-db for real DB key pools
```

`train` samples each numeric column (Normal / Beta / Gamma / Uniform / ECDF) and
keeps the family with the smallest KS statistic on a held-out slice, so skewed,
multi-modal and zero-inflated columns keep their shape; `columns.<name>.marginal`
in the rules YAML forces a family instead. It also writes
`<table>.report-baseline.json` (quantile knots, value frequencies, pair
statistics; no raw rows), which lets `synth report` score a model offline:
`1-KS` per numeric column, `1-TV` per categorical column, Pearson/joint TV per
column pair, and FK join rates (against the generated parent keys, or the live
database with `--against-db`). Categorical columns with more than 50 levels are
reported but left out of the average, since total variation over hundreds of
levels is sampling noise rather than fidelity; a models directory without a
baseline reports `skipped` (exit 0) unless `--strict` is set, and `--min-score`
gates on every model: a table that could not be scored fails the run instead of
being averaged away.

Tables are generated in FK topological order (cycles rejected). Child FK values are drawn from the parent's generated keys; `pool_strategy: !projection { unique: true }` samples them without replacement. `--seed` derives a stable per-table RNG stream. See [UserGuide.md](UserGuide.md) §10 for the rules YAML reference.

Benchmarks: Case A (synthetic 4-column vs SDV-GC, [report](tests/benchmark/REPORT.md)), P1 (SynMeter real single-table, Adult-only: Wasserstein / MLA / QueryError gates, [report](tests/benchmark/p1/REPORT.md)) and P2 (ogagila multi-table FK: insert/orphan/amount-on-grid gated; fan-out KS recorded, [report](tests/benchmark/p2/REPORT.md)) live under `tests/benchmark/` with gates enforced by the `synth-benchmark` CI workflow (P2 is dispatch-only). Case B (CTGAN/TabDDPM/GReaT) and Case C (HMA/ClavaDDPM) comparisons are out of scope for this milestone; SynMeter/torch/SDV exist only in the benchmark venv, never in `Cargo.toml`.

### Cross-database delta-diff

```bash
# Compare the same table on two connections
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders

# Incremental window + CSV export
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --update-column updated_at --update-since "1 day" \
  --export /tmp/orders.diff.csv

# Resume a long run
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --checkpoint /tmp/orders.ckpt

# SQL patch generation stays CLI-only (writes DML)
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --export /tmp/orders.patch.sql --apply-to right
```

## MCP Tools

When running as MCP server, the following tools are available:

| Tool | Description |
|------|-------------|
| `get_database_info` | Server version, current user, charset, OS |
| `list_tables` | All user tables/views with engine, row count, size |
| `get_table_metadata` | Column types, nullability, defaults, indexes |
| `execute_query` | Read-only query (dialect prefixes above). Appends `LIMIT N` / `FETCH FIRST N ROWS ONLY` / 11g `ROWNUM`. Default `max_rows` 1000, cap 10000 |
| `get_execution_plan` | EXPLAIN or EXPLAIN ANALYZE (MySQL TEXT/JSON; Oracle `EXPLAIN PLAN` + `DBMS_XPLAN`; GaussDB `EXPLAIN`) |
| `list_connections` | List all configured connections and their status |
| `delta_diff` | Cross-DB table compare (read-only). Supports incremental (`update_column`/`update_since`), `checkpoint`, and csv/jsonl/json `export`. SQL patch `--apply-to` stays on the CLI. |

`delta_diff` parameters: `left_connection`, `right_connection`, `table` (required); optional `left_table` / `right_table`, `schema` / `left_schema` / `right_schema`, `key_columns`, `columns`, `where_condition`, `update_column` / `update_since`, `checkpoint`, `export`, `export_format` (csv/jsonl/json), `export_rows`, `strategy`, `consistency`, `recheck`, `sample_limit` (default 1000), `summary_only`.

## Development

```bash
# Release (default features: oracle-rs + oracle + gaussdb + synth)
cargo build --release -p polar-mysql

# Format check
cargo fmt --all -- --check

# Lint (Ubuntu: apt-get install libdbus-1-dev pkg-config first)
cargo clippy --all --all-targets

# Unit tests
cargo test --all

# Synth unit tests (included in default features since 0.5.0)
cargo test --all

# Integration tests — see tests/README.md
# MySQL
HEPTA_DBCLI_TEST_URL=mysql://mcp:testpass@127.0.0.1:3306/testdb \
  cargo test --all --features integration --test regress_mysql

# Oracle (needs Docker Oracle)
POLARDB_ORACLE_TEST_URL=oracle://system:testpass@127.0.0.1:1521/FREEPDB1 \
  cargo test --features "oracle,integration" --test regress_oracle

# GaussDB
GAUSSDB_TEST_URL="host=127.0.0.1 port=5432 user=gaussdb password=testpass@123 dbname=testdb" \
  cargo test --features "gaussdb,integration" --test regress_gaussdb

# DuckDB (embedded — no external service needed)
cargo test --features "duckdb,integration" --test regress_duckdb
```

CI enforces: `cargo fmt --check` → `cargo clippy` → `cargo test` (in that order).

Quick Docker fixtures: `docker compose -f tests/docker-compose.yml up -d` and the TOML files under `tests/` (`docker-mysql.toml`, `docker-oracle.toml`, `docker-gaussdb.toml`, `docker-all.toml`).

## License

MIT OR Apache-2.0
