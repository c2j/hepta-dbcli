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
- 比较两侧类型不匹配（字符串 vs 数值）是错误。

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
| V2 | `fixed`/`values` 与 `null_rate > 0` 同时出现 | 互斥 | #76-AC4 |
| V3 | `fixed`/`values` 的列是被其他表 `references` 的父键 | 唯一性数学不可达 | #76-AC4 |
| V4 | `fixed`/`values` 的列同时是 relationship 的 `pk`（FK 子列） | 引用完整性 | #68 冲突校验 |
| V5 | `values` 权重和偏离 1.0 或存在 ≤ 0 权重 | 概率非法 | #76-A |
| V6 | `fixed_range` 的 low > high，或列类型不是可比较类型 | 区间非法 | #68 |
| V7 | `derive[].column` 引用不存在的列 / 引用了被 fixed 或被 `set` 写的列 | 优先级矛盾 | #70 |
| V8 | derive 链成环 | 无法求值 | #70 |
| V9 | `branches[].predicate` 语法错误 / 引用未知列 | 加载期 fail-fast | #70-AC2 |
| V10 | 表达式含白名单外节点（函数调用、属性访问、下标） | 安全 | #70-AC2 |
| V11 | 修复循环目标列落在可翻列白名单外 | 引用完整性 | 本文 §1 |

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
