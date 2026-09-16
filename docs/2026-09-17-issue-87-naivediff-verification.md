# naivediff 策略 openGauss 实测验证报告（issue #87 / PR #88）

> 本报告对 PR #88（`feat/issue-87`，实施计划见 `docs/plans/2026-09-16-issue-87-naivediff.md`）新增的 delta-diff 显式策略 **`naivediff`** 做实例化验证：在同一 openGauss 夹具上，与既有策略 `keyeddiff` / `bucketdiff`（以及显式 `iblt` / `hashdiff` 的回退行为）逐场景对比**耗时、查询数、客户端内存、判定正确性**，并用 `EXPLAIN ANALYZE` 解释 openGauss 优化器层面的差异来源。

| 项 | 值 |
|---|---|
| 日期 | 2026-09-17 |
| 被测代码 | `hepta-dbcli` @ `feat/issue-87` 分支头部 `dc39e8f`（PR #88） |
| 数据库 | openGauss 5.0.0（Docker 单实例，`dota-gaussdb`） |
| 夹具 | `tests/delta-diff-verify/cjqs_day/`（随本 PR 入库；`dota.toml` 密码为占位符，不含真实凭据） |
| 计时方式 | macOS `/usr/bin/time -l`（wall time + maximum resident set size），每策略连跑 2 次 |
| 说明 | 左右连接指向同一实例（策略机制对比目的）；生产跨实例的绝对数值会有偏移，但查询次数/内存/判定语义结论不受影响 |

---

## 0. 结论速览（TL;DR）

| 结论 | 数据支撑 |
|---|---|
| **日对账主场景（复合 VARCHAR 主键 + 日过滤，10 行差异）naivediff 比 keyeddiff 快 5.4×** | 2.71s vs 14.46s |
| **查询数恒定 4 次 vs 随脏桶/页数放大** | naivediff 4 次 vs keyeddiff 20 次（日切片）/ 40 次（全表） |
| **全表 240k 差 10 行：8.5×** | 7.79s vs 66.45s |
| **代价是内存 O(总行数)** | 80k 行≈545-550MB；240k 行≈1.05-1.2GB（keyeddiff/bucketdiff 恒 ~15MB） |
| **判定正确性与 keyeddiff 逐行一致** | 导出 CSV 排序后 `diff` 完全相同（10 Modified） |
| **bucketdiff 无行身份语义差异确认** | 同一 10 行差异报 **0 Modified / 10 Missing+10 Missing** |
| **200k 硬顶门控生效** | 超限 0.22s 快速拒绝（exit 2），提示语正确 |
| **auto 不选 naivediff；显式 iblt/hashdiff 在复合键上回退 keyeddiff** | 均实测确认 |

---

## 1. 验证环境与方法

### 1.1 表结构（DAT_FUND_CJQS 形态）

- 34 列；**14 列复合 VARCHAR 主键**：
  `xwdm, security_id, scdm, fund_code, trade_type, bs, pay_type, stock_kind, bcrq, etf_flag, gddm, gddmzm, check_type, mom_fund`
- 业务列以 `NUMERIC(15,2)/(20,8)` 为主（金额/费用），其余 VARCHAR。
- 左表 `gaussdb.dat_fund_cjqs_bak` 与右表 `gaussdb.dat_fund_cjqs_bak_r` 各 **240,000 行**（3 天 × 80k）。
- 对账切片：`--where "bcrq='20251215'"`（单日，80,000 行/侧）。

### 1.2 连接与命令模板

```toml
# dota.toml（密码略）——两个连接名，同一实例
default_connection = "dota_l"
[connections.dota_l]
driver = "gaussdb"
host = "127.0.0.1"
port = 5433
user = "gaussdb"
database = "postgres"
sslmode = "disable"
[connections.dota_r]  # 同上
```

```bash
hepta_dbcli --config dota.toml delta-diff \
  --left dota_l --right dota_r --schema gaussdb \
  --left-table dat_fund_cjqs_bak --right-table dat_fund_cjqs_bak_r \
  --strategy <S> --format json --verbose \
  [--where "bcrq='20251215'"] [--naive-max-rows <N>] \
  --export out.diff.csv --sample 0
```

差异注入（右侧 10 行 `cjje +1`，与 `tests/delta-diff-verify/cjqs_day/inject_10.sql` 等价）：

```sql
UPDATE gaussdb.dat_fund_cjqs_bak_r SET cjje = COALESCE(cjje,0)+1
WHERE ctid IN (SELECT ctid FROM gaussdb.dat_fund_cjqs_bak_r
               WHERE bcrq='20251215' LIMIT 10);
```

退出码契约逐项核对：`0` identical / `1` 有差异 / `2` 错误（含门控拒绝）。

---

## 2. 场景一：零差对账（identical）

