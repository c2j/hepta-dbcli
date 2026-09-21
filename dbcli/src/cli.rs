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
use crate::interactive::SqlTokenizer;
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
    /// `--url`: ad-hoc connection URL; skips config file and keyring.
    pub url: Option<String>,
    pub config_path: Option<String>,
    pub format: OutputFormat,
    pub statement_timeout: Option<String>,
    pub connection_max_lifetime: Option<String>,
    pub no_history: bool,
    pub timeout_action: Option<String>,
    /// `--allow-write`: permit L2 data changes (issue #58).
    pub allow_write: bool,
    /// `--allow-ddl`: permit DDL (DROP/TRUNCATE/ALTER/CREATE/RENAME, issue #112).
    pub allow_ddl: bool,
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
    /// `DROP` / `TRUNCATE` / `ALTER` / `CREATE` / `RENAME` — needs `--allow-ddl`.
    Ddl,
    /// `GRANT` / `REVOKE` — never allowed from the CLI/REPL.
    Privilege,
    /// Transaction control and anything unrecognised: unchanged behaviour.
    Other,
}

/// What the gate decided for one statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteGate {
    /// Execute as-is.
    Allow,
    /// Refused: the statement's class is not covered by the flags that were set
    /// (a data change without `--allow-write`, DDL without `--allow-ddl`, or a
    /// privilege statement, which is never allowed).
    NeedsFlag(StatementClass),
}

const DDL_KEYWORDS: &[&str] = &["DROP", "TRUNCATE", "ALTER", "CREATE", "RENAME"];
const PRIVILEGE_KEYWORDS: &[&str] = &["GRANT", "REVOKE"];
const DATA_CHANGE_KEYWORDS: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "REPLACE", "MERGE", "UPSERT", "LOAD", "IMPORT",
];
/// Statement-leading keywords that mutate data but are not INSERT-family.
/// COPY (server-side bulk load) and SELECT ... INTO (table creation from a
/// query) run through the read path once a session is writable (--allow-ddl),
/// so they must classify as DataChange to keep the --allow-write gate
/// (issue #112 PR review).
const DATA_CHANGE_LEAD_KEYWORDS: &[&str] = &["COPY", "IMPORT"];
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

    if DDL_KEYWORDS.contains(&first.as_str()) {
        return StatementClass::Ddl;
    }
    if PRIVILEGE_KEYWORDS.contains(&first.as_str()) {
        return StatementClass::Privilege;
    }
    if DATA_CHANGE_KEYWORDS.contains(&first.as_str())
        || DATA_CHANGE_LEAD_KEYWORDS.contains(&first.as_str())
    {
        return StatementClass::DataChange;
    }
    if CALL_KEYWORDS.contains(&first.as_str()) {
        return StatementClass::Call;
    }
    if first == "WITH" {
        // A CTE can carry a data change (`WITH x AS (...) INSERT ...`).
        if DDL_KEYWORDS.iter().any(|k| is_keyword(&upper, k)) {
            return StatementClass::Ddl;
        }
        if PRIVILEGE_KEYWORDS.iter().any(|k| is_keyword(&upper, k)) {
            return StatementClass::Privilege;
        }
        if DATA_CHANGE_KEYWORDS.iter().any(|k| is_keyword(&upper, k))
            || DATA_CHANGE_LEAD_KEYWORDS
                .iter()
                .any(|k| is_keyword(&upper, k))
            || upper.contains(" INTO ")
        {
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
        // SELECT ... INTO creates a table from a query (a data change), but
        // only when INTO is followed by a table name — `SELECT ... into_t`
        // inside a FROM list is still a plain read. The first FROM-position
        // word check keeps the heuristic conservative (issue #112 PR review).
        if first == "SELECT" && upper.contains(" INTO ") {
            // `INTO` immediately after SELECT (SELECT INTO var_list is
            // PL/SQL) vs `SELECT ... INTO table`: both mutate something, so
            // stay conservative and require --allow-write.
            return StatementClass::DataChange;
        }
        return StatementClass::ReadOnly;
    }
    StatementClass::Other
}

