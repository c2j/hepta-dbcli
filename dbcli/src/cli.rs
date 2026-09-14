use std::path::PathBuf;
use std::str::FromStr;
use std::time::Instant;

use serde_json::Value;
use tracing::{info, warn};

use crate::audit::event::{
    read_only_session_for, ActionClass, AuditOutcome, Channel, ConnectionInfo, Decision,
    DraftEvent, SqlInfo,
};
use crate::audit::AuditSession;
use crate::backend::factory::BackendRegistry;
use crate::backend::DbConn;
pub(crate) use crate::backend::QueryResult;
use crate::config::{
    read_config, resolve_env_var_connection, resolve_single_connection,
    rewrite_password_to_sentinel, store_keyring_password, TimeoutConfig,
};
use crate::output;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputFormat {
    Table,
    Json,
    Vertical,
    Csv,
}

impl FromStr for OutputFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "table" => Ok(OutputFormat::Table),
            "json" => Ok(OutputFormat::Json),
            "vertical" => Ok(OutputFormat::Vertical),
            "csv" => Ok(OutputFormat::Csv),
            _ => Err(format!(
                "Unknown output format '{}'. Use table, json, vertical, or csv.",
                s
            )),
        }
    }
}

pub(crate) struct CliArgs {
    pub sql: Option<String>,
    pub file: Option<String>,
    pub connection_name: Option<String>,
    pub config_path: Option<String>,
    pub format: OutputFormat,
    pub statement_timeout: Option<String>,
    pub connection_max_lifetime: Option<String>,
    pub no_history: bool,
    pub timeout_action: Option<String>,
    /// `--allow-write`: permit L2 data changes (issue #58).
    pub allow_write: bool,
}

fn value_to_compact_string(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

fn strip_leading_comments(sql: &str) -> &str {
    let trimmed = sql.trim_start();
    let bytes = trimmed.as_bytes();
    if bytes.len() >= 2 && &bytes[..2] == b"--" {
        match trimmed.find('\n') {
            Some(pos) => strip_leading_comments(&trimmed[pos + 1..]),
            None => "",
        }
    } else if bytes.len() >= 2 && &bytes[..2] == b"/*" {
        let mut depth: usize = 1;
        let mut i = 2;
        while i + 1 < bytes.len() && depth > 0 {
            if &bytes[i..i + 2] == b"/*" {
                depth += 1;
                i += 2;
            } else if &bytes[i..i + 2] == b"*/" {
                depth -= 1;
                i += 2;
            } else {
                i += 1;
            }
        }
        if depth == 0 {
            strip_leading_comments(&trimmed[i..])
        } else {
            ""
        }
    } else {
        trimmed
    }
}

pub(crate) fn is_read_only_query(sql: &str) -> bool {
    let trimmed = sql.trim();
    let stripped = strip_leading_comments(trimmed);
    let upper = stripped.to_uppercase();
    upper.starts_with("SELECT")
        || upper.starts_with("EXPLAIN")
        || upper.starts_with("SHOW")
        || upper.starts_with("DESC")
        || upper.starts_with("DESCRIBE")
        || upper.starts_with("WITH")
}

pub(crate) fn is_read_only_mcp(sql: &str, prefixes: &[&str]) -> bool {
    let trimmed = sql.trim();
    let stripped = strip_leading_comments(trimmed);
    let upper = stripped.to_uppercase();
    prefixes.iter().any(|p| upper.starts_with(p))
}

// ─── Statement classification for the CLI write gate (#58) ──────────

/// How a statement may be executed from the CLI/REPL.
///
/// This is a UX gate, not a security boundary — the real boundary is the
/// database account (issue #58 D8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatementClass {
    /// `SELECT` / `EXPLAIN` / `SHOW` / `DESCRIBE` / a plain CTE.
    ReadOnly,
    /// `INSERT` / `UPDATE` / `DELETE` / ... — needs `--allow-write`.
    DataChange,
    /// `CALL` / `EXEC` / `DO` / an anonymous block — needs `--allow-write`.
    Call,
    /// `DROP` / `TRUNCATE` / `ALTER` / `CREATE` / `GRANT` / ... — always refused.
    Destructive,
    /// Transaction control and anything unrecognised: unchanged behaviour.
    Other,
}

