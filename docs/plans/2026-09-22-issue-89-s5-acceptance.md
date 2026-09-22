# issue #89 S5 总验收（2026-09-22）

结论先行：**达标。基线夹具 2000 行 overall 均值 0.8847（3 seeds 全部 ≥ 0.875），
≥ 0.85 目标线**。验收运行于分支 `feat/issue-117-cross-table-derive` HEAD
`ff3b9fb`，全部证据由当次新鲜 train → draft → generate → report 产出。

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
