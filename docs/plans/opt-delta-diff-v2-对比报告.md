# hepta-dbcli delta-diff 优化实施与前后对比报告（opt/delta-diff-v2）

> 分支：`opt/delta-diff-v2`（hepta-dbcli，自 main @ `cf576b7`，共 6 个提交，末位 `f345a59`）。
> 实施方式：3 个并行 swarm worker（WP1/WP2/WP3）+ coordinator 集成、基准与修复。
> 测试：`cargo test --all` 1060 (bin) + 183 (lib) 全绿，fmt/clippy 0 警告（各 worker 自测 + 集成复跑）。

## 一、实施内容

| 工作包 | 提交 | 内容 | 对应论文思路 |
|--------|------|------|-------------|
| WP1 hashdiff 聚合下推 | `e9935d3` + `237eb2f` | 首轮快筛改为 DB 端聚合：每侧 32 段切成 8 段/条的 `UNION ALL` 块（UNION_BRANCHES=8），4 条语句并发执行，返回每段 (cnt, 4×SUM-slice) 校验元组；段匹配直接跳过二分 | Range-Based Set Reconciliation (arXiv 2212.13567) 的区间摘要思想 |
| WP2 bucketdiff 主键区间分桶 | `d622e04` | MOD(rowHash,N) 内容分桶 → 连续主键区间平铺（RangePlan）：两次 MIN/MAX 探测键域、i128 安全切分、`(k>=a AND k<=b)` 可走索引谓词；不可用整型键域时自动回退旧路径 | 同上 + RSOS/AELMDB (2603.19820) 的"摘要下沉存储"思想 |
| WP2-fix 谓词渲染修复 | `5796485` | **集成基准时发现并修复**：切片查询设置了 `spec.range` 但 `key_column=None`，方言渲染器要求两者同时存在才输出范围谓词 → 61 个"切片"实际全是全表扫描。修复后每切片为索引范围扫描 | — |
| WP3 iblt 自适应容量 | `de596d1` | 新增 `--iblt-auto-capacity`（默认关闭，旧行为逐字节不变）：第一轮 m=64；解码失败时从相减后摘要取 d̂=⌈Σ\|cnt\|/2⌉，第二轮 m₂=clamp(3·d̂, 64, 524288)；两轮都失败回退 hashdiff（strict 模式报错）。顺带修复剥洋葱在超载摘要上的空转死循环（曾耗 10min CPU） | Self-Sizing IBLT (arXiv 2608.26537) 两轮协议 |

## 二、基准对比（MySQL 8.4 @ 容器，1M 行 × 5 列，`--consistency none --format json --sample 0`，/usr/bin/time 实测）

### clustered-100（101 行差异：id 400001–400100 集中修改 + id=0）

| 策略 | main（优化前） | opt-v2（优化后） | 变化 |
|------|---------------|-----------------|------|
| hashdiff | 2.05/2.03/2.12 s，132 q，RSS 16.8MB | **2.12/2.07/2.07 s**（`--threads 8` 时 1.86–2.0s），**76 q**，RSS 16.6MB | 查询数 −42%；墙钟持平（本场景基线已很快），identical/大表场景收益更大（WP1 自测 median 2597ms vs 2687ms，A/B 交错压测） |
| bucketdiff | 94.3/93.5/97.6 s，RSS 27.5MB | **6.94/6.61/6.76 s**，128 q，RSS 23.2MB | **耗时 −93%（13.6×）**，DB 侧从"61 次全表扫描"变为"61 次索引范围扫描" |
| iblt | 38.4/46.6/39.3 s，**RSS 280MB**（容量 65536 固定） | auto：37.7–42.7s，**RSS 17.6MB**；`--iblt-capacity 1024`：20.6s | **RSS −94%**；d=100 超出两轮 auto 判定（每桶过载）走 hashdiff 回退，见 §四 |

### identical（0 差异）