/// What the gate decided for one statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteGate {
    /// Execute as-is.
    Allow,
    /// Refused: a data change without `--allow-write`.
    NeedsFlag(StatementClass),
    /// Refused: destructive DDL is out of scope even with `--allow-write`.
    Destructive,
}

const DESTRUCTIVE_KEYWORDS: &[&str] = &[
    "DROP", "TRUNCATE", "ALTER", "CREATE", "GRANT", "REVOKE", "RENAME",
];
const DATA_CHANGE_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "REPLACE", "MERGE", "UPSERT", "LOAD", "IMPORT",
];
const CALL_KEYWORDS: &[&str] = &["CALL", "EXEC", "EXECUTE", "DO", "DECLARE", "PERFORM"];
const READ_ONLY_KEYWORDS: &[&str] = &[
    "SELECT",
    "EXPLAIN",
    "SHOW",
    "DESC",
    "DESCRIBE",
    "TABLE",
    "VALUES",
    "SUMMARIZE",
];

/// Split into SQL-ish words so a keyword inside a literal does not match by
/// accident (still a heuristic; classification is UX, not security).
fn is_keyword(upper: &str, keyword: &str) -> bool {
    upper
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| word == keyword)
}

pub(crate) fn classify_statement(sql: &str) -> StatementClass {
    let stripped = strip_leading_comments(sql.trim());
    let upper = stripped.to_uppercase();
    let first = upper
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .to_string();

    if DESTRUCTIVE_KEYWORDS.contains(&first.as_str()) {
        return StatementClass::Destructive;
    }
    if DATA_CHANGE_KEYWORDS.contains(&first.as_str()) {
        return StatementClass::DataChange;
    }
    if CALL_KEYWORDS.contains(&first.as_str()) {
        return StatementClass::Call;
    }
    if first == "WITH" {
        // A CTE can carry a data change (`WITH x AS (...) INSERT ...`).
        if DESTRUCTIVE_KEYWORDS.iter().any(|k| is_keyword(&upper, k)) {
            return StatementClass::Destructive;
        }
        if DATA_CHANGE_KEYWORDS.iter().any(|k| is_keyword(&upper, k)) {
            return StatementClass::DataChange;
        }
        if CALL_KEYWORDS.iter().any(|k| is_keyword(&upper, k)) {
            return StatementClass::Call;
        }
        return StatementClass::ReadOnly;
    }
    if first == "BEGIN" {
        // `BEGIN ... END;` is an anonymous block (not auditable, stays behind
        // the flag); a bare `BEGIN` is transaction control.
        let tail = upper.trim_end().trim_end_matches(';').trim_end();
        if tail.ends_with("END") {
            return StatementClass::Call;
        }
        return StatementClass::Other;
    }
    if READ_ONLY_KEYWORDS.contains(&first.as_str()) {
        return StatementClass::ReadOnly;
    }
    StatementClass::Other
}

/// Apply the CLI write policy (issue #58 D4): L1 read-only and transaction
/// control pass, L2 data changes need the flag, L3 destructive never passes.
pub(crate) fn write_gate(sql: &str, allow_write: bool) -> WriteGate {
    match classify_statement(sql) {
        StatementClass::Destructive => WriteGate::Destructive,
        class @ (StatementClass::DataChange | StatementClass::Call) => {
            if allow_write {
                WriteGate::Allow
            } else {
                WriteGate::NeedsFlag(class)
            }
        }
        _ => WriteGate::Allow,
    }
}

pub(crate) async fn execute_query(conn: &mut dyn DbConn, sql: &str) -> Result<QueryResult, String> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err("Empty SQL statement".to_string());
    }
    conn.query(trimmed)
        .await
        .map_err(|e| format!("Query failed: {}", e))
}

