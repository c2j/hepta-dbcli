// ─── delta-diff output: DiffReport → QueryResult 投影（§四 output 层）───
//
// 将差异样本投影为 QueryResult，复用 cli.rs::render_result 的
// table/csv/vertical 管道；JSON 走 serde 直出（报告结构非表格）。

use serde_json::Value;

use crate::backend::QueryResult;
use crate::delta_diff::report::DiffReport;

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
}
