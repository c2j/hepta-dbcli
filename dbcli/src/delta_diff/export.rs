// ─── delta-diff full-diff file export (csv / jsonl / json) ─────────────

use serde_json::{json, Value};

use crate::delta_diff::cmd::ExportFormat;
use crate::delta_diff::report::{DiffReport, DiffRow, DiffStatus};

pub(crate) fn render_export(
    report: &DiffReport,
    format: ExportFormat,
    export_rows: bool,
) -> Result<String, String> {
    match format {
        ExportFormat::Csv => Ok(render_csv(report, export_rows)),
        ExportFormat::Jsonl => Ok(render_jsonl(report, export_rows)),
        ExportFormat::Json => {
            serde_json::to_string_pretty(report).map_err(|e| format!("json serialize: {e}"))
        }
        ExportFormat::Sql => Err("sql export is handled by sql_patch".into()),
    }
}

fn keyless_hash(row: &DiffRow) -> Value {
    if let Value::Object(o) = &row.key {
        return o.get("hash").cloned().unwrap_or(Value::Null);
    }
    row.left
        .as_ref()
        .or(row.right.as_ref())
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or_else(|| row.key.clone())
}

fn keyless_counts(row: &DiffRow) -> (Option<Value>, Option<Value>) {
    if let Value::Object(o) = &row.key {
        return (o.get("left").cloned(), o.get("right").cloned());
    }
    (
        row.left.as_ref().and_then(|r| r.get(1)).cloned(),
        row.right.as_ref().and_then(|r| r.get(1)).cloned(),
    )
}

fn status_export(s: DiffStatus) -> &'static str {
    match s {
        DiffStatus::MissingLeft => "missing_left",
        DiffStatus::MissingRight => "missing_right",
        DiffStatus::Modified => "modified",
    }
}

fn key_map(row: &DiffRow, report: &DiffReport) -> Value {
    if report.key_columns.is_empty() {
        return json!({ "hash": keyless_hash(row) });
    }
    let src = row.left.as_ref().or(row.right.as_ref());
    let mut obj = serde_json::Map::new();
    for (i, name) in report.key_columns.iter().enumerate() {
        let v = src
            .and_then(|r| r.get(i))
            .cloned()
            .or_else(|| match &row.key {
                Value::Array(a) => a.get(i).cloned(),
                v if i == 0 => Some(v.clone()),
                _ => None,
            })
            .unwrap_or(Value::Null);
        obj.insert(name.clone(), v);
    }
    Value::Object(obj)
}

fn side_value_map(row: &DiffRow, report: &DiffReport, left: bool) -> Value {
    let cells = if left {
        row.left.as_ref()
    } else {
        row.right.as_ref()
    };
    let Some(cells) = cells else {
        return Value::Null;
    };
    let mut obj = serde_json::Map::new();
    let key_len = report.key_columns.len();
    for (i, name) in report.value_columns.iter().enumerate() {
        obj.insert(
            name.clone(),
            cells.get(key_len + i).cloned().unwrap_or(Value::Null),
        );
    }
    Value::Object(obj)
}

fn changed_value_names(row: &DiffRow, report: &DiffReport) -> Vec<String> {
    let key_len = report.key_columns.len();
    report
        .value_columns
        .iter()
        .enumerate()
        .filter(|(i, _)| {
            let l = row.left.as_ref().and_then(|r| r.get(key_len + i));
            let r = row.right.as_ref().and_then(|r| r.get(key_len + i));
            l != r
        })
        .map(|(_, n)| n.clone())
        .collect()
}

