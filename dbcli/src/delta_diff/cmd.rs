// ─── delta-diff CLI argument parsing (clap) ────────────────────────────
//
// Implements §2.2 of the delta-diff design doc: the full parameter list,
// mutual-exclusion rules, and the exit-code contract (§2.3) for the
// argument layer. Strategy execution lives in the engine/strategy modules.

use clap::Args;

use crate::cli::OutputFormat;

// ─── Enums ─────────────────────────────────────────────────────────────

/// 比对策略（§2.2 --strategy）
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Strategy {
    Auto,
    Hashdiff,
    Joindiff,
    Bucketdiff,
    Iblt,
    Keyeddiff,
}

impl std::fmt::Display for Strategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Strategy::Auto => "auto",
            Strategy::Hashdiff => "hashdiff",
            Strategy::Joindiff => "joindiff",
            Strategy::Bucketdiff => "bucketdiff",
            Strategy::Iblt => "iblt",
            Strategy::Keyeddiff => "keyeddiff",
        };
        write!(f, "{s}")
    }
}

/// 一致性模式（§2.2 --consistency）
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ConsistencyMode {
    Snapshot,
    None,
}

impl std::fmt::Display for ConsistencyMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ConsistencyMode::Snapshot => "snapshot",
            ConsistencyMode::None => "none",
        };
        write!(f, "{s}")
    }
}

// ─── CLI Arguments ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Args)]
pub(crate) struct DeltaDiffArgs {
    /// 左数据源连接名（对应配置文件中的 connections）
    #[arg(long)]
    pub left: String,

    /// 右数据源连接名
    #[arg(long)]
    pub right: String,

    /// 表名（左右相同）
    #[arg(long, conflicts_with_all = ["left_table", "right_table"])]
    pub table: Option<String>,

    /// 左表名（与右表不同名时使用）
    #[arg(long)]
    pub left_table: Option<String>,

    /// 右表名
    #[arg(long)]
    pub right_table: Option<String>,

    /// Schema/数据库名（覆盖连接默认库）
    #[arg(long, conflicts_with_all = ["left_schema", "right_schema"])]
    pub schema: Option<String>,

    /// 左 Schema
    #[arg(long)]
    pub left_schema: Option<String>,

    /// 右 Schema
    #[arg(long)]
    pub right_schema: Option<String>,

    /// 主键/比对键列，逗号分隔（自动发现失败时指定）
    #[arg(long)]
    pub key: Option<String>,

    /// 要比对的列，逗号分隔（默认全部）
    #[arg(long)]
    pub columns: Option<String>,

    /// WHERE 条件（两侧同时应用；禁止分号）
    #[arg(
        long = "where",
        value_name = "CONDITION",
        conflicts_with = "update_column"
    )]
    pub where_condition: Option<String>,

    /// 增量比对列（与 --where 互斥）
    #[arg(long)]
    pub update_column: Option<String>,

    /// 增量窗口，如 "1 day"、"2026-08-01 00:00:00"
    #[arg(long)]
    pub update_since: Option<String>,

    /// 比对策略：auto | hashdiff | joindiff | bucketdiff | iblt | keyeddiff
    #[arg(long, value_enum, default_value = "auto")]
    pub strategy: Strategy,

    /// Hashdiff 二分因子
    #[arg(long, default_value_t = 32)]
    pub bisection_factor: usize,

    /// Hashdiff 行级阈值
    #[arg(long, default_value_t = 16384)]
    pub bisection_threshold: usize,

    /// 一致性模式：snapshot | none
    #[arg(long, value_enum, default_value = "snapshot")]
    pub consistency: ConsistencyMode,

    /// 对差异行二次复核（snapshot 模式下默认开启）
    #[arg(long)]
    pub recheck: bool,

    /// 差异行采样上限
    #[arg(long, default_value_t = 1000)]
    pub sample: usize,

    /// 仅输出统计，不输出差异明细
    #[arg(long)]
    pub summary_only: bool,

    /// 预检模式：输出策略、行数估算、分片计划，不执行比对
    #[arg(long)]
    pub dry_run: bool,

    /// 输出格式：table | json | csv | vertical
    #[arg(long, default_value = "table")]
    pub format: OutputFormat,

    /// 输出到文件
    #[arg(long)]
    pub output: Option<String>,

    /// 总并发度（两侧各自不超过 ⌈N/2⌉ 个会话）
    #[arg(long, default_value_t = 4)]
    pub threads: usize,

    /// 单条查询超时（秒）
    #[arg(long, default_value_t = 300)]
    pub statement_timeout: u64,

    /// 断点续传文件路径（JSONL）
    #[arg(long)]
    pub checkpoint: Option<String>,

    /// 输出分片级进度及每步执行的 SQL 到 stderr（避免在共享 CI 日志中泄露 --where 内容）
    #[arg(long)]
    pub verbose: bool,

    /// IBLT 预期差异容量 d [默认: 65536]
    #[arg(long, default_value_t = 65536)]
    pub iblt_capacity: u64,

    /// 与 --strategy iblt 联用：解码失败时报错（exit 2）而非回退
    #[arg(long)]
    pub strict: bool,

    /// Keyeddiff: pull all filtered rows when max(COUNT) is at most this (default 4096)
    #[arg(long, default_value_t = 4096)]
    pub fetch_all_threshold: u64,
}

