# AGENTS.md — hepta_dbcli (dbcli)

Rust workspace. Single binary crate `hepta_dbcli` (package `polar-mysql`): a CLI + MCP server for MySQL/PolarDB-X/Oracle/GaussDB/DuckDB database introspection.

## TDD 工作流（Red → Green → Refactor）

本仓库采用测试驱动开发。一次循环只锁定一个行为：先写会失败的测试（Red），再写最小实现让它通过（Green），最后在测试全绿的前提下重构（Refactor）。探索草稿不得直接合入，必须按本文件用 TDD 重写。

### 先读再改
1. 确认改动落在哪个 crate（本仓库是 Cargo workspace，见「仓库地图」）。
2. 只用本文件列出的 cargo 命令；不要发明裸 `cargo update`。本仓库**没有** `rust-toolchain.toml`，toolchain 以 CI 使用的 `stable` 为准，不要擅自切换或加 `+nightly`。
3. 先跑与改动相关的最小测试；提交前再跑 workspace 门禁（fmt + clippy + test）。
4. 完成一个循环后按「完成标准与汇报」汇报，不要只说「做完了」。

### Never / Ask first / Always

**Never（不必请示，直接禁止）**
- 删除、注释、跳过已有测试：`#[ignore]`、注释掉 `#[test]`、把断言改成 `is_ok()` / `unwrap()` 了事
- 修改人类已有测试的断言来迁就实现
- 先提交无测试的业务行为，再「回头补」
- 写永真测试：无断言、只检查 `is_some()`、只 verify 调用次数不查参数与状态
- 用全量端到端测试覆盖本可单测完成的改动
- 提交半成品；每次对人类可见的结果必须能构建且相关测试为绿
- 把探索草稿、临时脚本、调试 `dbg!`/`println!` 留在主代码

**Ask first**
- 改人类已有测试（含断言、fixture、golden 期望值）
- 新增运行时依赖、`unsafe`、新的 workspace crate、新的外部服务
- 为不可测代码做超出当前改动路径的重构
- 接受/更新 golden file 或固定 fixture 的期望值，且行为含义发生变化
- 关闭 clippy lint、新增 `#[allow]`

**Always**
- 改遗留路径前：先写特征测试，锁定当前可观察行为（允许丑，必须可重复）
- 新行为：先有会失败的行为断言，再写最少实现
- 难以测试时：先造接缝，再写测试（见「遗留代码与接缝」）
- 测试名描述行为：`should_reject_negative_amount`
- 现有测试因你的改动失败：修实现，不修测试（除非人类明确要求）

测试权限：

| 测试来源 | 权限 |
|---|---|
| 人类已有测试 | 只读 |
| 本任务新建测试 | 可改，直到该行为稳定 |
| 过时或环境偶发失败 | 只报告，不擅自跳过 |

### 工作流

**Red** — 写生产行为之前先写测试；测试必须能被收集且必须失败（断言失败，或因缺失 API 导致编译失败，二者都算合法 Red）。修改已有功能先写特征测试锁定当前输出。一次只加一个行为的测试。

**Green** — 只写让当前失败测试通过的最少代码。禁止删掉/改掉失败测试、一次引入多个未验证变更、用更宽断言或 `unwrap()` 换绿。

**Refactor** — 相关测试全绿后才重构；重构后立刻跑同一组测试；范围限于当前 crate。

**探索 vs 实现** — 需求或方案不清可写草稿验证；草稿不得合并；方案确定后必须走 TDD 重写。

### 遗留代码与接缝

**特征测试** — 锁定现有行为，不是证明它正确。本仓库**未引入 `insta`**，用固定 fixture + 显式断言，或与 golden 文件逐字比对。更新期望值必须在汇报里写清 diff 含义；默认不接受「看起来差不多」。

