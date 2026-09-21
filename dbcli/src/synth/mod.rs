#[cfg(feature = "synth")]
pub mod cardinality;
#[cfg(feature = "synth")]
pub mod cmd;
#[cfg(feature = "synth")]
pub mod copula;
#[cfg(feature = "synth")]
pub mod datetime;
#[cfg(feature = "synth")]
pub mod export;
#[cfg(feature = "synth")]
pub mod expr;
#[cfg(feature = "synth")]
pub mod fk_pool;
#[cfg(feature = "synth")]
pub mod generator;
#[cfg(feature = "synth")]
pub mod marginal;
#[cfg(feature = "synth")]
pub mod mine;
#[cfg(feature = "synth")]
pub mod model;
#[cfg(feature = "synth")]
pub mod pii;
#[cfg(feature = "synth")]
pub mod profile;
#[cfg(feature = "synth")]
pub mod quality;
#[cfg(feature = "synth")]
pub mod report;
#[cfg(feature = "synth")]
pub mod rules;
#[cfg(feature = "synth")]
pub mod rules_draft;
#[cfg(feature = "synth")]
pub mod stats;

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
            categorical_top_k,
            rules,
            holdout_ratio,
        } => {
            run_train(
                name,
                &tables,
                TrainRunOptions {
                    schema: schema.as_deref(),
                    output_dir: Path::new(&output),
                    sample,
                    categorical_top_k,
                    rules_path: rules.as_deref(),
                    holdout_ratio,
                },
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
            mine,
        } => {
            run_rules_draft(
                name,
                &tables,
                schema,
                Path::new(&output),
                Path::new(&models),
                mine,
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
            no_schema_qualifier,
        } => cmd::run_generate(
            &models,
            &rules,
            &output,
            rows,
            seed,
            &format,
            cmd::GenerateFlags {
                enforce_min_max_values,
                no_schema_qualifier,
            },
        ),
        cmd::SynthCommand::Validate { model } => cmd::run_validate(&model),
        cmd::SynthCommand::Report {
            models,
            data,
            rules,
            against_db,
            rows,
            seed,
            output,
            min_score,
            strict,
        } => {
            run_report(
                ReportRunOptions {
                    models,
                    data,
                    rules,
                    against_db,
                    rows,
                    seed,
                    output,
                    min_score,
                    strict,
                },
                config_path,
            )
            .await
        }
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
        cmd::SynthCommand::Report {
            models,
            data,
            output,
            ..
        } => {
            let mut detail = format!(
                "models={}; data={}",
                models,
                data.as_deref().unwrap_or("(generated)")
            );
            if let Some(output) = output {
                detail.push_str(&format!("; output={output}"));
            }
            ("report".to_string(), detail)
        }
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
        crate::config::resolve_env_var_connection(target.url.clone().unwrap_or_default())
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

/// Reject `schema.table` dotted names up front. The rest of the pipeline
/// quotes the name as a single identifier, so a dotted name surfaces much
/// later as a confusing `relation "db.schema.table" does not exist`
/// (double qualification) instead of pointing at the flag to use.
#[cfg(feature = "synth")]
fn check_table_names(tables: &[String]) -> Result<(), String> {
    if let Some(dotted) = tables.iter().find(|t| t.contains('.')) {
        let (schema, table) = dotted.split_once('.').expect("checked contains '.'");
        return Err(format!(
            "--tables entry '{dotted}' uses schema-qualified `schema.table` \
             notation, which is not supported; pass the table list without \
             the qualifier and select the schema with `--schema {schema}` \
             (table: '{table}')"
        ));
    }
    Ok(())
}

/// Shared `--schema` resolution for train and rules-draft: an explicit
/// non-empty value wins, otherwise the connection default schema.
fn resolve_schema(explicit: Option<&str>, connection_default: String) -> String {
    match explicit {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => connection_default,
    }
}

/// Known-limitation #11: the warning shown when `--against-db` names a
/// connection while `HEPTA_DBCLI_URL` is set (env connections are always
/// named `default`). Pure so tests can exercise both branches.
fn against_db_env_warning(name: Option<&str>, env_url_set: bool) -> Option<String> {
    name.filter(|_| env_url_set).map(|name| {
        format!(
            "warning: HEPTA_DBCLI_URL is set, so the --against-db connection name \
                 '{}' is ignored (the env-var connection is always named \
                 'default'); pass a --config file to address a named connection",
            name
        )
    })
}

/// Schema precedence shared by `train` / `rules-draft` / `report`: an explicit
/// `--schema` flag beats the configured `[connections.X] schema`, which beats
/// the driver probe. `Some(explicit)/Some(configured)` short-circuit without
/// touching the connection; `None` means "probe the driver".
fn schema_priority(
    explicit: Option<&str>,
    connection_default_schema: Option<&str>,
) -> Option<String> {
    if let Some(s) = explicit.filter(|s| !s.is_empty()) {
        return Some(s.to_string());
    }
    // Configured `[connections.X] schema` beats the driver probe, but an
    // explicit `--schema` flag beats both.
    connection_default_schema
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

async fn resolved_side_schema(
    explicit: Option<&str>,
    conn: &mut (dyn crate::backend::DbConn + Send),
    url: &str,
    name: &str,
    connection_default_schema: Option<&str>,
) -> Result<String, String> {
    if let Some(s) = schema_priority(explicit, connection_default_schema) {
        return Ok(s);
    }
    crate::delta_diff::side_schema_from_conn(conn, url, name).await
}

#[cfg(feature = "synth")]
const FULL_MODEL_SIZE_WARN_BYTES: u64 = 10 * 1024 * 1024;
const ORACLE_DRIVER_PREFETCH_CAP: usize = 100;

/// Pure-Rust Oracle driver silently stops at 100 prefetched rows. A sample
/// that lands exactly on that cap is treated as truncated so train can warn
/// and record `provenance.truncated`.
///
/// `requested_sample` is the table's `--sample` value: asking for 100 rows (or
/// fewer) makes the cap the caller's own limit, so it is not a truncation.
/// A table that genuinely holds exactly 100 rows still produces a false
/// positive when more were requested - that is inherent to the heuristic.
fn sample_may_be_truncated(scheme: &str, row_count: usize, requested_sample: usize) -> bool {
    scheme.eq_ignore_ascii_case("oracle")
        && row_count == ORACLE_DRIVER_PREFETCH_CAP
        && requested_sample > ORACLE_DRIVER_PREFETCH_CAP
}

/// Flags of a `synth train` run, grouped so the function signature stays
/// readable as options grow.
struct TrainRunOptions<'a> {
    schema: Option<&'a str>,
    output_dir: &'a Path,
    sample: usize,
    categorical_top_k: cmd::CategoricalTopK,
    rules_path: Option<&'a str>,
    holdout_ratio: f64,
}

async fn run_train(
    name: Option<String>,
    tables: &str,
    options: TrainRunOptions<'_>,
    config_path: Option<String>,
) -> Result<(), String> {
    let TrainRunOptions {
        schema,
        output_dir,
        sample,
        categorical_top_k,
        rules_path,
        holdout_ratio,
    } = options;
    let tables = split_tables(tables);
    check_table_names(&tables)?;
    if tables.is_empty() {
        return Err("--tables must list at least one table".to_string());
    }
    std::fs::create_dir_all(output_dir).map_err(|e| format!("create output dir: {}", e))?;

    // Per-column overrides, keyed by table then column. Optional: a rules file
    // only needs the `columns` section to steer training (marginal family and
    // PII handling, issue #71).
    type SdTypeOverride = (crate::synth::rules::SdType, Option<String>);
    let mut forced_marginals: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut forced_sdtype: HashMap<String, HashMap<String, SdTypeOverride>> = HashMap::new();
    if let Some(path) = rules_path {
        let rules = crate::synth::rules::SynthRules::load(Path::new(path))?;
        rules.validate()?;
        for table in &rules.tables {
            for (column, rule) in &table.columns {
                if let Some(marginal) = &rule.marginal {
                    forced_marginals
                        .entry(table.name.clone())
                        .or_default()
                        .insert(column.clone(), marginal.clone());
                }
                if !matches!(rule.sdtype, crate::synth::rules::SdType::Auto) {
                    forced_sdtype
                        .entry(table.name.clone())
                        .or_default()
                        .insert(column.clone(), (rule.sdtype, rule.pii_provider.clone()));
                }
            }
        }
    }

    let raw =
        crate::config::read_config(config_path.map(PathBuf::from)).map_err(|e| e.to_string())?;
    let side = resolve_connection(&raw, &name)?;
    let mut conn = connect(&side).await?;

    let scheme = side
        .connection_url
        .find("://")
        .map(|i| side.connection_url[..i].to_string())
        .unwrap_or_else(|| "mysql".to_string());

    let schema = resolved_side_schema(
        schema,
        &mut *conn,
        &side.connection_url,
        &side.name,
        side.default_schema.as_deref(),
    )
    .await?;

    // Foreign keys are read once so training can learn each child table's
    // rows-per-parent-key distribution (issue #72). A dialect without FK
    // metadata degrades to "no cardinality learned", never a failed train.
    let foreign_keys = match conn.query(&conn.dialect().foreign_keys_sql(&schema)).await {
        Ok(result) => cmd::parse_foreign_keys(&result).unwrap_or_else(|e| {
            eprintln!("warning: not learning cardinality: {e}");
            Vec::new()
        }),
        Err(e) => {
            eprintln!("warning: not learning cardinality: {e}");
            Vec::new()
        }
    };
    let mut key_distinct: HashMap<(String, String), usize> = HashMap::new();
    let mut fk_values: HashMap<(String, String), Vec<serde_json::Value>> = HashMap::new();

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

        // Cardinality inputs: the distinct count of this table's key (for the
        // `0` bucket of a child's distribution) and the FK value columns
        // (issue #72). Both are read from the same sample the model is fitted
        // on, so no extra query is needed.
        if let Some(pk_column) = pk.first() {
            if let Some(index) = result.columns.iter().position(|column| column == pk_column) {
                key_distinct.insert(
                    (table.clone(), pk_column.clone()),
                    distinct_non_null(&result.rows, index),
                );
            }
        }
        for fk in foreign_keys.iter().filter(|fk| &fk.from_table == table) {
            if let Some(index) = result
                .columns
                .iter()
                .position(|column| column == &fk.from_column)
            {
                fk_values.insert(
                    (table.clone(), fk.from_column.clone()),
                    result
                        .rows
                        .iter()
                        .filter_map(|row| row.get(index).cloned())
                        .collect(),
                );
            }
        }

        let mut profile = crate::synth::profile::TableProfile::from_rows_typed(
            table,
            &result.columns,
            &result.rows,
            Some(&data_types),
            categorical_top_k.cap(),
        );
        let forced = forced_marginals.get(table);
        let (mut model, skipped) = cmd::build_model_with_overrides(
            table,
            &scheme,
            &profile,
            &result.rows,
            pk,
            Some(schema.clone()),
            forced,
        )?;
        for col in &skipped {
            eprintln!(
                "warning: table '{}': column '{}' skipped (unsupported or untrainable type)",
                table, col
            );
        }

        // PII columns are anonymized before anything is written (issue #71):
        // the model loses their observed dictionary/range and leaves the
        // correlation matrix, and the profile drops their top values. Foreign
        // keys are exempt because generation assigns them from the parent pool
        // before the PII fill; a primary key is *not* exempt, and SQL-export
        // uniqueness redraws it through the provider.
        let reserved = pii_reserved_columns(&foreign_keys, table);
        let mut detected = detect_pii_columns(
            &result.columns,
            &result.rows,
            &data_types,
            forced_sdtype.get(table),
        );
        let skipped: Vec<String> = detected
            .keys()
            .filter(|column| reserved.contains(*column))
            .cloned()
            .collect();
        for column in skipped {
            eprintln!(
                "warning: table '{}': column '{}' is a foreign key; PII anonymization skipped \
                 (generation fills it from the parent pool)",
                table, column
            );
        }
        detected.retain(|column, _| !reserved.contains(column));
        for (column, provider) in detected {
            if let Some(model_column) = model.columns.get_mut(&column) {
                crate::synth::pii::anonymize_model_column(model_column, provider);
            }
            zero_correlation(&mut model, &column);
            if let Some(profile_column) = profile.columns.get_mut(&column) {
                profile_column.top_values = None;
            }
        }

        if sample_may_be_truncated(&scheme, result.row_count, sample) {
            eprintln!(
                "warning: Oracle driver truncated the sample of table '{}' at {} rows; \
                 fitted distributions may be distorted",
                table, ORACLE_DRIVER_PREFETCH_CAP
            );
            model.provenance.truncated = true;
        }
        for (column, size) in cmd::oversized_marginal_columns(&model) {
            eprintln!(
                "warning: table '{}': column '{}' marginal serializes to {} bytes, over the {} byte budget",
                table,
                column,
                size,
                cmd::COLUMN_MARGINAL_BUDGET_BYTES
            );
        }

        let model_path = output_dir.join(format!("{}.model.json", table));
        let profile_path = output_dir.join(format!("{}.profile.json", table));
        model.save(&model_path)?;
        if categorical_top_k == cmd::CategoricalTopK::Full {
            if let Ok(meta) = std::fs::metadata(&model_path) {
                if meta.len() > FULL_MODEL_SIZE_WARN_BYTES {
                    eprintln!(
                        "warning: {} is {:.1} MiB; --categorical-top-k full stored every dictionary level and the file exceeds 10 MiB",
                        model_path.display(),
                        meta.len() as f64 / (1024.0 * 1024.0)
                    );
                }
            }
        }
        profile.save(&profile_path)?;
        write_report_baseline(
            table,
            &model,
            &profile,
            &result.rows,
            output_dir,
            holdout_ratio,
        )?;
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

    attach_fk_cardinality(output_dir, &foreign_keys, &key_distinct, &fk_values)?;

    Ok(())
}

/// Number of distinct non-NULL values of one row column.
#[cfg(feature = "synth")]
fn distinct_non_null(rows: &[Vec<serde_json::Value>], index: usize) -> usize {
    rows.iter()
        .filter_map(|row| row.get(index))
        .filter(|value| !value.is_null())
        .map(|value| value.to_string())
        .collect::<std::collections::HashSet<String>>()
        .len()
}

/// Columns anonymization must leave alone: foreign keys, which generation
/// fills from the parent pool. Primary keys are *not* reserved: SQL-export
/// uniqueness redraws a PII key through its provider.
#[cfg(feature = "synth")]
fn pii_reserved_columns(
    foreign_keys: &[crate::synth::rules_draft::ForeignKeyInfo],
    table: &str,
) -> std::collections::HashSet<String> {
    foreign_keys
        .iter()
        .filter(|fk| fk.from_table == table)
        .map(|fk| fk.from_column.clone())
        .collect()
}

/// PII columns of one table: `sdtype: keep` disables, `sdtype: pii` forces a
/// provider (falling back to the recognizer, then to `name`), and `auto` uses
/// the recognizer (issue #71).
#[cfg(feature = "synth")]
fn detect_pii_columns(
    columns: &[String],
    rows: &[Vec<serde_json::Value>],
    types: &HashMap<String, String>,
    overrides: Option<&HashMap<String, (crate::synth::rules::SdType, Option<String>)>>,
) -> HashMap<String, crate::synth::pii::PiiProvider> {
    let mut detected = HashMap::new();
    for (index, name) in columns.iter().enumerate() {
        let override_ = overrides.and_then(|map| map.get(name));
        let sdtype = override_
            .map(|(sdtype, _)| *sdtype)
            .unwrap_or(crate::synth::rules::SdType::Auto);
        if matches!(sdtype, crate::synth::rules::SdType::Keep) {
            continue;
        }
        let samples: Vec<serde_json::Value> = rows
            .iter()
            .filter_map(|row| row.get(index).cloned())
            .collect();
        let guess = crate::synth::pii::detect(name, &samples, types.get(name).map(String::as_str));
        let provider = if matches!(sdtype, crate::synth::rules::SdType::Pii) {
            override_
                .and_then(|(_, provider)| provider.as_deref())
                .and_then(crate::synth::pii::PiiProvider::parse)
                .or(guess)
                .unwrap_or(crate::synth::pii::PiiProvider::Name)
        } else {
            match guess {
                Some(provider) => provider,
                None => continue,
            }
        };
        detected.insert(name.clone(), provider);
    }
    detected
}

/// Drop a column from the copula correlation matrix (diagonal 1, everything
/// else 0): a PII column is generated independently, so it must not shape the
/// other columns' joint draw (issue #71).
#[cfg(feature = "synth")]
fn zero_correlation(model: &mut crate::synth::model::TableModel, column: &str) {
    let Some(index) = model
        .copula
        .column_order
        .iter()
        .position(|name| name == column)
    else {
        return;
    };
    let size = model.copula.correlation.len();
    for other in 0..size {
        if let Some(row) = model.copula.correlation.get_mut(index) {
            if let Some(cell) = row.get_mut(other) {
                *cell = if other == index { 1.0 } else { 0.0 };
            }
        }
        if other != index {
            if let Some(row) = model.copula.correlation.get_mut(other) {
                if let Some(cell) = row.get_mut(index) {
                    *cell = 0.0;
                }
            }
        }
    }
}

#[cfg(feature = "synth")]
fn learned_cardinality(
    models: &HashMap<String, crate::synth::model::TableModel>,
) -> HashMap<(String, String), crate::synth::cardinality::CardinalityDist> {
    let mut learned = HashMap::new();
    for (table, model) in models {
        for (column, distribution) in &model.fk_cardinality {
            learned.insert((table.clone(), column.clone()), distribution.clone());
        }
    }
    learned
}

/// Learn each child table's rows-per-parent distribution and store it in the
/// child's model (issue #72). Runs after the training loop because a parent
/// key's distinct count may come from a table trained after its child.
#[cfg(feature = "synth")]
fn attach_fk_cardinality(
    output_dir: &Path,
    foreign_keys: &[crate::synth::rules_draft::ForeignKeyInfo],
    key_distinct: &HashMap<(String, String), usize>,
    fk_values: &HashMap<(String, String), Vec<serde_json::Value>>,
) -> Result<(), String> {
    // Load each child model once; a table may have several foreign keys.
    let mut loaded: HashMap<String, crate::synth::model::TableModel> = HashMap::new();
    let mut changed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for fk in foreign_keys {
        let Some(values) = fk_values.get(&(fk.from_table.clone(), fk.from_column.clone())) else {
            continue;
        };
        let parent_distinct = key_distinct
            .get(&(fk.to_table.clone(), fk.to_column.clone()))
            .copied();
        let Some(distribution) =
            crate::synth::cardinality::learn_cardinality(values, parent_distinct)
        else {
            continue;
        };
        let model = match loaded.entry(fk.from_table.clone()) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let path = output_dir.join(format!("{}.model.json", fk.from_table));
                entry.insert(crate::synth::model::TableModel::load(&path)?)
            }
        };
        if !model.columns.contains_key(&fk.from_column) {
            continue;
        }
        model
            .fk_cardinality
            .insert(fk.from_column.clone(), distribution);
        changed.insert(fk.from_table.clone());
    }
    for table in changed {
        if let Some(model) = loaded.get(&table) {
            let path = output_dir.join(format!("{}.model.json", table));
            model.save(&path)?;
        }
    }
    Ok(())
}

