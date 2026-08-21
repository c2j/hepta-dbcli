# `delta-diff` 设计文档（修订版 v2.1）

> 基于 `hepta-dbcli` 现有架构（Rust + 多连接管理 + MCP/CLI 双模式）的表数据比对指令设计方案。
>
> **修订说明（相对 v1.0）**：
>
> - **[P0-修复]** Checksum 算法由 `GROUP_CONCAT`/`LISTAGG`/`string_agg` 拼接式改为**顺序无关的位切片聚合**（data-diff 同款），消除截断/溢出/报错三类正确性风险，同时去掉排序开销（见 §七、§十）。
> - **[P0-修复]** 无主键策略由物理块切分（`_rowid`/`ctid`/`ROWID`）改为**内容分桶（BucketDiffer）**，解决跨库物理布局不可比的问题（见 §6.3）。
> - **[P0-新增]** 一致性语义设计：单侧快照 + 差异二次复核（见 §八）。
> - **[P1-新增]** 性能设计专章：SLO 目标、性能模型、并行化方案、基准测试矩阵（见 §十一）。
> - **[P1-新增]** 退出码、限流、dry-run、类型规范化矩阵、`--update-column` 参数定义（见 §二、§九、§十二）。
> - **[P1-调整]** 实施计划重排：正确性与基准前置（见 §十四）。
>
> **修订说明（v2.1，相对 v2.0）**——基于对 hepta-dbcli 代码库的逐项适配核验：
>
> - **[P0-修复]** §10.1 MySQL checksum SQL 运算顺序错误：`CAST(SUM(...) AS UNSIGNED)` 会先饱和截断到 2⁶⁴-1 再取模，改为对 DECIMAL 和直接 `MOD(SUM(...), 2⁶⁴)`（见 §10.1）。
> - **[P0-修复]** 行级拉取由"流式 merge-join"改为 **keyset 分页归并**：现有 `DbConn` trait 无流式 API（`query()` 全物化 `Vec<Vec<Value>>`），流式假设不成立（见 §6.2.1、§十、§11.3-7）。
> - **[P0-新增]** 代码库适配核验发现的三个结构性前置项：GaussDB 后端为单连接共享池，快照事务前必须重构为真多连接；`SideExecutor`/`scheduler` 补完整定义；MySQL `exec` 绑定参数扩展（见 §8.2、§5.2、§十四）。
> - **[P0-调整]** 方言层放弃自建 `Dialect` 枚举，改为**扩展现有 `backend::Dialect` trait**（已有 13 个方法，主键发现/行数估算可直接复用）；删除无后端支撑的 `PostgreSQL` 变体（见 §七）。
> - **[P1-调整]** SLO 按一致性模式分层：MySQL 快照单会话为串行聚合，`1B 行 ~5min` 仅在 `--consistency none` 多会话并行档下作为目标，快照档按实测回填（见 §11.1）。
> - **[P1-修复]** MCP 集成方式由独立 `ServerHandler` 改为在现有 `#[tool_router] impl DbMcp` 内加 `#[tool]` 方法，并旁路 `is_read_only_mcp` 前缀门卫（快照语句不在只读前缀内）（见 §13.1）。
> - **[P1-新增]** Phase 0 技术预研 spike：checksum 三方实测、`hash_any_extended`/CRC32 链验证、oracle-rs 事务能力确认（见 §十四）。
> - **[P2-标注]** CRC32 链碰撞风险告警；`hash_any_extended` 可用性待验证（见 §11.3-5）；类型规范化明确"SQL 层 checksum 路径"与"客户端行级比较路径"双轨（见 §九）。
>
> **验证回填（2026-08-17，详见 §十六）**：Phase 0 的 MySQL/PolarDB-X/openGauss 三侧已容器实测——位切片 checksum 跨库逐位一致（8/8 用例）；§10.1 修正写法与溢出语义实证成立；§九无时区 DATETIME 公式修正（`AT TIME ZONE 'UTC'` 会偏移会话时区）；`hash_any_extended` 证伪（GaussDB 固定 MD5）；PolarDB-X 快照语法降级方案确认；单会话吞吐实测（MySQL ~0.2M 行/s）支撑 §11.1 SLO 分层。

---

## 一、现有架构适配分析

| hepta-dbcli 特性 | delta-diff 复用点 |
|-----------------|------------------|
| **多连接配置**（`~/.hepta-dbcli.toml`） | 直接复用 `connections` 定义左右数据源 |
| **OS Keychain** | 密码安全存储无需改动 |
| **CLI 子命令体系** | 新增 `delta-diff` 子命令，与 `cli`/`check` 同级 |
| **输出格式**（table/json/csv/vertical） | 差异结果统一走现有格式化管道 |
| **MCP Server 只读安全** | diff 操作天然只读，可直接暴露为 MCP Tool |
| **多数据库驱动**（MySQL/Oracle/GaussDB） | 通过现有连接层获取元数据和执行 checksum（三后端均已存在且默认启用；适配细节与限制见 §七/§八/§九 的 v2.1 注） |

> **v2.1 注**：上表为声明级适配。逐项核验结论：配置/keychain/子命令体系完全复用；输出管道需将 DiffReport 投影为 `QueryResult` 后复用 `render_result`（§四 output 层）；MCP 暴露方式见 §13.1 修正；连接层"执行 checksum"需先完成 §7.2 的 trait 扩展与 §8.2 的两个前置项。

---

## 二、命令行接口设计

### 2.1 基本用法

```bash
# 比对两个已配置连接中的同一张表
hepta_dbcli delta-diff --left dev --right prod --table orders

# 比对不同表名
hepta_dbcli delta-diff --left dev --right prod \
  --left-table orders_v1 --right-table orders_v2

# 带条件过滤（两侧同时应用，见 §12.3 注意事项）
hepta_dbcli delta-diff --left dev --right prod --table orders \
  --where "create_time >= '2024-01-01'"

# 指定比对列和主键
hepta_dbcli delta-diff --left dev --right prod --table orders \
  --key id --columns id,amount,status

# 选择比对策略
hepta_dbcli delta-diff --left dev --right prod --table orders \
  --strategy hashdiff --bisection-factor 32

# 预检：只输出策略选择、行数估算与分片计划，不执行比对
hepta_dbcli delta-diff --left dev --right prod --table orders --dry-run

# 增量比对：只比对 update_time 近一天有变化的行
hepta_dbcli delta-diff --left dev --right prod --table orders \
  --update-column update_time --update-since "1 day"

# 输出格式
hepta_dbcli delta-diff --left dev --right prod --table orders \
  --format json --output diff_report.json

# 仅输出统计，不输出差异行
hepta_dbcli delta-diff --left dev --right prod --table orders --summary-only
```

### 2.2 完整参数说明

```
delta-diff
  --left <NAME>              左数据源连接名（对应配置文件中的 connections）
  --right <NAME>             右数据源连接名
  --table <TABLE>            表名（左右相同）
  --left-table <TABLE>       左表名（与右表不同名时使用）
  --right-table <TABLE>      右表名
  --schema <SCHEMA>          Schema/数据库名（覆盖连接默认库）
  --left-schema <SCHEMA>     左 Schema
  --right-schema <SCHEMA>    右 Schema
  --key <COLS>               主键/比对键列，逗号分隔（自动发现失败时指定）
                             v2.0 约束：二分切分仅支持单列数值/日期时间主键，
                             其他类型自动降级为 bucketdiff 并提示（见 §6.4）
  --columns <COLS>           要比对的列，逗号分隔（默认全部）
  --where <CONDITION>        WHERE 条件（两侧同时应用；禁止分号，见 §12.3）
  --update-column <COL>      增量比对列（与 --where 互斥）
  --update-since <EXPR>      增量窗口，如 "1 day"、"2026-08-01 00:00:00"
  --strategy <STRATEGY>      auto | hashdiff | joindiff | bucketdiff [默认: auto]
  --bisection-factor <N>     Hashdiff 二分因子 [默认: 32]
  --bisection-threshold <N>  Hashdiff 行级阈值 [默认: 16384]
  --consistency <MODE>       一致性模式：snapshot | none [默认: snapshot]
  --recheck                  对差异行二次复核，过滤比对窗口内的并发写入
                             [默认: snapshot 模式下开启]
  --sample <N>               差异行采样上限 [默认: 1000]
  --summary-only             仅输出统计，不输出差异明细
  --dry-run                  预检模式：输出策略、行数估算、分片计划、预计查询数
  --format <FMT>             输出格式：table | json | csv | vertical [默认: table]
  --output <FILE>            输出到文件
  --threads <N>              总并发度（两侧各自不超过 ⌈N/2⌉ 个会话）[默认: 4]
  --statement-timeout <SEC>  单条查询超时 [默认: 300]
  --checkpoint <FILE>        断点续传文件路径（JSONL，见 §13.2）
  --verbose                  显示分片级进度
```

### 2.3 退出码（CI/CD 契约）

| 退出码 | 含义 |
|--------|------|
| `0` | 比对完成，两侧一致 |
| `1` | 比对完成，存在差异 |
| `2` | 执行错误（连接失败、权限不足、参数非法、超时等），报告输出到 stderr |

---

## 三、配置集成设计

复用现有 `~/.hepta-dbcli.toml` 多连接配置，无需新增配置段：

```toml
default_connection = "dev"

[connections.dev]
host = "127.0.0.1"
port = 3306
user = "root"
password = "keyring"
database = "test"

[connections.prod]
host = "prod-db.example.com"
port = 3306
user = "readonly"
password = "keyring"
database = "test"

[connections.gauss]
host = "gauss-host"
port = 5432
user = "reader"
password = "keyring"
database = "prod"

[connections.ora]
driver = "oracle"
host = "oracle.internal"
port = 1521
user = "scott"
password = "keyring"
database = "FREEPDB1"
```

---

## 四、核心模块架构（Rust）

