use crate::synth::export::{export, ExportFormat};
use crate::synth::generator::{generate, generate_unique_primary_keys, GeneratorConfig};
use crate::synth::marginal::{
    compute_gaussian_correlation, CategoricalParams, EcdfFitter, Marginal, MarginalFitter,
    NormalParams,
};
use crate::synth::model::{ColumnModel, CopulaInfo, LogicalType, Provenance, TableModel};
use crate::synth::profile::TableProfile;
use crate::synth::rules::SynthRules;
use crate::synth::rules_draft::ForeignKeyInfo;
use clap::{Args, Subcommand};
use std::collections::HashMap;
use std::path::Path;

/// Cap on categorical `top_values` kept while training. `full` stores every
/// observed level; the default of 50 matches the historical hard cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CategoricalTopK {
    Limit(usize),
    Full,
}

impl CategoricalTopK {
    pub fn cap(self) -> Option<usize> {
        match self {
            Self::Limit(n) => Some(n),
            Self::Full => None,
        }
    }
}

fn parse_categorical_top_k(s: &str) -> Result<CategoricalTopK, String> {
    if s.eq_ignore_ascii_case("full") {
        return Ok(CategoricalTopK::Full);
    }
    let n: usize = s.parse().map_err(|_| {
        format!("invalid --categorical-top-k '{s}': expected a positive integer or 'full'")
    })?;
    if n == 0 {
        Err("--categorical-top-k must be a positive integer or 'full'".to_string())
    } else {
        Ok(CategoricalTopK::Limit(n))
    }
}

// ─── CLI 参数 ───────────────────────────────────────────────────────────

#[derive(Args, Debug)]
pub struct SynthArgs {
    #[command(subcommand)]
    pub command: SynthCommand,
}

#[derive(Subcommand, Debug)]
pub enum SynthCommand {
    /// Train table models from database samples
    Train {
        /// Connection name (defaults to configured default connection)
        #[arg(short, long)]
        name: Option<String>,

        /// Comma-separated table names
        #[arg(short, long)]
        tables: String,

        /// Schema qualifier for the tables (defaults to the connection default)
        #[arg(long)]
        schema: Option<String>,

        /// Output directory for model and profile JSON files
        #[arg(short, long, default_value = ".synth")]
        output: String,

        /// Max rows sampled per table
        #[arg(long, default_value_t = 10_000)]
        sample: usize,

        /// Categorical top_values cap (`N` or `full`; default 50)
        #[arg(long, default_value = "50", value_parser = parse_categorical_top_k)]
        categorical_top_k: CategoricalTopK,

        /// Rules YAML supplying per-column overrides (`columns.<name>.marginal`)
        #[arg(long)]
        rules: Option<String>,

        /// Fraction of the sampled rows kept as the report holdout
        /// (`0` disables the baseline file)
        #[arg(long, default_value_t = 0.1)]
        holdout_ratio: f64,
    },

    /// Draft a rules YAML from database foreign keys
    RulesDraft {
        /// Connection name (defaults to configured default connection)
        #[arg(short, long)]
        name: Option<String>,

        /// Comma-separated table names
        #[arg(short, long)]
        tables: String,

        /// Schema to scan for foreign keys (defaults to the connection default)
        #[arg(long)]
        schema: Option<String>,

        /// Path of the rules YAML to write
        #[arg(short, long, default_value = "synth-rules.yaml")]
        output: String,

        /// Directory holding trained profiles; enables unique-FK detection
        #[arg(long, default_value = ".synth")]
        models: String,

        #[command(flatten)]
        mine: MineArgs,
    },

    /// Generate synthetic rows from trained models and rules
    Generate {
        /// Directory holding trained model JSON files
        #[arg(long, default_value = ".synth")]
        models: String,

        /// Rules YAML path
        #[arg(long, default_value = "synth-rules.yaml")]
        rules: String,

        /// Output directory for generated data
        #[arg(short, long, default_value = "synth-out")]
        output: String,

        /// Rows per table (default 100)
        #[arg(long)]
        rows: Option<usize>,

        /// Deterministic seed
        #[arg(long)]
        seed: Option<u64>,

        /// Output format: csv, jsonl, json, sql
        #[arg(short, long, default_value = "csv")]
        format: String,

        /// Clip generated values to training min/max
        #[arg(
            long,
            action = clap::ArgAction::Set,
            num_args = 0..=1,
            default_missing_value = "true",
            default_value_t = true
        )]
        enforce_min_max_values: bool,

        /// Omit schema qualifiers from SQL export (legacy `INSERT INTO t`)
        #[arg(long, default_value_t = false)]
        no_schema_qualifier: bool,
    },

    /// Validate a trained model file
    Validate {
        /// Path to the model JSON file
        #[arg(short, long)]
        model: String,
    },

    /// Score generated data against the holdout baseline recorded by `train`
    Report {
        /// Directory holding trained model JSON files
        #[arg(long, default_value = ".synth")]
        models: String,

        /// Directory with generated data (`<table>.csv|jsonl|json`); generated
        /// on the fly when omitted
        #[arg(long)]
        data: Option<String>,

        /// Rules YAML; supplies the foreign keys scored in the `fk` section
        #[arg(long)]
        rules: Option<String>,

        /// Connection whose real keys back the `fk` section; `--against-db`
        /// without a value uses the default connection
        #[arg(long, num_args = 0..=1, default_missing_value = "")]
        against_db: Option<String>,

        /// Rows per table when generating on the fly (ignored with `--data`)
        #[arg(long, default_value_t = 1_000)]
        rows: usize,

        /// Deterministic seed for on-the-fly generation
        #[arg(long)]
        seed: Option<u64>,

        /// Write the report JSON to this path
        #[arg(short, long)]
        output: Option<String>,

        /// Exit non-zero when the overall score falls below this threshold
        #[arg(long)]
        min_score: Option<f64>,

        /// Treat a missing report baseline as an error instead of skipping
        #[arg(long, default_value_t = false)]
        strict: bool,
    },
}

/// `rules-draft` conditional-rule mining options (issue #69).
///
/// Mining is off by default: without `--mine` the draft is byte-identical to
/// the pre-#69 output. Candidates are only ever written as comments (see
/// `synth::mine`), never enabled.
#[derive(Args, Debug, Clone)]
pub struct MineArgs {
    /// Mine conditional `A=a => B=b` candidates from sampled rows (default off)
    #[arg(long, default_value_t = false)]
    pub mine: bool,

    /// Minimum `P(B=b | A=a)` for a candidate
    #[arg(long, default_value_t = 0.95)]
    pub mine_confidence: f64,

    /// Minimum antecedent support, and the minimum total-variation distance
    /// between the conditional and the marginal distribution
    #[arg(long, default_value_t = 0.05)]
    pub mine_support: f64,

    /// Cap on directed column pairs scanned per table (guards the O(c^2) scan)
    #[arg(long, default_value_t = 2000)]
    pub mine_max_pairs: usize,

    /// Write the full candidate list to this path (comments stay in the YAML)
    #[arg(long)]
    pub emit_candidates: Option<String>,

    /// Mine PII-looking columns anyway. Default skips them so raw training
    /// values never surface in candidate comments; only use this on data you
    /// know is fake PII-shaped.
    #[arg(long, default_value_t = false)]
    pub keep_pii_columns: bool,
}

impl Default for MineArgs {
    fn default() -> Self {
        Self {
            mine: false,
            mine_confidence: 0.95,
            mine_support: 0.05,
            mine_max_pairs: 2000,
            emit_candidates: None,
            keep_pii_columns: false,
        }
    }
}

// ─── 模型拟合（纯函数，无 IO）────────────────────────────────────────────

pub(crate) fn build_model(
    table: &str,
    dialect: &str,
    profile: &TableProfile,
    rows: &[Vec<serde_json::Value>],
    pk: Vec<String>,
    schema: Option<String>,
) -> Result<(TableModel, Vec<String>), String> {
    build_model_with_overrides(table, dialect, profile, rows, pk, schema, None)
}