// ─── Helpers & validation ──────────────────────────────────────────────

impl DeltaDiffArgs {
    pub(crate) fn left_table_name(&self) -> Option<&str> {
        self.left_table.as_deref().or(self.table.as_deref())
    }

    pub(crate) fn right_table_name(&self) -> Option<&str> {
        self.right_table.as_deref().or(self.table.as_deref())
    }

    pub(crate) fn key_list(&self) -> Vec<String> {
        split_csv(self.key.as_deref())
    }

    pub(crate) fn columns_list(&self) -> Vec<String> {
        split_csv(self.columns.as_deref())
    }

    pub(crate) fn recheck_effective(&self) -> bool {
        self.recheck || matches!(self.consistency, ConsistencyMode::Snapshot)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if let Some(cond) = &self.where_condition {
            if cond.contains(';') {
                return Err(
                    "--where must not contain ';' (multi-statement injection guard)".to_string(),
                );
            }
        }

        if self.update_since.is_some() && self.update_column.is_none() {
            return Err("--update-since requires --update-column".to_string());
        }

        if self.left_table_name().is_none() {
            return Err("missing table for left side: specify --table or --left-table".to_string());
        }
        if self.right_table_name().is_none() {
            return Err(
                "missing table for right side: specify --table or --right-table".to_string(),
            );
        }

        Ok(())
    }
}

