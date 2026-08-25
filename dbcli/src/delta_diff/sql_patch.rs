// ─── delta-diff SQL patch renderer ─────────────────────────────────────

use serde_json::Value;

use crate::backend::{quote_ident_scheme, quote_table_scheme, sql_literal};
use crate::delta_diff::cmd::ApplyTo;
use crate::delta_diff::report::{DiffReport, DiffRow, DiffStatus, RowPayload};

pub(crate) const SQL_ROW_CAP: usize = 100_000;

pub(crate) struct SqlPatchOpts<'a> {
    pub apply_to: ApplyTo,
    pub scheme: &'a str,
    pub quote: char,
    pub backslash_escape: bool,
    pub target_conn: &'a str,
    pub target_schema: Option<&'a str>,
    pub target_table: &'a str,
}

pub(crate) fn render_sql_patch(
    report: &DiffReport,
    opts: &SqlPatchOpts<'_>,
) -> Result<String, String> {
    if report.sample_diffs.len() > SQL_ROW_CAP {
        return Err(format!(
            "refusing single .sql with {} diffs (cap {SQL_ROW_CAP}); --export out.csv first",
            report.sample_diffs.len()
        ));
    }
    if report.row_payload == RowPayload::HashCount || report.strategy == "bucketdiff" {
        for row in &report.sample_diffs {
            match row.status {
                DiffStatus::Modified => {
                    return Err(
                        "keyless diff cannot emit precise UPDATE; use --export out.csv".into(),
                    );
                }
                DiffStatus::MissingRight => {
                    return Err(
                        "keyless diff cannot emit precise DELETE; use --export out.csv".into(),
                    );
                }
                DiffStatus::MissingLeft => {
                    if !row_hydrated(row, report) {
                        return Err(
                            "keyless INSERT needs hydrated row values; omit --no-fetch-sample or pass --export-rows".into(),
                        );
                    }
                }
            }
        }
    }

    let other = match opts.apply_to {
        ApplyTo::Left => "right",
        ApplyTo::Right => "left",
    };
    let table = qualified_table(opts);
    let mut out = String::new();
    out.push_str("-- REVIEW BEFORE EXECUTE\n");
    out.push_str(&format!(
        "-- apply-to: {} = {}.{}.{}  (make it match {})\n",
        match opts.apply_to {
            ApplyTo::Left => "left",
            ApplyTo::Right => "right",
        },
        opts.target_conn,
        opts.target_schema.unwrap_or(""),
        opts.target_table,
        other
    ));
    out.push_str(&format!(
        "-- missing_left={}  missing_right={}  modified={}\n",
        report.summary.missing_left, report.summary.missing_right, report.summary.modified
    ));

    for row in &report.sample_diffs {
        out.push_str(&render_row_sql(report, row, opts, &table)?);
        out.push('\n');
    }
    Ok(out)
}

fn qualified_table(opts: &SqlPatchOpts<'_>) -> String {
    quote_table_scheme(
        opts.scheme,
        opts.quote,
        opts.target_schema,
        opts.target_table,
    )
}

fn row_hydrated(row: &DiffRow, report: &DiffReport) -> bool {
    if report.row_payload != RowPayload::Columns {
        return false;
    }
    let need = report.key_columns.len() + report.value_columns.len();
    if need == 0 {
        return false;
    }
    row.left
        .as_ref()
        .or(row.right.as_ref())
        .map(|r| r.len() >= need && need > report.key_columns.len())
        .unwrap_or(false)
}

fn render_row_sql(
    report: &DiffReport,
    row: &DiffRow,
    opts: &SqlPatchOpts<'_>,
    table: &str,
) -> Result<String, String> {
    let insert_from_right = matches!(
        (opts.apply_to, row.status),
        (ApplyTo::Left, DiffStatus::MissingLeft) | (ApplyTo::Right, DiffStatus::MissingRight)
    );
    let delete_from_target = matches!(
        (opts.apply_to, row.status),
        (ApplyTo::Left, DiffStatus::MissingRight) | (ApplyTo::Right, DiffStatus::MissingLeft)
    );
    if insert_from_right {
        let cells = match opts.apply_to {
            ApplyTo::Left => row.right.as_ref(),
            ApplyTo::Right => row.left.as_ref(),
        }
        .ok_or("INSERT missing source row values")?;
        let stmt = render_insert(report, cells, opts, table);
        let n = insert_copies(report, row);
        if n <= 1 {
            return Ok(stmt);
        }
        return Ok(vec![stmt; n].join("\n"));
    }
    if delete_from_target {
        return Ok(render_delete(report, row, opts, table));
    }
    render_update(report, row, opts, table)
}

