# `delta-diff` 设计补充：复合主键路由修复 + scan-once 性能快路径

> **文档性质**：本文档是《delta-diff 设计文档（修订版 v2.1）》的**补充（Addendum）**，不修改 v2.1 正文。v2.1 §6.4「hashdiff/iblt 仅单列整型可二分键」仍然成立；本文档**新增策略 `keyeddiff`**，并把 keyless `bucketdiff` 的 N 次全表扫描收成每侧 1 次 `GROUP BY`。
>
> **动机（issue #35）**：14 列联合主键表上 `--key` 被 `resolve_key` 丢弃，auto 当成无主键走 bucketdiff，过滤命中 0 vs 5 行仍执行约 **68** 条 SQL / **88s**。

---

## 问题与复现

`dat_fund_cjqs`（53 列，约 450k 行，**14 列联合主键**）：

```
hepta_dbcli delta-diff --left touchigh --right dota --table dat_fund_cjqs \
  --key <14列联合主键> --where "bcrq='20260114' and scdm='001' and xwdm='59267'"
```

| 运行方式 | strategy | queries_total | 告警 |
|---|---|---|---|
| auto（自动发现 PK） | bucketdiff | **68** | `keyless table diff reports row-content multiset differences only` |
| `--key k01,…,k14` | bucketdiff | **68** | 同上 |

根因两处，均在算法/路由层：

1. **`resolve_key` 只接受单列**：`key_columns.len()==1` 才返回 `Some(String)`。14 列联合主键与 `--key` CSV 写入 `TablePlan` 后被丢弃 → auto 的 `(None, _)` 分支 → keyless bucketdiff。复合主键被误报为无主键。
2. **bucketdiff 每桶一条 checksum = 一次全表扫描**：`N = clamp(ceil(reltuples/16384), 1, 1024)`。桶谓词 `MOD(MD5(concat_ws(全列)), N) = b` 无法下推索引。`Q = 2 + 2N + 2D`（估计 + 每桶 checksum + 差异桶 multiset）。450k → N=28、D=5 → 68。估计忽略 `--where`；无空侧短路。

hashdiff/iblt 要求单列可二分整型键（v2.1 §6.4），本表进不去；joindiff 要求同连接 MySQL 且单列键。

---

## 路由表

新增策略名 **`keyeddiff`**（`KeyedDiffer`）。`--strategy keyeddiff` 合法。hashdiff/iblt/joindiff **不**扩展复合键。

`resolve_key` 改为返回 `Vec<String>`：两侧 `key_columns` 非空且相等则原样返回，否则空。`Route` / `DiffContext` 增加 `key_columns`；保留 `key_column = key_columns.first()` 供旧策略使用。

**auto：**

| 条件 | 策略 |
|---|---|
| 无键（两侧 key 皆空） | `bucketdiff` + keyless 提示 |
| 有键、单列整型、同连接 MySQL | `joindiff`（不变） |
| 有键、单列整型、其余 | `iblt`（不变） |
| 有键、不可二分（复合 **或** 非整型） | **`keyeddiff`** |

**显式回退：**

| `--strategy` | 复合/字符串键 | 无键 |
|---|---|---|
| `hashdiff` / `iblt` | `keyeddiff` + 可读告警 | `bucketdiff` |
| `joindiff`（含同连接 MySQL） | `keyeddiff` | `bucketdiff` |
| `bucketdiff` | 仍 `bucketdiff`（用户要多重集） | `bucketdiff` |
| `keyeddiff` | `keyeddiff` | `bucketdiff` + 告警 |

**keyless 文案**仅在两侧都没有键时出现。复合主键 **禁止** 再报 `keyless table diff`。

---

## KeyedDiffer 四段

只读；不依赖 `ANALYZE` / `CREATE INDEX`。快照模式与 bucketdiff 相同（`open_snapshot` / `capture_scn` / `COMMIT`）。

1. **COUNT**：两侧并行 `SELECT COUNT(*) … WHERE (--where)`。`queries += 2`。N 与 FETCH_ALL 决策用**过滤后 COUNT**，不用 `reltuples` / `TABLE_ROWS`。
2. **空侧短路**：两侧 0 → 相同。一侧 0 → 只对非空侧按键列分页拉取（不哈希 53 列、不对空侧再扫）。每行 `MissingLeft` / `MissingRight`，键为完整元组。
3. **FETCH_ALL**：`--fetch-all-threshold` 默认 **4096**。`max(COUNT_L, COUNT_R) ≤ threshold` → 拉键+比对列，内存归并，不做 checksum。
4. **scan-once checksum + 复合键 keyset**：`N = clamp(ceil(max(COUNT)/bisection_threshold), 1, 1024)`。每侧 1 条 `GROUP BY MOD(hash, N)`。仅对差异桶做复合键 keyset 归并。匹配桶不拉行。

`DiffRow.key`：单列标量 JSON；复合键为 JSON **数组**。

---

## 复合键 SQL

分页用 **lexicographic OR**，禁止行值构造器 `(k1,k2) > (...)`（Oracle/空值语义不稳）：

```
(k1 > l1) OR (k1 = l1 AND k2 > l2) OR (k1 = l1 AND k2 = l2 AND k3 > l3) ...
ORDER BY k1, k2, ...
```

- 标识符用方言引号；非数字字面量把 `'` 双写。首页无 last-key 谓词。
- **ORDER BY 用原始列名**（PK 前缀索引可走），禁止 `ORDER BY CAST(col AS CHAR)`。
- 客户端归并对 `serde_json::Value` 做数字/数字字符串强制；跨库 collation 尽力而为。空侧短路不归并双流。
- 可空键列：继续跑；SQL `=` / `>` 会丢掉 NULL 键。不因此回退 bucketdiff。

scan-once checksum 的 **GROUP BY** 必须是 `MOD(...)` **表达式本身**，禁止 `GROUP BY` 选择列表别名（Oracle-safe）。

---

## bucketdiff scan-once

keyless 语义不变（内容多重集，无键身份）。实现把「N 条单桶 checksum」换成每侧 1 条 `render_batch_checksum_sql`。差异桶仍走现有 `render_bucket_multiset_sql`。

有 `--where` 时用 COUNT 定 N；无过滤时保留 `list_tables` 估计（避免无过滤全表 COUNT 的额外代价，相对 28 次扫描仍可接受）。一侧 batch map 为空时只拉非空侧出现过的桶（至多 D，不是 N）。

查询数从 `2+2N+2D` 收敛到约 `2+2+2D`。

---

## 非目标

- 不把 hashdiff/iblt 做成复合键等距二分。
- 不扩展 joindiff 多列 `ON`。
- **`--probe-columns` / 列感知两级哈希（M3）**：本补充不做、不增加该旗标。
- 450k 行不进 `cargo test`；仅 `tests/delta-diff-verify/` 下可选生成脚本。
- 不追求跨库字符串键与数据库 collation 逐字节一致。

---

## 验收

- auto + 14 列 PK → `strategy=keyeddiff`，warnings **不含** keyless 文案。
- 过滤 0 vs 5（snapshot 开）：`queries_total <= 12`；5 条 Missing*，`DiffRow.key` 为 14 元数组。
- keyless 表仍 bucketdiff，且仍有 keyless 提示。
- `--strategy bucketdiff` 对有键表仍走多重集。
- MySQL / GaussDB / Oracle batch checksum 测试断言 `GROUP BY` 表达式、非别名。
- 复合键 keyset 测试断言 lexicographic OR，而非 `(k1,k2) >`。
- `oracle/dialect.rs` 与 `oracle_native/dialect.rs` 的 batch checksum / keyset SQL 保持同步。
