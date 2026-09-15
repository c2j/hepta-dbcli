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
use crate::audit::event::{
    ActionClass, AuditOutcome, Channel, ConnectionInfo, Decision, DraftEvent,
};

#[cfg(feature = "synth")]
const EXIT_OK: i32 = 0;
#[cfg(feature = "synth")]
const EXIT_ERROR: i32 = 1;

#[cfg(feature = "synth")]
pub async fn run(
    args: cmd::SynthArgs,
    config_path: Option<String>,
    audit: &crate::audit::AuditSession,
) -> i32 {
    let (subcommand, detail) = synth_subcommand_detail(&args.command);
    audit.record_best_effort(synth_start_event(&subcommand, &detail));

    let started = std::time::Instant::now();
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
        } => cmd::run_generate(
            &models,
            &rules,
            &output,
            rows,
            seed,
            &format,
            enforce_min_max_values,
        ),
        cmd::SynthCommand::Validate { model } => cmd::run_validate(&model),
    };
    let duration_ms = started.elapsed().as_millis() as u64;

    let outcome = match &code {
        Ok(()) => AuditOutcome::ok(duration_ms),
        Err(_) => AuditOutcome::error(duration_ms, "synth"),
    };
    audit.record_best_effort(synth_outcome_event(&subcommand, &detail, outcome));

    match code {
        Ok(()) => EXIT_OK,
        Err(e) => {
            eprintln!("error: {}", e);
            EXIT_ERROR
        }
    }
}

// ─── Audit event builders (issue #57) ────────────────────────────────────

/// Map a subcommand to its audit (subcommand, detail) pair. The detail holds
/// only metadata (table names / rules path / model name); generated rows are
/// never recorded.
#[cfg(feature = "synth")]
fn synth_subcommand_detail(command: &cmd::SynthCommand) -> (String, String) {
    match command {
        cmd::SynthCommand::Train { tables, .. } => {
            ("train".to_string(), format!("tables={tables}"))
        }
        cmd::SynthCommand::RulesDraft { tables, output, .. } => (
            "rules-draft".to_string(),
            format!("rules={output}; tables={tables}"),
        ),
        cmd::SynthCommand::Generate { models, rules, .. } => (
            "generate".to_string(),
            format!("models={models}; rules={rules}"),
        ),
        cmd::SynthCommand::Validate { model } => ("validate".to_string(), format!("model={model}")),
    }
}

/// Synth operates on local files/models, not a database session.
#[cfg(feature = "synth")]
fn synth_connection() -> ConnectionInfo {
    ConnectionInfo {
        name: "(local)".to_string(),
        driver: "none".to_string(),
        user: None,
        host: None,
        port: None,
        database: None,
        read_only_session: true,
    }
}

/// Action-specific context for a `synth` event: the subcommand plus metadata
/// only (table names / rules path / model name). Generated rows are never
/// recorded.
#[cfg(feature = "synth")]
fn synth_detail(subcommand: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({ "subcommand": subcommand, "detail": detail })
}

#[cfg(feature = "synth")]
fn synth_start_event(subcommand: &str, detail: &str) -> DraftEvent {
    DraftEvent::new(
        Channel::Synth,
        synth_connection(),
        "synth",
        ActionClass::Meta,
        Decision::Allow,
    )
    .with_detail(synth_detail(subcommand, detail))
}

