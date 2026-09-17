# naivediff 实施计划(issue #87)

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.
> 仓库 TDD 规范见 AGENTS.md:每个 Task 都是 Red(失败测试)→ Green(最小实现)→ Refactor;
> 每个 Task 结束跑 `cargo test --all <filter>` 并 commit;收尾跑全量门禁 fmt → clippy → test。

**Goal:** 新增 delta-diff 显式策略 `naivediff`:每侧一次 `SELECT … WHERE …` 全扫描(裸列 ORDER BY 主键、无 NLSSORT/COLLATE、无 LIMIT),客户端完成按键归并,使 DAT_FUND_CJQS 一类「单日数万行、复合 VARCHAR 主键、经常零差/少量差」的日对账达到秒级。

**Architecture:** 新增 `NaiveDiffer`(实现既有 `DiffStrategy` trait)+ 新方言方法 `render_scan_sql`(5 处实现,oracle/oracle_native 强制同步)。有键路径用 **规范化指纹 HashMap join**(规避跨库 collation 排序不一致),无键路径用 **规范化全行指纹多重集差**。不进 `auto`;COUNT 门控行数硬顶;快照/COMMIT 模式与 keyeddiff 相同。

**Tech Stack:** 纯 Rust,无新依赖。复用 `rust_decimal`(已有)、`serde_json::Value`(Hash/Eq)、rowdiff 比较原语。

**Issue:** https://github.com/c2j/hepta-dbcli/issues/87 · 分支 `feat/issue-87`

---

## 0. 已探明的代码事实(实施者必读)

| 事实 | 位置 |
|---|---|
| `Strategy` 枚举 + `Display` | `dbcli/src/delta_diff/cmd.rs:14-36` |
| CLI `--strategy` 参数 | `cmd.rs:138-140` |
| 路由 match(显式臂 + auto 臂) | `dbcli/src/delta_diff/engine.rs:71-189` |
| MCP 策略解析 | `dbcli/src/server.rs:1134-1146`;MCP options 构造 `:1198-1234`(fetch_all_threshold 硬编码 4096) |
| `DiffStrategy` trait | `dbcli/src/delta_diff/strategy.rs:96-107` |
| `DiffContext` 字段(新字段需同步全部测试构造器) | `strategy.rs:19-54`;测试构造器:`strategy.rs:154`、`keyed_diff.rs:585`、`bucket_diff.rs` 测试、`api.rs` 测试 |
| keyeddiff 全流程模板(COUNT→空侧→FETCH_ALL→checksum→assemble) | `dbcli/src/delta_diff/keyed_diff.rs:81-243`;快照开/COMMIT `:47-62`;`assemble` `:463-545` |
| **NLSSORT/COLLATE 根源(禁止复用)** | `dbcli/src/backend/mod.rs:427-438 key_sort_expr`;ORDER BY `:507-514`;谓词 `:457-483` |
| `KeysetPageSpec` / 引号助手 / `NULL_SENTINEL` | `backend/mod.rs:329-351` / `:353-409` / `:294` |
| 各方言 `render_keyset_page_sql`(照抄结构,去掉 keyset 谓词/collation/limit) | mysql `dialect.rs:267-295`、oracle `dialect.rs:358-393`(FETCH_FIRST/ROWNUM 分支不要)、oracle_native `dialect.rs:358-393`、gaussdb `mod.rs:300-328`、duckdb `dialect.rs:278-303` |
| COUNT SQL + 解析(复用,改 pub(crate)) | `keyed_diff.rs:279-330` |
| 数字比较原语 `cmp_value`/`values_equal` | `rowdiff.rs:216-253`(`values_equal` 已 pub(crate)) |
| `row_values_equal`/`diff_key`/`diff_row_n`(私有,需改 pub(crate)) | `rowdiff.rs:255-281` |
| 列序与数字旗标(键在前 + norm_specs) | `keyed_diff.rs:378-430 full_row_spec/full_row_numeric_flags`(改 pub(crate) 复用) |
| 报告结构/`stamp_columns_from_plan` | `report.rs:86-168` |
| 假连接测试模式(`impl DbConn` 返回预置行) | `rowdiff.rs:352+ struct PagedConn` |
| DuckDB 嵌入式连接(测试用) | `dbcli/tests/regress_duckdb.rs:19-41`(`duckdb://:memory:`) |
| dry-run key domain 特判 | `mod.rs:819-828 format_key_domain_line` |
| `--where` 注入契约 | `side_filter`(`strategy.rs:111-144`)原样返回,渲染侧统一 `({f})` 包裹;仅 `;` 守卫(CLI `cmd.rs:265-272`,MCP `server.rs:1201-1205`) |
| 文档更新点 | `README.md`(strategy 表)、`UserGuide.md`(`--fetch-all-threshold` 附近,~865 行) |