pub(crate) fn render_result(
    result: &QueryResult,
    writer: &mut dyn std::io::Write,
    format: OutputFormat,
) -> Result<(), String> {
    if result.columns.is_empty() {
        match result.rows_affected {
            Some(n) => match format {
                OutputFormat::Json => {
                    let v = serde_json::json!({ "rows_affected": n });
                    writeln!(
                        writer,
                        "{}",
                        serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
                    )
                    .map_err(|e| format!("write error: {}", e))?;
                }
                _ => {
                    writeln!(writer, "{} rows affected", n)
                        .map_err(|e| format!("write error: {}", e))?;
                }
            },
            None => {
                writeln!(writer, "(0 rows)").map_err(|e| format!("write error: {}", e))?;
            }
        }
        return Ok(());
    }

    match format {
        OutputFormat::Table => {
            let table_str = output::format_table(&result.columns, &result.rows);
            writeln!(writer, "{}", table_str).map_err(|e| format!("write error: {}", e))?;
            let count_label = if result.row_count == 1 {
                "1 row".to_string()
            } else {
                format!("{} rows", result.row_count)
            };
            writeln!(writer, "({})", count_label).map_err(|e| format!("write error: {}", e))?;
        }
        OutputFormat::Json => {
            let v = serde_json::json!({
                "columns": result.columns,
                "rows": result.rows,
                "row_count": result.row_count,
            });
            writeln!(
                writer,
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            )
            .map_err(|e| format!("write error: {}", e))?;
        }
        OutputFormat::Vertical => {
            for (row_idx, row) in result.rows.iter().enumerate() {
                writeln!(writer, "-[ RECORD {} ]-", row_idx + 1)
                    .map_err(|e| format!("write error: {}", e))?;
                for (col_idx, col_name) in result.columns.iter().enumerate() {
                    let val_str = value_to_compact_string(&row[col_idx]);
                    writeln!(writer, "{} | {}", col_name, val_str)
                        .map_err(|e| format!("write error: {}", e))?;
                }
            }
            let count_label = if result.row_count == 1 {
                "1 row".to_string()
            } else {
                format!("{} rows", result.row_count)
            };
            writeln!(writer, "({})", count_label).map_err(|e| format!("write error: {}", e))?;
        }
        OutputFormat::Csv => {
            let mut csv_writer = csv::Writer::from_writer(writer);
            csv_writer
                .write_record(&result.columns)
                .map_err(|e| format!("write error: {}", e))?;
            for row in &result.rows {
                let str_row: Vec<String> = row.iter().map(value_to_compact_string).collect();
                csv_writer
                    .write_record(&str_row)
                    .map_err(|e| format!("write error: {}", e))?;
            }
            csv_writer
                .flush()
                .map_err(|e| format!("write error: {}", e))?;
        }
    }
    Ok(())
}

// ─── CLI audit helpers (pure, unit-tested) ──────────────────────────

/// Classify how the CLI SQL was supplied: `argv` | file path | `stdin`.
pub(crate) fn cli_source_label(sql: Option<&str>, file: Option<&str>) -> String {
    if sql.is_some() {
        "argv".to_string()
    } else if let Some(path) = file {
        path.to_string()
    } else {
        "stdin".to_string()
    }
}

/// Map the statement class onto the audit `class` enum.
pub(crate) fn action_class(class: StatementClass) -> ActionClass {
    match class {
        StatementClass::ReadOnly | StatementClass::Other => ActionClass::Dql,
        StatementClass::DataChange => ActionClass::Dml,
        StatementClass::Call => ActionClass::Call,
        StatementClass::Destructive => ActionClass::Ddl,
    }
}

/// Human-readable refusal for a gate that did not pass.
pub(crate) fn write_gate_message(gate: WriteGate) -> String {
    match gate {
        WriteGate::Allow => String::new(),
        WriteGate::Destructive => {
            "refusing destructive DDL: --allow-write covers INSERT/UPDATE/DELETE \
and CALL, not DROP/TRUNCATE/ALTER/CREATE/GRANT"
                .to_string()
        }
        WriteGate::NeedsFlag(StatementClass::Call) => {
            "CALL / DO / anonymous blocks require --allow-write".to_string()
        }
        WriteGate::NeedsFlag(_) => {
            "data changes require --allow-write (INSERT/UPDATE/DELETE are refused by default); \
re-run with --allow-write to execute them"
                .to_string()
        }
    }
}

/// Shared context for auditing one executed statement (CLI and REPL).
pub(crate) struct StmtAudit<'a> {
    pub channel: Channel,
    pub action: &'a str,
    pub conn_name: &'a str,
    pub url: &'a str,
    pub sql: &'a str,
    pub source: Option<&'a str>,
    pub class: StatementClass,
    pub allow_write: bool,
}

