//! `hepta_dbcli load` CLI argument parsing (issue #98).

use clap::Args;

/// `--format` values: `auto` falls back through jsonl → json → csv; the
/// explicit formats are strict (the file must exist in exactly that format).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum LoadFormat {
    Auto,
    Jsonl,
    Json,
    Csv,
}

impl LoadFormat {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            LoadFormat::Auto => "auto",
            LoadFormat::Jsonl => "jsonl",
            LoadFormat::Json => "json",
            LoadFormat::Csv => "csv",
        }
    }
}

#[derive(Args, Debug)]
pub(crate) struct LoadArgs {
    /// Directory holding generated data (`<table>.jsonl|json|csv`)
    #[arg(long)]
    pub data: String,

    /// Data file format (auto discovers jsonl → json → csv)
    #[arg(long, default_value = "auto")]
    pub format: LoadFormat,

    /// Schema qualifier for the tables (defaults to the connection default)
    #[arg(long)]
    pub schema: Option<String>,

    /// Comma-separated table names to load (defaults to every file found)
    #[arg(long, value_delimiter = ',')]
    pub tables: Option<Vec<String>>,

    /// Show the plan (order, files, rows) without touching the database
    #[arg(long)]
    pub dry_run: bool,

    /// Require the file columns to exactly match the table columns. Without
    /// this, a missing nullable (or defaulted) column loads as NULL and only a
    /// NOT NULL column with no default rejects (issue #113).
    #[arg(long)]
    pub strict_columns: bool,
}

impl LoadArgs {
    /// Format selector as the `auto|jsonl|json|csv` string used by the plan.
    pub(crate) fn format_str(&self) -> &'static str {
        self.format.as_str()
    }
}
