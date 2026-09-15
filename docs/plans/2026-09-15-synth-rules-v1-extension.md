# synth rules v1 扩展提案：列值写入优先级 + #68/#69/#70/#76 schema 冻结

> **Status: proposal / frozen contract.** 本文是 #68、#69、#70、#76 四个 issue 的
> schema 契约。任何实现 PR 只能使用这里冻结的字段名与优先级，**不得自行发明字段**。
> 需要改字段 → 先改本文并说明理由。
>
> 关联：#68（列级固定值/区间 + copula 条件采样）、#69（rules-draft 条件规则挖掘）、
> #70（derive 表达式 + 分支覆盖率修复）、#76（shadow-seed 规则模型借鉴）。

**Goal.** 四个 issue 都要改 `rules.rs` 的 serde schema 与 `validate()`；如果不先冻结，
会出现三套并行 schema（#68 表级 `fixed_columns`、#76 列级 `columns.*`、#70 表级
`derive/branches`）和三次逐字节回归。本文一次性定型，之后各 PR 只填实现。

**Depends on / already landed（已核实，非假设）:**

| 事实 | 位置 |
|---|---|
| `rust_decimal` 已是依赖 | `dbcli/Cargo.toml:72` |
| 列模型已有 `decimal_scale`，生成器已有 `quantize` / `round_to_scale` | `synth/model.rs`、`synth/generator.rs:296-335` |
| ECDF 边际（inverse CDF 可用） | `synth/marginal.rs`（#66 已合） |
| `GeneratedData` 已物化全表行 | `synth/generator.rs:27-32`，`cmd.rs::run_generate` = `generate()` → `export()` |
| `rules.validate()` 已在 generate 前调用 | `synth/cmd.rs:631-632` |
| `RulesDraft` 已有 `TableStats{cardinality}` 唯一性判据 | `synth/rules_draft.rs` |
| `profile` 已有 `cardinality` / `top_values` / `column_order` | `synth/profile.rs` |
| `main.rs` / `lib.rs` 顶部已有 `#![allow(dead_code)]` | 未接线的新模块不会触发 clippy |

---

## 1. 列值写入优先级（唯一权威表）

对同一行、同一列，按下列顺序应用，**后者覆盖前者**。实现必须严格按此顺序，
任何偏离都要在 PR 里写明理由。

| 阶段 | 来源 | 作用范围 | 备注 |
|---|---|---|---|
| 1 | copula / marginal 采样 | 全部行全部列 | 基础值 |
| 2 | FK 池赋值（relationship） | FK 列全部行 | 从父表 `column_pools` 复制，保证引用完整性 |
| 3 | `columns[col].null_rate` | 该列全部行 | 现行为，不变 |
| 4 | `columns[col].fixed` / `.values` / `.fixed_range` | 该列全部行 | 本提案新增；`copula_conditional` 模式在此阶段做条件采样 |
| 5 | 表级 `rules[].when` + `set` | 仅被选中的行 | #76-B；`set` 后 `derive` 可读到新值 |
| 6 | `derive[]` | derive 目标列全部行 | #70；无条件重算，可引用阶段 1-5 的结果 |
| 7 | 覆盖率修复循环 | 仅「可翻列白名单」 | #70-4；翻行后对受影响行重放阶段 5-6 |

**可翻列白名单**（修复循环唯一允许改动的列集合）：

```
可翻 = 非 FK 列 ∧ 非被其他表 references 的列 ∧ 非 derive 目标 ∧ 非 branches.predicate 直接依赖的常量列
```

**分支互斥（第三轮补充）**：同一轮里，已被兄弟分支改写的行、以及已命中任一分支的行，都不允许
被其他分支改写。`set` 只能让行**命中**谓词，改写这两类行只会破坏兄弟分支已达成的覆盖；
让它们只从「不命中任何分支」的行里取，两个争抢同一列的分支才能在一轮内收敛（见 §9.1/#2）。

理由：FK 值在生成时从父表复制，父表行也可能已被子表引用；改动 FK/被引用列会破坏
引用完整性。derive 目标由公式决定，不能独立翻动。

**derive 求值顺序**：derive 之间可互相引用，需按列级依赖拓扑排序求值，成环在加载期报错。