/// Rows sampled per table while mining conditional rules (issue #69). The
/// pair scan is O(c^2 * n), so the sample is capped and the decision logged.
#[cfg(feature = "synth")]
const MINE_SAMPLE_ROWS: usize = 50_000;

#[cfg(feature = "synth")]
async fn run_rules_draft(
    name: Option<String>,
    tables: &str,
    schema: Option<String>,
    output: &Path,
    models_dir: &Path,
    mine: cmd::MineArgs,
    config_path: Option<String>,
) -> Result<(), String> {
    let tables = split_tables(tables);
    check_table_names(&tables)?;
    if tables.is_empty() {
        return Err("--tables must list at least one table".to_string());
    }

    let raw =
        crate::config::read_config(config_path.map(PathBuf::from)).map_err(|e| e.to_string())?;
    let side = resolve_connection(&raw, &name)?;
    let mut conn = connect(&side).await?;

    // train 与 rules-draft 共用 resolve_schema：显式 --schema 优先，否则连接默认。
    let schema = resolved_side_schema(
        schema.as_deref(),
        &mut *conn,
        &side.connection_url,
        &side.name,
        side.default_schema.as_deref(),
    )
    .await?;

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

    // Primary keys come from the trained models; without them the heuristic
    // simply cannot tell a child key from a reference (issue #76-E), so a
    // failure here is reported instead of silently degrading the draft.
    let primary_keys = match load_primary_keys(models_dir) {
        Ok(keys) => keys,
        Err(e) => {
            eprintln!("warning: ignoring trained models: {}", e);
            HashMap::new()
        }
    };
    if let Some(warning) = implicit_fk_warning(&primary_keys, &profiles, models_dir) {
        eprintln!("warning: {}", warning);
    }

    let (rules, inferred) = crate::synth::rules_draft::generate_draft_with_implicit(
        &tables,
        &foreign_keys,
        &profiles,
        &primary_keys,
    );
    if inferred > 0 {
        eprintln!("inferred {} implicit relationship(s)", inferred);
    }

    let mut yaml = serde_yaml::to_string(&rules).map_err(|e| format!("serialize rules: {}", e))?;
    if let Some(warning) = draft_cycle_warning(&rules) {
        eprintln!("warning: {}", warning);
    }

    if mine.mine {
        let mined = mine_tables(&mut *conn, &schema, &tables, &mine).await?;
        if let Some(path) = mine.emit_candidates.as_deref() {
            std::fs::write(path, crate::synth::mine::render_candidate_report(&mined))
                .map_err(|e| format!("write candidate list: {}", e))?;
            println!("Candidate list saved to {}", path);
        }
        let total: usize = mined.iter().map(|t| t.candidates.len()).sum();
        if total > 0 {
            eprintln!(
                "mined {} conditional rule candidate(s); they are comments only (never enabled)",
                total
            );
        }
        // Candidates are appended as comments: the YAML stays parseable and
        // nothing is enabled (issue #69, AC4/A hard constraint).
        yaml = crate::synth::mine::append_candidate_comments(&yaml, &mined);
    }

    std::fs::write(output, yaml).map_err(|e| format!("write rules file: {}", e))?;
    println!("Rules draft saved to {}", output.display());
    Ok(())
}