fn union_changed_value_names(report: &DiffReport, export_rows: bool) -> Vec<String> {
    if export_rows {
        return report.value_columns.clone();
    }
    let mut seen = vec![false; report.value_columns.len()];
    let key_len = report.key_columns.len();
    for row in &report.sample_diffs {
        match row.status {
            DiffStatus::Modified => {
                for (i, on) in seen.iter_mut().enumerate() {
                    let l = row.left.as_ref().and_then(|r| r.get(key_len + i));
                    let r = row.right.as_ref().and_then(|r| r.get(key_len + i));
                    if l != r {
                        *on = true;
                    }
                }
            }
            DiffStatus::MissingLeft | DiffStatus::MissingRight => {
                seen.fill(true);
            }
        }
    }
    report
        .value_columns
        .iter()
        .enumerate()
        .filter(|(i, _)| seen[*i])
        .map(|(_, n)| n.clone())
        .collect()
}

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn csv_cell(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => csv_escape(s),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(other) => csv_escape(&other.to_string()),
    }
}

fn render_csv(report: &DiffReport, export_rows: bool) -> String {
    if report.key_columns.is_empty() && report.value_columns.is_empty() {
        return render_keyless_csv(report);
    }
    let changed = union_changed_value_names(report, export_rows);
    let mut headers = vec!["status".to_string()];
    headers.extend(report.key_columns.iter().cloned());
    for name in &changed {
        headers.push(format!("{name}_left"));
        headers.push(format!("{name}_right"));
    }
    let mut lines = vec![headers.join(",")];
    let key_len = report.key_columns.len();
    for row in &report.sample_diffs {
        let mut cells = vec![status_export(row.status).to_string()];
        let src = row.left.as_ref().or(row.right.as_ref());
        for i in 0..key_len {
            let v = src.and_then(|r| r.get(i)).or_else(|| match &row.key {
                Value::Array(a) => a.get(i),
                v if i == 0 => Some(v),
                _ => None,
            });
            cells.push(csv_cell(v));
        }
        for name in &changed {
            let idx = report
                .value_columns
                .iter()
                .position(|c| c == name)
                .unwrap_or(0);
            cells.push(csv_cell(
                row.left.as_ref().and_then(|r| r.get(key_len + idx)),
            ));
            cells.push(csv_cell(
                row.right.as_ref().and_then(|r| r.get(key_len + idx)),
            ));
        }
        lines.push(cells.join(","));
    }
    lines.join("\n") + "\n"
}

fn render_keyless_csv(report: &DiffReport) -> String {
    let mut lines = vec!["status,hash,left_count,right_count".to_string()];
    for row in &report.sample_diffs {
        let hash = keyless_hash(row);
        let (lc, rc) = keyless_counts(row);
        lines.push(format!(
            "{},{},{},{}",
            status_export(row.status),
            csv_cell(Some(&hash)),
            csv_cell(lc.as_ref()),
            csv_cell(rc.as_ref())
        ));
    }
    lines.join("\n") + "\n"
}