/// Build a table model, optionally forcing a marginal family per column from
/// the rules file (`columns.<name>.marginal`). Forced families bypass the KS
/// auto-selection; an override that cannot apply to its column fails the table
/// (see [`validate_forced_marginals`]) rather than dropping the column, and a
/// family whose fitted parameters are unusable falls back to auto-selection
/// with a warning.
pub(crate) fn build_model_with_overrides(
    table: &str,
    dialect: &str,
    profile: &TableProfile,
    rows: &[Vec<serde_json::Value>],
    pk: Vec<String>,
    schema: Option<String>,
    forced: Option<&HashMap<String, String>>,
) -> Result<(TableModel, Vec<String>), String> {
    if profile.columns.is_empty() {
        return Err(format!("table '{}' has no columns to model", table));
    }
    validate_forced_marginals(table, profile, forced)?;

    let mut columns = HashMap::new();
    let mut skipped = Vec::new();
    for (col_name, col_profile) in &profile.columns {
        let forced_family = forced.and_then(|m| m.get(col_name)).map(String::as_str);
        let col_idx = profile.column_order.iter().position(|c| c == col_name);
        if col_profile.logical_type == "datetime" {
            if let Some(fmt) = col_profile.datetime_format.as_deref() {
                columns.insert(
                    col_name.clone(),
                    fit_datetime_epoch_model(
                        col_name,
                        col_profile,
                        profile,
                        rows,
                        fmt,
                        forced_family,
                    ),
                );
                continue;
            }
            if col_profile.null_rate < 1.0 && col_profile.mean.is_none() {
                eprintln!(
                    "warning: column '{}': datetime format could not be inferred; falling back to legacy modelling",
                    col_name
                );
            }
        }
        let samples = col_idx
            .map(|idx| crate::synth::marginal::column_numeric_samples(rows, idx))
            .unwrap_or_default();
        match fit_marginal(col_name, col_profile, &samples, forced_family) {
            Ok(marginal) => {
                let logical_type = match col_profile.logical_type.as_str() {
                    "categorical" => LogicalType::Categorical,
                    "datetime" => LogicalType::Datetime,
                    _ => LogicalType::Numerical,
                };
                let rounding = if col_profile.is_integer {
                    Some(0)
                } else {
                    None
                };
                columns.insert(
                    col_name.clone(),
                    ColumnModel {
                        logical_type,
                        rounding,
                        datetime_epoch: None,
                        decimal_scale: col_profile.decimal_scale,
                        datetime_format: None,
                        min: col_profile.min.as_ref().and_then(|v| v.as_f64()),
                        max: col_profile.max.as_ref().and_then(|v| v.as_f64()),
                        null_rate: Some(col_profile.null_rate),
                        marginal,
                        pii: None,
                    },
                );
            }
            Err(_) => skipped.push(col_name.clone()),
        }
    }

    if columns.is_empty() {
        return Err(format!(
            "table '{}' has no trainable columns (all unsupported)",
            table
        ));
    }

    // A key column that did not survive training (unsupported type) cannot be
    // part of the generated table, so drop it from the recorded key too.
    let pk: Vec<String> = pk.into_iter().filter(|k| columns.contains_key(k)).collect();

    let mut column_order: Vec<String> = profile
        .column_order
        .iter()
        .filter(|c| columns.contains_key(*c))
        .cloned()
        .collect();
    if column_order.is_empty() {
        column_order = columns.keys().cloned().collect();
    }
    let n = column_order.len();

    // Compute correlation matrix from training rows
    let correlation = if rows.len() >= 2 && n > 0 {
        // rows 是全宽（含被跳过的列），先按过滤后的 column_order 投影，
        // 否则 compute_gaussian_correlation 的列索引会错位
        let col_pos: HashMap<&str, usize> = profile
            .column_order
            .iter()
            .enumerate()
            .map(|(i, name)| (name.as_str(), i))
            .collect();
        let projected: Vec<Vec<serde_json::Value>> = rows
            .iter()
            .map(|row| {
                column_order
                    .iter()
                    .filter_map(|c| col_pos.get(c.as_str()).and_then(|&i| row.get(i).cloned()))
                    .collect()
            })
            .collect();
        compute_gaussian_correlation(&projected, &column_order, &columns)
    } else {
        (0..n)
            .map(|i| (0..n).map(|j| if i == j { 1.0 } else { 0.0 }).collect())
            .collect()
    };

    let model = TableModel {
        version: 1,
        table: table.to_string(),
        dialect: dialect.to_string(),
        schema,
        provenance: Provenance {
            source: "native".to_string(),
            converter_version: None,
            sdv_version: None,
            truncated: false,
            trained_rows: Some(rows.len()),
        },
        pk,
        columns,
        copula: CopulaInfo {
            column_order,
            correlation,
        },
        fk_cardinality: Default::default(),
    };
    Ok((model, skipped))
}

fn fit_datetime_epoch_model(
    col_name: &str,
    col_profile: &crate::synth::profile::ColumnProfile,
    profile: &TableProfile,
    rows: &[Vec<serde_json::Value>],
    fmt: &str,
    forced: Option<&str>,
) -> ColumnModel {
    let col_idx = profile.column_order.iter().position(|c| c == col_name);
    let epochs: Vec<f64> = col_idx
        .map(|idx| {
            rows.iter()
                .filter_map(|row| {
                    row.get(idx)
                        .and_then(|v| crate::synth::datetime::parse_to_epoch(v, Some(fmt)))
                })
                .collect()
        })
        .unwrap_or_default();
    let (loc, scale, min, max) = if epochs.is_empty() {
        (
            col_profile.mean.unwrap_or(0.0),
            col_profile.std_dev.unwrap_or(0.0),
            col_profile.min.as_ref().and_then(serde_json::Value::as_f64),
            col_profile.max.as_ref().and_then(serde_json::Value::as_f64),
        )
    } else {
        let min = epochs.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = epochs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let loc = epochs.iter().sum::<f64>() / epochs.len() as f64;
        let var = epochs.iter().map(|x| (x - loc).powi(2)).sum::<f64>() / epochs.len() as f64;
        (loc, var.sqrt(), Some(min), Some(max))
    };
    ColumnModel {
        logical_type: LogicalType::Datetime,
        rounding: None,
        datetime_epoch: Some(true),
        decimal_scale: None,
        datetime_format: Some(fmt.to_string()),
        min,
        max,
        null_rate: Some(col_profile.null_rate),
        // Formatted datetimes keep the Normal marginal by default: the epoch
        // axis is calendar-driven, and putting it through auto-selection is a
        // separate decision. A rules override still wins, because the axis is
        // numeric and a user asking for it has a reason.
        marginal: match forced {
            Some(name) => match try_forced_marginal(&epochs, name) {
                Ok(Some(marginal)) => marginal,
                Ok(None) => {
                    eprintln!(
                        "warning: column '{}': forced marginal '{}' produced unusable parameters on the epoch axis; keeping the Normal marginal",
                        col_name, name
                    );
                    Marginal::Normal(NormalParams { loc, scale })
                }
                Err(err) => {
                    eprintln!(
                        "warning: column '{}': forced marginal error ({}); keeping the Normal marginal",
                        col_name, err
                    );
                    Marginal::Normal(NormalParams { loc, scale })
                }
            },
            None => Marginal::Normal(NormalParams { loc, scale }),
        },
        pii: None,
    }
}

fn fit_marginal(
    col_name: &str,
    col: &crate::synth::profile::ColumnProfile,
    samples: &[f64],
    forced: Option<&str>,
) -> Result<Marginal, String> {
    // Overrides reach this point already validated against the column
    // (`validate_forced_marginals`): a rule wins even when its family is not
    // the best fit, and only an unusable fit falls back to auto-selection.
    if let Some(name) = forced {
        if name == "categorical" {
            // Validated above: a value dictionary exists for this column.
            let top = col.top_values.as_deref().ok_or_else(|| {
                format!(
                    "column '{}': marginal 'categorical' needs a value dictionary",
                    col_name
                )
            })?;
            return Ok(categorical_from_top_values(top));
        }
        return match try_forced_marginal(samples, name) {
            Ok(Some(marginal)) => Ok(marginal),
            Ok(None) => {
                eprintln!(
                    "warning: column '{}': forced marginal '{}' produced unusable parameters; falling back to auto-selection",
                    col_name, name
                );
                fit_auto_marginal(col_name, col, samples)
            }
            Err(err) => Err(format!("column '{}': {}", col_name, err)),
        };
    }
    match col.logical_type.as_str() {
        "numerical" => {
            // Low-cardinality numeric columns stay on the value-dictionary
            // (top_values -> Categorical) path; the ECDF never swallows them.
            if let Some(top) = col.top_values.as_deref() {
                Ok(categorical_from_top_values(top))
            } else {
                fit_auto_marginal(col_name, col, samples)
            }
        }
        // Numeric-encoded datetimes (compact YYYYMMDD, epoch integers) get the
        // same shape treatment; date/timestamp strings fall through to the
        // frequency path.
        "datetime" if col.mean.is_some() => fit_auto_marginal(col_name, col, samples),
        // A datetime column with no observed value (all NULL) has no
        // distribution to fit: keep it in the model so generated data keeps
        // the column, and let the recorded null_rate drive NULL emission.
        "datetime" if col.top_values.is_none() => Ok(Marginal::Normal(NormalParams {
            loc: 0.0,
            scale: 0.0,
        })),
        "categorical" | "datetime" => {
            let top = col.top_values.as_deref().ok_or_else(|| {
                format!(
                    "column '{}' is {} but profile has no value frequencies; retrain",
                    col_name, col.logical_type
                )
            })?;
            Ok(categorical_from_top_values(top))
        }
        other => Err(format!(
            "column '{}' has logical type '{}'; training supports numerical, datetime, and categorical",
            col_name, other
        )),
    }
}

fn categorical_from_top_values(top: &[(String, f64)]) -> Marginal {
    Marginal::Categorical(CategoricalParams {
        values: top.iter().map(|(v, _)| v.clone()).collect(),
        weights: top.iter().map(|(_, w)| *w).collect(),
    })
}

/// Stable name of a fitted marginal, for diagnostics and reports.
pub(crate) fn marginal_name(marginal: &Marginal) -> &'static str {
    match marginal {
        Marginal::Normal(_) => "normal",
        Marginal::Beta(_) => "beta",
        Marginal::Gamma(_) => "gamma",
        Marginal::Uniform(_) => "uniform",
        Marginal::Ecdf(_) => "ecdf",
        Marginal::Categorical(_) => "categorical",
    }
}

