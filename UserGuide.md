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
11. [数据回灌](#11-数据回灌-load)
12. [进阶用法](#12-进阶用法)
13. [错误排查](#13-错误排查)

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
# 可选：MCP `delta_diff` 的 export/checkpoint 写入根目录，未配置时用系统临时目录
delta_diff_export_root = "/var/tmp/hepta-exports"

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

### 3.6 免配置内联 URL（`--url`）

临时连接（典型场景：随手查询一个 DuckDB 文件）不值得写进配置文件。URL 可以直接放在命令行上：

```bash
# CLI / REPL 一次性连接（与 --name 互斥，优先级高于环境变量和配置文件）
hepta_dbcli --url "duckdb:///data/analytics/shop.duckdb?mode=ro" cli --sql "SELECT 42"
hepta_dbcli --url "duckdb://:memory:" cli --interactive
hepta_dbcli --url "duckdb://:memory:" check          # 探活，同样免配置

# delta-diff 任一侧可用 URL 替代连接名（两侧可混搭）
hepta_dbcli delta-diff --left-url duckdb:///tmp/orders_copy.duckdb \
  --right mysql_dev --table orders
```

规则：

- 每侧「URL 与连接名」二选一，同时给出会被拒绝；
- URL 必须带 `scheme://`（如 `duckdb://`、`mysql://`），缺失时启动前即报错；
- `--url` 支持 `cli`（含 `--check-connection`）、REPL 与 `check` 子命令；`delta-diff` 用侧级的 `--left-url` / `--right-url`；`mcp`、`store-password` 与 `synth` 不支持 `--url`（显式报错退出码 2）；
- `delta-diff` 两侧都是 URL 时完全不读取配置文件；
- REPL 内的 `.connect <名字>` 仍只接受连接名，内联 URL 只在进程启动时生效；
- MCP 无配置文件也能启动（连接表为空），配合 `delta_diff` 的 `left_url` / `right_url` 即可全程免配置。

### 3.7 超时设置

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
      --url <URL>
          Inline connection URL (e.g. duckdb:///tmp/a.duckdb);
          conflicts with --name, overrides HEPTA_DBCLI_URL and the
          config file
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

配置文件缺失**不再是致命错误**：进程会以空连接表启动并在 stderr 打警告。此时 `list_connections` 等按名字取连接的工具会逐调用报错，但配合 `delta_diff` 的 `left_url` / `right_url`（见 §8.4）即可全程免配置使用。降级仅在「未传 `--config`、home 下无默认配置文件且未设 `HEPTA_DBCLI_URL`」时发生；显式 `--config` 存在时，任何配置错误（toml 语法错误、不可读、文件不存在）都会启动失败（exit 1）。另外 `hepta_dbcli --url ... mcp` 会被显式拒绝（exit 2），MCP 的免配置路径是工具参数 `left_url`/`right_url`。

> **信任边界**：允许 MCP 客户端传 `left_url` / `right_url` 意味着它可以让服务器连接任意主机，`duckdb://` 还能读取进程权限可达的任意本地文件。只有 `delta_diff` 接受内联 URL；`execute_query` 等其余工具仍只能用命名连接。请用只读权限最小的账号运行 MCP 服务器，并把内联 URL 当作交给受信客户端的凭据对待。

### 8.2 提供的工具

| 工具 | 说明 |
|------|------|
| `get_database_info` | 获取服务器版本、当前用户、字符集、操作系统等 |
| `list_tables` | 列出所有用户表/视图（含引擎类型、行数、数据大小） |
| `get_table_metadata` | 获取指定表的列类型、是否可空、默认值、索引信息 |
| `execute_query` | 执行只读 SQL（按方言前缀校验） |
| `get_execution_plan` | 获取 SQL 执行计划（EXPLAIN / EXPLAIN ANALYZE；Oracle 走 EXPLAIN PLAN + DBMS_XPLAN） |
| `list_connections` | 列出所有配置的连接及其状态 |
| `delta_diff` | 跨库表比对（只读）。每侧可用 `left_url`/`right_url` 免配置接入。支持增量（`update_column`/`update_since`）、`checkpoint`、csv/jsonl/json `export`。SQL 补丁 `--apply-to` 仅 CLI |

`execute_query` 自动追加行数限制：MySQL / GaussDB 为 `LIMIT N`，Oracle 12c+ 为 `FETCH FIRST N ROWS ONLY`，Oracle 11g 为 `ROWNUM`，DuckDB 仅对 `SELECT` / `WITH` 形语句追加 `LIMIT N`（`SHOW` / `DESCRIBE` / `SUMMARIZE` 不追加）。`max_rows` 默认 1000，上限 10000。

### 8.3 安全限制

- **只读执行**（按方言）：
  - MySQL / PolarDB-X：`SELECT`、`EXPLAIN`、`SHOW`、`DESCRIBE`、`DESC`
  - Oracle / GaussDB：`SELECT`、`EXPLAIN`、`WITH`
  - DuckDB：`SELECT`、`EXPLAIN`、`WITH`、`SHOW`、`DESCRIBE`、`DESC`、`SUMMARIZE`（`PRAGMA` 不允许——部分 PRAGMA 有写副作用）
- **行数限制**：默认 `LIMIT 1000`，可通过 `max_rows` 调整（上限 10000）
- **超时控制**：支持按查询设置 `timeout_ms`
- **delta_diff 只读**：可导出 csv/jsonl/json 与写 checkpoint 文件；不会对数据库执行 DML。生成 SQL 补丁请用 CLI `--export *.sql --apply-to`。**路径限制**：`export` / `checkpoint` 路径被限制在配置的导出根目录 `delta_diff_export_root` 内（未配置时默认系统临时目录；相对路径相对该根目录解析）。写入前会 `canonicalize` 校验，路径逃逸出根目录（`..` 穿越、指向根目录外的符号链接等）会被拒绝且不写任何文件

### 8.4 `delta_diff` 工具参数

必填：每侧 `left_connection` **或** `left_url` 二选一（右侧同理），两者皆缺会明确报错；`table` 必填。URL 侧无需在配置文件中登记任何连接，例如本地 DuckDB 文件可直接传 `left_url: "duckdb:///tmp/a.duckdb"`。同侧同时给出连接名与 `*_url` 会被拒绝（mutually exclusive）；URL 形态须含 `scheme://`。URL 侧在审计与返回报告中的连接名显示为 `inline-<scheme>`（如 `inline-duckdb`）。

可选：`left_table` / `right_table`、`schema` / `left_schema` / `right_schema`、`key_columns`、`columns`、`exclude_columns`、`where_condition`、`strategy`（`auto` / `hashdiff` / `joindiff` / `bucketdiff` / `iblt` / `keyeddiff` / `naivediff`）、`consistency`（`snapshot` / `none`）、`recheck`、`sample_limit`（默认 1000）、`summary_only`、`update_column` / `update_since`（增量窗口；`update_since` 默认 `"1 day"`，须与 `update_column` 同用，且与 `where_condition` 互斥）、`checkpoint`（JSONL 断点文件路径）、`export`（导出文件路径，后缀推断 csv/jsonl/json）、`export_format`（显式指定 csv/jsonl/json；`sql` 被拒绝——SQL 补丁仍需 CLI `--apply-to`）、`export_rows`（默认 `false`，导出内容不含差异行明细）。`--iblt-auto-capacity` 两轮自适应仅 CLI 提供（MCP/API 走固定 `--iblt-capacity`，小于 16 按 16 处理）。

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

# 任一侧可用内联 URL 替代连接名，无需配置（适合本地 DuckDB 文件）
hepta_dbcli delta-diff --left-url duckdb:///tmp/orders_copy.duckdb \
  --right mysql_dev --table orders

# 异名表 / 指定 schema
hepta_dbcli delta-diff --left mysql_dev --right ora_dev \
  --left-table orders --right-table ORDERS \
  --left-schema shop --right-schema SCOTT

# 指定比对键与列（自动发现失败时）
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --key id --columns id,amount,status

# 排除若干列（自动发现后做减法，适合 etl_time / remark 这类噪声列）
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders \
  --exclude-columns etl_time,remark,src_file
```

`--exclude-columns` 语义：

- 在自动发现（或 `--columns` 白名单）之后做减法，两侧同名同用；列名大小写不敏感。
- 列名必须是该表现有列，拼错直接报错，不会静默跳过。
- 同一列同时出现在 `--columns` 与 `--exclude-columns` 会报错（语义冲突），两者只在列名不重叠时共存。
- 排除的是「值比较」：被排除的键列仍然用于行配对（`key_columns` 不变），报告会在 warnings 里写明；被排除的列也不会出现在 `--export` / `.sql` 补丁的列清单里。
- 全部列都被排除时报错。

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
| `naivediff` | `auto` **不会**选它 | `--strategy naivediff` 强制「每侧一次全扫描 + 客户端归并」：裸键 ORDER BY、无 NLSSORT/COLLATE、无 LIMIT，正确性不依赖库端行序；适合单日数万行、复合 VARCHAR 主键、经常零差/少量差的日对账。超过 `--naive-max-rows` 行数硬顶直接拒绝并提示改用 keyeddiff；无键表走全行多重集差（不回退 bucketdiff） |

DuckDB 参与比对：两侧均为 DuckDB 连接时用法与上表一致（含 `--update-column`/`--update-since` 增量窗口）。`BLOB` / `JSON` / `TEXT` 列不参与行哈希（预检排除并给出 warning）；`TIMESTAMPTZ` 以 UTC 文本规范化（bundled 构建无 ICU，不受影响）。**增量窗口时区语义**：bundled 构建无 ICU，会话时区固定为 UTC，窗口 cutoff 是 UTC 墙钟（与本地时间相差机器 UTC 偏移）；两侧使用同一 cutoff，比对结果自洽，但短窗口覆盖的行范围可能与本地时间直觉不同。跨引擎注意：UUID / BLOB / 非有限浮点（NaN/Infinity）在 DuckDB 与 GaussDB/Oracle 的规范化行为不同——相应列会被响亮排除或可能报差异，跨引擎比对时先核对。

预检（只看元数据与路由，不跑比对）：

```bash
hepta_dbcli delta-diff --left mysql_dev --right gauss_dev --table orders --dry-run
```

`bucketdiff` 有两种分桶方式，按「是否存在可用的单列整数键」自动选择：

- **有单列整数键**（类型为整数/数值/decimal 等）：先探针一次 `MIN/MAX` 拿键域，再按键区间分桶，每个桶的拉取都走索引范围。
- **无键表，或键类型不可能是整数**（VARCHAR/日期/JSON 等）：不做任何探针，直接按 `MOD(rowHash, N)` 内容分桶。

探针只在能构成整数键域时才发；探针语句本身被库拒绝（例如键列不支持 `MIN()`）会直接报错并给出替代策略提示，不会静默降级。键没有单列整数形态时请改用 `--strategy naivediff` 或 `--strategy keyeddiff`。

### 9.4 一致性与复核

- `--consistency snapshot`（默认）：单侧快照，snapshot 模式下默认开启差异二次复核
- `--consistency none`：不做快照，吞吐更高，可能看到比对窗口内的并发写入
- `--recheck`：显式打开复核（snapshot 下本来就是开的）

跨库列类型不对称时会在报告里给出 warning。Oracle `CHAR`/`NCHAR` 与 GaussDB `character`/`bpchar` 的尾部空格可用 `--rtrim-char-columns` 在哈希/比较前裁掉；开启后「全空格」与 `NULL` 无法区分。

### 9.5 终端输出与导出

| 参数 | 作用 |
|------|------|
| `--sample N` | 终端差异明细行数上限，默认 20；`0` 表示终端也打全量。**不裁剪** `--export`。抽样默认 `diverse` |
| `--sample-mode` | 终端抽样模式：`diverse`（默认；status 配额 + 变化列覆盖 + 签名去重，按 key 序展示）或 `prefix`（key 序前 N 行）。只影响终端样本与 MCP payload，**不影响** `--export` |
| `--summary-only` | 只打统计，不打明细 |
| `--wide` | 终端显示全部比对列，不只变化列 |
| `--format` | 终端/ `--output` 的汇总格式：`table` / `json` / `csv` / `vertical` |
| `--output FILE` | 把终端那份报告写到文件 |
| `--export PATH` | 写出**全部**差异；后缀推断 `csv` / `jsonl` / `json` / `sql` |
| `--export-format` | 覆盖后缀推断 |
| `--export-rows` | 导出文件带完整左右行值（`.sql` 自动打开） |
| `--no-fetch-sample` | keyless 比对不要回查真实行 |

summary 中 Modified 行会附列级变化直方图（`modified_by_column[列名]` 计数行，count 降序）。计数含义是「该列发生变化的 Modified 行数」，**不是** `modified` 的拆分：数值标度差异（如 `12150.0` vs `12150`，变化判定与终端一致）不算变化，计数之和可能小于 `modified`；一行改多列时计数之和会大于 `modified`。keyless（hash-count）比对没有列可比，不生成直方图、样本不做列多样化；`--summary-only` 同样输出直方图。

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
| `--iblt-capacity` | 65536 | IBLT 预期差异容量（小于 16 按 16 处理） |
| `--iblt-auto-capacity` | 关 | IBLT 自适应两轮：第一轮 m=64，解码失败后按估计差异量 d̂ 放大一轮重试（m₂=clamp(3·d̂)），两轮都失败才回退 hashdiff；内存占用固定在桶表，不随容量预分配增长 |
| `--fetch-all-threshold` | 4096 | keyeddiff：`max(COUNT)` 不超过此值时一次拉全量 |
| `--naive-max-rows` | 200000 | naivediff：`max(COUNT)` 超过此值时拒绝（exit 2）并提示改用 `--strategy keyeddiff`；`0` = 不设顶（大表物化内存自负） |
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

# 2. 起草规则：从数据库外键自动生成 YAML 规则草案（--models 存在时额外做隐式引用推断）
hepta_dbcli synth rules-draft --name dev --tables users,orders \
  --models .synth --output synth-rules.yaml
# 2b. 顺带挖掘条件业务规则候选（只输出注释，永不自动启用）
hepta_dbcli synth rules-draft --name dev --tables users,orders \
  --models .synth --output synth-rules.yaml \
  --mine --mine-confidence 0.95 --mine-support 0.05 --emit-candidates candidates.txt

# 3. 生成：按模型与规则批量产出合成数据
hepta_dbcli synth generate --models .synth --rules synth-rules.yaml \
  --output synth-out --rows 1000 --seed 42 --format csv

# 4. 质量报告：对生成数据打分（对比 train 记录的留出集摘要）
hepta_dbcli synth report --models .synth --data synth-out \
  --rules synth-rules.yaml --min-score 0.85

# 校验模型文件
hepta_dbcli synth validate --model .synth/users.model.json
```

`train` 为每张表写出两到三个文件：

| 文件 | 内容 |
|------|------|
| `{table}.model.json` | 边际分布参数 + Copula 相关矩阵（带版本号，拒绝更高版本） |
| `{table}.profile.json` | 列统计（类型 / 基数 / top 值频次），供 rules-draft 唯一性检测 |
| `{table}.report-baseline.json` | 留出集摘要（数值列分位、类别列频次、列对统计），供 `synth report` 离线打分；`--holdout-ratio 0` 时不生成。文件只含聚合摘要、不含原始行 |

### 10.3 子命令参数

| 子命令 | 参数 | 说明 |
|--------|------|------|
| `train` | `--name`、`--tables`、`--schema`、`--output`、`--sample`、`--categorical-top-k`、`--rules`、`--holdout-ratio` | `--schema` 限定表所在 schema；每表最多采样 `--sample` 行（默认 10000）；`--categorical-top-k N\|full` 控制分类列写入模型的档数（默认 50，与历史硬上限一致；`full` 不截断，模型文件超过 10 MiB 时打印警告）；`--rules` 里的 `columns.<列>.marginal` 可强制该列边际族（见 §10.4），`--holdout-ratio`（默认 0.1，`0` 关闭）决定写入 `report-baseline.json` 的留出行占比（上限 5 万行） |
| `report` | `--models`、`--data`、`--rules`、`--against-db [连接名]`、`--rows`、`--seed`、`--output`、`--min-score`、`--strict` | 对生成数据打分：`--data` 指定 `generate` 的输出目录（`{table}.csv/jsonl/json`；CSV 空字段 = NULL、`""` = 空字符串），省略时按 `--rows`（默认 1000）与 `--seed` 现场生成；`--rules` 提供 FK 关系用于 `fk` 节；`--against-db`（可省值，裸用即默认连接）用真库键池算 join-rate，否则用**生成出的父表键**；`--min-score F` 低于阈值时退出码非 0（要求**每张表都能打分**：缺 baseline/缺生成数据导致该表无分时直接失败）；`--strict` 把缺 baseline 或缺生成数据当作错误 |
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
      score:
        marginal: gamma          # 强制该列边际族：normal/beta/gamma/uniform/ecdf/categorical
      part_date:
        fixed: "20240101"        # 全表同值（数值字面量按列类型输出，SQL 里不带引号）
      status:
        values:                  # 加权值池；也支持等权写法 values: [normal, peak]
          normal: 0.7
          peak: 0.3
      created_at:
        fixed_range: ["2026-01-01", "2026-01-31"]   # 闭区间，端点可为数值/日期字符串
        mode: rejection          # rejection（默认）| copula_conditional
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

列级 `null_rate` 优先于模型里训练到的 `null_rate`；省略则用模型值，再省略则视为 0。

列级 `marginal` **只在 `synth train --rules` 时生效**（`generate` 读模型，不重训）：它跳过自动择优、直接按指定族拟合；指定族拟合出的参数对该列不可用时（例如跨零样本拟合 Gamma 得到负 scale）打印告警并回退自动择优。规则与列不匹配会让 `train` 直接失败而不是静默丢列：未知列名、未知族名、给低基数（有 `top_values` 字典）列指定连续族（保留 `categorical`），或给带格式的日期时间列指定 `categorical`。被其它表 `references` 指向的父键在生成时强制为 0（父键不能为 NULL），模型或规则若写了非 0 会在 stderr 告警。

`pool_strategy` 取值：

| 取值 | 行为 |
|------|------|
| `!projection { unique }` / `!generated { unique }` | 从父表已生成的引用列取值；`unique: true` 无放回 |
| `!fixed { values: [...] }` | 只从给定字面量集合中取值 |

#### 子表基数建模（`cardinality`，issue #72）

`synth train` 会按外键统计"每个父键对应多少子行"，把计数分布写进**子表模型**的 `fk_cardinality`（键为 FK 列名；`0` 档来自父表键去重数减去被引用键数）。默认生成仍按 `--rows` 生成固定行数、逐行独立采父键；要让子表行数跟随学习到的基数分布，在 relationship 上加 `cardinality: modeled`：

```yaml
  - name: orders
    relationships:
      - pk: user_id
        references: [users.id]
        cardinality: modeled   # 默认 exact_rows
```

| 取值 | 行为 |
|------|------|
| `exact_rows`（默认） | 子表行数 = `--rows`（或 rules 的 `rows`），与现状逐字节一致 |
| `modeled` | 逐父键按 `fk_cardinality` 采样子行数，**子表行数由分布求和得出**（`--rows` 对这张表不再生效，父表仍是 `--rows`）；FK 值按父键成块写入，不再逐行独立采样 |

- 模型里没有对应 `fk_cardinality`（旧模型或未训练）时 `generate` 直接报错，不会静默退回固定行数；
- `unique: true` 的 1:1 关系把每个父键的子行数截断到 0/1，保证父值不被重复引用；
- NULL 外键份额按训练期 `null_share` 复现，NULL 行不计入任何父键的基数；
- 每张表最多一个 `cardinality: modeled` 关系（多个会报错）；
- 训练分布与实际生成的基数对比见 `synth report` 的 fk 行（`cardinality tv`，越小越接近）。

#### PII 识别与匿名化（`sdtype`，issue #71）

`synth train` 默认识别 PII 列并**匿名化**：列名模式（`email/mail/phone/mobile/tel/name/real_name/id_card/ssn/nickname` 等，含中文）与内容正则（email、电话、18 位证件号）投票，命中的列在生成时用格式合法的假值替换，**不复现任何训练值**。识别结果写入模型列元数据（`pii: email|phone|name|id_card`）。

```yaml
tables:
  - name: users
    columns:
      email:
        sdtype: keep            # 关闭该列的匿名化（回到 top_values 采样）
      mobile:
        sdtype: pii             # 强制匿名化
        pii_provider: phone     # 显式指定 provider：email/phone/name/id_card
        pii_unique: true        # 假值不重复
        pii_stable_mapping: true  # 训练中同值 → 生成同假值
```

| `sdtype` | 行为 |
|----------|------|
| `auto`（默认） | 采用识别结果 |
| `keep` | 不匿名化，保留训练值域（`top_values` 频次采样，现状） |
| `pii` | 强制匿名化；`pii_provider` 省略时用识别结果，再退回 `name` |

- **防泄漏**：训练为 PII 的列不写入 `profile.json` 的 `top_values`；模型里该列的字典被替换为 `__pii_level_N` 占位（保留档位/频次结构，不保留原值），数值/日期列的 min/max/格式一并清空，相关矩阵中该维归零（不参与其它列的联合采样）。
- **键列规则**：主键（含自然键，如 `email` 主键）**会**被匿名化；导出时主键唯一性会通过 provider 重抽（不再从占位字典取值）。外键列**不**匿名化——生成时它由父表键池赋值，命中识别或强制 `sdtype: pii` 都会在训练期告警/加载期报错，以免破坏引用完整性。
- **格式合法**：email 来自 `fake` 的 `SafeEmail`，name 来自 `Name`，phone / id_card 用固定模板；生成端会校验并重抽，保证 100% 通过各自格式。
- **确定性**：假值由表名+列名+seed 派生，同 seed 同配置逐字节可复现；`stable_mapping` 时同档位映射同一假值（不同档位不碰撞）。
- **二进制体积**：引入 `fake`（2.9，复用已有 `rand 0.8`）后 release 二进制增加约 1.9%（<15% 门禁），未做 feature gate。

#### 主键唯一性（issue #82，#103 起覆盖全部导出格式）

模型里记录的主键（`model.pk`，含复合主键）在**所有导出格式**下都强制唯一，即使没有其它表引用它：#98 的 `load` 使 CSV / JSONL / JSON 产物也能关系化回灌，重复主键在任何格式下都会 `Duplicate entry`（#82 时只有 SQL 需要这个保证，#103 把它推广到全部格式）。单列主键与父键一样做拒绝重抽；复合主键只要求**元组**唯一，单个成员可以重复。

- 训练观测到的可取值足够 `--rows` 时用重抽填满；不够时（如 12 个整数主键要生成 200 行）超出部分**外推**到训练值域之外（整数主键续号、日期主键按秒推进），保证产物始终可插入。只有主键既不可外推（非数值/日期字符串）又不够行数时才报错，错误含列名与请求行数；
- 列级 `fixed` / `values` / `fixed_range` 覆盖、relationship 的 `pk`（外键子列）与零方差列不在此检查范围内：前者是用户的显式选择，后两者由池策略/边际决定；
- 主键列在训练集里全部为 NULL 时按 NULL 生成，不参与唯一性判定。

#### 列级固定值与区间（`fixed` / `values` / `fixed_range`）

| 字段 | 行为 |
|------|------|
| `fixed: <字面量>` | 该列全表取同一值。按列的逻辑类型输出：数值列的数值字面量在 JSON/SQL 中都是数值（分区键不会被引号包裹），日期时间列会按模型的 `datetime_format` 解析成 epoch 再定位 |
| `values: {值: 权重}` 或 `values: [值, ...]` | 逐行独立按权重/等权取值；省略权重即等权。加权池用有序映射，保证同 seed 可复现。键按**标量**读取，`values: {1: 0.7, 2: 0.3}` 这种数值键不需要加引号 |
| `fixed_range: [low, high]` | 闭区间。数值与可解析的日期时间列都支持；`low > high`、端点类型不一致、或区间落在训练分布外都会在配置期/生成期报错。日期时间列的两个端点按**时刻**比较（epoch 秒，与 `copula_conditional` 同一域），端点先按列的 `datetime_format` 解析、失败再按通用 ISO 形状解析，纯数字端点直接当 epoch 秒；只写到日期的端点等于**当天 00:00:00** |

`fixed` / `values` / `fixed_range` 与以下组合都会被拒绝（错误含表名与列名）：`fixed` 与 `values` 并存、与该列 `null_rate > 0` 并存（阶段 4 覆盖整列，rate 无意义）、该列是被其他表 `references` 的父键、该列是 relationship 的 `pk`。

区间约束有两条路径，取舍如下：

| `mode` | 机制 | 保真 | 代价 |
|--------|------|------|------|
| `rejection`（默认） | 正常采样后校验，落在区间外就用该列自己的独立流重抽 | 保留原始边际形状（截断分布） | 区间概率质量越小时越慢；单值超过 10000 次重抽或区间内质量 < 1% 直接报错（不会静默截断行数） |
| `copula_conditional` | 把该列钉到 `z = Φ⁻¹(F(x))`（区间则逐行取 `[F(low), F(high)]` 内的分位数），**其余列按条件多元正态分布采样** | 同样落在区间内，且**保留与被固定列的相关结构** | 需要可逆的数值/日期边际（分类列报错）；固定维度的协方差子矩阵奇异时报错 |

`copula_conditional` 只能配 `fixed` 或 `fixed_range`（`values` 是逐行独立的，没有可条件化的量）。被 pin 的列不再做 min/max 裁剪，用户的区间优先。示例：把 `occurred_at` 钉到 2026-01-05..01-10、且它与 `event_id` 相关系数 0.77 时，生成的 `event_id` 会跟着下移（均值 10.9 → 4.9），而不是独立重采样。

#### 派生列（`derive`，issue #70）

```yaml
tables:
  - name: line_items
    derive:
      - column: total          # 覆盖该列：total = price * qty * 2 + 0.01
        expr: "price * qty * 2 + 0.01"
```

- 语法（**白名单**，加载期校验）：数字字面量、列引用、`+ - * / %`、一元负号、括号、比较（`== != < <= > >=`）、逻辑（`&& ||`）、单引号字符串（仅用于比较）、**函数调用 `if(cond, then, else)`**（#94 起，白名单函数；条件必须为布尔表达式，两分支静态类型一致，惰性求值）。**没有一元 `!`**：写 `status != 'A'`，孤立的 `!` 会报 `unexpected \`!\`; use \`!=\` for inequality`。**白名单外的函数调用、属性访问、下标一律拒绝**（加载期报错；未知函数名会列出已知函数）。
- 求值用 `rust_decimal`：`0.1 * 3` 精确等于 `0.3`；除零/取余零、与 NULL 比较（恒为 false）、字符串与数值混比都有明确错误，溢出报错而不是 panic。
- **NULL 传播（三值逻辑）**：`derive` 无条件重算目标列，源列为 NULL 时结果也是 NULL（`total = price * qty` 在 `price` 为 NULL 的那一行把 `total` 写成 NULL），**不会**中断整张表的生成。目标列若在库里是 `NOT NULL`，会在写入时报数据库错误。除零/类型错误仍是硬错误：那是配置写错，不是 NULL 输入。
- 时机：**所有列生成（含 copula、FK、NULL、`fixed`/`values`/`fixed_range`）之后统一求值**，因此表达式读到的是最终值；`derive` 之间按依赖顺序求值（`c = b + 1`、`b = price * 2` 可以声明为任意顺序），成环在加载期报错。
- 输出按目标列的类型量化：整数列取整为 i64；带 `decimal_scale` 的列量化到该标度（避免 `110.16999999999999` 这类二进制尾差写入 CSV/SQL）。
- 冲突校验（**加载期**，错误含表名+列名）：目标列同时有 `fixed`/`values`/`fixed_range`、目标是 relationship 的 `pk`、目标是被其他表 `references` 的父键、目标重复、derive 成环。**未知列在生成期报错**：`table.columns` 只记录覆盖项，列的存在性要拿模型才判得出来（`table 'orders' derive 'amount': unknown column 'x'`）。
- 不含 `derive` 的 rules 输出与之前逐字节一致。

#### 分支覆盖率（`branches`，issue #70）

```yaml
tables:
  - name: orders
    branches:
      - id: paid                 # 报告里的标识
        predicate: "status == 'A'"   # 布尔表达式（白名单同 derive）
        target_ratio: 0.30        # 目标命中比例
        tolerance: 0.02           # 可选，默认 0.05
        repair:
          set:
            status: "A"           # 把未命中行改写成这些字面量
          linked_derive_recompute: true
```

- 时机：`derive` 之后的最后一个阶段。先度量谓词命中率，未达目标就改写未命中的行，再重算 `derive`，最多 10 轮；命中率进入报告（stdout 为 Pass，stderr 为 `warning:` 的 Warn/Fail），**不阻断主流程退出码**（exit 仍为 0）。
- 改写行的选择是确定性的：在未命中行里按等距抽取，避免把改动堆在表头；`set` 只能让行**命中**谓词，因此只从下方补齐，超出目标不会被「反向撤销」。
- 谓词读 `derive` 列时（如 `predicate: "total > 50"` 配合 `derive: total = price * qty`），判定「某次改写会不会破坏未达标兄弟的覆盖」会在**单行上重跑同一条 derive**：`set` 不能写 derive 目标，但可以写它的**输入**，而 derive 只在轮末整体刷新。不重跑 derive 会漏判（把 `price` 写成 0 看起来无害，实际把 `total > 50` 的命中抹掉），也会让兄弟分支覆盖掉刚为本分支写下的行。
- 可写列白名单（错误含表名、分支 id 与列名）：不能写 FK 列、被其它表 `references` 的父键、`derive` 目标列、以及被 `fixed`/`values`/`fixed_range` 钉住的列。这些关系在 YAML 里就能判定，因此都在**加载期**报错；只有「列是否存在」要等模型，在**生成期**报错。
- 多分支同轮：一个分支只在改写**会破坏某个未达标兄弟的命中**时才被拦（`set` 只能让行命中，未达标的兄弟补不回来）。**超目标的兄弟不是拦截对象**，它的富余行可以被其他分支转化——互补谓词（二值列上 `A` 0.3 与 `B` 0.7）正是靠这一点在一轮内收敛，而不是把整张表锁死。改写不涉及兄弟谓词所读的列时同样放行，因此两个分支可以叠加在同一批行上（`status == 'A'` 90% 与 `qty == 99` 50% 可同时达标）。两个分支争抢同一列且都未达标时不再互相覆盖到最后一轮（此前只有最后声明的分支达标）。
- 超目标只能被其他分支的转化消化，`set` 本身不会「反向撤销」超射（见下一条）。
- 一轮下来命中率没有任何变化（典型是 `set` 写的列与谓词无关）会立即停止并报 Warn，不会空转到 10 轮；若**所有**行都无法求值（例如字符串列写了 `predicate: "name == 1"`），直接报错而不是伪装成 0% 覆盖。
- 与影子数据的差异（诚实说明）：本实现只做 `set` 补齐与 `derive` 联动重算，**不做** `derive` 表达式形式的 `set`、不做多轮最小扰动选行、不做「反向撤销」。不含 `branches` 的 rules 输出与之前逐字节一致。

#### 生成后分布校验（诚实 WARN，#76-C）

- **值池**：生成结束后统计每个 `values` 池各声明值的实际占比与声明权重，最大偏差 > 0.05 时向 stderr 打印 `warning:`，并列出 declared 与 actual 两列。
- **分支**：每个 `branches[]` 的命中率与 `target_ratio` 的偏差超过 `tolerance` 时同样按 `warning:` 输出（见上一节）。
- 两者都是**告警不阻断**：退出码仍为 0，生成结果照常落盘。小样本（如 20 行）上 0.05 的容差本就容易被采样噪声触发，这是有意的——它提示「这批数据在这个规模上并不复现你声明的分布」。

#### rules-draft：隐式引用推断与规则挖掘

- **隐式引用推断**（有 `--models` 时默认开启）：库中没写外键时，若子表列与父表列**精确同名**，且父表该列在训练 profile 中唯一（`cardinality == row_count`）、子表该列不是自身主键，则推断出一条 relationship，与数据库外键结果去重，并在 stderr 汇总 `inferred N implicit relationship(s)`。不做后缀猜测或模糊匹配。
  - 推断出的 `unique` **跟随子表列**，与显式外键同一判据：子表该列在 profile 中唯一（`cardinality == row_count`，即 1:1）才是无放回采样的 `unique: true`；1:N 一律 `unique: false`，否则子表行数超过父表池时会报 `exhausted its parent pool`。
- **条件规则挖掘**（`--mine`，默认关闭）：对类别列对 `(A, B)`，统计每个高频值 `a` 下 `B` 的条件分布与边缘分布的 TV 距离，`TV > --mine-support` 且最大条件取值占比 `≥ --mine-confidence` 时输出候选 `A=a => B=b`（附 confidence / support）。
  - 候选**只写入 YAML 注释或 `--emit-candidates` 指定文件，永不自动启用**；不传 `--mine` 时 draft 输出与之前完全一致。
  - 唯一值占该列非 NULL 行数过半的列（id、单据号等）视为标识符不参与，避免小样本上「每个取值都蕴含一条规则」的伪候选；`--mine-max-pairs` 限制列对数量（超出会记录并截断）；同输入同参数候选清单逐位一致。
  - 挖掘结果是**相关性观测，不是因果**，也未必是业务约束；启用前请人工确认，把它当作 `rules` / `columns` 的候选来源而不是自动配置。

### 10.5 语义与限制

- 表按外键依赖拓扑排序生成；检测到循环依赖直接报错并列出环路径
- 同一 `--seed` 下每张表派生独立随机流（djb2 混淆），同名表跨运行可复现
- 数值列：**低基数且值重复出现**的数值列（如 19 档离散价格）按观测档位拟合分类分布（`top_values`），生成值保持在观测档位上并保留数值类型；其余数值列在 `normal` / `beta` / `gamma` / `uniform` / `ecdf` 五个候选里按**留出集上的 KS 统计量**自动择优。ECDF 是训练样本的均匀概率网格分位（每列至多 512 个 knot，序列化后单列 ≤16KB，超预算时 train 打印告警），用来还原偏态、多峰与零堆积形状；因为 ECDF 在拟合样本上按构造必胜，择优在留出集上打分，并要求 ECDF 相对最优参数族有 ≥20%（或绝对 ≥0.005）的优势才胜出，否则保持参数族（体积小）。整数列仍生成取整值
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
- 质量报告（`synth report`）以 `1-KS`（数值列）/ `1-TV`（类别列）/ Pearson-Δ 与 joint-TV（`baseline` 里登记过的列对）/ FK join-rate 打分，总分是各表已打分节的均值。留出集摘要只含聚合量（分位点、频次、相关系数），泄漏敏感性与 `top_values` 同级。类别列档数超过 50 时分数照常显示但 `counted: false`，不计入均值：数百档上两个多项分布的 TV 在完美模型下也接近 1，计入只会淹没真实信号。`model.pk` 列同理 `counted: false`：#103 起主键在所有格式下强制唯一，行数超过观测键空间时会外推、KS 必然下降——那是**可装载性约束**而非保真度问题，计入只会掩盖其他列的真实信号。缺 baseline（旧模型）时对应节输出 `status: skipped` 与原因、退出码仍为 0，加 `--strict` 才报错；某表的生成数据缺失同样记为 skipped 表，并同时受 `--strict`（报错）与 `--min-score`（该表无分即失败）约束
- 纯 Rust Oracle 后端（oracle-rs 0.1.7）存在驱动缺陷：查询超过 100 行被静默截断。`synth train` 仅在**请求行数超过 100（`--sample` 默认 10000）且实际采样恰好 100 行**时认定被截断：向 stderr 打印 WARNING（含「分布可能失真」），并把 `{table}.model.json` 的 `provenance.truncated` 设为 `true`。`--sample 100`（或更小）是调用方自己的上限，不算截断；采样不足 100 行或非 Oracle 连接也不警告。该判定是启发式：恰好只有 100 行的表在请求更多行时仍会误报。native OCI 后端不受影响但当前无法从配置强制选择（见 `tests/benchmark/REPORT.md`）
- SQL 导出携带引用标识符与列名：MySQL 反引号、Oracle 双引号并折叠为大写、GaussDB 双引号小写。`synth train --schema S` 写入 `TableModel.schema`，`generate --format sql` **默认**输出 `INSERT INTO "S"."t"`（标识符按方言引用）；`--no-schema-qualifier` 恢复旧的无前缀语句
- `train` 与 `rules-draft` 的 `--schema` 语义一致：显式值优先，缺省时都取连接默认 schema（`side_schema_from_conn`），不再分别回落到 `current_schema`

#### 已知限制与实测注意事项（2026-09 全链路核对）

以下条目均在 MySQL 8 / GaussDB(opengauss 5.0.0) / Oracle 26ai / DuckDB 四后端上实测得出，使用前请留意：

1. **`--tables` 不接受 `schema.table` 点号形式（fail-fast 拒绝）**：点号写法会在连接前被直接拒绝（GaussDB 实测原文）：``--tables entry 'staging.customer' uses schema-qualified `schema.table` notation, which is not supported; pass the table list without the qualifier and select the schema with `--schema staging` (table: 'customer')``。跨 schema 请始终用 `--schema`。
2. **TOML `[connections.X]` 不支持 `options` 字段**：写了会被静默忽略；`schema` 字段自 0.5.x 起已生效（优先级：`--schema` > 连接段 `schema` > 驱动探测）。synth 省略 `--schema` 且连接段未配 `schema` 时的默认推断是：MySQL 取 URL 里的数据库名，Oracle 取 `SYS_CONTEXT('USERENV','CURRENT_SCHEMA')`，GaussDB/DuckDB 取 `current_schema()`。GaussDB URL 也不接受 `?currentSchema=` 查询参数（驱动报 `unknown option currentSchema`）。
3. **rules-draft 隐式推断已跳过 datetime 列**：`last_update` 这类多表同名的更新时间戳列不再被连成互指关系（推断层跳过 datetime 子列，自引用一律跳过）。若 draft 仍因其他原因成环，落盘前会打 `warning: draft references form a cycle ...`（含同一错误文案里点名的两类常见误报），此时按提示手工删除残余伪 relationship 再 `generate`。
4. **`--mine` 默认跳过 PII 列**：挖掘器在「档数 ≤ 50 且非标识符」之外还会用 PII 识别过滤列（被跳过的列名打在 stderr），低基数的 email/phone 列不会再把**训练原值**写进 YAML 注释与 `--emit-candidates` 文件。确知数据是假 PII 形状时可加 `--keep-pii-columns` 恢复旧行为。
5. **父键 `unique: true` 受父池大小约束**：生成开始前预检——需求唯一值数超过父表**已生成**行数即报错并给出两条线索：父池当前大小、父列训练时观测到的 distinct 容量。容量足够时提示增大父表 rules 行数，容量不足时提示增大 train `--sample` 重新训练；也可以改 `unique: false` 或给父键配 `values` 池。例：users rules 只生成 3 行、orders `unique: true` 请求 300 行会直接报 `unique FK 'user_id' requests 300 unique value(s) but its parent pool 'users.id' holds only 3 generated value(s)`。
6. **`derive` 表达式类型必须与目标列匹配**（#94 起支持条件函数与布尔目标列）：表达式白名单新增**条件函数 `if(cond, then, else)`**——`cond` 必须是布尔表达式（比较/逻辑组合），两个分支静态类型须一致（数字或字符串），**惰性求值**（未被选中的分支不参与求值，其中的除零不会触发）。比较/逻辑表达式（结果为布尔）可以直接 `derive` 进**布尔目标列**：训练后档位恰为 `true`/`false` 文本的列（即真实 BOOLEAN 列的形态）会按表达式真值生成 `"true"`/`"false"` 文本（与该列既有导出载体一致）；布尔表达式配数值目标、数值/字符串表达式配布尔目标，以及**字符串结果配任意非布尔目标**（字符串分支只用于比较，没有字符串写出路径），都会在生成前报错并点名目标列。未知函数名仍然 fail-fast 拒绝并给出已知函数列表。函数白名单当前：`if`。
7. **PII phone provider 固定美式格式**：始终生成 `+1-XXX-XXX-XXXX`，不随训练值地域变化（训练值 `13812345678` 这类中文手机号在生成结果中为 0% 格式匹配）。需要本地格式时可 `sdtype: keep`（保留值域，注意泄漏面）或导出后自行变换。
8. **Oracle（oracle-rs 驱动）**：表不存在时报 `Oracle closed the connection without an error packet…`，语义误导（实为对象不存在被驱动吞掉）；采样超 100 行会被驱动静默截断（见上文截断告警逻辑）。
9. **DuckDB**：连接串指向的数据库文件必须已存在，CLI 不自动创建（报 `DuckDB database file not found`）；DuckDB 引擎不支持 `ALTER TABLE … ADD CONSTRAINT … FOREIGN KEY`，外键必须在建表 DDL 中内联，`rules-draft` 才能扫到。
10. **synth 全部子命令写审计日志**：channel=`synth`、class=`meta`，detail 记录子命令名与 models/rules 路径，不含任何生成数据行（见 §4.5）。
11. **`report --against-db` 的连接名**：用 `HEPTA_DBCLI_URL` 环境变量连接时连接名固定为 `default`（不是配置文件中的名字），此时 `--against-db` 必须写 `default`；写成其他名字会在运行前收到 stderr 告警（该名字被忽略），要指名连接请改用 `--config` 文件。

### 10.6 基准测试与评测

| 基准 | 内容 | 报告 | 复现入口 |
|------|------|------|----------|
| Case A | 合成 4 列高斯 Copula，对标 SDV `GaussianCopulaSynthesizer(norm)` | [tests/benchmark/REPORT.md](../tests/benchmark/REPORT.md) | `tests/benchmark/run_case_a.sh` |
| P1 | SynMeter 真实单表（**仅 Adult**）：Wasserstein / MLA / QueryError 相对门禁（hepta ≤ SDV-GC × 1.15） | [tests/benchmark/p1/REPORT.md](../tests/benchmark/p1/REPORT.md) | `tests/benchmark/p1/run_p1.sh` |
| P2 | ogagila pagila 三表（customer–rental–payment）：门禁 = 可插入 0 错误、孤儿 FK = 0、**payment.amount on-grid ≥ 0.95**；P2-2 每 customer 扇出 KS **仅记录**（uniform 0.1888 / zipf 0.7238，empirical fan-out 不在本里程碑）；1-hop 相关仅记录 | [tests/benchmark/p2/REPORT.md](../tests/benchmark/p2/REPORT.md) | `tests/benchmark/p2/run_p2.sh` |
| M1 验收 | synth M1 端到端（真实 MySQL fixture）：datetime 格式还原与值域、NULL 比例复现、DECIMAL 标度、字典列全档、FK 引用完整性、SQL schema 限定、同 seed 逐字节一致 | [tests/synth-verify/README.md](../tests/synth-verify/README.md) | `HEPTA_DBCLI_TEST_URL=... bash tests/synth-verify/run_m1.sh` |
| M2 验收 | synth M2 端到端（同一 fixture）：边际自动择优（非整表 Normal）、留出集摘要隐私、report 的 shapes/pairs/fk 三节、劣化检出与 `--min-score` 退出码、离线（生成父键）与真库（`--against-db`）FK join-rate、缺 baseline / 缺数据的 skip 与 `--strict`、两次报告逐字节一致 | [tests/synth-verify/README.md](../tests/synth-verify/README.md) | `HEPTA_DBCLI_TEST_URL=... bash tests/synth-verify/run_m2.sh` |
| M4 验收（#82） | SQL 导出主键唯一：12 键表 `--rows 200` 外推后可回灌 200 唯一 id、20×20 复合键 200 唯一元组、非数值键 fail-fast 且报列名+行数 | [tests/synth-verify/README.md](../tests/synth-verify/README.md) | `HEPTA_DBCLI_TEST_URL=... bash tests/synth-verify/run_m4_pk.sh` |
| M4 验收（#72） | 子表基数：模型学习分布精确 `{0:.5,1:.3,2:.2}`、modeled 生成形状复现（零占比 [0.4,0.6]、TV<0.1）、report 带 `cardinality tv`、默认 exact_rows 保持 `--rows` | [tests/synth-verify/README.md](../tests/synth-verify/README.md) | `HEPTA_DBCLI_TEST_URL=... bash tests/synth-verify/run_m4_cardinality.sh` |
| M4 验收（#71） | PII：模型/剖面双泄漏面清除、生成值与训练值零交集且格式合法、同 seed 可复现、stable_mapping 同档同值、`sdtype: keep` 保留值域 | [tests/synth-verify/README.md](../tests/synth-verify/README.md) | `HEPTA_DBCLI_TEST_URL=... bash tests/synth-verify/run_m4_pii.sh` |

CI：`.github/workflows/synth-benchmark.yml`——每周 cron 只跑 P1-adult（零外部服务）；Case A / P2 为 `workflow_dispatch` 且需仓库变量 `OGAGILA_DIR`（ogagila 检出 URL）。门禁断言决定 job 成败，报告作为 artifact 上传。on-grid 门禁在 P2 强制执行（P1 不含 payment 表）。

范围声明：Case B（vs CTGAN / TVAE / TabDDPM / GReaT）与 Case C（vs SDV HMA / ClavaDDPM / REaLTabFormer）**不在本里程碑**；SynMeter / torch / SDV 仅存在于 benchmark venv（`tests/benchmark/requirements.txt`），不进入 `Cargo.toml`。

### 10.7 能力矩阵（对照 SDV / shadow-seed）

图例：✅ 开箱可用；⚠️ 部分/需额外组件；❌ 不具备。仅列 synth 相关能力，均为 M1-M4 结束后的状态（2026-09）。

| 能力 | hepta-dbcli | SDV（社区版） | shadow-seed |
|------|-------------|---------------|-------------|
| 便携模型（纯 JSON，无 Python） | ✅ | ❌（pkl / Python 栈） | ❌ |
| 单表边际 + copula | ✅ Normal/Beta/Gamma/Uniform/ECDF 自动择优 | ✅ 多种合成器 | ✅ copula |
| 日期/时间列 | ✅ 格式还原 + epoch 边际 | ✅ | ⚠️ 部分 |
| NULL 复现（含 pairwise-complete 相关修正） | ✅ | ✅ | ⚠️ 部分 |
| FK 图拓扑排序 + 引用完整性 | ✅ | ✅ | ✅（固定模式） |
| 子表基数建模（HMA-lite） | ✅ `cardinality: modeled` | ✅ HMA 全量 | ❌ |
| 主键唯一性（全部格式可回灌） | ✅ 含数值外推 | ✅ 内建 id 处理 | ✅ |
| PII 识别 + 不可逆匿名化 | ✅ email/phone/name/id_card，默认匿名、可 `keep` | ✅ AnonymizedFaker（40+ locale） | ❌（有意保留真实键值） |
| 可逆伪匿名化 | ❌（明确不做，见 #73） | ✅ PseudoAnonymizedFaker | ❌ |
| 差分隐私保证 | ❌（明确不做） | ⚠️ 企业版 | ❌ |
| 条件规则 / 固定值 / 派生列 / 分支覆盖 | ✅ `fixed`/`values`/`fixed_range`/`derive`/`branches` + rules-draft 2.0 | ⚠️ constraints 子集 | ✅ 业务规则修复层 |
| 离线质量报告（留出集 KS/TV/pairs/FK/基数 TV） | ✅ `synth report` | ✅ SDMetrics | ❌ |
| 多方言直连训练（MySQL/Oracle/GaussDB/DuckDB） | ✅ | ❌（只吃 DataFrame） | ❌ |
| 部署形态 | ✅ 单二进制，无 Python | ❌ Python 依赖栈 | ❌ |

当前 `synth report` 基线（M1 fixture：日期/金额/字典/可空/FK 多表）为 **0.733**，明细见 [docs/plans/2026-09-16-synth-report-baseline.md](../docs/plans/2026-09-16-synth-report-baseline.md)；路线的 ≥0.85 目标尚未达成（差在 FK 列与高基数类列的 1-KS/1-TV）。

---

## 11. 数据回灌 (load)

把 `synth generate` 的产物（或任何同结构的 JSONL / JSON 数组 / CSV 文件）批量装回**已存在**的数据库。CLI-only，需要 `--allow-write`；MCP 不暴露。

**合同：`load` 只装数据，不做 schema 映射、不做 DDL。** 目标表必须已存在，数据文件的列名和类型必须与目标表匹配（CSV 表头按名字对齐，顺序可以不同）。

```bash
# 装载目录下全部 *.jsonl|json|csv 文件，FK 安全顺序（父表先装）
hepta_dbcli --allow-write load --name dev --data synth-out

# 只看计划（顺序、文件、行数），不写库
hepta_dbcli --allow-write load --name dev --data synth-out --dry-run

# 只装子集；用 --schema 限定目标 schema
hepta_dbcli --allow-write load --name dev --data synth-out \
  --tables users,orders --schema testdb

# 指定格式（默认 auto：jsonl → json → csv）
hepta_dbcli --allow-write load --name dev --data synth-out --format csv
```

要点：

- 每张表一个事务，失败快速回滚；多表装载时后失败的表不会回滚已完成的表（错误信息会列出 completed 集合，并附恢复 hint：用 `--tables` 只补装失败的表，或先清空已完成表再全量重跑——盲跑全量会撞已完成表的 PK）。
- 文件只匹配**目标 schema** 下的表（#106）：显式 `--schema` 优先，否则用连接的默认 schema；文件基名在该 schema 下找不到对应表即报错（load 永不建表/建 schema）。要跨 schema 匹配裸表名时，请选一个默认 schema 为空/更宽的连接或显式传 `--schema`。
- CSV 语义：未加引号的空字段 = NULL，`""` = 空字符串；引号内逗号/换行/转义按 RFC 4180。
- 整数值必须落在目标列类型范围内（GaussDB 绑定溢出会报错而不是截断）。
- 连接选择用 `--name`；不存在的连接名直接报错退出，不会静默落到默认连接。
- 每张表写 intent/outcome 审计事件（只有行数，不含行数据）。

---

## 12. 进阶用法

### 12.1 多连接切换

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

### 12.2 超时控制

```bash
# 设置单条 SQL 最大 5 分钟
hepta_dbcli cli --statement-timeout 5min --sql "SELECT SLEEP(10)"

# 设置连接 10 分钟后自动回收
hepta_dbcli cli --connection-max-lifetime 10min --sql "..."

# 超时后断开连接（而非保持）
hepta_dbcli cli --timeout-action disconnect --sql "..."
```

### 12.3 使用 PolarDB-X

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

### 12.4 本地三后端 Docker

仓库 `tests/` 下有现成 compose 与 TOML：

```bash
docker compose -f tests/docker-compose.yml up -d mysql oracle gaussdb

hepta_dbcli --config tests/docker-all.toml check --name mysql
hepta_dbcli --config tests/docker-all.toml check --name oracle
hepta_dbcli --config tests/docker-all.toml check --name gaussdb
```

GaussDB 测试配置见 `tests/docker-gaussdb.toml`（`sslmode = "disable"`）。

### 12.5 在脚本中使用

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

## 13. 错误排查

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
| synth ``--tables entry 'x.y' uses schema-qualified `schema.table` notation`` | `--tables` 写了 `schema.table` 点号形式（fail-fast 拒绝） | 拆开：`--tables customer --schema staging` |
| synth `unique FK '...' requests N unique value(s) but its parent pool '...' holds only M generated value(s)` | `unique: true` 父表生成行数（或训练观测容量）< 请求行数 | 按报错里的两条线索：父表 rules 行数不够就增大行数，训练观测不够就增大 train `--sample` 重训；也可改 `unique: false` 或给父键配 `values` 池（见 §10.5 已知限制 5） |
| synth rules-draft 报 `warning: draft references form a cycle` | 残余的隐式误报关系成环（同名时间戳已被跳过；自引用也已被跳过） | 按警告提示手工删除 draft YAML 中的伪 relationship（见 §10.5 已知限制 3） |
| synth derive 报 `boolean expression`/`boolean column`/`string expression` 类型不匹配（点名表.列） | derive 表达式结果类型与目标列档位不兼容（布尔/字符串表达式配数值列，或数值/字符串表达式配 `true`/`false` 布尔列） | 按提示改成同类型表达式；布尔真值应配布尔目标列（#94 起支持），或用**无引号**的 `if(cond, 1, 0)` 把结果编码成数值（带引号的 `if(cond, '1', '0')` 会被计划期拒绝——字符串分支只用于比较，见 §10.5 已知限制 6） |
| synth derive 报 `function \`xxx\` is not permitted; known functions: if` | 用了白名单外的函数 | 首批白名单仅 `if`，其余函数暂不支持（见 §10.5 已知限制 6） |
| GaussDB synth 连接报 `unknown option currentSchema` | URL 查询参数不被 gaussdb 驱动接受 | 去掉参数，改用 `--schema` |
| DuckDB `database file not found` | CLI 不自动创建 DuckDB 文件 | 先用任意 DuckDB 客户端建库再连接 |

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
hepta_dbcli load --help

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
hepta_dbcli delta-diff --left-url duckdb:///tmp/a.duckdb --right mysql_dev --table orders
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