```
src/
├── main.rs                    # CLI 入口；Commands 加 DeltaDiff 变体 + match arm + mod 声明
├── cli.rs                     # 现有：OutputFormat/render_result/is_read_only_mcp（复用，勿动语义）
├── server.rs                  # 现有 MCP；★ 在 #[tool_router] impl DbMcp 内加 #[tool] 方法（§13.1）
├── config.rs                  # 现有：多连接解析/keyring（原样复用，左右各解析一次）
├── backend/
│   ├── mod.rs                 # ★ Dialect trait 扩展新方法（§七）；DbConn::exec 参数扩展（§8.2-4）
│   └── gaussdb/pool.rs        # ★ 单连接共享池 → 真多连接池重构（§8.2-3，Phase 2 前置）
├── delta_diff/                # ★ 新增模块（本设计主体）
│   ├── mod.rs                 # 公共类型与入口
│   ├── cmd.rs                 # CLI 参数解析 (clap)
│   ├── engine.rs              # 智能路由引擎 (SmartRouter)
│   ├── executor.rs            # ★ SideExecutor：每侧快照连接 + 查询队列 + 信号量调度（§8.2）
│   ├── consistency/
│   │   ├── mod.rs             # 一致性抽象
│   │   ├── snapshot.rs        # 单侧快照事务管理（§8.2）
│   │   └── recheck.rs         # 差异行二次复核
│   ├── strategies/
│   │   ├── mod.rs             # DiffStrategy trait
│   │   ├── hash_diff.rs       # HashDiffer（分段并行 + 位切片 checksum）
│   │   ├── join_diff.rs       # JoinDiffer（同构同版本，联邦查询可选）
│   │   └── bucket_diff.rs     # ★ BucketDiffer（无主键表，内容分桶）
│   ├── metadata/
│   │   ├── mod.rs             # ★ 主键/行数估算复用 dialect.table_indexes()/list_tables()，
│   │   │                      #   本层只做结果解析与缓存，不重复写内省 SQL（§七-注）
│   │   ├── partition.rs       # 分区信息发现
│   │   └── histogram.rs       # 直方图/统计信息（Phase 4）
│   ├── sql/
│   │   ├── mod.rs             # SQL 模板渲染
│   │   ├── checksum.rs        # ★ 位切片聚合 Checksum SQL 生成（§十，含 Oracle AS OF SCN 注入钩子）
│   │   ├── normalize.rs       # ★ 列值规范化表达式生成（§九）
│   │   ├── paginate.rs        # ★ keyset 分页拉取 SQL 生成（§6.2.1）
│   │   └── join.rs            # JOIN SQL 生成
│   ├── types/
│   │   ├── mod.rs             # 类型映射中心（行级比较语义层，§九-2）
│   │   ├── canonical.rs       # 统一中间类型
│   │   └── mapper.rs          # 跨库类型映射
│   ├── progress/
│   │   └── mod.rs             # 进度报告与断点续传（JSONL）
│   └── output/
│       └── mod.rs             # ★ DiffReport → QueryResult 投影，复用 cli.rs::render_result 四格式管道
└── ...
```

> **注（v2.1）**：现有代码为扁平布局——`src/cli.rs`（非 `src/cli/mod.rs`）、`src/server.rs` 单文件承载 MCP（rmcp `#[tool_router]` 宏，`src/mcp/` 目录不存在）；`main.rs` 是 mod 根。新增子命令的接入点：`main.rs` 的 `Commands` 枚举加变体 + `main()` match 加 arm + 顶部 `mod delta_diff;`。

---

## 五、核心类型设计

### 5.1 差异结果类型

```rust
// src/delta_diff/mod.rs

use serde::{Deserialize, Serialize};
use chrono::{DateTime, Utc};

/// 差异行
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffRow {
    pub key: serde_json::Value,           // 主键值（复合主键为对象）
    pub left: Option<serde_json::Value>,  // 左表行数据
    pub right: Option<serde_json::Value>, // 右表行数据
    pub status: DiffStatus,
    pub confirmed: bool,                  // ★ 是否经二次复核确认（§8.3）
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DiffStatus {
    MissingLeft,   // 右有左无
    MissingRight,  // 左有右无
    Modified,      // 键相同但列值不同
}

/// 分片比对结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShardResult {
    pub shard_id: String,
    /// ★ 键范围用 JSON 值表达，兼容数值/时间/复合键；
    /// 内容分桶模式下为桶号范围
    pub key_range: (serde_json::Value, serde_json::Value),
    pub left_count: u64,
    pub right_count: u64,
    pub match_count: u64,
    pub diff_count: u64,
    pub status: ShardStatus,
    pub duration_ms: u64,     // ★ 分片耗时（性能埋点，§11.4）
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ShardStatus {
    Match,
    Diff,
    Skipped, // 断点续传已跳过
}

/// 最终报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffReport {
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub left: TableRef,
    pub right: TableRef,
    pub strategy: String,
    pub consistency: ConsistencyMode, // ★ snapshot | none
    pub hash_algorithm: String,       // ★ md5 | crc32chain | xxh64（§11.3-5）
    pub summary: DiffSummary,
    pub perf: PerfMetrics,            // ★ 性能埋点汇总（§11.4）
    pub shards: Vec<ShardResult>,
    pub sample_diffs: Vec<DiffRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffSummary {
    pub left_total: u64,
    pub right_total: u64,
    pub match_count: u64,
    pub missing_left: u64,
    pub missing_right: u64,
    pub modified: u64,
    pub diff_rate: f64,
}

/// ★ 性能埋点：直接进报告，支撑 SLO 验收与回归（§11.4）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerfMetrics {
    pub queries_total: u64,
    pub bytes_from_left: u64,
    pub bytes_from_right: u64,
    pub left_rows_per_sec: f64,
    pub right_rows_per_sec: f64,
    pub shard_duration_p50_ms: u64,
    pub shard_duration_p99_ms: u64,
    pub peak_rss_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableRef {
    pub connection: String,
    pub schema: Option<String>,
    pub table: String,
}
```

### 5.2 策略 trait

```rust
// src/delta_diff/strategies/mod.rs

use async_trait::async_trait;

#[async_trait]
pub trait DiffStrategy: Send + Sync {
    fn name(&self) -> &'static str;

    /// 是否支持给定的左右表配置
    fn supports(&self, left: &TableConfig, right: &TableConfig) -> bool;

    /// 执行比对
    async fn diff(
        &self,
        left: &mut SideExecutor,   // ★ 每侧一个执行器（内部持有快照连接池，§8.2）
        right: &mut SideExecutor,
        ctx: &DiffContext,
    ) -> Result<DiffReport, DiffError>;
}

/// 比对上下文
pub struct DiffContext {
    pub table: TableRef,
    pub key_columns: Vec<String>,
    pub compare_columns: Vec<String>,
    pub filter: Option<String>,
    pub update_column: Option<IncrementalSpec>, // ★ 增量比对
    pub bisection_factor: usize,
    pub bisection_threshold: usize,
    pub consistency: ConsistencyMode,
    pub recheck: bool,
    pub sample_limit: usize,
    pub threads: usize,
    pub statement_timeout: Duration,
    pub checkpoint: Option<CheckpointManager>,
    /// ★ v2.1 补全（v2.0 伪代码引用但未定义）：分片任务信号量调度器。
    /// 内部为 tokio::Semaphore(threads) + JoinSet；
    /// spawn() 先 acquire 许可再提交任务，--threads 由此真实约束并发。
    pub scheduler: ShardScheduler,
}

/// ★ v2.1 补全：每侧执行器（v2.0 仅散文描述）。
/// 快照模式：持有本侧唯一快照连接，SQL 经队列串行下发（侧内串行、侧间并行）；
/// none 模式：持有 ⌈threads/2⌉ 连接的池 + 信号量，同侧并行下发（无一致性保证，见 §8.4、§11.1）。
pub struct SideExecutor {
    conn: SideConn,                    // Snapshot(Box<dyn DbConn>) | Pool(Vec<Box<dyn DbConn>> + Semaphore)
    queue: mpsc::Sender<QueryTask>,    // 快照模式：单消费者串行执行；none 模式：每连接一个消费者
    dialect: Arc<dyn Dialect>,         // 现有 backend::Dialect trait 对象（§七）
}

/// 分片任务信号量调度器
pub struct ShardScheduler {
    permits: Semaphore,                // permits = threads
    tasks: JoinSet<Result<ShardResult, DiffError>>,
}
```

---

## 六、策略实现

### 6.1 SmartRouter（智能路由）

```rust
// src/delta_diff/engine.rs

pub struct SmartRouter;

impl SmartRouter {
    pub fn select_strategy(
        left: &TableConfig,
        right: &TableConfig,
        user_preference: Option<&str>,
    ) -> Box<dyn DiffStrategy> {
        if let Some(name) = user_preference {
            return Self::create_by_name(name);
        }

        // 无主键 → BucketDiffer（内容分桶，§6.3）
        if left.primary_keys.is_empty() || right.primary_keys.is_empty() {
            return Box::new(BucketDiffer);
        }

        // ★ 主键类型不支持二分（复合/字符串）→ BucketDiffer 并告警（§6.4）
        if !Self::is_bisectable_key(&left.primary_keys, &left.column_types) {
            warn!("key type not bisectable, falling back to bucketdiff");
            return Box::new(BucketDiffer);
        }

        // 同构同版本 → JoinDiffer 可用；跨库 → HashDiffer
        if Self::is_same_dialect_and_version(left, right) {
            return Box::new(JoinDiffer::with_fallback(HashDiffer::new()));
        }

        Box::new(HashDiffer::new())
    }
}
```

### 6.2 HashDiffer（核心算法）

相对 v1.0 的两处关键修改：**表级快筛改为分段并行**（否则 MySQL 单线程聚合无法达成 §11.1 的 SLO）、**checksum 改为位切片聚合**（§十）。

