# hepta_dbcli 用户指南

当前版本：**0.4.4**。CLI + MCP Server，覆盖 MySQL / PolarDB-X / Oracle / GaussDB，并提供跨库表数据比对（`delta-diff`）。

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
10. [进阶用法](#10-进阶用法)
11. [错误排查](#11-错误排查)

---

## 1. 安装

### 二进制下载

从 [GitHub Releases](https://github.com/c2j/hepta-dbcli/releases) 下载对应平台的预编译二进制：

- `hepta_dbcli-{version}-x86_64-unknown-linux-gnu.zip`
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

默认 feature 已包含 `oracle-rs`、`oracle`、`gaussdb`。连接 Oracle 11g 时会回退到 `oracle` crate，需要本机安装 [Oracle Instant Client](https://www.oracle.com/database/technologies/instant-client.html) 并配置动态库路径。12c+ 走纯 Rust 的 `oracle-rs`，无需 Instant Client。

验证安装：

```bash
$ hepta_dbcli --version
hepta_dbcli 0.4.4
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

### 3.4 Oracle / GaussDB 连接

`driver` 决定 URL scheme 与默认端口。省略时为 `mysql`。

| driver | scheme | 默认端口 |
|--------|--------|----------|
| `mysql`（默认） | `mysql://` | 3306 |
| `oracle` | `oracle://` | 1521 |
| `gaussdb` | `gaussdb://` | 5432 |

`database` 同时接受别名 `dbname`（方便 GaussDB / PostgreSQL 习惯）。

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

### 6.3 详细检查（MySQL）

```bash
hepta_dbcli check --verbose
```

`--verbose` 模式额外显示：服务器版本、当前用户、当前数据库、字符集、排序规则、连接耗时。

### 6.4 通过 CLI 子命令检查

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
| `delta_diff` | 跨库表比对，返回 JSON 报告。只读、不写文件 |

`execute_query` 自动追加行数限制：MySQL / GaussDB 为 `LIMIT N`，Oracle 12c+ 为 `FETCH FIRST N ROWS ONLY`，Oracle 11g 为 `ROWNUM`。`max_rows` 默认 1000，上限 10000。

### 8.3 安全限制

- **只读执行**（按方言）：
  - MySQL / PolarDB-X：`SELECT`、`EXPLAIN`、`SHOW`、`DESCRIBE`、`DESC`
  - Oracle / GaussDB：`SELECT`、`EXPLAIN`、`WITH`
- **行数限制**：默认 `LIMIT 1000`，可通过 `max_rows` 调整（上限 10000）
- **超时控制**：支持按查询设置 `timeout_ms`
- **delta_diff**：只做比对，不导出文件、不写 checkpoint、不做增量窗口、不生成 SQL 补丁。这些能力在 CLI（见第 9 节）

### 8.4 `delta_diff` 工具参数

必填：`left_connection`、`right_connection`、`table`。

可选：`left_table` / `right_table`、`schema` / `left_schema` / `right_schema`、`key_columns`、`columns`、`where_condition`、`strategy`（`auto` / `hashdiff` / `joindiff` / `bucketdiff` / `iblt` / `keyeddiff`）、`consistency`（`snapshot` / `none`）、`recheck`、`sample_limit`（默认 1000）、`summary_only`。

`where_condition` 禁止包含分号。MCP 返回的差异样本上限由 `sample_limit` 裁剪；CLI 终端默认只显示 20 行（`--sample`），全量走 `--export`。

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

## 10. 进阶用法

### 10.1 多连接切换

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

### 10.2 超时控制

```bash
# 设置单条 SQL 最大 5 分钟
hepta_dbcli cli --statement-timeout 5min --sql "SELECT SLEEP(10)"

# 设置连接 10 分钟后自动回收
hepta_dbcli cli --connection-max-lifetime 10min --sql "..."

# 超时后断开连接（而非保持）
hepta_dbcli cli --timeout-action disconnect --sql "..."
```

### 10.3 使用 PolarDB-X

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

### 10.4 本地三后端 Docker

仓库 `tests/` 下有现成 compose 与 TOML：

```bash
docker compose -f tests/docker-compose.yml up -d mysql oracle gaussdb

hepta_dbcli --config tests/docker-all.toml check --name mysql
hepta_dbcli --config tests/docker-all.toml check --name oracle
hepta_dbcli --config tests/docker-all.toml check --name gaussdb
```

GaussDB 测试配置见 `tests/docker-gaussdb.toml`（`sslmode = "disable"`）。

### 10.5 在脚本中使用

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

## 11. 错误排查

### 11.1 常见错误

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

### 11.2 诊断流程

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

### 11.3 TLS 证书问题

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
```
