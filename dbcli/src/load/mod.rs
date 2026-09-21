//! `hepta_dbcli load` (issue #98): load generated data files back into an
//! existing database in FK-safe topological order.

pub(crate) mod cmd;
pub(crate) mod loader;
pub(crate) mod plan;

// ─── Write gate (issue #58 pattern, load flavor) ────────────────────────

use crate::audit::event::{
    ActionClass, AuditOutcome, Channel, ConnectionInfo, Decision, DraftEvent,
};
use std::collections::HashMap;
use std::path::PathBuf;

/// Exit code for a refused invocation (usage problem, nothing ran).
const EXIT_REFUSED: i32 = 2;
/// Exit code for a failed invocation.
const EXIT_ERROR: i32 = 1;

#[derive(Debug, PartialEq)]
pub(crate) enum GateRefusal {
    /// Data changes need `--allow-write` (issue #58 D4).
    WriteFlagRequired,
}

/// Pure load write gate: the command is a data-change channel, so it is
/// refused without `--allow-write` (audit deny `write_flag_required`), and
/// allowed otherwise. Destructive DDL cannot occur here (load emits INSERTs
/// only, never DDL), so there is no Destructive arm.
pub(crate) fn gate_decision(allow_write: bool) -> Result<(), GateRefusal> {
    if !allow_write {
        return Err(GateRefusal::WriteFlagRequired);
    }
    Ok(())
}