---

## 2. 冻结的 schema（v1 向后兼容）

`version` 保持 `"1"`。所有新增字段 `#[serde(default)]`，旧文件解析行为不变。

```yaml
version: "1"
tables:
  - name: orders
    rows: 1000
    strategy: uniform            # uniform | weighted | zipf（不变）

    columns:
      part_date:
        fixed: "20240101"        # 标量，全表同值；与 values 互斥
      status:
        values:                  # 加权值池；省略权重则均匀；与 fixed 互斥
          normal: 0.7
          peak: 0.3
      created_at:
        fixed_range: ["2026-01-01", "2026-01-31"]   # [low, high]，闭区间
        mode: copula_conditional  # rejection | copula_conditional（默认 rejection）
      note:
        null_rate: 0.05
        marginal: ecdf            # 不变

    rules:                       # 条件行规则（#76-B；#70 的表达式 predicate 是超集）
      - ratio: 0.8               # 可选；占全表行数比例
        when:                    # 可选；条件列表为 AND；NULL 不匹配任何条件
          - { column: bs, op: eq, value: "1" }
        set:    { part_date: "20240101" }        # set / derive 至少其一
        derive: { ghcjje: "cjje * 1.005" }       # #70 表达式引擎的同一入口

    derive:                      # 无条件列级派生（#70）
      - { column: total, expr: "price * qty" }

    branches:                    # #70 覆盖率修复
      - id: paid
        predicate: "status == 'A'"
        target_ratio: 0.30
        tolerance: 0.015          # 可选，默认 0.05（小分支自适应）
        repair:
          set: { closed_at: "null" }
          linked_derive_recompute: true

    relationships:                # 不变
      - pk: user_id
        references: ["users.id"]
        pool_strategy: { projection: { unique: false } }
        null_label: "null"
```

约束：

- `columns[col]` 新增 `fixed` / `values` / `fixed_range` / `mode`，与既有
  `null_rate` / `marginal` 共处一个 map，**不新增表级 `fixed_columns`**（否决 #68 原提案
  的表级形状，理由：同一语义不能有两种写法）。
- `rules[]` / `derive[]` / `branches[]` 均为表级，`#[serde(default)]`。
- `when` 的 `op` ∈ `eq | ne | lt | le | gt | ge | in | not_in | between`（结构化，
  不引入表达式求值；#70 的 predicate 表达式是超集）。
- `values` 权重和必须 ≈ 1.0（±1e-6），全部 > 0。

### 2.1 冻结的 Rust 类型

```rust
// rules.rs
pub struct ColumnRule {
    pub null_rate: Option<f64>,          // 既有
    pub marginal: Option<String>,        // 既有
    pub fixed: Option<String>,           // 新增：字面量，按 logical_type 类型化
    pub values: Option<ValuePool>,       // 新增
    pub fixed_range: Option<[serde_json::Value; 2]>, // 新增：[low, high]，闭区间
    pub mode: ColumnMode,                // 新增，默认 Rejection
}

/// YAML 两种形态：`values: {a: 0.7, b: 0.3}`（加权）与 `values: [a, b]`（均匀）。
/// 加权形态用 BTreeMap 而非 HashMap，保证迭代顺序确定（同 seed 可复现）。
#[serde(untagged)]
pub enum ValuePool {
    Weighted(std::collections::BTreeMap<String, f64>),
    Uniform(Vec<String>),
}

#[serde(rename_all = "snake_case")]
pub enum ColumnMode { Rejection, CopulaConditional }   // Default = Rejection
```

`fixed_range` 用 `serde_json::Value` 两端，兼容日期字符串（`"2026-01-01"`）与数值
（`1.0`），避免「数值必须加引号」的伪约束。

### 2.2 表达式引擎契约（`synth/expr.rs`，#70）

语法（**冻结**，不得扩展）：

```
expr    := or
or      := and ( "||" and )*
and     := cmp ( "&&" cmp )*
cmp     := add ( ("==" | "!=" | "<=" | ">=" | "<" | ">") add )?
add     := mul ( ("+" | "-") mul )*
mul     := unary ( ("*" | "/" | "%") unary )*
unary   := "-" unary | primary
primary := number | string | column | "(" expr ")"
number  := 十进制字面量，解析为 rust_decimal::Decimal
string  := '单引号'（仅用于比较）
column  := 标识符
```