**接缝（优先顺序，靠后的更差）**
1. trait + 泛型或 `impl Trait`，测试用假类型
2. 用类型去掉非法状态（enum / newtype），而不是在测试里补分支
3. 时钟、ID、熵、文件系统做成可注入依赖；测试用 `tempfile` / 内存实现
4. `unsafe` 不是接缝。新增 `unsafe` 必须 Ask first，并写 `SAFETY` 注释

只给即将修改的代码路径补测试，不要一次性给整个模块「补全覆盖率」。

### 测试分层

| 层级 | 位置 | 测什么 |
|---|---|---|
| 单元 | `src` 内 `#[cfg(test)] mod tests` | 模块不变量、错误类型、状态转换 |
| 集成 | `tests/*.rs` | 公共 API；不可访问私有项 |
| 文档测试 | `///` 示例 | 公共 API 必须可运行；禁止滥用 `no_run` |
| CLI/二进制 | 项目惯用方式 | 退出码与 stdout 契约 |
| 不变量 | `proptest`（项目已用时） | 往返解析、幂等、单调性 |
| 特征/golden | 固定 fixture（本仓库未引入 `insta`） | 遗留输出；更新期望值必须说明 |

不要把本该测公共契约的内容塞进 `#[cfg(test)]` 去读私有字段。

Rust 的 Red 允许是：测试引用了尚不存在的类型/函数导致编译失败。不要为了先编译而写空 `todo!()` 实现再补测试——可以留 `todo!()` 仅作为 Green 的最小占位，且下一步必须替换。

### Rust Never 补遗
- 库代码（非 main/example/测试）用 `unwrap` / `expect` / `panic!` 做控制流
- 无必要 `unsafe`；有则必须 `SAFETY` 注释
- 一次性 `cargo update` 整个 lockfile
- 用 `#[allow(...)]` 静默应修复的 lint
- 为绿而改 golden/期望值却不解释行为是否应该变

### 命令

```bash
# 单测（按测试名过滤）
cargo test --all <test_name>

# 单元测试（无需 DB）
cargo test --all

# Oracle 单元测试（`oracle` 已在 default features 里，此行只是显式说明）
cargo test --all --features oracle

# synth 单元测试（无需 DB；synth 自 0.5.0 起在 default features 中）
cargo test --all

# MySQL 集成测试（需运行 MySQL + 环境变量）
HEPTA_DBCLI_TEST_URL=mysql://mcp:testpass@127.0.0.1:3306/testdb cargo test --all --features integration

# Oracle 集成测试（需运行 Oracle）
POLARDB_ORACLE_TEST_URL=oracle://system:testpass@127.0.0.1:1521/FREEPDB1 cargo test --all --features "oracle,integration" -- oracle

# 提交前门禁（与 .github/workflows/ci.yml 一致，顺序：fmt → clippy → test，勿跳过 clippy）
cargo fmt --all -- --check
cargo clippy --all --all-targets
cargo test --all
cargo test --all --features integration   # CI 也跑这条；无 DB 时集成用例会自行跳过
```

根 `Cargo.toml` 是虚拟 manifest（`members = ["dbcli"]`，package 名 `polar-mysql`），所以带 feature 的命令都写成 `--all --features ...` 的形式。

> CI 的 clippy 没有 `-D warnings`；本文件仍要求 warning 清零，不要因为「CI 绿」就放过。

### 完成标准与汇报

提交或交还人类前，确认：
- [ ] 新行为有失败→通过的测试
- [ ] 修改的遗留路径有特征测试
- [ ] 未删除、跳过、改写人类已有测试
- [ ] 已跑与改动匹配的门禁（fmt + clippy + test）
- [ ] `cargo fmt` 与 clippy 干净
- [ ] 没有把草稿、调试输出、无主 lockfile 大面积变更带上

每个 TDD 循环汇报：
1. 测试了什么行为（测试函数名）
2. 最小实现改了哪些文件
3. 是否重构、边界在哪
4. 实际执行的命令和结果（通过 / 失败原因；不要只写「测过了」）