fn insert_copies(report: &DiffReport, row: &DiffRow) -> usize {
    if report.strategy != "bucketdiff" {
        return 1;
    }
    if let Value::Object(o) = &row.key {
        let lc = o.get("left").and_then(Value::as_u64).unwrap_or(0);
        let rc = o.get("right").and_then(Value::as_u64).unwrap_or(0);
        return lc.abs_diff(rc).max(1) as usize;
    }
    parse_multiset_gap(&row.key).max(1)
}

fn parse_multiset_gap(key: &Value) -> usize {
    let Some(s) = key.as_str() else {
        return 1;
    };
    let Some((_, rest)) = s.rsplit_once('×') else {
        return 1;
    };
    let rest = rest.trim_end_matches(')');
    let Some((l, r)) = rest.split_once('/') else {
        return 1;
    };
    match (l.parse::<u64>(), r.parse::<u64>()) {
        (Ok(lc), Ok(rc)) => lc.abs_diff(rc) as usize,
        _ => 1,
    }
}

fn all_col_names(report: &DiffReport) -> Vec<String> {
    let mut c = report.key_columns.clone();
    c.extend(report.value_columns.iter().cloned());
    c
}

fn render_insert(
    report: &DiffReport,
    cells: &[Value],
    opts: &SqlPatchOpts<'_>,
    table: &str,
) -> String {
    let names = all_col_names(report);
    let cols = names
        .iter()
        .map(|n| quote_ident_scheme(opts.scheme, opts.quote, n))
        .collect::<Vec<_>>()
        .join(", ");
    let vals = names
        .iter()
        .enumerate()
        .map(|(i, _)| sql_literal(cells.get(i).unwrap_or(&Value::Null), opts.backslash_escape))
        .collect::<Vec<_>>()
        .join(", ");
    format!("INSERT INTO {table} ({cols}) VALUES ({vals});")
}

fn render_delete(
    report: &DiffReport,
    row: &DiffRow,
    opts: &SqlPatchOpts<'_>,
    table: &str,
) -> String {
    format!(
        "DELETE FROM {table} WHERE {};",
        key_predicate(report, row, opts)
    )
}

fn render_update(
    report: &DiffReport,
    row: &DiffRow,
    opts: &SqlPatchOpts<'_>,
    table: &str,
) -> Result<String, String> {
    let key_len = report.key_columns.len();
    let src_is_right = matches!(opts.apply_to, ApplyTo::Left);
    let mut sets = Vec::new();
    for (i, name) in report.value_columns.iter().enumerate() {
        let l = row.left.as_ref().and_then(|r| r.get(key_len + i));
        let r = row.right.as_ref().and_then(|r| r.get(key_len + i));
        if l == r {
            continue;
        }
        let new_v = if src_is_right { r } else { l }.unwrap_or(&Value::Null);
        sets.push(format!(
            "{} = {}",
            quote_ident_scheme(opts.scheme, opts.quote, name),
            sql_literal(new_v, opts.backslash_escape)
        ));
    }
    if sets.is_empty() {
        return Ok(format!(
            "-- skip unchanged {}",
            key_predicate(report, row, opts)
        ));
    }
    Ok(format!(
        "UPDATE {table} SET {} WHERE {};",
        sets.join(", "),
        key_predicate(report, row, opts)
    ))
}

fn key_eq(name: &str, v: &Value, opts: &SqlPatchOpts<'_>) -> String {
    let col = quote_ident_scheme(opts.scheme, opts.quote, name);
    if v.is_null() {
        format!("{col} IS NULL")
    } else {
        format!("{col} = {}", sql_literal(v, opts.backslash_escape))
    }
}

