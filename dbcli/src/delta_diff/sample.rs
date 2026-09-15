// ─── delta-diff sample: 列级变化直方图 + 终端抽样选择器（issue #79）────

use serde_json::Value;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta_diff::report::*;
    use chrono::Utc;
    use serde_json::Value;

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
}