impl<'a> StmtAudit<'a> {
    pub(crate) fn cli(
        conn_name: &'a str,
        url: &'a str,
        sql: &'a str,
        source: &'a str,
        class: StatementClass,
        allow_write: bool,
    ) -> Self {
        Self {
            channel: Channel::Cli,
            action: "cli_sql",
            conn_name,
            url,
            sql,
            source: Some(source),
            class,
            allow_write,
        }
    }

    pub(crate) fn repl(
        conn_name: &'a str,
        url: &'a str,
        sql: &'a str,
        class: StatementClass,
        allow_write: bool,
    ) -> Self {
        Self {
            channel: Channel::Repl,
            action: "repl_sql",
            conn_name,
            url,
            sql,
            source: None,
            class,
            allow_write,
        }
    }
}

/// A statement event without an outcome (used for the fail-closed intent
/// record, and for session-level markers).
pub(crate) fn stmt_event(ctx: &StmtAudit<'_>, decision: Decision) -> DraftEvent {
    let mut event = DraftEvent::new(
        ctx.channel,
        ConnectionInfo::from_url(
            ctx.conn_name,
            ctx.url,
            read_only_session_for(ctx.url) && !ctx.allow_write,
        ),
        ctx.action,
        action_class(ctx.class),
        decision,
    )
    .with_sql(SqlInfo::new(ctx.sql));
    if let Some(source) = ctx.source {
        event = event.with_source(source);
    }
    event
}

pub(crate) fn stmt_ok_event(
    ctx: &StmtAudit<'_>,
    duration_ms: u64,
    rows: u64,
    rows_affected: bool,
) -> DraftEvent {
    let outcome = if rows_affected {
        AuditOutcome::ok(duration_ms).with_rows_affected(rows)
    } else {
        AuditOutcome::ok(duration_ms).with_row_count(rows)
    };
    stmt_event(ctx, Decision::Allow).with_outcome(outcome)
}

pub(crate) fn stmt_error_event(ctx: &StmtAudit<'_>, duration_ms: u64, error: &str) -> DraftEvent {
    stmt_event(ctx, Decision::Error).with_outcome(AuditOutcome::error(duration_ms, error))
}

/// Emitted once when a CLI/REPL session is opened with `--allow-write`, before
/// any statement runs (issue #58 CLI contract).
pub(crate) fn session_mode_event(conn_name: &str, url: &str, allow_write: bool) -> DraftEvent {
    DraftEvent::new(
        Channel::Cli,
        ConnectionInfo::from_url(conn_name, url, read_only_session_for(url) && !allow_write),
        "session_mode",
        ActionClass::Admin,
        Decision::Allow,
    )
    .with_detail(serde_json::json!({
        "mode": if allow_write { "allow_write" } else { "read_only" }
    }))
}

/// Event for an action that is not a SQL statement (connect, check,
/// store-password). Keeps one construction site for those actions.
pub(crate) fn action_event(
    channel: Channel,
    action: &str,
    conn_name: &str,
    url: &str,
    class: ActionClass,
    decision: Decision,
) -> DraftEvent {
    DraftEvent::new(
        channel,
        ConnectionInfo::from_url(conn_name, url, read_only_session_for(url)),
        action,
        class,
        decision,
    )
}