```rust
// src/delta_diff/strategies/hash_diff.rs

pub struct HashDiffer {
    factor: usize,
    threshold: usize,
}

#[async_trait]
impl DiffStrategy for HashDiffer {
    fn name(&self) -> &'static str { "hashdiff" }

    async fn diff(
        &self,
        left: &mut SideExecutor,
        right: &mut SideExecutor,
        ctx: &DiffContext,
    ) -> Result<DiffReport, DiffError> {
        // 0. 一致性：每侧开启快照事务（§8.2）
        left.open_snapshot(ctx).await?;
        right.open_snapshot(ctx).await?;

        // 1. 获取 key 范围（min/max，单次索引探查）
        let key_range = self.get_key_range(left, right, ctx).await?;

        // 2. ★ 首轮即分段并行快筛：按 key 空间切成 threads × 8 段，
        //    左右两侧同时执行（侧间天然并行，侧内受信号量约束）。
        //    全部一致 → 直接返回，这就是"零差异快筛"路径。
        let segments = key_range.split(ctx.threads * 8);
        let first_pass = self
            .parallel_checksum(left, right, ctx, segments)
            .await?;

        if first_pass.all_matched() {
            return Ok(DiffReport::no_diff(ctx, first_pass));
        }

        // 3. 仅对不一致段递归二分（§6.2.1），匹配段直接落账
        let mut results = first_pass.into_results();
        for seg in results.iter().filter(|s| s.status == ShardStatus::Diff) {
            self.bisect_compare(left, right, ctx, seg.range(), &mut results).await?;
        }

        // 4. 差异行二次复核（§8.3）
        if ctx.recheck {
            self.recheck_diffs(left, right, ctx, &mut results).await?;
        }

        // 5. 差异采样
        let samples = self.sample_diffs(left, right, ctx, &results).await?;

        Ok(DiffReport::from_results(ctx, results, samples))
    }
}
```

#### 6.2.1 二分递归（并行版）

```rust
impl HashDiffer {
    /// ★ 相对 v1.0：由串行 for 循环改为信号量控制的任务队列，
    /// 任务提交到本侧快照连接的查询队列（§8.2），--threads 真正生效。
    async fn bisect_compare(
        &self,
        left: &mut SideExecutor,
        right: &mut SideExecutor,
        ctx: &DiffContext,
        range: KeyRange,
        results: &mut Vec<ShardResult>,
    ) -> Result<(), DiffError> {
        // 高差异率保护：差异分片占比超过 50% 时降级为
        // 范围 keyset 分页归并（§11.3-7），防止二分退化为逐行查询
        if results.diff_ratio() > 0.5 {
            return self.range_merge_join(left, right, ctx, range, results).await;
        }

        if let Some(ref cp) = ctx.checkpoint {
            if cp.is_completed(&range).await? {
                results.push(ShardResult::skipped(range));
                return Ok(());
            }
        }

        let (l, r) = tokio::join!(
            self.shard_checksum(left, ctx, &range),
            self.shard_checksum(right, ctx, &range),
        );
        let (l, r) = (l?, r?);

        if l == r {
            results.push(ShardResult::matched(range, l.count));
            return Ok(());
        }

        if l.count.max(r.count) <= self.threshold as u64 {
            // 行级复核：双侧按 key 做 keyset 分页拉取（页大小 8192），
            // 客户端页式归并，内存 O(页大小)（§6.2.2）
            let detail = self.row_level_diff(left, right, ctx, &range).await?;
            results.push(detail);
            return Ok(());
        }

        // 子分片提交到并行调度器，而非同步递归等待
        for sub in range.split(self.factor) {
            ctx.scheduler.spawn(self.bisect_task(left, right, ctx, sub));
        }
        Ok(())
    }
}
```

#### 6.2.2 行级拉取：keyset 分页归并（v2.1 替代"流式 merge-join"）

**v2.0 假设不成立**：现有 `DbConn` trait（`backend/mod.rs`）的 `query()` 返回全物化 `Vec<Vec<serde_json::Value>>`，**无流式/游标 API**；三后端驱动（mysql_async、oracle-rs、gaussdb）流式能力不一致，为 delta-diff 单独给 trait 加流式方法成本高、风险大。

**替代方案：keyset 分页拉取 + 页式归并**，纯 SQL 实现、对三后端一致可行：

```sql
-- 每页（双侧对称执行，页大小 page_size = 8192）
SELECT <key>, <cols> FROM orders
WHERE id >= :lo AND id < :hi          -- 分片范围
  AND id > :last_key                  -- keyset 游标（首页省略）
ORDER BY id
LIMIT 8192;                           -- GaussDB: LIMIT；Oracle 12c+: FETCH FIRST 8192 ROWS ONLY
```

- 客户端维护左右两侧各一个页缓冲，按键做**页式归并**（任一侧缓冲耗尽即拉下一页），禁止 O(n²) 比对；
- 内存 = O(页大小)，与分片行数无关——`range_merge_join` 降级路径处理大分片时内存同样有界，满足 §11.1 "内存与行数无关" 的性质；
- 同一分片的所有页查询在**同一快照连接上串行**下发（§8.2），页间快照一致；
- 行值比较语义见 §九-2（客户端比较路径）。

> 备选（不采纳）：给 `DbConn` 新增 `query_stream`——mysql_async 支持流式，但 oracle-rs 0.1 的流式能力未验证，gaussdb 驱动待查；且 trait 扩展涉及三后端同时改动。如 Phase 4 压测显示 keyset 分页成为瓶颈，再评估该方案。

### 6.3 BucketDiffer（无主键表，替代 v1.0 的 BlockDiffer）

**v1.0 的物理块切分（`_rowid`/`ctid`/`ROWID`）废弃**，原因：两侧是独立数据库，物理布局不可比；伪列稳定性无保障（`ctid` 随 UPDATE/VACUUM 变化，Oracle `ROWID` 随 MOVE 变化，InnoDB `_rowid` 仅在无非空唯一键时存在且不可排序）。

**新方案：内容分桶。** 按 `MOD(row_hash, N)` 分桶，分桶由行内容决定，天然跨库对齐：

```rust
// src/delta_diff/strategies/bucket_diff.rs

pub struct BucketDiffer;

#[async_trait]
impl DiffStrategy for BucketDiffer {
    fn name(&self) -> &'static str { "bucketdiff" }

    fn supports(&self, _l: &TableConfig, _r: &TableConfig) -> bool { true }

    async fn diff(
        &self,
        left: &mut SideExecutor,
        right: &mut SideExecutor,
        ctx: &DiffContext,
    ) -> Result<DiffReport, DiffError> {
        // 1. 估算行数，确定桶数 N：目标每桶 ≈ threshold 行，上限 1024 桶
        let n = self.bucket_count(left, right, ctx).await?;

        // 2. 并行逐桶比对：桶内做位切片聚合 checksum（与 HashDiffer 同一套 SQL，
        //    仅 WHERE 条件改为 MOD(hash, N) = b）
        let mut diff_buckets = Vec::new();
        for b in 0..n {
            let (l, r) = tokio::join!(
                self.bucket_checksum(left, ctx, b, n),
                self.bucket_checksum(right, ctx, b, n),
            );
            if l? != r? { diff_buckets.push(b); }
        }

        // 3. 差异桶拉全行，客户端做多重集合比对（multiset diff）：
        //    输出"差异行内容 + 两侧各自出现次数"
        let rows = self.multiset_diff(left, right, ctx, &diff_buckets).await?;

        Ok(DiffReport::from_multiset(ctx, n, rows))
    }
}
```

**已知理论极限（必须在报告和文档中明示）**：无主键表无法回答"哪一行被改了"，只能输出"哪些行内容多/少了几次"。报告头部固定输出提示：`note: keyless table diff reports row-content multiset differences only`。

### 6.4 二分可切分的键类型约束

| 主键形态 | 策略 | 说明 |
|---------|------|------|
| 单列整型 | hashdiff | 等距切分 |
| 单列日期/时间戳 | hashdiff | 转 epoch 后等距切分 |
| 单列浮点 | bucketdiff + 告警 | 浮点范围切分有精度风险，v2.0 不支持 |
| 字符串 | bucketdiff + 告警 | 等距切分无定义，后续版本可支持字典序采样切分 |
| 复合主键 | bucketdiff + 告警 | 多维空间切分留待后续版本 |

---

## 七、方言适配层

> **v2.0 → v2.1 架构修正**：v2.0 计划在 `delta_diff/sql/dialect.rs` 自建 `Dialect` 枚举，存在三个问题——
> ① 与现有 `backend::Dialect` **trait 同名冲突**；② 现有 trait 已有 13 个方法（参数化内省、只读前缀、语句超时 SQL、标识符引号等），自建枚举意味着元数据/超时/引号逻辑全部重复一份；③ 枚举中的 `PostgreSQL` 变体**无后端支撑**（scheme 路由只认 `mysql`/`oracle`/`gaussdb`；PolarDB-X 走 mysql 协议隐式支持）。
>
> **v2.1 改为：扩展现有 `backend::Dialect` trait，三后端（MySQL/Oracle/GaussDB）各实现新方法。**

### 7.1 复用现有能力（无需新建）

| 需求 | 现有机制（`backend/mod.rs` Dialect trait） | 复用方式 |
|------|------------------------------------------|---------|
| 主键发现 | `table_indexes()` 三后端已归一化返回 `is_primary` + `columns`（CSV） | `metadata/mod.rs` 解析 `is_primary=true` 行的列清单，**不要重复写 information_schema/pg_index/all_constraints 查询** |
| 行数估算（--dry-run/分片规划） | `list_tables()` 返回 row_count（MySQL `TABLE_ROWS` / GaussDB `reltuples` / Oracle `NUM_ROWS`，均为统计估算值） | 直接过滤目标表读取；注意均为**近似值**，不作为一致性依据（语义恰好匹配） |
| 版本探测（同构同版本检测） | `database_info()` 首列即版本串 | 解析版本串比较 |
| 语句级超时 | `set_statement_timeout_sql(ms)`（Oracle 返回 `None`，见 §12.2 注意事项） | SideExecutor 建连后统一下发 |
| 只读前缀/标识符引号 | `read_only_prefixes()` / `identifier_quote()` | 表名/列名引用与 `--where` 拼接时使用 |

### 7.2 新增 trait 方法（三后端各实现一遍）

```rust
// src/backend/mod.rs — 在现有 Dialect trait 上扩展

/// 快照事务开启语句（§8.2）；Oracle 见 snapshot_scn_sql
fn begin_snapshot_sql(&self) -> &'static str {
    // MySQL:    "START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY"
    // GaussDB:  "BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY"
    // Oracle:   "SET TRANSACTION READ ONLY"（SCN 由 flashback 查询承载，见 §8.2）
}

/// ★ Oracle 专用：取当前 SCN（"SELECT CURRENT_SCN FROM V$DATABASE"）；
/// 其余后端返回 None
fn snapshot_scn_sql(&self) -> Option<&'static str> { None }

/// 可用 hash 函数能力（§11.3-5）
fn hash_capability(&self) -> HashCapability {
    // GaussDB: Md5（hash_any_extended 已于 openGauss 5.0.0 实测证伪：函数不存在，§16.3-F4）
    // MySQL:   Md5（默认）；Crc32Chain 为可选加速档（语法可行性已实测，碰撞风险见 §11.3-5）
    // Oracle:  Md5（STANDARD_HASH，待 Phase 0 验证）
}

/// 位切片聚合 checksum SQL 渲染（§十；含 range/分桶/规范化表达式/Oracle AS OF SCN 钩子）
fn render_checksum_sql(&self, spec: &ChecksumSpec) -> String;

/// keyset 分页拉取 SQL 渲染（§6.2.2）
fn render_keyset_page_sql(&self, spec: &KeysetSpec) -> String;

/// 列值规范化表达式（§九矩阵，按列类型逐条配单测）
fn normalize_expr(&self, col: &ColumnMeta) -> String;
```