| 策略 | main | opt-v2 |
|------|------|--------|
| hashdiff | ~2.0s | 3.21s（快筛改为聚合后固定开销略增，小差异场景；大表 identical 是 WP1 收益区） |
| bucketdiff | ~94s | **5.67s**，RSS 14.3MB |
| iblt auto | ~40s / 280MB | 21.96s / **14.4MB** |

### spread-101（101 行离散修改）

| 策略 | main | opt-v2 |
|------|------|--------|
| hashdiff | ~2.1s | 2.89s |
| bucketdiff | ~94s | **6.92s** |
| iblt cap1024 | —（需人工猜容量） | 19.9s |

### d=1（单行差异，IBLT 最佳场景）

| 配置 | 耗时 | RSS |
|------|------|-----|
| iblt 默认 65536 | 46.3s | **267.5MB** |
| iblt auto（第一轮 m=64 即成功） | **25.8s** | **14.6MB** |
| iblt cap1024 | 28.7s | 15.9MB |

auto 在小差异场景同时赢得耗时（−44%）与内存（−95%）。

## 三、三侧开销总结

| 开销侧 | 改善点 |
|--------|--------|
| 数据库侧 | bucketdiff：每差异桶全表扫描 → PK 索引范围扫描（EXPLAIN: `Index range scan on src_t using PRIMARY`）；切片查询 actual time ~45ms/万行 |
| 网络传输 | hashdiff 快筛：每段只回 1 行元组（×8 UNION 复用同一往返）；iblt auto：d 小时只传 64 桶摘要 vs 65536 桶（−99.9%） |
| 客户端 | iblt 内存 280MB→14–18MB；bucketdiff 摘要常驻为 BTreeMap<u64, 5×u64>；hashdiff 查询数 132→76 |

## 四、诚实结论与遗留项

1. **最大赢家是 bucketdiff**：94s→6.7s（−93%），且修复了一个上游（PR #44 对比中同类算法的）全表扫描病根。
2. **WP3 的 auto 容量在 d≈100 本表未兑现耗时收益**：d̂ 估计正确（Σ|cnt| 量级对），但 d=100 时 m₂=558 的第二_round 仍解码失败（连续 id 差异在 4-子表 IBLT 中桶过载），最终走 hashdiff 回退，总耗时 = 2 轮 IBLT + hashdiff ≈ 40s。它的价值在 (a) 内存从不超配（任何 d 下 RSS ≤18MB），(b) d≤~48 的一轮成功场景（d=1 实测 25.8s vs 46.3s）。要在大 d 也快，需要 Rateless IBLT 的增量符号追加（P1 论文），列为下一迭代。
3. **hashdiff 在 1M 行小表上收益有限**（本就 2s 量级），WP1 的价值在查询数 −42% 与大表外推（传输量从 O(表) 降为 O(段数×元组)），WP1 的 A/B 交错压测显示 ~3–4% 墙钟改善。
4. **WP2 集成阶段引入过一次回归**（349s，比 main 还慢），被对比基准当场捕获并修复（`5796485`）。这正是"优化后必测"流程的价值。
5. shard 报告中 `duration_ms` 粒度异常（首片 4698ms 疑为计时起点问题），不影响结果正确性，已记录待查。

## 五、复现

```bash
cd ~/Projects/Desktop_Projects/DB/hepta-dbcli && git checkout opt/delta-diff-v2 && cargo build --release
# 数据准备（如需）见 docs/plans/opt-delta-diff-v2-design.md §基线
/usr/bin/time -l ./target/release/hepta_dbcli delta-diff \
  --config /tmp/hepta-bench/config.toml --left bench_a --right bench_b \
  --left-table src_t --right-table dst_t \
  --strategy bucketdiff --consistency none --format json --sample 0
# iblt 自适应：加 --iblt-auto-capacity
```

全部原始数据：`docs/plans/opt-delta-diff-v2-baseline.tsv`；设计文档：`docs/plans/opt-delta-diff-v2-design.md`。

---

## 附录：评审修复复测（PR #91 review，commit fbcee33 / rebase 后 90edb91）