**方言数字文本化差异(假脏根源,naivediff 客户端比较天然规避):** Oracle `TO_CHAR` 定标度掩码(`oracle/dialect.rs:235-245`)、GaussDB `::text`、MySQL `CAST AS CHAR`。naivediff 对数字列统一客户端 `Decimal` 比较,不依赖库端文本。

---

## 1. 设计决策(D1-D8,Momus 审核对象)

**D1 — 归并用「规范化指纹 HashMap join」,不用双流 sorted-merge。**
理由:`query()` 本就全量物化(`QueryResult.rows: Vec<Vec<Value>>`),HashMap join 不增内存;裸 `ORDER BY` 去掉 collation 包装后,两侧库对同一批 VARCHAR 键的排序可能不一致(尾随空格、大小写),sorted-merge 会错位产生假阳性。SQL 仍保留裸 `ORDER BY 主键`(满足索引友好 + issue 必须项),但**正确性不依赖库端顺序**。

**D2 — 规范化指纹 = `Vec<Fingerprint>`,`Fingerprint ∈ {Null, Num(Decimal), Text(String)}`。**
- `Num(Decimal)`:数字旗标列(`numeric_value_flags_for`)按 `Decimal::from_str(value_text)` 归一,解决跨库 `1` vs `1.0` vs `"1"` 假差(rust_decimal 的 `PartialEq/Eq/Hash` 数值等价且一致)。
- `Null`/`Text` 严格区分 → 杜绝 Java §6.1「NULL 与 '' 同指纹」缺陷。
- 解析失败的数字文本回退 `Text`。
- 同一 `Fingerprint` 类型同时用于有键 join 的键前缀与无键多重集的全行。

**D3 — 有键路径多行键(multiset join):`HashMap<Vec<Fingerprint>, Vec<usize>>`,按下标配对,min(左右数) 配对比较,余量出 Missing*。** 主键应唯一;若因显式 `--key` 选错出现重复键,结果仍正确(退化为多重集语义),配合既有 `push_key_shape_warnings` 告警。

**D4 — 行数硬顶:默认拒绝(refuse),`--naive-max-rows` 默认 200000,0 = 不设顶。** COUNT 后判定 `max(left,right) > cap` → 返回 Err,文案提示改用 `--strategy keyeddiff`。选「拒绝」而非「强告警」:200k×53 列物化后内存可达 GB 量级,告警后继续跑的风险不对称。MCP 不暴露新参数,`build_mcp_diff_options` 硬编码 200000(与现有 4096/65536 硬编码一致;issue 仅要求 MCP 枚举加 naivediff)。

**D5 — 不接 recheck。** 差异直接来自实值比较(ground truth);keyeddiff 同样不接(`recheck.rs` 仅 hashdiff/iblt 使用)。

**D6 — `auto` 永不选 naivediff。** engine 只加显式臂;auto 的复合/字符串键继续 keyeddiff(保住零差 ~6s checksum)。无键表显式 `--strategy naivediff` 走无键模式,**不回退 bucketdiff**(无键模式本身即其超集:带行 payload 的多重集差)。

**D7 — 报告对齐:** `strategy: "naivediff"`,`hash_algorithm: "none"`(无库端哈希),单 shard `shard_id: "naive-all"`,`RowPayload::Columns`,`stamp_columns_from_plan` 照常。dry-run `format_key_domain_line` 对 naivediff 显示 not applicable。

**D8 — NULL 键天然支持。** 无 keyset 谓词 → NULL 键行不丢;指纹 `Null` 变体可正确配对。报告 `DiffRow.key` 用原始 JSON(`diff_key`),空值语义与 keyeddiff 一致。

