#[cfg(feature = "synth")]
pub mod cmd;
#[cfg(feature = "synth")]
pub mod copula;
#[cfg(feature = "synth")]
pub mod export;
#[cfg(feature = "synth")]
pub mod fk_pool;
#[cfg(feature = "synth")]
pub mod generator;
#[cfg(feature = "synth")]
pub mod graph;
#[cfg(feature = "synth")]
pub mod marginal;
#[cfg(feature = "synth")]
pub mod model;
#[cfg(feature = "synth")]
pub mod profile;
#[cfg(feature = "synth")]
pub mod report;
#[cfg(feature = "synth")]
pub mod rules;
#[cfg(feature = "synth")]
pub mod rules_draft;

#[cfg(feature = "synth")]
use std::collections::HashMap;
#[cfg(feature = "synth")]
use std::path::{Path, PathBuf};

#[cfg(feature = "synth")]
const EXIT_OK: i32 = 0;
#[cfg(feature = "synth")]
const EXIT_ERROR: i32 = 1;

#[cfg(feature = "synth")]
pub async fn run(args: cmd::SynthArgs, config_path: Option<String>) -> i32 {
    let code = match args.command {
        cmd::SynthCommand::Train {
            name,
            tables,
            schema,
            output,
            sample,
        } => {
            run_train(
                name,
                &tables,
                schema.as_deref(),
                Path::new(&output),
                sample,
                config_path,
            )
            .await
        }
        cmd::SynthCommand::RulesDraft {
            name,
            tables,
            schema,
            output,
            models,
        } => {
            run_rules_draft(
                name,
                &tables,
                schema,
                Path::new(&output),
                Path::new(&models),
                config_path,
            )
            .await
        }
        cmd::SynthCommand::Generate {
            models,
            rules,
            output,
            rows,
            seed,
            format,
            enforce_min_max_values,
        } => cmd::run_generate(&models, &rules, &output, rows, seed, &format, enforce_min_max_values),
        cmd::SynthCommand::Validate { model } => cmd::run_validate(&model),
    };

    match code {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("error: {}", e);
            EXIT_ERROR
        }
    }
}

#[cfg(feature = "synth")]
fn resolve_connection(
    raw: &crate::config::McpRawConfig,
    name: &Option<String>,
) -> Result<crate::config::ResolvedConnection, String> {
    let target_name = name.as_deref().unwrap_or(raw.default_name.as_str());
    let target = raw
        .connections
        .iter()
        .find(|c| c.name == target_name)
        .ok_or_else(|| {
            let available: Vec<&str> = raw.connections.iter().map(|c| c.name.as_str()).collect();
            format!(
                "connection '{}' not found\n  available: {:?}",
                target_name, available
            )
        })?;

    if raw.is_env_var {
        Ok(crate::config::resolve_env_var_connection(
            target.url.clone().unwrap_or_default(),
        ))
    } else {
        crate::config::resolve_single_connection(
            target,
            raw.config_path.clone(),
            raw.base_timeout.as_ref(),
        )
    }
}

#[cfg(feature = "synth")]
async fn connect(
    side: &crate::config::ResolvedConnection,
) -> Result<Box<dyn crate::backend::DbConn + Send>, String> {
    let scheme = side
        .connection_url
        .find("://")
        .map(|i| &side.connection_url[..i])
        .unwrap_or("mysql");
    let registry = crate::create_registry();
    let pool = registry
        .connect_with_fallback(scheme, &side.connection_url, Some(&side.timeout_config))
        .await
        .map_err(|e| format!("connect '{}': {}", side.name, e))?;
    pool.acquire().await.map_err(|e| e.to_string())
}

#[cfg(feature = "synth")]
fn split_tables(tables: &str) -> Vec<String> {
    tables
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

#[cfg(feature = "synth")]
async fn run_train(
    name: Option<String>,
    tables: &str,
    schema: Option<&str>,
    output_dir: &Path,
    sample: usize,
    config_path: Option<String>,
) -> Result<(), String> {
    let tables = split_tables(tables);
    if tables.is_empty() {
        return Err("--tables must list at least one table".to_string());
    }
    std::fs::create_dir_all(output_dir).map_err(|e| format!("create output dir: {}", e))?;

    let raw =
        crate::config::read_config(config_path.map(PathBuf::from)).map_err(|e| e.to_string())?;
    let side = resolve_connection(&raw, &name)?;
    let mut conn = connect(&side).await?;

    let scheme = side
        .connection_url
        .find("://")
        .map(|i| side.connection_url[..i].to_string())
        .unwrap_or_else(|| "mysql".to_string());

    for table in &tables {
        let sql = {
            let dialect = conn.dialect();
            dialect.add_limit(
                &format!("SELECT * FROM {}", dialect.quote_table(schema, table)),
                sample,
            )
        };
        let result = conn
            .query(&sql)
            .await
            .map_err(|e| format!("sample table '{}': {}", table, e))?;

        let profile =
            crate::synth::profile::TableProfile::from_rows(table, &result.columns, &result.rows);
        let (model, skipped) = cmd::build_model(table, &scheme, &profile, &result.rows)?;
        for col in &skipped {
            eprintln!(
                "warning: table '{}': column '{}' skipped (unsupported or untrainable type)",
                table, col
            );
        }

        let model_path = output_dir.join(format!("{}.model.json", table));
        let profile_path = output_dir.join(format!("{}.profile.json", table));
        model.save(&model_path)?;
        profile.save(&profile_path)?;
        println!(
            "trained {}: {} columns, {} rows sampled -> {}",
            table,
            model.copula.column_order.len(),
            result.row_count,
            model_path.display()
        );
    }

    Ok(())
}

#[cfg(feature = "synth")]
async fn run_rules_draft(
    name: Option<String>,
    tables: &str,
    schema: Option<String>,
    output: &Path,
    models_dir: &Path,
    config_path: Option<String>,
) -> Result<(), String> {
    let tables = split_tables(tables);
    if tables.is_empty() {
        return Err("--tables must list at least one table".to_string());
    }

    let raw =
        crate::config::read_config(config_path.map(PathBuf::from)).map_err(|e| e.to_string())?;
    let side = resolve_connection(&raw, &name)?;
    let mut conn = connect(&side).await?;

    // 与 train 的 --schema 语义一致：显式指定优先，否则取连接默认 schema，
    // 保证 FK 发现与训练看到同一张表
    let schema = match schema {
        Some(s) => s,
        None => {
            crate::delta_diff::side_schema_from_conn(&mut *conn, &side.connection_url, &side.name)
                .await?
        }
    };

    let fk_sql = conn.dialect().foreign_keys_sql(&schema);
    let result = conn
        .query(&fk_sql)
        .await
        .map_err(|e| format!("query foreign keys: {}", e))?;
    let foreign_keys = cmd::parse_foreign_keys(&result)?;

    let profiles = cmd::load_profiles(models_dir).unwrap_or_else(|e| {
        eprintln!("warning: ignoring trained profiles: {}", e);
        HashMap::new()
    });

    let rules =
        crate::synth::rules_draft::generate_draft_from_profiles(&tables, &foreign_keys, &profiles);

    let yaml = serde_yaml::to_string(&rules).map_err(|e| format!("serialize rules: {}", e))?;
    std::fs::write(output, yaml).map_err(|e| format!("write rules file: {}", e))?;
    println!("Rules draft saved to {}", output.display());
    Ok(())
}

#[cfg(feature = "synth")]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_tables_trims_and_skips_empty() {
        assert_eq!(
            split_tables(" a , b,,  c "),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(split_tables(" , ").is_empty());
    }
}