/// Sample each table (capped at [`MINE_SAMPLE_ROWS`]) and mine conditional
/// candidates. Logs the sampling decision per table (issue #69, AC5).
#[cfg(feature = "synth")]
async fn mine_tables(
    conn: &mut (dyn crate::backend::DbConn + Send),
    schema: &str,
    tables: &[String],
    mine: &cmd::MineArgs,
) -> Result<Vec<crate::synth::mine::TableCandidates>, String> {
    let config = crate::synth::mine::MineConfig {
        confidence: mine.mine_confidence,
        support: mine.mine_support,
        max_pairs: mine.mine_max_pairs,
        exclude_pii: !mine.keep_pii_columns,
        ..crate::synth::mine::MineConfig::default()
    };

    let mut mined = Vec::new();
    for table in tables {
        let sample_sql = {
            let dialect = conn.dialect();
            dialect.add_limit(
                &format!("SELECT * FROM {}", dialect.quote_table(Some(schema), table)),
                MINE_SAMPLE_ROWS,
            )
        };
        let result = conn
            .query(&sample_sql)
            .await
            .map_err(|e| format!("mine table '{}': {}", table, e))?;
        let report = crate::synth::mine::mine_candidates(&result.columns, &result.rows, &config);
        eprintln!(
            "mining table '{}': sampled {} row(s) (cap {}), scanned {} of {} column pair(s), {} candidate(s){}",
            table,
            result.row_count,
            MINE_SAMPLE_ROWS,
            report.pairs_considered,
            report.pairs_total,
            report.candidates.len(),
            if report.pairs_truncated() {
                " - pair cap hit"
            } else {
                ""
            }
        );
        if !report.pii_skipped.is_empty() {
            eprintln!(
                "mining table '{}': PII filter skipped column(s): {}",
                table,
                report.pii_skipped.join(", ")
            );
        }
        if !report.candidates.is_empty() {
            mined.push(crate::synth::mine::TableCandidates {
                table: table.clone(),
                candidates: report.candidates,
            });
        }
    }
    Ok(mined)
}