**明确不做(另开 issue,与 issue #87 一致):** 流式 `query_cursor`、`auto` 按 COUNT 切 naivediff、改 keyeddiff 默认行为、1:1 复刻 Java(全列 ORDER BY、每万行 ping、Excel/WebSocket)。

---

## Task 1: `Strategy::Naivediff` 枚举 + CLI 解析

**Files:**
- Modify: `dbcli/src/delta_diff/cmd.rs:14-36`(枚举/Display)、`:138-140`(帮助文本)
- Test: `cmd.rs` 既有 `#[cfg(test)]` 内(策略解析测试附近,~424-439/591-606 区)

**Step 1 (Red):** 在 `cmd.rs` 测试模块加:

```rust
#[test]
fn strategy_naivediff_parses_and_displays() {
    let s: Strategy = clap::ValueEnum::from_str("naivediff", true).unwrap_or_else(|_| {
        // clap::ValueEnum 无 direct from_str 时改走 Args 解析路径:
        // DeltaDiffArgs::parse_from(["delta-diff", "--left", "l", "--right", "r",
        //   "--table", "t", "--strategy", "naivediff"]).strategy
        panic!("naivediff must be a valid value");
    });
    assert_eq!(s.to_string(), "naivediff");
}
```

(若 `clap::ValueEnum::from_str` 不便直接调用,用 `DeltaDiffArgs::parse_from` 断言 `args.strategy == Strategy::Naivediff` —— 实施时择一可用者,测试语义不变。)

**Step 2:** `cargo test --all strategy_naivediff` → 预期编译失败(变体不存在)。
**Step 3 (Green):** 枚举加 `Naivediff`;`Display` 加 `Strategy::Naivediff => "naivediff"`;`--strategy` 帮助文本改为 `比对策略：auto | hashdiff | joindiff | bucketdiff | iblt | keyeddiff | naivediff`。
**Step 4:** 同过滤测试 → PASS。
**Step 5:** `git add -A && git commit -m "feat(delta-diff): add Naivediff strategy variant (#87)"`

---

## Task 2: MCP 策略解析

**Files:**
- Modify: `dbcli/src/server.rs:1134-1146`
- Test: `server.rs:1236-1253 delta_diff_strategy_tests`

**Step 1 (Red):**

```rust
#[test]
fn parses_naivediff() {
    assert_eq!(
        parse_delta_diff_strategy(Some("naivediff")).unwrap(),
        Some(Strategy::Naivediff)
    );
}
```

**Step 2:** `cargo test --all parses_naivediff` → FAIL。
**Step 3 (Green):** match 加 `Some("naivediff") => Ok(Some(crate::delta_diff::cmd::Strategy::Naivediff)),`
**Step 4:** PASS。
**Step 5:** `git commit -am "feat(delta-diff): accept naivediff in MCP strategy parsing (#87)"`

---

## Task 3: engine 显式路由臂(不进 auto)

**Files:**
- Modify: `dbcli/src/delta_diff/engine.rs:14`(use 加 `naive_diff`)、`:71-189`(match 加臂)
- Modify: `dbcli/src/delta_diff/mod.rs`(模块表加 `pub(crate) mod naive_diff;` —— 此时先建空文件+骨架,Task 6 填实现)
- Test: `engine.rs` 测试模块(`:281+`,仿 `plan()` helper)

**Step 1 (Red):**

```rust
#[test]
fn explicit_naivediff_routes_composite_key() {
    let r = route_impl(&plan(vec!["k1", "k2"], "varchar"), &plan(vec!["k1", "k2"], "varchar"),
        "oracle://a", "oracle://b", Some(Strategy::Naivediff)).unwrap();
    assert_eq!(r.strategy.name(), "naivediff");
}

#[test]
fn explicit_naivediff_keyless_keeps_naivediff() {
    let r = route_impl(&plan(vec![], "int"), &plan(vec![], "int"),
        "mysql://a", "mysql://b", Some(Strategy::Naivediff)).unwrap();
    assert_eq!(r.strategy.name(), "naivediff");
    assert!(r.warnings.iter().any(|w| w.contains("keyless")), "must warn keyless semantics");
}

#[test]
fn auto_never_picks_naivediff() {
    let r = route_impl(&plan(vec!["k1", "k2"], "varchar"), &plan(vec!["k1", "k2"], "varchar"),
        "oracle://a", "oracle://b", None).unwrap();
    assert_eq!(r.strategy.name(), "keyeddiff");
}
```

**Step 2:** `cargo test --all naivediff` → FAIL。
**Step 3 (Green):** `naive_diff.rs` 先落骨架(`pub(crate) struct NaiveDiffer;` + `impl DiffStrategy` name 返回 `"naivediff"`、`diff` 暂 `todo!()`——仅作占位,Task 6 必须替换);engine 加:

```rust
Strategy::Naivediff => {
    if key_columns.is_empty() {
        warnings.push(
            "note: keyless naivediff reports row-content multiset differences only \
             (source-only / target-only rows, no Modified)"
                .to_string(),
        );
    }
    Box::new(naive_diff::NaiveDiffer)
}
```

(有键无告警;`finish_route` 由 match 尾部统一走。)
**Step 4:** PASS。
**Step 5:** `git commit -am "feat(delta-diff): route explicit naivediff in engine (#87)"`

---

## Task 4: 方言 `render_scan_sql`(5 处实现,oracle_native 同步)

**Files:**
- Modify: `dbcli/src/backend/mod.rs`(`ScanSqlSpec` 定义放 `:351` 之后;trait 方法声明放 `render_keyset_page_sql` 声明 `:236` 之后)
- Modify: `mysql/dialect.rs`、`oracle/dialect.rs`、`oracle_native/dialect.rs`、`gaussdb/mod.rs`、`duckdb/dialect.rs`
- Test: 各文件 `#[cfg(test)] mod tests`

**Spec 定义:**

```rust
/// One full-table scan for the naivediff strategy (issue #87):
/// single statement, no pagination, NO collation wrappers on ORDER BY,
/// no row limit. `columns` 是已渲染的 SELECT 列表(裸键列/normalize 表达式);
/// `order_by` 是已渲染的裸键列列表(空 = 无 ORDER BY,无键表)。
#[derive(Debug, Clone)]
pub struct ScanSqlSpec {
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<String>,
    pub order_by: Vec<String>,
    pub filter: Option<String>,
    /// Oracle AS OF SCN anchor (snapshot mode); other dialects ignore.
    pub scn: Option<u64>,
}
```

**Trait 声明:**

```rust
/// Render one full-scan row fetch for naivediff (issue #87):
/// `SELECT <cols> FROM <table>[ AS OF SCN n][ WHERE (<filter>)][ ORDER BY <order_by>]`
/// — single statement, no LIMIT/FETCH FIRST/ROWNUM, and ORDER BY must use the
/// RAW key columns (never NLSSORT/COLLATE — that is the 34s-vs-6s regression).
fn render_scan_sql(&self, spec: &ScanSqlSpec) -> String;
```

**Step 1 (Red,各方言一个测试;Oracle 为例):**

```rust
#[test]
fn scan_sql_orders_by_raw_key_without_nlssort() {
    let d = OracleDialect::new();
    let spec = ScanSqlSpec {
        schema: Some("SCOTT".into()),
        table: "T".into(),
        columns: vec![r#""K1""#.into(), r#""K2""#.into(), d.normalize_expr(&col("AMT", "NUMBER", false)).unwrap()],
        order_by: vec![r#""K1""#.into(), r#""K2""#.into()],
        filter: Some("BCRQ='20251215'".into()),
        scn: Some(424242),
    };
    let sql = d.render_scan_sql(&spec);
    assert!(sql.contains("ORDER BY \"K1\", \"K2\""), "{sql}");
    assert!(!sql.contains("NLSSORT"), "{sql}");
    assert!(!sql.contains("FETCH FIRST") && !sql.contains("ROWNUM"), "{sql}");
    assert!(sql.contains("AS OF SCN 424242"), "{sql}");
    assert!(sql.contains("WHERE (BCRQ='20251215')"), "{sql}");
}

#[test]
fn scan_sql_omits_order_by_when_keyless() { /* order_by: vec![] → !sql.contains("ORDER BY") */ }
```

MySQL 断言反引号 + `!contains("COLLATE")`;GaussDB 断言 `!contains("COLLATE \"C\"")`;DuckDB 同构;**oracle_native 加一个 parity 测试:同一 spec 两方言输出逐字节相等**(`docs/…scan-once…md:119` 同步要求)。

**Step 2:** 各方言测试编译失败(方法不存在)→ FAIL。
**Step 3 (Green):** 每个 impl 照抄本方言 `render_keyset_page_sql` 的引号/表渲染,删去 keyset 谓词、`keyset_order_by`、limit 分支:

```rust
// oracle/dialect.rs(oracle_native 同文)
fn render_scan_sql(&self, spec: &ScanSqlSpec) -> String {
    let mut table = quoted_table(&spec.schema, &spec.table);
    if let Some(scn) = spec.scn {
        table.push_str(&format!(" AS OF SCN {scn}"));
    }
    let where_clause = spec
        .filter
        .as_ref()
        .map(|f| format!("\nWHERE ({f})"))
        .unwrap_or_default();
    let order_by = if spec.order_by.is_empty() {
        String::new()
    } else {
        format!("\nORDER BY {}", spec.order_by.join(", "))
    };
    format!("SELECT {}\nFROM {table}{where_clause}{order_by}", spec.columns.join(", "))
}
```

MySQL/GaussDB/DuckDB:`scn` 忽略;表渲染分别用各自 keyset impl 的现有写法(反引号/`quote_table_scheme`)。
**Step 4:** `cargo test --all scan_sql` → PASS。
**Step 5:** `git commit -am "feat(backend): dialect render_scan_sql for naivediff full scans (#87)"`

---

## Task 5: 比较原语解私有(纯重构,先行完成以便复用)

**Files:**
- Modify: `dbcli/src/delta_diff/rowdiff.rs:255,265,273`(`row_values_equal`/`diff_key`/`diff_row_n` → `pub(crate)`)
- Modify: `dbcli/src/delta_diff/keyed_diff.rs:279-330,293-308,378-430`(`render_count_sql`/`parse_count`/`parse_count_value`/`fetch_count_mismatch_warning`/`full_row_spec`? 否——`full_row_spec` 依赖 KeysetPageSpec,不搬;只 `pub(crate)` `render_count_sql`、`parse_count`、`full_row_numeric_flags:418-430`)

**Step 1:** 先加一个使用新可见性的空转测试(Red 无从谈起——可见性改动用「编译即测试」):在 `naive_diff.rs` 骨架里写一个引用 `rowdiff::row_values_equal` 的 `#[test]` 占位(断言 `row_values_equal(&[json!(1)], &[json!(1)], &[true])`),先 FAIL(私有)。
**Step 2 (Green):** 改可见性;**不改任何函数体**(Refactor 纪律:纯重命名级变更)。
**Step 3:** `cargo test --all` 全量(此时不新增失败)→ PASS。
**Step 4:** `git commit -am "refactor(delta-diff): share row/compare primitives for naivediff (#87)"`

---

## Task 6: NaiveDiffer 有键路径(指纹 HashMap join)

**Files:**
- Modify: `dbcli/src/delta_diff/naive_diff.rs`(主体)
- Modify: `dbcli/src/delta_diff/strategy.rs`(DiffContext 加字段,见 Task 8;本 Task 先不动)
- Test: `naive_diff.rs` `#[cfg(test)]`(假行向量,不需要连接)

**核心类型与纯函数(先测后写):**

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Fingerprint {
    Null,
    Num(Decimal),
    Text(String),
}

fn fingerprint_cell(v: &Value, numeric: bool) -> Fingerprint {
    if v.is_null() {
        return Fingerprint::Null;
    }
    if numeric {
        let text = rowdiff::value_text(v); // value_text 也改 pub(crate)(Task 5 顺带)
        if let Ok(d) = Decimal::from_str(text.trim()) {
            return Fingerprint::Num(d);
        }
        return Fingerprint::Text(text);
    }
    Fingerprint::Text(rowdiff::value_text(v))
}

fn fingerprint_row(row: &[Value], flags: &[bool]) -> Vec<Fingerprint> {
    row.iter()
        .enumerate()
        .map(|(i, v)| fingerprint_cell(v, flags.get(i).copied().unwrap_or(false)))
        .collect()
}
```

**有键归并(纯函数,可单测):**

```rust
/// 键前缀(前 arity 列)→ 左侧行下标列表;右侧行顺序遍历配对。
/// 配对成功且行值不等 → Modified;右多出 → MissingRight;左多出 → MissingLeft。
/// DiffRow.key 用左侧原始键 JSON(diff_key),confirmed: true。
fn keyed_merge(
    lrows: &[Vec<Value>],
    rrows: &[Vec<Value>],
    arity: usize,
    lnum: &[bool],
    rnum: &[bool],
) -> Vec<DiffRow>
```

实现要点:左映射 `HashMap<Vec<Fingerprint>, Vec<usize>>`;右行出队左下标(`Vec<usize>` 用 swap 移除);`row_values_equal(&lrow[arity..], &rrow[arity..], 非键位旗标)` 决定 Modified——**注意旗标按全行列序(键在前),比较段要偏移 arity**。产出顺序:右扫描序(MissingRight/Modified 在前),随后左原序补 MissingLeft。

**Step 1 (Red) 行为测试(命名描述行为,≥5 条):**

```rust
#[test]
fn keyed_merge_reports_modified_when_nonkey_values_differ()          // 同键 CJJE 变了 → Modified,不带「各有一条」
#[test]
fn keyed_merge_treats_numeric_one_and_text_one_as_same_key()         // 1 vs "1" 同键(D2)
#[test]
fn keyed_merge_marks_rows_missing_on_right_when_absent()             // 仅左有 → MissingRight
#[test]
fn keyed_merge_marks_rows_missing_on_left_when_absent()              // 仅右有 → MissingLeft
#[test]
fn keyed_merge_pairs_duplicate_keys_without_false_modified()         // 重复键 multiset 配对
#[test]
fn keyed_merge_null_key_pairs_with_null_key()                        // NULL 键不丢(D8)
#[test]
fn keyed_merge_compares_decimals_exactly_across_scale()              // 1.50 vs 1.5 非键列相等
```

**Step 2:** `cargo test --all keyed_merge_ 2>&1 | tail -5` → 预期 **编译失败**(`keyed_merge`/`Fingerprint` 不存在),即合法 Red。
**Step 3 (Green):** 实现上述类型与 `keyed_merge`。
**Step 4:** `cargo test --all keyed_merge_ 2>&1 | tail -5` → 预期 7 条测试全 PASS(`test result: ok`,failed=0)。
**Step 5:** `git commit -am "feat(delta-diff): naivediff keyed merge with canonical fingerprints (#87)"`

---

## Task 7: NaiveDiffer 无键路径(规范化多重集差)

**Files / Test:** 同 Task 6 文件。

**核心函数:**

```rust
/// 全行指纹多重集:min(lc,rc) 抵消;左余量 → MissingRight(附左行),
/// 右余量 → MissingLeft(附右行)。产出顺序:右扫描序 MissingLeft,随后左原序 MissingRight。
fn keyless_merge(lrows: &[Vec<Value>], rrows: &[Vec<Value>], lnum: &[bool], rnum: &[bool]) -> Vec<DiffRow>
```

**Step 1 (Red):**

```rust
#[test]
fn keyless_merge_reports_only_missing_statuses()                     // 无 Modified
#[test]
fn keyless_merge_multiset_cancel_counts()                            // 左2右1 → 1 MissingRight
#[test]
fn keyless_merge_null_and_empty_string_are_distinct_fingerprints()   // Java §6.1 缺陷回归
#[test]
fn keyless_merge_numeric_equivalence_across_scales()                 // 1 vs 1.0 vs "1.0" 抵消
```

**Step 2:** `cargo test --all keyless_merge 2>&1 | tail -5` → 预期 **编译失败**(`keyless_merge` 不存在),合法 Red。
**Step 3 (Green):** 实现(复用 `fingerprint_row` + 两个 `HashMap<Vec<Fingerprint>, Vec<usize>>`)。
**Step 4:** `cargo test --all keyless_merge 2>&1 | tail -5` → 预期 4 条测试全 PASS;再跑 `cargo test --all naive 2>&1 | tail -5` 确认 Task 6 无回归。
**Step 5:** `git commit -am "feat(delta-diff): naivediff keyless multiset merge (#87)"`

---

## Task 8: DiffContext 加 `naive_max_rows` + NaiveDiffer 主流程接线

**Files:**
- Modify: `dbcli/src/delta_diff/strategy.rs:19-54`(字段 `pub(crate) naive_max_rows: u64,`)+ `:154` 测试构造器
- Modify: `dbcli/src/delta_diff/cmd.rs`(`--naive-max-rows`,default 200000,`/// Naivediff: refuse when max(COUNT) exceeds this (0 = unlimited)`)
- Modify: `dbcli/src/delta_diff/mod.rs:477-514`(CLI ctx 赋值 `naive_max_rows: args.naive_max_rows`)
- Modify: `dbcli/src/delta_diff/api.rs:37-56`(DiffOptions 字段)+ `:119-157`(ctx 赋值)
- Modify: `dbcli/src/server.rs:1215-1233`(`naive_max_rows: 200_000` 硬编码;注意是 `src/server.rs`,不在 delta_diff/ 下)
- Modify: 其余 DiffContext 测试构造器(`keyed_diff.rs:585`、`bucket_diff.rs` 测试、`api.rs` 测试等——编译错误逐一补 `naive_max_rows: 4096` 之类默认值)
- Modify: `dbcli/src/delta_diff/naive_diff.rs`(主流程)
- Test: `naive_diff.rs`(假 `DbConn`,仿 `rowdiff.rs:352 PagedConn`)+ `cmd.rs`(默认值测试)

**NaiveDiffer::diff 主流程(镜像 keyed_diff.rs:31-77):**

```rust
async fn diff(&self, left, right, ctx) -> Result<DiffReport, DbError> {
    let started = Utc::now();
    let mut queries = 0u64;
    ctx.vlog(format!("[delta-diff] strategy=naivediff consistency={} keys={} naive_max_rows={}",
        ctx.consistency.as_str(), ctx.key_columns.join(","), ctx.naive_max_rows));
    if ctx.consistency == ConsistencyMode::Snapshot {
        open_snapshot(left, ctx.verbose).await?;
        open_snapshot(right, ctx.verbose).await?;
        let _ = ctx.scns.set((capture_scn(left, ctx.verbose).await?, capture_scn(right, ctx.verbose).await?));
    }
    let result = self.diff_inner(left, right, ctx, &mut queries).await;
    if ctx.consistency == ConsistencyMode::Snapshot {
        ctx.vlog("[sql] COMMIT");
        let _ = left.query_drop("COMMIT").await;
        let _ = right.query_drop("COMMIT").await;
    }
    let (rows, ltotal, rtotal, extra) = result?;
    let mut report = assemble(ctx, rows, ltotal, rtotal, extra);   // 镜像 keyed_diff::assemble
    report.started_at = started;
    report.finished_at = Utc::now();
    report.perf.queries_total = queries;
    Ok(report)
}
```

**diff_inner:**

1. COUNT 两侧 `tokio::join!`(`keyed_diff::render_count_sql` + `parse_count`,queries += 2,`ctx.vlog` 两侧 SQL)。
2. **硬顶**:`ctx.naive_max_rows > 0 && left_total.max(right_total) > ctx.naive_max_rows`
   → `return Err(DbError::query(format!(
      "naivediff row cap exceeded: left={left_total} right={right_total} cap={}; \
       use --strategy keyeddiff for larger filtered sets", ctx.naive_max_rows)))`。
3. 两侧皆 0 → 空。单侧 0 → 只扫非空侧,merge 与空向量(自然产出全 Missing*)。
4. 扫描 spec(有键:`columns` = 裸引号键列 + 非键 `normalize_expr`;`order_by` = 裸引号键列。无键:`columns` = 全部 `normalize_expr`,`order_by` = 空)。两侧 `tokio::join!(left.query(&lsql), right.query(&rsql))`(queries += 2,verbose 打 SQL)。
5. 数目核对告警(naive 文案,~10 行小函数,不复用 keyeddiff 的 keyset 措辞):
   `naivediff scan count mismatch: left count={X} fetched={Y}, right count={Z} fetched={W}; concurrent writes may have changed rows`。
6. 列序旗标:有键 = `keyed_diff::full_row_numeric_flags`(Task 5 已 pub(crate));无键 = `numeric_value_flags_for(全部 norm_specs 名)`。
7. merge:有键 → `keyed_merge(rows_l, rows_r, arity, &lnum, &rnum)`;无键 → `keyless_merge`。

**assemble 差异(相对 keyed_diff::assemble):** `strategy: "naivediff"`、`hash_algorithm: "none"`、shard `shard_id: "naive-all"`、其余逐字段同(`RowPayload::Columns`、`stamp_columns_from_plan`、warnings 链)。

**Step 1 (Red):**
- `cmd.rs`:`#[test] fn naive_max_rows_defaults_to_200k()`
- `naive_diff.rs`(假连接,预置左右 `QueryResult`,依次响应 COUNT 与 scan;仿 PagedConn 按 SQL 内容分派):
  - `naive_diff_refuses_over_cap` → Err 含 "row cap exceeded" 与 "keyeddiff"
  - `naive_diff_zero_diff_single_scan` → 两侧同数据:空 rows,`perf.queries_total == 4`(2 COUNT + 2 scan)
  - `naive_diff_reports_modified_missing_and_key_shape` → 混合差异 + `DiffRow.key` 复合键为 JSON 数组
  - `naive_diff_strategy_report_fields` → `report.strategy == "naivediff"`、`hash_algorithm == "none"`、`row_payload == Columns`
**Step 2:** FAIL(字段/流程不存在)。
**Step 3 (Green):** 按 1-7 实现;编译错误的旧测试构造器补新字段(**不得改动断言**)。
**Step 4:** `cargo test --all naive` → PASS;`cargo test --all`(构造器波及面)→ PASS。
**Step 5:** `git commit -am "feat(delta-diff): NaiveDiffer end-to-end with count gate and report (#87)"`

---

## Task 9: dry-run 呈现 + 文档

**Files:**
- Modify: `dbcli/src/delta_diff/mod.rs:819-828`(`format_key_domain_line` 加 `"naivediff" => "  key domain       : (not applicable — naivediff)"`)
- Modify: `README.md`(delta-diff strategy 表加一行:naivediff 不被 auto 选中,显式指定用于日对账)
- Modify: `UserGuide.md`(`--fetch-all-threshold` 条目旁加 `--naive-max-rows`;strategy 列表加 naivediff)
- Test: `mod.rs` dry-run 测试区

**Step 1 (Red):** 在 `mod.rs` dry-run 测试区加(测试名固定,便于命令过滤):

```rust
#[test]
fn dry_run_shows_naivediff_strategy() {
    // 仿既有 dry-run 输出断言:构造 DeltaDiffArgs { strategy: Naivediff, dry_run: true, … }
    // 断言输出串含 "strategy: naivediff" 且含 "(not applicable — naivediff)"
}
```

**Step 2:** `cargo test --all dry_run_shows_naivediff 2>&1 | tail -5` → 预期 **FAIL**(特判不存在,输出仍是通用 key domain 行)。
**Step 3 (Green):** `format_key_domain_line` 加 `"naivediff"` 特判;更新 `README.md` strategy 表(naivediff 不被 auto 选中,显式指定用于日对账)与 `UserGuide.md`(`--naive-max-rows` 条目 + strategy 列表)。
**Step 4:** `cargo test --all dry_run_shows_naivediff 2>&1 | tail -5` → PASS;再跑 `cargo test --all 2>&1 | tail -3` 确认 dry-run 既有测试无回归(全量 `test result: ok`)。
**Step 5:** `git commit -am "feat(delta-diff): dry-run display and docs for naivediff (#87)"`

---

## Task 10: DuckDB 端到端(feature-gated 单测)+ 全量门禁

**Files:**
- Modify: `dbcli/src/delta_diff/naive_diff.rs` 测试模块尾部

**Step 1 (Red):** 加 `#[cfg(all(test, feature = "duckdb"))]` 端到端测试:内存库 `DuckDbFactory.connect("duckdb://:memory:", None)` 建表灌数(复合键 `(k1 VARCHAR, k2 VARCHAR, amt DECIMAL(20,6))`,含 NULL 键行),手工构造 `DiffContext` + 真 conn 跑 `NaiveDiffer.diff`;断言零差时 `queries_total==4`、差异场景 Modified/Missing 正确、`row_values_equal` 语义下 `1.50` vs `1.5` 不报差。参考 `dbcli/tests/regress_duckdb.rs:19-41` 的连接方式。
**Step 2:** FAIL(实现 bug 则修复——注意这是唯一允许暴露实现缺陷的层;若 Red 直接 PASS,检查测试是否真的构造了差异)。
**Step 3 (Green):** 修实现直至 PASS。
**Step 4 门禁(AGENTS.md 顺序,勿跳过 clippy):**

```bash
cargo fmt --all -- --check
cargo clippy --all --all-targets          # warning 必须清零(超出 CI 标准)
cargo test --all
cargo test --all --features integration   # 无 DB 用例自跳过
cargo test --all --features duckdb        # CI duckdb job 同款
cargo test --features "duckdb,integration" --test regress_duckdb
```

**Step 5:** `git commit -am "test(delta-diff): duckdb e2e for naivediff + gates green (#87)"`

---

## 验收口径

**CI 可验(本计划 Task 1-10 全覆盖):**
- `--strategy naivediff` CLI/MCP 合法;auto 路由不变(composite → keyeddiff)
- 扫描 SQL:裸 ORDER BY 主键、无 NLSSORT/COLLATE、无 LIMIT、`({where})` 包裹、Oracle 带 AS OF SCN、oracle_native 逐字节一致
- 语义:Modified/MissingLeft/MissingRight;`1` vs `"1"` 键等价;`1.50` vs `1.5` 值等价;NULL 键/NULL vs '' 不混淆;重复键不假阳
- 硬顶:超限 Err + 提示 keyeddiff;默认 200000;0 = 不设顶
- 报告/导出/patch 与 DiffReport 契约对齐;dry-run 显示 naivediff

**人工验(--实施后单独执行,不进 CI;issue 验收口径):**

```bash
# dat_fund_cjqs_bak 式 8 万行夹具(需手工生成,仓库内无 cjqs_day 夹具):
# 零差异 ≈ 或略慢于 checksum(~6s 量级),严禁再现 34s;
# 10 条随机 UPDATE ≈ 5s 量级,明显快于 checksum 脏桶回扫(11s+)
hepta_dbcli delta-diff --left … --right … --table DAT_FUND_CJQS \
  --where "BCRQ='20251215'" --strategy naivediff --verbose
```

人工验结果回填 issue #87,不在本 PR 范围内声称达标。

## 风险与边界

| 风险 | 缓解 |
|---|---|
| 跨库 collation 排序不一致 → 归并错位 | D1:正确性不依赖库端顺序(HashMap join + 规范化指纹) |
| 跨库数字文本假差 | D2:客户端 Decimal;扫描列仍走 normalize_expr 保持列语义一致 |
| 大表内存(200k×宽列) | D4:COUNT 前置硬顶,默认拒绝;0 可显式解除 |
| 重复键(显式 --key 选错) | D3:multiset 配对不假阳 + 既有 key_shape 告警 |
| oracle_native 漂移 | Task 4 逐字节 parity 测试 |
| 构造器波及(新 DiffContext 字段) | Task 8 一次性编译修复,禁改断言 |