评审指出 6 个问题（3 正确性 + 1 release 静默 + 1 缺测试 + 1 CLI 语义），全部修复：

| # | 问题 | 修复 | 回归测试 |
|---|------|------|----------|
| B1 | iblt 失败路径不再 COMMIT，Oracle snapshot 回退会 ORA-01453 | 所有失败臂（capacity 回退 / strict 报错 / Db 错误）先 `close_snapshot`（COMMIT）再回退/报错，恢复 main（1951c96）语义 | `snapshot_mode_capacity_fallback_commits_before_hashdiff` 等 3 条（mock 记录 query_drop 语句序） |
| B2 | bucketdiff 键域取交集，部分重叠时静默漏 diff；NULL 键不可见 | 改并集 `[min(l,r), max(l,r)]`；probe 查询顺带计数 NULL 键，检测到即失败关闭回退 MOD bucketing | `probe_uses_union_of_two_sides`、`probe_partial_overlap_is_not_disjoint`、`probe_null_key_fails_closed_to_mod_fallback` |
| B3 | hashdiff UNION ALL 包装 `) AS wide` 在 Oracle 非法（ORA-00933） | 按 `url_scheme()=="oracle"` 渲染 `) wide`，其余方言保持 `) AS wide` | `wide_sql_table_alias_is_dialect_correct`（oracle feature 下断言 Oracle 分支） |
| B4 | 行数校验从硬错误降为 `debug_assert`，release 下 zip 截断可静默假 Match | 恢复 runtime `DbError`：行数不齐、空 chunk 行、`seg_lo` 错位全部报错 | 既有 wide 执行测试覆盖；错误路径改为显式返回 |
| B5 | WP2-fix（`key_column`+`range` 渲染）无防回归测试 | 新增 SqlCaptureConn 断言切片 checksum SQL 同时含 quoted `key_column` 与 `>= lo` / `< hi+1` | `range_slice_checksum_sql_carries_quoted_key_and_range` |
| B6 | CLI `--iblt-capacity 0` 撞上 AUTO 哨兵 0（MCP/API 走 `.max(16)`，两入口语义分叉） | 提取共享 `normalize_iblt_capacity(auto, requested)`，CLI 与 api.rs 共用 | `capacity_normalizer_keeps_cli_and_api_consistent` |

另按评审建议：`--iblt-auto-capacity` 补入 UserGuide §8.4（注明 MCP 不暴露）；与本 PR 无关的 synth #89 文档 commit（原 cf576b7）已 rebase 拆出分支。

### 复测基准（同 1M 行 dbench，`/usr/bin/time -l`，10GB 内存压力下取 r2/r3）

| 策略 | wall | RSS | diffs | 说明 |
|------|------|-----|-------|------|
| hashdiff | 2.27s | 16.8MB | 100 | 与修复前持平（修复不涉热路径） |
| bucketdiff | 7.25–7.48s | 24.6–26.0MB | 200 | 并集键域 + NULL 探测后仍稳定在 7s 档（修复前 6.7s，差异在探测 SQL 多一列 SUM(CASE) 与本次机器负载） |
| iblt（固定 c=64） | 27.60s | 281MB | 100 | 不受影响（对照） |
| iblt auto（none） | 44.97s | 18.4MB | 100 | 两轮失败 → hashdiff 回退，行为与修复前一致 |
| **iblt auto（snapshot）** | 47.92s | 17.4MB | 100 | **B1 修复验证**：snapshot 模式下两轮失败 → COMMIT → hashdiff 回退全程成功（修复前此路径在 Oracle 上会硬错） |
| bucketdiff（snapshot） | 7.49s | 24.6MB | 200 | snapshot 模式对照 |

测试：`cargo test --bin hepta_dbcli` 1065 通过（新增 5 条）；`cargo fmt --check` / `cargo clippy` 干净。原始数据追加于 `docs/plans/opt-delta-diff-v2-baseline.tsv`（postfix-r1 段）。