/// Apply the CLI write policy (issue #58 D4, issue #112): L1 read-only and
/// transaction control pass, L2 data changes need `--allow-write`, DDL needs
/// `--allow-ddl`, and GRANT/REVOKE never pass.
pub(crate) fn write_gate(sql: &str, allow_write: bool, allow_ddl: bool) -> WriteGate {
    match classify_statement(sql) {
        StatementClass::Privilege => WriteGate::NeedsFlag(StatementClass::Privilege),
        StatementClass::Ddl => {
            if allow_ddl {
                WriteGate::Allow
            } else {
                WriteGate::NeedsFlag(StatementClass::Ddl)
            }
        }
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

// ─── Driver error classification (shared with MCP) ──────────────────

/// Map a [`DbError`](crate::backend::DbError) to the audit `error_kind` plus
/// the SQLSTATE the driver embedded in its message, if any. MCP and the
/// CLI/REPL must agree here so the ledger reads the same whichever channel
/// ran the statement (issue #57 review).
pub(crate) fn classify_query_error(err: &crate::backend::DbError) -> (String, Option<String>) {
    let kind = format!("{:?}", err.kind);
    let sqlstate = extract_sqlstate(&err.to_string());
    (kind, sqlstate)
}

/// Extract a 5-character SQLSTATE code from an error message, if present.
pub(crate) fn extract_sqlstate(msg: &str) -> Option<String> {
    let idx = msg.to_ascii_uppercase().find("SQLSTATE")?;
    let tail = &msg[idx + "SQLSTATE".len()..];
    let code: String = tail
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(5)
        .collect();
    if code.len() == 5 {
        Some(code)
    } else {
        None
    }
}

/// Execute a statement and hand back the driver error untouched, so callers
/// can classify it for the ledger.
pub(crate) async fn execute_query_typed(
    conn: &mut dyn DbConn,
    sql: &str,
) -> Result<QueryResult, crate::backend::DbError> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(crate::backend::DbError::query("Empty SQL statement"));
    }
    conn.query(trimmed).await
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
        StatementClass::Ddl => ActionClass::Ddl,
        StatementClass::Privilege => ActionClass::Dcl,
    }
}

/// Machine-readable reason for a gate that did not pass, recorded as
/// `deny_reason` so an attempted write is visible in the ledger even though it
/// never reached the engine (issue #57 §5 "deny_reason").
pub(crate) fn write_gate_reason(gate: WriteGate) -> &'static str {
    match gate {
        WriteGate::Allow => "",
        WriteGate::NeedsFlag(StatementClass::Ddl) => "ddl_flag_required",
        WriteGate::NeedsFlag(StatementClass::Privilege) => "privilege_refused",
        WriteGate::NeedsFlag(_) => "write_flag_required",
    }
}

