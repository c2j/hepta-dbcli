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
    Naivediff,
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
            Strategy::Naivediff => "naivediff",
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ExportFormat {
    Csv,
    Jsonl,
    Json,
    Sql,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ApplyTo {
    Left,
    Right,
}

/// 终端抽样模式：diverse 按差异形态挑选，prefix 为 key 序前 N 行
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum SampleMode {
    /// 按差异形态挑选：status 配额 + 变化列覆盖 + 签名去重（默认）
    Diverse,
    /// key 序前 N 行（历史行为）
    Prefix,
}

// ─── CLI Arguments ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Args)]
pub(crate) struct DeltaDiffArgs {
    /// 左数据源连接名（对应配置文件中的 connections；与 --left-url 二选一）
    #[arg(long, conflicts_with = "left_url")]
    pub left: Option<String>,

    /// 左数据源 URL（免配置直连，如 duckdb:///tmp/copy.duckdb；与 --left 互斥）
    #[arg(long, conflicts_with = "left")]
    pub left_url: Option<String>,

    /// 右数据源连接名（与 --right-url 二选一）
    #[arg(long, conflicts_with = "right_url")]
    pub right: Option<String>,

    /// 右数据源 URL（免配置直连；与 --right 互斥）
    #[arg(long, conflicts_with = "right")]
    pub right_url: Option<String>,

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

    /// 不参与比对的列，逗号分隔（在发现/--columns 之后做减法；两侧同名）
    #[arg(long)]
    pub exclude_columns: Option<String>,

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

    /// 比对策略：auto | hashdiff | joindiff | bucketdiff | iblt | keyeddiff | naivediff
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

    /// 终端差异明细行数上限（只裁终端；0 = 终端也打全量）
    #[arg(long, default_value_t = 20)]
    pub sample: usize,

    /// 终端抽样模式：diverse（按差异形态）| prefix（key 序前 N 行）。
    /// 只影响终端样本与 MCP payload，不影响 --export 全量
    #[arg(long, value_enum, default_value = "diverse")]
    pub sample_mode: SampleMode,

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

    /// IBLT 自适应两轮：忽略 --iblt-capacity，第一轮 m=64，失败后按
    /// d̂=⌈Σ|cnt|/2⌉ 放大一轮 m=clamp(3·d̂)，两轮都失败才回退 hashdiff
    #[arg(long, default_value_t = false)]
    pub iblt_auto_capacity: bool,

    /// 与 --strategy iblt 联用：解码失败时报错（exit 2）而非回退
    #[arg(long)]
    pub strict: bool,

    /// Keyeddiff: pull all filtered rows when max(COUNT) is at most this (default 4096)
    #[arg(long, default_value_t = 4096)]
    pub fetch_all_threshold: u64,

    /// Naivediff: refuse when max(COUNT) exceeds this (0 = unlimited)
    #[arg(long, default_value_t = 200_000)]
    pub naive_max_rows: u64,

    /// 终端显示全部比对列，不只变化列
    #[arg(long)]
    pub wide: bool,

    /// 写出全部差异（后缀推断 csv/jsonl/json/sql）
    #[arg(long)]
    pub export: Option<String>,

    /// 覆盖 --export 后缀推断
    #[arg(long, value_enum)]
    pub export_format: Option<ExportFormat>,

    /// 文件带完整左右行值（.sql 自动打开）
    #[arg(long)]
    pub export_rows: bool,

    /// 写 .sql 时必填：让哪一侧变成另一侧
    #[arg(long, value_enum)]
    pub apply_to: Option<ApplyTo>,

    /// keyless 不要回查真实行
    #[arg(long)]
    pub no_fetch_sample: bool,

    /// Trim trailing blanks of fixed-width char columns on both sides before
    /// hashing/comparison (CHAR/NCHAR on Oracle, character/bpchar on GaussDB).
    /// While active, all-blank fixed-width values are indistinguishable from NULL.
    #[arg(long)]
    pub rtrim_char_columns: bool,
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

    pub(crate) fn exclude_columns_list(&self) -> Vec<String> {
        split_csv(self.exclude_columns.as_deref())
    }

    pub(crate) fn recheck_effective(&self) -> bool {
        self.recheck || matches!(self.consistency, ConsistencyMode::Snapshot)
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.left.is_none() && self.left_url.is_none() {
            return Err(
                "missing left side: specify --left <connection> or --left-url <url>".to_string(),
            );
        }
        if self.right.is_none() && self.right_url.is_none() {
            return Err(
                "missing right side: specify --right <connection> or --right-url <url>".to_string(),
            );
        }
        for (flag, url) in [
            ("--left-url", &self.left_url),
            ("--right-url", &self.right_url),
        ] {
            if let Some(u) = url {
                let trimmed = u.trim();
                if trimmed.is_empty() || !trimmed.contains("://") {
                    return Err(format!(
                        "{flag} expects scheme://... (e.g. duckdb:///tmp/copy.duckdb), got '{}'",
                        u
                    ));
                }
            }
        }

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

        if self.export.is_some() || self.export_format.is_some() || self.apply_to.is_some() {
            let fmt = infer_export_format(self.export.as_deref(), self.export_format)?;
            if fmt == ExportFormat::Sql {
                if self.apply_to.is_none() {
                    return Err("--export .sql requires --apply-to left|right".to_string());
                }
            } else if self.apply_to.is_some() {
                return Err("--apply-to is only valid with --export-format sql / *.sql".to_string());
            }
        }

        Ok(())
    }

    pub(crate) fn export_rows_effective(&self) -> bool {
        self.export_rows
            || infer_export_format(self.export.as_deref(), self.export_format)
                .map(|f| f == ExportFormat::Sql)
                .unwrap_or(false)
    }
}

pub(crate) fn infer_export_format(
    path: Option<&str>,
    override_fmt: Option<ExportFormat>,
) -> Result<ExportFormat, String> {
    if let Some(fmt) = override_fmt {
        return Ok(fmt);
    }
    let path = path.ok_or_else(|| {
        "cannot infer --export format from PATH; pass --export-format".to_string()
    })?;
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "csv" => Ok(ExportFormat::Csv),
        "jsonl" => Ok(ExportFormat::Jsonl),
        "json" => Ok(ExportFormat::Json),
        "sql" => Ok(ExportFormat::Sql),
        _ => Err("cannot infer --export format from PATH; pass --export-format".to_string()),
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
    fn left_url_and_right_url_parse_as_inline_sides() {
        let args = parse(&[
            "delta-diff",
            "--left-url",
            "mysql://u:p@127.0.0.1:3306/db",
            "--right-url",
            "duckdb:///tmp/copy.duckdb",
            "--table",
            "t",
        ])
        .expect("args should parse");
        assert_eq!(
            args.left_url.as_deref(),
            Some("mysql://u:p@127.0.0.1:3306/db")
        );
        assert_eq!(args.right_url.as_deref(), Some("duckdb:///tmp/copy.duckdb"));
        // Sides may be mixed: name on one side, URL on the other.
        assert_eq!(args.left, None);
        assert_eq!(args.right, None);
    }

    #[test]
    fn left_url_conflicts_with_left_name() {
        let err = parse(&[
            "delta-diff",
            "--left",
            "prod",
            "--left-url",
            "mysql://u:p@127.0.0.1:3306/db",
            "--right",
            "staging",
            "--table",
            "t",
        ])
        .expect_err("--left-url must conflict with --left");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn right_url_conflicts_with_right_name() {
        let err = parse(&[
            "delta-diff",
            "--left",
            "prod",
            "--right",
            "staging",
            "--right-url",
            "duckdb:///tmp/copy.duckdb",
            "--table",
            "t",
        ])
        .expect_err("--right-url must conflict with --right");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn url_side_requires_url_or_name_before_validation() {
        // Both sides given as URLs: names are not needed.
        let args = parse(&[
            "delta-diff",
            "--left-url",
            "duckdb:///tmp/a.duckdb",
            "--right-url",
            "duckdb:///tmp/b.duckdb",
            "--table",
            "t",
        ])
        .expect("url-only invocation should parse");
        assert!(args.validate().is_ok());

        // Neither --left nor --left-url: parse succeeds, validate() rejects.
        let args = parse(&[
            "delta-diff",
            "--right-url",
            "duckdb:///tmp/b.duckdb",
            "--table",
            "t",
        ])
        .expect("args should parse");
        let err = args.validate().expect_err("missing left side must fail");
        assert!(err.contains("--left"), "unexpected error: {err}");
    }

    #[test]
    fn left_url_rejects_scheme_less_value_at_validation() {
        let args = parse(&[
            "delta-diff",
            "--left-url",
            "/tmp/not-a-url.duckdb",
            "--right",
            "staging",
            "--table",
            "t",
        ])
        .expect("args should parse");
        let err = args
            .validate()
            .expect_err("scheme-less URL must be rejected");
        assert!(err.contains("--left-url"), "unexpected error: {err}");
    }

    #[test]
    fn sample_mode_default_is_diverse() {
        let args = parse(&["delta-diff", "--left", "a", "--right", "b", "--table", "t"])
            .expect("args should parse");
        assert!(matches!(args.sample_mode, SampleMode::Diverse));
    }

    #[test]
    fn sample_mode_prefix_parsed() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--sample-mode",
            "prefix",
        ])
        .expect("args should parse");
        assert!(matches!(args.sample_mode, SampleMode::Prefix));
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
        assert_eq!(args.sample, 20);
        assert_eq!(args.format, OutputFormat::Table);
        assert_eq!(args.threads, 4);
        assert_eq!(args.statement_timeout, 300);
        assert!(!args.recheck);
        assert!(!args.dry_run);
        assert!(!args.summary_only);
        assert!(!args.verbose);
        assert!(!args.wide);
        assert!(args.export.is_none());
        assert!(args.export_format.is_none());
        assert!(!args.export_rows);
        assert!(args.apply_to.is_none());
        assert!(!args.no_fetch_sample);
        assert!(!args.rtrim_char_columns);
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
    fn naive_max_rows_defaults_to_200k() {
        let args =
            parse(&["delta-diff", "--left", "a", "--right", "b", "--table", "t"]).expect("parse");
        assert_eq!(args.naive_max_rows, 200_000);
    }

    #[test]
    fn strategy_naivediff_parses_and_displays() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "l",
            "--right",
            "r",
            "--table",
            "t",
            "--strategy",
            "naivediff",
        ])
        .expect("naivediff must be a valid value");
        assert_eq!(args.strategy, Strategy::Naivediff);
        assert_eq!(args.strategy.to_string(), "naivediff");
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
    fn exclude_columns_flag_splits_csv() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "dev",
            "--right",
            "prod",
            "--table",
            "orders",
            "--columns",
            "id,amount,status",
            "--exclude-columns",
            "etl_time, remark ",
        ])
        .unwrap();
        assert_eq!(args.exclude_columns_list(), vec!["etl_time", "remark"]);
        assert_eq!(args.columns_list(), vec!["id", "amount", "status"]);
    }

    #[test]
    fn exclude_columns_defaults_empty() {
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
        assert!(args.exclude_columns_list().is_empty());
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

    #[test]
    fn sample_zero_allowed() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--sample",
            "0",
        ])
        .unwrap();
        assert_eq!(args.sample, 0);
        assert!(args.validate().is_ok());
    }

    #[test]
    fn export_sql_requires_apply_to() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--export",
            "patch.sql",
        ])
        .unwrap();
        let err = args.validate().unwrap_err();
        assert!(err.contains("--apply-to"), "{err}");
    }

    #[test]
    fn apply_to_rejected_unless_sql() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--export",
            "out.csv",
            "--apply-to",
            "left",
        ])
        .unwrap();
        let err = args.validate().unwrap_err();
        assert!(err.contains("--apply-to"), "{err}");
    }

    #[test]
    fn export_unknown_suffix_requires_format() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--export",
            "out.bin",
        ])
        .unwrap();
        let err = args.validate().unwrap_err();
        assert!(err.contains("--export-format"), "{err}");
    }

    #[test]
    fn export_format_overrides_suffix() {
        assert_eq!(
            infer_export_format(Some("out.csv"), Some(ExportFormat::Jsonl)).unwrap(),
            ExportFormat::Jsonl
        );
        assert_eq!(
            infer_export_format(Some("patch.sql"), None).unwrap(),
            ExportFormat::Sql
        );
        assert!(infer_export_format(Some("out.bin"), None).is_err());
    }

    #[test]
    fn export_sql_with_apply_to_ok() {
        let args = parse(&[
            "delta-diff",
            "--left",
            "a",
            "--right",
            "b",
            "--table",
            "t",
            "--export",
            "patch.sql",
            "--apply-to",
            "left",
        ])
        .unwrap();
        assert_eq!(args.apply_to, Some(ApplyTo::Left));
        assert!(args.validate().is_ok());
        assert!(args.export_rows_effective());
    }
}
