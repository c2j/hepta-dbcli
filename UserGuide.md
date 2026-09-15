# hepta_dbcli 用户指南

当前版本：**0.5.0**。CLI + MCP Server，覆盖 MySQL / PolarDB-X / Oracle / GaussDB / DuckDB，并提供跨库表数据比对（`delta-diff`）。

## 目录

1. [安装](#1-安装)
2. [快速开始](#2-快速开始)
3. [配置详解](#3-配置详解)
4. [命令行模式](#4-命令行模式-cli)
5. [交互模式](#5-交互模式-repl)
6. [连接检查](#6-连接检查-check)
7. [密码管理](#7-密码管理)
8. [MCP 服务器模式](#8-mcp-服务器模式)
9. [跨库比对](#9-跨库比对-delta-diff)
10. [合成数据生成](#10-合成数据生成-synth)
11. [进阶用法](#11-进阶用法)
12. [错误排查](#12-错误排查)

---

## 1. 安装

### 二进制下载

从 [GitHub Releases](https://github.com/c2j/hepta-dbcli/releases) 下载对应平台的预编译二进制：

- `hepta_dbcli-{version}-x86_64-unknown-linux-gnu.zip`（glibc 2.28+，如 Debian 10 / RHEL 8 / Ubuntu 20.04）
- `hepta_dbcli-{version}-aarch64-unknown-linux-gnu.zip`
- `hepta_dbcli-{version}-x86_64-pc-windows-msvc.zip`

macOS 暂无预编译包，请从源码编译。

### 源码编译

```bash
git clone https://github.com/c2j/hepta-dbcli.git
cd hepta-dbcli
cargo build --release -p polar-mysql
# 二进制位于: target/release/hepta_dbcli
```

默认 feature 已包含 `oracle-rs`、`oracle`、`gaussdb`、`synth`（自 0.5.0）。GitHub Release 预编译包与此一致。连接 Oracle 11g 时会回退到 `oracle` crate，需要本机安装 [Oracle Instant Client](https://www.oracle.com/database/technologies/instant-client.html) 并配置动态库路径。12c+ 走纯 Rust 的 `oracle-rs`，无需 Instant Client。若不需要 synth：`cargo build --release -p polar-mysql --no-default-features --features "oracle-rs,oracle,gaussdb"`。

DuckDB 为可选 feature（不默认编译）：

```bash
cargo build --release -p polar-mysql --features duckdb
```

`bundled` 特性会从源码编译 DuckDB C++ 内核（首次构建需数分钟，要求 C++ 工具链）。注意：bundled 构建不含 ICU 扩展，`now() - interval '1 day'` 等日期运算需运行时 `INSTALL icu; LOAD icu;`。

验证安装：

```bash
$ hepta_dbcli --version
hepta_dbcli 0.5.0
```

---

## 2. 快速开始

最简单的使用方式是通过环境变量连接数据库：

```bash
export HEPTA_DBCLI_URL="mysql://root:password@127.0.0.1:3306/mysql"
```

然后直接执行 SQL：

```bash
$ hepta_dbcli cli --sql "SELECT VERSION() AS version, DATABASE() AS db, CURRENT_USER() AS user"
┌─────────┬───────┬────────────────┐
│ version │ db    │ user           │
├─────────┼───────┼────────────────┤
│ 8.4.10  │ mysql │ root@localhost │
└─────────┴───────┴────────────────┘
(1 row)
```

Oracle / GaussDB 同样可以用 URL：

```bash
export HEPTA_DBCLI_URL="oracle://scott:tiger@127.0.0.1:1521/FREEPDB1"
export HEPTA_DBCLI_URL="gaussdb://gaussdb:secret@127.0.0.1:5432/testdb?sslmode=disable"
export HEPTA_DBCLI_URL="duckdb:///data/analytics/shop.duckdb"
hepta_dbcli cli --sql "SELECT 1"
```

URL 中的密码若含 `@`、`:` 等字符，需要百分号编码（`@` → `%40`）。

> **提示**：推荐使用配置文件管理连接（见下一节），密码会自动迁移到系统钥匙串。

---

## 3. 配置详解

### 3.1 配置文件位置

- 默认路径：`~/.hepta-dbcli.toml`
- 自定义路径：通过 `--config <PATH>` 指定
- 环境变量：`HEPTA_DBCLI_URL`（优先级最高）
- 兼容旧路径：若不存在新文件，会尝试读取 `~/.polardb-mysql.toml`

### 3.2 单连接配置

创建 `~/.hepta-dbcli.toml`：

```toml
host = "127.0.0.1"
port = 3306
user = "root"
password = "your-password"
database = "mysql"
```

首次连接成功后，密码会自动迁移到操作系统钥匙串（macOS Keychain / Linux Secret Service），配置文件中的 `password` 会被改写为 `"keyring"`：

```toml
database = "mysql"
host = "127.0.0.1"
password = "keyring"
port = 3306
user = "root"
```

### 3.3 多连接配置

使用 `[connections.NAME]` 语法配置多个数据库连接。**连接名取自 TOML 段名**（`[connections.dev]` → `--name dev`），段内不需要再写 `name` 字段。

```toml
default_connection = "dev"

[connections.dev]
host = "127.0.0.1"
port = 3306
user = "root"
password = "keyring"
database = "mydb"

[connections.prod]
host = "prod-db.example.com"
port = 3306
user = "readonly"
password = "keyring"
database = "mydb"
statement_timeout = "60s"
connection_max_lifetime = "30min"
```

使用 `--name` 选择连接：

```bash
hepta_dbcli cli --name prod --sql "SELECT COUNT(*) FROM orders"
```

### 3.4 Oracle / GaussDB / DuckDB 连接

`driver` 决定 URL scheme 与默认端口。省略时为 `mysql`。

| driver | scheme | 默认端口 |
|--------|--------|----------|
| `mysql`（默认） | `mysql://` | 3306 |
| `oracle` | `oracle://` | 1521 |
| `gaussdb` | `gaussdb://` | 5432 |
| `duckdb` | `duckdb://` | 无（嵌入式） |

`database` 同时接受别名 `dbname`（方便 GaussDB / PostgreSQL 习惯）。

**DuckDB 是嵌入式数据库**：没有 host/port/user/password，`database` 字段填数据库文件路径或 `:memory:`；password 字段（含 `keyring`）一律忽略。文件不存在时**直接报错**，不会静默创建。URL 加 `?mode=ro` 以只读方式打开（多进程可并发读同一文件）。

```toml
default_connection = "mysql_dev"

[connections.mysql_dev]
host = "127.0.0.1"
port = 3306
user = "root"
password = "keyring"
database = "shop"

[connections.ora_dev]
driver = "oracle"
host = "127.0.0.1"
port = 1521
user = "system"
password = "keyring"
database = "FREEPDB1"

[connections.gauss_dev]
driver = "gaussdb"
host = "127.0.0.1"
port = 5432
user = "gaussdb"
password = "keyring"
database = "testdb"
sslmode = "disable"
```

也可以直接写 URL：

```toml
[connections.ora_dev]
url = "oracle://system:testpass@127.0.0.1:1521/FREEPDB1"

[connections.gauss_dev]
url = "gaussdb://gaussdb:secret@127.0.0.1:5432/testdb?sslmode=disable"
```

Oracle 连接顺序：先试纯 Rust 的 `oracle-rs`（12c+），失败再回退到需要 Instant Client 的原生驱动（11g+）。

### 3.5 环境变量模式

适合临时使用或脚本场景：

```bash
export HEPTA_DBCLI_URL="mysql://user:password@host:port/database"
# 或只把密码放环境变量
export HEPTA_DBCLI_PASSWORD="secret"
```

使用环境变量时，连接名固定为 `default`，不使用钥匙串。

### 3.6 超时设置

| 参数 | 说明 | 默认值 |
|------|------|--------|
| `statement_timeout` | 单条 SQL 最大执行时间 | `30s` |
| `connection_max_lifetime` | 连接最大存活时间，超时后自动回收 | `1h` |

支持单位：`ms`（毫秒）、`s`（秒）、`min`（分钟）、`h`（小时），或直接写数字（按秒计）。

```toml
# 全局设置
statement_timeout = "30s"
connection_max_lifetime = "1h"

# 也可以按连接覆盖
[connections.slow_db]
host = "..."
user = "..."
password = "keyring"
statement_timeout = "5min"
connection_max_lifetime = "30min"
```

也可通过命令行参数覆盖（仅 CLI 模式）：

```bash
hepta_dbcli cli --statement-timeout "5min" --connection-max-lifetime "10min" --sql "..."
```

`delta-diff` 的查询超时是独立参数 `--statement-timeout`（单位秒，默认 300），与 CLI 的 duration 字符串不是同一套解析。

### 3.7 SSL/TLS 连接

MySQL 在连接 URL 中添加 SSL 参数：

```toml
url = "mysql://user:password@host:3306/db?ssl-mode=REQUIRED"
```

或通过字段方式配置（仅 `require` 模式）：

```toml
host = "127.0.0.1"
port = 3306
user = "root"
password = "keyring"
sslmode = "require"
```

GaussDB 的 `sslmode`：

| 值 | 含义 |
|----|------|
| `disable` | 不加密（本地 / Docker 常用） |
| `require` | 加密，不校验证书 |
| `verify-ca` | 校验 CA，不校验主机名 |
| `verify-full` | 校验 CA + 主机名 |

若 GaussDB 服务器未开 TLS，必须写 `sslmode = "disable"`，否则 `check` 会失败。

---

## 4. 命令行模式 (CLI)

CLI 模式**允许写操作**。只读限制仅作用于 MCP 的 `execute_query`。

### 4.1 基本用法

```bash
hepta_dbcli cli --sql "<SQL语句>"
```

```bash
$ hepta_dbcli cli --sql "SELECT 1 AS one, 2 AS two"
┌─────┬─────┐
│ one │ two │
├─────┼─────┤
│ 1   │ 2   │
└─────┴─────┘
(1 row)
```

### 4.2 从文件读取 SQL

```bash
$ echo "SELECT name, email FROM users ORDER BY id" > query.sql
$ hepta_dbcli cli --file query.sql
```

### 4.3 从标准输入读取 SQL

```bash
$ echo "SELECT COUNT(*) AS total_users FROM users" | hepta_dbcli cli
```

### 4.4 输出格式

支持四种输出格式，通过 `--format` 指定：`table`（默认）、`json`、`csv`、`vertical`。

#### Table（默认）

```bash
$ hepta_dbcli cli --sql "SELECT id, name, email FROM users LIMIT 2" --format table
┌────┬───────┬───────────────────┐
│ id │ name  │ email             │
├────┼───────┼───────────────────┤
│ 1  │ Alice │ alice@example.com │
│ 2  │ Bob   │ bob@example.com   │
└────┴───────┴───────────────────┘
(2 rows)
```

#### JSON

```bash
$ hepta_dbcli cli --sql "SELECT id, name, email FROM users LIMIT 2" --format json
{
  "columns": [
    "id",
    "name",
    "email"
  ],
  "row_count": 2,
  "rows": [
    [
      1,
      "Alice",
      "alice@example.com"
    ],
    [
      2,
      "Bob",
      "bob@example.com"
    ]
  ]
}
```

#### CSV

```bash
$ hepta_dbcli cli --sql "SELECT id, name, email FROM users LIMIT 2" --format csv
id,name,email
1,Alice,alice@example.com
2,Bob,bob@example.com
```

#### Vertical（垂直展示）

```bash
$ hepta_dbcli cli --sql "SELECT id, name, email FROM users LIMIT 1" --format vertical
-[ RECORD 1 ]-
id | 1
name | Alice
email | alice@example.com
(1 row)
```

### 4.5 完整的命令行参数

```bash
$ hepta_dbcli cli --help
Execute SQL from command line

Usage: hepta_dbcli cli [OPTIONS]

Options:
      --config <CONFIG>
          Path to config file
  -s, --sql <SQL>
          SQL statement to execute
  -f, --file <FILE>
          Read SQL from file
      --name <NAME>
          Target connection name
      --check-connection
          Test database connectivity without executing SQL
  -v, --verbose
          Show detailed connection info (use with --check-connection)
      --format <FORMAT>
          Output format: table, json, vertical, csv [default: table]
      --statement-timeout <STATEMENT_TIMEOUT>
          Statement timeout (e.g. "30s", "5min"). Overrides config
      --connection-max-lifetime <CONNECTION_MAX_LIFETIME>
          Connection max lifetime before reconnect (e.g. "10min")
  -i, --interactive
          Enter interactive REPL mode
      --no-history
          Do not read or write persistent per-connection SQL history
      --timeout-action <TIMEOUT_ACTION>
          Timeout action: "cancel" (default, keep connection alive) or
          "disconnect" (recycle connection)
  -h, --help
          Print help
```

全局旗标（`hepta_dbcli --help` 可见，子命令前后都可以给）：

```bash
      --config <CONFIG>
          Path to config file
      --name <NAME>
          Target connection name
      --audit-dir <AUDIT_DIR>
          Directory for the JSONL audit log
          (default: <data-dir>/hepta-dbcli/audit)
      --audit-meta
          Also audit high-noise meta actions (list_tables, check, ...)
      --audit-retention-days <DAYS>
          Audit log retention in days (0 = keep forever) [default: 30]
      --allow-write
          Allow CLI/REPL data changes (INSERT/UPDATE/DELETE and CALL);
          destructive DDL stays refused, MCP stays read-only
```

审计账本默认开启且无法关闭：`--audit-dir` 只改变写入位置，`--audit-meta` 只增加低价值元数据动作。审计目录不可写时降级并在 stderr 警告，只读查询照常执行；写路径（见 §4.6）则 fail-closed。

### 4.6 写入模式（`--allow-write`）

> ⚠️ **破坏性变更**：MySQL / Oracle 的 CLI 会话以前接受裸 `INSERT`，现在与 GaussDB 一样先被客户端拒绝。这是刻意的分层设计，不是回归。

| 层 | 语句 | CLI / REPL | MCP |
|---|---|---|---|
| L1 只读 | `SELECT` / `SHOW` / `EXPLAIN` / `DESCRIBE` / `SET` / 事务控制 | 直接执行 | 允许 |
| L2 数据变更 | `INSERT` / `UPDATE` / `DELETE` / `REPLACE` / `MERGE` / `CALL` | 需要 `--allow-write` | 拒绝 |
| L3 破坏性 | `DROP` / `TRUNCATE` / `ALTER` / `CREATE` / `GRANT` / `REVOKE` / `RENAME` | **始终拒绝** | 拒绝 |

```bash
hepta_dbcli cli --sql "INSERT INTO t VALUES (1)"                 # 被拒
hepta_dbcli cli --allow-write --sql "INSERT INTO t VALUES (1)"   # 执行，输出 "1 rows affected"
hepta_dbcli cli --allow-write --sql "CALL foo(1)"                # 执行
hepta_dbcli cli --allow-write --sql "DROP TABLE t"               # 仍被拒
hepta_dbcli --allow-write mcp                                    # 退出码 2（MCP 只读）
```

要点：

- **分类是启发式**（`cli.rs::classify_statement`），定位是 UX 门而非安全边界；真正的边界是数据库账号，写模式请配低权限用户。
- **拒绝也留痕**：被拒语句在账本里记 `decision=deny` + `deny_reason`（`write_flag_required` / `destructive_ddl`），且**不连接引擎**，所以「谁试图写」事后可查。
- **写路径 fail-closed**：写之前先落一条无 `outcome` 的 intent 事件，写不进去就拒绝执行；执行后再落一条带 `rows_affected` 的 outcome。查询账本时不要把 intent 当成已执行。
- **GaussDB**：该进程新建连接用显式 `SET default_transaction_read_only = OFF`；MCP 连接始终带只读护栏，不受影响。
- 每个会话开始会记一条 `session_mode`（`read_only` / `allow_write`）。

---

## 5. 交互模式 (REPL)

### 5.1 启动

```bash
$ hepta_dbcli cli --interactive
hepta_dbcli interactive -- connected to 'default'
end SQL with ';' + Enter to execute (multi-line ok) .help .connect .exit
$
```

也可以指定连接：

```bash
hepta_dbcli cli --interactive --name prod
# 或简写
hepta_dbcli cli -i --name gauss_dev
```

### 5.2 基本操作

在 REPL 中输入 SQL，以分号 `;` 结束并按回车执行。支持多行输入（不完整语句自动续行）：

```sql
$ SELECT id, name
. FROM users
. WHERE id < 3;
┌────┬───────┐
│ id │ name  │
├────┼───────┤
│ 1  │ Alice │
│ 2  │ Bob   │
└────┴───────┘
(2 rows)
```

### 5.3 点命令（Dot Commands）

| 命令 | 说明 |
|------|------|
| `.help` / `?` | 显示帮助信息 |
| `.exit` / `.quit` | 退出 REPL |
| `.connect [name]` | 切换到指定连接（不指定则重连当前连接） |
| `.history` | 显示 SQL 执行历史 |
| `.clear` / `.cls` | 清屏 |
| `.output [file]` | 将 SQL 输出重定向到文件（不指定参数恢复 stdout） |
| `.save <file> [format]` | 将上一次查询结果保存到文件，可指定格式 |

#### 示例：输出重定向

```
$ .output /tmp/result.txt
$ SELECT * FROM users;
$ .output
output reset to stdout
```

#### 示例：保存结果

```
$ SELECT * FROM users;
...
(4 rows)

$ .save /tmp/users.csv csv
saved 4 row(s) to /tmp/users.csv (csv)
```

### 5.4 REPL 选项

```bash
# 禁用历史记录（不读写历史文件）
hepta_dbcli cli --interactive --no-history

# 指定超时断开行为
hepta_dbcli cli --interactive --timeout-action disconnect
```

- `--no-history`：不保存也不读取 SQL 历史记录
- `--timeout-action cancel`（默认）：超时后保持连接
- `--timeout-action disconnect`：超时后断开并回收连接

---

## 6. 连接检查 (Check)

### 6.1 MySQL：三种 TLS 探测

```bash
$ hepta_dbcli check
Connection: default

[Keyring] Password read from OS keychain (user: default#a3f9b2c1)
  Keyring accessible, password retrieved (8 chars)

[1/3] Connecting without TLS (plain TCP) ...
  ✓ NoTls  — 5ms  8.4.10
[2/3] Connecting with TLS (skip cert verify) ...
  ✓ TLS(skip-verify)  — 7ms  8.4.10
[3/3] Connecting with TLS (verify cert) ...
  ✗ TLS(verify)  — FAILED: Connection failed: ... TLS error ...

  ✓ Connection successful (mode: NoTls)
  Database Version: 8.4.10
```

MySQL 的 `check` 会自动尝试三种连接方式：

1. **无 TLS**（plain TCP）
2. **TLS（跳过证书验证）**
3. **TLS（验证证书）**

### 6.2 Oracle / GaussDB

Oracle 与 GaussDB 各做一次连接尝试（Oracle 内部仍会 `oracle-rs` → Instant Client 回退）：

```bash
hepta_dbcli check --name ora_dev
hepta_dbcli check --name gauss_dev --verbose
```

GaussDB `--verbose` 额外打印 `version()`、`current_database()`、`current_user()`、服务器地址。若认证失败，会提示钥匙串命名空间与独立 `gaussdb` CLI **不共享**（本工具 service 为 `hepta-dbcli`）。

### 6.3 DuckDB

DuckDB 为嵌入式打开（无网络服务），一次连接尝试：

```bash
hepta_dbcli check --name duck --verbose
```

`--verbose` 额外打印 `version()`、`current_database()`。常见失败：文件不存在（不会隐式创建）、文件被其它进程占用写锁（单写者；可改用 `?mode=ro` 并发读）。

### 6.4 详细检查（MySQL）

```bash
hepta_dbcli check --verbose
```

`--verbose` 模式额外显示：服务器版本、当前用户、当前数据库、字符集、排序规则、连接耗时。

### 6.5 通过 CLI 子命令检查

```bash
hepta_dbcli cli --check-connection
hepta_dbcli cli --check-connection --verbose
```

效果等价于 `hepta_dbcli check`。

---

## 7. 密码管理

### 7.1 手动存储密码

```bash
$ hepta_dbcli store-password
Enter password:
Confirm password:
Password stored in OS keychain for 'default#a3f9b2c1' (connection: 'default').
```

```bash
# 为指定连接存储密码
$ hepta_dbcli store-password --name prod
Enter password:
Confirm password:
Password stored in OS keychain for 'prod#a3f9b2c1' (connection: 'prod').
```

钥匙串条目：

- **Service**：`hepta-dbcli`（≤ 0.2.7 为 `polar-mysql`，读取时自动迁移）
- **Account**：`{连接名}#{8 位十六进制}`，后缀是配置文件路径的 djb2 hash，用来区分不同配置文件里的同名连接

macOS「钥匙串访问」里显示为 `hepta-dbcli (dev#a3f9b2c1)`。Linux 用 Secret Service。

### 7.2 自动迁移

首次使用含明文密码的配置文件成功连接数据库后，系统会自动：

1. 将密码存入操作系统钥匙串
2. 将配置文件中的 `password` 字段改写为 `"keyring"`

整个过程透明，无需手动操作。

### 7.3 密码读取优先级

1. 环境变量 `HEPTA_DBCLI_PASSWORD`（配合 `HEPTA_DBCLI_URL`）
2. 配置文件明文密码（首次连接后自动迁移到钥匙串）
3. 操作系统钥匙串（配置文件 `password = "keyring"`）

---

## 8. MCP 服务器模式

### 8.1 启动

不带子命令即为 MCP 服务器模式：

```bash
# 默认模式
hepta_dbcli

# 显式指定
hepta_dbcli mcp

# 指定配置文件
hepta_dbcli --config /path/to/config.toml
```

MCP 服务器通过 **stdio** 协议与 MCP 客户端（如 Claude Desktop、Cursor）通信。

### 8.2 提供的工具

| 工具 | 说明 |
|------|------|
| `get_database_info` | 获取服务器版本、当前用户、字符集、操作系统等 |
| `list_tables` | 列出所有用户表/视图（含引擎类型、行数、数据大小） |
| `get_table_metadata` | 获取指定表的列类型、是否可空、默认值、索引信息 |
| `execute_query` | 执行只读 SQL（按方言前缀校验） |
| `get_execution_plan` | 获取 SQL 执行计划（EXPLAIN / EXPLAIN ANALYZE；Oracle 走 EXPLAIN PLAN + DBMS_XPLAN） |
| `list_connections` | 列出所有配置的连接及其状态 |
| `delta_diff` | 跨库表比对（只读）。支持增量（`update_column`/`update_since`）、`checkpoint`、csv/jsonl/json `export`。SQL 补丁 `--apply-to` 仅 CLI |

`execute_query` 自动追加行数限制：MySQL / GaussDB 为 `LIMIT N`，Oracle 12c+ 为 `FETCH FIRST N ROWS ONLY`，Oracle 11g 为 `ROWNUM`，DuckDB 仅对 `SELECT` / `WITH` 形语句追加 `LIMIT N`（`SHOW` / `DESCRIBE` / `SUMMARIZE` 不追加）。`max_rows` 默认 1000，上限 10000。

### 8.3 安全限制

- **只读执行**（按方言）：
  - MySQL / PolarDB-X：`SELECT`、`EXPLAIN`、`SHOW`、`DESCRIBE`、`DESC`
  - Oracle / GaussDB：`SELECT`、`EXPLAIN`、`WITH`
  - DuckDB：`SELECT`、`EXPLAIN`、`WITH`、`SHOW`、`DESCRIBE`、`DESC`、`SUMMARIZE`（`PRAGMA` 不允许——部分 PRAGMA 有写副作用）
- **行数限制**：默认 `LIMIT 1000`，可通过 `max_rows` 调整（上限 10000）
- **超时控制**：支持按查询设置 `timeout_ms`
- **delta_diff 只读**：可导出 csv/jsonl/json 与写 checkpoint 文件；不会对数据库执行 DML。生成 SQL 补丁请用 CLI `--export *.sql --apply-to`。注意：`export` / `checkpoint` 路径由调用方任意指定，MCP 不做路径白名单

### 8.4 `delta_diff` 工具参数

必填：`left_connection`、`right_connection`、`table`。

可选：`left_table` / `right_table`、`schema` / `left_schema` / `right_schema`、`key_columns`、`columns`、`where_condition`、`strategy`（`auto` / `hashdiff` / `joindiff` / `bucketdiff` / `iblt` / `keyeddiff`）、`consistency`（`snapshot` / `none`）、`recheck`、`sample_limit`（默认 1000）、`summary_only`、`update_column` / `update_since`（增量窗口；`update_since` 默认 `"1 day"`，须与 `update_column` 同用，且与 `where_condition` 互斥）、`checkpoint`（JSONL 断点文件路径）、`export`（导出文件路径，后缀推断 csv/jsonl/json）、`export_format`（显式指定 csv/jsonl/json；`sql` 被拒绝——SQL 补丁仍需 CLI `--apply-to`）、`export_rows`（默认 `false`，导出内容不含差异行明细）。

`where_condition` 禁止包含分号。`update_column` 与 `where_condition` 互斥，同时提供会被拒绝。MCP 返回的差异样本上限由 `sample_limit` 裁剪（导出文件不受影响，始终全量）；CLI 终端默认只显示 20 行（`--sample`），全量走 `--export`。

### 8.5 Claude Desktop 配置示例

在 `claude_desktop_config.json` 中添加：

```json
{
  "mcpServers": {
    "hepta_dbcli": {
      "command": "/usr/local/bin/hepta_dbcli",
      "args": ["--config", "/home/user/.hepta-dbcli.toml"]
    }
  }
}
```

---

## 9. 跨库比对 (delta-diff)

比较两个已配置连接上的同名（或异名）表数据。默认 `auto` 策略 + `snapshot` 一致性 + 差异复核。比对过程对数据库只读；生成 SQL 补丁只写本地文件，不会对库执行 DML。

### 9.1 基本用法

```bash
# 左右同表名
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders

# 异名表 / 指定 schema
hepta_dbcli delta-diff --left mysql_dev --right ora_dev \
  --left-table orders --right-table ORDERS \
  --left-schema shop --right-schema SCOTT

# 指定比对键与列（自动发现失败时）
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --key id --columns id,amount,status
```

### 9.2 过滤与增量

`--where` 与 `--update-column` 互斥。`--where` 禁止分号。

```bash
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --where "status = 'PAID'"

hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --update-column updated_at --update-since "1 day"

hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --update-column updated_at --update-since "2026-08-01 00:00:00"
```

`--update-since` 必须搭配 `--update-column`。

### 9.3 策略选择

`--strategy` 默认 `auto`：

| 策略 | `auto` 何时选用 | 说明 |
|------|-----------------|------|
| `bucketdiff` | 无可用主键，或左右键无法 1:1 配对 | 按行内容做多重集合比对，不定位具体主键 |
| `keyeddiff` | 有键但不是单列整数（复合键、字符串等） | 按键拉取比对 |
| `joindiff` | 同一连接 + MySQL 系 + 单列整数键 | 同库两表联邦 JOIN |
| `iblt` | 跨连接（或非 MySQL）+ 单列整数键 | 可逆布隆表快路径；`--strict` 时解码失败 exit 2 而不回退 |
| `hashdiff` | `auto` **不会**选它 | `--strategy hashdiff` 强制二分 checksum |

DuckDB 参与比对：两侧均为 DuckDB 连接时用法与上表一致（含 `--update-column`/`--update-since` 增量窗口）。`BLOB` / `JSON` / `TEXT` 列不参与行哈希（预检排除并给出 warning）；`TIMESTAMPTZ` 以 UTC 文本规范化（bundled 构建无 ICU，不受影响）。**增量窗口时区语义**：bundled 构建无 ICU，会话时区固定为 UTC，窗口 cutoff 是 UTC 墙钟（与本地时间相差机器 UTC 偏移）；两侧使用同一 cutoff，比对结果自洽，但短窗口覆盖的行范围可能与本地时间直觉不同。跨引擎注意：UUID / BLOB / 非有限浮点（NaN/Infinity）在 DuckDB 与 GaussDB/Oracle 的规范化行为不同——相应列会被响亮排除或可能报差异，跨引擎比对时先核对。

预检（只看元数据与路由，不跑比对）：

```bash
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders --dry-run
```

### 9.4 一致性与复核

- `--consistency snapshot`（默认）：单侧快照，snapshot 模式下默认开启差异二次复核
- `--consistency none`：不做快照，吞吐更高，可能看到比对窗口内的并发写入
- `--recheck`：显式打开复核（snapshot 下本来就是开的）

跨库列类型不对称时会在报告里给出 warning。Oracle `CHAR`/`NCHAR` 与 GaussDB `character`/`bpchar` 的尾部空格可用 `--rtrim-char-columns` 在哈希/比较前裁掉；开启后「全空格」与 `NULL` 无法区分。

### 9.5 终端输出与导出

| 参数 | 作用 |
|------|------|
| `--sample N` | 终端差异明细行数上限，默认 20；`0` 表示终端也打全量。**不裁剪** `--export` |
| `--summary-only` | 只打统计，不打明细 |
| `--wide` | 终端显示全部比对列，不只变化列 |
| `--format` | 终端/ `--output` 的汇总格式：`table` / `json` / `csv` / `vertical` |
| `--output FILE` | 把终端那份报告写到文件 |
| `--export PATH` | 写出**全部**差异；后缀推断 `csv` / `jsonl` / `json` / `sql` |
| `--export-format` | 覆盖后缀推断 |
| `--export-rows` | 导出文件带完整左右行值（`.sql` 自动打开） |
| `--no-fetch-sample` | keyless 比对不要回查真实行 |

CSV 第一列为 `deltadiff_type`：`only_left` / `only_right` / `modified_left` / `modified_right`。`NUMBER`/`NUMERIC`/`DECIMAL` 按声明精度输出。无主键（hash-count）CSV 列为 `deltadiff_type,hash,left_count,right_count`。

```bash
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --export /tmp/orders.diff.csv

hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --export /tmp/orders.diff.jsonl --export-rows
```

### 9.6 SQL 补丁（仅 CLI）

`--export *.sql` **必须**同时给 `--apply-to left|right`，含义是「让这一侧变成另一侧」。只生成文件，不连库执行。MCP 没有对应参数。

```bash
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --export /tmp/orders.patch.sql --apply-to right
```

限制：

- 单文件差异行上限 100_000，超出请先 `--export out.csv`
- keyless / `bucketdiff` 无法生成精确 `UPDATE`/`DELETE`；`INSERT` 需要已回查的行值（不要加 `--no-fetch-sample`，或显式 `--export-rows`）

### 9.7 断点续传

```bash
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --checkpoint /tmp/orders.ckpt
```

Checkpoint 为 JSONL，带 `checkpoint_format_version`（当前为 **2**）。旧格式文件会被拒绝，请换新路径。损坏行会跳过并告警。

### 9.8 退出码（CI 契约）

| 退出码 | 含义 |
|--------|------|
| `0` | 比对完成，两侧一致（`--dry-run` 成功也是 0） |
| `1` | 比对完成，存在差异 |
| `2` | 执行错误（连接失败、权限不足、参数非法、超时、IBLT `--strict` 解码失败等） |

### 9.9 其他常用参数

| 参数 | 默认 | 说明 |
|------|------|------|
| `--threads` | 4 | 总并发；两侧各自不超过 ⌈N/2⌉ 个会话 |
| `--statement-timeout` | 300 | 单条查询超时，**秒** |
| `--bisection-factor` | 32 | hashdiff 二分因子 |
| `--bisection-threshold` | 16384 | hashdiff 行级阈值 |
| `--iblt-capacity` | 65536 | IBLT 预期差异容量 |
| `--fetch-all-threshold` | 4096 | keyeddiff：`max(COUNT)` 不超过此值时一次拉全量 |
| `--verbose` | 关 | 分片进度与每步 SQL 打到 stderr |

---

## 10. 合成数据生成 (synth)

`synth` 从真实表学习每列统计分布（Gaussian Copula：边际 + 从训练数据估计的相关矩阵），生成形似的合成数据，跨表外键保持引用完整性。自 **0.5.0** 起 `synth` 在默认 feature 中（预编译包同样包含）；仅提供 CLI 子命令（MCP 不暴露）。

### 10.1 编译启用

默认 `cargo build --release -p polar-mysql` 已包含 synth。若从源码关闭：

```bash
cargo build --release -p polar-mysql --no-default-features --features "oracle-rs,oracle,gaussdb"
```

### 10.2 工作流

```bash
# 1. 训练：采样真实数据，拟合每列边际分布 + Copula 相关矩阵
hepta_dbcli synth train --name dev --tables users,orders --output .synth

# 2. 起草规则：从数据库外键自动生成 YAML 规则草案
hepta_dbcli synth rules-draft --name dev --tables users,orders \
  --models .synth --output synth-rules.yaml

# 3. 生成：按模型与规则批量产出合成数据
hepta_dbcli synth generate --models .synth --rules synth-rules.yaml \
  --output synth-out --rows 1000 --seed 42 --format csv

# 校验模型文件
hepta_dbcli synth validate --model .synth/users.model.json
```

`train` 为每张表写出两个文件：

| 文件 | 内容 |
|------|------|
| `{table}.model.json` | 边际分布参数 + Copula 相关矩阵（带版本号，拒绝更高版本） |
| `{table}.profile.json` | 列统计（类型 / 基数 / top 值频次），供 rules-draft 唯一性检测 |

### 10.3 子命令参数

| 子命令 | 参数 | 说明 |
|--------|------|------|
| `train` | `--name`、`--tables`、`--schema`、`--output`、`--sample`、`--categorical-top-k` | `--schema` 限定表所在 schema；每表最多采样 `--sample` 行（默认 10000）；`--categorical-top-k N\|full` 控制分类列写入模型的档数（默认 50，与历史硬上限一致；`full` 不截断，模型文件超过 10 MiB 时打印警告） |
| `rules-draft` | `--name`、`--tables`、`--schema`、`--output`、`--models` | `--schema` 指定 FK 扫描的 schema；`--models` 下的 profile 用于唯一外键检测 |
| `generate` | `--models`、`--rules`、`--output`、`--rows`、`--seed`、`--format`、`--no-schema-qualifier` | `--format`: csv / jsonl / json / sql；`--rows` 为全表统一覆盖值，规则 YAML 的每表 `rows:` 优先级在其下（CLI > 规则 > 缺省 100）；SQL 默认带训练 schema 限定，`--no-schema-qualifier` 恢复旧的无前缀语句 |
| `validate` | `--model` | 校验模型 JSON 版本与结构 |

未指定 `--name` 时使用配置的 `default_connection`，与 `check` / MCP 行为一致。
退出码：`0` 成功，`1` 出错。

### 10.4 规则 YAML

```yaml
version: "1"
tables:
  - name: users
    rows: 599                    # 可选：本表生成行数（CLI --rows 优先于它）
    strategy: uniform            # uniform | zipf | weighted（按父列观测频次加权引用）
    columns:
      email:
        null_rate: 0.20          # 覆盖该列训练得到的 NULL 比例；0.0 = 从不 NULL
    relationships: []
  - name: orders
    strategy: zipf               # 子表按 Zipf 偏置引用父表键；weighted 按父列观测频次（或分类边际权重）采样
    columns:
      user_id:
        null_rate: 0.10          # 可空 FK：命中 NULL 时不消耗父池
    relationships:
      - pk: user_id              # 本表 FK 列
        references: [users.id]   # 父表.列
        pool_strategy: !projection
          unique: false          # true = 无放回采样（1:1）；子行数超过父池时报错
        # null_label 仍可写（兼容旧 YAML），生成路径不再读取它
```

列级 `null_rate` 优先于模型里训练到的 `null_rate`；省略则用模型值，再省略则视为 0。被其它表 `references` 指向的父键在生成时强制为 0（父键不能为 NULL），模型或规则若写了非 0 会在 stderr 告警。

`pool_strategy` 取值：

| 取值 | 行为 |
|------|------|
| `!projection { unique }` / `!generated { unique }` | 从父表已生成的引用列取值；`unique: true` 无放回 |
| `!fixed { values: [...] }` | 只从给定字面量集合中取值 |

### 10.5 语义与限制

- 表按外键依赖拓扑排序生成；检测到循环依赖直接报错并列出环路径
- 同一 `--seed` 下每张表派生独立随机流（djb2 混淆），同名表跨运行可复现
- 数值列：高基数或值无重复的列拟合 Normal 分布（整数列生成取整值）；**低基数且值重复出现**的数值列（如 19 档离散价格）自动按观测档位拟合分类分布，生成值保持在观测档位上并保留数值类型
- 数值列会学习 `decimal_scale`：字符串样本里纯小数面值（可选正负号、数字、至多一个 `.`，拒绝科学计数法）的最大小数位数，与 DDL 类型括号中的 scale（如 `numeric(16,2)` / `decimal(18,4)` / `NUMBER(18,4)`）取较大值。生成时按该标度做十进制 half-up 量化、再钳回 `[ceil(min), floor(max)]` 的格点区间，既避免 `1004.9999999999999` 这类二进制尾差写入 CSV/SQL，也不会因进位越出训练值域。**整数性由声明决定**：`int`/`bigint` 等整型或 scale 为 0 的 `DECIMAL` 输出 i64；声明了非零 scale 的列即使样本恰好全是整数（`"1.0000"` → 1.0）也按标度输出小数；只有拿不到 DDL 信息时才回退到样本判据。旧模型未带 `decimal_scale` 时不量化，输出与原先逐字节一致。日期时间和分类列不量化
- 字符串列拟合分类分布，分类列输出原始字符串值
- Copula 相关矩阵从训练数据估计（PIT 变换 + Pearson，分类列用累计频次中点编码；推断到格式的 datetime 列按 UTC epoch 做 PIT），PSD 修正用对角占优近似
- `unique: true`（无放回）可与 `uniform` / `zipf` / `weighted` 组合；`zipf`/`weighted` 使用 Efraimidis–Spirakis 加权无放回抽样（按权重一次排序，随后 O(1) 弹出）
- 不支持的列类型（如驱动的 `<unsupported type …>` 占位）在训练时跳过并打印警告，生成的数据不含这些列
- `{table}.model.json` 的 `pk` 来自 catalog 主键（`Dialect::table_indexes`），支持联合主键；无主键时为 `[]`。列名按采样列的大小写归一，且只保留最终进入模型的列（训练时被跳过的 PK 列不会出现在 `pk` 中）；`synth train` 与 `synth validate` 会打印主键
- 列逻辑类型会结合 DDL：全空的 `numeric`/`decimal`/`number`（含 MySQL `unsigned`/`zerofill`、DuckDB `ubigint` 等）记为 `numerical`；全空的 `date`/`timestamp` 记为 `datetime`（不再 `unknown`）
- 生成按列复现 NULL：有效比例为规则 `columns.<col>.null_rate`，否则模型 `null_rate`，否则 0。每列用独立 RNG 流（`{table}:{column}:null`）做 Bernoulli；比例为 0 时不构造该流，因此全 0 模型与开启 NULL 注入前的输出逐字节一致。`null_rate: 1.0` 仍为整列 NULL。可空 FK 命中 NULL 时不从父池取值，也不计入 `unique` 池耗尽。关系上的 `null_label` 仅保留解析兼容，生成不再消费。
- Copula 相关矩阵改为 pairwise-complete：列对中任一侧为 NULL 的行不参与 Pearson，分母是有效成对行数。不再用 `loc` / `0.5` 填缺失（那会把缺失当成典型值、扭曲相关）。
- 导出时 NULL 与空字符串可区分：CSV 空字段 = NULL、`""` = 空字符串；JSON/JSONL 为 `null`；SQL 为 `NULL`
- 全空列仍写入 `model.json`（`null_rate: 1.0`），生成时该列输出 NULL——保留列以便导出/建表结构与源表一致，但不会用 0 之类的常量伪造数据
- `YYYYMMDD`（及 ISO 日期字符串）即使物理类型是 `varchar(8)` 也会识别为 `datetime`，不再当成 numerical。DECIMAL/NUMBER 仍按数值训练。邮编、零填充 SKU、纯数字类别码仍可能被误判为数值；此类列请勿用于 synth 或先在库内转型
- 可推断格式的文本 datetime（`%Y-%m-%d`、`%Y-%m-%d %H:%M:%S`、固定宽度小数秒 `%.3f`/`%.6f`、ISO-8601、`%Y/%m/%d` 等）按 UTC epoch 拟合 Normal（`datetime_epoch: true` + `datetime_format`），生成时再按该格式还原字符串。无时区的值视为 UTC 午夜/墙钟。`enforce_min_max_values` 在 epoch 空间裁剪
- **带时区的列（`2024-01-15T12:34:56+08:00` / `+08` / `Z`）按 UTC 规范化输出**：绝对时刻与训练一致，但偏移与墙钟文本变成 `+00:00` 形式（与 delta-diff 对 `TIMESTAMPTZ` 的约定一致）。无时区的列才是逐字符还原；微秒列（`%.6f`）同样逐字符还原
- 无法推断格式的 datetime（如 Oracle `15-JAN-24`）保持旧行为（按观测值做 Categorical）并打印警告。旧模型 `logical_type: datetime` 且无 `datetime_format` 的生成路径不变
- `YYYYMMDD` 这类紧凑日期**不**走 epoch 格式还原，在 `model.json` 中仍以整数（如 `20240515`）建模，生成值裁剪在训练 min/max 之间但不保证是合法日历日（可能得到 `20240337`）；需要严格合法日期时请勿用 synth 生成该列或改用真实 `date`/`timestamp` 类型
- 生成值默认裁剪到训练 min/max（`--enforce-min-max-values`，默认开）。关闭该开关或 min/max 缺失时，数值列（含整数 PK）可能生成负数或越界值
- 纯 Rust Oracle 后端（oracle-rs 0.1.7）存在驱动缺陷：查询超过 100 行被静默截断。`synth train` 仅在**请求行数超过 100（`--sample` 默认 10000）且实际采样恰好 100 行**时认定被截断：向 stderr 打印 WARNING（含「分布可能失真」），并把 `{table}.model.json` 的 `provenance.truncated` 设为 `true`。`--sample 100`（或更小）是调用方自己的上限，不算截断；采样不足 100 行或非 Oracle 连接也不警告。该判定是启发式：恰好只有 100 行的表在请求更多行时仍会误报。native OCI 后端不受影响但当前无法从配置强制选择（见 `tests/benchmark/REPORT.md`）
- SQL 导出携带引用标识符与列名：MySQL 反引号、Oracle 双引号并折叠为大写、GaussDB 双引号小写。`synth train --schema S` 写入 `TableModel.schema`，`generate --format sql` **默认**输出 `INSERT INTO "S"."t"`（标识符按方言引用）；`--no-schema-qualifier` 恢复旧的无前缀语句
- `train` 与 `rules-draft` 的 `--schema` 语义一致：显式值优先，缺省时都取连接默认 schema（`side_schema_from_conn`），不再分别回落到 `current_schema`

### 10.6 基准测试与评测

| 基准 | 内容 | 报告 | 复现入口 |
|------|------|------|----------|
| Case A | 合成 4 列高斯 Copula，对标 SDV `GaussianCopulaSynthesizer(norm)` | [tests/benchmark/REPORT.md](../tests/benchmark/REPORT.md) | `tests/benchmark/run_case_a.sh` |
| P1 | SynMeter 真实单表（**仅 Adult**）：Wasserstein / MLA / QueryError 相对门禁（hepta ≤ SDV-GC × 1.15） | [tests/benchmark/p1/REPORT.md](../tests/benchmark/p1/REPORT.md) | `tests/benchmark/p1/run_p1.sh` |
| P2 | ogagila pagila 三表（customer–rental–payment）：门禁 = 可插入 0 错误、孤儿 FK = 0、**payment.amount on-grid ≥ 0.95**；P2-2 每 customer 扇出 KS **仅记录**（uniform 0.1888 / zipf 0.7238，empirical fan-out 不在本里程碑）；1-hop 相关仅记录 | [tests/benchmark/p2/REPORT.md](../tests/benchmark/p2/REPORT.md) | `tests/benchmark/p2/run_p2.sh` |
| M1 验收 | synth M1 端到端（真实 MySQL fixture）：datetime 格式还原与值域、NULL 比例复现、DECIMAL 标度、字典列全档、FK 引用完整性、SQL schema 限定、同 seed 逐字节一致 | [tests/synth-verify/README.md](../tests/synth-verify/README.md) | `HEPTA_DBCLI_TEST_URL=... bash tests/synth-verify/run_m1.sh` |

CI：`.github/workflows/synth-benchmark.yml`——每周 cron 只跑 P1-adult（零外部服务）；Case A / P2 为 `workflow_dispatch` 且需仓库变量 `OGAGILA_DIR`（ogagila 检出 URL）。门禁断言决定 job 成败，报告作为 artifact 上传。on-grid 门禁在 P2 强制执行（P1 不含 payment 表）。

范围声明：Case B（vs CTGAN / TVAE / TabDDPM / GReaT）与 Case C（vs SDV HMA / ClavaDDPM / REaLTabFormer）**不在本里程碑**；SynMeter / torch / SDV 仅存在于 benchmark venv（`tests/benchmark/requirements.txt`），不进入 `Cargo.toml`。

---

## 11. 进阶用法

### 11.1 多连接切换

```bash
# CLI 模式切换连接
hepta_dbcli cli --name dev --sql "SELECT * FROM users"
hepta_dbcli cli --name prod --sql "SHOW PROCESSLIST"
hepta_dbcli cli --name gauss_dev --sql "SELECT current_schema()"

# REPL 模式内切换
hepta_dbcli cli --interactive
$ .connect prod
hepta_dbcli interactive -- connected to 'prod'
```

### 11.2 超时控制

```bash
# 设置单条 SQL 最大 5 分钟
hepta_dbcli cli --statement-timeout 5min --sql "SELECT SLEEP(10)"

# 设置连接 10 分钟后自动回收
hepta_dbcli cli --connection-max-lifetime 10min --sql "..."

# 超时后断开连接（而非保持）
hepta_dbcli cli --timeout-action disconnect --sql "..."
```

### 11.3 使用 PolarDB-X

hepta_dbcli 完全兼容 PolarDB-X（基于 MySQL 协议）：

```bash
# 使用 PolarDB-X Docker 镜像快速体验
docker run -d --name polardb-x -p 8527:8527 -m 12GB \
  polardbx/polardb-x

# 等待约 1 分钟容器启动后连接
export HEPTA_DBCLI_URL="mysql://polardbx_root:123456@127.0.0.1:8527"

# 或使用配置文件
cat > ~/.hepta-dbcli.toml << 'EOF'
host = "127.0.0.1"
port = 8527
user = "polardbx_root"
password = "123456"
EOF

hepta_dbcli cli --sql "SELECT VERSION()"
```

> **注意**：PolarDB-X 默认端口为 `8527`（非 3306），默认用户 `polardbx_root`。

### 11.4 本地三后端 Docker

仓库 `tests/` 下有现成 compose 与 TOML：

```bash
docker compose -f tests/docker-compose.yml up -d mysql oracle gaussdb

hepta_dbcli --config tests/docker-all.toml check --name mysql
hepta_dbcli --config tests/docker-all.toml check --name oracle
hepta_dbcli --config tests/docker-all.toml check --name gaussdb
```

GaussDB 测试配置见 `tests/docker-gaussdb.toml`（`sslmode = "disable"`）。

### 11.5 在脚本中使用

```bash
#!/bin/bash
# 查询用户数并判断
COUNT=$(hepta_dbcli cli --format json --sql "SELECT COUNT(*) AS c FROM users" | jq -r '.rows[0][0]')
if [ "$COUNT" -gt 100 ]; then
    echo "用户数超过 100: $COUNT"
fi

# 跨库比对作为 CI 门禁
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders --summary-only
case $? in
  0) echo identical ;;
  1) echo diverge; exit 1 ;;
  *) echo error; exit 2 ;;
esac
```

---

## 12. 错误排查

### 12.1 常见错误

| 错误信息 | 原因 | 解决方案 |
|----------|------|----------|
| `No connection configuration found` | 未找到配置文件或环境变量 | 创建 `~/.hepta-dbcli.toml` 或设置 `HEPTA_DBCLI_URL` |
| `Connection failed` | 数据库不可达或凭据错误 | 使用 `hepta_dbcli check --verbose` 诊断 |
| `Connection 'xxx' not found` | 指定的连接名在配置中不存在 | 检查配置文件的 `[connections.xxx]` 段名 |
| `keyring password not found` | 钥匙串中无密码且配置文件未含明文密码 | 执行 `hepta_dbcli store-password --name …` |
| `Only SELECT, EXPLAIN, … queries are allowed` | MCP `execute_query` 仅允许只读前缀 | 写操作请用 `hepta_dbcli cli` |
| `No SQL provided` | 未提供 SQL 且 stdin 为空 | 使用 `-s`、`-f` 参数或管道传入 SQL |
| `--where must not contain ';'` | 防注入 | 去掉分号，拆成一次比对 |
| `--update-since requires --update-column` | 增量参数不完整 | 同时提供列名与窗口 |
| `--export .sql requires --apply-to` | SQL 补丁必须声明对齐方向 | `--apply-to left` 或 `right` |
| checkpoint 版本不匹配 | 断点文件是旧格式 | 换新路径，不要复用 v1 文件 |
| GaussDB TLS / SSL 失败 | 服务器未开 TLS | 连接段加 `sslmode = "disable"` |
| GaussDB `28P01` 密码被拒 | 密码错误，或钥匙串不是这一套 | 本工具 service 是 `hepta-dbcli`，与独立 `gaussdb` CLI 的钥匙串**不共享** |
| `No backend registered for scheme '…'` | 二进制未编入对应 feature | 用默认 feature 重新 `cargo build --release -p polar-mysql` |

### 12.2 诊断流程

```bash
# 1. 检查配置文件是否能正确解析
hepta_dbcli check --config /path/to/config.toml

# 2. 详细连接诊断
hepta_dbcli check --verbose
hepta_dbcli check --name gauss_dev --verbose

# 3. 检查钥匙串状态
# macOS: 打开「钥匙串访问」，搜索 "hepta-dbcli"
# Linux: secret-tool search service hepta-dbcli

# 4. 查看日志
cat ~/.local/share/hepta-dbcli/hepta-dbcli.log
```

### 12.3 TLS 证书问题

如果 MySQL 的 `TLS(verify)` 失败但 `NoTls` 和 `TLS(skip-verify)` 成功，说明服务器 TLS 证书配置有问题。可以：

1. 使用无 TLS 连接（如果网络环境安全）
2. 使用 `TLS(skip-verify)` 模式
3. 联系 DBA 修复服务器证书配置

GaussDB 本地 Docker 几乎都应设 `sslmode = "disable"`。

---

## 附录：命令速查

```bash
# 帮助
hepta_dbcli --help
hepta_dbcli cli --help
hepta_dbcli delta-diff --help
hepta_dbcli synth --help

# 连接检查
hepta_dbcli check
hepta_dbcli check --verbose
hepta_dbcli check --name prod
hepta_dbcli check --name gauss_dev

# CLI 执行
hepta_dbcli cli --sql "SELECT 1"
hepta_dbcli cli --file query.sql
echo "SELECT 1" | hepta_dbcli cli
hepta_dbcli cli --sql "SELECT 1" --format json
hepta_dbcli cli --name gauss_dev --sql "SELECT version()"

# REPL
hepta_dbcli cli --interactive
hepta_dbcli cli -i --name dev

# 密码管理
hepta_dbcli store-password
hepta_dbcli store-password --name prod

# MCP 服务器
hepta_dbcli
hepta_dbcli mcp
hepta_dbcli --config /path/to/config.toml

# 跨库比对
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders --dry-run
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --update-column updated_at --update-since "1 day" \
  --export /tmp/orders.diff.csv
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --checkpoint /tmp/orders.ckpt
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --export /tmp/orders.patch.sql --apply-to right

# 合成数据生成（默认 feature 自 0.5.0 起包含 synth）
hepta_dbcli synth train --name dev --tables users,orders --output .synth
hepta_dbcli synth rules-draft --name dev --tables users,orders --output synth-rules.yaml
hepta_dbcli synth generate --models .synth --rules synth-rules.yaml \
  --output synth-out --rows 1000 --seed 42 --format csv
hepta_dbcli synth validate --model .synth/users.model.json
```
