use crate::synth::export::{export, ExportFormat};
use crate::synth::generator::{generate, GeneratorConfig};
use crate::synth::model::TableModel;
use crate::synth::rules::SynthRules;
use crate::synth::rules_draft::generate_rules_draft;
use std::collections::HashMap;

pub fn run_train(
    model_dir: &std::path::Path,
    profile: &crate::synth::profile::TableProfile,
    table_name: &str,
) -> Result<(), String> {
    std::fs::create_dir_all(model_dir).map_err(|e| format!("create output dir: {}", e))?;

    let mut columns_map = HashMap::new();

    for (col_name, col_profile) in &profile.columns {
        let marginal = match col_profile.logical_type.as_str() {
            "numerical" => {
                let mean = col_profile.mean.unwrap_or(0.0);
                let std_dev = col_profile.std_dev.unwrap_or(1.0);
                crate::synth::marginal::Marginal::Normal(crate::synth::marginal::NormalParams {
                    loc: mean,
                    scale: std_dev,
                })
            }
            "categorical" => crate::synth::marginal::Marginal::Categorical(
                crate::synth::marginal::CategoricalParams {
                    values: vec!["unknown".to_string()],
                    weights: vec![1.0],
                },
            ),
            _ => crate::synth::marginal::Marginal::Normal(crate::synth::marginal::NormalParams {
                loc: 0.0,
                scale: 1.0,
            }),
        };

        columns_map.insert(
            col_name.clone(),
            crate::synth::model::ColumnModel {
                logical_type: match col_profile.logical_type.as_str() {
                    "numerical" => crate::synth::model::LogicalType::Numerical,
                    "categorical" => crate::synth::model::LogicalType::Categorical,
                    _ => crate::synth::model::LogicalType::Numerical,
                },
                rounding: None,
                datetime_epoch: None,
                marginal,
            },
        );
    }

    let model = TableModel {
        version: 1,
        table: table_name.to_string(),
        dialect: "mysql".to_string(),
        provenance: crate::synth::model::Provenance {
            source: "native".to_string(),
            converter_version: None,
            sdv_version: None,
        },
        pk: vec![],
        columns: columns_map,
        copula: crate::synth::model::CopulaInfo {
            column_order: profile.columns.keys().cloned().collect(),
            correlation: vec![vec![1.0]],
        },
    };

    let model_path = model_dir.join(format!("{}.model.json", table_name));
    model.save(&model_path)?;

    println!("Model saved to {}", model_path.display());
    Ok(())
}

pub fn run_rules_draft(
    tables: &[String],
    foreign_keys: &[crate::synth::rules_draft::ForeignKeyInfo],
    output: &std::path::Path,
) -> Result<(), String> {
    let table_stats = std::collections::HashMap::new();
    let rules = generate_rules_draft(tables, foreign_keys, &table_stats);

    let yaml = serde_yaml::to_string(&rules).map_err(|e| format!("serialize rules: {}", e))?;

    std::fs::write(output, yaml).map_err(|e| format!("write rules file: {}", e))?;

    println!("Rules draft saved to {}", output.display());
    Ok(())
}

pub fn run_generate(
    models_dir: &std::path::Path,
    rules_path: &std::path::Path,
    output_dir: &std::path::Path,
    rows_per_table: Option<usize>,
    seed: Option<u64>,
    format: &str,
) -> Result<(), String> {
    std::fs::create_dir_all(output_dir).map_err(|e| format!("create output dir: {}", e))?;

    let rules = SynthRules::load(rules_path)?;

    let mut models = HashMap::new();
    for entry in std::fs::read_dir(models_dir).map_err(|e| format!("read models dir: {}", e))? {
        let entry = entry.map_err(|e| format!("read dir entry: {}", e))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                if name.ends_with(".model") {
                    let table_name = name.strip_suffix(".model").unwrap_or(name);
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
    };

    let data = generate(&models, &rules, &config)?;

    let export_format = match format {
        "csv" => ExportFormat::Csv,
        "jsonl" => ExportFormat::Jsonl,
        "json" => ExportFormat::Json,
        "sql" => ExportFormat::Sql,
        _ => return Err(format!("unsupported format: {}", format)),
    };

    export(&data.tables, &export_format, output_dir)?;

    println!(
        "Generation complete. Data saved to {}",
        output_dir.display()
    );
    Ok(())
}

pub fn run_validate(model_path: &std::path::Path) -> Result<(), String> {
    let model = TableModel::load(model_path)?;

    println!("Model validation passed:");
    println!("  Table: {}", model.table);
    println!("  Columns: {}", model.columns.len());
    println!("  Dialect: {}", model.dialect);

    Ok(())
}

pub fn run_import_sdv(pkl_path: &std::path::Path, output: &std::path::Path) -> Result<(), String> {
    let _content = std::fs::read(pkl_path).map_err(|e| format!("read pkl file: {}", e))?;
    let _ = output;

    Err("SDV pkl import not yet implemented".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_model_file() {
        let temp_dir = std::env::temp_dir().join("synth_test_validate");
        std::fs::create_dir_all(&temp_dir).unwrap();

        let model = TableModel {
            version: 1,
            table: "test".to_string(),
            dialect: "mysql".to_string(),
            provenance: crate::synth::model::Provenance {
                source: "test".to_string(),
                converter_version: None,
                sdv_version: None,
            },
            pk: vec![],
            columns: std::collections::HashMap::new(),
            copula: crate::synth::model::CopulaInfo {
                column_order: vec![],
                correlation: vec![],
            },
        };

        let model_path = temp_dir.join("test.model.json");
        model.save(&model_path).unwrap();

        let result = run_validate(&model_path);
        assert!(result.is_ok());

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }
}
