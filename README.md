# hepta_dbcli

CLI and MCP server for MySQL / PolarDB-X / Oracle / GaussDB database introspection, plus cross-database table comparison (`delta-diff`).

Current version: **0.4.5**.

## Features

- **MCP server** — spawn as a Model Context Protocol server for AI tools (Claude, Cursor, etc.) with per-dialect read-only enforcement
- **Multi-database** — MySQL, PolarDB-X, Oracle, and GaussDB (default features: `oracle-rs`, `oracle`, `gaussdb`)
- **One-shot CLI** — execute SQL from command line, file, or stdin with `table` / `json` / `csv` / `vertical` output
- **Interactive REPL** — database-aware SQL prompt with multi-line editing, history, and dot commands
- **Cross-DB delta-diff** — compare table data across two named connections (`hashdiff` / `joindiff` / `bucketdiff` / `iblt` / `keyeddiff`); CLI + MCP
- **Multi-connection** — `~/.hepta-dbcli.toml` with per-connection timeouts
- **OS keychain** — passwords stored in macOS Keychain or Linux Secret Service, with automatic migration from plaintext config files

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
```

Default features already include Oracle (`oracle-rs` + native fallback) and GaussDB. Oracle 11g connections fall back to the `oracle` crate and need [Oracle Instant Client](https://www.oracle.com/database/technologies/instant-client.html) on the PATH.

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
```

`database` also accepts the alias `dbname`. Default ports: MySQL `3306`, Oracle `1521`, GaussDB `5432`.

URL form is also accepted:

```toml
[connections.ora]
url = "oracle://scott:tiger@oracle.internal:1521/FREEPDB1"

[connections.gauss]
url = "gaussdb://gaussdb:secret@gauss.internal:5432/testdb?sslmode=disable"
```

Special characters in passwords must be percent-encoded in URLs (`@` → `%40`).

### Environment variable

```bash
export HEPTA_DBCLI_URL="mysql://user:password@host:port/database"
export HEPTA_DBCLI_URL="oracle://scott:tiger@host:1521/FREEPDB1"
export HEPTA_DBCLI_URL="gaussdb://gaussdb:secret@host:5432/testdb?sslmode=disable"
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

CLI mode is **not** read-only (unlike MCP).

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
```

MySQL probes three TLS modes (plain / skip-verify / verify). Oracle and GaussDB each make a single connect attempt (Oracle tries pure-Rust `oracle-rs` first, then Instant Client).

### Store password

```bash
hepta_dbcli store-password
hepta_dbcli store-password --name prod
```

Prompts for password and stores it in the OS keychain under service `hepta-dbcli`, account `{connection_name}#{8_hex_chars}`.

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
| `delta_diff` | Cross-DB table compare. Returns a JSON report. Compare-only: no file export, no checkpoint, no incremental window, no SQL patch |

`delta_diff` parameters: `left_connection`, `right_connection`, `table` (required); optional `left_table` / `right_table`, `schema` / `left_schema` / `right_schema`, `key_columns`, `columns`, `where_condition`, `strategy`, `consistency`, `recheck`, `sample_limit` (default 1000), `summary_only`.

Incremental (`--update-column` / `--update-since`), `--checkpoint`, `--export` (csv/jsonl/json/sql), and `--apply-to` stay on the CLI.

## Development

```bash
# Release (default features: oracle-rs + oracle + gaussdb)
cargo build --release -p polar-mysql

# Format check
cargo fmt --all -- --check

# Lint (Ubuntu: apt-get install libdbus-1-dev pkg-config first)
cargo clippy --all --all-targets

# Unit tests
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
```

CI enforces: `cargo fmt --check` → `cargo clippy` → `cargo test` (in that order).

Quick Docker fixtures: `docker compose -f tests/docker-compose.yml up -d` and the TOML files under `tests/` (`docker-mysql.toml`, `docker-oracle.toml`, `docker-gaussdb.toml`, `docker-all.toml`).

## License

MIT OR Apache-2.0
