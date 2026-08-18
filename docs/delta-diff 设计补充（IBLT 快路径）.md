# `delta-diff` 设计补充：IBLT 小差异快路径（Addendum A，v1.1）

> **文档性质**：本文档是《delta-diff 设计文档（修订版 v2.1）》的**补充（Addendum）**，不修改 v2.1 任何内容。v2.1 中的策略、一致性设计、性能模型全部保持有效；本文档仅**新增一个可选策略 `iblt`** 作为小差异场景的快路径。
>
> **动机**：v2.1 的二分 checksum 在"差异极少"这一实践中最常见的场景下仍需 O(log N) 轮查询和行级拉取。IBLT（Invertible Bloom Lookup Table，可逆 Bloom 查找表）可以在**一次往返、O(d) 传输量**（d = 实际差异数）内直接解出全部差异 key，且摘要可用一条 SQL 在源库端算出，与 v2.1 的位切片 checksum 共享同一套规范化层（v2.1 §九）。
>
> **修订说明（v1.1，相对初版 Addendum）**——基于 IBLT 原始文献核对与三容器实测（2026-08-17，见 §七）：
>
> - **[P0-修复]** 摘要结构由"单桶分配"修复为 **j=4 哈希子表**：初版每行仅入一桶，剥洋葱无级联，容量规则退化为生日界（d=64K 时解码成功率 ≈ 0），"k≈3d" 结论仅在每条目多哈希的 IBLT 下成立（见 §1.2）。
> - **[P0-修复]** Oracle XOR 重构注的数学错误：4-bit 切片求和无法推出 XOR，须逐 1-bit 奇偶；另 Oracle 21c+/23ai 原生 `BIT_XOR_AGG`（见 §3.3）。
> - **[P0-实测]** openGauss 5.0.0 **无 `bit_xor` 聚合**（仅标量 `bitxor`），§3.2 改写为逐位奇偶 SUM 方案；MySQL/PolarDB-X `BIT_XOR` 聚合实测可用且结果互验一致（见 §七）。
> - **[P1-修复]** 桶位推导与 val_xor 的比特位**对齐约束**显式化（剥洋葱剔除条目需从 val_xor 重算桶位，见 §1.4）；`§2.3` 删除"复用 IBLT 扫描作 hashdiff 输入"的不成立声明；§4.3 SLO 按 v2.1 §16.3-F7 实测吞吐分层重写。
> - **[P1-同步]** 基线由 v2.0 升至 v2.1：PolarDB-X 快照降级语法（v2.1 §8.2/§16.3-F5）、GaussDB 无时区 DATETIME 公式修正（v2.1 §16.3-F3）。

---

## 一、算法原理（简述）

IBLT 是 Bloom 过滤器的**可解码**版本（Eppstein et al., *"What's the Difference? Efficient Set Reconciliation without Prior Context"*, SIGCOMM 2011）。

### 1.1 与 Bloom 过滤器的对比

| 能力 | Bloom 过滤器 | IBLT |
|------|-------------|------|
| 回答"某 key 在不在" | ✅（否定答案零假阴性，肯定答案有误判率 p） | 不用于单点查询 |
| 回答"差异集合是什么" | ❌（只存存在性，无法枚举） | ✅（d ≤ 容量时可完整解出） |
| 两侧摘要相减求差 | ❌ | ✅（逐桶相减即得对称差的摘要） |
| 源库端 SQL 原生聚合 | ❌（需流传全量 key 到客户端构建） | ✅（COUNT/XOR 均为顺序无关聚合，见 §三） |
| 适用场景 | 一侧数据已在本地的预筛 | 双活库小差异恢复 ★ |

> **为什么不选 Bloom 做双库比对**：Bloom 无法 DB 端构建，需把一侧 key 全集（1B keys ≈ 8GB）传至客户端，违背 v2.1 "传输量与差异数成正比、客户端内存常数级"的性能目标（v2.1 §11.1/§11.2）。Bloom 预筛仅作为"导出文件 vs 数据库"类场景的离线选项，不进入主策略。

### 1.2 IBLT 结构（v1.1 修复：j=4 哈希子表）