### 7.3 方言身份判定

`SideExecutor` 从 `conn.dialect().url_scheme()`（现有方法：`"mysql"`/`"oracle"`/`"gaussdb"`）+ `database_info()` 版本串获得方言身份，用于 SmartRouter 的 `is_same_dialect_and_version` 判定；**不引入新的 Dialect 枚举**。若确需值对象传递，命名 `DbFlavor` 避免与 trait 冲突。

---

## 八、一致性设计（新增）

### 8.1 问题定义

对两张持续写入的表做分钟级比对，比对窗口内的并发写入会产生伪差异。v1.0 未定义一致性语义，v2.0 提供两级保证。

### 8.2 `--consistency snapshot`（默认）：单侧快照

- 每侧在**一个长事务**内完成全部查询：MySQL `WITH CONSISTENT SNAPSHOT, READ ONLY`（✅实测）；GaussDB `REPEATABLE READ READ ONLY`（✅实测）；Oracle 记录启动 SCN，所有查询带 `AS OF SCN :scn`。
  - ⚠ **PolarDB-X 例外（§16.3-F5 实测）**：`WITH CONSISTENT SNAPSHOT` 语法不支持，降级为 `SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; START TRANSACTION READ ONLY`（首次一致读建立快照视图）；因 PolarDB-X 走 mysql scheme，`begin_snapshot_sql` 需按 version 串（含 "PXC"）运行时识别后选择语法。PolarDB-X 另有 TSO 与 `AS OF TIMESTAMP '<literal>'` flashback（不接受 NOW()），留作后续增强。
- **与并发的冲突及解法**：快照绑定单连接，与"分片多连接并发"矛盾。解法为 `SideExecutor` 模式——每侧一个快照连接 + 一个查询队列，分片任务并行生成 SQL 后排队提交，**侧间并行、侧内串行**。侧内并行带来的吞吐损失由"分段并行快筛"（§6.2，段数 = threads × 8，数据库内部可并行执行范围扫描，且两条连接本身并行）和减少查询轮次弥补。
- **Oracle AS OF SCN 的注入点（v2.1 补全）**：`SET TRANSACTION READ ONLY` 提供语句级一致性，但跨查询的 SCN 锚定依赖 flashback 语法——`checksum.rs` 的 SQL 模板必须支持**表名改写钩子**：`FROM orders` → `FROM orders AS OF SCN :scn`。keyset 分页 SQL（§6.2.2）同样注入。该钩子由 `Dialect::snapshot_scn_sql()`（§7.2）驱动，仅 Oracle 生效。Phase 0 需验证 oracle-rs 0.1 驱动对 flashback 查询的支持。
- **GaussDB 池重构（v2.1 前置项）**：现有 `backend/gaussdb/pool.rs` 为**单连接共享池**（`Arc<gaussdb::Client>`，所有 `acquire()` 共享同一底层连接），多连接独立快照事务在 GaussDB 上物理不可能（BEGIN 会串会话状态）。Phase 2 前必须重构为真多连接池（每 acquire 独立 Client，对齐 Oracle 后端的专连专用模式）。该项不在 v2.0 任何 Phase 中，为 v2.1 新增工作。
- **MySQL `exec` 参数扩展（v2.1 前置项）**：现有 `mysql/conn.rs` 的 `exec` 硬编码仅支持 0–2 个绑定参数（tuple 限制），checksum/分桶/keyset SQL 的占位符（lo/hi/bucket_mod/bucket_eq/last_key）超出上限。需扩展为 N 参数绑定，或全部走内联插值——内联时必须经 `identifier_quote()` 与字面量转义函数处理，禁止裸拼用户输入（`--where` 条件除外，其语义见 §12.3）。
- 代价与提示：长快照会持有 undo/阻止 vacuum，报告输出 `snapshot_duration`，超过 10min 打 WARN。
- 两侧快照时刻不同（先后开启），**不保证跨库一致**，因此默认叠加 §8.3。
- **`SideExecutor` 结构定义见 §5.2**：快照模式 = 单连接 + mpsc 队列单消费者串行执行；`--consistency none` 模式 = ⌈threads/2⌉ 连接池 + 信号量，同侧并行下发（见 §8.4）。

### 8.3 `--recheck`：差异行二次复核

行级 diff 产出的每条差异，在比对结束前按 key 向两侧各发一次点查（小事务、当前读）：

- 复核一致 → 该差异为比对窗口内的并发写入，标记 `confirmed: false`，不计入 `diff_count`；
- 复核仍不一致 → `confirmed: true` 计入。

点查量 = 差异行数，与总行数无关，成本可忽略。

### 8.4 `--consistency none`

跳过快照与复核（如对离线备份库比对），报告标注 `consistency: none`，由调用方自行理解并发伪差异风险。

**v2.1 补充——none 模式同时是性能档**：`SideExecutor` 在此模式下启用同侧多连接并行（⌈threads/2⌉ 连接 + 信号量，见 §5.2），分段快筛的段查询可真正并行下发。MySQL 单查询聚合为单线程，这是突破 §11.1 单会话吞吐瓶颈的唯一路径；代价是各查询看到的数据版本不一致，伪差异风险完全交给调用方。SLO 分层口径见 §11.1。

---

## 九、类型规范化矩阵（新增）

hash 比对要求两侧列值**文本表示字节级一致**。`sql/normalize.rs` 按下表为每列生成规范化表达式，这是跨库可比性的核心，逐条配方言单测。

> **v2.1 澄清——规范化存在两条独立路径，不可混淆：**
>
> 1. **Checksum 路径（本节矩阵）**：规范化在 **SQL 层**完成（表达式拼进 checksum SQL，数据库侧文本化），客户端只比较五元组。**不依赖** 现有 `backend/*/types.rs`，本节矩阵即全部。
> 2. **行级比较路径（§6.2.2 页式归并 + 差异采样展示）**：原始行经驱动进客户端为 `serde_json::Value`，比较语义受现有 `types.rs` 影响。已知缺陷（Phase 内必须处理）：
>    - `oracle/types.rs` 为逐级试探式映射（i64→f64→string，仅 28 行）：NUMBER 走 f64 **丢精度**，且对任意字符串尝试 JSON 解析（`'1'` 变 number）——**Oracle 行级比较需按列类型重写分发**；过渡期降级策略：Oracle 侧行值一律经 `TO_CHAR`/SQL 层规范化后按**字符串**比较，报告标注 `note: oracle row-level compare is stringified (precision-limited)`；
>    - MySQL BLOB 输出 `0x`+hex vs GaussDB `\x`+hex，前缀不一致需在 `types/canonical.rs` 归一；
>    - MySQL/GaussDB 会把 JSON 列 parse 成结构化 Value，跨库表示必然不同——行级比较对 JSON 列统一取**原始文本**（SQL 层 `CAST(col AS CHAR)` / `col::text`）；
>    - FLOAT/DOUBLE 走 f64 字节级不可比，跨库场景按本节矩阵的告警处理（`--columns` 排除或定点转换）。

| 类型 | MySQL / PolarDB-X | GaussDB | Oracle |
|------|------------------|---------|--------|
| 整型 | `CAST(c AS CHAR)` | `c::text` | `TO_CHAR(c)` |
| DECIMAL | `CAST(c AS CHAR)`（保留原生标度）✅实测 | `c::text`（尾零保留，实测一致）✅实测 | `TO_CHAR(c, 'TM9')` 去尾零后按标度补齐 ★ |
| FLOAT/DOUBLE | `CAST(c AS CHAR)` ⚠ 仅同构可比；跨库建议 `--columns` 排除或按定点转换 | 同左 | 同左 |
| DATETIME/TIMESTAMP（无时区） | `DATE_FORMAT(c,'%Y-%m-%d %H:%i:%s.%f')`（DATETIME 无时区，identity）✅实测 | `to_char(c,'YYYY-MM-DD HH24:MI:SS.US')`（无时区列**禁止**加 `AT TIME ZONE 'UTC'`，实测偏移会话时区，§16.3-F3）✅实测 | `TO_CHAR(c,'YYYY-MM-DD HH24:MI:SS.FF6')` |
| TIMESTAMP WITH TZ（timestamptz） | 会话先 `SET time_zone='+00:00'` 后同上 | `to_char(c AT TIME ZONE 'UTC','YYYY-MM-DD HH24:MI:SS.US')`（仅带时区列适用） | 会话 `TIME_ZONE='UTC'` 后同上 |
| DATE | `DATE_FORMAT(c,'%Y-%m-%d')` | `to_char(c,'YYYY-MM-DD')` | `TO_CHAR(c,'YYYY-MM-DD')` |
| 字符串 | 原样 ⚠ 两侧字符集/排序规则需一致，启动时校验并告警 | 同左 | 同左 |
| BOOLEAN | `CAST(c AS CHAR)`（0/1）✅实测 | `c::int::text`（0/1）★ 对齐 MySQL，实测一致 ✅实测 | `TO_CHAR(c)` |
| NULL | `CONCAT_WS` 跳过 NULL，故用哨兵：`COALESCE(<expr>, '<0x1F>NULL<0x1F>')`（原始字节 0x1F（Unit Separator）包裹 "NULL"，ASCII 编码安全，issue #27） | 同左 | 同左 |
| BLOB/BYTEA/RAW | `HEX(c)` | `encode(c,'hex')` | `RAWTOHEX(c)` |
| LOB (CLOB/TEXT 大字段) | 默认排除并告警；`--columns` 显式指定时按前 4000 字节 hex | 同左 | 同左 |

