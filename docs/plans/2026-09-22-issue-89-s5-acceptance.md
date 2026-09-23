# issue #89 S5 总验收（2026-09-22）

结论先行：**0.85 质量线达标。基线夹具 2000 行 overall 均值 0.8847（3 seeds
全部 ≥ 0.875），≥ 0.85 目标线**。验收运行于分支 `feat/issue-117-cross-table-derive`
HEAD `ff3b9fb`，全部证据由当次新鲜 train → draft → generate → report 产出。

#89 关闭条件判定：质量线已达标，但 PR #120 复审在 S4 留有五项非阻塞缺口
（见文末「S4 验收补充」）。全部落地后 PR 以 `Closes #89` 关闭 issue；在那之前
PR 描述不得写 `Closes #89`，避免质量证据未齐就关闭跟踪。

## 验收配置

- 规则：rules-draft 产出 + `pool_strategy: !density`（S2b，commit `8132e1c`）
- 模型：`m1_verify_parent`/`m1_verify_child`，train `--sample 10000
  --categorical-top-k full`，email `sdtype: keep`（同基线）
- 网格：rows {500, 1000, 2000} × seed {7, 11, 23}，jsonl，`synth report`
- 复现：`.scratch-s89/acceptance_s5.sh .scratch-s89/s5`

## 结果（overall，S5 新鲜运行）

| rows | mean | min（3 seeds） | parent_id 1-ks mean |
|---:|---:|---:|---:|
| 500 | 0.9194 | 0.9158 | 0.918 |
| 1000 | 0.9012 | 0.8940 | 0.938 |
| **2000** | **0.8847** | **0.8752** | **0.941** |

分阶段对照（2000 行均值）：

| 阶段 | overall | 主要变化 |
|---|---:|---|
| 原始基线（09-16 文档） | 0.742 | — |
| S1 后（run2，S2a 前） | 0.732 | 测量确认 modeled 单独不够 |
| S2a 后（`ad11d08`） | 0.732→(同代码重测 0.732) | PSD 投影修复（网格均值 0.659→0.751） |
| **S2b 后（`8132e1c`）** | **0.8847** | FK 密度加权池，parent_id 1-ks 0.33→0.94 |

## 引用完整性与旧产物回归

- FK join rate **1.0**（641/641 hits，2000 行 seed 7），`warn: false`——密度
  加权没有破坏引用完整性
- 旧 rules（`tests/synth-verify/rules_m1_keep.yaml`，无 relationship 顶层键的
  旧式规则）在 HEAD 生成 + report 正常（overall 0.886）
- run2 旧模型（先前训练产物）在 HEAD 直接 load + generate 成功
- 新旧 model.json **schema 完全一致**，`version: 1` 未变（兼容承诺兑现）

## 质量门禁（HEAD）

- `cargo fmt --all -- --check` ✓
- `cargo clippy --all --all-targets` 0 warning ✓（含 `--features duckdb`）
- `cargo test --all` 1496 通过 ✓；`cargo test --all --features duckdb` 1592 通过 ✓

## 计分列余量（2000 行 seed 7，未达标项均为非阻塞）

- trade_time / trade_time_micro 1-ks 0.870（datetime；S3 未做，overall 已达标，
  按 S2 计划「测量显示不需要则记录不做结论」处理）
- cjje 1-ks 0.911（确定性派生列，度量上限 0.945）
- status 1-tv 0.927、note 1-tv 0.116（高基数分类，top-k 截断所致，不计分项）

## S3 决定

不做。依据：0.85 目标在 S2a+S2b 后已达成且 2000 行最小 seed 仍有 0.025 余量；
datetime 列 1-ks 0.870 已高于目标线本身，ECDF-epoch 扩展的预期收益（~+0.01
overall）不足以引入 marginal.rs 选择器行为变更的风险。留作后续 issue 跟踪。

## S4 验收补充（PR #120 复审五项非阻塞缺口，2026-09-23）

五项全部落地，逐项独立 commit，证据如下：

| 项 | 内容 | commit | 锚定测试 |
|---|---|---|---|
| ① | `train --no-cardinality` 跳过 FK 基数学习 | `b9817a7` | `should_skip_fk_cardinality_attachment_when_learning_is_disabled`（含 learn 控制组） |
| ② | rules-draft 追加建议注释（非 unique FK → `cardinality: modeled`；PII 列名 → `sdtype: pii`） | `b54d375` | 两个 Red：可解析性+双建议 / 无可建议时字节不变 |
| ③ | PK 形 FK 训练期 1:>1 fan-out WARN | `c866f8c` | `unique_fanout_warning_*`（含两个控制组） |
| ④ | 10 万父键基数建模计时 + 往返 TV 断言 | （本节下方 commit） | `bench_100k_parent_cardinality_modeling` |
| ⑤ | 四 provider PII 吞吐 + id_card 唯一性下限 | （同上） | `bench_pii_generation_throughput` |

④⑤ 落地形式：`#[ignore]` bench 测试（`dbcli/src/synth/mod.rs` 测试模块）+
`tests/synth-verify/run_bench_s4.sh` 包装脚本；`--ignored --nocapture` 手动跑，
不拖 CI。实测（debug profile，2026-09-23）：

- 基数建模：10 万父键长尾 fan-out，learn 96.6/106.8ms，sample 121.4/146.7ms，
  往返 TV(learned, sampled) = 0.0017（断言 ≤ 0.02）
- PII 吞吐（20 万值/provider）：email 394-544k values/s、phone 338-414k、
  name 356-516k、id_card 99-116k（唯一性 99,999+/100,000，断言 > 99k）

S4 完成后 #89 全部关闭条件齐备，PR 描述可写 `Closes #89`。