每行映射到 **j=4 个桶**（每个哈希子表一个桶），而非初版的单桶。128-bit 行哈希切 4 个 32-bit 切片 `s1..s4`，第 j 个子表的桶位为 `MOD(s_j, m)`，m = ⌈3d/4⌉ 为子表桶数，**总桶数 k = 4m ≈ 3d**。每桶累加三个**顺序无关**的聚合量：

```
cell[j][i] = {
  cnt:     落入该桶的行数（两侧相减时为代数和）,
  key_xor: 落入该桶所有行的 key 之 XOR（key 映射为定长整数，§四.2）,
  val_xor: 落入该桶所有行的 row_hash 之 XOR（128-bit，四个 32-bit 切片并列，承载 s1..s4）
}
```

> **v1.1 修复理由（初版致命缺陷）**：初版"每行按 `MOD(row_hash, k)` 落入一桶"是单哈希 XOR 桶，剥洋葱**没有级联**——两个及以上差异条目同桶即永久卡死，解码成功 ⟺ 全部 d 条差异各自独占一桶，成功率受生日界支配（k=196608 时 d=1000 成功率仅 ~8%，d=64K 时 ≈ 0）。IBLT 文献（Goodrich & Mitzenmacher, arXiv:1101.2245）的容量阈值建立在**每条目 j ≥ 2 个独立哈希**之上：总桶数 m_total > c_j·d 时解码高概率成功，阈值常数 c_3=1.222、c_4=1.295、c_5=1.425。本文档取 j=4（恰好用尽 MD5 的 4 个切片）、m_total=3d，为 c_4 阈值的 2.3 倍余量，失败概率 O(d^{-(j-2)})（d=64K 时 ≪ 10⁻⁶），且失败永远**失败安全**（卡住即回退，无误解码，见 §2.3）。

**两侧摘要逐桶相减**（cnt 相减、key_xor/val_xor 相异或），得到对称差的 IBLT。随后**剥洋葱解码（peeling）**：

1. 找到 `cnt = ±1` 的桶 → 候选条目 = (key_xor, val_xor)；先做**纯桶校验**（§1.4）：由 val_xor 重算 4 个桶位，当前桶必须在其列；
2. 校验通过 → 将该条目从它的全部 4 个桶中剔除（递减/递增 cnt、异或 key/val）；
3. 重复直到摘要全空（**解码成功**，得到全部差异条目），或找不到 `cnt = ±1` 的桶（**解码失败**，差异超容量，回退，见 §2.3）。

**容量规则**：要可靠解出 d 条差异，总桶数 k ≈ 3d（j=4，余量 2.3 倍于 c_4 阈值）。每桶固定字节数（cnt 4B + key_xor 8B + val_xor 16B ≈ 28B），故摘要大小 = 3d × 28B，**与表行数 N 完全无关**。

### 1.3 差异类型识别

| 情况 | 摘要表现 | 判定 |
|------|---------|------|
| 左有右无（MissingRight） | 该 key 的条目只出现在左侧摘要，相减后在其 4 个桶中 cnt 净 +1 | 增/删（方向由 cnt 符号判定） |
| 右有左无（MissingLeft） | cnt 净 −1 | 同上 |
| 同 key 值被改（Modified） | 条目 = (key, row_hash)，修改前后 row_hash 不同 → 旧值条目仅左有（cnt +1）、新值条目仅右有（cnt −1），**通常落入不同桶**，解码出同一 key 两次 | 解码后统一经 v2.1 §8.3 点查复核确认最终类型 ★ |

> ★ 初版"key_xor 同桶抵消（cnt 代数和为 0）"仅为 1/m 概率的碰撞特例（此时该桶 cnt=0、key_xor=0、val_xor ≠ 0 → 卡死 → 失败安全回退）。主场景是上述"两桶各解出一次"。Modified 的识别完全复用 v2.1 §8.3 复核通道，不新增机制。

### 1.4 对齐约束（v1.1 新增，正确性硬性条件）

剥洋葱剔除条目时，客户端只能从摘要中获得 `(key_xor, val_xor)`——**桶位必须能由 val_xor 重算**。因此 §三 的 SQL 必须满足：第 j 子表的桶位 = `MOD(val_xor 的第 j 个切片, m)`，即**桶位切片 ⊆ val_xor 覆盖的 128 bit**。纯桶校验同理（候选条目重算的桶位须包含当前桶，否则为"净 ±1 的伪纯桶"，跳过并继续；残留非空即判解码失败）。初版 SQL（slice1 定桶、slice2/3 作 val_xor）恰好错位，不满足此约束。