fn render_jsonl(report: &DiffReport, export_rows: bool) -> String {
    let mut lines = Vec::with_capacity(report.sample_diffs.len());
    for row in &report.sample_diffs {
        let mut obj = serde_json::Map::new();
        obj.insert("status".into(), Value::from(status_export(row.status)));
        obj.insert("key".into(), key_map(row, report));
        match row.status {
            DiffStatus::Modified => {
                let names = if export_rows {
                    report.value_columns.clone()
                } else {
                    changed_value_names(row, report)
                };
                let key_len = report.key_columns.len();
                let mut fields = serde_json::Map::new();
                for name in names {
                    let idx = report
                        .value_columns
                        .iter()
                        .position(|c| c == &name)
                        .unwrap_or(0);
                    fields.insert(
                        name,
                        json!({
                            "left": row.left.as_ref().and_then(|r| r.get(key_len + idx)).cloned().unwrap_or(Value::Null),
                            "right": row.right.as_ref().and_then(|r| r.get(key_len + idx)).cloned().unwrap_or(Value::Null),
                        }),
                    );
                }
                obj.insert("fields".into(), Value::Object(fields));
            }
            DiffStatus::MissingLeft => {
                obj.insert("right".into(), side_value_map(row, report, false));
            }
            DiffStatus::MissingRight => {
                obj.insert("left".into(), side_value_map(row, report, true));
            }
        }
        lines.push(Value::Object(obj).to_string());
    }
    if lines.is_empty() {
        String::new()
    } else {
        lines.join("\n") + "\n"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta_diff::report::*;
    use chrono::Utc;

    fn report() -> DiffReport {
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
            summary: DiffSummary {
                left_total: 2,
                right_total: 2,
                missing_left: 1,
                missing_right: 0,
                modified: 1,
                diff_rate: 1.0,
            },
            perf: PerfMetrics::default(),
            shards: vec![],
            sample_diffs: vec![
                DiffRow {
                    key: json!(["59267", "600000"]),
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
                    key: json!(["59267", "600001"]),
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
            ],
            warnings: vec![],
            key_columns: vec!["xwdm".into(), "security_id".into()],
            value_columns: vec!["cjsl".into(), "yhs".into()],
            ident_quote: '"',
            ident_scheme: String::new(),
            backslash_escape: false,
        }
    }

    #[test]
    fn csv_default_has_status_keys_and_changed_only() {
        let csv = render_csv(&report(), false);
        let header = csv.lines().next().unwrap();
        assert!(header.contains("status"), "{header}");
        assert!(header.contains("xwdm"), "{header}");
        assert!(header.contains("cjsl_left"), "{header}");
        assert!(header.contains("cjsl_right"), "{header}");
        assert_eq!(csv.lines().count(), 3);
    }

    #[test]
    fn csv_export_rows_splits_all_value_cols() {
        let csv = render_csv(&report(), true);
        let header = csv.lines().next().unwrap();
        assert!(header.contains("yhs_left"), "{header}");
        assert!(header.contains("yhs_right"), "{header}");
    }

    #[test]
    fn csv_keyless_default_is_hash_and_counts() {
        let mut r = report();
        r.key_columns.clear();
        r.value_columns.clear();
        r.sample_diffs = vec![DiffRow {
            key: Value::from("h"),
            left: Some(vec![Value::from("abc"), Value::from(0)]),
            right: Some(vec![Value::from("abc"), Value::from(1)]),
            status: DiffStatus::MissingLeft,
            confirmed: true,
        }];
        let csv = render_csv(&r, false);
        assert!(
            csv.starts_with("status,hash,left_count,right_count"),
            "{csv}"
        );
        assert!(csv.contains("missing_left,abc,0,1"), "{csv}");
    }

    #[test]
    fn jsonl_keyless_hash_survives_hydrate() {
        let mut r = report();
        r.key_columns.clear();
        r.value_columns = vec!["xwdm".into(), "cjsl".into()];
        r.sample_diffs = vec![DiffRow {
            key: json!({"hash":"abc123def456","left":0,"right":1}),
            left: None,
            right: Some(vec![Value::from("59267"), Value::from(100)]),
            status: DiffStatus::MissingLeft,
            confirmed: true,
        }];
        let line: Value =
            serde_json::from_str(render_jsonl(&r, false).lines().next().unwrap()).unwrap();
        assert_eq!(line["key"]["hash"], "abc123def456");
        assert_eq!(line["right"]["xwdm"], "59267");
    }

    #[test]
    fn jsonl_modified_uses_fields_left_right() {
        let text = render_jsonl(&report(), false);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let modified: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(modified["status"], "modified");
        assert_eq!(modified["fields"]["cjsl"]["left"], 10);
        assert_eq!(modified["fields"]["cjsl"]["right"], 12);
        assert!(modified["fields"].get("yhs").is_none());
    }

    #[test]
    fn jsonl_missing_left_has_right_object() {
        let text = render_jsonl(&report(), false);
        let missing: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(missing["status"], "missing_left");
        assert_eq!(missing["right"]["cjsl"], 100);
    }

    #[test]
    fn json_export_is_full_diffreport() {
        let s = render_export(&report(), ExportFormat::Json, false).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["sample_diffs"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn export_row_count_equals_sample_diffs_len() {
        let csv = render_csv(&report(), false);
        assert_eq!(csv.lines().count() - 1, 2);
        let jsonl = render_jsonl(&report(), false);
        assert_eq!(jsonl.lines().count(), 2);
    }
}