#[cfg(feature = "synth")]
fn synth_outcome_event(subcommand: &str, detail: &str, outcome: AuditOutcome) -> DraftEvent {
    let decision = if outcome.ok {
        Decision::Allow
    } else {
        Decision::Error
    };
    DraftEvent::new(
        Channel::Synth,
        synth_connection(),
        "synth",
        ActionClass::Meta,
        decision,
    )
    .with_detail(synth_detail(subcommand, detail))
    .with_outcome(outcome)
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
        .connect_with_fallback(
            scheme,
            &side.connection_url,
            Some(&side.timeout_config),
            false,
        )
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

    let schema = match schema {
        Some(s) => s.to_string(),
        None => {
            crate::delta_diff::side_schema_from_conn(&mut *conn, &side.connection_url, &side.name)
                .await?
        }
    };

    for table in &tables {
        let (col_sql, idx_sql, sample_sql) = {
            let dialect = conn.dialect();
            (
                dialect.table_columns().to_string(),
                dialect.table_indexes().to_string(),
                dialect.add_limit(
                    &format!(
                        "SELECT * FROM {}",
                        dialect.quote_table(Some(&schema), table)
                    ),
                    sample,
                ),
            )
        };
        let col_result =
            crate::delta_diff::metadata::exec_or_inline(&mut *conn, &col_sql, &schema, table)
                .await
                .map_err(|e| format!("describe columns for '{}': {}", table, e))?;
        let idx_result =
            crate::delta_diff::metadata::exec_or_inline(&mut *conn, &idx_sql, &schema, table)
                .await
                .map_err(|e| format!("describe indexes for '{}': {}", table, e))?;
        let data_types = cmd::parse_column_types(&col_result);

        let result = conn
            .query(&sample_sql)
            .await
            .map_err(|e| format!("sample table '{}': {}", table, e))?;

        let pk = cmd::reconcile_primary_key(cmd::parse_primary_key(&idx_result), &result.columns);

        let profile = crate::synth::profile::TableProfile::from_rows_typed(
            table,
            &result.columns,
            &result.rows,
            Some(&data_types),
        );
        let (model, skipped) = cmd::build_model(table, &scheme, &profile, &result.rows, pk)?;
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
        let pk_note = if model.pk.is_empty() {
            String::new()
        } else {
            format!(", pk [{}]", model.pk.join(", "))
        };
        println!(
            "trained {}: {} columns, {} rows sampled{} -> {}",
            table,
            model.copula.column_order.len(),
            result.row_count,
            pk_note,
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

    #[test]
    fn synth_subcommand_detail_maps_every_variant() {
        use cmd::SynthCommand;
        assert_eq!(
            synth_subcommand_detail(&SynthCommand::Train {
                name: None,
                tables: "users,orders".to_string(),
                schema: None,
                output: ".synth".to_string(),
                sample: 1000,
            }),
            ("train".to_string(), "tables=users,orders".to_string())
        );
        assert_eq!(
            synth_subcommand_detail(&SynthCommand::RulesDraft {
                name: None,
                tables: "orders".to_string(),
                schema: None,
                output: "rules.yaml".to_string(),
                models: ".synth".to_string(),
            }),
            (
                "rules-draft".to_string(),
                "rules=rules.yaml; tables=orders".to_string()
            )
        );
        assert_eq!(
            synth_subcommand_detail(&SynthCommand::Generate {
                models: "m".to_string(),
                rules: "r.yaml".to_string(),
                output: "out".to_string(),
                rows: None,
                seed: None,
                format: "csv".to_string(),
                enforce_min_max_values: true,
            }),
            ("generate".to_string(), "models=m; rules=r.yaml".to_string())
        );
        assert_eq!(
            synth_subcommand_detail(&SynthCommand::Validate {
                model: "m.model.json".to_string()
            }),
            ("validate".to_string(), "model=m.model.json".to_string())
        );
    }

    #[test]
    fn synth_start_event_has_channel_action_and_decision() {
        let e = synth_start_event("train", "tables=users");
        assert_eq!(e.channel, Channel::Synth);
        assert_eq!(e.action, "synth");
        assert_eq!(e.class, ActionClass::Meta);
        assert_eq!(e.decision, Decision::Allow);
        assert_eq!(e.connection.name, "(local)");
        assert!(e.outcome.is_none());
    }

    #[test]
    fn synth_start_event_records_subcommand_and_detail() {
        let e = synth_start_event("generate", "models=m; rules=r.yaml");
        assert!(e.sql.is_none(), "synth has no SQL text");
        let v = e.detail.expect("detail");
        assert_eq!(v["subcommand"], "generate");
        assert_eq!(v["detail"], "models=m; rules=r.yaml");
    }

    #[test]
    fn synth_outcome_records_duration_without_rows() {
        let e = synth_outcome_event("generate", "models=m; rules=r.yaml", AuditOutcome::ok(7));
        let o = e.outcome.expect("outcome");
        assert!(o.ok);
        assert_eq!(o.duration_ms, 7);
        assert!(o.row_count.is_none());
        assert!(o.rows_affected.is_none());
    }

    #[test]
    fn synth_events_never_record_generated_rows() {
        let start = synth_start_event("generate", "models=m; rules=r.yaml");
        let v = start.detail.expect("detail");
        let text = v.to_string();
        assert!(text.len() < 512, "detail must stay metadata-only");
        assert!(!text.contains("row"), "must not record generated rows");
    }

    #[test]
    fn synth_error_outcome_marks_decision_error() {
        let e = synth_outcome_event("train", "tables=users", AuditOutcome::error(3, "synth"));
        assert_eq!(e.decision, Decision::Error);
    }
}