---

## 二、策略设计：`--strategy iblt`

### 2.1 定位

在 v2.1 §六的策略族中新增第四个策略，**纯增量、不改既有策略**：

```
auto ──┬── 无主键 / 不可二分键 → bucketdiff（不变）
       ├── 同构同版本          → joindiff（不变）
       └── 其余               → ★ iblt 快路径优先，失败回退 hashdiff（§2.3）
```

### 2.2 执行流程

```
1. 一致性：复用 v2.1 §8.2 单侧快照（两侧各一条摘要 SQL，均在各自快照事务内；
   PolarDB-X 侧用 §16.3-F5 降级语法 RR + START TRANSACTION READ ONLY）
2. 两侧各执行一条 IBLT 摘要 SQL（§三，j=4 子表），返回 k 个桶（k 由 --iblt-capacity 决定）
3. 客户端逐桶相减 → peeling 解码（含 §1.4 纯桶校验）
   ├─ 解码成功且结果为空        → 两侧一致，直接返回（等同于零差异快筛）
   ├─ 解码成功且得到差异 key 集 → 逐 key 点查复核（v2.1 §8.3），分类
   │                             MissingLeft/MissingRight/Modified，出报告
   └─ 解码失败（差异超容量）    → 回退 HashDiffer 全量二分（§2.3）
```

### 2.3 失败回退（安全性关键）

- 回退对**用户透明**：报告中记录 `strategy: "iblt"` + `fallback: "hashdiff (capacity exceeded, d > 64K)"`；
- 回退成本有界但**不可免除**（v1.1 修正初版错误声明）：IBLT 摘要按内容哈希分桶，hashdiff 需要按 **key 范围**的分段 checksum，两者维度不同，**IBLT 扫描结果不能复用为 hashdiff 输入**；回退后 hashdiff 的首轮分段快筛扫描仍需执行。可在摘要 SQL 同层附带 4 个全局 SUM 切片产出全表级 checksum（供日志/审计），但不替代分段扫描；
- `--strategy iblt` 显式指定且差异超容量时，行为同上（回退而非报错），加 `--strict` 才报错退出（exit 2）。

### 2.4 新增参数（追加到 v2.1 §2.2，既有参数不变）

```
  --iblt-capacity <N>   IBLT 预期差异容量 d [默认: 65536]
                        摘要桶数 k = 3d；超过容量自动回退 hashdiff
  --strict              与 --strategy iblt 联用：解码失败时报错（exit 2）而非回退
```

auto 模式下 IBLT 快路径的启用条件：有可二分主键（数值/时间戳）**且** 用户未显式指定策略。不满足条件（无主键、字符串/复合键）时直接走 v2.1 既有路由。

---

## 三、摘要 SQL（源库端，三方言；v1.1 重写为 j=4 子表）

核心技巧与 v2.1 §十同源：COUNT 与 XOR 均为顺序无关聚合，无需排序、结果定长。每行经 4 行辅助表展开为 4 条投影，`GROUP BY (grp, cell)` 一次扫描产出全部 k 个桶。**硬性约束（§1.4）**：第 j 个子表的桶位必须取 val_xor 覆盖的切片——下述 SQL 中桶位与 val_xor 共用 `s1..s4`（MD5 的 4 个 32-bit 切片），桶位 j 用 `s_j`。`ORDER BY` 不需要（客户端按 (grp, cell) 对齐，顺序无关）。

### 3.1 MySQL / PolarDB-X ✅ 2026-08-17 实测通过（§七）

```sql
SELECT g.grp                                                    AS grp,
       MOD(CONV(SUBSTRING(h, g.grp * 8 - 7, 8), 16, 10), :m)    AS cell,
       COUNT(*)                                                  AS cnt,
       BIT_XOR(CAST(:key_expr AS UNSIGNED))                      AS key_xor,
       BIT_XOR(CONV(SUBSTRING(h,  1, 8), 16, 10))                AS val_xor_1,
       BIT_XOR(CONV(SUBSTRING(h,  9, 8), 16, 10))                AS val_xor_2,
       BIT_XOR(CONV(SUBSTRING(h, 17, 8), 16, 10))                AS val_xor_3,
       BIT_XOR(CONV(SUBSTRING(h, 25, 8), 16, 10))                AS val_xor_4
FROM (
  SELECT MD5(CONCAT_WS('#', <v2.1 §九 规范化表达式>)) AS h, :key_expr
  FROM orders
  WHERE <filter>
) t
JOIN (SELECT 1 AS grp UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4) g
GROUP BY g.grp, cell;
```