/// Human-readable refusal for a gate that did not pass.
pub(crate) fn write_gate_message(gate: WriteGate) -> String {
    match gate {
        WriteGate::Allow => String::new(),
        WriteGate::NeedsFlag(StatementClass::Ddl) => {
            "DDL requires --allow-ddl (DROP/TRUNCATE/ALTER/CREATE/RENAME); \
--allow-write only covers INSERT/UPDATE/DELETE and CALL"
                .to_string()
        }
        WriteGate::NeedsFlag(StatementClass::Privilege) => {
            "refusing GRANT/REVOKE: privilege changes are never allowed from the CLI/REPL"
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
    /// Whether `--allow-ddl` put the session in write mode (issue #112).
    pub allow_ddl: bool,
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
            allow_ddl: false,
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
            allow_ddl: false,
        }
    }

    /// Record that the session also ran with `--allow-ddl` (issue #112).
    pub(crate) fn with_allow_ddl(mut self, allow_ddl: bool) -> Self {
        self.allow_ddl = allow_ddl;
        self
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
            read_only_session_for(ctx.url) && !ctx.allow_write && !ctx.allow_ddl,
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

pub(crate) fn stmt_error_event(
    ctx: &StmtAudit<'_>,
    duration_ms: u64,
    error_kind: &str,
    sqlstate: Option<&str>,
) -> DraftEvent {
    let mut outcome = AuditOutcome::error(duration_ms, error_kind);
    if let Some(state) = sqlstate {
        outcome = outcome.with_sqlstate(state);
    }
    stmt_event(ctx, Decision::Error).with_outcome(outcome)
}

/// Emitted once when a CLI/REPL session is opened with `--allow-write` and/or
/// `--allow-ddl`, before any statement runs (issue #58 CLI contract, #112).
pub(crate) fn session_mode_event(
    conn_name: &str,
    url: &str,
    allow_write: bool,
    allow_ddl: bool,
) -> DraftEvent {
    let mode = match (allow_write, allow_ddl) {
        (false, false) => "read_only",
        (true, false) => "allow_write",
        (false, true) => "allow_ddl",
        (true, true) => "allow_write+ddl",
    };
    DraftEvent::new(
        Channel::Cli,
        ConnectionInfo::from_url(
            conn_name,
            url,
            read_only_session_for(url) && !allow_write && !allow_ddl,
        ),
        "session_mode",
        ActionClass::Admin,
        Decision::Allow,
    )
    .with_detail(serde_json::json!({ "mode": mode }))
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

/// `--url`（Some）直接短路为 inline 连接；否则按名解析（现有路径）。
pub(crate) fn resolve_cli_target(
    raw: Option<&crate::config::McpRawConfig>,
    url: Option<&str>,
    connection_name: Option<&str>,
) -> Result<crate::config::ResolvedConnection, String> {
    if let Some(u) = url {
        return crate::config::resolve_inline_url_connection_result(u);
    }
    let raw = raw.expect("config is required when --url is absent");
    let target_name = connection_name.unwrap_or(&raw.default_name);
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
    if raw.is_env_var {
        resolve_env_var_connection(target_conn.url.clone().unwrap_or_default())
    } else {
        resolve_single_connection(
            target_conn,
            raw.config_path.clone(),
            raw.base_timeout.as_ref(),
        )
    }
}

/// Tokenizer quoting rules for a URL scheme. MySQL uses backticks and `#`
/// comments; GaussDB/Oracle/DuckDB use ANSI double quotes and dollar-quoted
/// bodies (issue #112).
pub(crate) fn statement_split_params(scheme: &str) -> (char, bool, bool) {
    match scheme {
        "mysql" => ('`', true, false),
        _ => ('"', false, true),
    }
}

/// True when a split fragment carries an actual statement (not blank and not
/// only comments), so comment-only tails do not reach the engine.
fn has_statement(fragment: &str) -> bool {
    !strip_leading_comments(fragment.trim()).trim().is_empty()
}

/// Split a `-f`/stdin SQL script into individual statements, dropping blank and
/// comment-only fragments. Reuses the REPL tokenizer so quoting, comments and
/// dollar-quoted bodies behave identically (issue #112).
pub(crate) fn split_cli_statements(sql: &str, scheme: &str) -> Vec<String> {
    let (id_quote, hash_comment, dollar_quote) = statement_split_params(scheme);
    let split = SqlTokenizer::split_statements(sql, id_quote, hash_comment, dollar_quote);
    split
        .complete
        .into_iter()
        .chain(std::iter::once(split.remainder))
        .map(|s| s.trim().to_string())
        .filter(|s| has_statement(s))
        .collect()
}

/// Statements to execute for one CLI invocation. A single `--sql` statement is
/// passed through untouched (exact pre-#112 behaviour); a `-f`/stdin script is
/// split so multi-statement files stop being sent as one prepared statement.
pub(crate) fn cli_statements(sql: &str, scheme: &str, from_argv: bool) -> Vec<String> {
    if from_argv {
        return vec![sql.to_string()];
    }
    let split = split_cli_statements(sql, scheme);
    if split.is_empty() {
        // Only comments/blank lines: keep the original so the empty-statement
        // error surface is unchanged.
        vec![sql.to_string()]
    } else {
        split
    }
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

    let config_path = args.config_path.map(PathBuf::from);
    // --url short-circuits config resolution entirely (no toml, no keyring).
    let raw = if args.url.is_some() {
        crate::config::McpRawConfig::empty()
    } else {
        read_config(config_path)?
    };

    let target = resolve_cli_target(
        Some(&raw),
        args.url.as_deref(),
        args.connection_name.as_deref(),
    )?;

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
            // GaussDB opens a read-only session unless told otherwise, and a
            // read-only session refuses DDL server-side — so `--allow-ddl`
            // must put the pool in write mode too (issue #112).
            args.allow_write || args.allow_ddl,
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
    let statements = cli_statements(&sql, scheme, args.sql.is_some());
    let total = statements.len();

    // One marker per session, so the ledger states whether it could write
    // (issue #57 CLI contract: read_only -> allow_write; #112 adds ddl).
    audit.record_best_effort(session_mode_event(
        &target.name,
        &target.connection_url,
        args.allow_write,
        args.allow_ddl,
    ));

    for (idx, stmt) in statements.iter().enumerate() {
        // Prefix only when a script carries several statements, so a single
        // statement keeps the exact pre-#112 error surface.
        let label = if total > 1 {
            Some(format!("statement {}/{}", idx + 1, total))
        } else {
            None
        };
        let fail = |msg: String| -> String {
            match &label {
                Some(l) => format!("{} failed: {}", l, msg),
                None => msg,
            }
        };

        let statement_class = classify_statement(stmt);
        let ctx = StmtAudit::cli(
            &target.name,
            &target.connection_url,
            stmt,
            &source_label,
            statement_class,
            args.allow_write,
        )
        .with_allow_ddl(args.allow_ddl);

        // Refuse before touching the engine (issue #58 acceptance 4). A refusal
        // is still an auditable fact: it answers "who tried to write"
        // (issue #57 §5).
        let gate = write_gate(stmt, args.allow_write, args.allow_ddl);
        if gate != WriteGate::Allow {
            audit.record_best_effort(
                stmt_event(&ctx, Decision::Deny).with_deny_reason(write_gate_reason(gate)),
            );
            return Err(fail(write_gate_message(gate)));
        }
        // Reject MySQL-only syntax (e.g. LIMIT on Oracle) before it reaches the
        // server, where it would kill the connection instead of reporting why.
        if let Some(hint) = conn.dialect().statement_syntax_hint(stmt) {
            return Err(fail(hint));
        }
        let is_write = matches!(
            statement_class,
            StatementClass::DataChange | StatementClass::Call | StatementClass::Ddl
        );

        // Writes are fail-closed (issue #58): the intent must be on disk before
        // the statement reaches the engine, otherwise a mutation would be
        // unaudited.
        if is_write {
            if let Err(e) = audit.record(stmt_event(&ctx, Decision::Allow)) {
                return Err(fail(format!(
                    "refusing to execute a data change without an audit record: {e}"
                )));
            }
        }

        let start = Instant::now();
        let result: Result<QueryResult, crate::backend::DbError> = if is_write {
            conn.execute_write(stmt).await
        } else {
            execute_query_typed(&mut *conn, stmt).await
        };
        let duration_ms = start.elapsed().as_millis() as u64;

        match &result {
            Ok(qr) => audit.record_best_effort(stmt_ok_event(
                &ctx,
                duration_ms,
                qr.rows_affected.unwrap_or(qr.row_count as u64),
                is_write,
            )),
            Err(e) => {
                let (error_kind, sqlstate) = classify_query_error(e);
                audit.record_best_effort(stmt_error_event(
                    &ctx,
                    duration_ms,
                    &error_kind,
                    sqlstate.as_deref(),
                ))
            }
        }
        let result = result.map_err(|e| fail(format!("Query failed: {}", e)))?;
        render_result(&result, &mut std::io::stdout(), args.format).map_err(fail)?;
    }

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
    fn resolve_cli_target_url_short_circuits_config() {
        // 无配置文件环境：--url 直接生效，不查 toml、不碰 keyring。
        let raw = crate::config::McpRawConfig::empty();
        let target = resolve_cli_target(Some(&raw), Some("duckdb:///tmp/shop.duckdb"), None)
            .expect("url target should resolve");
        assert_eq!(target.connection_url, "duckdb:///tmp/shop.duckdb");
        assert_eq!(target.name, "inline-duckdb");
    }

    #[test]
    fn resolve_cli_target_rejects_bad_url_shape() {
        let raw = crate::config::McpRawConfig::empty();
        let err = resolve_cli_target(Some(&raw), Some("not-a-url"), None)
            .expect_err("scheme-less --url must fail");
        assert!(err.contains("invalid connection URL"), "{err}");
    }

    #[test]
    fn resolve_cli_target_resolves_url_without_any_config() {
        // `check --url` 复用本函数：raw=None 时 URL 必须独立生效，
        // 名字参数被忽略；URL 形态非法时报错而不是 panic。
        let resolved = resolve_cli_target(None, Some("duckdb:///tmp/copy.duckdb"), None)
            .expect("valid url resolves without config");
        assert_eq!(resolved.name, "inline-duckdb");

        let err = resolve_cli_target(None, Some("not-a-url"), None)
            .expect_err("scheme-less url must fail");
        assert!(err.contains("invalid connection URL"), "{err}");
    }

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
    fn should_classify_session_writable_mutations_as_data_changes() {
        // PR review (#112): once --allow-ddl opens a writable session, these
        // statements run through the read path and would skip --allow-write
        // unless they classify as DataChange.
        for sql in [
            "COPY t FROM '/tmp/data.csv' WITH CSV",
            "copy t (a, b) from stdin",
            "SELECT * INTO backup_t FROM t",
            "select a into new_t from old_t where a > 1",
            "WITH x AS (SELECT 1) SELECT * INTO t2 FROM x",
        ] {
            assert_eq!(
                classify_statement(sql),
                StatementClass::DataChange,
                "expected DataChange for {sql:?}"
            );
        }
        // Plain SELECT stays read-only.
        assert_eq!(
            classify_statement("SELECT * FROM t"),
            StatementClass::ReadOnly
        );
        assert_eq!(
            classify_statement("SELECT * FROM into_t"),
            StatementClass::ReadOnly,
            "a table named into_t must not trip the INTO match"
        );
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
            "RENAME TABLE a TO b",
        ] {
            assert_eq!(
                classify_statement(sql),
                StatementClass::Ddl,
                "expected Ddl for {sql:?}"
            );
        }
        for sql in ["GRANT SELECT ON t TO u", "REVOKE SELECT ON t FROM u"] {
            assert_eq!(
                classify_statement(sql),
                StatementClass::Privilege,
                "expected Privilege for {sql:?}"
            );
        }
    }

    #[test]
    fn should_refuse_data_changes_without_the_flag() {
        assert_eq!(
            write_gate("INSERT INTO t VALUES (1)", false, false),
            WriteGate::NeedsFlag(StatementClass::DataChange)
        );
        assert_eq!(
            write_gate("CALL foo()", false, false),
            WriteGate::NeedsFlag(StatementClass::Call)
        );
        assert_eq!(write_gate("SELECT 1", false, false), WriteGate::Allow);
    }

    #[test]
    fn should_allow_data_changes_with_the_flag_but_never_destructive() {
        assert_eq!(
            write_gate("INSERT INTO t VALUES (1)", true, false),
            WriteGate::Allow
        );
        assert_eq!(write_gate("CALL foo()", true, false), WriteGate::Allow);
        assert_eq!(
            write_gate("DROP TABLE t", true, false),
            WriteGate::NeedsFlag(StatementClass::Ddl)
        );
        assert_eq!(
            write_gate("TRUNCATE TABLE t", true, false),
            WriteGate::NeedsFlag(StatementClass::Ddl)
        );
        assert_eq!(
            write_gate("ALTER TABLE t ADD c INT", true, false),
            WriteGate::NeedsFlag(StatementClass::Ddl)
        );
    }

    #[test]
    fn should_classify_ddl_and_privilege_separately() {
        for sql in [
            "DROP TABLE t",
            "TRUNCATE TABLE t",
            "ALTER TABLE t ADD c INT",
            "CREATE TABLE t (id INT)",
            "RENAME TABLE a TO b",
        ] {
            assert_eq!(
                classify_statement(sql),
                StatementClass::Ddl,
                "expected Ddl for {sql:?}"
            );
        }
        for sql in ["GRANT SELECT ON t TO u", "REVOKE SELECT ON t FROM u"] {
            assert_eq!(
                classify_statement(sql),
                StatementClass::Privilege,
                "expected Privilege for {sql:?}"
            );
        }
    }

    #[test]
    fn should_refuse_ddl_without_the_ddl_flag() {
        for sql in [
            "TRUNCATE TABLE t",
            "CREATE TABLE t (id INT)",
            "DROP TABLE t",
            "ALTER TABLE t ADD c INT",
        ] {
            assert_eq!(
                write_gate(sql, true, false),
                WriteGate::NeedsFlag(StatementClass::Ddl),
                "--allow-write alone must not open DDL: {sql:?}"
            );
            assert_eq!(
                write_gate(sql, false, false),
                WriteGate::NeedsFlag(StatementClass::Ddl),
                "{sql:?}"
            );
        }
    }

    #[test]
    fn should_allow_ddl_with_the_ddl_flag() {
        for sql in [
            "TRUNCATE TABLE t",
            "CREATE TABLE t (id INT)",
            "DROP TABLE t",
            "ALTER TABLE t ADD c INT",
            "RENAME TABLE a TO b",
        ] {
            assert_eq!(
                write_gate(sql, false, true),
                WriteGate::Allow,
                "--allow-ddl must open DDL: {sql:?}"
            );
        }
    }

    #[test]
    fn should_always_refuse_privileges() {
        for sql in ["GRANT SELECT ON t TO u", "REVOKE SELECT ON t FROM u"] {
            assert_eq!(
                write_gate(sql, true, true),
                WriteGate::NeedsFlag(StatementClass::Privilege),
                "privileges are never allowed: {sql:?}"
            );
            assert_eq!(
                write_gate(sql, false, false),
                WriteGate::NeedsFlag(StatementClass::Privilege),
                "{sql:?}"
            );
        }
    }

    #[test]
    fn allow_ddl_does_not_open_data_changes_without_allow_write() {
        assert_eq!(
            write_gate("INSERT INTO t VALUES (1)", false, true),
            WriteGate::NeedsFlag(StatementClass::DataChange)
        );
        assert_eq!(
            write_gate("CALL foo()", false, true),
            WriteGate::NeedsFlag(StatementClass::Call)
        );
    }

    #[test]
    fn split_cli_statements_handles_a_gaussdb_seed_script() {
        let sql = "SET search_path TO app;\n\
                   INSERT INTO t VALUES (1);\n\
                   INSERT INTO t VALUES (2);\n\
                   DO $$ BEGIN PERFORM 1; END $$;";
        let stmts = split_cli_statements(sql, "gaussdb");
        assert_eq!(stmts.len(), 4, "{stmts:?}");
        assert_eq!(stmts[0], "SET search_path TO app");
        assert_eq!(stmts[3], "DO $$ BEGIN PERFORM 1; END $$");
    }

    #[test]
    fn split_cli_statements_mysql_treats_backticks_as_identifiers() {
        let stmts = split_cli_statements("SELECT `a;b` FROM t; SELECT 2;", "mysql");
        assert_eq!(stmts, vec!["SELECT `a;b` FROM t", "SELECT 2"]);
    }

    #[test]
    fn split_cli_statements_drops_blank_and_comment_only_fragments() {
        let stmts = split_cli_statements("-- header\n;\n\nSELECT 1;", "gaussdb");
        assert_eq!(stmts, vec!["SELECT 1"]);
    }

    #[test]
    fn split_cli_statements_keeps_a_single_statement_intact() {
        let stmts = split_cli_statements("SELECT 1", "mysql");
        assert_eq!(stmts, vec!["SELECT 1"]);
    }

    #[test]
    fn cli_statements_passes_argv_through_untouched() {
        // A single `--sql` argument keeps its exact text (including a trailing
        // semicolon), so the pre-#112 single-statement path is unchanged.
        assert_eq!(
            cli_statements("SELECT 1;", "mysql", true),
            vec!["SELECT 1;"]
        );
    }

    #[test]
    fn cli_statements_splits_a_stdin_script() {
        let stmts = cli_statements("SELECT 1; SELECT 2;", "mysql", false);
        assert_eq!(stmts, vec!["SELECT 1", "SELECT 2"]);
    }

    #[test]
    fn cli_statements_falls_back_to_the_original_for_comment_only_input() {
        assert_eq!(
            cli_statements("-- just a comment", "mysql", false),
            vec!["-- just a comment"]
        );
    }

    #[test]
    fn write_gate_reason_names_each_refusal() {
        assert_eq!(
            write_gate_reason(write_gate("DROP TABLE t", true, false)),
            "ddl_flag_required"
        );
        assert_eq!(
            write_gate_reason(write_gate("TRUNCATE TABLE t", false, false)),
            "ddl_flag_required"
        );
        assert_eq!(
            write_gate_reason(write_gate("GRANT SELECT ON t TO u", true, true)),
            "privilege_refused"
        );
        assert_eq!(
            write_gate_reason(write_gate("INSERT INTO t VALUES (1)", false, false)),
            "write_flag_required"
        );
        assert_eq!(
            write_gate_reason(write_gate("CALL p()", false, false)),
            "write_flag_required"
        );
    }

    #[test]
    fn error_events_record_kind_and_sqlstate() {
        let ctx = StmtAudit::cli(
            "dev",
            "gaussdb://u:p@h:5432/db",
            "CREATE TABLE t (id INT)",
            "argv",
            StatementClass::Ddl,
            true,
        );
        let ev = stmt_error_event(&ctx, 7, "QueryFailed", Some("25006"));
        assert_eq!(ev.decision, Decision::Error);
        let outcome = ev.outcome.as_ref().unwrap();
        assert_eq!(outcome.error_kind.as_deref(), Some("QueryFailed"));
        assert_eq!(outcome.sqlstate.as_deref(), Some("25006"));

        let plain = stmt_error_event(&ctx, 1, "QueryFailed", None);
        assert!(plain.outcome.as_ref().unwrap().sqlstate.is_none());
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
        let ro = session_mode_event("g", "gaussdb://u:p@h:5432/db", false, false);
        assert_eq!(ro.action, "session_mode");
        assert_eq!(ro.detail.as_ref().unwrap()["mode"], "read_only");
        assert!(ro.connection.read_only_session);

        let rw = session_mode_event("g", "gaussdb://u:p@h:5432/db", true, false);
        assert_eq!(rw.detail.as_ref().unwrap()["mode"], "allow_write");
        assert!(!rw.connection.read_only_session);

        let ddl = session_mode_event("g", "gaussdb://u:p@h:5432/db", false, true);
        assert_eq!(ddl.detail.as_ref().unwrap()["mode"], "allow_ddl");
        assert!(!ddl.connection.read_only_session);

        let both = session_mode_event("g", "gaussdb://u:p@h:5432/db", true, true);
        assert_eq!(both.detail.as_ref().unwrap()["mode"], "allow_write+ddl");
        assert!(!both.connection.read_only_session);
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
