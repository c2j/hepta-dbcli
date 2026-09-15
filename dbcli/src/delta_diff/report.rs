// ─── delta-diff report: 差异结果与报告类型（设计文档 §5.1）──────────────

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 差异行
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DiffRow {
    /// 主键值（复合主键为数组）
    pub(crate) key: serde_json::Value,
    pub(crate) left: Option<Vec<serde_json::Value>>,
    pub(crate) right: Option<Vec<serde_json::Value>>,
    pub(crate) status: DiffStatus,
    /// 是否经二次复核确认（§8.3；Phase 2 接入，当前恒 true）
    pub(crate) confirmed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DiffStatus {
    MissingLeft,
    MissingRight,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RowPayload {
    /// cells are [key…, value…], aligned to key_columns ++ value_columns
    #[default]
    Columns,
    /// cells are [row_hash, multiset_count] — NOT column-aligned
    HashCount,
}

/// Modified 行按「变化列」聚合计数（仅统计 DiffStatus::Modified）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ColumnChangeCount {
    pub(crate) name: String,
    pub(crate) count: u64,
}

/// 分片比对结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ShardResult {
    pub(crate) shard_id: String,
    pub(crate) key_range: (serde_json::Value, serde_json::Value),
    pub(crate) left_count: u64,
    pub(crate) right_count: u64,
    pub(crate) diff_count: u64,
    pub(crate) status: ShardStatus,
    pub(crate) duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ShardStatus {
    Match,
    Diff,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TableRef {
    pub(crate) connection: String,
    pub(crate) schema: Option<String>,
    pub(crate) table: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct DiffSummary {
    pub(crate) left_total: u64,
    pub(crate) right_total: u64,
    pub(crate) missing_left: u64,
    pub(crate) missing_right: u64,
    pub(crate) modified: u64,
    pub(crate) diff_rate: f64,
}

/// 性能埋点（§5.1 PerfMetrics；P1-6 接入 CI 基准）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PerfMetrics {
    pub(crate) queries_total: u64,
    pub(crate) shard_duration_p50_ms: u64,
    pub(crate) shard_duration_p99_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DiffReport {
    pub(crate) started_at: DateTime<Utc>,
    pub(crate) finished_at: DateTime<Utc>,
    pub(crate) left: TableRef,
    pub(crate) right: TableRef,
    pub(crate) strategy: String,
    pub(crate) consistency: String,
    pub(crate) hash_algorithm: String,
    pub(crate) summary: DiffSummary,
    pub(crate) perf: PerfMetrics,
    pub(crate) shards: Vec<ShardResult>,
    pub(crate) sample_diffs: Vec<DiffRow>,
    pub(crate) warnings: Vec<String>,
    #[serde(default)]
    pub(crate) row_payload: RowPayload,
    /// Compare-key column names, in key order. Empty for keyless reports.
    #[serde(default)]
    pub(crate) key_columns: Vec<String>,
    /// Non-key compare columns, in row-tail order. Empty until keyless hydrate.
    #[serde(default)]
    pub(crate) value_columns: Vec<String>,
    /// Declared types aligned with `row_columns()` (left-plan names).
    #[serde(default)]
    pub(crate) column_data_types: Vec<String>,
    /// Modified 行按变化列计数，count 降序；None = 无 Modified 行 / keyless / 尚未计算
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) modified_columns: Option<Vec<ColumnChangeCount>>,
    #[serde(skip)]
    pub(crate) ident_quote: char,
    #[serde(skip)]
    pub(crate) ident_scheme: String,
    #[serde(skip)]
    pub(crate) backslash_escape: bool,
}

impl DiffReport {
    pub(crate) fn has_diff(&self) -> bool {
        let s = &self.summary;
        s.missing_left + s.missing_right + s.modified > 0
    }

    pub(crate) fn total_diffs(&self) -> u64 {
        self.summary.missing_left + self.summary.missing_right + self.summary.modified
    }

    pub(crate) fn row_columns(&self) -> Vec<String> {
        let mut cols = self.key_columns.clone();
        cols.extend(self.value_columns.iter().cloned());
        cols
    }
}

pub(crate) fn stamp_columns(
    report: &mut DiffReport,
    key_columns: &[String],
    compare_columns: &[String],
) {
    report.key_columns = key_columns.to_vec();
    report.value_columns = compare_columns
        .iter()
        .filter(|c| !key_columns.iter().any(|k| k == *c))
        .cloned()
        .collect();
}

pub(crate) fn stamp_columns_from_plan(
    report: &mut DiffReport,
    plan: &crate::delta_diff::metadata::TablePlan,
) {
    stamp_columns(report, &plan.key_columns, &plan.compare_columns);
    report.column_data_types = report
        .row_columns()
        .iter()
        .map(|name| {
            plan.norm_specs
                .iter()
                .find(|spec| spec.name.eq_ignore_ascii_case(name))
                .map(|spec| spec.data_type.clone())
                .unwrap_or_default()
        })
        .collect();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_json_roundtrip() {
        let report = DiffReport {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            left: TableRef {
                connection: "dev".into(),
                schema: Some("test".into()),
                table: "orders".into(),
            },
            right: TableRef {
                connection: "prod".into(),
                schema: None,
                table: "orders".into(),
            },
            strategy: "hashdiff".into(),
            consistency: "none".into(),
            hash_algorithm: "md5".into(),
            summary: DiffSummary {
                left_total: 100,
                right_total: 99,
                missing_left: 0,
                missing_right: 1,
                modified: 0,
                diff_rate: 0.01,
            },
            perf: PerfMetrics::default(),
            shards: vec![],
            sample_diffs: vec![DiffRow {
                key: serde_json::json!(42),
                left: Some(vec![serde_json::json!(42)]),
                right: None,
                status: DiffStatus::MissingRight,
                confirmed: true,
            }],
            warnings: vec![],
            row_payload: RowPayload::Columns,
            key_columns: vec![],
            value_columns: vec![],
            column_data_types: vec![],
            ident_quote: '"',
            ident_scheme: String::new(),
            backslash_escape: false,
            modified_columns: None,
        };
        let s = serde_json::to_string(&report).unwrap();
        let back: DiffReport = serde_json::from_str(&s).unwrap();
        assert!(back.has_diff());
        assert_eq!(back.summary.missing_right, 1);
    }

    #[test]
    fn legacy_report_without_row_payload_defaults_to_columns() {
        let report = DiffReport {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            left: TableRef {
                connection: "a".into(),
                schema: None,
                table: "t".into(),
            },
            right: TableRef {
                connection: "b".into(),
                schema: None,
                table: "t".into(),
            },
            strategy: "hashdiff".into(),
            consistency: "none".into(),
            hash_algorithm: "md5".into(),
            summary: DiffSummary::default(),
            perf: PerfMetrics::default(),
            shards: vec![],
            sample_diffs: vec![],
            warnings: vec![],
            row_payload: RowPayload::HashCount,
            key_columns: vec![],
            value_columns: vec![],
            column_data_types: vec![],
            ident_quote: '"',
            ident_scheme: String::new(),
            backslash_escape: false,
            modified_columns: None,
        };
        let mut json = serde_json::to_value(report).expect("report should serialize");
        json.as_object_mut()
            .expect("report JSON should be an object")
            .remove("row_payload");

        let legacy: DiffReport =
            serde_json::from_value(json).expect("legacy report should deserialize");

        assert_eq!(legacy.row_payload, RowPayload::Columns);
    }

    #[test]
    fn has_diff_false_when_clean() {
        let mut report: DiffReport = serde_json::from_str(
            &serde_json::to_string(&DiffReport {
                started_at: Utc::now(),
                finished_at: Utc::now(),
                left: TableRef {
                    connection: "a".into(),
                    schema: None,
                    table: "t".into(),
                },
                right: TableRef {
                    connection: "b".into(),
                    schema: None,
                    table: "t".into(),
                },
                strategy: "hashdiff".into(),
                consistency: "none".into(),
                hash_algorithm: "md5".into(),
                summary: DiffSummary::default(),
                perf: PerfMetrics::default(),
                shards: vec![],
                sample_diffs: vec![],
                warnings: vec![],
                row_payload: RowPayload::Columns,
                key_columns: vec![],
                value_columns: vec![],
                column_data_types: vec![],
                ident_quote: '"',
                ident_scheme: String::new(),
                backslash_escape: false,
                modified_columns: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert!(!report.has_diff());
        report.summary.modified = 1;
        assert!(report.has_diff());
    }

    #[test]
    fn total_diffs_sums_three_counters() {
        let mut report = serde_json::from_str::<DiffReport>(
            &serde_json::to_string(&DiffReport {
                started_at: Utc::now(),
                finished_at: Utc::now(),
                left: TableRef {
                    connection: "a".into(),
                    schema: None,
                    table: "t".into(),
                },
                right: TableRef {
                    connection: "b".into(),
                    schema: None,
                    table: "t".into(),
                },
                strategy: "keyeddiff".into(),
                consistency: "none".into(),
                hash_algorithm: "md5".into(),
                summary: DiffSummary {
                    left_total: 10,
                    right_total: 12,
                    missing_left: 3,
                    missing_right: 1,
                    modified: 4,
                    diff_rate: 0.8,
                },
                perf: PerfMetrics::default(),
                shards: vec![],
                sample_diffs: vec![],
                warnings: vec![],
                row_payload: RowPayload::Columns,
                key_columns: vec![],
                value_columns: vec![],
                column_data_types: vec![],
                ident_quote: '"',
                ident_scheme: String::new(),
                backslash_escape: false,
                modified_columns: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(report.total_diffs(), 8);
        report.key_columns = vec!["id".into()];
        report.value_columns = vec!["amount".into()];
        assert_eq!(report.row_columns(), vec!["id", "amount"]);
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["key_columns"][0], "id");
        assert_eq!(json["value_columns"][0], "amount");
    }

    #[test]
    fn stamp_columns_drops_keys_from_value_list_preserving_order() {
        let mut report = serde_json::from_str::<DiffReport>(
            &serde_json::to_string(&DiffReport {
                started_at: Utc::now(),
                finished_at: Utc::now(),
                left: TableRef {
                    connection: "a".into(),
                    schema: None,
                    table: "t".into(),
                },
                right: TableRef {
                    connection: "b".into(),
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
                key_columns: vec![],
                value_columns: vec![],
                column_data_types: vec![],
                ident_quote: '"',
                ident_scheme: String::new(),
                backslash_escape: false,
                modified_columns: None,
            })
            .unwrap(),
        )
        .unwrap();
        stamp_columns(
            &mut report,
            &["k2".into(), "k1".into()],
            &["k1".into(), "amt".into(), "k2".into(), "yhs".into()],
        );
        assert_eq!(report.key_columns, vec!["k2", "k1"]);
        assert_eq!(report.value_columns, vec!["amt", "yhs"]);
    }

    #[test]
    fn legacy_report_without_modified_columns_defaults_to_none() {
        let report = DiffReport {
            started_at: Utc::now(),
            finished_at: Utc::now(),
            left: TableRef {
                connection: "dev".into(),
                schema: Some("test".into()),
                table: "orders".into(),
            },
            right: TableRef {
                connection: "prod".into(),
                schema: None,
                table: "orders".into(),
            },
            strategy: "hashdiff".into(),
            consistency: "none".into(),
            hash_algorithm: "md5".into(),
            summary: DiffSummary {
                left_total: 100,
                right_total: 99,
                missing_left: 0,
                missing_right: 1,
                modified: 0,
                diff_rate: 0.01,
            },
            perf: PerfMetrics::default(),
            shards: vec![],
            sample_diffs: vec![DiffRow {
                key: serde_json::json!(42),
                left: Some(vec![serde_json::json!(42)]),
                right: None,
                status: DiffStatus::MissingRight,
                confirmed: true,
            }],
            warnings: vec![],
            row_payload: RowPayload::Columns,
            key_columns: vec![],
            value_columns: vec![],
            column_data_types: vec![],
            ident_quote: '"',
            ident_scheme: String::new(),
            backslash_escape: false,
            modified_columns: None,
        };
        let mut json = serde_json::to_value(report).expect("report should serialize");
        json.as_object_mut()
            .expect("report JSON should be an object")
            .remove("modified_columns");
        let legacy: DiffReport =
            serde_json::from_value(json).expect("legacy report should deserialize");
        assert!(legacy.modified_columns.is_none());
    }
}
