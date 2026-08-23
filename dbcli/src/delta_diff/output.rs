// ─── delta-diff output: DiffReport → QueryResult 投影（§四 output 层）───
//
// 将差异样本投影为 QueryResult，复用 cli.rs::render_result 的
// table/csv/vertical 管道；JSON 走 serde 直出（报告结构非表格）。

use serde_json::Value;

use crate::backend::QueryResult;
use crate::delta_diff::report::{DiffReport, DiffRow, DiffStatus};

/// 差异样本投影：列 [key, status, left, right]
pub(crate) fn diffs_to_query_result(report: &DiffReport) -> QueryResult {
    let rows = report
        .sample_diffs
        .iter()
        .map(|d| {
            vec![
                d.key.clone(),
                Value::from(format!("{:?}", d.status)),
                d.left
                    .as_ref()
                    .map(|r| Value::from(format!("{r:?}")))
                    .unwrap_or(Value::Null),
                d.right
                    .as_ref()
                    .map(|r| Value::from(format!("{r:?}")))
                    .unwrap_or(Value::Null),
            ]
        })
        .collect::<Vec<_>>();
    let n = rows.len();
    QueryResult {
        columns: vec!["key".into(), "status".into(), "left".into(), "right".into()],
        rows,
        row_count: n,
    }
}

/// 汇总段投影：key/value 行
pub(crate) fn summary_to_query_result(report: &DiffReport) -> QueryResult {
    let s = &report.summary;
    let rows: Vec<Vec<Value>> = vec![
        kv("strategy", &report.strategy),
        kv("consistency", &report.consistency),
        kv("hash_algorithm", &report.hash_algorithm),
        kv(
            "left",
            &format!("{} ({})", report.left.connection, report.left.table),
        ),
        kv(
            "right",
            &format!("{} ({})", report.right.connection, report.right.table),
        ),
        kv_num("left_total", s.left_total),
        kv_num("right_total", s.right_total),
        kv_num("missing_left", s.missing_left),
        kv_num("missing_right", s.missing_right),
        kv_num("modified", s.modified),
        kv("diff_rate", &format!("{:.4}%", s.diff_rate * 100.0)),
        kv_num("queries_total", report.perf.queries_total),
    ];
    QueryResult {
        columns: vec!["metric".into(), "value".into()],
        row_count: rows.len(),
        rows,
    }
}

fn kv(k: &str, v: &str) -> Vec<Value> {
    vec![Value::from(k), Value::from(v)]
}

fn kv_num(k: &str, v: u64) -> Vec<Value> {
    vec![Value::from(k), Value::from(v)]
}

pub(crate) fn render_compact_sample(report: &DiffReport, sample: usize, wide: bool) -> String {
    let total = report.sample_diffs.len();
    if total == 0 {
        return String::new();
    }
    let n = if sample == 0 {
        total
    } else {
        sample.min(total)
    };
    let rows = &report.sample_diffs[..n];
    let key_len = report.key_columns.len();
    let value_len = report.value_columns.len();
    let val_idxs: Vec<usize> = if wide {
        (0..value_len).collect()
    } else {
        visible_value_indices(rows, key_len, value_len)
    };

    let mut headers = vec!["status".to_string()];
    if report.key_columns.is_empty() {
        headers.push("key".into());
    } else {
        headers.extend(report.key_columns.iter().cloned());
    }
    for i in &val_idxs {
        headers.push(report.value_columns[*i].clone());
    }

    let mut table: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for row in rows {
        let mut cells = vec![status_terminal(row.status).to_string()];
        cells.extend(key_cells(row, &report.key_columns));
        for i in &val_idxs {
            cells.push(value_cell(row, key_len, *i, wide));
        }
        table.push(cells);
    }

    let widths = col_widths(&headers, &table);
    let mut out = format!("sample diffs ({n} of {total}):\n");
    out.push_str(&align_row(&headers, &widths));
    out.push('\n');
    for row in &table {
        out.push_str(&align_row(row, &widths));
        out.push('\n');
    }
    if n < total {
        out.push_str(&format!("showing {n} of {total} — --export out.csv\n"));
    }
    out
}

fn status_terminal(s: DiffStatus) -> &'static str {
    match s {
        DiffStatus::MissingLeft => "+ right",
        DiffStatus::MissingRight => "+ left",
        DiffStatus::Modified => "~",
    }
}

