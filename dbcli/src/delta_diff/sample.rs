// ─── delta-diff sample: 列级变化直方图 + 终端抽样选择器（issue #79）────

use std::collections::HashSet;

use serde_json::Value;

use crate::delta_diff::cmd::SampleMode;
use crate::delta_diff::report::{ColumnChangeCount, DiffReport, DiffRow, DiffStatus, RowPayload};

/// Modified 行按「变化列」聚合计数（issue #79 §4.1）。
/// 判定复用 cells_differ（数值标度对齐，D9）；HashCount / 无比对列 / 无 Modified 行 → None（D6）。
pub(crate) fn compute_modified_columns(report: &DiffReport) -> Option<Vec<ColumnChangeCount>> {
    if report.row_payload == RowPayload::HashCount || report.value_columns.is_empty() {
        return None;
    }
    let key_len = report.key_columns.len();
    let value_len = report.value_columns.len();
    let mut counts = vec![0u64; value_len];
    let mut any = false;
    for row in &report.sample_diffs {
        if row.status != DiffStatus::Modified {
            continue;
        }
        for i in changed_value_indices(row, report, key_len, value_len) {
            counts[i] += 1;
            any = true;
        }
    }
    if !any {
        return None;
    }
    let mut out: Vec<ColumnChangeCount> = counts
        .into_iter()
        .enumerate()
        .filter(|(_, count)| *count > 0)
        .map(|(i, count)| ColumnChangeCount {
            name: report.value_columns[i].clone(),
            count,
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.name.cmp(&b.name)));
    Some(out)
}

pub(crate) fn value_data_type(report: &DiffReport, key_len: usize, value_idx: usize) -> &str {
    report
        .column_data_types
        .get(key_len + value_idx)
        .map(String::as_str)
        .unwrap_or("")
}

pub(crate) fn cells_differ(left: Option<&Value>, right: Option<&Value>, data_type: &str) -> bool {
    match (left, right) {
        (None, None) => false,
        (Some(left), Some(right)) => !crate::delta_diff::rowdiff::values_equal(
            left,
            right,
            crate::delta_diff::metadata::TablePlan::is_numeric_type(data_type),
        ),
        _ => true,
    }
}

pub(crate) fn changed_value_indices(
    row: &DiffRow,
    report: &DiffReport,
    key_len: usize,
    value_len: usize,
) -> Vec<usize> {
    (0..value_len)
        .filter(|&i| {
            let ty = value_data_type(report, key_len, i);
            let l = row.left.as_ref().and_then(|r| r.get(key_len + i));
            let r = row.right.as_ref().and_then(|r| r.get(key_len + i));
            cells_differ(l, r, ty)
        })
        .collect()
}

/// 终端抽样选择器（issue #79 D2/D3/D6/D7）。返回**升序**行下标（key 序展示）。
/// n == 0 或 n >= total → 全量下标（与今日 `--sample 0` / 足额语义一致）。
pub(crate) fn select_sample_indices(report: &DiffReport, n: usize, mode: SampleMode) -> Vec<usize> {
    let total = report.sample_diffs.len();
    if n == 0 || total <= n {
        return (0..total).collect();
    }
    match mode {
        SampleMode::Prefix => (0..n).collect(),
        SampleMode::Diverse => diverse_indices(report, n),
    }
}

