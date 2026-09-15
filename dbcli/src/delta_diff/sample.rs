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

/// 就地按选择器保留样本（MCP cap 替身；issue #79 D5）。n = 0 视为全量。
pub(crate) fn retain_sample(report: &mut DiffReport, n: usize, mode: SampleMode) {
    let indices = select_sample_indices(report, n, mode);
    let old = std::mem::take(&mut report.sample_diffs);
    report.sample_diffs = indices.into_iter().map(|i| old[i].clone()).collect();
}

fn diverse_indices(report: &DiffReport, n: usize) -> Vec<usize> {
    let total = report.sample_diffs.len();
    let column_aware =
        report.row_payload != RowPayload::HashCount && !report.value_columns.is_empty();
    let key_len = report.key_columns.len();
    let value_len = report.value_columns.len();

    // 单次 O(N·C) 扫描：各 status 首现位次 + Modified 行的变化列集合
    let mut quota: [Option<usize>; 3] = [None; 3]; // [Modified, MissingLeft, MissingRight]
    let mut modified: Vec<(usize, Vec<usize>)> = Vec::new();

    for (idx, row) in report.sample_diffs.iter().enumerate() {
        match row.status {
            DiffStatus::Modified => {
                if quota[0].is_none() {
                    quota[0] = Some(idx);
                }
                if column_aware {
                    let changed = changed_value_indices(row, report, key_len, value_len);
                    modified.push((idx, changed));
                }
            }
            DiffStatus::MissingLeft => {
                if quota[1].is_none() {
                    quota[1] = Some(idx);
                }
            }
            DiffStatus::MissingRight => {
                if quota[2].is_none() {
                    quota[2] = Some(idx);
                }
            }
        }
    }

    let mut picked: Vec<usize> = Vec::with_capacity(n);
    let mut chosen: HashSet<usize> = HashSet::new();
    let mut covered = vec![false; value_len];

    // 1) status 配额——Modified 优先：它是列形态的唯一载体，预算装不下全部
    //    status 时不能被 Missing 行挤掉
    for slot in quota.into_iter().flatten() {
        if picked.len() >= n {
            break;
        }
        if chosen.insert(slot) {
            picked.push(slot);
            if let Some((_, cols)) = modified.iter().find(|(i, _)| *i == slot) {
                for &c in cols {
                    covered[c] = true;
                }
            }
        }
    }

    // 2) set-cover——key 序扫 Modified，仅当覆盖尚未出现的列；重叠签名不得挤占稀有列的预算
    for (idx, cols) in &modified {
        if picked.len() >= n {
            break;
        }
        if chosen.contains(idx) {
            continue;
        }
        if cols.iter().any(|&c| !covered[c]) {
            chosen.insert(*idx);
            picked.push(*idx);
            for &c in cols {
                covered[c] = true;
            }
        }
    }

    // 3) 签名去重——同一变化列 bitmask 只留 1 个代表（不再提供新列，仅为形状样本）
    let mut seen_sigs: HashSet<Vec<u64>> = HashSet::new();
    for &p in &picked {
        if let Some((_, cols)) = modified.iter().find(|(i, _)| *i == p) {
            seen_sigs.insert(bitmask(cols, value_len));
        }
    }
    for (idx, cols) in &modified {
        if picked.len() >= n {
            break;
        }
        if chosen.contains(idx) {
            continue;
        }
        if seen_sigs.insert(bitmask(cols, value_len)) {
            chosen.insert(*idx);
            picked.push(*idx);
        }
    }

    // 4) 原序回填——剩余名额按 key 序补，避免样本全是罕见离群点
    for idx in 0..total {
        if picked.len() >= n {
            break;
        }
        if chosen.insert(idx) {
            picked.push(idx);
        }
    }

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
    fn select_diverse_set_cover_skips_overlapping_signatures_for_new_columns() {
        // 预算 2：row0 改 {cjsl,yhs}（配额），rows 1..10 只改 {cjsl}（新签名但零新列），
        // row11 只改 {cjrq}。重叠签名不得挤占预算——{cjrq} 的唯一覆盖者必须入选。
        let mut r = base_report();
        let mut rows = Vec::new();
        rows.push(mod_row(
            0,
            vec![
                Value::from(0),
                Value::from(1),
                Value::from(1),
                Value::from("d"),
            ],
            vec![
                Value::from(0),
                Value::from(2),
                Value::from(2),
                Value::from("d"),
            ],
        ));
        for i in 1..11 {
            rows.push(mod_row(
                i,
                vec![
                    Value::from(i),
                    Value::from(1),
                    Value::from(1),
                    Value::from("d"),
                ],
                vec![
                    Value::from(i),
                    Value::from(2),
                    Value::from(1),
                    Value::from("d"),
                ],
            ));
        }
        rows.push(mod_row(
            11,
            vec![
                Value::from(11),
                Value::from(1),
                Value::from(1),
                Value::from("x"),
            ],
            vec![
                Value::from(11),
                Value::from(1),
                Value::from(1),
                Value::from("y"),
            ],
        ));
        r.sample_diffs = rows;
        let picked = select_sample_indices(&r, 2, SampleMode::Diverse);
        assert_eq!(
            picked,
            vec![0, 11],
            "set-cover must reach the cjrq row: {picked:?}"
        );
    }

    #[test]
    fn select_diverse_quota_prefers_modified_when_budget_cannot_hold_all_statuses() {
        // 混合 status + 预算 2：Modified 是列形态的唯一载体，不能被 Missing 配额挤掉
        let mut r = base_report();
        let mut rows: Vec<DiffRow> = vec![DiffRow {
            key: Value::from(0),
            left: None,
            right: Some(vec![
                Value::from(0),
                Value::from(1),
                Value::from(2),
                Value::from("d"),
            ]),
            status: DiffStatus::MissingLeft,
            confirmed: true,
        }];
        for i in 1..31 {
            rows.push(DiffRow {
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
            });
        }
        rows.push(mod_row(
            31,
            vec![
                Value::from(31),
                Value::from(1),
                Value::from(2),
                Value::from("d"),
            ],
            vec![
                Value::from(31),
                Value::from(9),
                Value::from(2),
                Value::from("d"),
            ],
        ));
        r.sample_diffs = rows;
        let picked = select_sample_indices(&r, 2, SampleMode::Diverse);
        assert_eq!(picked, vec![0, 31], "Modified survives quota: {picked:?}");
        let picked1 = select_sample_indices(&r, 1, SampleMode::Diverse);
        assert_eq!(
            picked1,
            vec![31],
            "budget 1 still prefers Modified: {picked1:?}"
        );
    }

    #[test]
    fn retain_sample_replaces_rows_with_diverse_selection() {
        let mut r = base_report();
        r.sample_diffs = skewed_rows();
        retain_sample(&mut r, 20, SampleMode::Diverse);
        assert_eq!(r.sample_diffs.len(), 20);
        assert!(
            r.sample_diffs.iter().any(|row| row.key == 30),
            "rare signature must survive cap"
        );
        // 升序 = key 序
        let keys: Vec<i64> = r
            .sample_diffs
            .iter()
            .map(|row| row.key.as_i64().expect("key should be an integer"))
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn retain_sample_zero_keeps_all() {
        let mut r = base_report();
        r.sample_diffs = skewed_rows();
        retain_sample(&mut r, 0, SampleMode::Prefix);
        assert_eq!(r.sample_diffs.len(), 31);
    }

    #[test]
    fn select_diverse_hash_count_degenerates_to_status_then_order() {
        // HashCount 无列可比 → status 配额 + 原序回填；Modified 在尾部，prefix 选不到它
        let mut r = base_report();
        r.row_payload = RowPayload::HashCount;
        r.key_columns.clear();
        r.value_columns.clear();
        r.column_data_types.clear();
        let mut rows: Vec<DiffRow> = (0..49)
            .map(|i| DiffRow {
                key: Value::from(i),
                left: Some(vec![Value::from("h"), Value::from(1)]),
                right: Some(vec![Value::from("h"), Value::from(2)]),
                status: DiffStatus::MissingRight,
                confirmed: true,
            })
            .collect();
        rows.push(DiffRow {
            key: Value::from(49),
            left: Some(vec![Value::from("h49"), Value::from(1)]),
            right: Some(vec![Value::from("h49"), Value::from(2)]),
            status: DiffStatus::Modified,
            confirmed: true,
        });
        r.sample_diffs = rows;
        let picked = select_sample_indices(&r, 3, SampleMode::Diverse);
        assert_eq!(
            picked,
            vec![0, 1, 49],
            "quota(Modified@49 + MissingRight@0) + 原序回填: {picked:?}"
        );
    }

    #[test]
    fn select_diverse_preserves_key_order_display() {
        // 展示序恒为 key 序（升序下标）；row9 携带新列 {cjrq}，使选择集偏离 key 序前缀
        let mut r = base_report();
        let mut rows = Vec::new();
        for i in 0..9 {
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
                    Value::from(3),
                    Value::from(2),
                    Value::from("d"),
                ],
            ));
        }
        rows.push(mod_row(
            9,
            vec![
                Value::from(9),
                Value::from(1),
                Value::from(2),
                Value::from("d"),
            ],
            vec![
                Value::from(9),
                Value::from(3),
                Value::from(2),
                Value::from("y"),
            ],
        ));
        r.sample_diffs = rows;
        let picked = select_sample_indices(&r, 4, SampleMode::Diverse);
        assert_eq!(
            picked,
            vec![0, 1, 2, 9],
            "non-prefix selection rendered in key order: {picked:?}"
        );
    }
}