/// Placeholder connection for events that fire before a connection exists
/// (the gate runs first; see `synth_connection` for the same pattern).
fn gate_connection() -> ConnectionInfo {
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

fn gate_deny_event() -> DraftEvent {
    DraftEvent::new(
        Channel::Load,
        gate_connection(),
        "load",
        ActionClass::Meta,
        Decision::Deny,
    )
    .with_deny_reason("write_flag_required")
    .with_detail(serde_json::json!({ "subcommand": "load" }))
}

// ─── Entry Point ────────────────────────────────────────────────────────

pub(crate) async fn run(
    args: cmd::LoadArgs,
    config_path: Option<String>,
    connection_name: Option<String>,
    allow_write: bool,
    audit: &crate::audit::AuditSession,
) -> i32 {
    // (a) Gate before touching config or the engine. A refusal is still an
    // auditable fact (issue #58 acceptance 4).
    if let Err(refusal) = gate_decision(allow_write) {
        debug_assert_eq!(refusal, GateRefusal::WriteFlagRequired);
        audit.record_best_effort(gate_deny_event());
        eprintln!(
            "error: load writes data and needs --allow-write (CLI/REPL data changes require it; destructive DDL is refused regardless)"
        );
        return EXIT_REFUSED;
    }

    // (c) Resolve the named connection (load has no inline-URL mode).
    // `--name` is the only connection selector: an unknown name must fail
    // loudly instead of silently falling back to the default connection.
    let raw = match crate::config::read_config(config_path.map(PathBuf::from)) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_ERROR;
        }
    };
    let target = match resolve_connection(&raw, &connection_name) {
        Ok(resolved) => resolved,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_ERROR;
        }
    };

    let conn_info = ConnectionInfo::from_url(&target.name, &target.connection_url, false);

    // Connect with the write-capable session flag (GaussDB honours it).
    let scheme = target
        .connection_url
        .find("://")
        .map(|i| &target.connection_url[..i])
        .unwrap_or("mysql")
        .to_string();
    let registry = crate::create_registry();
    let pool = match registry
        .connect_with_fallback(
            &scheme,
            &target.connection_url,
            Some(&target.timeout_config),
            allow_write,
        )
        .await
    {
        Ok(pool) => pool,
        Err(e) => {
            eprintln!("error: connect '{}': {}", target.name, e);
            return EXIT_ERROR;
        }
    };
    let mut conn = match pool.acquire().await {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("error: acquire '{}': {}", target.name, e);
            return EXIT_ERROR;
        }
    };

    // Discover data files for the requested (or all) tables.
    let requested = args.tables.as_deref();
    let data_dir = PathBuf::from(&args.data);
    let files = match plan::discover_table_files(&data_dir, requested, args.format_str()) {
        Ok(files) => files,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_ERROR;
        }
    };
    if files.is_empty() {
        eprintln!(
            "error: no data files found in {} (expected <table>.jsonl|json|csv)",
            data_dir.display()
        );
        return EXIT_ERROR;
    }

    // Which tables exist? Issue #106: the schema in effect (--schema, else
    // the connection default) takes part in the match so tables from other
    // schemas do not satisfy it, and a schema with no tables fails with a
    // schema-level error. Without either, matching stays by bare table name.
    let list_sql = conn.dialect().list_tables().to_string();
    let listed = match conn.query(&list_sql).await {
        Ok(result) => result,
        Err(e) => {
            eprintln!("error: list tables: {e}");
            return EXIT_ERROR;
        }
    };
    let effective_schema = args
        .schema
        .clone()
        .or_else(|| target.default_schema.clone());
    let (matched, skipped) = match plan::match_files_to_db_tables_in_schema(
        &files,
        &listed,
        effective_schema.as_deref(),
    ) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("error: {e}");
            return EXIT_ERROR;
        }
    };

    // FK edges drive the topological load order. The FK query needs a schema;
    // explicit --schema wins, then the connection's configured default, then
    // the schema the planned tables were listed under (never an unrelated
    // schema that happens to host a same-named table).
    let listed_schemas = plan::parse_table_schemas(&listed);
    let planned_tables: Vec<String> = matched.iter().map(|f| f.table.clone()).collect();
    let fk_schema = plan::schema_for_fk_lookup(
        args.schema.clone(),
        target.default_schema.clone(),
        &listed_schemas,
        &planned_tables,
    )
    .unwrap_or_default();
    let fk_result = if fk_schema.is_empty() {
        crate::backend::QueryResult {
            columns: Vec::new(),
            row_count: 0,
            rows: Vec::new(),
            rows_affected: None,
        }
    } else {
        let sql = conn.dialect().foreign_keys_sql(&fk_schema);
        match conn.query(&sql).await {
            Ok(result) => result,
            Err(e) => {
                eprintln!("error: foreign keys: {e}");
                return EXIT_ERROR;
            }
        }
    };
    let plan_data =
        match plan::build_plan(matched, args.schema.clone(), &fk_result, &listed_schemas) {
            Ok(plan) => plan,
            Err(e) => {
                eprintln!("error: {e}");
                return EXIT_ERROR;
            }
        };

    // Per-table schema (for column lookup): explicit --schema wins, else the
    // schema each table was listed under.
    let schema_of = |table: &str| -> Option<String> {
        args.schema
            .clone()
            .or_else(|| listed_schemas.get(table).cloned().flatten())
    };

    // Column metadata for strict validation (order-insensitive set equality).
    let mut db_columns: HashMap<String, Vec<String>> = HashMap::new();
    for entry in &plan_data.entries {
        let Some(schema) = schema_of(&entry.table) else {
            eprintln!(
                "error: table '{}': no schema known (--schema or a default schema is required for column validation)",
                entry.table
            );
            return EXIT_ERROR;
        };
        let sql = conn.dialect().table_columns().to_string();
        let result = match conn
            .exec(
                &sql,
                &[
                    serde_json::Value::from(schema),
                    serde_json::Value::from(entry.table.clone()),
                ],
            )
            .await
        {
            Ok(result) => result,
            Err(e) => {
                eprintln!("error: columns of {}: {e}", entry.table);
                return EXIT_ERROR;
            }
        };
        db_columns.insert(entry.table.clone(), plan::parse_column_names(&result));
    }
    if let Err(e) = plan::validate_column_sets(&plan_data, &db_columns) {
        eprintln!("error: {e}");
        return EXIT_ERROR;
    }

    if args.dry_run {
        let text = plan::render_plan(&plan_data, &target.name, &scheme, &fk_schema, &skipped);
        println!("{text}");
        let tables = plan_data.entries.len();
        let rows: usize = plan_data.entries.iter().map(|e| e.row_count).sum();
        audit.record_best_effort(dry_run_event(&conn_info, tables, rows));
        return 0;
    }

    // Fail-closed intent: on disk before anything executes (issue #58).
    let table_names: Vec<String> = plan_data.entries.iter().map(|e| e.table.clone()).collect();
    let planned_rows: usize = plan_data.entries.iter().map(|e| e.row_count).sum();
    let intent = load_intent_event(&conn_info, &table_names, planned_rows);
    if let Err(e) = audit.record(intent) {
        eprintln!("error: refusing to load data without an audit record: {e}");
        return EXIT_ERROR;
    }
    let started = std::time::Instant::now();

    match loader::execute(&mut *conn, &plan_data, audit, &conn_info).await {
        Ok(inserted) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            audit.record_best_effort(load_outcome_event(
                &conn_info,
                AuditOutcome::ok(duration_ms),
                inserted,
            ));
            println!(
                "loaded {inserted} rows into {} table(s) via connection '{}'",
                table_names.len(),
                conn_info.name
            );
            // Per-table receipt in load order: counts only, never row data.
            for entry in &plan_data.entries {
                println!(
                    "  {} ({} rows from {})",
                    entry.table,
                    entry.row_count,
                    entry.path.display()
                );
            }
            0
        }
        Err(e) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            audit.record_best_effort(load_error_event(
                &conn_info,
                AuditOutcome::error(duration_ms, "LoadFailed"),
                &e,
            ));
            eprintln!("error: {e}");
            EXIT_ERROR
        }
    }
}

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
            format!("connection '{target_name}' not found\n  available: {available:?}")
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