### 质量判断（自我检查）
- 这条测试在实现写错时会失败吗？
- 我是否在测行为，而不是私有实现细节？
- 我是否用 skip、更宽断言、unwrap、golden 盲收换绿？
- 命令是否来自本文件，而不是我编的？

## Build & Dev Commands

```bash
# Build (debug, default: oracle-rs + oracle + gaussdb + synth)
cargo build

# Build release (same default features)
cargo build --release -p polar-mysql

# Format check
cargo fmt --all -- --check

# Clippy (requires libdbus-1-dev on Ubuntu)
sudo apt-get install -y libdbus-1-dev pkg-config
cargo clippy --all --all-targets

# Unit tests
cargo test --all

# Integration tests (require running MySQL)
HEPTA_DBCLI_TEST_URL=mysql://mcp:testpass@127.0.0.1:3306/testdb cargo test --all --features integration

# Oracle tests (unit)
cargo test --features oracle

# Oracle integration tests (require running Oracle)
POLARDB_ORACLE_TEST_URL=oracle://system:testpass@127.0.0.1:1521/FREEPDB1 cargo test --features "oracle,integration" -- oracle
```

**CI order matters**: `cargo fmt --check` → `cargo clippy` → `cargo test` (do NOT skip clippy).

## Architecture

```
dbcli/                          # Cargo workspace root
├── Cargo.toml                  # workspace: members = ["dbcli"]
├── dbcli/
│   ├── Cargo.toml              # bin crate, name = "hepta_dbcli" (package = "polar-mysql")
│   └── src/
│       ├── main.rs             # CLI arg parsing (clap), entrypoint + MCP server bootstrap
│       ├── cli.rs              # SQL execution, output rendering, read-only enforcement
│       ├── config.rs           # TOML config parsing, URL building (mysql:// + oracle://), keyring
│       ├── server.rs           # MCP server via rmcp: DbMcp with 7 tools (multi-backend)
│       ├── interactive.rs      # REPL mode: rustyline + SQL tokenizer (MySQL/Oracle aware)
│       ├── output.rs           # Table formatting (type mapping moved to backend/)
│       ├── queries.rs          # Legacy: MySQL SQL strings (still used by check command)
│       ├── connection.rs       # Legacy: MySQL connection helpers (still used by check command)
│       ├── logger.rs           # Tracing to ~/.local/share/hepta-dbcli/hepta-dbcli.log (daily)
│       └── backend/            # Multi-database abstraction layer
│           ├── mod.rs          # DbPool, DbConn, Dialect, BackendFactory traits + QueryResult
│           ├── error.rs        # DbError, DbErrorKind
│           ├── factory.rs      # BackendRegistry (scheme → factory routing)
│           ├── mysql/          # MySQL backend
│           │   ├── mod.rs      # MySqlFactory
│           │   ├── pool.rs     # MySqlPool (wraps mysql_async::Pool)
│           │   ├── conn.rs     # MySqlConn (wraps mysql_async::Conn)
│           │   ├── dialect.rs  # MySqlDialect (information_schema queries)
│           │   └── types.rs    # mysql_async::ColumnType → serde_json::Value
│           └── oracle/         # Oracle backend (feature-gated: --features oracle)
│               ├── mod.rs      # OracleFactory
│               ├── pool.rs     # OraclePool + oracle:// URL parser
│               ├── conn.rs     # OracleConn (wraps oracle_rs::Connection)
│               ├── dialect.rs  # OracleDialect (ALL_TABLES, SYS_CONTEXT, LISTAGG)
│               └── types.rs    # oracle_rs::Row → serde_json::Value
│       ├── synth/              # Synthetic data generation (default feature `synth` since 0.5.0, CLI-only)
│           ├── mod.rs          # synth::run dispatcher; config/connection resolution (default_connection aware)
│           ├── cmd.rs          # clap SynthArgs + build_model (marginals + PIT/Pearson copula)
│           ├── graph.rs        # Topological sort (Kahn) + Tarjan SCC; edge (from,to) = from first
│           ├── marginal.rs     # Marginals: Normal/Beta/Gamma/Categorical/Uniform (real CDF+PPF, bisection inverse)
│           ├── copula.rs       # GaussianCopula: Cholesky + PSD projection + correlated sampling
│           ├── model.rs        # TableModel JSON (version-checked load)
│           ├── profile.rs      # Column/table stats incl. top_values + column_order
│           ├── rules.rs        # YAML rules parser (!projection/!generated/!fixed pool strategies)
│           ├── rules_draft.rs  # FK-driven rules draft (profiles feed unique-FK detection)
│           ├── fk_pool.rs      # FK pools: uniform/zipf selection, sample_unique (without replacement)
│           ├── generator.rs    # Engine: per-table seeds (djb2), FK integrity via table.column pools
│           ├── export.rs       # CSV/JSONL/JSON/SQL with real column names + quoted identifiers
│           └── report.rs       # Synthesis report (library-only; not wired to CLI yet)
│       └── duckdb/         # DuckDB backend (feature-gated: --features duckdb)
│           ├── mod.rs      # DuckDbFactory
│           ├── pool.rs     # DuckDbPool + duckdb:// URL parser (embedded, open once + try_clone)
│           ├── conn.rs     # DuckDbConn (sync driver wrapped in spawn_blocking)
│           ├── dialect.rs  # DuckDbDialect (duckdb_tables/views/indexes, information_schema)
│           └── types.rs    # duckdb::types::ValueRef → serde_json::Value
└── .github/workflows/
    ├── ci.yml                  # PR/push: fmt, clippy, test (MySQL 8 service container)
    └── release-build.yml       # Tag push: linux-x86_64, linux-arm64, windows-x86_64 (--features oracle-rs,oracle,gaussdb,synth)
```

