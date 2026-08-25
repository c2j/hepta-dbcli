# hepta_dbcli 用户指南

## 目录

1. [安装](#1-安装)
2. [快速开始](#2-快速开始)
3. [配置详解](#3-配置详解)
4. [命令行模式](#4-命令行模式-cli)
5. [交互模式](#5-交互模式-repl)
6. [连接检查](#6-连接检查-check)
7. [密码管理](#7-密码管理)
8. [MCP 服务器模式](#8-mcp-服务器模式)
9. [进阶用法](#9-进阶用法)
10. [错误排查](#10-错误排查)

---

## 1. 安装

### 二进制下载

从 [GitHub Releases](https://github.com/YOUR_ORG/dbcli/releases) 下载对应平台的预编译二进制：

- `hepta_dbcli-{version}-x86_64-unknown-linux-gnu.zip`
- `hepta_dbcli-{version}-aarch64-unknown-linux-gnu.zip`
- `hepta_dbcli-{version}-x86_64-pc-windows-msvc.zip`

### 源码编译

```bash
git clone https://github.com/YOUR_ORG/dbcli.git
cd dbcli
cargo build --release -p polar-mysql
# 二进制位于: target/release/hepta_dbcli
```

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

> **💡 提示**：推荐使用配置文件管理连接（见下一节），密码会自动迁移到系统钥匙串。

---

## 3. 配置详解

### 3.1 配置文件位置

- 默认路径：`~/.hepta-dbcli.toml`
- 自定义路径：通过 `--config <PATH>` 指定
- 环境变量：`HEPTA_DBCLI_URL`（优先级最高）

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

使用 `[connections.NAME]` 语法配置多个数据库连接，每个连接需要包含 `name` 字段：

```toml
default_connection = "dev"

[connections.dev]
name = "dev"
host = "127.0.0.1"
port = 3306
user = "root"
password = "keyring"
database = "mydb"

[connections.prod]
name = "prod"
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

### 3.4 环境变量模式

适合临时使用或脚本场景：

```bash
export HEPTA_DBCLI_URL="mysql://user:password@host:port/database"
```

使用环境变量时，连接名固定为 `default`，不使用钥匙串。

### 3.5 超时设置

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
name = "slow_db"
host = "..."
statement_timeout = "5min"
connection_max_lifetime = "30min"
```

也可通过命令行参数覆盖（仅 CLI 模式）：

```bash
hepta_dbcli cli --statement-timeout "5min" --connection-max-lifetime "10min" --sql "..."
```

### 3.6 SSL/TLS 连接

在连接 URL 中添加 SSL 参数：

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

---

## 4. 命令行模式 (CLI)

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
┌─────────┬────────────────────┐
│ name    │ email              │
├─────────┼────────────────────┤
│ Alice   │ alice@example.com  │
│ Bob     │ bob@example.com    │
│ Charlie │ charlie@example.com│
│ Diana   │ diana@example.com  │
└─────────┴────────────────────┘
(4 rows)
```

### 4.3 从标准输入读取 SQL

```bash
$ echo "SELECT COUNT(*) AS total_users FROM users" | hepta_dbcli cli
┌─────────────┐
│ total_users │
├─────────────┤
│ 4           │
└─────────────┘
(1 row)
```

### 4.4 输出格式

支持四种输出格式，通过 `--format` 指定：

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
hepta_dbcli cli -i --name prod
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

### 6.1 基本检查

```bash
$ hepta_dbcli check
Connection: default

[Keyring] Password read from OS keychain (user: mcp/default)
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

`check` 命令会自动尝试三种连接方式：
1. **无 TLS**（plain TCP）
2. **TLS（跳过证书验证）**
3. **TLS（验证证书）**

### 6.2 详细检查

```bash
$ hepta_dbcli check --verbose
Connection: default

[Keyring] Password from config file (plaintext)
  OS keychain is available -- password will be migrated on first successful connection

[1/3] Connecting without TLS (plain TCP) ...
  ✓ NoTls  — 5ms  8.4.10
  [verbose] Connection Details:
    server_version           8.4.10
    current_user             mcp@%
    current_database         testdb
    charset                  utf8mb4
    collation                utf8mb4_0900_ai_ci
    connect_time             5ms
...
```

`--verbose` 模式额外显示：服务器版本、当前用户、当前数据库、字符集、排序规则、连接耗时。

### 6.3 通过 CLI 子命令检查

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
Password stored in OS keychain for 'root/default' (connection: 'default').
```

```bash
# 为指定连接存储密码
$ hepta_dbcli store-password --name prod
Enter password: 
Confirm password: 
Password stored in OS keychain for 'readonly/prod' (connection: 'prod').
```

### 7.2 自动迁移

首次使用含明文密码的配置文件成功连接数据库后，系统会自动：

1. 将密码存入操作系统钥匙串
2. 将配置文件中的 `password` 字段改写为 `"keyring"`

整个过程透明无需手动操作：

```
[Keyring] Password from config file (plaintext)
  OS keychain is available -- password will be migrated on first successful connection

...连接成功后，配置文件自动更新...
```

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
| `execute_query` | 执行只读 SQL（SELECT/EXPLAIN/SHOW/DESCRIBE/DESC） |
| `get_execution_plan` | 获取 SQL 执行计划（支持 EXPLAIN / EXPLAIN ANALYZE，TEXT/JSON 格式） |
| `list_connections` | 列出所有配置的连接及其状态 |

### 8.3 安全限制

- **只读执行**：`execute_query` 只允许 `SELECT`、`EXPLAIN`、`SHOW`、`DESCRIBE`、`DESC` 开头的语句
- **行数限制**：默认自动追加 `LIMIT 1000`，可通过 `max_rows` 参数调整（上限 10000）
- **超时控制**：支持按查询设置 `timeout_ms`

### 8.4 Claude Desktop 配置示例

在 `claude_desktop_config.json` 中添加：

```json
{
  "mcpServers": {
    "hepta_dbcli": {
      "command": "/usr/local/bin/hepta_dbcli",
      "args": ["--config", "/home/user/.polardb-mysql.toml"]
    }
  }
}
```

---

## 9. 进阶用法

### 9.1 多连接切换

```bash
# CLI 模式切换连接
hepta_dbcli cli --name dev --sql "SELECT * FROM users"
hepta_dbcli cli --name prod --sql "SHOW PROCESSLIST"

# REPL 模式内切换
hepta_dbcli cli --interactive
$ .connect prod
hepta_dbcli interactive -- connected to 'prod'
```

### 9.2 超时控制

```bash
# 设置单条 SQL 最大 5 分钟
hepta_dbcli cli --statement-timeout 5min --sql "SELECT SLEEP(10)"

# 设置连接 10 分钟后自动回收
hepta_dbcli cli --connection-max-lifetime 10min --sql "..."

# 超时后断开连接（而非保持）
hepta_dbcli cli --timeout-action disconnect --sql "..."
```

### 9.3 使用 PolarDB-X

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

### 9.4 在脚本中使用

```bash
#!/bin/bash
# 查询用户数并判断
COUNT=$(hepta_dbcli cli --format json --sql "SELECT COUNT(*) AS c FROM users" | jq -r '.rows[0][0]')
if [ "$COUNT" -gt 100 ]; then
    echo "用户数超过 100: $COUNT"
fi
```

---

## 10. 错误排查

### 10.1 常见错误

| 错误信息 | 原因 | 解决方案 |
|----------|------|----------|
| `No connection configuration found` | 未找到配置文件或环境变量 | 创建 `~/.hepta-dbcli.toml` 或设置 `HEPTA_DBCLI_URL` |
| `Connection failed` | 数据库不可达或凭据错误 | 使用 `hepta_dbcli check --verbose` 诊断 |
| `Connection 'xxx' not found` | 指定的连接名在配置中不存在 | 检查配置文件的 `[connections.xxx]` 段 |
| `keyring password not found` | 钥匙串中无密码且配置文件未含明文密码 | 执行 `hepta_dbcli store-password` |
| `Only SELECT, EXPLAIN, SHOW, and DESCRIBE queries are allowed` | MCP 模式仅允许只读查询 | 使用 CLI 模式（`hepta_dbcli cli`）执行写操作 |
| `No SQL provided` | 未提供 SQL 且 stdin 为空 | 使用 `-s`、`-f` 参数或管道传入 SQL |

### 10.2 诊断流程

```bash
# 1. 检查配置文件是否能正确解析
hepta_dbcli check --config /path/to/config.toml

# 2. 详细连接诊断
hepta_dbcli check --verbose

# 3. 检查钥匙串状态
# macOS: 打开"钥匙串访问"，搜索 "hepta_dbcli"
# Linux: secret-tool search service hepta_dbcli

# 4. 查看日志
cat ~/.local/share/hepta-dbcli/hepta-dbcli.log
```

### 10.3 TLS 证书问题

如果 `TLS(verify)` 失败但 `NoTls` 和 `TLS(skip-verify)` 成功，说明服务器 TLS 证书配置有问题。可以：

1. 使用无 TLS 连接（如果网络环境安全）
2. 使用 `TLS(skip-verify)` 模式
3. 联系 DBA 修复服务器证书配置

---

## 附录：命令速查

```bash
# 帮助
hepta_dbcli --help
hepta_dbcli cli --help

# 连接检查
hepta_dbcli check
hepta_dbcli check --verbose
hepta_dbcli check --name prod

# CLI 执行
hepta_dbcli cli --sql "SELECT 1"
hepta_dbcli cli --file query.sql
echo "SELECT 1" | hepta_dbcli cli
hepta_dbcli cli --sql "SELECT 1" --format json

# REPL
hepta_dbcli cli --interactive
hepta_dbcli cli -i --name dev

# 密码管理
hepta_dbcli store-password
hepta_dbcli store-password --name prod

# MCP 服务器
hepta_dbcli
hepta_dbcli --config /path/to/config.toml

# 多连接
hepta_dbcli cli --name prod --sql "..."
hepta_dbcli cli --config /path/to/config.toml --name dev --sql "..."
```