/// Record the holdout baseline next to a freshly trained model. A ratio of 0
/// disables it (and `synth report` then skips the shapes/pairs sections).
fn write_report_baseline(
    table: &str,
    model: &crate::synth::model::TableModel,
    profile: &crate::synth::profile::TableProfile,
    rows: &[Vec<serde_json::Value>],
    output_dir: &Path,
    ratio: f64,
) -> Result<(), String> {
    if ratio <= 0.0 || !ratio.is_finite() {
        return Ok(());
    }
    let col_pos: HashMap<&str, usize> = profile
        .column_order
        .iter()
        .enumerate()
        .map(|(i, name)| (name.as_str(), i))
        .collect();
    let mut indices = Vec::with_capacity(model.copula.column_order.len());
    for name in &model.copula.column_order {
        let Some(&idx) = col_pos.get(name.as_str()) else {
            return Err(format!(
                "table '{}': column '{}' is in the model but not in the sampled order",
                table, name
            ));
        };
        indices.push(idx);
    }
    let baseline = crate::synth::quality::build_baseline(table, model, rows, &indices, ratio)?;
    let path = output_dir.join(format!("{}.report-baseline.json", table));
    baseline.save(&path)?;
    println!(
        "baseline {}: {} holdout rows -> {}",
        table,
        baseline.holdout_rows,
        path.display()
    );
    Ok(())
}

/// Pre-check a freshly drafted rules file for reference cycles. `generate`
/// would fail later with `cycle detected`; warning here (draft time) points
/// the user at the offending YAML before they build on it. Returns the
/// warning text, or `None` when the dependency graph is acyclic.
#[cfg(feature = "synth")]
fn draft_cycle_warning(rules: &crate::synth::rules::SynthRules) -> Option<String> {
    let nodes: Vec<String> = rules.tables.iter().map(|t| t.name.clone()).collect();
    let mut edges: Vec<(String, String)> = Vec::new();
    for table in &rules.tables {
        for rel in &table.relationships {
            for reference in &rel.references {
                if let Some((parent, _col)) = reference.split_once('.') {
                    if nodes.iter().any(|n| n == parent) {
                        edges.push((parent.to_string(), table.name.clone()));
                    }
                }
            }
        }
    }
    match crate::graph::topological_sort(&nodes, &edges) {
        Ok(_) => None,
        Err(msg) => Some(format!(
            "draft references form a {} — `synth generate` would fail. \
             Review the relationships in this draft (same-name timestamp \
             columns and self-references like employee.manager_id → \
             employee.id are common false positives) and delete the ones \
             that are not real foreign keys.",
            msg
        )),
    }
}

/// The implicit-FK heuristic skips a child column that is its own primary key,
/// and it can only know the primary keys from the trained models. Profiles
/// without models (for example a directory holding only `*.profile.json`) leave
/// that guard inert, so the draft can invent a reference such as
/// `orders.id -> users.id`. Returns the warning to print in that case.
fn implicit_fk_warning(
    primary_keys: &HashMap<String, String>,
    profiles: &HashMap<String, crate::synth::profile::TableProfile>,
    models_dir: &Path,
) -> Option<String> {
    if primary_keys.is_empty() && !profiles.is_empty() {
        return Some(format!(
            "trained profiles in {} carry no primary keys; implicit relationship \
             detection cannot skip a child key and may invent references \
             (retrain with `synth train` to restore it)",
            models_dir.display()
        ));
    }
    None
}

/// Primary key per table, as recorded by `synth train`. Errors are returned
/// rather than defaulted: an empty map silently weakens the implicit-FK
/// heuristic (`rules_draft`) so primary keys get mistaken for references.
fn load_primary_keys(models_dir: &Path) -> Result<HashMap<String, String>, String> {
    Ok(load_models(models_dir)?
        .into_iter()
        .filter_map(|(table, model)| model.pk.first().map(|pk| (table, pk.clone())))
        .collect())
}