fn visible_value_indices(rows: &[DiffRow], key_len: usize, value_len: usize) -> Vec<usize> {
    if value_len == 0 {
        return Vec::new();
    }
    let mut seen = vec![false; value_len];
    for row in rows {
        match row.status {
            DiffStatus::Modified => {
                for i in changed_value_indices(row, key_len, value_len) {
                    seen[i] = true;
                }
            }
            DiffStatus::MissingLeft | DiffStatus::MissingRight => {
                seen.fill(true);
            }
        }
    }
    seen.iter()
        .enumerate()
        .filter(|(_, on)| **on)
        .map(|(i, _)| i)
        .collect()
}

fn changed_value_indices(row: &DiffRow, key_len: usize, value_len: usize) -> Vec<usize> {
    (0..value_len)
        .filter(|&i| {
            let l = row.left.as_ref().and_then(|r| r.get(key_len + i));
            let r = row.right.as_ref().and_then(|r| r.get(key_len + i));
            l != r
        })
        .collect()
}

fn key_cells(row: &DiffRow, key_names: &[String]) -> Vec<String> {
    if key_names.is_empty() {
        return vec![trunc32(&keyless_label(row))];
    }
    let src = row.left.as_ref().or(row.right.as_ref());
    key_names
        .iter()
        .enumerate()
        .map(|(i, _)| {
            let raw = src
                .and_then(|r| r.get(i))
                .cloned()
                .or_else(|| match &row.key {
                    Value::Array(a) => a.get(i).cloned(),
                    v if i == 0 => Some(v.clone()),
                    _ => None,
                });
            trunc32(&raw.as_ref().map(cell_str).unwrap_or_default())
        })
        .collect()
}

fn keyless_label(row: &DiffRow) -> String {
    row.left
        .as_ref()
        .or(row.right.as_ref())
        .and_then(|r| r.first())
        .map(cell_str)
        .unwrap_or_else(|| cell_str(&row.key))
}

fn value_cell(row: &DiffRow, key_len: usize, value_idx: usize, wide: bool) -> String {
    let l = row.left.as_ref().and_then(|r| r.get(key_len + value_idx));
    let r = row.right.as_ref().and_then(|r| r.get(key_len + value_idx));
    match row.status {
        DiffStatus::Modified => {
            if l != r {
                trunc32(&format!(
                    "{} → {}",
                    l.map(cell_str).unwrap_or_default(),
                    r.map(cell_str).unwrap_or_default()
                ))
            } else if wide {
                trunc32(&l.or(r).map(cell_str).unwrap_or_default())
            } else {
                String::new()
            }
        }
        DiffStatus::MissingLeft => trunc32(&r.map(cell_str).unwrap_or_default()),
        DiffStatus::MissingRight => trunc32(&l.map(cell_str).unwrap_or_default()),
    }
}

fn cell_str(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn trunc32(s: &str) -> String {
    let n = s.chars().count();
    if n <= 32 {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(32).collect::<String>())
    }
}

fn col_widths(headers: &[String], rows: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    widths
}