fn split_csv(s: Option<&str>) -> Vec<String> {
    s.map(|v| {
        v.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect()
    })
    .unwrap_or_default()
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// 独立解析入口：DeltaDiffArgs 以 `#[derive(Args)]` 扁平嵌入，
    /// 经外层 Parser 包装以复用真实 clap 解析路径。
    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: DeltaDiffArgs,
    }

    fn parse(argv: &[&str]) -> Result<DeltaDiffArgs, clap::Error> {
        TestCli::try_parse_from(argv).map(|c| c.args)
    }

    #[test]
    fn defaults_applied() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "dev",
            "--right",
            "prod",
            "--table",
            "orders",
        ])
        .unwrap();
        assert_eq!(args.strategy, Strategy::Auto);
        assert_eq!(args.fetch_all_threshold, 4096);
        assert_eq!(args.bisection_factor, 32);
        assert_eq!(args.bisection_threshold, 16384);
        assert_eq!(args.consistency, ConsistencyMode::Snapshot);
        assert_eq!(args.sample, 1000);
        assert_eq!(args.format, OutputFormat::Table);
        assert_eq!(args.threads, 4);
        assert_eq!(args.statement_timeout, 300);
        assert!(!args.recheck);
        assert!(!args.dry_run);
        assert!(!args.summary_only);
        assert!(!args.verbose);
        assert_eq!(args.left_table_name(), Some("orders"));
        assert_eq!(args.right_table_name(), Some("orders"));
        assert!(args.validate().is_ok());
    }

    #[test]
    fn strategy_keyeddiff_parses() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--strategy",
            "keyeddiff",
        ])
        .expect("parse");
        assert_eq!(args.strategy, Strategy::Keyeddiff);
        assert_eq!(args.strategy.to_string(), "keyeddiff");
    }

    #[test]
    fn snapshot_mode_implies_recheck_by_default() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "dev",
            "--right",
            "prod",
            "--table",
            "orders",
        ])
        .unwrap();
        assert!(!args.recheck);
        assert!(args.recheck_effective());
    }

    #[test]
    fn explicit_values_parsed() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "dev",
            "--right",
            "prod",
            "--left-table",
            "orders_v1",
            "--right-table",
            "orders_v2",
            "--left-schema",
            "s1",
            "--right-schema",
            "s2",
            "--strategy",
            "hashdiff",
            "--bisection-factor",
            "64",
            "--bisection-threshold",
            "8192",
            "--consistency",
            "none",
            "--recheck",
            "--sample",
            "500",
            "--summary-only",
            "--dry-run",
            "--format",
            "json",
            "--output",
            "diff.json",
            "--threads",
            "8",
            "--statement-timeout",
            "600",
            "--checkpoint",
            "cp.jsonl",
            "--verbose",
        ])
        .unwrap();
        assert_eq!(args.strategy, Strategy::Hashdiff);
        assert_eq!(args.bisection_factor, 64);
        assert_eq!(args.bisection_threshold, 8192);
        assert_eq!(args.consistency, ConsistencyMode::None);
        assert!(args.recheck);
        assert!(args.recheck_effective());
        assert_eq!(args.sample, 500);
        assert!(args.summary_only);
        assert!(args.dry_run);
        assert_eq!(args.format, OutputFormat::Json);
        assert_eq!(args.output.as_deref(), Some("diff.json"));
        assert_eq!(args.threads, 8);
        assert_eq!(args.statement_timeout, 600);
        assert_eq!(args.checkpoint.as_deref(), Some("cp.jsonl"));
        assert!(args.verbose);
        assert_eq!(args.left_table_name(), Some("orders_v1"));
        assert_eq!(args.right_table_name(), Some("orders_v2"));
    }

    #[test]
    fn update_column_conflicts_with_where() {
        let err = parse(&[
            "delta-diff",
            "--left",
            "dev",
            "--right",
            "prod",
            "--table",
            "orders",
            "--update-column",
            "updated_at",
            "--where",
            "status = 1",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn where_with_semicolon_rejected() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "dev",
            "--right",
            "prod",
            "--table",
            "orders",
            "--where",
            "id = 1; DROP TABLE orders",
        ])
        .unwrap();
        let err = args.validate().unwrap_err();
        assert!(err.contains(';'), "unexpected error: {err}");
    }

    #[test]
    fn key_and_columns_are_comma_split() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "dev",
            "--right",
            "prod",
            "--table",
            "orders",
            "--key",
            "id, user_id",
            "--columns",
            "id,amount,status",
        ])
        .unwrap();
        assert_eq!(args.key_list(), vec!["id", "user_id"]);
        assert_eq!(args.columns_list(), vec!["id", "amount", "status"]);
    }

    #[test]
    fn key_and_columns_default_empty() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "dev",
            "--right",
            "prod",
            "--table",
            "orders",
        ])
        .unwrap();
        assert!(args.key_list().is_empty());
        assert!(args.columns_list().is_empty());
    }

    #[test]
    fn invalid_strategy_rejected() {
        let err = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--strategy",
            "hash",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn invalid_format_rejected() {
        let err = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--format",
            "xml",
        ])
        .unwrap_err();
        // FromStr-based parsing reports ValueValidation (ValueEnum reports InvalidValue)
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn invalid_consistency_rejected() {
        let err = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--consistency",
            "repeatable",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn table_conflicts_with_left_table() {
        let err = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--left-table",
            "t2",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn schema_conflicts_with_left_schema() {
        let err = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--schema",
            "s",
            "--left-schema",
            "s2",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn missing_table_rejected_by_validation() {
        let args = parse(&["delta-diff", "--left", "a", "--right", "b"]).unwrap();
        assert!(args.validate().is_err());
    }

    #[test]
    fn update_since_requires_update_column() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--update-since",
            "1 day",
        ])
        .unwrap();
        assert!(args.validate().is_err());
    }
}