> - val_xor 取全 128 bit（4 段 32-bit XOR）：既作纯桶校验，又承载桶位重算所需的 4 个切片（§1.4）。
> - PolarDB-X 注意（实测）：`MOD()` 返回结果为 DECIMAL 类型（客户端读出 `'0.0'` 形态），驱动层解析需按 DECIMAL 处理；聚合值本身与 MySQL 逐桶一致。

### 3.2 GaussDB（v1.1 改写：无 BIT_XOR 聚合，逐位奇偶 SUM）

实测 openGauss 5.0.0 仅有标量 `bitxor(a,b)`，**无 bit_xor 聚合**（§七-F2）。回退方案为**逐位奇偶求和**：XOR 的第 i 位 = 该位累加和 mod 2。

```sql
SELECT g.grp,
       MOD(('x' || SUBSTR(h, g.grp * 8 - 7, 8))::bit(32)::bigint, :m) AS cell,
       COUNT(*)                                                       AS cnt,
       -- key_xor 的 bit i（i = 0..63 展开为 64 列）：
       MOD(SUM(((:key_expr)::bigint >> 0) & 1), 2) AS kx_0,
       /* ... kx_1 .. kx_63 同形展开 ... */
       -- val_xor 四切片共 128 bit，同理各展开 32 列：
       MOD(SUM((('x' || SUBSTR(h, 1, 8))::bit(32)::bigint >> 0) & 1), 2) AS vx1_0
       /* ... 共 128 列 ... */
FROM (
  SELECT MD5(<v2.1 §九 规范化拼接，无时区列用 plain to_char —— §16.3-F3>) AS h, :key_expr
  FROM orders
  WHERE <filter>
) t
JOIN (SELECT 1 AS grp UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4) g
GROUP BY g.grp, cell;
```

> 代价：每侧返回 k 行 × ~196 列（cnt + 64 + 128 + grp/cell），客户端按位 mod 2 拼回 key_xor/val_xor。SQL 文本由 `sql/checksum.rs` 生成（单列模板循环展开），与位切片 checksum 共用 fixture 跨库断言单测。
>
> 备选（不默认采用）：`CREATE AGGREGATE bit_xor_agg(bigint) (SFUNC=int8xor, STYPE=bigint, INITCOND='0')`——语法更简，但要求**在源库创建对象**，违背只读原则，仅在用户明确授权时作为部署选项。
>
> 范围决策：GaussDB 的 iblt 支持可延后（路由层先降级 hashdiff），不阻塞 Phase 2.5 的 MySQL/PolarDB-X 主线（见 §五）。

### 3.3 Oracle（v1.1 修正数学错误 + 版本分叉）

初版注称"16 个 4-bit 切片求和，客户端重组为 XOR 等价量"——**数学上不成立**：XOR 是逐位奇偶，4-bit 切片的 SUM 丢失了位内分布，无法重构 XOR。正确回退为逐 1-bit 奇偶；21c+ 有原生聚合。