行拼接统一为 `CONCAT_WS('#', <规范化表达式...>)`（PG/Oracle 用 `||` 链）。跨库类型映射（如 MySQL `DATETIME` ↔ Oracle `TIMESTAMP`）由 `types/mapper.rs` 校验可映射性，不可映射列启动即报错并列出。

---

## 十、Checksum SQL：顺序无关位切片聚合（替代 v1.0 拼接式）

**原理**：每行 hash（128-bit）切 4 个 32-bit 切片，分别求和（模 2⁶⁴ 语义），得到 `{count, s1, s2, s3, s4}` 五元组作为分片 checksum。顺序无关 → 免 `ORDER BY`；定长结果 → 无截断/上限；天然支持数据库并行聚合。

**v1.0 废弃原因**：MySQL `GROUP_CONCAT` 受 `group_concat_max_len`（默认 1024）限制，超限**静默截断**导致漏报；Oracle `LISTAGG` 超 32767 字节直接报错；大字符串聚合本身不可伸缩。

### 10.1 MySQL / PolarDB-X

```sql
SELECT COUNT(*) AS cnt,
  MOD(SUM(CONV(SUBSTRING(h,  1, 8), 16, 10)), 18446744073709551616) AS s1,
  MOD(SUM(CONV(SUBSTRING(h,  9, 8), 16, 10)), 18446744073709551616) AS s2,
  MOD(SUM(CONV(SUBSTRING(h, 17, 8), 16, 10)), 18446744073709551616) AS s3,
  MOD(SUM(CONV(SUBSTRING(h, 25, 8), 16, 10)), 18446744073709551616) AS s4
FROM (
  SELECT MD5(CONCAT_WS('#', /* §九 规范化表达式 */)) AS h
  FROM orders
  WHERE id >= ? AND id < ?
    /* AND <filter> | AND MOD(CONV(SUBSTRING(MD5(...),1,8),16,10), ?) = ? -- 分桶模式 */
) t;
```

> ⚠ **v2.1 修正（v2.0 SQL 有运算顺序 bug）**：v2.0 写作 `CAST(SUM(...) AS UNSIGNED) % 2⁶⁴`——MySQL `SUM()` 对整型参数返回 **DECIMAL**（官方文档明确），若和超过 u64 最大值，`CAST(... AS UNSIGNED)` 会**先饱和截断到 2⁶⁴-1 再取模**，checksum 直接错误。正确写法如上：`CONV(...)` 返回字符串经数值上下文转 u64（单值 ≤ 2³²-1 不溢出），`SUM` 以 DECIMAL 累加（精度足够），`MOD(decimal, 2⁶⁴)` 按 DECIMAL 语义取模——与 PG/Oracle 的数值取模结果一致。
>
> ⚠ `checksum.rs` 对三方取模实现逐一单测，用同一 fixture 数据跨库断言五元组相等（Phase 0 spike 首项）。触发条件提示：单分片 > ~4.3B 行才会让 SUM 超 u64，正常分片规模下 v2.0 写法不会暴露——**正因为隐蔽，必须进 CI 断言**。
>
> ✅ **2026-08-17 实测（§十六）**：本页 SQL 在 MySQL 8 与 PolarDB-X 上与 python 期望五元组逐位一致（全表+4 分段+3 分桶）；溢出语义对比实测证实 CAST 写法饱和、MOD 写法正确。**注意**：分桶表达式必须与 checksum 行 hash 出自同一模板（实测曾因两处模板漂移——bucket 残留 `AT TIME ZONE 'UTC'`——导致分桶归属错误）。

### 10.2 GaussDB

```sql
SELECT COUNT(*) AS cnt,
  MOD(SUM(('x' || SUBSTR(h,  1, 8))::bit(32)::bigint), 18446744073709551616) AS s1,
  MOD(SUM(('x' || SUBSTR(h,  9, 8))::bit(32)::bigint), 18446744073709551616) AS s2,
  MOD(SUM(('x' || SUBSTR(h, 17, 8))::bit(32)::bigint), 18446744073709551616) AS s3,
  MOD(SUM(('x' || SUBSTR(h, 25, 8))::bit(32)::bigint), 18446744073709551616) AS s4
FROM (
  SELECT MD5(/* §九 规范化拼接；无时区列用 plain to_char，§16.3-F3 */) AS h
  FROM orders
  WHERE id >= ? AND id < ?
) t;
```

> ✅ 2026-08-17 实测（§十六）：本页 SQL 在 openGauss 5.0.0 上与 MySQL/PolarDB-X/python 期望五元组逐位一致（全表+4 分段+3 分桶）。前提：`::bit(32)::bigint` 在 openGauss 为无符号解释（实测与 python `int(h,16)` 一致）。

### 10.3 Oracle

```sql
SELECT COUNT(*) AS cnt,
  MOD(SUM(TO_NUMBER(SUBSTR(RAWTOHEX(h),  1, 8), 'XXXXXXXX')), POWER(2,64)) AS s1,
  MOD(SUM(TO_NUMBER(SUBSTR(RAWTOHEX(h),  9, 8), 'XXXXXXXX')), POWER(2,64)) AS s2,
  MOD(SUM(TO_NUMBER(SUBSTR(RAWTOHEX(h), 17, 8), 'XXXXXXXX')), POWER(2,64)) AS s3,
  MOD(SUM(TO_NUMBER(SUBSTR(RAWTOHEX(h), 25, 8), 'XXXXXXXX')), POWER(2,64)) AS s4
FROM (
  SELECT STANDARD_HASH(/* §九 规范化拼接 */, 'MD5') AS h
  FROM orders AS OF SCN :scn           -- ★ 快照模式注入点（§8.2）；none 模式省略
  WHERE id >= :lo AND id < :hi
) t;
```

> 注（v2.1）：`AS OF SCN` 由 `checksum.rs` 的表名改写钩子注入（仅 Oracle 快照模式），keyset 分页 SQL（§6.2.2）同理。

**行级复核查询**（分片 ≤ threshold 时）：两侧各自按 keyset 分页拉取（`SELECT <key>, <cols> FROM ... WHERE <range> AND key > :last ORDER BY key LIMIT 8192`），客户端页式归并，内存 O(页大小)（见 §6.2.2）。

---

## 十一、性能设计（新增）

### 11.1 性能目标（SLO）

所有数字绑定前提，验收以 §11.4 基准实测回填为准。

> **v2.1 分层修正**：v2.0 的 `1B 行 ~5min`（单侧 ≥3.3M 行/s）与 §8.2 的快照单会话**自相矛盾**——MySQL 8.0 单查询聚合为单线程执行，MD5+CONCAT_WS 逐行计算的实际吞吐通常仅 0.5–2M 行/s，且同侧多连接无法共享快照点（MySQL 无跨会话快照机制）。v2.1 将 SLO 按一致性模式分层：

| 场景 | 一致性模式 | 目标 | 前提 |
|------|-----------|------|------|
| 🔥 零差异快筛 | snapshot（默认） | 25M 行 < 30s（单侧 ≥1M 行/s）；1B 行 **按实测回填，不预设承诺** | 快照单会话串行；NVMe、行宽 ≤ 200B、覆盖索引或列存；客户端与库同机房 |
| 🔥 零差异快筛 | none（性能档） | 25M 行 < 10s；1B 行 ~ 5min（单侧 ≥3.3M 行/s） | 同侧 ⌈threads/2⌉ 连接并行（§8.4），段查询真正并行下发；其余前提同上 |
| 🔍 差异定位 | 任意 | 100M 行、0.01% 差异率，全链路 < 3min；行级拉取数据量 = 阈值 × 差异分片数，与总行数无关 | factor=32、threshold=16384 |
| ♾️ 超大规模 | 任意 | 10B+ 行可完成：客户端内存 ≤ 512MB（O(分片数 + 页缓冲)，与行数无关，见 §6.2.2）；递归深度 ⌈log₃₂(10B/16K)⌉ ≈ 4 层 | 断点文件 JSONL（§13.2） |
| 💥 高差异率 | 任意 | 差异分片占比 > 50% 时自动降级范围 keyset 分页归并，查询数有硬上界 | §6.2.1 保护逻辑 |
| 🛡️ 源库负载 | snapshot | 单侧 1 会话（快照串行） | §8.2 |
| 🛡️ 源库负载 | none | 单侧并发会话 ≤ ⌈threads/2⌉ | §8.4 |

> ⚠ Phase 1 的 CI 25M 行基准**先出实测值再定稿本表**；MySQL snapshot 档若实测显著低于 1M 行/s，对外口径只承诺 none 档数字并明示一致性代价。
>
> ✅ 实测支撑（§16.3-F7）：开发容器单会话 checksum 吞吐 MySQL 0.19–0.22M 行/s、openGauss 0.05M 行/s（100 万行窄行）。即使生产环境放大 5–10 倍，快照单会话档也应按 **~0.2–2M 行/s** 规划——本表的分层口径由实测背书。
>
> ✅ Phase 4 实测回填（2026-08-17，ARM Mac 容器、窄行 3 列、单机同机房）：10M 行零差异快筛 none 档 62.6s / snapshot 档 74.2s（≈0.13–0.16M 行/s/侧，峰值 RSS 27–29MB ≪ 512MB）；10M 行注入 1000 条分散差异全链路 3m16s（1000/1000 精确）；1M 行 9:1 倾斜 key + 6 差异 8.3s（二分自适应，94 分片）。**1B 行外推（§11.2 模型，N/R_scan）**：snapshot 单会话档 ≈ 100 分钟量级（1B ÷ 0.16M 行/s）；none 档多连接并行按 ⌈threads/2⌉ 近似线性可降至 ~25–30 分钟（容器口径），生产 NVMe + 覆盖索引可再降一个量级——与 v2.0 "1B ~5min" 的原始假设差距源于单会话 MD5 聚合吞吐，none 档仍是唯一的性能路径。

### 11.2 性能模型

```
总耗时 T ≈ max(T_left, T_right)                       （侧间并行）
T_side ≈ N / R_scan                                   （分段并行快筛）
       + D × (T_query + S_thresh / R_scan)            （D=差异分片数的行级复核）
查询数 Q ≈ S_first + D × depth × 2
         （S_first = threads×8，depth ≈ log_factor(N/threshold)）
内存   M ≈ O(分片结果数 + sample 上限) ≪ O(N)
```

