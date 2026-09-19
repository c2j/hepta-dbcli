# opt/delta-diff-v2 实施设计（swarm 共享上下文）

> 分支：`opt/delta-diff-v2`（自 main @ cf576b7）。三个工作包(WP)并行，改动文件互不重叠。
> 基线：`docs/plans/opt-delta-diff-v2-baseline.tsv`（1M 行 dbench.src_t/dst_t，101 行差异：id 400001–400100 集中修改 + id=0 c2 修改）。

## 基线数据（macOS aarch64, MySQL 8.4 @127.0.0.1:13306, --consistency none --format json --sample 0）

| strategy | wall (3 runs) | max RSS | diffs |
|----------|--------------|---------|-------|
| hashdiff | 2.05 / 2.03 / 2.12 s | ~16.8 MB | 100 |
| bucketdiff | 94.29 / 93.47 / 97.61 s | ~27.5 MB | 200 (multiset 语义) |
| iblt | 38.42 / 46.61 / 39.30 s | ~280 MB | 100 |

注意：iblt 容量 65536（默认），差异仅 100 → 通信与内存都严重超配。bucketdiff 的 ~94s 主要耗在逐 diff-bucket 的 multiset 全行拉取（GROUP BY mod 哈希聚合无索引 → 每桶一次扫描；本轮数据集差异集中在 400001–400100 连续 id，桶分布密集）。

## 全局约束（所有 WP 必须遵守）

1. **不改公开 CLI 语义**：现有 flag 全部保持兼容；新增能力走新增 flag（如 `--iblt-auto-capacity`）且默认行为必须经维护者确认后再改，本轮新增 flag 默认值=旧行为。
2. **正确性红线**：所有策略的 diff 结果与 main 分支在以下场景必须一致（除新增 flag 显式改变行为外）：
   - identical（0 差异）
   - spread-101（`UPDATE dst_t SET c2=CONCAT('x',id) WHERE MOD(id,9973)=0`）
   - clustered-100（id 400001–400100）
   - 空 vs 非空、单侧缺行、modified、multiset 重复行
3. **方言红线**：MySQL 8 必须工作；GaussDB/Oracle/PG 路径不得回归（render_*_sql 有对应单测，跑全量 `cargo test`）。
4. **提交纪律**：每个 WP 独立小步提交，message 前缀 `perf(delta-diff): [WPn] ...`。改动仅限各自文件清单，避免合并冲突。
5. **测试**：`cargo test` 全绿 + 新增单测覆盖新逻辑。禁止引入新的重量级依赖；解析/算术自己写。
6. bench 复测命令（对照基线）：
   ```bash
   /usr/bin/time -l ./target/release/hepta_dbcli delta-diff \
     --config /tmp/hepta-bench/config.toml --left bench_a --right bench_b \
     --left-table src_t --right-table dst_t \
     --strategy <S> --consistency none --format json --sample 0
   ```

## WP1：iblt 自适应容量（self-sizing 两轮协议）

**动机**：默认容量 65536 对 d=100 场景浪费 ~256×；容量过小又直接回退 hashdiff 浪费已扫描成果。

**文件**：`dbcli/src/delta_diff/iblt_diff.rs`（主）、`dbcli/src/delta_diff/api.rs`（opts 透传）、`dbcli/src/delta_diff/cmd.rs`（flag）、`dbcli/src/backend/mod.rs` 与 `dbcli/src/backend/mysql/dialect.rs`（如需新增 render 变体）

**设计**：
1. 新 flag `--iblt-auto-capacity`（bool，默认 false）。开启时忽略 `--iblt-capacity`：
   - 第一轮 m₁=64（最小可行）。解码成功 → 直接返回（d≤48 场景一轮完成）。
   - 解码失败 → **从摘要 SQL 增产 d 的估计**：IBLT 摘要已含每桶 cnt；跨桶 |cnt| 之和的上界（或正 cnt 之和）是 d 的 2–4 倍内估计。取 d̂ = max(64, ceil(Σ|cnt| / 2))，第二轮 m₂ = clamp(3·d̂, 64, 上限)。
   - 至多两轮；两轮都失败 → 现有回退 hashdiff 路径不变。