/// Load every `<table>.model.json` in a models directory.
fn load_models(
    models_dir: &Path,
) -> Result<HashMap<String, crate::synth::model::TableModel>, String> {
    let mut models = HashMap::new();
    for entry in std::fs::read_dir(models_dir)
        .map_err(|e| format!("read models dir {}: {}", models_dir.display(), e))?
    {
        let entry = entry.map_err(|e| format!("read dir entry: {}", e))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(file_name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(table_name) = file_name.strip_suffix(".model") else {
            continue;
        };
        let model = crate::synth::model::TableModel::load(&path)?;
        models.insert(table_name.to_string(), model);
    }
    Ok(models)
}

struct ReportRunOptions {
    models: String,
    data: Option<String>,
    rules: Option<String>,
    against_db: Option<String>,
    rows: usize,
    seed: Option<u64>,
    output: Option<String>,
    min_score: Option<f64>,
    strict: bool,
}

async fn run_report(options: ReportRunOptions, config_path: Option<String>) -> Result<(), String> {
    use crate::synth::quality::{
        evaluate_fk, evaluate_table, fk_relations, read_generated_table, BaselineSummary, FkRate,
        QualityReport, Section, REPORT_SCHEMA_VERSION,
    };

    let models_dir = Path::new(&options.models);
    let models = load_models(models_dir)?;
    if models.is_empty() {
        return Err(format!(
            "no trained models found in {} (expected <table>.model.json)",
            models_dir.display()
        ));
    }

    let mut baselines: HashMap<String, BaselineSummary> = HashMap::new();
    let mut missing: Vec<String> = Vec::new();
    for table in models.keys() {
        let path = models_dir.join(format!("{}.report-baseline.json", table));
        if path.is_file() {
            baselines.insert(table.clone(), BaselineSummary::load(&path)?);
        } else {
            missing.push(table.clone());
        }
    }
    if options.strict && !missing.is_empty() {
        return Err(format!(
            "--strict: no report baseline for {:?} (retrain with --holdout-ratio > 0)",
            missing
        ));
    }
    for table in &missing {
        eprintln!(
            "warning: table '{}': no report baseline; shapes and pairs will be skipped",
            table
        );
    }

    let rules = match options.rules.as_deref() {
        Some(path) => {
            let rules = crate::synth::rules::SynthRules::load(Path::new(path))?;
            rules.validate()?;
            Some(rules)
        }
        None => None,
    };

    let mut table_columns: crate::synth::quality::GeneratedColumns = HashMap::new();
    let mut table_rows: crate::synth::quality::GeneratedTables = HashMap::new();
    match options.data.as_deref() {
        Some(dir) => {
            let dir = Path::new(dir);
            for table in models.keys() {
                match read_generated_table(dir, table)? {
                    Some((columns, rows)) => {
                        table_columns.insert(table.clone(), columns);
                        table_rows.insert(table.clone(), rows);
                    }
                    None => eprintln!(
                        "warning: no generated data for table '{}' in {}",
                        table,
                        dir.display()
                    ),
                }
            }
        }
        None => {
            let generated_rules = match &rules {
                Some(rules) => rules.clone(),
                None => rules_from_models(&models, options.rows),
            };
            let rows_per_table = models.keys().map(|t| (t.clone(), options.rows)).collect();
            let data = crate::synth::generator::generate(
                &models,
                &generated_rules,
                &crate::synth::generator::GeneratorConfig {
                    rows_per_table,
                    seed: options.seed,
                    enforce_min_max_values: true,
                },
            )?;
            for (table, columns) in &data.columns {
                if data.tables.contains_key(table) {
                    table_columns.insert(table.clone(), columns.clone());
                }
            }
            table_rows = data.tables;
        }
    }

    if options.strict {
        let mut without_data: Vec<&String> = models
            .keys()
            .filter(|table| !table_rows.contains_key(*table))
            .collect();
        without_data.sort();
        if !without_data.is_empty() {
            return Err(format!(
                "--strict: no generated data for {:?} (pass --data including those tables, or \
                 narrow --models)",
                without_data
            ));
        }
    }

    let relations = rules.as_ref().map(fk_relations).unwrap_or_default();
    let (parent_pools, fk_source) = match options.against_db.as_deref() {
        Some(connection) => {
            // A bare `--against-db` (empty value) means the default connection.
            let name = (!connection.is_empty()).then(|| connection.to_string());
            // Known-limitation #11: with `HEPTA_DBCLI_URL` the connection is
            // always named `default`; a user-supplied name would resolve
            // against the env-var config and fail confusingly.
            if let Some(warning) = against_db_env_warning(
                name.as_deref(),
                std::env::var(crate::config::ENV_VAR_URL).is_ok(),
            ) {
                eprintln!("{}", warning);
            }
            (
                load_real_key_pools(&relations, &name, config_path).await?,
                "database",
            )
        }
        // Without a database the pool is the generated parent column, so the
        // check is "does the child reference keys its parent actually made".
        None => (
            crate::synth::quality::generated_key_pools(&relations, &table_columns, &table_rows),
            "generated",
        ),
    };
    let fk: Section<Vec<FkRate>> = evaluate_fk(
        &relations,
        &table_columns,
        &table_rows,
        &parent_pools,
        fk_source,
        &learned_cardinality(&models),
    );

    // Every model gets a row in the report: a table whose data is missing is
    // reported as `skipped` instead of silently absent, and `--min-score`
    // additionally refuses to gate on a report with unscored tables.
    let mut table_names: Vec<&String> = models.keys().collect();
    table_names.sort();
    let mut tables = Vec::new();
    for table in table_names {
        let Some(model) = models.get(table) else {
            continue;
        };
        let (Some(columns), Some(rows)) = (table_columns.get(table), table_rows.get(table)) else {
            let reason = match options.data.as_deref() {
                Some(dir) => format!(
                    "no generated data for table '{}' in {}",
                    table,
                    Path::new(dir).display()
                ),
                None => format!("no generated data for table '{}'", table),
            };
            tables.push(missing_data_quality(table, &reason));
            continue;
        };
        let table_fk = crate::synth::quality::fk_section_for_table(&fk, table);
        tables.push(evaluate_table(
            table,
            model,
            baselines.get(table),
            columns,
            rows,
            table_fk,
        ));
    }
    let overall_score = crate::synth::quality::mean_of(tables.iter().map(|t| t.score));
    let report = QualityReport {
        schema_version: REPORT_SCHEMA_VERSION,
        tables,
        overall_score,
    };

    println!("{}", report.summary());
    if let Some(path) = options.output.as_deref() {
        report.save(Path::new(path))?;
        println!("report written to {}", path);
    }
    if let Some(min_score) = options.min_score {
        // The gate covers every model, not just the tables that produced a
        // score: `mean_of` drops `None`, so without this an unscored table
        // would let a partially scored report pass.
        let mut unscored: Vec<String> = report
            .tables
            .iter()
            .filter(|table| table.score.is_none())
            .map(|table| {
                let reason = match &table.shapes {
                    Section::Skipped { reason } => reason.clone(),
                    Section::Scored { .. } => "no scored section".to_string(),
                };
                format!("{} ({})", table.table, reason)
            })
            .collect();
        unscored.sort();
        if !unscored.is_empty() {
            return Err(format!(
                "--min-score {} requires every table to be scored, but {} could not be: {}",
                min_score,
                unscored.len(),
                unscored.join("; ")
            ));
        }
        let score = overall_score.ok_or_else(|| {
            format!(
                "--min-score {} cannot be checked: no section was scored",
                min_score
            )
        })?;
        if score < min_score {
            return Err(format!(
                "quality score {:.3} is below --min-score {:.3}",
                score, min_score
            ));
        }
        println!("quality score {:.3} >= --min-score {:.3}", score, min_score);
    }
    Ok(())
}

/// Report entry for a table whose generated data is missing: every section is
/// skipped with the same reason, so the omission is visible in the JSON.
fn missing_data_quality(table: &str, reason: &str) -> crate::synth::quality::TableQuality {
    use crate::synth::quality::TableQuality;
    TableQuality {
        table: table.to_string(),
        rows: 0,
        shapes: skipped_section(reason),
        pairs: skipped_section(reason),
        fk: skipped_section(reason),
        score: None,
    }
}

fn skipped_section<T>(reason: &str) -> crate::synth::quality::Section<T> {
    crate::synth::quality::Section::Skipped {
        reason: reason.to_string(),
    }
}

/// Rules used for on-the-fly generation: every model becomes a table with the
/// requested row count and no relationships (FKs come from `--rules`).
fn rules_from_models(
    models: &HashMap<String, crate::synth::model::TableModel>,
    rows: usize,
) -> crate::synth::rules::SynthRules {
    let mut names: Vec<&String> = models.keys().collect();
    names.sort();
    crate::synth::rules::SynthRules {
        version: "1".to_string(),
        tables: names
            .into_iter()
            .map(|name| crate::synth::rules::TableRule {
                name: name.clone(),
                rows: Some(rows),
                columns: HashMap::new(),
                derive: vec![],
                branches: vec![],
                relationships: vec![],
                strategy: crate::synth::rules::TableStrategy::Uniform,
            })
            .collect(),
    }
}

/// Parent key pools read from the live database (`select distinct`).
async fn load_real_key_pools(
    relations: &[crate::synth::quality::FkRelation],
    connection: &Option<String>,
    config_path: Option<String>,
) -> Result<crate::synth::quality::ParentKeyPools, String> {
    let mut wanted: Vec<(String, String)> = relations
        .iter()
        .map(|r| (r.parent_table.clone(), r.parent_column.clone()))
        .collect();
    wanted.sort();
    wanted.dedup();

    let mut pools = HashMap::new();
    if wanted.is_empty() {
        return Ok(pools);
    }

    let raw =
        crate::config::read_config(config_path.map(PathBuf::from)).map_err(|e| e.to_string())?;
    let side = resolve_connection(&raw, connection)?;
    let mut conn = connect(&side).await?;
    let schema = resolved_side_schema(
        None,
        &mut *conn,
        &side.connection_url,
        &side.name,
        side.default_schema.as_deref(),
    )
    .await?;

    for (table, column) in wanted {
        let sql = {
            let dialect = conn.dialect();
            // Ordered so the capped pool is reproducible instead of whatever
            // the engine returns first.
            let query = format!(
                "SELECT DISTINCT {} FROM {} ORDER BY {}",
                dialect.quote_ident(&column),
                dialect.quote_table(Some(&schema), &table),
                dialect.quote_ident(&column)
            );
            dialect.add_limit(&query, KEY_POOL_LIMIT)
        };
        let result = conn
            .query(&sql)
            .await
            .map_err(|e| format!("read key pool {}.{}: {}", table, column, e))?;
        let values: Vec<String> = result
            .rows
            .iter()
            .filter_map(|row| row.first())
            .filter(|value| !value.is_null())
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string())
            })
            .collect();
        if values.len() >= KEY_POOL_LIMIT {
            eprintln!(
                "warning: key pool {}.{} hit the {} key cap; join rates are computed against a truncated pool",
                table, column, KEY_POOL_LIMIT
            );
        }
        pools.insert((table, column), values);
    }
    Ok(pools)
}

/// Upper bound on keys pulled from the database for a join-rate check.
const KEY_POOL_LIMIT: usize = 100_000;

#[cfg(test)]
mod tests {
    #[test]
    fn schema_flag_beats_connection_schema_beats_driver_probe() {
        // 已知限制 #2（H）：`--schema` > 连接段 `schema` > 驱动探测。
        // Some(explicit) / Some(configured) 表示不用探测；None 才探测。
        assert_eq!(
            schema_priority(Some("cli"), Some("conn")),
            Some("cli".to_string())
        );
        assert_eq!(
            schema_priority(None, Some("conn")),
            Some("conn".to_string())
        );
        assert_eq!(schema_priority(None, None), None);
        // 空串视为未配置
        assert_eq!(
            schema_priority(Some(""), Some("conn")),
            Some("conn".to_string())
        );
        assert_eq!(schema_priority(None, Some("")), None);
    }

    #[test]
    fn against_db_name_warns_only_when_env_url_is_set() {
        // 已知限制 #11（E）：env 连接恒名 default，用户给的名字会被忽略。
        let warning = against_db_env_warning(Some("prod"), true).expect("warning expected");
        assert!(
            warning.contains("--against-db connection name 'prod' is ignored"),
            "{}",
            warning
        );
        assert!(warning.contains("always named"), "{}", warning);
        // 名字为空（裸 --against-db）或 env 未设置时不警告
        assert!(against_db_env_warning(None, true).is_none());
        assert!(against_db_env_warning(Some("prod"), false).is_none());
    }

    use super::*;

    fn profile_for(table: &str) -> crate::synth::profile::TableProfile {
        serde_json::from_value(serde_json::json!({
            "table": table,
            "row_count": 10,
            "columns": {}
        }))
        .expect("test profile")
    }

    #[test]
    fn should_warn_when_profiles_run_without_primary_keys() {
        let profiles = HashMap::from([("orders".to_string(), profile_for("orders"))]);
        let warning =
            implicit_fk_warning(&HashMap::new(), &profiles, Path::new("/nonexistent/.synth"))
                .expect("profiles without models must warn");
        assert!(
            warning.contains("may invent references"),
            "warning must name the consequence: {warning}"
        );
    }