fn key_predicate(report: &DiffReport, row: &DiffRow, opts: &SqlPatchOpts<'_>) -> String {
    let src = row.left.as_ref().or(row.right.as_ref());
    report
        .key_columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let v = src
                .and_then(|r| r.get(i))
                .or_else(|| match &row.key {
                    Value::Array(a) => a.get(i),
                    v if i == 0 => Some(v),
                    _ => None,
                })
                .unwrap_or(&Value::Null);
            key_eq(name, v, opts)
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta_diff::report::*;
    use chrono::Utc;
    use serde_json::json;

    fn report_keyed() -> DiffReport {
        DiffReport {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            left: TableRef {
                connection: "touchigh".into(),
                schema: Some("bigfund".into()),
                table: "dat_fund_cjqs".into(),
            },
            right: TableRef {
                connection: "dota".into(),
                schema: Some("bigfund".into()),
                table: "dat_fund_cjqs".into(),
            },
            strategy: "keyeddiff".into(),
            consistency: "none".into(),
            hash_algorithm: "md5".into(),
            summary: DiffSummary {
                left_total: 1,
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
                    ]),
                    right: Some(vec![
                        Value::from("59267"),
                        Value::from("600001"),
                        Value::from(12),
                    ]),
                    status: DiffStatus::Modified,
                    confirmed: true,
                },
            ],
            warnings: vec![],
            row_payload: RowPayload::Columns,
            key_columns: vec!["xwdm".into(), "security_id".into()],
            value_columns: vec!["cjsl".into()],
            column_data_types: vec![],
            ident_quote: '"',
            ident_scheme: String::new(),
            backslash_escape: false,
        }
    }

    fn opts_left<'a>() -> SqlPatchOpts<'a> {
        SqlPatchOpts {
            apply_to: ApplyTo::Left,
            scheme: "gaussdb",
            quote: '"',
            backslash_escape: false,
            target_conn: "touchigh",
            target_schema: Some("bigfund"),
            target_table: "dat_fund_cjqs",
        }
    }

    #[test]
    fn apply_to_left_inserts_into_left_table() {
        let sql = render_sql_patch(&report_keyed(), &opts_left()).unwrap();
        assert!(
            sql.contains("INSERT INTO \"bigfund\".\"dat_fund_cjqs\""),
            "{sql}"
        );
        assert!(sql.contains("100"), "{sql}");
        assert!(sql.contains("-- REVIEW BEFORE EXECUTE"), "{sql}");
        assert!(!sql.contains("BEGIN"), "{sql}");
        assert!(!sql.contains("COMMIT"), "{sql}");
    }

    #[test]
    fn apply_to_left_update_only_changed_cols() {
        let sql = render_sql_patch(&report_keyed(), &opts_left()).unwrap();
        assert!(sql.contains("UPDATE"), "{sql}");
        assert!(sql.contains("\"cjsl\" = 12"), "{sql}");
        assert!(sql.contains("\"xwdm\" = '59267'"), "{sql}");
    }

    #[test]
    fn key_predicate_uses_is_null_for_null_key() {
        let mut r = report_keyed();
        r.sample_diffs = vec![DiffRow {
            key: json!(["59267", Value::Null]),
            left: Some(vec![Value::from("59267"), Value::Null, Value::from(8)]),
            right: None,
            status: DiffStatus::MissingRight,
            confirmed: true,
        }];
        r.summary.missing_right = 1;
        r.summary.missing_left = 0;
        r.summary.modified = 0;
        let sql = render_sql_patch(&r, &opts_left()).unwrap();
        assert!(sql.contains("\"security_id\" IS NULL"), "{sql}");
        assert!(!sql.contains("\"security_id\" = NULL"), "{sql}");
        assert!(sql.contains("\"xwdm\" = '59267'"), "{sql}");
    }

    #[test]
    fn update_set_still_assigns_null_with_eq() {
        let mut r = report_keyed();
        r.sample_diffs = vec![DiffRow {
            key: json!(["59267", "600001"]),
            left: Some(vec![
                Value::from("59267"),
                Value::from("600001"),
                Value::from(10),
            ]),
            right: Some(vec![
                Value::from("59267"),
                Value::from("600001"),
                Value::Null,
            ]),
            status: DiffStatus::Modified,
            confirmed: true,
        }];
        r.summary.modified = 1;
        r.summary.missing_left = 0;
        let sql = render_sql_patch(&r, &opts_left()).unwrap();
        assert!(sql.contains("\"cjsl\" = NULL"), "{sql}");
        assert!(!sql.contains("SET \"cjsl\" IS NULL"), "{sql}");
    }

    #[test]
    fn apply_to_left_deletes_from_left() {
        let mut r = report_keyed();
        r.sample_diffs = vec![DiffRow {
            key: json!(["59267", "600002"]),
            left: Some(vec![
                Value::from("59267"),
                Value::from("600002"),
                Value::from(8),
            ]),
            right: None,
            status: DiffStatus::MissingRight,
            confirmed: true,
        }];
        r.summary.missing_right = 1;
        r.summary.missing_left = 0;
        r.summary.modified = 0;
        let sql = render_sql_patch(&r, &opts_left()).unwrap();
        assert!(
            sql.contains("DELETE FROM \"bigfund\".\"dat_fund_cjqs\""),
            "{sql}"
        );
        assert!(sql.contains("\"security_id\" = '600002'"), "{sql}");
    }

    #[test]
    fn apply_to_right_mirrors_direction() {
        let mut opts = opts_left();
        opts.apply_to = ApplyTo::Right;
        opts.target_conn = "dota";
        let sql = render_sql_patch(&report_keyed(), &opts).unwrap();
        assert!(sql.contains("DELETE FROM"), "{sql}");
        assert!(sql.contains("UPDATE"), "{sql}");
        assert!(sql.contains("\"cjsl\" = 10"), "{sql}");
    }

    #[test]
    fn sql_refuses_over_100k() {
        let mut r = report_keyed();
        r.sample_diffs = (0..SQL_ROW_CAP + 1)
            .map(|i| DiffRow {
                key: Value::from(i as i64),
                left: None,
                right: Some(vec![Value::from(i as i64), Value::from(1)]),
                status: DiffStatus::MissingLeft,
                confirmed: true,
            })
            .collect();
        let err = render_sql_patch(&r, &opts_left()).unwrap_err();
        assert!(err.contains("100000"), "{err}");
    }

    #[test]
    fn sql_keyless_modified_errors() {
        let mut r = report_keyed();
        r.strategy = "bucketdiff".into();
        r.row_payload = RowPayload::HashCount;
        r.key_columns.clear();
        r.sample_diffs = vec![r.sample_diffs[1].clone()];
        let err = render_sql_patch(&r, &opts_left()).unwrap_err();
        assert!(err.contains("UPDATE"), "{err}");
    }

    #[test]
    fn sql_keyless_insert_repeats_for_multiset_gap() {
        let mut r = report_keyed();
        r.strategy = "bucketdiff".into();
        r.key_columns.clear();
        r.value_columns = vec!["xwdm".into(), "security_id".into(), "cjsl".into()];
        r.sample_diffs = vec![DiffRow {
            key: Value::from("015d418e4e99…(×1/3)"),
            left: None,
            right: Some(vec![
                Value::from("59267"),
                Value::from("600000"),
                Value::from(100),
            ]),
            status: DiffStatus::MissingLeft,
            confirmed: true,
        }];
        r.summary.modified = 0;
        let sql = render_sql_patch(&r, &opts_left()).unwrap();
        let inserts = sql.matches("INSERT INTO").count();
        assert_eq!(inserts, 2, "{sql}");
    }

    #[test]
    fn sql_keyless_insert_repeats_from_key_object_counts() {
        let mut r = report_keyed();
        r.strategy = "bucketdiff".into();
        r.key_columns.clear();
        r.value_columns = vec!["xwdm".into(), "security_id".into(), "cjsl".into()];
        r.sample_diffs = vec![DiffRow {
            key: json!({"hash":"abc123def456","left":1,"right":3}),
            left: None,
            right: Some(vec![
                Value::from("59267"),
                Value::from("600000"),
                Value::from(100),
            ]),
            status: DiffStatus::MissingLeft,
            confirmed: true,
        }];
        r.summary.modified = 0;
        let sql = render_sql_patch(&r, &opts_left()).unwrap();
        assert_eq!(sql.matches("INSERT INTO").count(), 2, "{sql}");
    }

    #[test]
    fn sql_keyless_insert_ok_when_hydrated() {
        let mut r = report_keyed();
        r.strategy = "bucketdiff".into();
        r.key_columns.clear();
        r.value_columns = vec!["xwdm".into(), "security_id".into(), "cjsl".into()];
        r.sample_diffs = vec![DiffRow {
            key: Value::from("h"),
            left: None,
            right: Some(vec![
                Value::from("59267"),
                Value::from("600000"),
                Value::from(100),
            ]),
            status: DiffStatus::MissingLeft,
            confirmed: true,
        }];
        r.summary.modified = 0;
        let sql = render_sql_patch(&r, &opts_left()).unwrap();
        assert!(sql.contains("INSERT INTO"), "{sql}");
    }

    #[test]
    fn sql_quotes_identifiers_with_dialect_quote() {
        let mut opts = opts_left();
        opts.quote = '`';
        let sql = render_sql_patch(&report_keyed(), &opts).unwrap();
        assert!(sql.contains("`bigfund`.`dat_fund_cjqs`"), "{sql}");
    }
}