两侧表完全一致（注入差异先回滚为 0）。所有策略判定一致：`modified=0, missing_left=0, missing_right=0`。

### 全表 240,000 × 240,000

| 策略 | 耗时 | 峰值 RSS | 查询数 | 备注 |
|---|---|---|---|---|
| `auto`（→keyeddiff） | 18.67 / 19.31 s | ~14.6 MB | 4 | 复合键走 checksum 路径 |
| `keyeddiff` | 19.14 / 19.22 s | ~14.6 MB | 4 | 零差 ⇒ 无脏桶重扫 |
| `bucketdiff` | 27.92 / 21.07 s | ~14.9 MB | 4 | 行内容哈希分桶 |
| `naivediff`（默认上限） | **0.22-0.27 s** | — | 2 | **门控拒绝，exit 2**（见 §5） |
| `naivediff`（`--naive-max-rows 0` 解除） | **6.34 / 6.88 s** | **1.05 / 1.21 GB** | 4 | **≈3× 提速，内存 ≈80×** |

### 日切片 80,000 × 80,000（`--where`）

| 策略 | 耗时 | 峰值 RSS | 查询数 |
|---|---|---|---|
| `keyeddiff` | 5.74 / 5.81 s | ~14.9 MB | 4 |
| `naivediff` | **2.36 / 2.50 s** | **544-550 MB** | 4 |
| `bucketdiff` | 6.95 / 7.32 s | ~15.1 MB | 4 |

**解读**：零差时三策略查询数同为 4（COUNT×2 + 主体查询×2），差异来自**主体查询的形态**——keyeddiff/bucketdiff 在库端做 MD5 分桶聚合（两次全表聚合），naivediff 只做一次流式扫描 + 客户端归并。naivediff 用内存换掉了库端聚合与归并开销。

---

## 3. 场景二：10 行 Modified（对账主场景）

右侧注入 10 行 `cjje` 差异后重跑。**所有策略 exit 1**（CI 契约正确）。

### 日切片 80k × 80k

| 策略 | Modified | Missing L/R | 查询数 | 耗时 |
|---|---|---|---|---|
| `keyeddiff` | **10** | 0 / 0 | **20**（2 COUNT + 2 checksum + 16 脏桶页查询） | 14.46 s |
| `naivediff` | **10** | 0 / 0 | **4** | **2.71 s**（5.4×） |
| `bucketdiff` | ⚠️ **0** | 10 / 10 | 14 | 22.29 s |
| 显式 `iblt` | 10 | 0 / 0 | 20 | 19.39 s（**回退→keyeddiff**） |
| 显式 `hashdiff` | 10 | 0 / 0 | 20 | 15.69 s（**回退→keyeddiff**） |

### 全表 240k × 240k

| 策略 | 查询数 | 耗时 |
|---|---|---|
| `keyeddiff` | **40**（脏桶随 15 个分桶放大） | 66.45 s |
| `naivediff`（cap 解除） | **4** | **7.79 s（8.5×）** |

### 正确性交叉验证

- `keyeddiff` 与 `naivediff` 导出 CSV（`--sample 0` 全量）**排序后逐行 `diff` 完全一致**——10 行 Modified 的键与新旧值完全相同。
- `bucketdiff` 按行内容哈希分桶、无键配对：10 行 Modified 被拆成 **10 MissingLeft + 10 MissingRight**，`modified=0`。这是既有设计语义（对账若需区分 Modified 必须用有键策略），PR #88 不改变它。

---

## 4. EXPLAIN 实证：差异的 openGauss 优化器根源

### 4.1 keyeddiff 脏桶页查询形态（verbose 日志原样提取）

```sql
SELECT "xwdm", ..., "sxf"                                  -- 14 键列 + 20 归一化值列
FROM "gaussdb"."dat_fund_cjqs_bak"
WHERE ((bcrq='20251215') AND (MOD(('x' || SUBSTR(MD5(concat_ws('#',
      "xwdm","security_id","scdm","fund_code","trade_type","bs","pay_type",
      "stock_kind","bcrq","etf_flag","gddm","gddmzm","check_type","mom_fund")),
      1, 8))::bit(32)::bigint, 5) = 0))                    -- 键哈希桶谓词
ORDER BY "xwdm" COLLATE "C", "security_id" COLLATE "C", ... -- 14 列 × COLLATE "C"
LIMIT 8192;
```

`EXPLAIN (ANALYZE, COSTS OFF)`（openGauss 5.0.0）：

```
Limit (actual time=374.103..374.953 rows=8192 loops=1)
  ->  Sort (actual time=374.101..374.498 rows=8192 loops=1)
        Sort Key: xwdm COLLATE "C", security_id COLLATE "C", ... (14 keys)
        Sort Method: quicksort  Memory: 5008kB
        ->  Seq Scan on dat_fund_cjqs_bak (actual time=1.131..349.543 rows=15996 loops=1)
              Filter: (bcrq='20251215' AND mod(md5桶谓词=0))
              Rows Removed by Filter: 224004
Total runtime: 378.785 ms
```

