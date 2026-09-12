# Synth Benchmark — Case A Reproduction

本目录包含 hepta-dbcli `synth` 子命令与 SDV `GaussianCopulaSynthesizer` 的公平对比（Case A）复现说明、运行脚本与报告模板。

## 背景

**Case A 定义**（见 PR #52 设计文档）：
- 单表、4 列：`qty` (int)、`amount` (numeric)、`category` (4值)、`region` (3值)
- 真实数据：2000 行，`qty` 与 `amount` 相关 ρ ≈ 0.79（高斯 Copula 生成）
- 对比双方：
  - hepta-dbcli `synth train | generate`（Rust，`--features synth`）
  - SDV `GaussianCopulaSynthesizer(default_distribution="norm")`（Python）
- 公平条件：
  - 同一 CSV、同一 2000 行
  - 两边数值边际均锁为 Normal（SDV `default_distribution="norm"`，hepta 只有 Normal）
  - 同一随机种子 `42`
  - 评测指标：Pearson 相关、KS、TV、SDMetrics QualityScore、墙钟、RSS

---

## 环境准备

### 1. hepta-dbcli（Rust）

```bash
# 编译（需 synth feature）
cd /Users/c2j/Projects/Desktop_Projects/DB/GaussDB_Heptadecagon/lib/hepta-dbcli
cargo build --features synth --release
# 二进制：target/release/hepta_dbcli
```

### 2. SDV（Python）

```bash
# 建议 Python 3.9–3.11（SDV 1.38.x 对 3.12+ wheel 支持有限）
python3 -m venv .venv-sdv
source .venv-sdv/bin/activate
pip install -U pip
pip install 'sdv>=1.17,<2' sdmetrics pandas scipy numpy
# 可选：pip install 'torch>=2.0'（仅若需 CTGAN/TVAE，本 Case 不需要）
```

### 3. 数据库（openGauss / pagila）

```bash
# 使用伞仓库现成的 pagila 容器
cd /Users/c2j/Projects/Desktop_Projects/DB/GaussDB_Heptadecagon/lib/ogagila
docker-compose up -d
# 等待健康检查通过（约 10s）
docker exec pagila gsql-pagila -c "SELECT 1;"
```

---

## 运行 Case A（一键）

```bash
cd /Users/c2j/Projects/Desktop_Projects/DB/GaussDB_Heptadecagon/lib/hepta-dbcli/tests/benchmark
chmod +x run_case_a.sh
./run_case_a.sh
```

脚本会：
1. 生成真实数据 CSV（`real.csv`，2000×4，ρ=0.79）
2. 灌入 pagila `gaussdb.bakeoff_t`
3. 运行 hepta `train → generate`（`--enforce-min-max-values`）
4. 运行 SDV `GaussianCopulaSynthesizer(norm)` + `sample(2000)`
5. 用 `sdmetrics` 打双边 QualityReport + 自定义指标（KS/TV/相关/范围）
5. 输出 `case_a_report.json` + 人类可读 `case_a_report.md`

---

## 关键文件

| 文件 | 说明 |
|------|------|
| `run_case_a.sh` | 一键跑完整流程（依赖 Docker、hepta 二进制、Python venv） |
| `case_a_real_data.py` | 生成真实数据（固定 seed=42，ρ=0.79 Copula） |
| `case_a_sdv.py` | SDV 端拟合+采样+评测 |
| `case_a_hepta.py` | hepta 端 train/generate+评测（调用 hepta 二进制） |
| `case_a_report.py` | 聚合双边指标，输出 JSON + Markdown 报告 |
| `case_a_report.json` | 机器可读结果（CI 可直接消费） |
| `case_a_report.md` | 人类可读报告（含表格、结论、可操作建议） |

---

## 指标定义

| 指标 | 计算方式 | 合格阈值（Case A） |
|------|----------|-------------------|
| `corr_qty_amount` | Pearson(r) | 误差 < 0.1（真实 0.79） |
| `ks_qty` / `ks_amount` | scipy.stats.ks_2samp | < 0.05 |
| `tv_category` / `tv_region` | 0.5 × Σ\|p−q\| | < 0.15 |
| `qty_min` / `qty_max` | min/max | 落在训练 `[1, 24]` 内，无负值 |
| `amount_min` / `amount_max` | min/max | 落在训练 `[-20, 90]` 内 |
| `QualityScore` | SDMetrics `QualityReport.get_score()` | ≥ 0.95 |
| `Column Shapes` | SDMetrics 子项 | ≥ 0.93 |
| `Column Pair Trends` | SDMetrics 子项 | ≥ 0.98 |
| `fit_time_s` / `sample_time_s` | wall clock | 仅记录，不设阈值 |

---

## 预期结果（P0+P1+P2 后）

| 指标 | 真实 | hepta (P0+P1+P2) | SDV (norm) |
|------|------|------------------|------------|
| corr | 0.794 | **0.79±0.02** | 0.80 |
| KS qty | 0 | <0.02 | 0.01 |
| KS amount | 0 | <0.03 | 0.03 |
| TV cat | 0 | <0.05 | 0.09 |
| TV region | 0 | <0.05 | 0.11 |
| qty range | [1, 24] | **[1, 24]** | [1, 24] |
| qty neg rate | 0 | **0%** | 0% |
| QualityScore | — | **≥0.95** | 0.97 |

若实际跑出数字在 ±10% 容差内，视为**复现通过**。

---

## CI 集成建议

```yaml
# .github/workflows/synth-benchmark.yml
name: Synth Benchmark (Case A)
on:
  workflow_dispatch:
  schedule:
    - cron: '0 2 * * 1'  # weekly
jobs:
  case-a:
    runs-on: ubuntu-latest
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v4
      - name: Build hepta (synth)
        run: |
          cd lib/hepta-dbcli
          cargo build --features synth --release
      - name: Start pagila
        run: |
          cd lib/ogagila
          docker-compose up -d
          sleep 15
      - name: Python deps
        run: |
          python3 -m venv .venv
          .venv/bin/pip install 'sdv>=1.17,<2' sdmetrics pandas scipy numpy
      - name: Run Case A
        run: |
          cd lib/hepta-dbcli/tests/benchmark
          ./run_case_a.sh
      - name: Upload report
        uses: actions/upload-artifact@v4
        with:
          name: case-a-report
          path: lib/hepta-dbcli/tests/benchmark/case_a_report.*
```

---

## 故障排查

| 现象 | 可能原因 | 解决 |
|------|----------|------|
| hepta `train` 连不上 | pagila 未就绪 / 端口映射 | `docker ps` 确认 5432 映射；`docker exec pagila gsql-pagila -c "SELECT 1;"` |
| `normal_ppf` 超时 | 二分查找过慢（极小 p） | 确认已用二分而非原 Acklam 实现（P0 已修复） |
| SDV 报 `ModuleNotFoundError: boto3` | 缺依赖 | `pip install boto3 botocore`（SDV 1.38 依赖） |
| 生成 qty 出现负值 | `--enforce-min-max-values` 未生效 | 确认 CLI 传了 `--enforce-min-max-values`（默认 true）；模型里有 `min/max` 字段 |
| 相关仍 ≈ 0 | `run_train` 未把 rows 传给 `build_model` | 检查 `mod.rs` `run_train` 是否传了 `&result.rows` |

---

## 版本记录

| 日期 | hepta 版本 | SDV 版本 | 备注 |
|------|------------|----------|------|
| 2026-09-13 | 0.4.5 + P0/P1/P2 | 1.38.3 | 初版 Case A，P0/P1/P2 全部落地 |