/// KS auto-selection over {Normal, Beta, Gamma, Uniform, Ecdf} for a numeric
/// column, falling back to the profile moments when the sampled rows carry no
/// usable values (e.g. a model built from a profile alone).
fn fit_auto_marginal(
    col_name: &str,
    col: &crate::synth::profile::ColumnProfile,
    samples: &[f64],
) -> Result<Marginal, String> {
    // No usable samples (e.g. an all-NULL column): keep a degenerate point
    // mass so the column still exists in the model, matching the pre-selector
    // behaviour driven by the recorded null_rate.
    if !samples.iter().any(|v| v.is_finite()) {
        return Ok(Marginal::Normal(NormalParams {
            loc: col.mean.unwrap_or(0.0),
            scale: col.std_dev.unwrap_or(0.0),
        }));
    }
    crate::synth::marginal::fit_auto_numeric_marginal(samples)
        .map_err(|err| format!("column '{}': {}", col_name, err))
}

/// Serialized-size budget for a single column's marginal. The ECDF knot cap
/// keeps one column under this on any sample size (issue #66).
pub(crate) const COLUMN_MARGINAL_BUDGET_BYTES: usize = 16 * 1024;

/// Columns whose marginal exceeds [`COLUMN_MARGINAL_BUDGET_BYTES`], with the
/// measured size. Empty for every model the current knot cap can produce.
pub(crate) fn oversized_marginal_columns(model: &TableModel) -> Vec<(String, usize)> {
    let mut oversized: Vec<(String, usize)> = model
        .columns
        .iter()
        .filter_map(|(name, column)| {
            let size = serde_json::to_string_pretty(&column.marginal)
                .map(|json| json.len())
                .unwrap_or(0);
            (size > COLUMN_MARGINAL_BUDGET_BYTES).then(|| (name.clone(), size))
        })
        .collect();
    // Deterministic order: HashMap iteration is not stable.
    oversized.sort();
    oversized
}

pub(crate) fn parse_primary_key(result: &crate::backend::QueryResult) -> Vec<String> {
    crate::delta_diff::metadata::primary_key_columns(result)
}

/// Normalize catalog primary-key names to the casing of the sampled columns and
/// drop key columns absent from the sample, so every recorded key column can be
/// looked up in the trained model.
pub(crate) fn reconcile_primary_key(pk: Vec<String>, columns: &[String]) -> Vec<String> {
    pk.into_iter()
        .filter_map(|key| {
            columns
                .iter()
                .find(|name| name.eq_ignore_ascii_case(&key))
                .cloned()
        })
        .collect()
}

pub(crate) fn parse_column_types(result: &crate::backend::QueryResult) -> HashMap<String, String> {
    crate::delta_diff::metadata::column_name_and_type(result)
}

pub(crate) fn load_profiles(dir: &Path) -> Result<HashMap<String, TableProfile>, String> {
    let mut profiles = HashMap::new();
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("read models dir {}: {}", dir.display(), e))?;
    for entry in entries {
        let path = entry.map_err(|e| format!("read dir entry: {}", e))?.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(table) = stem.strip_suffix(".profile") else {
            continue;
        };
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            let profile = TableProfile::load(&path)?;
            profiles.insert(table.to_string(), profile);
        }
    }
    Ok(profiles)
}

// ─── 本地命令（无 DB）──────────────────────────────────────────────────

pub struct GenerateFlags {
    pub enforce_min_max_values: bool,
    pub no_schema_qualifier: bool,
}

pub fn run_generate(
    models_dir: &str,
    rules_path: &str,
    output_dir: &str,
    rows_per_table: Option<usize>,
    seed: Option<u64>,
    format: &str,
    flags: GenerateFlags,
) -> Result<(), String> {
    let models_dir = Path::new(models_dir);
    let rules_path = Path::new(rules_path);
    let output_dir = Path::new(output_dir);
    std::fs::create_dir_all(output_dir).map_err(|e| format!("create output dir: {}", e))?;

    let rules = SynthRules::load(rules_path)?;
    rules.validate()?;

    let mut models = HashMap::new();
    for entry in std::fs::read_dir(models_dir).map_err(|e| format!("read models dir: {}", e))? {
        let entry = entry.map_err(|e| format!("read dir entry: {}", e))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                if let Some(table_name) = name.strip_suffix(".model") {
                    let model = TableModel::load(&path)?;
                    models.insert(table_name.to_string(), model);
                }
            }
        }
    }

    let rows_map = rows_map_for_rules(&rules, rows_per_table);

    let config = GeneratorConfig {
        rows_per_table: rows_map,
        seed,
        enforce_min_max_values: flags.enforce_min_max_values,
    };

    let data = if format == "sql" {
        // Unique primary keys only matter where a relational constraint will
        // be applied; see `generate_unique_primary_keys` (issue #82).
        generate_unique_primary_keys(&models, &rules, &config)?
    } else {
        generate(&models, &rules, &config)?
    };

    let export_format = match format {
        "csv" => ExportFormat::Csv,
        "jsonl" => ExportFormat::Jsonl,
        "json" => ExportFormat::Json,
        "sql" => ExportFormat::Sql,
        other => return Err(format!("unsupported format: {}", other)),
    };

    let schemas = if flags.no_schema_qualifier {
        HashMap::new()
    } else {
        data.schemas.clone()
    };
    let payload = crate::synth::export::ExportPayload {
        tables: &data.tables,
        columns: &data.columns,
        dialect: &data.dialect,
        schemas,
    };
    export(&payload, &export_format, output_dir)?;

    for pool in &data.value_pools {
        if pool.is_within_tolerance() {
            continue;
        }
        let declared = pool
            .declared
            .iter()
            .map(|(value, share)| format!("{}={:.3}", value, share))
            .collect::<Vec<_>>()
            .join(", ");
        let actual = pool
            .actual
            .iter()
            .map(|(value, share)| format!("{}={:.3}", value, share))
            .collect::<Vec<_>>()
            .join(", ");
        // Declared weights are what the sampler draws from, so a deviation
        // this large means the pool did not take effect (#76-C). Honest
        // warning, no effect on the exit code.
        eprintln!(
            "warning: table '{}' column '{}': generated shares deviate from the declared weights by {:.3} (tolerance {:.2}); declared [{}] actual [{}]",
            pool.table,
            pool.column,
            pool.max_deviation,
            crate::synth::generator::VALUE_POOL_TOLERANCE,
            declared,
            actual
        );
    }

    for outcome in &data.branches {
        let line = format!(
            "branch '{}': target {:.3}, actual {:.3} ({:?}, {} round(s), {} row(s) rewritten, {} evaluation failure(s))",
            outcome.id,
            outcome.target_ratio,
            outcome.actual_ratio,
            outcome.status,
            outcome.rounds,
            outcome.flips,
            outcome.failed_evaluations
        );
        match outcome.status {
            crate::synth::generator::CoverageStatus::Pass => println!("{}", line),
            // A missed coverage target is a warning, not a failed generation
            // (plan §7 D2); the exit code stays 0.
            crate::synth::generator::CoverageStatus::Warn
            | crate::synth::generator::CoverageStatus::Fail => eprintln!("warning: {}", line),
        }
    }

    println!(
        "Generation complete. Data saved to {}",
        output_dir.display()
    );
    Ok(())
}

fn rows_map_for_rules(rules: &SynthRules, cli_rows: Option<usize>) -> HashMap<String, usize> {
    rules
        .tables
        .iter()
        .map(|table| (table.name.clone(), cli_rows.or(table.rows).unwrap_or(100)))
        .collect()
}

pub fn run_validate(model_path: &str) -> Result<(), String> {
    let model = TableModel::load(Path::new(model_path))?;

    println!("Model validation passed:");
    println!("  Table: {}", model.table);
    println!("  Columns: {}", model.columns.len());
    println!("  Copula dimension: {}", model.copula.correlation.len());
    println!("  Dialect: {}", model.dialect);
    if model.pk.is_empty() {
        println!("  Primary key: (none)");
    } else {
        println!("  Primary key: {}", model.pk.join(", "));
    }

    Ok(())
}

pub(crate) fn parse_foreign_keys(
    result: &crate::backend::QueryResult,
) -> Result<Vec<ForeignKeyInfo>, String> {
    if result.rows.is_empty() {
        return Ok(Vec::new());
    }

    let col = |wanted: &str| -> Result<usize, String> {
        result
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case(wanted))
            .ok_or_else(|| format!("FK query result missing column '{}'", wanted))
    };

    let t = col("table_name")?;
    let c = col("column_name")?;
    let rt = col("referenced_table")?;
    let rc = col("referenced_column")?;

    Ok(result
        .rows
        .iter()
        .filter_map(|row| {
            Some(ForeignKeyInfo {
                from_table: row.get(t)?.as_str()?.to_string(),
                from_column: row.get(c)?.as_str()?.to_string(),
                to_table: row.get(rt)?.as_str()?.to_string(),
                to_column: row.get(rc)?.as_str()?.to_string(),
            })
        })
        .collect())
}

