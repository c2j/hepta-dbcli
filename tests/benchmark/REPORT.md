# Case A 复现报告（多数据库验证）

> 复现时间：2026-09-13 · 二进制：`target/release/hepta_dbcli --features synth` · seed=42 · 真实数据 2000×4（qty–amount ρ=0.7938）

## 结论

1. **hepta 相关性保真优于 SDV**：修复数值字符串识别后，openGauss/MySQL/PolarDB-X 三库 corr_error = 0.0020（SDV 0.0118），SDMetrics Quality 0.9916（SDV 0.9677）。
2. **三库逐位一致**：同一 CSV、同 seed 下，openGauss/MySQL/PolarDB-X 生成数据指标完全相同——跨后端类型映射（含 DECIMAL→字符串）修复后，训练只取决于数据本身。
3. **qty 无负值、范围正确**：整数取整 + min/max 裁剪后各库 qty ∈ [1,24]（与真实一致），neg=0%。
4. **Oracle 受驱动上限限制**：oracle-rs 0.1.7 硬编码 prefetch=100 且 `has_more_rows` 恒 false、`fetch_more` 协议损坏，训练只能采样 100 行（quality 0.9617，仍可用）。已加 ignore 回归测试，驱动修复后移除。

## 指标表

| backend | rows | corr | corr_err | KS_qty | KS_amt | TV_cat | TV_rgn | quality | qty_rng | neg% | train_s |
|---------|-----:|-----:|---------:|-------:|-------:|-------:|-------:|--------:|---------|-----:|--------:|
| opengauss | 2000 | 0.7918 | 0.0020 | 0.014 | 0.026 | 0.017 | 0.007 | 0.9916 | [1,24] | 0% | 0.070 |
| mysql | 2000 | 0.7918 | 0.0020 | 0.014 | 0.026 | 0.017 | 0.007 | 0.9916 | [1,24] | 0% | 0.051 |
| polardbx | 2000 | 0.7918 | 0.0020 | 0.014 | 0.026 | 0.017 | 0.007 | 0.9916 | [1,24] | 0% | 0.222 |
| oracle* | 2000 | 0.7763 | 0.0175 | 0.023 | 0.038 | 0.132 | 0.077 | 0.9617 | [1,21] | 0% | 0.096 |
| SDV(norm) | 2000 | 0.7820 | 0.0118 | 0.014 | 0.022 | 0.083 | 0.116 | 0.9677 | [1,24] | 0% | 0.055 |

\* Oracle 训练采样受驱动 100 行上限（见下「发现的产品缺陷」）。

真实数据基准：corr=0.7938，qty∈[1,24]，neg=0%。

## 环境

| 数据库 | 版本 | 连接 |
|--------|------|------|
| openGauss | openGauss-lite 7.0.0-RC1（pagila 容器） | `pagila.toml`，schema gaussdb |
| MySQL | 8.4.10 | root@13306/verify |
| PolarDB-X | 5.4.19-SNAPSHOT（CN 8527） | polardbx_root@18527/verify |
| Oracle | 26ai Free 23.26.2 | system@1521/FREEPDB1 |
| SDV | 1.38.3 + sdmetrics | venv，`GaussianCopulaSynthesizer(default_distribution="norm")` |

## 复现中发现并修复的产品缺陷

1. **DECIMAL/NUMBER 被驱动序列化为 JSON 字符串**，profile 误判为 categorical → 训练出分类分布，高基数下相关性静默归零（MySQL/Oracle/PolarDB-X 首轮 corr≈0）。修复：profile 识别数值字符串为 numerical；PIT 相关性计算增加解析回退（含回归测试）。
2. **oracle-rs 0.1.7 静默截断 >100 行查询**：execute 硬编码 prefetch=100、`has_more_rows` 恒 false、`fetch_more` 协议损坏。影响 cli / MCP `execute_query` / synth train。已加 `#[ignore]` 回归测试记录缺陷；`drain_result` 按 `has_more_rows` 契约编写，驱动修复后自动生效。
3. **钥匙串自动迁移的无人值守摩擦**：首次连接成功后配置文件中明文密码被改写为 `"keyring"`，headless 场景后续读取钥匙串触发 GUI 授权弹窗导致 CLI 挂起。基准脚本用 `rewrite_tomls.sh` 保持明文规避；产品层面可考虑「TTY 检测或 `--no-keyring-migrate` 开关」。

## 产物

- `per-db 指标 json`：`/tmp/opencode/bench/per_db_metrics.json`
- SDV 基线：`sdv_metrics.json` / `sdv_out.csv`（种子适配 sdv 1.38 API：`_set_random_state`）
- 生成物 `gen_*.csv`、`real.csv` 不入库（可由脚本重建）

> 注：`real.csv` / `gen_*.csv` / `sdv_out.csv` 为运行产物，不入库；`case_a_real_data.py` 可重建 real.csv。