fn diverse_indices(report: &DiffReport, n: usize) -> Vec<usize> {
    let total = report.sample_diffs.len();
    let column_aware =
        report.row_payload != RowPayload::HashCount && !report.value_columns.is_empty();
    let key_len = report.key_columns.len();
    let value_len = report.value_columns.len();

    // 单次 O(N·C) 扫描：status 配额候选 + 变化列签名代表（D3）
    let mut quota: [Option<usize>; 3] = [None; 3]; // [MissingLeft, MissingRight, Modified]
    let mut sig_reps: Vec<usize> = Vec::new();
    let mut seen_sigs: HashSet<Vec<u64>> = HashSet::new();

    for (idx, row) in report.sample_diffs.iter().enumerate() {
        match row.status {
            DiffStatus::MissingLeft => {
                if quota[0].is_none() {
                    quota[0] = Some(idx);
                }
            }
            DiffStatus::MissingRight => {
                if quota[1].is_none() {
                    quota[1] = Some(idx);
                }
            }
            DiffStatus::Modified => {
                if quota[2].is_none() {
                    quota[2] = Some(idx);
                }
                if !column_aware {
                    continue;
                }
                let changed = changed_value_indices(row, report, key_len, value_len);
                let sig = bitmask(&changed, value_len);
                if seen_sigs.insert(sig) {
                    sig_reps.push(idx);
                }
            }
        }
    }

    // 预算内装配：配额 → 新签名代表 → 原序回填（D3/D7）
    let mut picked: Vec<usize> = quota.into_iter().flatten().collect();
    let mut chosen: HashSet<usize> = picked.iter().copied().collect();
    for idx in &sig_reps {
        if picked.len() >= n {
            break;
        }
        if chosen.insert(*idx) {
            picked.push(*idx);
        }
    }
    if picked.len() < n {
        for idx in 0..total {
            if picked.len() >= n {
                break;
            }
            if chosen.insert(idx) {
                picked.push(idx);
            }
        }
    }
    picked.truncate(n);
    picked.sort_unstable();
    picked
}