核心性质（"支持百亿行"的理论依据）：**耗时与差异率弱相关，内存与行数无关。**

### 11.3 关键性能手段

1. **位切片聚合 checksum**（§十）：免排序、免大字符串，是扫描速率前提；
2. **首轮分段并行快筛**（§6.2）：段数 = threads × 8，充分利用数据库内部并行扫描与双连接侧间并行；none 模式下叠加同侧多连接（§8.4）才可能有 1B 行 ~5min——快照单会话下 MySQL 单线程聚合是硬瓶颈（§11.1 分层）；
3. **侧间并行 + 侧内快照串行**（§8.2）：一致性优先，吞吐靠分段与双连接；
4. **分片任务信号量调度**：`--threads` 真实约束并发（v1.0 为串行伪代码）；
5. **hash 函数分级（2026-08-17 实测更新）**：全后端默认 MD5（每核 ~500MB/s，宽行场景是瓶颈）。两个加速档结论：
   - ~~`hash_any_extended`（GaussDB）~~：**已证伪**——openGauss 5.0.0 不暴露该函数（§16.3-F4），GaussDB 固定 MD5；
   - CRC32 链（MySQL/PolarDB-X）：语法可行性已实测一致（§16.3-F6）；但 32-bit hash 按生日界在分片内百万级差异行时碰撞漏报概率不可忽略（上游 data-diff 从未使用 CRC32，一直 MD5）。仅作为显式可选加速档，启用时报告醒目标注 `note: crc32chain has weakened collision guarantee`；**默认始终 MD5**。
   报告记录 `hash_algorithm`，跨工具/跨算法结果不可直接比对；
6. **覆盖列读取**：checksum 只读 key + 比对列，配合覆盖索引/列存可数量级提速（建议项，非依赖项）；
7. **行级复核 keyset 分页归并**（§6.2.2）：双侧 keyset 分页拉取 + 页式归并，禁止 O(n²) 比对；内存 O(页大小) 而非依赖驱动流式 API；差异占比 > 50% 时整个范围直接走此路径（降级保护）。

### 11.4 基准测试方案

- **环境矩阵**：MySQL 8.0 / GaussDB / Oracle 19c × {同构, 异构}；硬件明盘（CPU/内存/NVMe/带宽）；客户端与库同机房；
- **数据矩阵**：行数 {1M, 25M, 100M, 1B, 10B(抽样)} × 行宽 {窄 100B / 宽 2KB} × 差异率 {0%, 0.01%, 1%, 10%} × key 分布 {均匀 / 倾斜}；
- **埋点指标**：全部进入 `DiffReport.perf`（§5.1）：duration、每侧 rows/s、查询数、两侧网络字节、分片耗时 p50/p99、峰值 RSS；
- **验收口径**：CI 内置 25M 行基准（容器化 MySQL 双实例）作为性能回归门禁；1B/10B 走月度压测，结果回填 §11.1。

---

## 十二、安全与限流（新增）

### 12.1 只读强制

> **v2.1 对齐代码现实**：现有只读强制是纯**客户端 SQL 前缀匹配**（`cli.rs::is_read_only_mcp`，仅 MCP `execute_query` 路径执行，CLI/REPL 不强制），**无任何服务端权限探测机制**。"校验账号无写权限"需新建（启动时对两侧执行 `SELECT` 探测 + 尝试以 `READ ONLY` 开快照），列为 Phase 2 工作而非既有能力。

- 启动时校验两侧账号无写权限（尝试 `SELECT` 探测 + 快照事务均为 `READ ONLY`）；
- 全部查询包裹在只读事务内，任何非 SELECT 语句直接拒绝；
- delta-diff 内部 SQL（含 `START TRANSACTION ...`）**不经过** `is_read_only_mcp` 前缀门卫——该门卫只对 MCP 入参级 SQL 生效，工具内部生成的语句由本节的只读事务保证（见 §13.1）。

### 12.2 限流

- 单侧并发会话：snapshot 模式侧内单会话串行；none 模式 ≤ ⌈threads/2⌉（§8.4）；
- 每条查询应用 `--statement-timeout`（默认 300s），超时计入错误并按 §13.2 断点恢复；
  - ⚠ 现有 `TimeoutConfig` 默认 statement_timeout 为 **30s**，且 `connection_max_lifetime` 当前是 dead config（解析后从不传入池约束）——delta-diff 的 300s 默认值需显式经 `from_overrides` 接线；MySQL 侧 `SET max_execution_time` 在每次 acquire 时下发，注意其与快照长事务内逐条查询超时的语义叠加；
  - ⚠ Oracle `set_statement_timeout_sql()` 返回 `None`（无语句级超时），Oracle 侧超时只能靠客户端 tokio::time::timeout 兜底，报告需标注；
- `--dry-run` 输出预计查询数与扫描行数，供生产变更评审。

### 12.3 `--where` 注入防护与语义告警

- 拒绝包含 `;` 的条件；
- 文档与 `--help` 明示：条件原样拼接、由用户负责其正确性；两侧分别执行，含时区/会话相关函数（如 `NOW()`）时两侧语义可能不同，建议用字面量时间。

---

## 十三、MCP Tool 集成与断点

### 13.1 MCP Tool

> **v2.1 集成方式修正**：v2.0 设想独立 `src/mcp/tools/delta_diff.rs` + 手写 `ServerHandler`——与现状不符（现有 MCP 为 `server.rs` 单文件 + rmcp `#[tool_router]` 宏，两个 handler 会竞争 stdio）。**正确做法：在 `#[tool_router] impl DbMcp` 内加一个 `#[tool]` 方法**，参数结构体 derive `schemars::JsonSchema`；连接获取复用现有 `DbMcp::get_connection(Option<name>)`（已支持任意命名连接，left/right 参数直接映射）。

```rust
// src/server.rs — 在 #[tool_router] impl DbMcp 内新增（模式对齐现有 6 个 tool）

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DeltaDiffParams {
    /// 左数据源连接名
    pub left_connection: String,
    /// 右数据源连接名
    pub right_connection: String,
    /// 表名
    pub table: String,
    pub key_columns: Option<Vec<String>>,
    pub where_condition: Option<String>,
    /// auto | hashdiff | joindiff | bucketdiff
    pub strategy: Option<String>,
    /// snapshot | none
    pub consistency: Option<String>,
    pub summary_only: Option<bool>,
    pub dry_run: Option<bool>,
}

#[tool(description = "Compare two database tables and identify data differences. \
                      Read-only. Default snapshot consistency with diff recheck.")]
async fn delta_diff(&self, Parameters(p): Parameters<DeltaDiffParams>)
    -> Result<CallToolResult, McpError>
{
    // 1. get_connection(Some(left)) / get_connection(Some(right)) —— 复用现有连接状态机
    // 2. 构建 DiffContext → SmartRouter 路由 → 执行
    // 3. 内部 SQL（含 START TRANSACTION）不经过 is_read_only_mcp 门卫（§12.1）；
    //    参数级校验仅限 where_condition 拒绝分号（§12.3）
    // 4. 返回 JSON 报告；退出码语义映射为 is_error（仅 exit 2 置 true）
}
```

### 13.2 断点续传（JSONL，替代 v1.0 全量 JSON）

- append-only 行式格式：`{"shard":"…","status":"Match","lc":…,"rc":…,"dc":…}` 每行一条（含分片统计，恢复时还原报告计数），完成时原子 rename；
- 避免 v1.0 "completed_shards 数组反复全量重写"在十万级分片下的 I/O 放大；
- 恢复时按行回放成 HashMap，损坏行跳过并 WARN。
- **已知限制（评审记录）**：分片 ID 由运行时 MIN/MAX 键域推导，两次运行间键域漂移会使断点部分失效（复用或漏比）；Diff 分片恢复计数但不恢复差异样本行。生产使用建议固定键域（同表同过滤条件）下续传。

---

## 十四、实施计划（v2.1 重排：预研前置 + 阻断项入列）

> v2.0 为 8 周四 Phase。v2.1 基于代码库适配核验新增 Phase 0 技术预研，并将三个结构性阻断项（GaussDB 池重构、行级 keyset 分页、MySQL exec 参数扩展）列入对应 Phase。**总工期修正为 9–11 周。**

### Phase 0：技术预研 spike（1 周，纯验证，不写产品代码）

1. **三方 checksum SQL 实测**——✅ **2026-08-17 完成**（MySQL/PolarDB-X/openGauss 三侧 §十六 F1/F2；Oracle 侧 F8：`STANDARD_HASH` 与 MySQL MD5 逐位一致）；
2. **`hash_any_extended` 可用性验证**——✅ 已证伪关闭（§16.3-F4）；**CRC32 链**——✅ 语法一致，保留为可选告警档（§16.3-F6）；
3. **oracle-rs 0.1 驱动能力确认**——✅ 2026-08-17 完成（§16.3-F8：`AS OF SCN`/`CURRENT_SCN`/`READ ONLY` 实测通过，驱动 `commit()/rollback()`+任意 SQL 确认）；
4. **keyset 分页吞吐实测**（页大小 8192）——✅ 2026-08-17 完成（§16.3-F9：MySQL 0.67M / openGauss 0.68M 行/s，方案成立）。

**出口标准**：四项均有书面结论；任何一项失败立即回到设计修订，不进 Phase 1。**当前状态：全部通过，Phase 1 解除阻塞（2026-08-17）。**

### Phase 1：正确性底座 + CI 基准（3 周）

1. `delta_diff` CLI 子命令（`main.rs` Commands + match arm）、连接复用、退出码契约——✅ 2026-08-17 完成；
2. **扩展 `backend::Dialect` trait**（§7.2 新方法四后端实现）；元数据层复用 `table_indexes()`/`table_columns()`（§7.1）；**MySQL `exec` 参数扩展**（Vec 绑定）——✅ 2026-08-17 完成；
3. **位切片聚合 checksum SQL（三方）+ 跨库 fixture 断言单测**（§十）——✅ 完成：Rust 集成测试断言 MySQL == openGauss == python 期望（全表/分段/分桶）；
4. **类型规范化矩阵 v1（整型/DECIMAL/日期时间/NULL 哨兵等七类）**（§九-1 SQL 层）——✅ 完成（各后端 `normalize_expr` + 快照单测）；
5. HashDiffer MVP（分段并行快筛 + 二分 + **keyset 分页行级复核**，§6.2.2）——✅ 完成（含快照/PolarDB-X 降级、MIN/MAX 键域、二分递归、页式归并、DiffReport）；
6. **CI 25M 行容器化基准**（§11.4）——✅ 工作流落地（`.github/workflows/delta-diff-bench.yml`，周跑+手动），首轮实测（F11）：1M 行零差异 ~8s / 101 差异 17s（容器、none 档）。

