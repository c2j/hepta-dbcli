# P1 SynMeter Benchmark Report

UCI Adult; deterministic seed-42 70/15/15 split; five repetitions (seeds 0–4).
MLA uses lr/rf/mlp/tree/svm/xgboost/cat_boost on CPU, synthetic train/val and real test.

| Gate | Metric | hepta | SDV-GC | Ratio | Threshold | Result |
|---|---|---:|---:|---:|---:|---|
| G1 | cont_error | 0.0325091 | 0.032003 | 1.01582 | <= 1.15 | PASS |
| G2 | cat_error | 0.0628667 | 0.0983259 | 0.63937 | <= 1.15 | PASS |
| G3 | fidelity_5_key_mean | 0.0836426 | 0.10405 | 0.80387 | <= 1.15 | PASS |
| G4 | mla_7_model_relative_loss_mean | 0.179058 | 0.163485 | 1.09526 | <= 1.15 | PASS |
| G5 | query_error_3_way | 0.00238334 | 0.00264614 | 0.900687 | <= 1.15 | PASS |
| G6 | payment_amount_on_grid_ratio | — | — | — | >= 0.95 | SKIPPED |