```sql
-- Oracle 21c+ / 23ai（原生 BIT_XOR_AGG）
SELECT g.grp,
       MOD(TO_NUMBER(SUBSTR(RAWTOHEX(h), g.grp * 8 - 7, 8), 'XXXXXXXX'), :m) AS cell,
       COUNT(*)                              AS cnt,
       BIT_XOR_AGG(:key_expr)                AS key_xor,
       BIT_XOR_AGG(TO_NUMBER(SUBSTR(RAWTOHEX(h),  1, 8), 'XXXXXXXX')) AS val_xor_1,
       BIT_XOR_AGG(TO_NUMBER(SUBSTR(RAWTOHEX(h),  9, 8), 'XXXXXXXX')) AS val_xor_2,
       BIT_XOR_AGG(TO_NUMBER(SUBSTR(RAWTOHEX(h), 17, 8), 'XXXXXXXX')) AS val_xor_3,
       BIT_XOR_AGG(TO_NUMBER(SUBSTR(RAWTOHEX(h), 25, 8), 'XXXXXXXX')) AS val_xor_4
FROM (
  SELECT STANDARD_HASH(<v2.1 §九 规范化拼接>, 'MD5') AS h, :key_expr
  FROM orders AS OF SCN :scn            -- 快照模式注入点（v2.1 §8.2）
  WHERE <filter>
) t
JOIN (SELECT 1 AS grp FROM dual UNION ALL SELECT 2 FROM dual
      UNION ALL SELECT 3 FROM dual UNION ALL SELECT 4 FROM dual) g
GROUP BY g.grp, cell;

-- Oracle 19c（无 BIT_XOR_AGG）：同 GaussDB 逐位奇偶 SUM：
--   MOD(SUM(BITAND(:key_expr, POWER(2, 0))) / POWER(2, 0), 2) AS kx_0, ... 展开
```

> 版本路由：`sql/checksum.rs` 按 `database_info()` 版本串选择模板；19c 形态同 GaussDB（~196 列奇偶位）。Oracle 全形态（STANDARD_HASH / AS OF SCN / BIT_XOR_AGG 可用性 / oracle-rs 驱动支持）均为 v2.1 §16.3-F8 待办，Phase 0 补验。

---

## 四、约束与边界

### 4.1 正确性约束

| 项 | 约束 | 处理 |
|----|------|------|
| key 类型 | key_xor 要求 key 可映射为定长整数：数值/时间戳（转 epoch）直接 XOR；**字符串/复合键不支持** | 路由层判定，不满足则不启用 iblt（走 bucketdiff/hashdiff） |
| 重复 key | IBLT 假设集合语义；比对键有重复时剥洋葱可能出错 | 启动校验 key 唯一性（复用 v2.1 主键发现，`dialect.table_indexes()`）；非唯一则禁用 |
| 容量估计 | d 超过 capacity 时解码**高概率失败**（而非静默出错，peeling 卡死即失败，无误解码风险） | 透明回退 hashdiff（§2.3） |
| 对齐约束 | 桶位切片必须 ⊆ val_xor 覆盖比特（§1.4） | `sql/checksum.rs` 模板单测断言：剥洋葱重算桶位与 SQL 桶位表达式一致 |
| 假空摘要 | 差异条目在同桶内三场全抵消 → 假阴性，概率 ~2⁻¹²⁸/单元量级 | 与 hashdiff 的碰撞边界同级；报告记录 capacity 与实际 d |
| XOR 聚合可用性 | openGauss 无 bit_xor 聚合（实测）；Oracle 19c 无 BIT_XOR_AGG | 逐位奇偶 SUM 回退（§3.2/§3.3），或路由层对该方言降级 hashdiff |
| 一致性 | 与 v2.1 完全相同的快照 + 复核语义（PolarDB-X 用降级快照语法） | 摘要 SQL 在快照事务内执行；解码结果经 v2.1 §8.3 复核 |

### 4.2 成本模型（对照 v2.1 §11.2）

```
IBLT 路径：
  传输量  ≈ 2 × 3d × 28B + d × 行宽          （O(d)，与 N 无关）
  查询数  ≈ 2（摘要）+ 2d（复核点查）
  源库成本 = 每侧 1 次全表聚合扫描（与 v2.1 首轮快筛同阶）

  ★ 可合并性（v1.1 补充）：摘要按不相交 key 范围切片执行后，客户端可逐桶合并
    （cnt 求和、key_xor/val_xor 异或）——即 none 模式下可复用 v2.1 §6.2 的
    分段并行手段扩展吞吐；snapshot 模式单会话串行（v2.1 §8.2），吞吐不扩展。

hashdiff 路径（对照）：
  查询数  ≈ S_first + D × depth × 2，depth ≈ log₃₂(N/16384)
  行级拉取 = threshold × 差异分片数
```

### 4.3 SLO 收益（追加到 v2.1 §11.1，既有目标不变；v1.1 按实测分层重写）