**Dual mode**: The binary defaults to MCP server (`hepta_dbcli` with no subcommand). Use `hepta_dbcli cli` for one-shot SQL or `hepta_dbcli cli --interactive` for REPL.

## Key Dependencies

| Crate | Purpose |
|-------|---------|
| `mysql_async` 0.37 | Async MySQL driver (rustls-tls, ring crypto) |
| `oracle-rs` 0.1 | Pure Rust Oracle driver (optional, `--features oracle`) |
| `rmcp` 1.5 | MCP server framework (stdio transport) |
| `clap` 4 | CLI argument parsing |
| `keyring` 3 | OS keychain for password storage |
| `rustyline` 18 | Interactive REPL |
| `tracing` | Structured logging |
| `async-trait` 0.1 | Async trait support |
| `rand` 0.8 + `rand_distr` 0.4 | RNG for synth (default feature) |
| `serde_yaml` 0.9 | synth rules YAML (default feature) |

## Multi-Database Abstraction

The `backend/` module defines three traits that every database backend implements:

```rust
trait DbPool: Send + Sync { async fn acquire(&self) -> Result<Box<dyn DbConn + Send>>; }
trait DbConn: Send { async fn query(&mut self, sql: &str) -> Result<QueryResult>; fn dialect(&self) -> &dyn Dialect; }
trait Dialect: Send + Sync { fn database_info(&self) -> &str; fn add_limit(&self, sql: &str, n: usize) -> String; ... }
```

Adding a new database requires implementing these three traits + a `BackendFactory` — all consumers (server, cli, interactive) work without changes.

## Quirks & Gotchas

### Connection Config
- Config file: `~/.hepta-dbcli.toml` (TOML format, also reads legacy `~/.polardb-mysql.toml`)
- Env var override: `HEPTA_DBCLI_URL=mysql://user:pass@host:port/db`
- Multi-connection support via `[connections.NAME]` sections in TOML
- **`driver` field**: set `driver = "oracle"` for Oracle connections (defaults to `"mysql"`)
- Password stored in OS keychain (macOS Keychain, Linux Secret Service). Plaintext passwords in config are **auto-migrated** to keychain on first successful connection.