**必须拒绝**（加载期 fail-fast，错误信息要指明违规节点）：

- 函数调用 `min(price, qty)`
- 属性访问 `price.__class__`
- 下标访问 `cols[0]`
- 未知列（列名集合由调用方提供）
- 引号外出现的任何其他字符

**语义**：

- 算术全用 `rust_decimal::Decimal`，禁止 f64 往返，`0.1 * 3 == 0.3` 必须精确成立；
- 除零是错误（不是 panic，不是 Inf）；
- 与 NULL 的任何比较结果都是 false；
- 比较两侧类型不匹配（字符串 vs 数值）是错误，**但有一个字面量级例外**：表达式里写出的引号字面量若能解析成数字，则与数值操作数按数值比较（`bs == '1'` 对数字型 `bs` 成立）。列**数据**永不参与强制转换，`name == 1`（字符串列 vs 数字字面量）仍是错误。理由：纯数字 VARCHAR 会被训练判为 numerical，而 `'1'` 是用户最自然的写法。

**API 形状**（签名可微调，能力必须齐备）：

```rust
pub struct Expr { .. }
impl Expr {
    /// 只做语法 + 白名单校验，不接触列名。
    pub fn parse(src: &str) -> Result<Expr, ExprError>;
    /// 引用列存在性校验（加载期）。
    pub fn check_columns(&self, known: &std::collections::BTreeSet<String>) -> Result<(), ExprError>;
    /// 求值；lookup 返回该列的 JSON 值。返回 bool 用于 predicate。
    pub fn eval_bool(&self, lookup: &dyn Fn(&str) -> Option<serde_json::Value>) -> Result<bool, ExprError>;
    /// 求值；返回 Decimal 结果用于 derive。
    pub fn eval_decimal(&self, lookup: &dyn Fn(&str) -> Option<serde_json::Value>) -> Result<rust_decimal::Decimal, ExprError>;
}
```

模块必须是纯函数式的（无 I/O、无全局状态），`#70` 接线阶段直接调用。

---

## 3. 配置期校验矩阵（`rules.rs::validate`，fail-fast，错误信息必须含表名+列名）

| # | 条件 | 语义 | 来源 |
|---|---|---|---|
| V1 | `fixed` 与 `values` 同时出现 | 互斥 | #76-AC4 |
| V2 | `fixed`/`values`/`fixed_range` 与 `null_rate > 0` 同时出现 | 互斥（阶段 4 覆盖整列，rate 无意义） | #76-AC4 |
| V3 | `fixed`/`values` 的列是被其他表 `references` 的父键 | 唯一性数学不可达 | #76-AC4 |
| V4 | `fixed`/`values`/`fixed_range` 的列同时是 relationship 的 `pk`（FK 子列） | 引用完整性 | #68 冲突校验 |
| V5 | `values` 权重和偏离 1.0 或存在 ≤ 0 权重 | 概率非法 | #76-A |
| V6 | `fixed_range` 的 low > high，或列类型不是可比较类型 | 区间非法 | #68 |
| V7 | `derive[].column` 引用不存在的列 / 引用了被 fixed 或被 `set` 写的列 | 优先级矛盾 | #70 |
| V8 | derive 链成环 | 无法求值 | #70 |
| V9 | `branches[].predicate` 语法错误 / 引用未知列 | 加载期 fail-fast | #70-AC2 |
| V10 | 表达式含白名单外节点（函数调用、属性访问、下标） | 安全 | #70-AC2 |
| V11 | 修复循环目标列落在可翻列白名单外（FK 列 / 被引用父键 / derive 目标 / 被 fixed 等 pin 的列） | 引用完整性；YAML 可判定的部分在 `validate()` 加载期拒绝，未知列只能在生成期判定 | 本文 §1 |
| V12 | branch 谓词在**所有**生成行上都求值失败 | 类型写错（如字符串列 `name == 1`），不能静默报 0% 覆盖 | 实现期发现 |

---

