use crate::synth::export::{export, ExportFormat};
use crate::synth::generator::{generate, GeneratorConfig};
use crate::synth::marginal::{CategoricalParams, Marginal, NormalParams, compute_gaussian_correlation};
use crate::synth::model::{ColumnModel, CopulaInfo, LogicalType, Provenance, TableModel};
use crate::synth::profile::TableProfile;
use crate::synth::rules::SynthRules;
use crate::synth::rules_draft::ForeignKeyInfo;
use clap::{Args, Subcommand};
use std::collections::HashMap;
use std::path::Path;

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
        #[arg(long, default_value_t = true)]
        enforce_min_max_values: bool,
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
                        min: col_profile.min.as_ref().and_then(|v| v.as_f64()),
                        max: col_profile.max.as_ref().and_then(|v| v.as_f64()),
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
        compute_gaussian_correlation(rows, &column_order, &columns)
    } else {
        (0..n).map(|i| (0..n).map(|j| if i == j { 1.0 } else { 0.0 }).collect()).collect()
    };
 
    let model = TableModel {
        version: 1,
        table: table.to_string(),
        dialect: dialect.to_string(),
        provenance: Provenance {
            source: "native".to_string(),
            converter_version: None,
            sdv_version: None,
        },
        pk: vec![],
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
        "numerical" => Ok(Marginal::Normal(NormalParams {
            loc: col.mean.unwrap_or(0.0),
            scale: col.std_dev.unwrap_or(0.0),
        })),
        "categorical" => {
            let top = col.top_values.as_deref().ok_or_else(|| {
                format!(
                    "column '{}' is categorical but profile has no value frequencies; retrain",
                    col_name
                )
            })?;
            Ok(Marginal::Categorical(CategoricalParams {
                values: top.iter().map(|(v, _)| v.clone()).collect(),
                weights: top.iter().map(|(_, w)| *w).collect(),
            }))
        }
        other => Err(format!(
            "column '{}' has logical type '{}'; training supports numerical and categorical only",
            col_name, other
        )),
    }
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

    let mut rows_map = HashMap::new();
    if let Some(rows) = rows_per_table {
        for table in &rules.tables {
            rows_map.insert(table.name.clone(), rows);
        }
    }

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

    let payload = crate::synth::export::ExportPayload {
        tables: &data.tables,
        columns: &data.columns,
        dialect: &data.dialect,
    };
    export(&payload, &export_format, output_dir)?;

    println!(
        "Generation complete. Data saved to {}",
        output_dir.display()
    );
    Ok(())
}

pub fn run_validate(model_path: &str) -> Result<(), String> {
    let model = TableModel::load(Path::new(model_path))?;

    println!("Model validation passed:");
    println!("  Table: {}", model.table);
    println!("  Columns: {}", model.columns.len());
    println!("  Copula dimension: {}", model.copula.correlation.len());
    println!("  Dialect: {}", model.dialect);

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
                ..
            } => {
                assert_eq!(tables, "users,orders");
                assert_eq!(output, ".synth");
                assert_eq!(sample, 10_000);
            }
            other => panic!("expected Train, got {:?}", other),
        }
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
                ..
            } => {
                assert_eq!(models, "m");
                assert_eq!(rules, "synth-rules.yaml");
                assert_eq!(rows, Some(42));
                assert_eq!(seed, Some(7));
                assert_eq!(format, "sql");
            }
            other => panic!("expected Generate, got {:?}", other),
        }
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

        let (model, skipped) = build_model("t", "mysql", &profile, &[]).unwrap();
        assert!(skipped.is_empty());
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
        assert!(build_model("t", "mysql", &profile, &[]).is_err());
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

        let (model, skipped) = build_model("t", "mysql", &profile, &[]).unwrap();
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

        let (model, skipped) = build_model("t", "mysql", &profile, &[]).unwrap();
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

        let (model, _) = build_model("users", "mysql", &profile, &[]).unwrap();
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

        let (model, skipped) = build_model("t", "mysql", &profile, &[]).unwrap();
        assert_eq!(skipped, vec!["last_update".to_string()]);
        assert_eq!(model.copula.column_order, vec!["id"]);
        assert!(!model.columns.contains_key("last_update"));
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

        let (model, _) = build_model("t", "mysql", &profile, &[]).unwrap();
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

        let (model, _) = build_model("t", "mysql", &profile, &[]).unwrap();
        let col = model.columns.get("v").unwrap();
        assert_eq!(col.rounding, Some(0));
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
        };
        assert_eq!(parse_foreign_keys(&result).unwrap(), vec![]);
    }
}