### Oracle Connection Example

```toml
default_connection = "dev"

[connections.dev]
host = "127.0.0.1"
user = "root"
password = "keyring"

[connections.ora]
driver = "oracle"
host = "oracle.internal"
port = 1521
user = "scott"
password = "keyring"
database = "FREEPDB1"
```

Or use URL directly:
```toml
[connections.ora]
url = "oracle://scott:tiger@oracle.internal:1521/FREEPDB1"
```

### URL Scheme Detection
The `BackendRegistry` routes connections by URL scheme:
- `mysql://...` → `MySqlFactory`
- `oracle://...` → `OracleFactory`
- `gaussdb://...` → `GaussdbFactory`
- `duckdb://...` → `DuckDbFactory`
- Config field `driver = "oracle"` / `"gaussdb"` / `"duckdb"` also routes accordingly

### Password Flow
When a connection has `password = "keyring"` (sentinel value), the system reads from OS keychain. The migration rewrites the config file replacing the password with `"keyring"`. This is important: do NOT commit config files with plaintext passwords to git.

**Keyring key structure**: The OS keychain stores passwords under two identifiers:
- **Service**: `hepta-dbcli` (was `polar-mysql` in versions ≤ 0.2.7)
- **Account**: `{connection_name}#{8_hex_chars}` — the hex suffix is a djb2 hash of the canonical config file path, which disambiguates identically-named connections across different config files

On macOS, this appears in Keychain Access as `hepta-dbcli (dev#a3f9b2c1)`. On Linux, it's stored in the Secret Service.

**Migration from old keyring**: When `read_keyring_password` fails to find an entry under the new service name (`hepta-dbcli`), it falls back to the old service name (`polar-mysql`) with the old account format (`{user}/{name}`). On success, it auto-migrates the password to the new key and logs a warning if migration fails. This ensures smooth upgrades from ≤ 0.2.7.

### Naming Conventions

The project was renamed from `polar-mysql` to `hepta_dbcli`. All new code must use the current names:

| Category | Current | Legacy (≤ 0.2.7) |
|----------|---------|-------------------|
| Binary | `hepta_dbcli` | `polar-mysql` |
| Config file | `~/.hepta-dbcli.toml` | `~/.polardb-mysql.toml` |
| Env var (URL) | `HEPTA_DBCLI_URL` | `POLARDB_MYSQL_URL` |
| Env var (password) | `HEPTA_DBCLI_PASSWORD` | `POLARDB_MYSQL_PASSWORD` |
| Test env var | `HEPTA_DBCLI_TEST_URL` | `POLARDB_MYSQL_TEST_URL` |
| Keyring service | `hepta-dbcli` | `polar-mysql` |
| Keyring account | `{name}#{path_hash}` | `{user}/{name}` |
| Logger dir | `~/.local/share/hepta-dbcli/` | — |
| History dir | `~/.local/share/polar-mysql/history/` | — |

**Backward compatibility rules**:
- Config file: `~/.hepta-dbcli.toml` is tried first; `~/.polardb-mysql.toml` is tried as fallback
- Keyring: new service+account tried first; old service+account as fallback with auto-migration
- Oracle test env var `POLARDB_ORACLE_TEST_URL` has NOT been renamed (Oracle-specific, less widespread)
- Package name in Cargo.toml remains `polar-mysql` (separate from binary name)
- Crate name remains `polar_mysql` (Rust naming convention)