## 4. 确定性与回归契约

- **旧 rules 逐字节一致**：不含任何新字段时，生成输出必须与当前版本逐字节相同
  （#76-AC8 / #70-AC5）。守护测试见 §6。
- **新能力必须用独立 RNG 流**：规则/修复循环不得借用既有的 copula / null / FK 流，
  流种子按 `djb2("{table}:{purpose}:{index}")` 派生（沿用现状 `column_null_rng` /
  `table_seed` 模式）。为 0 的功能不得消耗任何随机数。
- **固定列不扰动其他列**（#68 非功能）：`fixed` 的逐行取值不得改变其他列的随机流。

---

## 5. 切片与文件归属（供并行执行）

| 工作流 | 内容 | 独占文件 | 依赖 |
|---|---|---|---|
| WS-A | #69 条件规则挖掘 + #76-E 隐式 FK 推断 | `synth/rules_draft.rs`、新增 `synth/mine.rs`、`synth/cmd.rs`、`synth/mod.rs` | 无 |
| WS-B | #68 列级 fixed/values/range + rejection + §3 校验 V1-V6/V11 | `synth/rules.rs`、`synth/generator.rs` | 本文 §1-§3 |
| WS-C | #70 表达式引擎（求值器 + 白名单 + 加载期校验），先不接线 | 新增 `synth/expr.rs`、`synth/mod.rs` | 本文 §2 的 expr 语法 |
| 串行后置 | #68 copula_conditional；#70 derive/规则接线 + 修复循环；#76-C WARN | `synth/copula.rs`、`synth/generator.rs` | WS-B、WS-C 合入后 |

`synth/mod.rs` 是唯一共享文件（各加一行 `pub mod`），冲突面可接受。

---

## 6. 先行守护测试（本提案的一部分，已随本文落地）

- `generator::tests::should_keep_legacy_rules_yaml_output_byte_identical`
  —— 用**真实 YAML 文件**（tempfile + `SynthRules::load`）驱动生成，锁定当前输出。
- `rules::tests::should_accept_legacy_yaml_without_new_fields`
  —— 锁定旧 rules 文件在 schema 扩展后仍能解析与校验。

任何 WS-B/WS-C 提交都必须先让这两个测试保持绿，再改 schema。

---

## 7. 仍待决策（实现期定，不阻塞开工）

| # | 问题 | 倾向 |
|---|---|---|
| D1 | `values` 的类型化输出（数值 vs 字符串） | 按 `logical_type` + `numeric_value_or_string`，避免数值分区键被引号包裹（#76-A） |
| D2 | coverage WARN/独立校验的 exit code 语义 | warn 不阻断主流程；`synth report` / 独立校验模式阻断。与 #76-C 一次定死 |
| D3 | `copula_conditional` 与 rejection 的默认 | 默认 `rejection`；条件满足率 < 1% 时显式失败并提示改用 `copula_conditional`（#68 非功能） |
| D4 | 挖掘候选是否自动启用 | 硬约束：**永不自动启用**，仅注释/清单（#69 非功能） |

---

## 8. 执行记录与验收证据（2026-09-15）

### 已落地

| 提交 | 范围 | 文件 |
|---|---|---|
| `08b86be` | 本文 + 两个守护测试 | `docs/plans/…`、`rules.rs`、`generator.rs` |
| `a084504` | 预注册 `expr` / `mine` 模块槽 | `mod.rs` |
| `0763ba1` | §2.1 / §2.2 冻结类型与表达式契约 | 本文 |
| `1a006f6` | copula 条件采样数学 | `copula.rs` |
| `7df6d7b` | #70 表达式引擎（未接线） | `expr.rs`（新） |
| `21c59fa` | #69 挖掘 + #76-E 隐式 FK | `mine.rs`（新）、`rules_draft.rs`、`cmd.rs`、`mod.rs` |
| `f24a6da` | #68 fixed/values/fixed_range + V1-V6 | `rules.rs`、`generator.rs` |

### 验收证据（真实 CLI，seed 42，2000 行）