**要点**：PK 索引（`pk_fund_cjqs_bak`）完全不可用——(a) 桶谓词是 MD5 表达式，不可索引；(b) `COLLATE "C"` 排序键与索引 collation 不匹配，无法 Index Scan 有序输出 ⇒ 每页都 **Seq Scan + quicksort**。而这一整套扫描按 `脏桶 × 页数 × 两侧` 重复执行（本例 16 次）。

### 4.2 naivediff 扫描形态

```sql
SELECT "xwdm", ..., "sxf"
FROM "gaussdb"."dat_fund_cjqs_bak"
WHERE (bcrq='20251215')
ORDER BY "xwdm", "security_id", ..., "mom_fund";   -- 裸列，无 COLLATE/NLSSORT，无 LIMIT
```

```
Sort (actual time=292.989..296.416 rows=80000 loops=1)
  Sort Key: xwdm, security_id, ... (14 keys, 裸列)
  Sort Method: quicksort  Memory: 26198kB
  ->  Seq Scan on dat_fund_cjqs_bak (actual time=1.002..186.051 rows=80000 loops=1)
        Filter: (bcrq='20251215')
        Rows Removed by Filter: 160000
Total runtime: 302.938 ms
```

**要点**：单次 Seq Scan + 一次排序，**每侧仅执行一次**；正确性由客户端 `HashMap` 指纹归并保证，不依赖库端行序——这正是计划文档里「裸列 ORDER BY（NLSSORT/COLLATE 即 34s↔6s 回归根因）」决策的 openGauss 侧实证。

---

## 5. 门控与回退行为实测

### 5.1 行数硬顶（默认 200,000）

全表 240k 直接拒绝，**仅 0.22s**（只执行 2 次 COUNT）：

```text
naivediff row cap exceeded: left=240000 right=240000 cap=200000;
use --strategy keyeddiff for larger filtered sets
```

退出码 `2`。日切片 80k 配 `--naive-max-rows 50000` 同样拒绝（消息中 cap=50000）。解除方式：`--naive-max-rows 0` 或调大阈值。

### 5.2 显式策略回退

| 显式指定 | 实际执行 | 证据 |
|---|---|---|
| `--strategy iblt` | **keyeddiff** | verbose 头行 `strategy=keyeddiff`；结果 10 Modified 一致 |
| `--strategy hashdiff` | **keyeddiff** | 同上 |

复合 VARCHAR 键不满足「单整数键」条件，按 `keyed_or_bucket_fallback` 路由回 keyeddiff——行为与 auto 一致，结果正确。
（按 PR 代码路径，无键表显式 `naivediff` 走无键多重集模式而非回退 bucketdiff；本夹具无无键表，该项以代码与单测为准。）

---

## 6. 策略选型决策表（openGauss 视角）

| 场景 | 推荐 | 依据 |
|---|---|---|
| 复合 VARCHAR 主键日对账（零差/少差，过滤后 ≤20 万行） | **naivediff** | 5.4× 提速、恒 4 查询；内存 ~550MB/80k 行可接受 |
| 大表 / 内存受限 / 过滤后 >20 万行 | **keyeddiff**（auto 默认即选） | 常数内存 ~15MB；零差 19s 可接受 |
| 无主键 / 键无法配对 | **bucketdiff**（auto 即选） | 唯一可用；注意 Modified→Missing 对语义 |
| 单整数主键跨实例 | auto→`iblt` | 本夹具不适用（复合键） |
| 同连接 MySQL 单整数键 | auto→`joindiff` | openGauss 上不适用（非 mysql family） |

内存量级参考（naivediff，34 宽列）：**80k 行 ≈ 550MB，240k 行 ≈ 1.1-1.2GB**。生产建议配合 `--columns` 裁剪对比列以降低内存。

---

## 7. 局限与注意事项

1. 左右连接指向同一实例（便于控制变量），跨实例网络往返会进一步放大 naivediff「少查询」的优势（4 次 vs 20-40 次）。
2. 计时为 macOS + Docker 单机数据，绝对值随环境浮动；查询次数、RSS 量级、判定语义、EXPLAIN 计划形态是稳定结论。
3. `naivediff` 判定正确性不依赖库端排序，但依赖两侧 schema 可配对（键列/类型一致），与 keyeddiff 前提相同。
4. PR #88 自述的人工计时验收（Oracle 真实夹具 8 万行零差/10 差 vs checksum ~6s/11s）待回填 issue #87；本报告提供的是 openGauss 侧的对应实测。