> 初版"< 60s"的前提是"单侧聚合吞吐 ≥ 3M 行/s"，已被 v2.1 §16.3-F7 实测推翻（开发容器单会话 MySQL 0.19–0.22M 行/s、openGauss 0.05M 行/s）。IBLT 摘要同样受单会话聚合吞吐约束（snapshot 模式），且 GROUP BY k 桶的聚合开销不低于分段 checksum——**收益在查询轮次与传输量，不在扫描吞吐**。

| 场景 | hashdiff（v2.1） | iblt 快路径（本文档） | 前提 |
|------|-----------------|----------------------|------|
| 🔍 小差异恢复：100M 行、d ≤ 64K | < 3min（log 轮次 + 行级拉取） | **none 档 < 60s**（分段并行摘要 + d 条点查，§4.2 可合并性）；**snapshot 档按实测回填**（100M 行 ÷ 实测 R_scan，容器口径约 500s） | 与 v2.1 §11.1 同前提；生产 NVMe R_scan 待 Phase 1 基准实测 |
| 🔥 零差异判定 | 分段并行快筛（threads×8 段查询） | 摘要相减为空即返回：snapshot 档 2 次查询（轮次更省，扫描量相同）；none 档分段并行同 hashdiff | — |
| ♾️ 10B 行 | 不变 | IBLT 摘要大小仍只与 d 有关，10B 行小差异场景收益最大 | 摘要传输 ~5.5MB/侧（默认容量） |

---

## 五、实施计划（追加到 v2.1 §十四，既有 Phase 不变；v1.1 估期修正 1 周 → 1.5–2 周）

**插入 Phase 2 与 Phase 3 之间，作为 Phase 2.5（实施状态见 ✅ 标记，2026-08-17）**：

1. **Phase 0 补验（前置，复用 `tests/delta-diff-verify/` 资产）**——✅ 完成：GaussDB 逐位奇偶 SUM 形态 fixture 断言（96 列拼回 XOR 逐桶一致）；Oracle 23ai `BIT_XOR_AGG` 实测存在；MySQL/PolarDB-X `BIT_XOR` 聚合互验一致；
2. `sql/checksum.rs` 扩展——✅ 落地为 `Dialect::render_iblt_sql`（四后端）：MySQL/PolarDB-X `BIT_XOR` 模板、GaussDB 奇偶 SUM（196 列）、Oracle `BIT_XOR_AGG`（21c+；19c 走路由降级）；对齐约束（§1.4）内建于模板（桶位 j 用 val_xor 第 j 切片）；
3. `strategies/iblt_diff.rs`——✅ 落地为 `delta_diff/iblt_diff.rs`：摘要相减 + peeling 解码器（纯桶校验 + 失败检测 + 按 key 分类）；
4. 路由接入——✅ 完成：auto 跨实例可二分键 → iblt（同连接仍 joindiff）；`--iblt-capacity`/`--strict` 参数；透明回退 hashdiff + 报告 fallback 告警；MCP strategy 参数加 iblt；
5. 测试——✅ E2E d 矩阵（MySQL↔PolarDB-X）：d=0 decoded-empty；d=5 精确解码（3 Modified + 1 MissingRight + 1 MissingLeft）；d=1999 > capacity 1000 透明回退（modified 精确）；`--strict` 同场景 exit 2。**实现期抓到一个真实缺陷**：`cell`（MOD 结果）以 f64/DECIMAL 形态返回时解析层归零（全部条目落 4 桶→必然卡死），已修并作为教训记入§七-F7；
6. CI 基准——复用 `.github/workflows/delta-diff-bench.yml`；iblt 场景（25M 行 / d=1K）随 Phase 4 基准一起回填 §4.3。

---

## 六、关键决策摘要