// ─── Tests ─────────────────────────────────────────────────────────────

/// Reject overrides that cannot apply to their column *before* any fitting, so
/// a bad rule fails the run with a precise message instead of being recorded
/// as "column failed to fit" (which drops the column from the model and from
/// `pk` while the rules file still looks applied).
fn validate_forced_marginals(
    table: &str,
    profile: &TableProfile,
    forced: Option<&HashMap<String, String>>,
) -> Result<(), String> {
    let Some(forced) = forced else {
        return Ok(());
    };
    for (column, name) in forced {
        let profile_column = profile.columns.get(column).ok_or_else(|| {
            format!(
                "table '{}': rules force a marginal for unknown column '{}'",
                table, column
            )
        })?;
        // A datetime with an inferred format trains on its epoch axis: the raw
        // value dictionary is irrelevant there, so continuous families apply.
        let epoch_axis =
            profile_column.logical_type == "datetime" && profile_column.datetime_format.is_some();
        if name == "categorical" {
            if epoch_axis {
                return Err(format!(
                    "table '{}' column '{}': a timestamp column trains on its epoch axis; \
                     marginal 'categorical' is not supported there",
                    table, column
                ));
            }
            if profile_column.top_values.is_none() {
                return Err(format!(
                    "table '{}' column '{}': marginal 'categorical' needs a value dictionary, \
                     but the column has none",
                    table, column
                ));
            }
            continue;
        }
        if crate::synth::marginal::NumericFamily::parse(name).is_none() && name != "ecdf" {
            return Err(format!(
                "table '{}' column '{}': unknown marginal '{}' (expected one of {})",
                table,
                column,
                name,
                crate::synth::rules::ALLOWED_MARGINALS.join(", ")
            ));
        }
        if !epoch_axis && profile_column.top_values.is_some() {
            return Err(format!(
                "table '{}' column '{}': marginal '{}' conflicts with the {}-level value \
                 dictionary; keep 'categorical' for low-cardinality columns",
                table,
                column,
                name,
                profile_column
                    .top_values
                    .as_ref()
                    .map(|levels| levels.len())
                    .unwrap_or(0)
            ));
        }
    }
    Ok(())
}

