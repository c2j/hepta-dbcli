# issue #89 S2b：FK 密度加权池（2026-09-22）

承接 `2026-09-22-issue-89-s1-modeled-remeasure.md` 与 S2a（commit `ad11d08`，
PSD 投影修复，全网格均值 0.659→0.751）。S1 结论明确剩余缺口第一位是
**FK 列 parent_id 1-ks ≈ 0.33**：父池均匀抽样，而子表训练数据集中在
1..700（父 id 范围 1..2000+），抽样分布与子表经验分布 KS 距离大。

## 根因

`build_rel_pools` 的 Projection/Generated 策略从父池**均匀**抽样；子表 FK 列
自己训练出的边际（此处 Ecdf 512 knots）被完全忽略。分布信息在训练时已经拿到，
生成时却扔掉了。

## 设计：`pool_strategy: !density`（无字段）

用**子表自己训练的 FK 列边际**给父池逐值加权。对排序后的池值 `v_i`，取
**边界中点质量**：

```text
mass_i = CDF((v_i + v_{i+1}) / 2) - CDF((v_{i-1} + v_i) / 2)
```

- 首尾边界 ±0.5（首边界用 `cdf_left`，Ecdf 原子质量不丢）
- 差值 floor 1e-9；全 floor 时 `weighted_index` 自然回退均匀（子表从未
  观察过父池范围时仍可抽）
- 窗外值（子表训练范围之外，如 701..2000）质量塌到 floor，实际不采——
  与子表训练经验一致，符合预期

离线 python 验证：KS 0.065（1-ks 0.935）vs 当前 0.33。

实现要点（Green 阶段踩坑）：`FkPool` 构造后，RelPool 的抽样策略最初沿用
表级 strategy（测试夹具默认 uniform），权重被完全忽略。密度池必须**自带
`SelectionStrategy::Weighted`**，与表级 strategy 解耦——这是探针实测发现的
行为缺陷，已写进 Density 分支实现。

## TDD 记录

Red（3 组测试，编译失败为合法 Red）：

1. `density_pool_strategy_parses`（rules.rs）：YAML `pool_strategy: !density`
   解析为 `PoolStrategy::Density`
2. `weighted_pool_draws_match_explicit_weights` +
   `weighted_pool_with_all_zero_weights_falls_back_to_uniform`（fk_pool.rs）：
   新构造器 `FkPool::from_weighted_values`；12k 抽样 share 10/12 ±0.02
3. `density_pool_strategy_tracks_child_trained_fk_distribution`（generator.rs）：
   端到端。子表 Ecdf 训在 1..100、父池 Uniform 1..201（200 keys）、2000 行：
   FK 均值必须 < 80（密度中点 ~50；均匀池中点 ~100）

Green：

- `rules.rs`：`PoolStrategy::Density` 变体（变体文档注释上移到 enum，
  否则 rustfmt 1.92-1.98 会把整个 enum 的紧凑 struct 变体展开成多行——
  波及人类已有行，探测确认后规避）
- `fk_pool.rs`：`from_weighted_values`（weight_sum 只累计正权重，
  0/负权重不影响回退判断）
- `generator.rs`：`build_rel_pools` 三元组 `(pool, rel_strategy, unique)`，
  Density 分支给 `SelectionStrategy::Weighted`；`density_weights` helper
  实现边界中点质量法

Refactor：clippy `cmp_owned` 用 `let first = Value::from(1);` 绑定解决，
无 `#[allow]`。人类已有测试零改动。

## 实测（基线夹具，!density，run3）

| rows | overall 均值（3 seeds） | parent_id 1-ks 均值 |
|---:|---:|---:|
| 500 | **0.9192** | 0.9181 |
| 1000 | **0.9016** | 0.9376 |
| 2000 | **0.8837** | 0.9413 |

对照（S2a 后、S2b 前，run2）：overall 0.7315-0.7813，parent_id 1-ks ≈ 0.33。

**2000 行 overall 0.884 ≥ 0.85 目标达成**；2000 行 3 seeds 全部 ≥ 0.875。

剩余（2000 行 seed 7 计分列）：trade_time/trade_time_micro 1-ks 0.870（S3，
datetime）、cjje 0.911（确定性列天花板，度量上限 0.945）、status 1-tv 0.927。
距 0.85 线已有余量，S3 是否执行视 S4/S5 验收策略决定。

## 复现

```bash
.scratch-s89/measure_s2b.sh .scratch-s89/run3   # train → draft → 注入 !density → 9 格 report
```