| 需求 | 检查 | 观察结果 |
|---|---|---|
| #76-AC1 固定值 | `columns.part_id.fixed: "20240101"` 生成 2000 行 | 2000/2000 行 == 20240101；JSON 中为 **int** 而非字符串 |
| #68-AC1 类型化输出 | 同上，`--format sql` | `VALUES (…, 20240101, 'normal')`：数值分区键**不带引号** |
| #76-AC2 加权值池 | `values {normal:0.7, peak:0.3}` | 实际 0.699 / 0.301（∈[0.67,0.73]） |
| #68 不扰动其他列 | 同批生成里 `trade_id` 的取值 | 2000 行 2000 个不同值，随机流未被固定列影响 |
| #76-AC2 确定性 | 同 seed 跑两次 `--format json` | `cmp` 逐字节一致 |
| #68 fixed_range | `fixed_range: [4,6]` 500 行 | min 4.008 / max 5.999，100% 落在闭区间内 |
| #68 非功能（拒绝率） | `fixed_range: [100,200]`（分布上不可达） | exit 1，`fixed_range rejection exceeded 10000 draws … range is likely empty or nearly degenerate`，**不静默截断行数** |
| #76-AC4 / §3 V1 | `fixed` 与 `values` 并存 | exit 1，`table 'trades' column 'part_id': 'fixed' and 'values' are mutually exclusive` |
| 诚实的未实现边界 | `mode: copula_conditional` | exit 1，`mode 'copula_conditional' is not implemented yet`（不静默降级） |
| #76-AC8 / #70-AC5 回归 | 旧 rules YAML 驱动生成 | `should_keep_legacy_rules_yaml_output_byte_identical` 逐字节绿；`git diff` 中无删除的测试断言行 |

门禁：`cargo fmt --all -- --check` 干净；`cargo clippy --all --all-targets` 0 warning；`cargo test --all` 826 passed / 0 failed。

### 尚未完成的串行项

1. **把生成期断言并入 `synth report`**：值池/分支的通过情况目前由 `synth generate` 直接报告；`synth report` 的打分仍只覆盖留出集形状、列对趋势与 FK join-rate。issue #76-C 只要求「结构上预留」，故未强制并入。
2. **#76-B**：其「结构化 `when` + `set`」已被 #70 的 `branches[].predicate + repair.set` 覆盖（谓词是表达式超集），不再单独实现；若要保留 `ratio` 语义，用 `branches[].target_ratio`。
3. **#76-D**：完全由 #70 的 `derive` 覆盖。

### 后续增量（2026-09-15，第二轮）

| 提交 | 范围 |
|---|---|
| `75dd48b` | MySQL 容器验证 + `mine` 同一性列守卫 |
| `e5f00ad` | #68 `copula_conditional` 接线 |
| `c8e45f0` | #70 `derive` 接线（切片 1+2） |
| `00b25cb` | #70 `branches` 覆盖度量 + 修复循环（切片 3+4） |
| （本轮） | #76-C 值池分布校验 + `values` 标量键 |

**#70 derive 验收（真实 CLI，MySQL 8.4 训练的 `line_items` 模型，seed 42，2000 行）**

| 需求 | 检查 | 观察结果 |
|---|---|---|
| AC1 派生精确性 | `total = price * qty * 2 + 0.01`，用 Python `Decimal` 逐行比对 CSV | 2000 行 **0 违例** |
| AC1 标度 | 目标列 `DECIMAL(16,4)` | 输出量化到 4 位小数，无二进制尾差（如 `110.17`） |
| AC2 白名单 | `min(a,b)` / `price.__class__` / 未知列 | 加载期 fail-fast，错误指出违规节点与列名 |
| 依赖顺序 | `c = b + 1` 先于 `b = price * 2` 声明 | 按依赖求值，结果正确；成环在加载期报错 |
| AC5 回归 | 不含 `derive` 的 rules | 新增逐字节快照测试锁定 |

**#76-C 值池校验验收（同一容器，`events` 模型）**

| 需求 | 检查 | 观察结果 |
|---|---|---|
| 值池实际占比 vs 声明权重 | `values: {9: 0.5, 10: 0.5}`，20 行 | 偏差 0.050 > 0.05 → stderr `warning:`，**退出码仍为 0**，数据照常落盘 |
| 同上，规模放大 | 同 rules，5000 行 | 实际 0.506 / 0.494，无告警 |
| 数值键不需要引号 | `values: {9: 0.5, 10: 0.5}` | 解析通过（此前会因键是整数而整份 rules 解析失败） |