### Testing
- Unit tests are **inline** (`#[cfg(test)] mod tests { ... }`) in each source file.
- Regression/integration suites live in `tests/` (`regress_mysql.rs`, `regress_oracle.rs`, `regress_gaussdb.rs`, `regress_duckdb.rs`) behind the `integration` feature flag (plus their backend feature).
- MySQL integration tests require `HEPTA_DBCLI_TEST_URL` env var and a running MySQL instance.
- Oracle integration tests require `POLARDB_ORACLE_TEST_URL` env var and a running Oracle instance (Docker: `gvenzl/oracle-free:23-slim`).
- DuckDB integration tests are **embedded** — no external service, no env var; they use `tempfile` fixtures (`cargo test --features "duckdb,integration" --test regress_duckdb`).
- CI has a dedicated **`duckdb` job** (`cargo clippy --all --all-targets --features duckdb`, `cargo test --all --features duckdb`, `cargo test --features "duckdb,integration" --test regress_duckdb`). It is slow (bundled C++ core), so run the same commands locally when touching `backend/mod.rs` traits (`DbConn`, `QueryResult`, `BackendFactory`) instead of waiting for CI.

### MCP Server
- Runs on **stdio** (not HTTP/WebSocket). Intended to be spawned by MCP clients (e.g., Claude, Cursor).
- MCP tools (7): `get_database_info`, `list_tables`, `get_table_metadata`, `execute_query`, `get_execution_plan`, `list_connections`, `delta_diff`.
- `delta_diff` is read-only: compare + csv/jsonl/json export + checkpoint + incremental. SQL patch (`--apply-to`) stays on the CLI.
- All tool calls from MCP enforce **read-only**: only SELECT, EXPLAIN, SHOW, DESCRIBE, DESC are allowed (MySQL). Oracle/GaussDB only allow SELECT, EXPLAIN, WITH. DuckDB allows SELECT, EXPLAIN, WITH, SHOW, DESCRIBE, DESC, SUMMARIZE.
- `execute_query` tool appends `LIMIT N` (MySQL) or `FETCH FIRST N ROWS ONLY` (Oracle 12c+) — dialect-specific. DuckDB appends `LIMIT N` only to SELECT/WITH-shaped statements.
- `get_execution_plan` uses `EXPLAIN FORMAT=JSON` (MySQL) or `EXPLAIN PLAN ... DBMS_XPLAN` (Oracle).
- Connection pooling: connections are reused and recycled based on `connection_max_lifetime`.
- Write control is layered (issue #58): MCP stays read-only; **CLI/REPL data changes require `--allow-write`** (`INSERT`/`UPDATE`/`DELETE`/`CALL`), destructive DDL (`DROP`/`TRUNCATE`/`ALTER`/`CREATE`/`GRANT`) is always refused client-side. Classification lives in `cli.rs::classify_statement` / `write_gate` (UX gate, not a security boundary). `--allow-write mcp` exits 2. The audit ledger is not optional (no `--no-audit`); an unusable audit directory degrades with a warning for reads and fails closed for writes. The gate runs after connection resolution and before connecting, and a refusal is audited (`decision=deny`, `deny_reason` = `write_flag_required` / `destructive_ddl`) without touching the engine. Each CLI/REPL session records a `session_mode` event (`read_only` / `allow_write`); writes record an intent before execution and an outcome after, so an intent event must not be read as "executed".

### CI
- `libdbus-1-dev` and `pkg-config` are system dependencies for `clippy` and `test`. Without them, `cargo clippy` will fail on the `keyring` crate.
- Jobs: `rustfmt`, `clippy`, `duckdb`, `test`. The `test` job runs a MySQL service and `HEPTA_DBCLI_TEST_URL`; the `duckdb` job runs the feature-gated clippy + unit + embedded DuckDB suites.
- Release builds on Windows link statically (`-C target-feature=+crt-static`).
- Release tags: `v*` (e.g. `v0.2.1`).
- Release binaries are built with `--features oracle-rs,oracle,gaussdb,synth` (not `--all-features`; `integration` and `duckdb` stay out).

### Style Conventions
- Section headers use `// ─── ... ───` style.
- `pub(crate)` visibility throughout, not `pub`.
- `use` statements organized: std → external crates → crate modules.
- Rust edition 2021, workspace resolver v2.