pub(crate) async fn run_cli(
    args: CliArgs,
    registry: &BackendRegistry,
    audit: &AuditSession,
) -> Result<(), String> {
    let sql = if let Some(s) = &args.sql {
        s.clone()
    } else if let Some(f) = &args.file {
        std::fs::read_to_string(f).map_err(|e| format!("Failed to read file '{}': {}", f, e))?
    } else {
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)
            .map_err(|e| format!("Failed to read stdin: {}", e))?;
        input
    };

    let sql = sql.trim().to_string();
    if sql.is_empty() {
        return Err("No SQL provided. Use -c/--sql, -f/--file, or pipe SQL to stdin.".to_string());
    }

    // Refuse before touching the engine (issue #58 acceptance 4).
    let gate = write_gate(&sql, args.allow_write);
    if gate != WriteGate::Allow {
        return Err(write_gate_message(gate));
    }
    let statement_class = classify_statement(&sql);
    let is_write = matches!(
        statement_class,
        StatementClass::DataChange | StatementClass::Call
    );

    let config_path = args.config_path.map(PathBuf::from);
    let raw = read_config(config_path)?;

    let target_name = args.connection_name.as_deref().unwrap_or(&raw.default_name);
    let target_conn = raw
        .connections
        .iter()
        .find(|c| c.name == target_name)
        .ok_or_else(|| {
            format!(
                "Connection '{}' not found. Available: {:?}",
                target_name,
                raw.connections.iter().map(|c| &c.name).collect::<Vec<_>>()
            )
        })?;

    let target = if raw.is_env_var {
        resolve_env_var_connection(target_conn.url.clone().unwrap())
    } else {
        resolve_single_connection(
            target_conn,
            raw.config_path.clone(),
            raw.base_timeout.as_ref(),
        )?
    };

    let effective_timeout = TimeoutConfig::from_overrides(
        args.statement_timeout.as_deref(),
        args.connection_max_lifetime.as_deref(),
        Some(&target.timeout_config),
    )
    .map_err(|e| format!("Invalid timeout configuration: {}", e))?;

    let scheme = target
        .connection_url
        .find("://")
        .map(|i| &target.connection_url[..i])
        .unwrap_or("mysql");
    let connect_event = |decision: Decision| {
        action_event(
            Channel::Cli,
            "connect",
            &target.name,
            &target.connection_url,
            ActionClass::Admin,
            decision,
        )
    };

    let pool = registry
        .connect_with_fallback(
            scheme,
            &target.connection_url,
            Some(&effective_timeout),
            args.allow_write,
        )
        .await
        .map_err(|e| {
            audit.record_best_effort(connect_event(Decision::Error));
            format!("Connection failed: {}", e)
        })?;

    let mut conn = pool.acquire().await.map_err(|e| {
        audit.record_best_effort(connect_event(Decision::Error));
        format!("Failed to acquire connection: {}", e)
    })?;
    audit.record_best_effort(connect_event(Decision::Allow));

    if let (Some(path), Some(plaintext)) = (&target.config_path, &target.plaintext_password) {
        info!(
            "migrating plaintext password to OS keychain for '{}'",
            target.keyring_username
        );
        match store_keyring_password(&target.keyring_username, plaintext) {
            Ok(()) => {
                if let Err(e) = rewrite_password_to_sentinel(path, &target.name) {
                    warn!(
                        "password stored in keychain but failed to update config: {}",
                        e
                    );
                } else {
                    info!(
                        "password migrated to OS keychain for '{}'",
                        target.keyring_username
                    );
                }
            }
            Err(e) => {
                warn!("failed to migrate password to keychain: {}", e);
            }
        }
    }

    let source_label = cli_source_label(args.sql.as_deref(), args.file.as_deref());

    if args.allow_write {
        audit.record_best_effort(session_mode_event(
            &target.name,
            &target.connection_url,
            true,
        ));
    }

    let ctx = StmtAudit::cli(
        &target.name,
        &target.connection_url,
        &sql,
        &source_label,
        statement_class,
        args.allow_write,
    );

    // Writes are fail-closed (issue #58): the intent must be on disk before the
    // statement reaches the engine, otherwise a mutation would be unaudited.
    if is_write {
        if let Err(e) = audit.record(stmt_event(&ctx, Decision::Allow)) {
            return Err(format!(
                "refusing to execute a data change without an audit record: {e}"
            ));
        }
    }

    let start = Instant::now();
    let result: Result<QueryResult, String> = if is_write {
        conn.execute_write(&sql)
            .await
            .map_err(|e| format!("Query failed: {}", e))
    } else {
        execute_query(&mut *conn, &sql).await
    };
    let duration_ms = start.elapsed().as_millis() as u64;

    match &result {
        Ok(qr) => audit.record_best_effort(stmt_ok_event(
            &ctx,
            duration_ms,
            qr.rows_affected.unwrap_or(qr.row_count as u64),
            is_write,
        )),
        Err(e) => audit.record_best_effort(stmt_error_event(&ctx, duration_ms, e)),
    }
    let result = result?;
    render_result(&result, &mut std::io::stdout(), args.format)?;

    if let Some(action) = args.timeout_action.as_deref() {
        if action == "disconnect" {
            if let Some(kill_sql) = conn.dialect().kill_own_connection_sql() {
                let _ = conn.query_drop(&kill_sql).await;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_leading_comments_plain() {
        assert_eq!(strip_leading_comments("SELECT 1"), "SELECT 1");
    }

    #[test]
    fn test_strip_leading_comments_line() {
        assert_eq!(
            strip_leading_comments("-- list users\nSELECT * FROM users"),
            "SELECT * FROM users"
        );
    }

    #[test]
    fn test_strip_leading_comments_block() {
        assert_eq!(strip_leading_comments("/* hint */ SELECT 1"), "SELECT 1");
    }

    #[test]
    fn test_strip_leading_comments_nested() {
        assert_eq!(
            strip_leading_comments("/* outer /* inner */ rest */ SELECT 1"),
            "SELECT 1"
        );
    }

    #[test]
    fn test_strip_leading_comments_multiple() {
        assert_eq!(
            strip_leading_comments("-- first\n-- second\n/* block */\nSELECT 1"),
            "SELECT 1"
        );
    }

    #[test]
    fn test_is_read_only_select() {
        let mysql_prefixes = &["SELECT", "EXPLAIN", "SHOW", "DESC", "DESCRIBE"];
        assert!(is_read_only_mcp("SELECT 1", mysql_prefixes));
        assert!(is_read_only_mcp("select * from users", mysql_prefixes));
        assert!(is_read_only_mcp("  SELECT 1", mysql_prefixes));
    }

    #[test]
    fn test_is_read_only_explain() {
        let mysql_prefixes = &["SELECT", "EXPLAIN", "SHOW", "DESC", "DESCRIBE"];
        assert!(is_read_only_mcp("EXPLAIN SELECT 1", mysql_prefixes));
        assert!(is_read_only_mcp("explain select 1", mysql_prefixes));
    }

    #[test]
    fn test_is_read_only_show() {
        let mysql_prefixes = &["SELECT", "EXPLAIN", "SHOW", "DESC", "DESCRIBE"];
        assert!(is_read_only_mcp("SHOW TABLES", mysql_prefixes));
        assert!(is_read_only_mcp("show databases", mysql_prefixes));
    }

    #[test]
    fn test_is_read_only_describe() {
        let mysql_prefixes = &["SELECT", "EXPLAIN", "SHOW", "DESC", "DESCRIBE"];
        assert!(is_read_only_mcp("DESC users", mysql_prefixes));
        assert!(is_read_only_mcp("DESCRIBE users", mysql_prefixes));
    }

    #[test]
    fn test_is_read_only_false() {
        let mysql_prefixes = &["SELECT", "EXPLAIN", "SHOW", "DESC", "DESCRIBE"];
        assert!(!is_read_only_mcp(
            "INSERT INTO t VALUES (1)",
            mysql_prefixes
        ));
        assert!(!is_read_only_mcp("UPDATE t SET a=1", mysql_prefixes));
        assert!(!is_read_only_mcp("DELETE FROM t", mysql_prefixes));
        assert!(!is_read_only_mcp("DROP TABLE t", mysql_prefixes));
    }

    #[test]
    fn test_is_read_only_mcp_with_oracle_prefixes_allows_with() {
        let oracle_prefixes = &["SELECT", "EXPLAIN", "WITH"];
        assert!(is_read_only_mcp(
            "WITH x AS (SELECT 1) SELECT * FROM x",
            oracle_prefixes
        ));
    }

    #[test]
    fn test_is_read_only_mcp_with_mysql_prefixes_rejects_with() {
        let mysql_prefixes = &["SELECT", "EXPLAIN", "SHOW", "DESC", "DESCRIBE"];
        assert!(!is_read_only_mcp(
            "WITH x AS (SELECT 1) SELECT * FROM x",
            mysql_prefixes
        ));
    }

    #[test]
    fn test_is_read_only_mcp_empty_prefixes_rejects_all() {
        assert!(!is_read_only_mcp("SELECT 1", &[]));
    }

    #[test]
    fn test_value_compact_null_and_string() {
        assert_eq!(value_to_compact_string(&Value::Null), "NULL");
        assert_eq!(
            value_to_compact_string(&Value::String("hello".into())),
            "hello"
        );
        assert_eq!(value_to_compact_string(&Value::Bool(true)), "true");
        assert_eq!(
            value_to_compact_string(&Value::Number(serde_json::Number::from(42))),
            "42"
        );
    }

    #[test]
    fn test_render_result_empty_query() {
        let result = QueryResult::empty();
        let mut buf: Vec<u8> = Vec::new();
        render_result(&result, &mut buf, OutputFormat::Table).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert_eq!(output, "(0 rows)\n");
    }

    #[test]
    fn test_render_result_table() {
        let result = QueryResult {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec![Value::String("1".into()), Value::String("2".into())]],
            row_count: 1,
            rows_affected: None,
        };
        let mut buf: Vec<u8> = Vec::new();
        render_result(&result, &mut buf, OutputFormat::Table).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains('│'));
        assert!(output.contains('─'));
    }

    #[test]
    fn test_render_result_csv() {
        let result = QueryResult {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec![Value::String("1".into()), Value::String("2".into())]],
            row_count: 1,
            rows_affected: None,
        };
        let mut buf: Vec<u8> = Vec::new();
        render_result(&result, &mut buf, OutputFormat::Csv).unwrap();
        let output = String::from_utf8(buf).unwrap();
        let output = output.replace("\r\n", "\n");
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "a,b");
        assert_eq!(lines[1], "1,2");
    }

    #[test]
    fn test_render_result_vertical() {
        let result = QueryResult {
            columns: vec!["col".into()],
            rows: vec![vec![Value::String("val".into())]],
            row_count: 1,
            rows_affected: None,
        };
        let mut buf: Vec<u8> = Vec::new();
        render_result(&result, &mut buf, OutputFormat::Vertical).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("-[ RECORD 1 ]-"));
        assert!(output.contains("col | val"));
        assert!(output.contains("(1 row)"));
    }

    #[test]
    fn test_output_format_from_str() {
        assert!(matches!(
            "table".parse::<OutputFormat>().unwrap(),
            OutputFormat::Table
        ));
        assert!(matches!(
            "json".parse::<OutputFormat>().unwrap(),
            OutputFormat::Json
        ));
        assert!(matches!(
            "vertical".parse::<OutputFormat>().unwrap(),
            OutputFormat::Vertical
        ));
        assert!(matches!(
            "csv".parse::<OutputFormat>().unwrap(),
            OutputFormat::Csv
        ));
        assert!("invalid".parse::<OutputFormat>().is_err());
    }

    #[test]
    fn should_classify_read_only_statements() {
        for sql in [
            "SELECT 1",
            "WITH x AS (SELECT 1) SELECT * FROM x",
            "WITH RECURSIVE r AS (SELECT 1) SELECT * FROM r",
        ] {
            assert_eq!(
                classify_statement(sql),
                StatementClass::ReadOnly,
                "expected ReadOnly for {sql:?}"
            );
        }
    }

    #[test]
    fn should_classify_transaction_control_as_other() {
        for sql in ["BEGIN", "COMMIT", "ROLLBACK", "START TRANSACTION"] {
            assert_eq!(
                classify_statement(sql),
                StatementClass::Other,
                "expected Other for {sql:?}"
            );
        }
    }

    #[test]
    fn should_classify_data_changes() {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "insert into t values (1)",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "REPLACE INTO t VALUES (1)",
            "MERGE INTO t USING s ON (1=1)",
            "  /* hint */ INSERT INTO t VALUES (1)",
            "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
        ] {
            assert_eq!(
                classify_statement(sql),
                StatementClass::DataChange,
                "expected DataChange for {sql:?}"
            );
        }
    }

    #[test]
    fn should_classify_calls_and_anonymous_blocks() {
        for sql in [
            "CALL foo(1)",
            "EXEC proc",
            "EXECUTE proc",
            "DO $$ BEGIN END $$",
            "DECLARE x NUMBER; BEGIN NULL; END;",
            "BEGIN INSERT INTO t VALUES (1); END;",
        ] {
            assert_eq!(
                classify_statement(sql),
                StatementClass::Call,
                "expected Call for {sql:?}"
            );
        }
    }

    #[test]
    fn should_classify_destructive_statements() {
        for sql in [
            "DROP TABLE t",
            "TRUNCATE TABLE t",
            "ALTER TABLE t ADD c INT",
            "CREATE TABLE t (id INT)",
            "GRANT SELECT ON t TO u",
            "REVOKE SELECT ON t FROM u",
            "RENAME TABLE a TO b",
        ] {
            assert_eq!(
                classify_statement(sql),
                StatementClass::Destructive,
                "expected Destructive for {sql:?}"
            );
        }
    }

    #[test]
    fn should_refuse_data_changes_without_the_flag() {
        assert_eq!(
            write_gate("INSERT INTO t VALUES (1)", false),
            WriteGate::NeedsFlag(StatementClass::DataChange)
        );
        assert_eq!(
            write_gate("CALL foo()", false),
            WriteGate::NeedsFlag(StatementClass::Call)
        );
        assert_eq!(write_gate("SELECT 1", false), WriteGate::Allow);
    }

    #[test]
    fn should_allow_data_changes_with_the_flag_but_never_destructive() {
        assert_eq!(
            write_gate("INSERT INTO t VALUES (1)", true),
            WriteGate::Allow
        );
        assert_eq!(write_gate("CALL foo()", true), WriteGate::Allow);
        assert_eq!(write_gate("DROP TABLE t", true), WriteGate::Destructive);
        assert_eq!(write_gate("TRUNCATE TABLE t", true), WriteGate::Destructive);
        assert_eq!(
            write_gate("ALTER TABLE t ADD c INT", true),
            WriteGate::Destructive
        );
    }

    #[test]
    fn cli_source_classifies_argv_file_stdin() {
        assert_eq!(cli_source_label(Some("SELECT 1"), None), "argv");
        assert_eq!(cli_source_label(None, Some("/tmp/q.sql")), "/tmp/q.sql");
        assert_eq!(cli_source_label(None, None), "stdin");
    }

    #[test]
    fn cli_sql_event_records_source_and_class() {
        let ctx = StmtAudit::cli(
            "dev",
            "mysql://u:p@h:3306/db",
            "SELECT 1",
            "argv",
            StatementClass::ReadOnly,
            false,
        );
        let ev = stmt_ok_event(&ctx, 5, 3, false);
        assert_eq!(ev.channel, Channel::Cli);
        assert_eq!(ev.action, "cli_sql");
        assert_eq!(ev.class, ActionClass::Dql);
        assert_eq!(ev.source.as_deref(), Some("argv"));
        assert!(!ev.connection.read_only_session);
        let outcome = ev.outcome.as_ref().unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.row_count, Some(3));

        let dml_ctx = StmtAudit::cli(
            "dev",
            "mysql://u:p@h:3306/db",
            "UPDATE t SET a=1",
            "stdin",
            StatementClass::DataChange,
            true,
        );
        let dml = stmt_ok_event(&dml_ctx, 5, 7, true);
        assert_eq!(dml.class, ActionClass::Dml);
        assert_eq!(dml.outcome.as_ref().unwrap().rows_affected, Some(7));
    }

    #[test]
    fn write_mode_marks_gaussdb_session_as_not_read_only() {
        let c = StmtAudit::cli(
            "g",
            "gaussdb://u:p@h:5432/db",
            "INSERT INTO t VALUES (1)",
            "argv",
            StatementClass::DataChange,
            false,
        );
        assert!(stmt_event(&c, Decision::Allow).connection.read_only_session);

        let c = StmtAudit::cli(
            "g",
            "gaussdb://u:p@h:5432/db",
            "INSERT INTO t VALUES (1)",
            "argv",
            StatementClass::DataChange,
            true,
        );
        assert!(!stmt_event(&c, Decision::Allow).connection.read_only_session);
    }

    #[test]
    fn session_mode_event_records_the_mode() {
        let ro = session_mode_event("g", "gaussdb://u:p@h:5432/db", false);
        assert_eq!(ro.action, "session_mode");
        assert_eq!(ro.detail.as_ref().unwrap()["mode"], "read_only");
        assert!(ro.connection.read_only_session);

        let rw = session_mode_event("g", "gaussdb://u:p@h:5432/db", true);
        assert_eq!(rw.detail.as_ref().unwrap()["mode"], "allow_write");
        assert!(!rw.connection.read_only_session);
    }

    #[test]
    fn render_result_reports_rows_affected_for_writes() {
        let mut out: Vec<u8> = Vec::new();
        render_result(&QueryResult::affected(3), &mut out, OutputFormat::Table).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "3 rows affected\n");

        let mut out: Vec<u8> = Vec::new();
        render_result(&QueryResult::affected(0), &mut out, OutputFormat::Json).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["rows_affected"], 0);

        let mut out: Vec<u8> = Vec::new();
        render_result(&QueryResult::empty(), &mut out, OutputFormat::Table).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "(0 rows)\n");
    }
}