/// Fit a rules-forced family on `samples`. `Ok(None)` means the family cannot
/// describe this data (moment matching produces unusable parameters), which
/// the caller reports and then resolves with auto-selection.
fn try_forced_marginal(samples: &[f64], name: &str) -> Result<Option<Marginal>, String> {
    if name == "ecdf" {
        return EcdfFitter
            .fit(samples)
            .map(Some)
            .map_err(|err| format!("forced marginal 'ecdf': {}", err));
    }
    let family = crate::synth::marginal::NumericFamily::parse(name)
        .ok_or_else(|| format!("unknown marginal '{}'", name))?;
    Ok(match family.fit(samples) {
        Ok(marginal) if family.parameters_are_usable(&marginal) => Some(marginal),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::Value;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: SynthCommand,
    }

    fn parse(argv: &[&str]) -> SynthCommand {
        TestCli::try_parse_from(argv)
            .expect("parse synth args")
            .command
    }

    #[test]
    fn draft_without_mine_flag_is_unchanged() {
        let cmd = parse(&["synth", "rules-draft", "--tables", "orders"]);
        match cmd {
            SynthCommand::RulesDraft { mine, .. } => {
                assert!(!mine.mine, "--mine must default to off");
                assert_eq!(mine.mine_confidence, 0.95);
                assert_eq!(mine.mine_support, 0.05);
                assert_eq!(mine.mine_max_pairs, 2000);
                assert_eq!(mine.emit_candidates, None);
            }
            other => panic!("expected rules-draft, got {other:?}"),
        }
    }

    #[test]
    fn parses_rules_draft_mine_flags() {
        let cmd = parse(&[
            "synth",
            "rules-draft",
            "--tables",
            "orders",
            "--mine",
            "--mine-confidence",
            "0.85",
            "--mine-support",
            "0.10",
            "--mine-max-pairs",
            "42",
            "--emit-candidates",
            "candidates.txt",
        ]);
        match cmd {
            SynthCommand::RulesDraft { mine, .. } => {
                assert!(mine.mine);
                assert_eq!(mine.mine_confidence, 0.85);
                assert_eq!(mine.mine_support, 0.10);
                assert_eq!(mine.mine_max_pairs, 42);
                assert_eq!(mine.emit_candidates.as_deref(), Some("candidates.txt"));
            }
            other => panic!("expected rules-draft, got {other:?}"),
        }
    }

    #[test]
    fn parses_train_subcommand() {
        let cmd = parse(&["synth", "train", "--tables", "users,orders"]);
        match cmd {
            SynthCommand::Train {
                tables,
                output,
                sample,
                categorical_top_k,
                ..
            } => {
                assert_eq!(tables, "users,orders");
                assert_eq!(output, ".synth");
                assert_eq!(sample, 10_000);
                assert_eq!(categorical_top_k, CategoricalTopK::Limit(50));
            }
            other => panic!("expected Train, got {:?}", other),
        }
    }

    #[test]
    fn should_keep_default_top_k_behaviour() {
        let cmd = parse(&["synth", "train", "--tables", "t"]);
        match cmd {
            SynthCommand::Train {
                categorical_top_k, ..
            } => {
                assert_eq!(categorical_top_k, CategoricalTopK::Limit(50));
                assert_eq!(categorical_top_k.cap(), Some(50));
            }
            other => panic!("expected Train, got {:?}", other),
        }

        let full = parse(&[
            "synth",
            "train",
            "--tables",
            "t",
            "--categorical-top-k",
            "full",
        ]);
        match full {
            SynthCommand::Train {
                categorical_top_k, ..
            } => {
                assert_eq!(categorical_top_k, CategoricalTopK::Full);
                assert_eq!(categorical_top_k.cap(), None);
            }
            other => panic!("expected Train, got {:?}", other),
        }

        let capped = parse(&[
            "synth",
            "train",
            "--tables",
            "t",
            "--categorical-top-k",
            "12",
        ]);
        match capped {
            SynthCommand::Train {
                categorical_top_k, ..
            } => assert_eq!(categorical_top_k, CategoricalTopK::Limit(12)),
            other => panic!("expected Train, got {:?}", other),
        }
    }

    #[test]
    fn should_cover_all_dictionary_levels_with_full_top_k() {
        const LEVELS: usize = 500;
        const GEN_ROWS: usize = 10_000;
        let samples: Vec<Value> = (0..LEVELS)
            .flat_map(|i| std::iter::repeat_n(Value::from(format!("L{i:03}")), 20))
            .collect();
        let col = crate::synth::profile::ColumnProfile::from_samples_typed(&samples, None, None);
        let top = col.top_values.as_ref().expect("top_values");
        assert_eq!(top.len(), LEVELS);

        let mut columns = HashMap::new();
        columns.insert("code".to_string(), col);
        let profile = TableProfile {
            table: "dict".to_string(),
            row_count: samples.len(),
            column_order: vec!["code".to_string()],
            columns,
        };
        let (model, skipped) = build_model("dict", "mysql", &profile, &[], vec![], None).unwrap();
        assert!(skipped.is_empty());

        let mut models = HashMap::new();
        models.insert("dict".to_string(), model);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![crate::synth::rules::TableRule {
                name: "dict".to_string(),
                columns: HashMap::new(),
                derive: vec![],
                branches: vec![],
                rows: Some(GEN_ROWS),
                relationships: vec![],
                strategy: crate::synth::rules::TableStrategy::default(),
            }],
        };
        let data = generate(
            &models,
            &rules,
            &GeneratorConfig {
                rows_per_table: HashMap::from([("dict".to_string(), GEN_ROWS)]),
                seed: Some(42),
                enforce_min_max_values: true,
            },
        )
        .unwrap();
        let rows = data.tables.get("dict").expect("dict rows");
        assert_eq!(rows.len(), GEN_ROWS);

        let mut counts: HashMap<String, usize> = HashMap::new();
        for row in rows {
            let key = row[0].as_str().expect("categorical value").to_string();
            *counts.entry(key).or_insert(0) += 1;
        }
        assert!(
            counts.len() >= 495,
            "full top-k should cover ≥495 of 500 levels, got {}",
            counts.len()
        );

        let n = GEN_ROWS as f64;
        let p = 1.0 / LEVELS as f64;
        let mut tv = 0.0;
        for i in 0..LEVELS {
            let key = format!("L{i:03}");
            let q = *counts.get(&key).unwrap_or(&0) as f64 / n;
            assert!(
                (q - p).abs() < 0.05,
                "level {key}: generated share {q} vs train {p}"
            );
            tv += (p - q).abs();
        }
        tv /= 2.0;
        // Multinomial E[TV] ≈ 0.09 for 500 levels / 10k rows, so the sharp
        // 0.05 overall bound is not stable; a 50-cap model still lands near
        // TV 0.9 and fails both coverage and this 0.15 ceiling.
        assert!(
            tv < 0.15,
            "TV distance {tv} should stay well below a 50-cap model (~0.9)"
        );
    }

    #[test]
    fn parses_generate_with_overrides() {
        let cmd = parse(&[
            "synth", "generate", "--models", "m", "--rows", "42", "--seed", "7", "--format", "sql",
        ]);
        match cmd {
            SynthCommand::Generate {
                models,
                rules,
                rows,
                seed,
                format,
                no_schema_qualifier,
                ..
            } => {
                assert_eq!(models, "m");
                assert_eq!(rules, "synth-rules.yaml");
                assert_eq!(rows, Some(42));
                assert_eq!(seed, Some(7));
                assert_eq!(format, "sql");
                assert!(!no_schema_qualifier);
            }
            other => panic!("expected Generate, got {:?}", other),
        }

        let flagged = parse(&[
            "synth",
            "generate",
            "--format",
            "sql",
            "--no-schema-qualifier",
        ]);
        match flagged {
            SynthCommand::Generate {
                no_schema_qualifier,
                ..
            } => assert!(no_schema_qualifier),
            other => panic!("expected Generate, got {:?}", other),
        }
    }

    #[test]
    fn should_prefer_cli_rows_over_rules() {
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![crate::synth::rules::TableRule {
                name: "orders".to_string(),
                columns: HashMap::new(),
                derive: vec![],
                branches: vec![],
                rows: Some(2),
                relationships: vec![],
                strategy: crate::synth::rules::TableStrategy::default(),
            }],
        };

        let rows = rows_map_for_rules(&rules, Some(42));

        assert_eq!(rows.get("orders"), Some(&42));
    }

    fn profile_from(columns: &[(&str, Vec<Value>)]) -> TableProfile {
        let names: Vec<String> = columns.iter().map(|(n, _)| n.to_string()).collect();
        let rows: Vec<Vec<Value>> = (0..4)
            .map(|i| columns.iter().map(|(_, vals)| vals[i].clone()).collect())
            .collect();
        TableProfile::from_rows("t", &names, &rows)
    }

    #[test]
    fn build_model_correlation_matches_column_count() {
        let profile = profile_from(&[
            (
                "a",
                vec![
                    Value::from(1),
                    Value::from(2),
                    Value::from(3),
                    Value::from(4),
                ],
            ),
            (
                "b",
                vec![
                    Value::from(10),
                    Value::from(20),
                    Value::from(30),
                    Value::from(40),
                ],
            ),
            (
                "c",
                vec![
                    Value::from("x"),
                    Value::from("x"),
                    Value::from("y"),
                    Value::from("y"),
                ],
            ),
        ]);

        let (model, skipped) = build_model(
            "t",
            "mysql",
            &profile,
            &[],
            vec![],
            Some("sales".to_string()),
        )
        .unwrap();
        assert!(skipped.is_empty());
        assert_eq!(model.schema.as_deref(), Some("sales"));
        let n = model.copula.column_order.len();
        assert_eq!(n, 3);
        assert_eq!(model.copula.column_order, vec!["a", "b", "c"]);
        assert_eq!(model.copula.correlation.len(), 3);
        for (i, row) in model.copula.correlation.iter().enumerate() {
            assert_eq!(row.len(), 3, "row {} must be 3 wide", i);
            for (j, &v) in row.iter().enumerate() {
                assert_eq!(v, if i == j { 1.0 } else { 0.0 });
            }
        }
    }

    #[test]
    fn build_model_rejects_empty_profile() {
        let profile = TableProfile::from_rows("t", &[], &[]);
        assert!(build_model("t", "mysql", &profile, &[], vec![], None).is_err());
    }

    #[test]
    fn build_model_fits_categorical_from_top_values() {
        let profile = profile_from(&[(
            "status",
            vec![
                Value::from("open"),
                Value::from("open"),
                Value::from("closed"),
                Value::from("closed"),
            ],
        )]);

        let (model, skipped) = build_model("t", "mysql", &profile, &[], vec![], None).unwrap();
        assert!(skipped.is_empty());
        let col = model.columns.get("status").unwrap();
        match &col.marginal {
            Marginal::Categorical(p) => {
                let total: f64 = p.weights.iter().sum();
                assert!((total - 1.0).abs() < 1e-10);
                assert_eq!(p.values.len(), 2);
            }
            other => panic!("expected Categorical, got {:?}", other),
        }
    }

    #[test]
    fn should_fit_low_cardinality_numeric_as_categorical() {
        let samples: Vec<Value> = (0..190).map(|i| Value::from((i % 19) as i64)).collect();
        let profile = crate::synth::profile::ColumnProfile::from_samples(&samples);
        let numeric: Vec<f64> = (0..190).map(|i| (i % 19) as f64).collect();

        let marginal = fit_marginal("amount", &profile, &numeric, None).unwrap();

        match marginal {
            Marginal::Categorical(params) => assert_eq!(params.values.len(), 19),
            other => panic!("expected Categorical, got {:?}", other),
        }
    }

    // Contract: a well-fitting parametric family wins, and ECDF is never
    // chosen for a clean shape. Uniform data therefore lands on Uniform.
    #[test]
    fn should_fit_high_cardinality_numeric_via_auto_selection() {
        let samples: Vec<Value> = (0..1000).map(Value::from).collect();
        let profile = crate::synth::profile::ColumnProfile::from_samples(&samples);
        let numeric: Vec<f64> = (0..1000).map(|i| i as f64).collect();

        let marginal = fit_marginal("amount", &profile, &numeric, None).unwrap();

        assert!(
            matches!(marginal, Marginal::Uniform(_)),
            "uniform data must stay parametric, got {:?}",
            marginal
        );
    }

    #[test]
    fn build_model_constant_column_reproduces_value() {
        let profile = profile_from(&[(
            "flag",
            vec![
                Value::from(5),
                Value::from(5),
                Value::from(5),
                Value::from(5),
            ],
        )]);

        let (model, skipped) = build_model("t", "mysql", &profile, &[], vec![], None).unwrap();
        assert!(skipped.is_empty());
        let col = model.columns.get("flag").unwrap();
        let generated = col.marginal.inverse_cdf(0.01);
        let generated2 = col.marginal.inverse_cdf(0.99);
        assert_eq!(generated, 5.0);
        assert_eq!(generated2, 5.0);
    }

    #[test]
    fn load_profiles_roundtrip_and_ignores_models() {
        let dir = std::env::temp_dir().join("synth_test_profiles");
        std::fs::create_dir_all(&dir).unwrap();

        let profile = profile_from(&[(
            "a",
            vec![
                Value::from(1),
                Value::from(2),
                Value::from(3),
                Value::from(4),
            ],
        )]);
        profile.save(&dir.join("users.profile.json")).unwrap();

        let (model, _) = build_model("users", "mysql", &profile, &[], vec![], None).unwrap();
        model.save(&dir.join("users.model.json")).unwrap();

        let loaded = load_profiles(&dir).unwrap();
        assert_eq!(loaded.len(), 1, "model json must not load as profile");
        assert!(loaded.contains_key("users"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn build_model_skips_unsupported_columns() {
        let placeholder = "<unsupported type timestamptz>: \\x00";
        let profile = profile_from(&[
            (
                "id",
                vec![
                    Value::from(1),
                    Value::from(2),
                    Value::from(3),
                    Value::from(4),
                ],
            ),
            (
                "last_update",
                vec![
                    Value::from(placeholder),
                    Value::from(placeholder),
                    Value::from(placeholder),
                    Value::from(placeholder),
                ],
            ),
        ]);

        let (model, skipped) = build_model("t", "mysql", &profile, &[], vec![], None).unwrap();
        assert_eq!(skipped, vec!["last_update".to_string()]);
        assert_eq!(model.copula.column_order, vec!["id"]);
        assert!(!model.columns.contains_key("last_update"));
    }

    #[test]
    fn build_model_correlation_ignores_skipped_columns() {
        // 中间列是驱动占位串会被跳过；id 与 v3 完全线性相关。
        // 回归：投影前用全宽行索引取数会导致 v3 读到占位串 → 相关性恒 0。
        let rows: Vec<Vec<Value>> = (0..10)
            .map(|i| {
                vec![
                    Value::from(i),
                    Value::from("<unsupported type timestamptz>: \\x00"),
                    Value::from(3 * i),
                ]
            })
            .collect();
        let columns = vec![
            "id".to_string(),
            "last_update".to_string(),
            "v3".to_string(),
        ];
        let profile = TableProfile::from_rows("t", &columns, &rows);

        let (model, skipped) = build_model("t", "mysql", &profile, &rows, vec![], None).unwrap();
        assert_eq!(skipped, vec!["last_update".to_string()]);
        assert_eq!(model.copula.column_order, vec!["id", "v3"]);
        let r = model.copula.correlation[0][1];
        assert!(
            r > 0.9,
            "id–v3 correlation must survive the skipped column, got {}",
            r
        );
    }

    #[test]
    fn build_model_marks_integer_columns_for_rounding() {
        let profile = profile_from(&[(
            "id",
            vec![
                Value::from(1),
                Value::from(2),
                Value::from(3),
                Value::from(4),
            ],
        )]);

        let (model, _) = build_model("t", "mysql", &profile, &[], vec![], None).unwrap();
        let col = model.columns.get("id").unwrap();
        assert_eq!(col.rounding, Some(0));
    }

    #[test]
    fn build_model_allows_negative_integer_column() {
        let profile = profile_from(&[(
            "v",
            vec![
                Value::from(-3),
                Value::from(-2),
                Value::from(5),
                Value::from(9),
            ],
        )]);

        let (model, _) = build_model("t", "mysql", &profile, &[], vec![], None).unwrap();
        let col = model.columns.get("v").unwrap();
        assert_eq!(col.rounding, Some(0));
    }

    #[test]
    fn build_model_records_composite_pk() {
        let profile = profile_from(&[
            (
                "a",
                vec![
                    Value::from(1),
                    Value::from(2),
                    Value::from(3),
                    Value::from(4),
                ],
            ),
            (
                "b",
                vec![
                    Value::from(5),
                    Value::from(6),
                    Value::from(7),
                    Value::from(8),
                ],
            ),
        ]);
        let (model, _) = build_model(
            "t",
            "mysql",
            &profile,
            &[],
            vec!["a".to_string(), "b".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(model.pk, vec!["a", "b"]);
    }

    #[test]
    fn build_model_drops_pk_column_missing_from_the_model() {
        let profile = profile_from(&[(
            "a",
            vec![
                Value::from(1),
                Value::from(2),
                Value::from(3),
                Value::from(4),
            ],
        )]);
        let (model, _) = build_model(
            "t",
            "mysql",
            &profile,
            &[],
            vec!["a".to_string(), "ghost".to_string()],
            None,
        )
        .unwrap();
        assert_eq!(model.pk, vec!["a"]);
    }

    #[test]
    fn reconcile_primary_key_normalizes_case_and_drops_unknown() {
        let columns = vec![
            "Xwdm".to_string(),
            "Security_Id".to_string(),
            "amt".to_string(),
        ];
        let pk = vec![
            "XWDM".to_string(),
            "security_id".to_string(),
            "missing".to_string(),
        ];
        assert_eq!(
            reconcile_primary_key(pk, &columns),
            vec!["Xwdm".to_string(), "Security_Id".to_string()]
        );
    }

    #[test]
    fn reconcile_primary_key_keeps_key_order_and_empty_input() {
        let columns = vec!["b".to_string(), "a".to_string()];
        assert_eq!(
            reconcile_primary_key(vec!["a".to_string(), "b".to_string()], &columns),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(reconcile_primary_key(vec![], &columns).is_empty());
    }

    #[test]
    fn build_model_maps_compact_date_to_datetime() {
        let profile = profile_from(&[(
            "biz_date",
            vec![
                Value::from("20240101"),
                Value::from("20240315"),
                Value::from("20240630"),
                Value::from("20241231"),
            ],
        )]);
        let (model, skipped) = build_model("t", "mysql", &profile, &[], vec![], None).unwrap();
        assert!(skipped.is_empty());
        let col = model.columns.get("biz_date").unwrap();
        assert!(matches!(col.logical_type, LogicalType::Datetime));
        assert_eq!(col.rounding, Some(0));
        match &col.marginal {
            Marginal::Normal(p) => assert!(p.loc > 20_000_000.0),
            other => panic!("expected Normal for compact dates, got {:?}", other),
        }
    }

    #[test]
    fn should_copy_decimal_scale_from_profile() {
        let samples: Vec<Value> = ["1.2345", "2.3456", "3.4567", "4.5678"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let col = crate::synth::profile::ColumnProfile::from_samples(&samples);
        assert_eq!(col.decimal_scale, Some(4));
        let mut columns = std::collections::HashMap::new();
        columns.insert("amt".to_string(), col);
        let profile = TableProfile {
            table: "t".to_string(),
            row_count: 4,
            column_order: vec!["amt".to_string()],
            columns,
        };
        let (model, skipped) = build_model("t", "mysql", &profile, &[], vec![], None).unwrap();
        assert!(skipped.is_empty());
        assert_eq!(model.columns.get("amt").unwrap().decimal_scale, Some(4));
    }

    #[test]
    fn build_model_fits_all_null_numeric_from_schema() {
        let samples = vec![Value::Null, Value::Null, Value::Null, Value::Null];
        let col = crate::synth::profile::ColumnProfile::from_samples_typed(
            &samples,
            Some("numeric(16,2)"),
            Some(crate::synth::profile::TOP_VALUES_CAP),
        );
        let mut columns = std::collections::HashMap::new();
        columns.insert("amt".to_string(), col);
        let profile = TableProfile {
            table: "t".to_string(),
            row_count: 4,
            column_order: vec!["amt".to_string()],
            columns,
        };
        let (model, skipped) = build_model("t", "mysql", &profile, &[], vec![], None).unwrap();
        assert!(skipped.is_empty());
        let col = model.columns.get("amt").unwrap();
        assert!(matches!(col.logical_type, LogicalType::Numerical));
        assert_eq!(col.null_rate, Some(1.0));
    }

    #[test]
    fn build_model_keeps_all_null_datetime() {
        let samples = vec![Value::Null, Value::Null, Value::Null, Value::Null];
        let col = crate::synth::profile::ColumnProfile::from_samples_typed(
            &samples,
            Some("timestamp without time zone"),
            Some(crate::synth::profile::TOP_VALUES_CAP),
        );
        let mut columns = std::collections::HashMap::new();
        columns.insert("ts".to_string(), col);
        let profile = TableProfile {
            table: "t".to_string(),
            row_count: 4,
            column_order: vec!["ts".to_string()],
            columns,
        };
        let (model, skipped) = build_model("t", "mysql", &profile, &[], vec![], None).unwrap();
        assert!(skipped.is_empty());
        let col = model.columns.get("ts").unwrap();
        assert!(matches!(col.logical_type, LogicalType::Datetime));
        assert_eq!(col.null_rate, Some(1.0));
    }

    #[test]
    fn parse_primary_key_reads_composite_index_row() {
        let result = crate::backend::QueryResult {
            columns: vec![
                "index_name".to_string(),
                "is_unique".to_string(),
                "is_primary".to_string(),
                "columns".to_string(),
                "index_type".to_string(),
            ],
            rows: vec![vec![
                Value::from("PRIMARY"),
                Value::from(true),
                Value::from(true),
                Value::from("xwdm, security_id"),
                Value::from("BTREE"),
            ]],
            row_count: 1,
            rows_affected: None,
        };
        assert_eq!(
            parse_primary_key(&result),
            vec!["xwdm".to_string(), "security_id".to_string()]
        );
    }

    #[test]
    fn parse_column_types_maps_name_to_sql_type() {
        let result = crate::backend::QueryResult {
            columns: vec![
                "column_name".to_string(),
                "data_type".to_string(),
                "nullable".to_string(),
            ],
            rows: vec![
                vec![
                    Value::from("biz_date"),
                    Value::from("character varying(8)"),
                    Value::from(false),
                ],
                vec![
                    Value::from("amt"),
                    Value::from("numeric(16,2)"),
                    Value::from(true),
                ],
            ],
            row_count: 2,
            rows_affected: None,
        };
        let types = parse_column_types(&result);
        assert_eq!(
            types.get("biz_date").map(String::as_str),
            Some("character varying(8)")
        );
        assert_eq!(types.get("amt").map(String::as_str), Some("numeric(16,2)"));
    }

    #[test]
    fn parse_foreign_keys_maps_columns_case_insensitively() {
        let result = crate::backend::QueryResult {
            columns: vec![
                "TABLE_NAME".to_string(),
                "COLUMN_NAME".to_string(),
                "REFERENCED_TABLE".to_string(),
                "REFERENCED_COLUMN".to_string(),
            ],
            rows: vec![vec![
                Value::from("orders"),
                Value::from("user_id"),
                Value::from("users"),
                Value::from("id"),
            ]],
            row_count: 1,
            rows_affected: None,
        };

        let fks = parse_foreign_keys(&result).unwrap();
        assert_eq!(fks.len(), 1);
        assert_eq!(fks[0].from_table, "orders");
        assert_eq!(fks[0].to_table, "users");
    }

    #[test]
    fn parse_foreign_keys_errors_on_missing_column() {
        let result = crate::backend::QueryResult {
            columns: vec!["table_name".to_string()],
            rows: vec![vec![Value::from("orders")]],
            row_count: 1,
            rows_affected: None,
        };
        assert!(parse_foreign_keys(&result).is_err());
    }

    // GaussDB 等驱动对 0 行结果返回 QueryResult::empty()（无列名），
    // 无外键是正常状态，不得报错
    #[test]
    fn parse_foreign_keys_empty_result_yields_no_fks() {
        let result = crate::backend::QueryResult::empty();
        assert_eq!(parse_foreign_keys(&result).unwrap(), vec![]);
    }

    #[test]
    fn parse_foreign_keys_zero_rows_with_columns_yields_no_fks() {
        let result = crate::backend::QueryResult {
            columns: vec![
                "table_name".to_string(),
                "column_name".to_string(),
                "referenced_table".to_string(),
                "referenced_column".to_string(),
            ],
            rows: vec![],
            row_count: 0,
            rows_affected: None,
        };
        assert_eq!(parse_foreign_keys(&result).unwrap(), vec![]);
    }

    fn typed_profile(
        table: &str,
        columns: &[(&str, &str, Vec<Value>)],
    ) -> (TableProfile, Vec<Vec<Value>>) {
        let names: Vec<String> = columns.iter().map(|(n, _, _)| n.to_string()).collect();
        let n_rows = columns[0].2.len();
        let rows: Vec<Vec<Value>> = (0..n_rows)
            .map(|i| columns.iter().map(|(_, _, vals)| vals[i].clone()).collect())
            .collect();
        let mut types = HashMap::new();
        for (name, ty, _) in columns {
            types.insert(name.to_string(), ty.to_string());
        }
        (
            TableProfile::from_rows_typed(
                table,
                &names,
                &rows,
                Some(&types),
                Some(crate::synth::profile::TOP_VALUES_CAP),
            ),
            rows,
        )
    }

    fn generate_table(model: TableModel, rows: usize) -> Vec<Vec<Value>> {
        let table = model.table.clone();
        let mut models = HashMap::new();
        models.insert(table.clone(), model);
        let rules = SynthRules {
            version: "1".to_string(),
            tables: vec![crate::synth::rules::TableRule {
                name: table.clone(),
                columns: HashMap::new(),
                derive: vec![],
                branches: vec![],
                rows: Some(rows),
                relationships: vec![],
                strategy: Default::default(),
            }],
        };
        let config = crate::synth::generator::GeneratorConfig {
            rows_per_table: HashMap::from([(table.clone(), rows)]),
            seed: Some(42),
            enforce_min_max_values: true,
        };
        crate::synth::generator::generate(&models, &rules, &config)
            .unwrap()
            .tables
            .remove(&table)
            .unwrap()
    }

    fn pearson(xs: &[f64], ys: &[f64]) -> f64 {
        let n = xs.len() as f64;
        let mx = xs.iter().sum::<f64>() / n;
        let my = ys.iter().sum::<f64>() / n;
        let mut cov = 0.0;
        let mut vx = 0.0;
        let mut vy = 0.0;
        for (x, y) in xs.iter().zip(ys.iter()) {
            let dx = x - mx;
            let dy = y - my;
            cov += dx * dy;
            vx += dx * dx;
            vy += dy * dy;
        }
        cov / (vx * vy).sqrt()
    }

    #[test]
    fn should_restore_primary_datetime_format_round_trip() {
        let values: Vec<Value> = (0..200)
            .map(|i| Value::from(format!("2024-03-{:02} {:02}:30:00", 1 + (i % 28), i % 24)))
            .collect();
        let (profile, rows) = typed_profile(
            "t1",
            &[("created_at", "timestamp without time zone", values)],
        );
        let (model, skipped) = build_model("t1", "gaussdb", &profile, &rows, vec![], None).unwrap();
        assert!(skipped.is_empty());
        let col = model.columns.get("created_at").unwrap();
        assert!(matches!(col.logical_type, LogicalType::Datetime));
        assert_eq!(col.datetime_epoch, Some(true));
        let fmt = col
            .datetime_format
            .clone()
            .expect("inferred datetime format");
        assert_eq!(fmt, "%Y-%m-%d %H:%M:%S");
        assert!(matches!(col.marginal, Marginal::Normal(_)));

        let generated = generate_table(model, 1000);
        assert_eq!(generated.len(), 1000);
        for row in &generated {
            let s = row[0].as_str().expect("datetime must be a string");
            let epoch = crate::synth::datetime::parse_to_epoch(&Value::from(s), Some(&fmt))
                .unwrap_or_else(|| panic!("generated {s:?} must parse as {fmt}"));
            let rendered = crate::synth::datetime::format_epoch(epoch, &fmt).unwrap();
            assert_eq!(rendered, s, "generated value must be character-identical");
        }
    }

    #[test]
    fn should_clip_generated_datetime_to_training_range() {
        let mut values = vec![Value::from("2020-01-01"), Value::from("2026-01-01")];
        for i in 0..80 {
            let year = 2020 + (i % 6);
            values.push(Value::from(format!("{year}-06-15")));
        }
        let (profile, rows) = typed_profile("t1", &[("created_at", "date", values)]);
        let (model, skipped) = build_model("t1", "mysql", &profile, &rows, vec![], None).unwrap();
        assert!(skipped.is_empty());
        let col = model.columns.get("created_at").unwrap();
        let fmt = col.datetime_format.clone().expect("date format");
        let min_epoch = col.min.expect("epoch min");
        let max_epoch = col.max.expect("epoch max");
        let generated = generate_table(model, 400);
        for row in &generated {
            let s = row[0].as_str().expect("datetime string");
            let epoch = crate::synth::datetime::parse_to_epoch(&Value::from(s), Some(&fmt))
                .unwrap_or_else(|| panic!("unparseable generated datetime {s:?}"));
            assert!(
                epoch + 1e-6 >= min_epoch && epoch <= max_epoch + 1e-6,
                "{s} epoch {epoch} outside [{min_epoch}, {max_epoch}]"
            );
        }
    }

    #[test]
    fn should_correlate_datetime_epoch_with_numeric_column() {
        let start =
            crate::synth::datetime::parse_to_epoch(&Value::from("2020-01-01"), Some("%Y-%m-%d"))
                .unwrap();
        let n = 200usize;
        let mut created = Vec::with_capacity(n);
        let mut amounts = Vec::with_capacity(n);
        let mut epochs = Vec::with_capacity(n);
        let mut amount_nums = Vec::with_capacity(n);
        for i in 0..n {
            let epoch = start + (i as f64) * 86_400.0;
            let s = crate::synth::datetime::format_epoch(epoch, "%Y-%m-%d").unwrap();
            created.push(Value::from(s));
            let amt = i as f64 * 10.0;
            amounts.push(Value::from(amt));
            epochs.push(epoch);
            amount_nums.push(amt);
        }
        let (profile, rows) = typed_profile(
            "t",
            &[
                ("created_at", "timestamp without time zone", created),
                ("amount", "numeric", amounts),
            ],
        );
        let (model, skipped) = build_model("t", "mysql", &profile, &rows, vec![], None).unwrap();
        assert!(skipped.is_empty());
        let order = &model.copula.column_order;
        let i_dt = order.iter().position(|c| c == "created_at").unwrap();
        let i_amt = order.iter().position(|c| c == "amount").unwrap();
        let r_model = model.copula.correlation[i_dt][i_amt];
        let r_train = pearson(&epochs, &amount_nums);
        assert!(
            r_model.signum() == r_train.signum() || r_model * r_train > 0.0,
            "correlation must keep the training sign: model={r_model} train={r_train}"
        );
        assert!(
            (r_model - r_train).abs() < 0.3,
            "model correlation {r_model} too far from training Pearson {r_train}"
        );
    }

    #[test]
    fn should_warn_and_fall_back_when_datetime_format_is_unknown() {
        let values: Vec<Value> = ["15-JAN-24", "16-JAN-24", "17-JAN-24", "18-JAN-24"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let (profile, rows) = typed_profile("t", &[("biz_date", "date", values)]);
        assert!(profile.columns["biz_date"].datetime_format.is_none());
        let (model, skipped) = build_model("t", "oracle", &profile, &rows, vec![], None).unwrap();
        assert!(skipped.is_empty());
        let col = model.columns.get("biz_date").unwrap();
        assert!(matches!(col.logical_type, LogicalType::Datetime));
        assert!(col.datetime_format.is_none());
        assert_ne!(col.datetime_epoch, Some(true));
        match &col.marginal {
            Marginal::Categorical(p) => {
                assert!(p.values.iter().any(|v| v == "15-JAN-24"));
            }
            other => panic!("expected legacy Categorical, got {other:?}"),
        }
    }

    // ─── marginal auto-selection wiring (#66) ────────────────────────────

    struct Lcg(u64);

    impl Lcg {
        fn next_u01(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
        }

        fn uniform(&mut self, low: f64, high: f64) -> f64 {
            low + (high - low) * self.next_u01()
        }
    }

    /// Unit exponential sample: strongly right-skewed, and cheap to draw.
    fn exponential_values(seed: u64, n: usize) -> Vec<Value> {
        let mut rng = Lcg(seed);
        (0..n)
            .map(|_| Value::from(-rng.next_u01().max(1e-12).ln()))
            .collect()
    }

    #[test]
    fn should_keep_low_cardinality_numeric_as_categorical() {
        // AC4 regression: 19 repeating numeric levels stay on the top_values
        // (Categorical) path and are never swallowed by the ECDF marginal.
        let values: Vec<Value> = (0..190).map(|i| Value::from((i % 19) as i64)).collect();
        let (profile, rows) = typed_profile("t", &[("code", "int", values)]);
        assert!(profile.columns["code"].top_values.is_some());

        let (model, skipped) = build_model("t", "mysql", &profile, &rows, vec![], None).unwrap();
        assert!(skipped.is_empty());
        match &model.columns["code"].marginal {
            Marginal::Categorical(p) => assert_eq!(p.values.len(), 19),
            other => panic!("expected Categorical, got {other:?}"),
        }
    }

    #[test]
    fn should_apply_auto_selection_to_high_cardinality_numeric() {
        let values = exponential_values(12345, 5_000);
        let (profile, rows) = typed_profile("t", &[("amount", "double", values)]);
        assert!(profile.columns["amount"].top_values.is_none());

        let (model, skipped) = build_model("t", "mysql", &profile, &rows, vec![], None).unwrap();
        assert!(skipped.is_empty());
        match &model.columns["amount"].marginal {
            Marginal::Gamma(_) | Marginal::Ecdf(_) => {}
            other => panic!("skewed column must not stay Normal, got {other:?}"),
        }
    }

    #[test]
    fn should_force_marginal_from_rules_override() {
        // Zero-inflated: without a rule the auto-selector picks Ecdf, which
        // still holds the 70% zero mass it cannot express parametrically.
        let mut rng = Lcg(777);
        let values: Vec<Value> = (0..5_000)
            .map(|i| {
                if i % 10 < 7 {
                    Value::from(0.0)
                } else {
                    Value::from(rng.uniform(10.0, 100.0))
                }
            })
            .collect();
        let (profile, rows) = typed_profile("t", &[("fee", "double", values)]);

        let (auto, _) = build_model("t", "mysql", &profile, &rows, vec![], None).unwrap();
        assert!(
            matches!(auto.columns["fee"].marginal, Marginal::Ecdf(_)),
            "auto selection should pick Ecdf, got {:?}",
            auto.columns["fee"].marginal
        );

        // AC3: the rule wins even though KS is not the smallest.
        let forced = HashMap::from([("fee".to_string(), "gamma".to_string())]);
        let (model, skipped) =
            build_model_with_overrides("t", "mysql", &profile, &rows, vec![], None, Some(&forced))
                .unwrap();
        assert!(skipped.is_empty());
        assert!(
            matches!(model.columns["fee"].marginal, Marginal::Gamma(_)),
            "forced Gamma must win, got {:?}",
            model.columns["fee"].marginal
        );

        let forced_ecdf = HashMap::from([("fee".to_string(), "ecdf".to_string())]);
        let (model, _) = build_model_with_overrides(
            "t",
            "mysql",
            &profile,
            &rows,
            vec![],
            None,
            Some(&forced_ecdf),
        )
        .unwrap();
        assert!(matches!(model.columns["fee"].marginal, Marginal::Ecdf(_)));
    }

    #[test]
    fn should_fall_back_to_auto_selection_when_forced_family_cannot_fit() {
        // Gamma needs strictly positive samples. A forced-but-inapplicable
        // family must not produce a broken marginal, and must not drop the
        // column either.
        let values: Vec<Value> = (0..1_000)
            .map(|i| Value::from((i as f64) - 500.0))
            .collect();
        let (profile, rows) = typed_profile("t", &[("v", "double", values)]);
        let forced = HashMap::from([("v".to_string(), "gamma".to_string())]);

        let (model, skipped) =
            build_model_with_overrides("t", "mysql", &profile, &rows, vec![], None, Some(&forced))
                .unwrap();
        assert!(skipped.is_empty());
        assert!(
            !matches!(model.columns["v"].marginal, Marginal::Gamma(_)),
            "inapplicable Gamma must fall back, got {:?}",
            model.columns["v"].marginal
        );
        match &model.columns["v"].marginal {
            Marginal::Uniform(p) => {
                assert!((p.low + 500.0).abs() < 1e-9);
                assert!((p.high - 499.0).abs() < 1e-9);
            }
            other => panic!("expected Uniform fallback, got {other:?}"),
        }
    }

    #[test]
    fn should_fail_train_on_unknown_forced_marginal() {
        let values = exponential_values(9, 500);
        let (profile, rows) = typed_profile("t", &[("v", "double", values)]);
        let forced = HashMap::from([("v".to_string(), "kde".to_string())]);

        let err =
            build_model_with_overrides("t", "mysql", &profile, &rows, vec![], None, Some(&forced))
                .expect_err("unknown marginal name must fail the table");
        assert!(err.contains("unknown marginal 'kde'"), "{err}");
        assert!(err.contains("column 'v'"), "{err}");
    }

    #[test]
    fn should_fail_train_when_override_conflicts_with_the_value_dictionary() {
        // A low-cardinality numeric column keeps its dictionary; silently
        // dropping it from the model (and from `pk`) while the rules file looks
        // applied is the failure this guards against.
        let values: Vec<Value> = (0..400).map(|i| Value::from((i % 5) as i64)).collect();
        let (profile, rows) = typed_profile("t", &[("status", "int", values)]);
        assert!(profile.columns["status"].top_values.is_some());
        let forced = HashMap::from([("status".to_string(), "gamma".to_string())]);

        let err = build_model_with_overrides(
            "t",
            "mysql",
            &profile,
            &rows,
            vec!["status".to_string()],
            None,
            Some(&forced),
        )
        .expect_err("conflicting override must fail the table");
        assert!(
            err.contains("conflicts with the 5-level value dictionary"),
            "{err}"
        );

        // The forcing name itself stays accepted (categorical is the identity).
        let forced = HashMap::from([("status".to_string(), "categorical".to_string())]);
        let (model, skipped) = build_model_with_overrides(
            "t",
            "mysql",
            &profile,
            &rows,
            vec!["status".to_string()],
            None,
            Some(&forced),
        )
        .unwrap();
        assert!(skipped.is_empty());
        assert_eq!(model.pk, vec!["status".to_string()]);
        assert!(matches!(
            model.columns["status"].marginal,
            Marginal::Categorical(_)
        ));
    }

    #[test]
    fn should_fail_train_when_categorical_is_forced_on_a_timestamp() {
        let values: Vec<Value> = (0..50)
            .map(|i| Value::from(format!("2024-01-{:02} 00:00:00", (i % 28) + 1)))
            .collect();
        let (profile, rows) = typed_profile("t", &[("biz_date", "datetime", values)]);
        assert!(profile.columns["biz_date"].datetime_format.is_some());
        let forced = HashMap::from([("biz_date".to_string(), "categorical".to_string())]);

        let err =
            build_model_with_overrides("t", "mysql", &profile, &rows, vec![], None, Some(&forced))
                .expect_err("categorical on an epoch column must fail");
        assert!(err.contains("epoch axis"), "{err}");
    }

    #[test]
    fn should_apply_marginal_override_on_the_datetime_epoch_axis() {
        // A rules override must reach the epoch axis, not be dropped by the
        // datetime path.
        let values: Vec<Value> = (0..400)
            .map(|i| Value::from(format!("2024-01-01 {:02}:{:02}:00", (i / 60) % 24, i % 60)))
            .collect();
        let (profile, rows) = typed_profile("t", &[("biz_date", "datetime", values)]);
        let fmt = profile.columns["biz_date"]
            .datetime_format
            .clone()
            .expect("inferred format");

        let (auto, _) = build_model("t", "mysql", &profile, &rows, vec![], None).unwrap();
        assert!(
            matches!(auto.columns["biz_date"].marginal, Marginal::Normal(_)),
            "formatted datetimes keep Normal by default, got {:?}",
            auto.columns["biz_date"].marginal
        );

        let forced = HashMap::from([("biz_date".to_string(), "ecdf".to_string())]);
        let (model, _) =
            build_model_with_overrides("t", "mysql", &profile, &rows, vec![], None, Some(&forced))
                .unwrap();
        let column = &model.columns["biz_date"];
        assert!(
            matches!(column.marginal, Marginal::Ecdf(_)),
            "override must reach the epoch axis, got {:?}",
            column.marginal
        );
        assert_eq!(column.datetime_format.as_deref(), Some(fmt.as_str()));
        // Generated text is still formatted with the inferred format.
        let epoch = column.marginal.inverse_cdf(0.5);
        assert!(crate::synth::datetime::format_epoch(epoch, &fmt).is_some());
    }

    #[test]
    fn should_reject_override_for_an_unknown_column() {
        let values = exponential_values(4, 200);
        let (profile, rows) = typed_profile("t", &[("v", "double", values)]);
        let forced = HashMap::from([("typo".to_string(), "gamma".to_string())]);

        let err =
            build_model_with_overrides("t", "mysql", &profile, &rows, vec![], None, Some(&forced))
                .expect_err("unknown column must fail");
        assert!(err.contains("unknown column 'typo'"), "{err}");
    }

    #[test]
    fn ecdf_column_marginal_stays_within_model_budget() {
        // AC5 volume: the ECDF knot cap must keep one column's marginal under
        // the documented 16KB budget even on a 10k-row sample.
        let values = exponential_values(3, 10_000);
        let (profile, rows) = typed_profile("t", &[("v", "double", values)]);
        let forced = HashMap::from([("v".to_string(), "ecdf".to_string())]);
        let (model, _) =
            build_model_with_overrides("t", "mysql", &profile, &rows, vec![], None, Some(&forced))
                .unwrap();

        let Marginal::Ecdf(p) = &model.columns["v"].marginal else {
            panic!("expected Ecdf");
        };
        assert!(p.knots.len() <= crate::synth::marginal::ECDF_MAX_KNOTS);
        let pretty = serde_json::to_string_pretty(&model.columns["v"].marginal).unwrap();
        assert!(
            pretty.len() <= COLUMN_MARGINAL_BUDGET_BYTES,
            "ECDF marginal serialized to {} bytes, budget {}",
            pretty.len(),
            COLUMN_MARGINAL_BUDGET_BYTES
        );
        assert!(oversized_marginal_columns(&model).is_empty());
    }
}