    #[test]
    fn should_not_warn_when_primary_keys_or_profiles_are_present() {
        let profiles = HashMap::from([("orders".to_string(), profile_for("orders"))]);
        let keys = HashMap::from([("orders".to_string(), "order_id".to_string())]);
        assert!(implicit_fk_warning(&keys, &profiles, Path::new(".synth")).is_none());
        // No profiles at all: inference never runs, so there is nothing to warn about.
        assert!(
            implicit_fk_warning(&HashMap::new(), &HashMap::new(), Path::new(".synth")).is_none()
        );
    }
    #[test]
    fn should_error_when_a_models_directory_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(
            load_primary_keys(&missing).is_err(),
            "a missing models directory must not silently yield an empty key map"
        );
    }

    #[test]
    fn should_read_primary_keys_from_trained_models() {
        let dir = tempfile::tempdir().unwrap();
        let model = crate::synth::model::TableModel {
            version: 1,
            table: "orders".to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: crate::synth::model::Provenance {
                source: "test".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
                trained_rows: None,
            },
            pk: vec!["order_id".to_string()],
            columns: HashMap::new(),
            copula: crate::synth::model::CopulaInfo {
                column_order: vec![],
                correlation: vec![],
            },
            fk_cardinality: Default::default(),
        };
        std::fs::write(
            dir.path().join("orders.model.json"),
            serde_json::to_string(&model).unwrap(),
        )
        .unwrap();

        let keys = load_primary_keys(dir.path()).expect("trained model must load");
        assert_eq!(keys.get("orders").map(String::as_str), Some("order_id"));
    }

    #[test]
    fn should_resolve_schema_from_connection_default_when_unspecified() {
        assert_eq!(resolve_schema(None, "public".to_string()), "public");
        assert_eq!(resolve_schema(Some(""), "public".to_string()), "public");
        assert_eq!(resolve_schema(Some("other"), "public".to_string()), "other");
    }

    #[test]
    fn should_flag_oracle_sample_at_driver_cap() {
        assert!(sample_may_be_truncated("oracle", 100, 10_000));
        assert!(sample_may_be_truncated("Oracle", 100, 10_000));
    }

    #[test]
    fn should_not_flag_oracle_sample_below_cap() {
        assert!(!sample_may_be_truncated("oracle", 99, 10_000));
        assert!(!sample_may_be_truncated("oracle", 101, 10_000));
        assert!(!sample_may_be_truncated("oracle", 0, 10_000));
    }

    #[test]
    fn should_not_flag_non_oracle_sample_of_100() {
        assert!(!sample_may_be_truncated("mysql", 100, 10_000));
        assert!(!sample_may_be_truncated("gaussdb", 100, 10_000));
        assert!(!sample_may_be_truncated("duckdb", 100, 10_000));
    }

    // An explicit `--sample 100` (or less) means the cap is the user's own
    // request, not the driver silently cutting the result short: warning about
    // a "truncated" sample would be false.
    #[test]
    fn should_not_flag_oracle_when_the_cap_was_requested() {
        assert!(!sample_may_be_truncated("oracle", 100, 100));
        assert!(!sample_may_be_truncated("oracle", 100, 5));
        assert!(sample_may_be_truncated("oracle", 100, 101));
        assert!(sample_may_be_truncated("oracle", 100, 10_000));
    }

    #[test]
    fn split_tables_trims_and_skips_empty() {
        assert_eq!(
            split_tables(" a , b,,  c "),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        assert!(split_tables(" , ").is_empty());
    }

    #[test]
    fn schema_qualified_table_name_is_rejected_with_actionable_error() {
        let err = check_table_names(&["staging.customer".to_string()])
            .expect_err("dotted names must fail fast");
        assert!(err.contains("--schema"), "must point at --schema: {err}");
        assert!(
            err.contains("staging.customer"),
            "must echo the name: {err}"
        );
    }

    #[test]
    fn plain_table_names_pass_the_dotted_name_check() {
        assert!(check_table_names(&["customer".to_string(), "orders".to_string()]).is_ok());
        assert!(
            check_table_names(&[]).is_ok(),
            "emptiness is checked elsewhere"
        );
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
                categorical_top_k: cmd::CategoricalTopK::Limit(50),
                rules: None,
                holdout_ratio: 0.1,
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
                mine: cmd::MineArgs::default(),
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
                no_schema_qualifier: false,
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

    // ─── rules-draft cycle pre-check (limitation #3) ──────────────────────

    fn draft_rule(name: &str, relationships: Vec<(&str, &str)>) -> crate::synth::rules::TableRule {
        serde_json::from_value(serde_json::json!({
            "name": name,
            "relationships": relationships
                .iter()
                .map(|(pk, refs)| serde_json::json!({
                    "pk": pk,
                    "references": [refs],
                    "pool_strategy": { "projection": { "unique": false } }
                }))
                .collect::<Vec<_>>(),
            "strategy": "uniform"
        }))
        .expect("table rule fixture")
    }

    #[test]
    fn cycle_warning_names_the_cycle_path() {
        let rules = crate::synth::rules::SynthRules {
            version: "1".to_string(),
            tables: vec![
                draft_rule("customer", vec![("last_update", "rental.last_update")]),
                draft_rule("rental", vec![("last_update", "customer.last_update")]),
            ],
        };
        let warning = draft_cycle_warning(&rules).expect("mutual reference must warn");
        assert!(warning.contains("customer"), "{warning}");
        assert!(warning.contains("rental"), "{warning}");
        assert!(warning.contains("cycle"), "{warning}");
    }

    #[test]
    fn acyclic_draft_produces_no_cycle_warning() {
        let rules = crate::synth::rules::SynthRules {
            version: "1".to_string(),
            tables: vec![
                draft_rule("users", vec![]),
                draft_rule("orders", vec![("user_id", "users.id")]),
            ],
        };
        assert!(draft_cycle_warning(&rules).is_none());
    }

    #[test]
    fn self_reference_is_reported_as_cycle() {
        let rules = crate::synth::rules::SynthRules {
            version: "1".to_string(),
            tables: vec![draft_rule("employee", vec![("manager_id", "employee.id")])],
        };
        assert!(
            draft_cycle_warning(&rules).is_some(),
            "self-loop is a cycle"
        );
    }

    // ─── report CLI (#67) ────────────────────────────────────────────────

    fn report_fixture_model() -> crate::synth::model::TableModel {
        use crate::synth::marginal::{CategoricalParams, NormalParams};
        use crate::synth::model::{ColumnModel, CopulaInfo, LogicalType, Provenance, TableModel};
        let numeric = ColumnModel {
            logical_type: LogicalType::Numerical,
            rounding: None,
            datetime_epoch: None,
            decimal_scale: None,
            datetime_format: None,
            min: None,
            max: None,
            null_rate: None,
            marginal: crate::synth::marginal::Marginal::Normal(NormalParams {
                loc: 50.0,
                scale: 10.0,
            }),
            pii: None,
        };
        let categorical = ColumnModel {
            logical_type: LogicalType::Categorical,
            rounding: None,
            datetime_epoch: None,
            decimal_scale: None,
            datetime_format: None,
            min: None,
            max: None,
            null_rate: None,
            marginal: crate::synth::marginal::Marginal::Categorical(CategoricalParams {
                values: vec!["a".to_string(), "b".to_string()],
                weights: vec![0.5, 0.5],
            }),
            pii: None,
        };
        TableModel {
            version: 1,
            table: "t".to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: Provenance {
                source: "native".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
                trained_rows: None,
            },
            pk: vec![],
            columns: HashMap::from([
                ("amount".to_string(), numeric),
                ("kind".to_string(), categorical),
            ]),
            copula: CopulaInfo {
                column_order: vec!["amount".to_string(), "kind".to_string()],
                correlation: vec![],
            },
            fk_cardinality: Default::default(),
        }
    }

    /// Build a models dir with a model plus a baseline built from `rows`.
    fn report_fixture_dir(
        name: &str,
        rows: &[Vec<serde_json::Value>],
        with_baseline: bool,
    ) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("synth-report-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let model = report_fixture_model();
        model.save(&dir.join("t.model.json")).unwrap();
        if with_baseline {
            let baseline = crate::synth::quality::build_baseline(
                "t",
                &model,
                rows,
                &[0, 1],
                crate::synth::quality::DEFAULT_HOLDOUT_RATIO,
            )
            .unwrap();
            baseline.save(&dir.join("t.report-baseline.json")).unwrap();
        }
        dir
    }

    fn report_rows(
        sample: usize,
        kind_of: impl Fn(usize) -> &'static str,
    ) -> Vec<Vec<serde_json::Value>> {
        (0..sample)
            .map(|i| {
                vec![
                    serde_json::Value::from(i as f64),
                    serde_json::Value::from(kind_of(i)),
                ]
            })
            .collect()
    }

    fn write_generated_jsonl(dir: &Path, rows: &[Vec<serde_json::Value>]) {
        let body: String = rows
            .iter()
            .map(|row| serde_json::json!({"amount": row[0], "kind": row[1]}).to_string())
            .map(|line| line + "\n")
            .collect();
        std::fs::write(dir.join("t.jsonl"), body).unwrap();
    }

    fn report_options(
        models_dir: &Path,
        data_dir: &Path,
        output: Option<PathBuf>,
    ) -> ReportRunOptions {
        ReportRunOptions {
            models: models_dir.to_string_lossy().to_string(),
            data: Some(data_dir.to_string_lossy().to_string()),
            rules: None,
            against_db: None,
            rows: 100,
            seed: Some(1),
            output: output.map(|p| p.to_string_lossy().to_string()),
            min_score: None,
            strict: false,
        }
    }

    #[tokio::test]
    async fn report_scores_generated_data_against_the_baseline() {
        // Period-3 values: every level must reach the holdout sample.
        let training = report_rows(1000, |i| ["a", "b", "c"][i % 3]);
        let models_dir = report_fixture_dir("scored", &training, true);

        let data_dir = std::env::temp_dir().join("synth-report-scored-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        write_generated_jsonl(&data_dir, &report_rows(1000, |i| ["a", "b", "c"][i % 3]));
        let output = std::env::temp_dir().join("synth-report-scored.json");

        run_report(
            report_options(&models_dir, &data_dir, Some(output.clone())),
            None,
        )
        .await
        .unwrap();

        let report: crate::synth::quality::QualityReport =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        // Same inputs, same report bytes.
        let second = std::env::temp_dir().join("synth-report-scored-2.json");
        run_report(
            report_options(&models_dir, &data_dir, Some(second.clone())),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            std::fs::read_to_string(&second).unwrap()
        );
        std::fs::remove_file(&second).ok();

        assert_eq!(report.tables.len(), 1);
        let crate::synth::quality::Section::Scored { items, .. } = &report.tables[0].shapes else {
            panic!("expected scored shapes");
        };
        assert_eq!(items.len(), 2);
        for item in items {
            // The categorical score carries the TV noise floor of a ~100-row
            // holdout against 1000 generated rows (~0.1), so it cannot reach
            // the numeric column's 0.95.
            let floor = if item.metric == "1-ks" { 0.9 } else { 0.85 };
            assert!(item.score > floor, "{item:?}");
        }
        // No rules: the fk section is honestly skipped.
        assert!(matches!(
            report.tables[0].fk,
            crate::synth::quality::Section::Skipped { .. }
        ));

        std::fs::remove_dir_all(&models_dir).ok();
        std::fs::remove_dir_all(&data_dir).ok();
        std::fs::remove_file(&output).ok();
    }

    #[tokio::test]
    async fn report_skips_shapes_without_a_baseline() {
        let training = report_rows(500, |i| ["a", "b", "c"][i % 3]);
        let models_dir = report_fixture_dir("nobaseline", &training, false);
        let data_dir = std::env::temp_dir().join("synth-report-nobaseline-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        write_generated_jsonl(&data_dir, &report_rows(100, |i| ["a", "b", "c"][i % 3]));

        // Exit code stays 0: a missing baseline is a skip, not an error.
        run_report(report_options(&models_dir, &data_dir, None), None)
            .await
            .unwrap();

        // ... unless --strict is requested.
        let mut options = report_options(&models_dir, &data_dir, None);
        options.strict = true;
        let err = run_report(options, None).await.unwrap_err();
        assert!(err.contains("--strict"), "{err}");

        std::fs::remove_dir_all(&models_dir).ok();
        std::fs::remove_dir_all(&data_dir).ok();
    }

    #[tokio::test]
    async fn report_min_score_rejects_degraded_data() {
        let training = report_rows(2000, |i| ["a", "b", "c"][i % 3]);
        let models_dir = report_fixture_dir("degraded", &training, true);
        let data_dir = std::env::temp_dir().join("synth-report-degraded-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        let degraded: Vec<Vec<serde_json::Value>> = report_rows(2000, |i| ["a", "b", "c"][i % 3])
            .into_iter()
            .map(|mut row| {
                row[0] = serde_json::Value::from(row[0].as_f64().unwrap() + 10_000.0);
                row
            })
            .collect();
        write_generated_jsonl(&data_dir, &degraded);

        let mut options = report_options(&models_dir, &data_dir, None);
        options.min_score = Some(0.9);
        let err = run_report(options, None).await.unwrap_err();
        assert!(err.contains("below --min-score"), "{err}");

        // Without the gate the same data reports the low score instead.
        run_report(report_options(&models_dir, &data_dir, None), None)
            .await
            .unwrap();

        std::fs::remove_dir_all(&models_dir).ok();
        std::fs::remove_dir_all(&data_dir).ok();
    }

    #[test]
    fn train_writes_a_holdout_baseline_next_to_the_model() {
        let rows: Vec<Vec<serde_json::Value>> = (0..400)
            .map(|i| {
                vec![
                    serde_json::Value::from(i as f64),
                    serde_json::Value::from(["a", "b", "c"][i % 3]),
                ]
            })
            .collect();
        let names = ["amount", "kind"];
        let types = HashMap::from([
            ("amount".to_string(), "double".to_string()),
            ("kind".to_string(), "varchar".to_string()),
        ]);
        let profile = crate::synth::profile::TableProfile::from_rows_typed(
            "t",
            &names.map(str::to_string),
            &rows,
            Some(&types),
            Some(crate::synth::profile::TOP_VALUES_CAP),
        );
        let (model, skipped) =
            cmd::build_model_with_overrides("t", "mysql", &profile, &rows, vec![], None, None)
                .unwrap();
        assert!(skipped.is_empty());

        let dir = std::env::temp_dir().join("synth-train-baseline");
        std::fs::create_dir_all(&dir).unwrap();
        write_report_baseline("t", &model, &profile, &rows, &dir, 0.2).unwrap();
        let path = dir.join("t.report-baseline.json");
        let baseline = crate::synth::quality::BaselineSummary::load(&path).unwrap();
        // Hash selection is binomial around 20% of 400 rows.
        assert!(
            (50..=110).contains(&baseline.holdout_rows),
            "{}",
            baseline.holdout_rows
        );
        assert!(baseline.columns.contains_key("amount"));
        assert!(baseline.columns.contains_key("kind"));

        // Ratio 0 disables the baseline instead of writing an empty one.
        std::fs::remove_file(&path).unwrap();
        write_report_baseline("t", &model, &profile, &rows, &dir, 0.0).unwrap();
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    fn categorical_model(
        table: &str,
        columns: &[(&str, &[&str])],
    ) -> crate::synth::model::TableModel {
        use crate::synth::marginal::{CategoricalParams, Marginal};
        use crate::synth::model::{ColumnModel, CopulaInfo, LogicalType, Provenance, TableModel};
        let map: HashMap<String, ColumnModel> = columns
            .iter()
            .map(|(name, values)| {
                (
                    name.to_string(),
                    ColumnModel {
                        logical_type: LogicalType::Categorical,
                        rounding: None,
                        datetime_epoch: None,
                        decimal_scale: None,
                        datetime_format: None,
                        min: None,
                        max: None,
                        null_rate: None,
                        marginal: Marginal::Categorical(CategoricalParams {
                            values: values.iter().map(|v| v.to_string()).collect(),
                            weights: vec![1.0 / values.len() as f64; values.len()],
                        }),
                        pii: None,
                    },
                )
            })
            .collect();
        TableModel {
            version: 1,
            table: table.to_string(),
            dialect: "mysql".to_string(),
            schema: None,
            provenance: Provenance {
                source: "native".to_string(),
                converter_version: None,
                sdv_version: None,
                truncated: false,
                trained_rows: None,
            },
            pk: vec![],
            columns: map,
            copula: CopulaInfo {
                column_order: columns.iter().map(|(n, _)| n.to_string()).collect(),
                correlation: vec![],
            },
            fk_cardinality: Default::default(),
        }
    }

    #[tokio::test]
    async fn report_scores_foreign_keys_from_the_rules_file() {
        let root = std::env::temp_dir().join("synth-report-fk");
        let models_dir = root.join("models");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&models_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        categorical_model("users", &[("id", &["1", "2", "3"])])
            .save(&models_dir.join("users.model.json"))
            .unwrap();
        categorical_model("orders", &[("user_id", &["1", "2", "9"])])
            .save(&models_dir.join("orders.model.json"))
            .unwrap();
        std::fs::write(
            data_dir.join("users.jsonl"),
            "{\"id\":\"1\"}\n{\"id\":\"2\"}\n{\"id\":\"3\"}\n",
        )
        .unwrap();
        std::fs::write(
            data_dir.join("orders.jsonl"),
            "{\"user_id\":\"1\"}\n{\"user_id\":\"2\"}\n{\"user_id\":\"9\"}\n",
        )
        .unwrap();
        let rules_path = root.join("rules.yaml");
        std::fs::write(
            &rules_path,
            "version: \"1\"\ntables:\n  - name: users\n    relationships: []\n  - name: orders\n    relationships:\n      - pk: user_id\n        references: [users.id]\n",
        )
        .unwrap();
        let output = root.join("report.json");

        let mut options = report_options(&models_dir, &data_dir, Some(output.clone()));
        options.rules = Some(rules_path.to_string_lossy().to_string());
        run_report(options, None).await.unwrap();

        let report: crate::synth::quality::QualityReport =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        let orders = report
            .tables
            .iter()
            .find(|t| t.table == "orders")
            .expect("orders table");
        let crate::synth::quality::Section::Scored { items, .. } = &orders.fk else {
            panic!(
                "expected a scored fk section on the child table, got {:?}",
                orders.fk
            );
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].parent_table, "users");
        assert_eq!(items[0].hits, 2);
        assert_eq!(items[0].keys, 3);
        assert!(items[0].warn, "2/3 is below the 0.99 threshold");
        assert_eq!(items[0].source, "generated");
        // The parent pool is what the generated parent actually produced
        // (1,2,3 in the fixture), not a model value dictionary.
        assert_eq!(items[0].hits, 2);

        // The parent table owns no FK edge and is skipped rather than shown
        // with the child's numbers.
        assert_eq!(orders.rows, 3, "the report counts generated rows");
        let users = report
            .tables
            .iter()
            .find(|t| t.table == "users")
            .expect("users table");
        assert!(matches!(
            users.fk,
            crate::synth::quality::Section::Skipped { .. }
        ));

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn report_lists_tables_whose_generated_data_is_missing() {
        // A model with no data file must appear in the JSON as skipped, so a
        // --min-score gate cannot pass by averaging away an unscored table.
        let root = std::env::temp_dir().join("synth-report-missing-data");
        let models_dir = root.join("models");
        let data_dir = root.join("data");
        std::fs::create_dir_all(&models_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        let main_model = report_fixture_model();
        main_model.save(&models_dir.join("t.model.json")).unwrap();
        let other_model = categorical_model("other", &[("id", &["1", "2"])]);
        other_model
            .save(&models_dir.join("other.model.json"))
            .unwrap();
        // Both tables have a baseline, so --strict fails on the missing data
        // rather than on the baseline check.
        let training = report_rows(200, |i| ["a", "b", "c"][i % 3]);
        crate::synth::quality::build_baseline(
            "t",
            &main_model,
            &training,
            &[0, 1],
            crate::synth::quality::DEFAULT_HOLDOUT_RATIO,
        )
        .unwrap()
        .save(&models_dir.join("t.report-baseline.json"))
        .unwrap();
        crate::synth::quality::build_baseline(
            "other",
            &other_model,
            &[(0..100)
                .map(|i| vec![serde_json::Value::from(format!("{}", i % 2 + 1))])
                .collect::<Vec<_>>()][0],
            &[0],
            crate::synth::quality::DEFAULT_HOLDOUT_RATIO,
        )
        .unwrap()
        .save(&models_dir.join("other.report-baseline.json"))
        .unwrap();
        write_generated_jsonl(&data_dir, &report_rows(100, |i| ["a", "b", "c"][i % 3]));
        let output = root.join("report.json");

        run_report(
            report_options(&models_dir, &data_dir, Some(output.clone())),
            None,
        )
        .await
        .unwrap();

        let report: crate::synth::quality::QualityReport =
            serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
        let other = report
            .tables
            .iter()
            .find(|table| table.table == "other")
            .expect("the data-less table must still be listed");
        assert_eq!(other.rows, 0);
        let crate::synth::quality::Section::Skipped { reason } = &other.shapes else {
            panic!("expected skipped shapes, got {:?}", other.shapes);
        };
        assert!(reason.contains("no generated data"), "{reason}");
        assert!(matches!(
            other.fk,
            crate::synth::quality::Section::Skipped { .. }
        ));
        assert!(other.score.is_none(), "a table with no data has no score");

        // --strict turns the omission into an error.
        let mut options = report_options(&models_dir, &data_dir, None);
        options.strict = true;
        let err = run_report(options, None).await.unwrap_err();
        assert!(err.contains("no generated data"), "{err}");

        // --min-score is a gate over every model: the scored table must not
        // let the data-less one pass unnoticed.
        let mut options = report_options(&models_dir, &data_dir, None);
        options.min_score = Some(0.5);
        let err = run_report(options, None).await.unwrap_err();
        assert!(err.contains("requires every table to be scored"), "{err}");
        assert!(
            err.contains("other"),
            "the error must name the table: {err}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn report_min_score_fails_when_nothing_is_scored() {
        // Baseline-less model: shapes and pairs are skipped, so the gate has
        // nothing to average and must fail rather than pass on an empty set.
        let training = report_rows(500, |i| ["a", "b", "c"][i % 3]);
        let models_dir = report_fixture_dir("unscored", &training, false);
        let data_dir = std::env::temp_dir().join("synth-report-unscored-data");
        std::fs::create_dir_all(&data_dir).unwrap();
        write_generated_jsonl(&data_dir, &report_rows(100, |i| ["a", "b", "c"][i % 3]));

        let mut options = report_options(&models_dir, &data_dir, None);
        options.min_score = Some(0.5);
        let err = run_report(options, None).await.unwrap_err();
        assert!(err.contains("requires every table to be scored"), "{err}");

        std::fs::remove_dir_all(&models_dir).ok();
        std::fs::remove_dir_all(&data_dir).ok();
    }
    #[test]
    fn should_apply_pii_sdtype_overrides() {
        use crate::synth::pii::PiiProvider;
        use crate::synth::rules::SdType;

        let columns = vec!["email".to_string(), "status".to_string()];
        let rows = vec![
            vec![
                serde_json::Value::from("a@b.com"),
                serde_json::Value::from("open"),
            ],
            vec![
                serde_json::Value::from("c@d.org"),
                serde_json::Value::from("closed"),
            ],
        ];

        // auto: the recognizer flags email only.
        let types = HashMap::new();
        let auto = detect_pii_columns(&columns, &rows, &types, None);
        assert_eq!(auto.get("email"), Some(&PiiProvider::Email));
        assert!(!auto.contains_key("status"));

        // keep: email is exempted.
        let keep = HashMap::from([("email".to_string(), (SdType::Keep, None))]);
        assert!(detect_pii_columns(&columns, &rows, &types, Some(&keep)).is_empty());

        // pii: forces the provider on a column the recognizer would ignore.
        let forced = HashMap::from([(
            "status".to_string(),
            (SdType::Pii, Some("name".to_string())),
        )]);
        let out = detect_pii_columns(&columns, &rows, &types, Some(&forced));
        assert_eq!(out.get("status"), Some(&PiiProvider::Name));
        assert_eq!(out.get("email"), Some(&PiiProvider::Email));
    }
    #[test]
    fn should_reserve_only_foreign_keys_from_pii_anonymization() {
        let foreign_keys = vec![crate::synth::rules_draft::ForeignKeyInfo {
            from_table: "orders".to_string(),
            from_column: "user_id".to_string(),
            to_table: "users".to_string(),
            to_column: "id".to_string(),
        }];
        let reserved = pii_reserved_columns(&foreign_keys, "orders");
        assert!(reserved.contains("user_id"));
        assert_eq!(reserved.len(), 1, "primary keys must not be reserved");
        assert!(pii_reserved_columns(&foreign_keys, "other").is_empty());
    }
}