| 决策 | 选择 | 理由 |
|------|------|------|
| 新增策略而非替换 | iblt 作为第 4 策略，hashdiff 保留为回退 | IBLT 有容量前提，无法覆盖大差异场景；快路径 + 兜底是标准形态 |
| 摘要结构 | **j=4 哈希子表**（MD5 四切片各定一子表桶位） | 单桶无级联、容量规则退化为生日界（d=64K 必败）；文献阈值 c_4=1.295，3d 总桶数余量 2.3 倍（§1.2） |
| 桶位/校验对齐 | 桶位切片 ⊆ val_xor（128 bit） | 剥洋葱只能拿到 (key_xor, val_xor)，桶位必须可重算（§1.4，模板单测断言） |
| 摘要位置 | 源库端 SQL 聚合 | 与 v2.1 checksum 同一原则：传输量 O(d)，客户端内存常数级 |
| Bloom 过滤器 | 不进入主策略 | 需全量 key 传输（1B keys ≈ 8GB），违背 v2.1 性能目标；仅留作离线预筛选项 |
| XOR 聚合 | MySQL/PolarDB-X 原生 BIT_XOR（✅实测）；GaussDB/Oracle 19c 逐位奇偶 SUM；Oracle 21c+ BIT_XOR_AGG | openGauss 5.0.0 无 bit_xor 聚合（§七实测）；奇偶 SUM 是唯一不建对象的纯只读回退 |
| 差异类型确认 | 解码后统一走 v2.1 §8.3 点查复核 | Modified 经"同一 key 解码两次"表达，分类由复核兜底，不新增机制 |
| 默认容量 | d = 65536（k = 196608 桶，摘要 ~5.5MB/侧） | 覆盖实践中小差异场景；传输 5.5MB 在秒级内完成 |
| SLO 口径 | 仅承诺 none 档数字；snapshot 档实测回填 | 与 v2.1 §11.1 分层一致（§16.3-F7 实测单会话吞吐远低于此前提） |

---

## 七、实测核验记录（2026-08-17，随 v1.1 回填）

环境同 v2.1 §十六（`tests/delta-diff-verify/` 三容器）。

- **F1**：MySQL 8 `BIT_XOR` 聚合可用；IBLT 摘要原型（k=8 桶单表形态）与 python 期望**逐桶一致**（cnt/key_xor/val_xor 全字段）。
- **F2**：openGauss 5.0.0 **无 `bit_xor` 聚合**（`pg_proc` 仅标量 `bitxor`/`int1xor`/`int2xor`/`int4xor`/`int8xor`；`bit_and`/`bit_or` 聚合存在）→ §3.2 逐位奇偶 SUM 改写依据。
- **F3**：PolarDB-X `BIT_XOR` 聚合可用且与 MySQL 逐桶一致；`MOD()` 返回类型为 DECIMAL（输出 `'0.0'` 形态），客户端解析注意。
- **F4**：文献核对（Goodrich & Mitzenmacher, arXiv:1101.2245）：IBLT 解码阈值为总桶数 > c_j × d（c_3=1.222 / c_4=1.295 / c_5=1.425），每条目 j≥2 哈希；单桶结构无该性质——§1.2 修复依据。
- **F5（2026-08-17 补验）**：Oracle 23ai 原生 `BIT_XOR_AGG` 存在（XOR(1,2,3)=0 探针通过）；19c 仍走 §3.3 逐位奇偶回退，版本路由按 `database_info()` 版本串。
- **F6（2026-08-17 补验）**：GaussDB 逐位奇偶 SUM 形态在 verify_t 上逐桶与 python 期望一致（96 奇偶列 + cnt，k=8；2000 行 1.44s）——§3.2 方案实测成立，宽列开销在小数据可接受，1M 行级性能复测列入 Phase 2.5-1。
- **F7（2026-08-17 实现回填）**：j=4 摘要 SQL 四后端落地 + peeling 解码器 E2E d 矩阵全过（decoded-empty / decoded / 透明回退 / strict exit 2）。**实现期抓到两个真实缺陷并修复**：① 摘要 SQL 外层引用未别名化的 key 列（子查询未导出，ERROR 1054）——外层统一改用 `k` 别名；② `cell`（MOD 结果）以 f64/DECIMAL 形态到达客户端时解析层归零，导致全部条目落入 4 个桶、解码必然卡死并误报"容量超限"——解析层兼容 number(i64/u64/f64 整值) 与 DECIMAL 字符串。另验证 peeling 对"Modified 对撞同桶"场景可收敛（提取 old 后对撞桶转为 new 的纯桶，python 仿真确认）。

---

*本文档为 v2.1 的 Addendum A（v1.1，含 2026-08-17 实测回填），可与 v2.1 独立评审、独立落地；落地时仅需追加 §五所列增量，无需改动 v2.1 既有设计。*