fn align_row(cells: &[String], widths: &[usize]) -> String {
    cells
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let w = widths.get(i).copied().unwrap_or(0);
            format!("{c:<w$}")
        })
        .collect::<Vec<_>>()
        .join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta_diff::report::*;
    use chrono::Utc;

    fn report_with_diff() -> DiffReport {
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
            strategy: "hashdiff".into(),
            consistency: "snapshot".into(),
            hash_algorithm: "md5".into(),
            summary: DiffSummary {
                left_total: 10,
                right_total: 9,
                missing_left: 0,
                missing_right: 1,
                modified: 0,
                diff_rate: 0.1,
            },
            perf: PerfMetrics::default(),
            shards: vec![],
            sample_diffs: vec![DiffRow {
                key: Value::from(5),
                left: Some(vec![Value::from(5), Value::from("x")]),
                right: None,
                status: DiffStatus::MissingRight,
                confirmed: true,
            }],
            warnings: vec![],
            key_columns: vec![],
            value_columns: vec![],
            ident_quote: '"',
            backslash_escape: false,
        }
    }

    #[test]
    fn summary_projection_has_metrics() {
        let qr = summary_to_query_result(&report_with_diff());
        assert!(qr.row_count >= 10);
        assert!(qr.rows.iter().any(|r| r[0] == "modified" && r[1] == 0));
        assert!(qr.rows.iter().any(|r| r[0] == "missing_right" && r[1] == 1));
    }

    #[test]
    fn diffs_projection_shape() {
        let qr = diffs_to_query_result(&report_with_diff());
        assert_eq!(qr.columns, vec!["key", "status", "left", "right"]);
        assert_eq!(qr.row_count, 1);
        assert_eq!(qr.rows[0][1], Value::from("MissingRight"));
    }

    #[test]
    fn diffs_projection_preserves_composite_key_array() {
        let mut report = report_with_diff();
        report.sample_diffs[0].key = serde_json::json!([1, "t"]);
        let qr = diffs_to_query_result(&report);
        assert_eq!(qr.rows[0][0], serde_json::json!([1, "t"]));
    }

    fn keyed_report() -> DiffReport {
        let mut report = report_with_diff();
        report.key_columns = vec!["xwdm".into(), "security_id".into()];
        report.value_columns = vec!["cjsl".into(), "yhs".into()];
        report.summary.missing_left = 1;
        report.summary.missing_right = 1;
        report.summary.modified = 1;
        report.sample_diffs = vec![
            DiffRow {
                key: serde_json::json!(["59267", "600000"]),
                left: None,
                right: Some(vec![
                    Value::from("59267"),
                    Value::from("600000"),
                    Value::from(100),
                    Value::from(0.5),
                ]),
                status: DiffStatus::MissingLeft,
                confirmed: true,
            },
            DiffRow {
                key: serde_json::json!(["59267", "600001"]),
                left: Some(vec![
                    Value::from("59267"),
                    Value::from("600001"),
                    Value::from(10),
                    Value::from(0.1),
                ]),
                right: Some(vec![
                    Value::from("59267"),
                    Value::from("600001"),
                    Value::from(12),
                    Value::from(0.1),
                ]),
                status: DiffStatus::Modified,
                confirmed: true,
            },
            DiffRow {
                key: serde_json::json!(["59267", "600002"]),
                left: Some(vec![
                    Value::from("59267"),
                    Value::from("600002"),
                    Value::from(8),
                    Value::from(0.1),
                ]),
                right: None,
                status: DiffStatus::MissingRight,
                confirmed: true,
            },
        ];
        report
    }

    #[test]
    fn compact_terminal_shows_real_columns_not_debug() {
        let out = render_compact_sample(&keyed_report(), 20, false);
        assert!(out.contains("cjsl"), "{out}");
        assert!(out.contains("10 → 12"), "{out}");
        assert!(!out.contains("String("), "{out}");
        assert!(!out.contains("Number("), "{out}");
    }

    #[test]
    fn compact_terminal_modified_hides_unchanged_unless_wide() {
        let mut report = keyed_report();
        report
            .sample_diffs
            .retain(|d| d.status == DiffStatus::Modified);
        let slim = render_compact_sample(&report, 20, false);
        assert!(!slim.contains("yhs"), "{slim}");
        let wide = render_compact_sample(&report, 20, true);
        assert!(wide.contains("yhs"), "{wide}");
    }

    #[test]
    fn compact_terminal_truncates_at_32_chars() {
        let mut report = keyed_report();
        report.sample_diffs[0].right = Some(vec![
            Value::from("59267"),
            Value::from("600000"),
            Value::from("abcdefghijklmnopqrstuvwxyz0123456789"),
            Value::from(0.5),
        ]);
        let out = render_compact_sample(&report, 20, true);
        assert!(out.contains('…'), "{out}");
        assert!(
            !out.contains("abcdefghijklmnopqrstuvwxyz0123456789"),
            "{out}"
        );
    }

    #[test]
    fn compact_terminal_status_symbols() {
        let out = render_compact_sample(&keyed_report(), 20, false);
        assert!(out.contains("+ right"), "{out}");
        assert!(out.contains("+ left"), "{out}");
        assert!(out.contains('~'), "{out}");
    }

    #[test]
    fn compact_terminal_footer_when_truncated() {
        let out = render_compact_sample(&keyed_report(), 1, false);
        assert!(out.contains("sample diffs (1 of 3)"), "{out}");
        assert!(out.contains("showing 1 of 3"), "{out}");
        assert!(out.contains("--export"), "{out}");
    }
}