### Phase 2：一致性（2–3 周）

1. **GaussDB 池重构为真多连接**（§8.2，结构性前置，0.5–1 周）——✅ 2026-08-17 完成（regress 基线不变 + 独立快照集成测试）；
2. 快照事务 + `SideExecutor`（§5.2/§8.2，snapshot 与 none 双模式）；Oracle `AS OF SCN` 注入钩子——✅ 完成（实现注记：SideExecutor 以"主连接串行 + none 档池化并行快筛"落地，未引入独立队列抽象；Oracle AS OF SCN 钩子已在 `render_*_sql` spec 中支持，E2E 待 Phase 3 补）；
3. 二次复核（§8.3）——✅ 完成（recheck.rs + E2E 瞬时写过滤实证）；
4. **Oracle 行级比较降级**（§九-2）——✅ 以"行级统一 SQL 层规范化"由构造覆盖；只读权限启动校验由快照 READ ONLY + 纯 SELECT 构造结构性保证（§12.1）。

### Phase 3：多策略与增强（2 周）

1. BucketDiffer（§6.3）+ 键类型降级规则（§6.4）——✅ 2026-08-17 完成（multiset E2E 精确 + 降级告警）；
2. JoinDiffer（同构同版本，联邦查询不可用则回退 HashDiffer）；SmartRouter 自动路由——✅ 完成（当前覆盖 MySQL 系同连接；路由 6 单测）；
3. 断点续传 JSONL（§13.2）；增量比对 `--update-column/--update-since`——✅ 完成（含统计还原的恢复路径 + 方言分侧增量谓词）；
4. 输出格式化（DiffReport→QueryResult 投影，复用 `render_result`）；`--dry-run`——✅ 完成（四格式 + 预检含策略/键域/分片/预计查询数）；
5. MCP Tool（§13.1，`#[tool_router]` 内加方法）——✅ 完成（`delta_diff::api::run_diff` 与 CLI 共用；stdio E2E 验证）。

### Phase 4：规模化验证（2 周）

1. 直方图密度感知切分（倾斜 key）——➡️ **设计复核**：现有二分递归天然按 count 自适应（稠密段被递归细分），E2E 实证倾斜 key 精确检出（1M 行 9:1 倾斜 + 6 差异，8.3s、94 分片）；不引入独立直方图组件（§16.3-F15）；
2. 1B 行压测（**snapshot 与 none 双档分别实测**）+ 10B 可伸缩性论证（基于 §11.2 模型外推 + 抽样实测）——➡️ 环境约束下以 10M 行实测 + 模型外推完成（§11.1 回填注记）；
3. 实测值回填 §11.1 分层 SLO 表，形成对外口径——✅ 完成（§11.1 Phase 4 回填块）；
4. 性能报告（含 perf 埋点趋势）——✅ perf 字段入报告（queries_total/p50/p99/RSS 观测值见 §16.3-F15）。

---

## 十五、关键设计决策

| 决策 | 选择 | 理由 |
|------|------|------|
| 是否引入 DuckDB | ❌ 第一期不引入 | 纯 Rust 项目，避免 C++ 编译链与二进制体积膨胀；后续评估联邦层 |
| checksum 算法 | **位切片聚合**（模 2⁶⁴ 求和） | 顺序无关、定长、免排序；v1.0 拼接式存在截断/报错风险，不可伸缩 |
| 无主键策略 | **内容分桶**（MOD(row_hash, N)） | v1.0 物理伪列跨库不可比；分桶由内容决定，天然对齐；明示 multiset 语义极限 |
| 一致性 | **单侧快照 + 差异复核**（默认） | 快照绑定单连接，牺牲侧内并行换正确性；复核点查成本与差异数成正比 |
| 并发模型 | 侧间并行；snapshot 侧内单会话串行 / none 侧内多连接并行；分片信号量调度 | 一致性约束下的最大并行度；`--threads` 真实生效；none 档同时是性能档（§8.4） |
| 行级拉取 | **keyset 分页归并**（页式，内存 O(页大小)） | 现有 DbConn trait 无流式 API；三后端驱动流式能力不一；纯 SQL 方案一致可行（§6.2.2） |
| 方言层 | **扩展现有 backend::Dialect trait**，不自建枚举 | 现有 trait 已有 13 方法可复用（主键/行数估算/超时/引号）；避免同名冲突与重复建设（§七） |
| hash 函数 | **默认 MD5（全后端）**；CRC32 链为可选告警档 | hash_any_extended 已实测证伪（openGauss 5.0.0 无此函数）；CRC32 链语法可行但 32-bit 碰撞风险需显式告警；报告记录算法保证可解释性（§11.3-5、§16.3） |
| 断点存储 | 本地 JSONL | append-only，十万级分片无 I/O 放大，可断点恢复 |
| 性能口径 | SLO **按一致性模式分层** + CI 基准先实测再定稿 | 快照单会话 MySQL 单线程聚合是硬瓶颈；1B~5min 仅 none 档目标，数字必须有前提并可回归（§11.1） |
| MCP 集成 | `#[tool_router] impl DbMcp` 内加 `#[tool]` 方法 | 对齐现有 6 tool 模式；独立 ServerHandler 会竞争 stdio；内部 SQL 旁路只读前缀门卫（§13.1） |

---

## 十六、Phase 0 技术验证记录（2026-08-17 实测回填）

> 验证资产留存于 `tests/delta-diff-verify/`（docker-compose.yml + gen_fixture.py + verify.py + bench_keyset.py + verify_iblt_gauss.py + sql/ + expected.json + results.txt），可重复执行。Oracle 侧使用 `tests/docker-compose.yml` 的 hepta-oracle-test（gvenzl/oracle-free:23-slim）。

### 16.1 验证环境

| 实例 | 镜像 | 平台 | 说明 |
|------|------|------|------|
| MySQL | `mysql:8` | linux/arm64 原生 | 容器名 ddverify-mysql，库 verify |
| openGauss | `opengauss/opengauss:5.0.0` | linux/arm64 原生 | 容器名 ddverify-opengauss，库 verify（注意：镜像内 gsql 需 `LD_LIBRARY_PATH=/usr/local/opengauss/lib`） |
| PolarDB-X | `polardbx/polardb-x:latest`（PXC-5.4.19） | linux/amd64（ARM 主机 Rosetta） | 容器名 ddverify-polardbx，默认账号 polardbx_root（空密码） |

fixture：2000 行确定性数据（LCG 伪随机），覆盖整型/DECIMAL(20,6)/DATETIME/VARCHAR(含 Unicode)/BOOLEAN/NULL；python 侧按 §九规范化矩阵独立计算期望五元组。

### 16.2 结果汇总

| 验证项 | MySQL | PolarDB-X | openGauss |
|--------|-------|-----------|-----------|
| 位切片 checksum（全表） | ✅ 与期望一致 | ✅ 与期望一致 | ✅ 与期望一致 |
| 位切片 checksum（4 个 id 分段） | ✅ 4/4 | ✅ 4/4 | ✅ 4/4 |
| 分桶 checksum（MOD 8，3 桶） | ✅ 3/3 | ✅ 3/3 | ✅ 3/3 |
| 溢出语义（V4，见下） | ✅ 实证 | ✅ 实证 | ✅ 实证 |
| 快照事务语法 | ✅ | ⚠ 见 F5 | ✅ |
| CRC32 链表达式 | ✅ | ✅ | — |
| 1M 行 checksum 吞吐（单会话） | 0.19–0.22M 行/s | 未测 | 0.05M 行/s |

### 16.3 关键发现