**#68 copula_conditional 验收（同一容器，`events` 模型，seed 42，500 行）**

| 需求 | 检查 | 观察结果 |
|---|---|---|
| 条件采样数学 | `sample_with_fixed_z_rows`：一半行 pin `z=1`、一半 `z=-1`，相关系数 0.8 | 条件均值 0.81 / -0.80 |
| 区间落点 | `occurred_at.fixed_range` = [2026-01-05, 2026-01-10]，`mode: copula_conditional` | 500/500 落在区间内（min 01-05 00:04，max 01-09 23:39） |
| **相关性保留**（issue 的反超点） | 同一模型不设规则做基线对比 | 基线：日期跨 01-02–03-17，`event_id` 均值 10.86 / 20 个不同值；条件化后：均值 **4.86** / 15 个不同值——`event_id` 跟随被固定的日期下移，说明是**条件分布**而非独立重采样 |
| 其他列不受损 | 条件化生成里的 `amount` | 380 个不同值，未被固定列冻结 |
| 确定性 | 同 seed 两次生成 | 测试内逐字节一致 |
| 无效配置 | categorical 列 + `copula_conditional` / 区间在训练分布外 / 无 pin / 与 `values` 并用 | 均 fail-fast 且错误信息含表名+列名 |

**#69 挖掘质量修复（MySQL 验证发现）**：对 20 行表，`id` 这类唯一列低于 50 级上限，会为每个取值产出一条「规则」（171 条候选里绝大多数是 `trades.id=1 => trades.note='a'`）。现增加「唯一值占非 NULL 行数过半即视为标识符」的守卫，同一 fixture 降到 7 条且全部是真实规则（`bs='1' => yhs='0.00'`，confidence 1.0 / support 0.75）。


### 环境限制（未验证项）

- `#69` 的挖掘与 `#76-E` 的隐式推断只在**单元测试**层面验证（纯函数 + 合成行）；它们的 CLI 路径需要真实数据库采样，写本文时本机无 MySQL，`cargo test --all --features integration` 因 `127.0.0.1:3306 connection refused` 未能执行。（2026-09-15 第三轮：本机 MySQL 8.4.10 可用，`--features integration` 全绿 908 项，本轮修复均已按 §9.2 在真实数据库上复现。）
- Oracle / GaussDB / DuckDB 相关路径与本次改动无关，未跑。


---

## 9. PR #81 评审修复（2026-09-15，第三轮）

源码评审（`gh api repos/c2j/hepta-dbcli/pulls/81/comments`）给出 8 条内联意见：4 条 `[bug]`、4 条
`[suggestion]`。逐条独立复现后确认 7 条成立、1 条部分成立，全部按 TDD 修复；另有 1 条相邻缺陷
在复现过程中被证伪（见下）。每条都先在真实 MySQL 上复现，再改代码。

### 9.1 行为决策（新增/收紧，需与 UserGuide 同步）

| # | 决策 | 理由 |
|---|---|---|
| 1 | `derive` 的源列为 NULL 时，目标列写成 NULL 并继续生成 | §1 阶段 6 无条件重算目标列，三值逻辑下 `price * qty` 遇 NULL 即 NULL；除零/类型错误仍是硬错误（那是配置写错，不是 NULL 输入） |
| 2 | 修复循环里「本轮已被兄弟分支改写」与「已命中任一分支」的行，其他分支不得再改写 | `set` 只能让行**命中**，改写这两类行只会破坏兄弟分支已达成的覆盖 |
| 3 | 日期时间列的 `fixed_range` 端点按**时刻**（epoch 秒）比较，只写到日期的端点 = 当天 00:00:00 | 与 `copula_conditional` 用同一域；文本比较会漏判（`01/02/2026 < 31/01/2026`）也会误杀（`2026-01-31 00:00:00 > 2026-01-31`） |
| 4 | 端点/`fixed` 字面量先按列的 `datetime_format` 解析，失败再按通用 ISO 形状解析；纯数字端点当 epoch 秒 | 文档示例 `["2026-01-01","2026-01-31"]` 在两种 `mode` 下都必须能用 |
| 5 | V2/V4 的 `fixed`/`values` 扩到 `fixed_range` | 阶段 4 覆盖整列，`null_rate` 无意义；`generate` 本来就拒绝这三个字段，两层必须一致 |
| 6 | V11 可判定部分（FK 列 / 被引用父键 / derive 目标 / 被 pin 的列）移到 `validate()` 加载期 | 这些关系 YAML 里就有；未知列仍只能在生成期判（`table.columns` 只记覆盖项） |
| 7 | 算术溢出返回 `ExprError::Overflow`，不 panic | `rust_decimal` 的 `+ - *` 在溢出时 panic；文档承诺「不会 panic」。一元负号只翻符号位，无需 checked |