2. `Summary` 已是 `HashMap<(u8,u64), Cell>`，直接在 subtract 后的 diff 表上算 Σ|cnt|。不需要改 SQL。
3. 预期：d=100 时第一轮 m=64 失败、Σ|cnt|≈200–400 → 第二轮 m≈600–1200 → RSS 从 280MB 降到 <10MB，墙钟从 ~40s 降到 <5s（省掉大表的桶扫描聚合代价是主项；两侧摘要 SQL 的扫描成本不变，但 m 决定 GROUP BY 桶数与传输行数）。
4. 单测：auto 容量的 d̂ 计算、两轮协议状态机（成功/失败/回退）、d̂ 上界正确性（构造 Cell）。

## WP2：bucketdiff 主键区间分桶（真 RBSR 化）

**动机**：mod 哈希桶不可索引，每桶 multiset 拉取都是全表扫描；94s 几乎全耗在此。

**文件**：`dbcli/src/delta_diff/bucket_diff.rs`（主）、`dbcli/src/delta_diff/checksum.rs`（如需 batch 变体）、`dbcli/src/backend/mod.rs`（新增 render 区间 checksum 方法时）及各方言实现

**设计**：
1. 桶键从 `MOD(hash, n)` 改为**主键有序区间**：利用 key domain [min,max]（现有 `estimate_rows`/key domain 已有），均分为 n 段 `pk ∈ [a_k, a_{k+1})`。int 主键直接用；现有单列整型键约束沿用。
2. 聚合 SQL 按 `FLOOR((pk - min)/seg_len)` 分组，**一次查询返回全部分桶**（与现 batch checksum 相同形态，只是分组键换成区间）。主键有序使 DB 可用 PK 索引流式聚合，消除逐桶重扫。
3. 下钻逻辑保留：diff 桶内用 `pk BETWEEN a AND b` 拉取 multiset/行级（现有 render_bucket_multiset_sql 改为区间 where）。
4. 保持 shards 语义（n 桶 = n shards）与报告结构不变。
5. 预期：clustered-100 场景从 ~94s → 与 hashdiff 同量级（≤5s）；行级拉取只针对 diff 区间。
6. 单测：区间划分边界（min=max、单桶、非整除）、diff 桶定位、multiset 语义保持。

## WP3：hashdiff chunk 聚合哈希下推（利用现有分段并行快筛）

**动机**：hashdiff 已经分段并行，但每段拉逐行哈希聚合。首筛段可下推为单行聚合值（cnt+XOR），段匹配时零行拉取。

**文件**：`dbcli/src/delta_diff/hash_diff.rs`（主）、`dbcli/src/delta_diff/checksum.rs`（新增 run_segment_aggregate）、方言 render（如需）

**设计**：
1. 快筛段（threads×8 首轮）SQL 从"逐行 (pk,hash) 分页拉取"改为"每段一行 (seg_id, cnt, xor_hash...)"（复用 ChecksumTuple 结构与 render_checksum_sql 形态，按段 where）。
2. 段匹配 → 直接标 Match（现逻辑的二分跳过相同）；段不匹配 → 现有二分/keyset 流程不变。
3. 保持 snapshot 一致性与 recheck 通道。并行结构（侧间并行）不变。
4. 预期：identical 场景接近"每侧 1 条聚合 SQL"极限（~0.5s/侧）；spread 场景快筛后二分深度不变但传输量大降。本场景基线已 2.1s，目标 ≤1.5s + 内存下降。若提升有限，重点记录 identical/large-table 外推收益。
5. 单测：段聚合解析、全匹配/部分匹配路径、ChecksumTuple 兼容。

## 集成与验收（coordinator 自做）

- 三 WP 合入后：`cargo test`、`cargo clippy -- -D warnings`（若 CI 有此门槛）、三场景回归 + identical/spread-101 补测、基线对比表。
- 若 WP 间有 api.rs/cmd.rs 冲突：coordinator 手工合并（预计只有 flag 注册行冲突）。

## 风险与回退

- 任一 WP 破坏正确性红线 → revert 该 WP 提交，不阻塞其他 WP。
- 方言 render 改动引发其他后端单测失败 → 该 WP 收敛为 MySQL-only 快路径 + 特性探测回退（Dialect 能力位已有先例：§16.3-F4/F8）。