// ─── Audit events ───────────────────────────────────────────────────────

fn dry_run_event(conn: &ConnectionInfo, tables: usize, rows: usize) -> DraftEvent {
    DraftEvent::new(
        Channel::Load,
        conn.clone(),
        "load",
        ActionClass::Meta,
        Decision::Allow,
    )
    .with_detail(serde_json::json!({
        "subcommand": "load",
        "dry_run": true,
        "tables": tables,
        "rows": rows,
    }))
}

fn load_intent_event(conn: &ConnectionInfo, tables: &[String], planned_rows: usize) -> DraftEvent {
    DraftEvent::new(
        Channel::Load,
        conn.clone(),
        "load",
        ActionClass::Dml,
        Decision::Allow,
    )
    .with_detail(serde_json::json!({
        "subcommand": "load",
        "tables": tables,
        "planned_rows": planned_rows,
    }))
}

fn load_outcome_event(
    conn: &ConnectionInfo,
    outcome: AuditOutcome,
    rows_inserted: u64,
) -> DraftEvent {
    DraftEvent::new(
        Channel::Load,
        conn.clone(),
        "load",
        ActionClass::Dml,
        Decision::Allow,
    )
    .with_outcome(outcome)
    .with_detail(serde_json::json!({
        "subcommand": "load",
        "rows_inserted": rows_inserted,
    }))
}

fn load_error_event(conn: &ConnectionInfo, outcome: AuditOutcome, message: &str) -> DraftEvent {
    DraftEvent::new(
        Channel::Load,
        conn.clone(),
        "load",
        ActionClass::Dml,
        Decision::Error,
    )
    .with_outcome(outcome)
    .with_detail(serde_json::json!({
        "subcommand": "load",
        "error": message,
    }))
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_refuse_load_without_allow_write() {
        assert_eq!(gate_decision(false), Err(GateRefusal::WriteFlagRequired));
    }

    #[test]
    fn should_allow_load_with_allow_write() {
        assert_eq!(gate_decision(true), Ok(()));
    }

    #[test]
    fn gate_deny_event_carries_reason_and_subcommand() {
        let event = gate_deny_event();
        assert_eq!(event.channel, Channel::Load);
        assert_eq!(event.class, ActionClass::Meta);
        assert_eq!(event.decision, Decision::Deny);
        assert_eq!(event.deny_reason.as_deref(), Some("write_flag_required"));
        let detail = event.detail.expect("detail");
        assert_eq!(detail["subcommand"], "load");
    }

    #[test]
    fn dry_run_event_is_meta_allow_with_counts() {
        let conn = gate_connection();
        let event = dry_run_event(&conn, 3, 17);
        assert_eq!(event.decision, Decision::Allow);
        assert_eq!(event.class, ActionClass::Meta);
        let detail = event.detail.expect("detail");
        assert_eq!(detail["subcommand"], "load");
        assert_eq!(detail["dry_run"], true);
        assert_eq!(detail["tables"], 3);
        assert_eq!(detail["rows"], 17);
    }

    #[test]
    fn intent_event_is_dml_allow_with_tables_and_rows() {
        let conn = gate_connection();
        let event = load_intent_event(&conn, &["users".to_string(), "orders".to_string()], 8);
        assert_eq!(event.decision, Decision::Allow);
        assert_eq!(event.class, ActionClass::Dml);
        let detail = event.detail.expect("detail");
        assert_eq!(detail["subcommand"], "load");
        assert_eq!(detail["tables"][0], "users");
        assert_eq!(detail["planned_rows"], 8);
    }
}