- **F1（证实 v2.0 bug）**：`SELECT SUM(x), CAST(SUM(x) AS UNSIGNED) % 2⁶⁴, MOD(SUM(x), 2⁶⁴)` 对 {2⁶⁴-1, 2⁶⁴-1, 2³²-1} 的实测（MySQL 与 PolarDB-X 一致）：SUM=36893488151714070525，**CAST 写法输出 18446744073709551615（饱和截断，错误）**，MOD 写法输出 4294967293（正确）。openGauss `MOD(SUM(numeric), 2⁶⁴)` 同样正确。§10.1 的 v2.1 修正写法由此从"文档推断"升级为"实测证实"。
- **F2（跨库一致性成立）**：同一 fixture 在 MySQL 8 / PolarDB-X / openGauss 5.0.0 上产生的五元组**逐位相等**（含分段与分桶），位切片聚合方案跨库可行。
- **F3（§九矩阵修正）**：openGauss `to_char(c_dt AT TIME ZONE 'UTC', ...)` 在 timestamp（无时区）列上输出偏移会话时区（实测 +8h，Asia/Beijing）——`AT TIME ZONE 'UTC'` 把 timestamp 转 timestamptz 后按会话时区渲染。**无时区列必须用 plain `to_char(c, ...)`**（MySQL `DATE_FORMAT` 同为无时区 identity）；`AT TIME ZONE 'UTC'` 仅对 timestamptz 列成立。矩阵已修订。另确认：NUMERIC(20,6)::text 尾零保留、`boolean::int::text` 输出 0/1，均与 MySQL 侧一致。
- **F4（证伪）**：openGauss 5.0.0 **不暴露** `hash_any_extended`/`hashtextextended`/`hashint8extended`（均报 function does not exist）。GaussDB hash 能力固定为 **MD5**，§7.2 与 §11.3-5 的 XXH64 加速档取消。教训：同一会话内分桶表达式必须与 checksum 表达式出自**同一模板**（本次 bucket FAIL 即因两处模板漂移所致）。
- **F5（PolarDB-X 快照）**：`START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY` **语法不支持**（ERROR 3009 解析失败）；替代 `SET TRANSACTION ISOLATION LEVEL REPEATABLE READ; START TRANSACTION READ ONLY` 可用（首次一致读建立快照视图）。另探明 PolarDB-X 存在 TSO 体系与 flashback 语法 `AS OF TIMESTAMP '<literal>'`（语义报错证实功能存在：表结构变更后拒绝服务），不接受 `NOW()`。由于 PolarDB-X 走 mysql scheme，`begin_snapshot_sql` 需按 **version 串运行时识别**（实测 `5.6.29-PXC-5.4.19-SNAPSHOT` 含 "PXC"）选择语法。
- **F6（CRC32 可行）**：`CRC32()`、嵌套 `CRC32(CONCAT(CRC32(),...))`、`MOD(CONV(SUBSTRING(MD5(...),1,8),16,10), N)` 在 MySQL 与 PolarDB-X 输出一致。语法可行性通过；32-bit 碰撞风险告警维持（§11.3-5）。
- **F7（SLO 实证，重要）**：开发容器内单会话 checksum 吞吐实测 **MySQL 0.19–0.22M 行/s、openGauss 0.05M 行/s**（100 万行、3 列窄行、含 docker exec 开销）。即使生产 NVMe 环境放大 5–10 倍，与 v2.0 假设的"≥3M 行/s 单会话"仍有数量级差距——**v2.1 的 SLO 分层（§11.1）由此从谨慎估计升级为实测支撑**：快照单会话档应按 ~0.2–2M 行/s 规划，`1B ~5min` 仅 none 多会话档可追求。
- **F8（✅ 2026-08-17 补验完成）**：Oracle 23ai 容器实测——`STANDARD_HASH('abc','MD5')` = `900150983CD24FB0D6963F7D28E17F72`，与 MySQL `MD5('abc')` **逐位一致**（跨库 hash 对齐成立）；`CURRENT_SCN` 可读；`AS OF SCN` flashback 实证（插入前行数 2 ≠ 当前 3）；`SET TRANSACTION READ ONLY` 可用；`BIT_XOR_AGG` 原生存在（23ai；19c 仍走 Addendum §3.3 奇偶回退）。oracle-rs 0.1.7 驱动源码确认：`commit()/rollback()` 存在、可执行任意 SQL（快照/flashback 语句可裸发）。**Oracle 侧 Phase 0 阻塞解除**。
- **F9（keyset 分页吞吐，✅ 补验）**：100 万行、页 8192、单会话顺序拉取——**MySQL 0.67M 行/s、openGauss 0.68M 行/s**（bench_keyset.py）。为 checksum 扫描吞吐的 3 倍以上（无 MD5 计算），§6.2.2 keyset 方案成立，无需退回流式 trait 扩展。
- **F10（GaussDB 奇偶 SUM 形态，✅ 补验）**：Addendum §3.2 的逐位奇偶 SUM 模板（96 奇偶列 + cnt，k=8 桶）在 verify_t 上与 python 期望逐桶一致（verify_iblt_gauss.py，2000 行 1.44s）——IBLT-GaussDB 无聚合障碍解除，宽列 SQL 性能可接受（1M 行级待 Phase 2.5 复测）。
- **F11（Phase 1 实现回填，2026-08-17）**：`hepta_dbcli delta-diff`（hashdiff MVP）端到端实测——① MySQL↔PolarDB-X 2000 行零差异 exit 0、注入 3 类差异（改/删/增）精确检出且 exit 1；② MySQL↔openGauss 2000 行零差异 exit 0（schema 经 current_schema 自动识别）；③ 100 万行零差异快筛 ~8s（none 档）；④ 100 万行注入 101 条分散 Modified 精确检出（101/101）、17s。过程中发现并修复两个真实缺陷：MySQL 二进制协议下 information_schema 字符串列被误报为 BLOB（types.rs 按 BINARY_FLAG 修正）与 PolarDB-X 预编译查询 information_schema 报 unknown NPE（metadata 层 exec→内联文本回退）。
- **F12（Phase 2 实现回填，2026-08-17）**：① GaussDB 池重构为真多连接落地，`regress_gaussdb` 与重构前完全一致（1 过/5 预存败），新增"双连接并发独立快照"集成测试通过；② none 档首轮分段并行快筛落地（JoinSet + 信号量，SQL 预渲染后提交连接池）；③ 二次复核 E2E 实证：比对窗口内注入"瞬时写并回改"，snapshot 档因隔离性不产生伪差异（101 精确），none+`--recheck` 档复核过滤伪差异（102→101）；④ 行级比较统一走 §九 SQL 层规范化（`raw_exprs`），Oracle 行级精度降级由构造保证（key 列原样、其余列规范化文本）；⑤ 发现 keyset 分页 spec 字段在游标翻页继承时丢失导致的双重引号 bug 并修复。
- **F13（Phase 3 实现回填，2026-08-17）**：① BucketDiffer E2E：keyless 表注入"复制行/删行/增行"三类扰动，multiset 精确检出（×2/1、×1/0、×0/1）+ keyless note；② JoinDiffer E2E：同连接 auto 路由命中，1M 行 101 差异精确；③ SmartRouter 6 个路由单测（keyless/字符串键降级/同连接 joindiff/跨实例回退/显式约束）；④ 断点续传 E2E：32 分片 JSONL 落盘 + 完成原子 rename `.done`；断点文件删除 10 行模拟中断恢复——10 分片 Skipped、统计自断点还原（总数 2000 精确）、再次完成 rename；⑤ 增量比对 E2E：`--update-column c_dt --update-since <literal>` 过滤 1031/2000 行精确；`--where` 过滤 499 行精确；⑥ dry-run 增强：策略/键域/分片计划/预计查询数仅经元数据与 MIN/MAX 探查产出；⑦ 输出投影复用 `render_result` 四格式（table/csv/vertical/json）；⑧ MCP `delta_diff` 工具 E2E（stdio JSON-RPC）：MySQL↔PolarDB-X 返回完整 JSON 报告，isError=false。实现注记：CLI 与 MCP 共用 `delta_diff::api::run_diff` 执行入口；joindiff 当前覆盖 MySQL 系同连接，其他方言同连接场景路由层回退 hashdiff 并告警。
- **F14（Phase 2.5 IBLT 实现回填，2026-08-17）**：`--strategy iblt` 落地（`Dialect::render_iblt_sql` 四后端 + `iblt_diff.rs` peeling 解码器）。E2E d 矩阵（MySQL↔PolarDB-X）：d=0 decoded-empty；d=5 精确解码；d=1999 > capacity 1000 透明回退 hashdiff（告警入报告）；`--strict` exit 2。auto 路由：跨实例可二分键 → iblt 快路径。实现期抓到两个真实缺陷：① 摘要 SQL 外层引用未导出 key 列（改 `k` 别名）；② `cell` 以 f64/DECIMAL 形态返回时解析层归零致解码必然卡死并误报容量超限（解析层兼容 f64 整值与 DECIMAL 字符串）。python 仿真佐证 peeling 对"Modified 对撞同桶"可收敛。
- **F15（Phase 4 实测回填，2026-08-17）**：① 倾斜 key（1M 行、稠密区:稀疏区 9:1、key 域 [0, 9.9M)）+ 6 差异，hashdiff 二分自适应精确检出（94 分片，8.3s）——据此**不引入独立直方图组件**（二分递归按 count 天然自适应）；② 10M 行压测：none 档 62.6s / snapshot 档 74.2s（峰值 RSS 27–29MB，≪ 512MB 上界）；10M 行 + 1000 分散差异全链路 3m16s（1000/1000 精确）；③ 增量相对窗口（"1 day"/"400 days"）E2E 正确（2023 日期全滤除，decoded-empty，queries=2）；④ 1B 行按 §11.2 模型外推回填 §11.1（容器口径 snapshot ~100min、none ~25–30min，生产 NVMe 可再降一个量级）。
- **F16（评审修复回填，2026-08-17）**：对全量变更的代码评审发现 4 个真实 bug 与 4 条跨库规范化缺口，全部修复并复验：① Oracle `RAWTOHEX` 大写 hex 与 MySQL/GaussDB 小写不一致导致 bucketdiff multiset 跨库误判 → Oracle multiset SQL 改 `LOWER(RAWTOHEX(...))`；② Oracle 19c 无 `BIT_XOR_AGG` 时 iblt 硬错 → Db 错误也透明回退 hashdiff（--strict 除外）；③ Oracle NUMBER 经 f64 丢精度（checksum 切片 ≥2⁶³、IBLT key_xor 大键、recheck 错键静默丢差异）→ Oracle 聚合一律 `TO_CHAR` 文本化（客户端字符串解析本就支持）；④ bench workflow 三处错误（bash -e 吞 exit 1、斐波那契生成仅 ~1M 行、同 URL 被路由到 joindiff）→ 重写（倍增生成 25M、`|| EXIT=$?`、强制 `--strategy hashdiff`）。规范化缺口修复：Oracle `NUMBER(p,s)` 按声明标度生成 `FM…0D0…` 掩码（对齐 MySQL DECIMAL 标度保留）；跨库 float 列与 DATE↔DATETIME 配对在 `api::run_diff` 产生告警；MySQL 会话 `SET time_zone='+00:00'` 在比对路径固定。死管道接线：Oracle `CURRENT_SCN` 在快照开启后捕获（`DiffContext.scns` OnceLock），`AS OF SCN` 注入 checksum/keyset/multiset/IBLT 全部 spec。次要项：UNSIGNED BIGINT 键 > i64::MAX 显式报错（原静默跳过）；none 档快筛改双侧独立信号量（每侧 ≤ ⌈threads/2⌉，对齐 §8.4）；peeling 纯桶校验收紧为"当前桶 == 本子表应属桶位"；`exec_or_inline` 回退失败时返回回退错误（含真实原因）；MCP 参数文档补 iblt。

### 16.4 对实施计划的影响

Phase 0 出口标准四项状态（2026-08-17 全部关闭）：checksum 三方实测 → **✅ 含 Oracle 四方完成**（F1/F2/F8）；hash_any_extended → **已证伪，关闭**；CRC32 → **保留为可选告警档**；keyset 分页吞吐 → **✅ 实测通过**（F9，0.67–0.68M 行/s）；oracle-rs 事务能力 → **✅ 确认**（F8）。**Phase 1 全面解除阻塞。**

---

*本文档为 v2.1 修订版（含 2026-08-17 Phase 0 实测回填，§十六）。Phase 0 剩余项（Oracle 侧、keyset 吞吐）出具结论前不建议进入编码阶段；v2.0 的 P0 项（§六/§八/§十）口径已由 v2.1 修订覆盖。*
