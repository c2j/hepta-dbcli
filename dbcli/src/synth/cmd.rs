use crate::synth::export::{export, ExportFormat};
use crate::synth::generator::{generate, GeneratorConfig};
use crate::synth::marginal::{
    compute_gaussian_correlation, CategoricalParams, Marginal, NormalParams,
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
    if profile.columns.is_empty() {
        return Err(format!("table '{}' has no columns to model", table));
    }

    let mut columns = HashMap::new();
    let mut skipped = Vec::new();
    for (col_name, col_profile) in &profile.columns {
        match fit_marginal(col_name, col_profile) {
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
                        decimal_scale: None,
                        datetime_format: None,
                        min: col_profile.min.as_ref().and_then(|v| v.as_f64()),
                        max: col_profile.max.as_ref().and_then(|v| v.as_f64()),
                        null_rate: Some(col_profile.null_rate),
                        marginal,
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
        },
        pk,
        columns,
        copula: CopulaInfo {
            column_order,
            correlation,
        },
    };
    Ok((model, skipped))
}

fn fit_marginal(
    col_name: &str,
    col: &crate::synth::profile::ColumnProfile,
) -> Result<Marginal, String> {
    match col.logical_type.as_str() {
        "numerical" => {
            if let Some(top) = col.top_values.as_deref() {
                Ok(Marginal::Categorical(CategoricalParams {
                    values: top.iter().map(|(v, _)| v.clone()).collect(),
                    weights: top.iter().map(|(_, w)| *w).collect(),
                }))
            } else {
                Ok(Marginal::Normal(NormalParams {
                    loc: col.mean.unwrap_or(0.0),
                    scale: col.std_dev.unwrap_or(0.0),
                }))
            }
        }
        // Numeric-encoded datetimes (compact YYYYMMDD, epoch integers) fit a
        // Normal; date/timestamp strings fall through to the frequency path.
        "datetime" if col.mean.is_some() => Ok(Marginal::Normal(NormalParams {
            loc: col.mean.unwrap_or(0.0),
            scale: col.std_dev.unwrap_or(0.0),
        })),
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
            Ok(Marginal::Categorical(CategoricalParams {
                values: top.iter().map(|(v, _)| v.clone()).collect(),
                weights: top.iter().map(|(_, w)| *w).collect(),
            }))
        }
        other => Err(format!(
            "column '{}' has logical type '{}'; training supports numerical, datetime, and categorical",
            col_name, other
        )),
    }
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

pub fn run_generate(
    models_dir: &str,
    rules_path: &str,
    output_dir: &str,
    rows_per_table: Option<usize>,
    seed: Option<u64>,
    format: &str,
    enforce_min_max_values: bool,
    no_schema_qualifier: bool,
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
        enforce_min_max_values,
    };

    let data = generate(&models, &rules, &config)?;

    let export_format = match format {
        "csv" => ExportFormat::Csv,
        "jsonl" => ExportFormat::Jsonl,
        "json" => ExportFormat::Json,
        "sql" => ExportFormat::Sql,
        other => return Err(format!("unsupported format: {}", other)),
    };

    let schemas = if no_schema_qualifier {
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

        let marginal = fit_marginal("amount", &profile).unwrap();

        match marginal {
            Marginal::Categorical(params) => assert_eq!(params.values.len(), 19),
            other => panic!("expected Categorical, got {:?}", other),
        }
    }

    #[test]
    fn should_still_fit_high_cardinality_numeric_as_normal() {
        let samples: Vec<Value> = (0..1000).map(Value::from).collect();
        let profile = crate::synth::profile::ColumnProfile::from_samples(&samples);

        let marginal = fit_marginal("amount", &profile).unwrap();

        assert!(matches!(marginal, Marginal::Normal(_)));
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
}
