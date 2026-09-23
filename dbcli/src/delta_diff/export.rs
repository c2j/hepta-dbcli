// ─── delta-diff full-diff file export (csv / jsonl / json) ─────────────

use rust_decimal::Decimal;
use serde_json::{json, Value};
use std::str::FromStr;

use crate::delta_diff::cmd::ExportFormat;
use crate::delta_diff::report::{DiffReport, DiffRow, DiffStatus, RowPayload};

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
    if report.row_payload == RowPayload::HashCount || report.strategy == "bucketdiff" {
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

fn declared_scale(data_type: &str) -> Option<u32> {
    let trimmed = data_type.trim();
    let base = trimmed
        .split('(')
        .next()
        .unwrap_or(trimmed)
        .trim()
        .to_ascii_lowercase();
    if !matches!(base.as_str(), "number" | "numeric" | "decimal" | "dec") {
        return None;
    }
    let inner = trimmed.split_once('(')?.1.strip_suffix(')')?;
    let scale = inner.split(',').nth(1)?.trim();
    if scale == "*" {
        return None;
    }
    scale.parse().ok()
}

fn format_fixed_scale(d: Decimal, scale: u32) -> String {
    let rounded = d.round_dp(scale);
    if scale == 0 {
        return rounded.trunc().to_string();
    }
    let mut text = rounded.to_string();
    if let Some(dot) = text.find('.') {
        let frac = text.len() - dot - 1;
        if frac < scale as usize {
            text.push_str(&"0".repeat(scale as usize - frac));
        }
        text
    } else {
        format!("{text}.{}", "0".repeat(scale as usize))
    }
}

pub(crate) fn csv_cell_typed(v: Option<&Value>, data_type: &str) -> String {
    let Some(scale) = declared_scale(data_type) else {
        return csv_cell(v);
    };
    let text = match v {
        None | Some(Value::Null) => return String::new(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => return csv_escape(&other.to_string()),
    };
    match Decimal::from_str(text.trim()) {
        Ok(d) => format_fixed_scale(d, scale),
        Err(_) => csv_escape(&text),
    }
}

fn table_column_names(report: &DiffReport) -> Vec<String> {
    let mut names = report.key_columns.clone();
    names.extend(report.value_columns.iter().cloned());
    names
}

fn csv_side_row(kind: &str, cells: Option<&[Value]>, types: &[String], ncols: usize) -> String {
    let mut out = Vec::with_capacity(ncols + 1);
    out.push(kind.to_string());
    for i in 0..ncols {
        let ty = types.get(i).map(String::as_str).unwrap_or("");
        out.push(csv_cell_typed(cells.and_then(|c| c.get(i)), ty));
    }
    out.join(",")
}

fn render_csv(report: &DiffReport, _export_rows: bool) -> String {
    if report.row_payload == RowPayload::HashCount {
        return render_keyless_csv(report);
    }
    let names = table_column_names(report);
    let ncols = names.len();
    let mut headers = vec!["deltadiff_type".to_string()];
    headers.extend(names);
    let mut lines = vec![headers.join(",")];
    for row in &report.sample_diffs {
        match row.status {
            DiffStatus::MissingRight => {
                lines.push(csv_side_row(
                    "only_left",
                    row.left.as_deref(),
                    &report.column_data_types,
                    ncols,
                ));
            }
            DiffStatus::MissingLeft => {
                lines.push(csv_side_row(
                    "only_right",
                    row.right.as_deref(),
                    &report.column_data_types,
                    ncols,
                ));
            }
            DiffStatus::Modified => {
                lines.push(csv_side_row(
                    "modified_left",
                    row.left.as_deref(),
                    &report.column_data_types,
                    ncols,
                ));
                lines.push(csv_side_row(
                    "modified_right",
                    row.right.as_deref(),
                    &report.column_data_types,
                    ncols,
                ));
            }
        }
    }
    lines.join("\n") + "\n"
}

fn render_keyless_csv(report: &DiffReport) -> String {
    let mut lines = vec!["deltadiff_type,hash,left_count,right_count".to_string()];
    for row in &report.sample_diffs {
        let hash = keyless_hash(row);
        let (lc, rc) = keyless_counts(row);
        let kind = match row.status {
            DiffStatus::MissingRight => "only_left",
            DiffStatus::MissingLeft => "only_right",
            DiffStatus::Modified => "modified",
        };
        lines.push(format!(
            "{},{},{},{}",
            kind,
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
        if report.row_payload == RowPayload::HashCount {
            let (left_count, right_count) = keyless_counts(row);
            lines.push(
                json!({
                    "status": status_export(row.status),
                    "hash": keyless_hash(row),
                    "left_count": left_count,
                    "right_count": right_count,
                })
                .to_string(),
            );
            continue;
        }
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
            row_payload: RowPayload::Columns,
            key_columns: vec!["xwdm".into(), "security_id".into()],
            value_columns: vec!["cjsl".into(), "yhs".into()],
            column_data_types: vec![],
            ident_quote: '"',
            ident_scheme: String::new(),
            backslash_escape: false,
            modified_columns: None,
            left_column_names: None,
            right_column_names: None,
        }
    }

    #[test]
    fn declared_scale_pads_and_trims_driver_literals() {
        assert_eq!(declared_scale("NUMBER(15,2)"), Some(2));
        assert_eq!(declared_scale("numeric(15,3)"), Some(3));
        assert_eq!(declared_scale("NUMBER(24,0)"), Some(0));
        assert_eq!(declared_scale("varchar2(8)"), None);
        assert_eq!(
            format_fixed_scale(Decimal::from_str("12150.0").unwrap(), 2),
            "12150.00"
        );
        assert_eq!(
            format_fixed_scale(Decimal::from_str("12150").unwrap(), 2),
            "12150.00"
        );
        assert_eq!(
            format_fixed_scale(Decimal::from_str("748.31").unwrap(), 8),
            "748.31000000"
        );
        assert_eq!(
            csv_cell_typed(Some(&Value::from(12150.0)), "NUMBER(15,2)"),
            "12150.00"
        );
        assert_eq!(
            csv_cell_typed(Some(&Value::from(12150)), "NUMBER(15,2)"),
            "12150.00"
        );
    }

    #[test]
    fn csv_applies_declared_scale_on_value_columns() {
        let mut r = report();
        r.column_data_types = vec![
            "varchar2(7)".into(),
            "varchar2(19)".into(),
            "NUMBER(15,2)".into(),
            "NUMBER(15,2)".into(),
        ];
        let csv = render_csv(&r, false);
        assert!(csv.contains("only_right,59267,600000,100.00,0.50"), "{csv}");
        assert!(
            csv.contains("modified_left,59267,600001,10.00,0.10"),
            "{csv}"
        );
        assert!(
            csv.contains("modified_right,59267,600001,12.00,0.10"),
            "{csv}"
        );
    }

    #[test]
    fn csv_is_isomorphic_plus_deltadiff_type() {
        let csv = render_csv(&report(), false);
        let header = csv.lines().next().unwrap();
        assert_eq!(header, "deltadiff_type,xwdm,security_id,cjsl,yhs");
        assert!(!header.contains("_left"), "{header}");
        let lines: Vec<&str> = csv.lines().collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[1], "only_right,59267,600000,100,0.5");
        assert_eq!(lines[2], "modified_left,59267,600001,10,0.1");
        assert_eq!(lines[3], "modified_right,59267,600001,12,0.1");
    }

    #[test]
    fn csv_export_rows_keeps_same_table_shape() {
        let csv = render_csv(&report(), true);
        assert_eq!(
            csv.lines().next(),
            Some("deltadiff_type,xwdm,security_id,cjsl,yhs")
        );
    }

    #[test]
    fn csv_keyless_default_is_hash_and_counts() {
        let mut r = report();
        r.row_payload = RowPayload::HashCount;
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
            csv.starts_with("deltadiff_type,hash,left_count,right_count"),
            "{csv}"
        );
        assert!(csv.contains("only_right,abc,0,1"), "{csv}");
    }

    #[test]
    fn csv_hash_count_ignores_stale_named_column_schema() {
        let mut r = report();
        r.row_payload = RowPayload::HashCount;
        r.key_columns = (0..14).map(|i| format!("key_{i}")).collect();
        r.value_columns = (0..20).map(|i| format!("value_{i}")).collect();
        r.sample_diffs = vec![DiffRow {
            key: Value::from("h"),
            left: Some(vec![Value::from("abc"), Value::from(0)]),
            right: Some(vec![Value::from("abc"), Value::from(1)]),
            status: DiffStatus::MissingLeft,
            confirmed: true,
        }];

        let csv = render_csv(&r, false);

        assert_eq!(
            csv.lines().next(),
            Some("deltadiff_type,hash,left_count,right_count")
        );
        assert!(!csv.contains("key_0"), "{csv}");
        assert!(!csv.contains("value_19"), "{csv}");
    }

    #[test]
    fn jsonl_keyless_hash_survives_hydrate() {
        let mut r = report();
        r.strategy = "bucketdiff".into();
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
        assert_eq!(csv.lines().count() - 1, 3);
        let jsonl = render_jsonl(&report(), false);
        assert_eq!(jsonl.lines().count(), 2);
    }
}