### 9.2 复现证据（真实 CLI + MySQL 8.4.10，`rev81` 库）

预修复二进制在 `git worktree add $SCRATCH/prefix 57f5ce5` 单独构建，与修复后二进制跑同一份
模型与 rules。

| 场景 | 修复前 | 修复后 |
|---|---|---|
| `derive: total = price * qty`，`price` 有 NULL | `error: ... expression evaluated to NULL (value left unchanged for this row)`，exit 1 | 200 行全部生成，31 行 price NULL ↔ 31 行 total NULL，169 行非 NULL 全部满足 `total = price*qty`，exit 0 |
| 两分支改写同一列（A 0.3 + B 0.7） | `a` Warn 0.272 / 10 轮 / 改写 364 行；`b` Pass 0.700 / 10 轮 / 改写 639 行 | 两个分支各 1 轮全部 Pass：0.300 / 0.700，改写 35 + 350 行 |
| `copula_conditional` + `fixed_range: ["2026-01-10","2026-01-20"]` | `error: ... cannot read '2026-01-10' as a datetime with format '%Y-%m-%d %H:%M:%S'` | 300/300 落在区间内（min 01-10 01:12，max 01-19 22:32），rejection 模式同样 300/300 |
| `derive: product = a * b`，`a`/`b` ≈ 1e20 | **panic** `Multiplication overflowed`（rust_decimal arithmetic_impls.rs:232），exit **101** | `error: table 'big_numbers' derive 'product': arithmetic overflow in expression (operator '*')`，exit 1 |
| `fixed_range` + `null_rate: 0.2` | 生成成功但可空列 0% NULL（静默回退） | 加载期报错：`'fixed'/'values'/'fixed_range' cannot be combined with null_rate > 0 (got 0.2)`，exit 1 |
| 分支 `repair.set` 写 FK 列 / derive 目标 / 被 pin 的列 | 生成期报错 | 加载期报错（错误信息含表名、分支 id、列名） |

SQL 往返（`-f sql` → 真实 MySQL）复核 `derive`：200 行全部插入成功，
`SUM(price IS NOT NULL AND total <> price*qty) = 0`、`SUM(price IS NULL AND total IS NOT NULL) = 0`。

### 9.3 被证伪的意见

- 「stalled 修复回滚后会留下陈旧的 derive 值」：`backup` 保存的是**整行**快照，回滚同时撤销
  `derive` 的写入，不存在陈旧值。已补一条特征测试锁定该不变量（若日后改成只快照 `set` 的单元
  格，测试会失败）。
- 「`!` 的 lexer 报错文案不应暗示用户想写 `!=`」：语法是冻结的 `unary := "-" unary | primary`，
  文案 `unexpected \`!\`; use \`!=\` for inequality` 是正确提示，保留；只改文档。

### 9.4 相邻发现（未修，待决策）

生成 `line_items`（12 行训练样本，`id` 为主键）时，`id` 因低基数被拟合为**分类**边际，
200 行里只有 12 个不同 id → 导出 SQL 插入到有主键的表会 `Duplicate entry`。修复前二进制
（`57f5ce5`）行为相同（12/200），与本次评审无关，属既有设计：只有被其他表 `references` 的列
才强制唯一。需要产品决策（例如对 `pk` 列强制唯一/改用序列），未擅自改动。