fn bitmask(changed: &[usize], value_len: usize) -> Vec<u64> {
    let mut bits = vec![0u64; value_len / 64 + 1];
    for &i in changed {
        bits[i / 64] |= 1u64 << (i % 64);
    }
    bits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta_diff::report::*;
    use chrono::Utc;
    use serde_json::Value;

    fn skewed_rows() -> Vec<DiffRow> {
        let mut rows = Vec::new();
        for i in 0..30 {
            rows.push(mod_row(
                i,
                vec![
                    Value::from(i),
                    Value::from(1),
                    Value::from(2),
                    Value::from("d"),
                ],
                vec![
                    Value::from(i),
                    Value::from(9),
                    Value::from(8),
                    Value::from("d"),
                ],
            ));
        }
        rows.push(mod_row(
            30,
            vec![
                Value::from(30),
                Value::from(1),
                Value::from(2),
                Value::from("a"),
            ],
            vec![
                Value::from(30),
                Value::from(1),
                Value::from(2),
                Value::from("b"),
            ],
        ));
        rows
    }

    fn base_report() -> DiffReport {
        DiffReport {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            left: TableRef {
                connection: "l".into(),
                schema: None,
                table: "t".into(),
            },
            right: TableRef {
                connection: "r".into(),
                schema: None,
                table: "t".into(),
            },
            strategy: "keyeddiff".into(),
            consistency: "none".into(),
            hash_algorithm: "md5".into(),
            summary: DiffSummary::default(),
            perf: PerfMetrics::default(),
            shards: vec![],
            sample_diffs: vec![],
            warnings: vec![],
            row_payload: RowPayload::Columns,
            key_columns: vec!["id".into()],
            value_columns: vec!["cjsl".into(), "yhs".into(), "cjrq".into()],
            column_data_types: vec![
                "varchar(19)".into(),  // key: id
                "NUMBER(15,2)".into(), // cjsl
                "NUMBER(15,2)".into(), // yhs
                "varchar(10)".into(),  // cjrq
            ],
            ident_quote: '"',
            ident_scheme: String::new(),
            backslash_escape: false,
            modified_columns: None,
        }
    }

    fn mod_row(id: i64, left: Vec<Value>, right: Vec<Value>) -> DiffRow {
        DiffRow {
            key: Value::from(id),
            left: Some(left),
            right: Some(right),
            status: DiffStatus::Modified,
            confirmed: true,
        }
    }

    #[test]
    fn histogram_counts_modified_rows_by_column_desc() {
        let mut r = base_report();
        let mut rows = Vec::new();
        for i in 0..20 {
            // 只改 cjsl：10 → 11
            rows.push(mod_row(
                i,
                vec![
                    Value::from(i),
                    Value::from(10),
                    Value::from(1),
                    Value::from("d"),
                ],
                vec![
                    Value::from(i),
                    Value::from(11),
                    Value::from(1),
                    Value::from("d"),
                ],
            ));
        }
        for i in 20..32 {
            // 只改 yhs
            rows.push(mod_row(
                i,
                vec![
                    Value::from(i),
                    Value::from(1),
                    Value::from(2),
                    Value::from("d"),
                ],
                vec![
                    Value::from(i),
                    Value::from(1),
                    Value::from(3),
                    Value::from("d"),
                ],
            ));
        }
        for i in 32..35 {
            // 只改 cjrq
            rows.push(mod_row(
                i,
                vec![
                    Value::from(i),
                    Value::from(1),
                    Value::from(2),
                    Value::from("a"),
                ],
                vec![
                    Value::from(i),
                    Value::from(1),
                    Value::from(2),
                    Value::from("b"),
                ],
            ));
        }
        // 1 行数值标度假差异：cjsl 12150.0 vs 12150（NUMBER 类型 → 不算变化，D9）
        rows.push(mod_row(
            99,
            vec![
                Value::from(99),
                serde_json::json!(12150.0),
                Value::from(2),
                Value::from("d"),
            ],
            vec![
                Value::from(99),
                Value::from(12150),
                Value::from(2),
                Value::from("d"),
            ],
        ));
        r.sample_diffs = rows;

        let got = compute_modified_columns(&r).expect("histogram");
        assert_eq!(got.len(), 3, "{got:?}");
        assert_eq!(got[0].name, "cjsl");
        assert_eq!(got[0].count, 20);
        assert_eq!(got[1].name, "yhs");
        assert_eq!(got[1].count, 12);
        assert_eq!(got[2].name, "cjrq");
        assert_eq!(got[2].count, 3);
    }

    #[test]
    fn histogram_none_without_modified_rows() {
        let mut r = base_report();
        r.sample_diffs = vec![DiffRow {
            key: Value::from(1),
            left: None,
            right: Some(vec![
                Value::from(1),
                Value::from(1),
                Value::from(2),
                Value::from("d"),
            ]),
            status: DiffStatus::MissingRight,
            confirmed: true,
        }];
        assert!(compute_modified_columns(&r).is_none());
    }

    #[test]
    fn histogram_none_for_hash_count_payload() {
        let mut r = base_report();
        r.row_payload = RowPayload::HashCount;
        r.key_columns.clear();
        r.value_columns.clear();
        r.column_data_types.clear();
        r.sample_diffs = vec![mod_row(
            1,
            vec![
                Value::from(1),
                Value::from(1),
                Value::from(2),
                Value::from("d"),
            ],
            vec![
                Value::from(1),
                Value::from(9),
                Value::from(2),
                Value::from("d"),
            ],
        )];
        assert!(compute_modified_columns(&r).is_none()); // D6：keyless 无列可比
    }

    #[test]
    fn select_prefix_matches_key_order_prefix() {
        let mut r = base_report();
        r.value_columns.clear();
        r.column_data_types.clear();
        r.sample_diffs = (0..30)
            .map(|i| DiffRow {
                key: Value::from(i),
                left: Some(vec![Value::from(i)]),
                right: None,
                status: DiffStatus::MissingRight,
                confirmed: true,
            })
            .collect();
        assert_eq!(
            select_sample_indices(&r, 3, SampleMode::Prefix),
            vec![0, 1, 2]
        );
        assert_eq!(select_sample_indices(&r, 0, SampleMode::Prefix).len(), 30); // 0 = 全量
        assert_eq!(select_sample_indices(&r, 50, SampleMode::Prefix).len(), 30);
        // 超额 = 全量
    }

    #[test]
    fn select_diverse_covers_all_column_shapes_within_budget() {
        // 循环 2：大量 {cjsl} + 各 1 条 {yhs} / {cjrq}，预算 3 → 三种列都出现
        let mut r = base_report();
        let mut rows = Vec::new();
        for i in 0..30 {
            rows.push(mod_row(
                i,
                vec![
                    Value::from(i),
                    Value::from(10),
                    Value::from(1),
                    Value::from("d"),
                ],
                vec![
                    Value::from(i),
                    Value::from(11),
                    Value::from(1),
                    Value::from("d"),
                ],
            ));
        }
        rows.push(mod_row(
            30,
            vec![
                Value::from(30),
                Value::from(1),
                Value::from(2),
                Value::from("d"),
            ],
            vec![
                Value::from(30),
                Value::from(1),
                Value::from(5),
                Value::from("d"),
            ],
        )); // yhs
        rows.push(mod_row(
            31,
            vec![
                Value::from(31),
                Value::from(1),
                Value::from(2),
                Value::from("a"),
            ],
            vec![
                Value::from(31),
                Value::from(1),
                Value::from(2),
                Value::from("b"),
            ],
        )); // cjrq
        r.sample_diffs = rows;
        let picked = select_sample_indices(&r, 3, SampleMode::Diverse);
        assert_eq!(picked.len(), 3);
        // cjsl-only 的签名代表 + yhs 行(30) + cjrq 行(31)
        assert!(picked.contains(&30), "{picked:?}");
        assert!(picked.contains(&31), "{picked:?}");
    }

    #[test]
    fn select_diverse_status_quota_includes_missing_and_modified() {
        // 循环 3：前 100 行 MissingRight，后面才有 Modified → 两种 status 都在
        let mut r = base_report();
        let mut rows: Vec<DiffRow> = (0..100)
            .map(|i| DiffRow {
                key: Value::from(i),
                left: Some(vec![
                    Value::from(i),
                    Value::from(1),
                    Value::from(2),
                    Value::from("d"),
                ]),
                right: None,
                status: DiffStatus::MissingRight,
                confirmed: true,
            })
            .collect();
        for i in 100..150 {
            rows.push(mod_row(
                i,
                vec![
                    Value::from(i),
                    Value::from(10),
                    Value::from(1),
                    Value::from("d"),
                ],
                vec![
                    Value::from(i),
                    Value::from(11 + (i % 3)),
                    Value::from(1),
                    Value::from("d"),
                ],
            ));
        }
        r.sample_diffs = rows;
        let picked = select_sample_indices(&r, 20, SampleMode::Diverse);
        assert!(
            picked.contains(&0),
            "leading MissingRight in quota: {picked:?}"
        );
        assert!(
            picked.iter().any(|&i| i >= 100),
            "Modified present: {picked:?}"
        );
    }

    #[test]
    fn select_diverse_signature_dedup_keeps_budget_for_rare_shape() {
        // 循环 4：30 行 {cjsl,yhs} + 1 行 {cjrq}，预算 20 → 罕见签名不能被挤掉
        let mut r = base_report();
        r.sample_diffs = skewed_rows();
        let picked = select_sample_indices(&r, 20, SampleMode::Diverse);
        assert_eq!(picked.len(), 20);
        assert!(
            picked.contains(&30),
            "rare signature row must be sampled: {picked:?}"
        );
    }

    #[test]
    fn select_diverse_hash_count_degenerates_to_status_then_order() {
        // 循环 7：HashCount 无列可比 → status 配额 + 原序回填
        let mut r = base_report();
        r.row_payload = RowPayload::HashCount;
        r.key_columns.clear();
        r.value_columns.clear();
        r.column_data_types.clear();
        let mut rows: Vec<DiffRow> = (0..50)
            .map(|i| DiffRow {
                key: Value::from(i),
                left: Some(vec![Value::from("h"), Value::from(1)]),
                right: Some(vec![Value::from("h"), Value::from(2)]),
                status: if i % 10 == 0 {
                    DiffStatus::Modified
                } else {
                    DiffStatus::MissingRight
                },
                confirmed: true,
            })
            .collect();
        rows[0].status = DiffStatus::Modified;
        rows[0].left = Some(vec![Value::from("h0"), Value::from(1)]);
        rows[0].right = Some(vec![Value::from("h0"), Value::from(2)]);
        r.sample_diffs = rows;
        let picked = select_sample_indices(&r, 3, SampleMode::Diverse);
        assert_eq!(picked, vec![0, 1, 2], "quota(Modified@0) + 原序回填");
    }

    #[test]
    fn select_diverse_preserves_key_order_display() {
        // 展示序恒为 key 序（升序下标），选择优先级只决定「选谁」
        let mut r = base_report();
        r.sample_diffs = (0..10)
            .map(|i| DiffRow {
                key: Value::from(i),
                left: Some(vec![
                    Value::from(i),
                    Value::from(1),
                    Value::from(2),
                    Value::from("d"),
                ]),
                right: Some(vec![
                    Value::from(i),
                    Value::from(3),
                    Value::from(2),
                    Value::from("d"),
                ]),
                status: DiffStatus::Modified,
                confirmed: true,
            })
            .collect();
        let picked = select_sample_indices(&r, 4, SampleMode::Diverse);
        assert_eq!(picked, vec![0, 1, 2, 3]);
    }
}
