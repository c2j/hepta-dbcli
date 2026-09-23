# synth report 基线重测：cardinality: modeled（issue #89 S1，2026-09-22）

承接 `2026-09-16-synth-report-baseline.md`（基线 2000 行 overall 0.742，target ≥0.85）。
S1 问题：**打开 `cardinality: modeled` 后基线夹具能到多少分？** 结论先行：
**到不了 0.85（2000 行 3 seeds 均值 0.667），且并不普遍优于 exact_rows**。
0.85 需要的是别的改进（见「缺口重定位」），modeled 只修 fk 分量。

## 复刻基线命令的差异

基线文档的命令链用 `rules-draft` 产出规则（含 relationship），因此 `fk` 维度被计分。
基线 0.742 与本次 exact 0.662 的差值来自 **fk 分量从 skipped 变为计分**：
same-shape 同分（child 0.326），加上 fk join rate 1.0 / cardinality_tv 0.2795 后
overall 被拉到 0.66。两代报告的口径差异，非回归。

## 实验设置

- 模型：`m1_verify_parent`(2000)/`m1_verify_child`(1000)，train `--sample 10000
  --categorical-top-k full`，email `sdtype: keep`（同基线）
- 两侧规则都带 relationship（draft 产出），唯一差异 `cardinality: exact_rows` vs
  `cardinality: modeled`
- generate --rows {500,1000,2000} --seed {7,11,23}，jsonl，然后 synth report

## 结果（overall）

| rows | seed | exact_rows | modeled |
|---:|---:|---:|---:|
| 500 | 7 | 0.674 | 0.641 |
| 500 | 11 | 0.648 | 0.653 |
| 500 | 23 | 0.651 | 0.662 |
| 1000 | 7 | 0.662 | 0.648 |
| 1000 | 11 | 0.658 | 0.652 |
| 1000 | 23 | 0.657 | 0.679 |
| 2000 | 7 | 0.662 | 0.673 |
| 2000 | 11 | 0.662 | 0.653 |
| 2000 | 23 | 0.655 | 0.676 |
| **均值** | | **0.659** | **0.660** |

## 分量对比（2000 行 seed 7）

| 分量 | exact_rows | modeled |
|---|---:|---:|
| child fk score | 1.0 | 1.0 |
| child fk cardinality_tv | **0.2795** | **0.0075** |
| child parent_id 1-ks（计分） | 0.326 | 0.356 |
| child shape | 0.326 | 0.356 |
| child pairs | 0.935 | 0.947 |
| child 总分 | 0.651 | 0.651 |
| parent 总分 | 0.695 | 0.695 |

## 结论

1. **modeled 修好了它该修的**：基数分布 TV 从 0.28 → 0.0075（join rate 双侧 1.0）。
2. **但 overall 没动**：`1-ks(parent_id)` 不受益——成块写入的 FK 值分布仍是父键值
   的重抽样，KS 对值分布形状敏感，两模式都在 0.33-0.36 的采样噪声带内。overall
   差值被 fk 分量外的 shape/pairs 噪声抵消。
3. **≥0.85 的真正拉低项重定位**（2000 行）：
   - child `parent_id` 1-ks 0.33-0.36（计分）：FK 值分布形状 vs 训练的采样地板，
     需要的是「FK 值分布对齐训练值集合的频次结构」（#117 的父行快照机制可复用），
     或者 report 对 FK 列改用更合适的度量（频次 TV 而非 KS）
   - parent datetime 两列 0.799/0.800（计分）：ECDF 自动选择不覆盖 datetime（M2 记录
     的取舍），S3 扩展可拿回约 0.2×2 列的份额
   - parent pairs 0.542：`cjje×id` 0.079、`cjje×whole_dec` 0.247、`id×whole_dec`
     0.057 —— copula 高斯假设下近独立列对的 pearson-delta 噪声地板问题
   - `note` 106 水平 1-tv 0.12（不计分，但 child shape 分量被其拉低感知）
4. **S2 方向修正**：优化 FK 列的采样策略（对齐训练频次结构）优先于扩大 modeled
   覆盖；modeled 已够用，不值得再投入。
5. 纯从 0.66 → 0.85 的量化缺口：child parent_id +0.6、datetime 两列 +0.4、
   pairs 地板 +0.4（粗算，均为计分项 1-ks/pearson-delta 的均值贡献）。

## 附：报告口径注意

- `synth report` 的 fk 维度只在规则含 relationship 时计分（skipped 时 overall 只含
  shape+pairs）。对比历史基线必须固定规则形态，否则口径不可比。
- exact_rows 下 fk cardinality_tv 0.2795 但 `warn: false`——阈值偏松，可在 #89 S4
  一并检查（不是本次目标）。
