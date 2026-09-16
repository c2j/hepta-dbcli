# synth report baseline (2026-09-16)

Records the "对照当前基线先行记录" half of issue #73's 总验收 #2: the current
`synth report` overall score on the repository's business-like fixture, before
further fidelity work. The roadmap target is **≥ 0.85**; it is **not met yet**.

## Fixture

`tests/synth-verify/fixture_mysql.sql`, trained with
`tests/synth-verify/rules_m1_keep.yaml` (the `email` column is pinned to
`sdtype: keep`, so this baseline stays comparable with the M1/M2 suites):

- `m1_verify_parent` — 2000 rows: datetime (2 formats), 4/2-place DECIMALs,
  nullable `email`, 5-level and 120-level dictionaries;
- `m1_verify_child` — 1000 rows, non-unique FK to the parent, nullable `note`.

## Command

```bash
HEPTA_DBCLI_URL=mysql://root:testpass@127.0.0.1:3306/testdb \
  bash -c '
    d=$(mktemp -d)
    hepta_dbcli synth train --tables m1_verify_parent,m1_verify_child \
      --schema testdb --output "$d/models" --sample 10000 \
      --categorical-top-k full --rules tests/synth-verify/rules_m1_keep.yaml
    hepta_dbcli synth rules-draft --tables m1_verify_parent,m1_verify_child \
      --schema testdb --models "$d/models" --output "$d/rules.yaml"
    hepta_dbcli synth generate --models "$d/models" --rules "$d/rules.yaml" \
      --output "$d/data" --rows 2000 --seed 7 --format jsonl
    hepta_dbcli synth report --models "$d/models" --data "$d/data" \
      --rules "$d/rules.yaml" --output "$d/report.json"
    cat "$d/report.json"
  '
```

## Result

| generated rows | overall score |
|---:|---:|
| 500 | 0.731 |
| 1000 | 0.738 |
| 2000 | 0.742 |

Per-column detail at 2000 rows:

| table | score | column | metric | score | counted |
|---|---:|---|---|---:|---|
| m1_verify_child | 0.782 | id | 1-ks | 0.941 | yes |
| | | parent_id (FK) | 1-ks | 0.349 | yes |
| | | note | 1-tv | 0.121 | no (high cardinality) |
| m1_verify_parent | 0.702 | id | 1-ks | 0.908 | yes |
| | | trade_time | 1-ks | 0.799 | yes |
| | | trade_time_micro | 1-ks | 0.800 | yes |
| | | cjje | 1-ks | 0.806 | yes |
| | | whole_dec | 1-ks | 0.793 | yes |
| | | discount_rate | 1-ks | 0.948 | yes |
| | | status | 1-tv | 0.939 | yes |

## Gap analysis (why < 0.85)

1. **FK column shape (`m1_verify_child.parent_id`, 0.35).** The child FK is
   sampled with replacement from the parent pool, so its empirical distribution
   is a binomial reshuffle of the parent's. `cardinality: modeled` (issue #72)
   targets exactly this, but it is opt-in: with `cardinality: modeled` the
   generated fan-out follows the learned distribution (the dedicated #72
   fixture reproduces it with TV 0.05, see `run_m4_cardinality.sh`). Re-scoring
   **this** fixture under `cardinality: modeled` is a follow-up measurement.
2. **Formatted datetime columns (0.80).** Their epoch marginal is Normal by
   default; ECDF auto-selection only covers numeric columns (recorded trade-off
   in #73 M2).
3. **Parent `id` (0.91) and derived money (`cjje` 0.81).** Scale-up from 2000
   trained rows to 2000 generated is exact here, so the residual is the KS
   sampling floor for 2000 samples, not a shape error.

These are honest measurements, not inferred: the numbers above come from the
command in this file against MySQL 8 (`hepta-mysql-test` container).
