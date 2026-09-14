use std::borrow::Cow;
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

use rustyline::config::Configurer;
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::history::DefaultHistory;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::Editor;
use rustyline_derive::{Completer, Helper, Hinter};

use tracing::{info, warn};

use crate::audit::event::{
    read_only_session_for, ActionClass, AuditOutcome, Channel, ConnectionInfo, Decision,
    DraftEvent, SqlInfo,
};
use crate::audit::AuditSession;
use crate::backend::factory::BackendRegistry;
use crate::backend::DbConn;
use crate::cli::QueryResult;
use crate::cli::{execute_query, is_read_only_query, render_result, CliArgs, OutputFormat};
use crate::config::{
    read_config, resolve_env_var_connection, resolve_single_connection,
    rewrite_password_to_sentinel, store_keyring_password, TimeoutConfig,
};

// ─── SqlTokenizer (MySQL variant: supports backtick quoting, # comments) ───

pub(crate) struct SplitResult {
    pub complete: Vec<String>,
    pub remainder: String,
}

pub(crate) struct SqlTokenizer;

impl SqlTokenizer {
    pub(crate) fn split_statements(
        input: &str,
        id_quote: char,
        hash_comment: bool,
        dollar_quote: bool,
    ) -> SplitResult {
        let mut complete: Vec<String> = Vec::new();
        let mut current = String::new();
        let mut in_single_quote = false;
        let mut in_double_quote = false;
        let mut in_id_quote = false;
        let mut in_line_comment = false;
        let mut in_block_comment_depth: u32 = 0;
        let _in_dollar_quote = false;
        let mut dollar_quote_tag: Option<String> = None;

        let chars: Vec<char> = input.chars().collect();
        let len = chars.len();
        let mut i = 0;

        while i < len {
            let c = chars[i];

            if in_line_comment {
                if c == '\n' {
                    in_line_comment = false;
                }
                current.push(c);
                i += 1;
                continue;
            }

            if in_block_comment_depth > 0 {
                current.push(c);
                if c == '/' && i + 1 < len && chars[i + 1] == '*' {
                    in_block_comment_depth += 1;
                    i += 1;
                    current.push(chars[i]);
                } else if c == '*' && i + 1 < len && chars[i + 1] == '/' {
                    in_block_comment_depth -= 1;
                    i += 1;
                    current.push(chars[i]);
                }
                i += 1;
                continue;
            }

            if in_single_quote {
                current.push(c);
                if c == '\'' && i + 1 < len && chars[i + 1] == '\'' {
                    i += 1;
                    current.push(chars[i]);
                } else if c == '\'' {
                    in_single_quote = false;
                }
                i += 1;
                continue;
            }

            if in_double_quote {
                current.push(c);
                if c == '"' && i + 1 < len && chars[i + 1] == '"' {
                    i += 1;
                    current.push(chars[i]);
                } else if c == '"' {
                    in_double_quote = false;
                }
                i += 1;
                continue;
            }

            if let Some(ref tag) = dollar_quote_tag {
                current.push(c);
                if c == '$' && dollar_quote {
                    if tag.is_empty() {
                        if i + 1 < len && chars[i + 1] == '$' {
                            i += 1;
                            current.push(chars[i]);
                            dollar_quote_tag = None;
                        }
                    } else {
                        let tag_chars: Vec<char> = tag.chars().collect();
                        let tag_len = tag_chars.len();
                        if i + 1 + tag_len < len && chars[i + 1 + tag_len] == '$' {
                            let mut matches = true;
                            for (k, &tc) in tag_chars.iter().enumerate() {
                                if chars[i + 1 + k] != tc {
                                    matches = false;
                                    break;
                                }
                            }
                            if matches {
                                for _ in 0..=tag_len {
                                    i += 1;
                                    current.push(chars[i]);
                                }
                                dollar_quote_tag = None;
                            }
                        }
                    }
                }
                i += 1;
                continue;
            }

            if in_id_quote {
                current.push(c);
                if c == id_quote {
                    in_id_quote = false;
                }
                i += 1;
                continue;
            }

            match c {
                '\'' => {
                    in_single_quote = true;
                    current.push(c);
                }
                '"' => {
                    in_double_quote = true;
                    current.push(c);
                }
                '$' if dollar_quote => {
                    if i + 1 < len && chars[i + 1] == '$' {
                        dollar_quote_tag = Some(String::new());
                        current.push(c);
                        i += 1;
                        current.push(chars[i]);
                    } else if i + 1 < len
                        && (chars[i + 1].is_ascii_alphabetic() || chars[i + 1] == '_')
                    {
                        let mut j = i + 1;
                        while j < len && (chars[j].is_ascii_alphanumeric() || chars[j] == '_') {
                            j += 1;
                        }
                        if j < len && chars[j] == '$' {
                            let tag: String = chars[i + 1..j].iter().collect();
                            dollar_quote_tag = Some(tag);
                            current.extend(chars[i..=j].iter());
                            i = j;
                        } else {
                            current.push(c);
                        }
                    } else {
                        current.push(c);
                    }
                }
                c if c == id_quote => {
                    in_id_quote = true;
                    current.push(c);
                }
                '-' if i + 1 < len && chars[i + 1] == '-' => {
                    in_line_comment = true;
                    current.push(c);
                    i += 1;
                    current.push(chars[i]);
                }
                '#' if hash_comment => {
                    in_line_comment = true;
                    current.push(c);
                }
                '/' if i + 1 < len && chars[i + 1] == '*' => {
                    in_block_comment_depth = 1;
                    current.push(c);
                    i += 1;
                    current.push(chars[i]);
                }
                ';' => {
                    let trimmed = current.trim().to_string();
                    if !trimmed.is_empty() {
                        complete.push(trimmed);
                    }
                    current = String::new();
                }
                _ => {
                    current.push(c);
                }
            }
            i += 1;
        }

        SplitResult {
            complete,
            remainder: current.trim_start().to_string(),
        }
    }
}

// ─── rustyline Helper ──────────────────────────────────────────────────

#[derive(Completer, Helper, Hinter)]
struct SqlHelper {
    id_quote: char,
    hash_comment: bool,
    dollar_quote: bool,
}

impl Validator for SqlHelper {
    fn validate(&self, ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        let input = ctx.input();
        let trimmed = input.trim();
        if trimmed.is_empty() || trimmed.starts_with('.') || trimmed == "?" {
            return Ok(ValidationResult::Valid(None));
        }
        let split = SqlTokenizer::split_statements(
            input,
            self.id_quote,
            self.hash_comment,
            self.dollar_quote,
        );
        if split.remainder.trim().is_empty() {
            Ok(ValidationResult::Valid(None))
        } else {
            Ok(ValidationResult::Incomplete)
        }
    }
}

impl Highlighter for SqlHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        Cow::Borrowed(prompt)
    }
}

// ─── OutputTarget ──────────────────────────────────────────────────────

enum OutputTarget {
    Stdout,
    File(std::fs::File),
}

// ─── Dot commands ─────────────────────────────────────────────────────

struct DotAction {
    exit: bool,
}

struct ReplContext<'a> {
    history: &'a [String],
    output_target: &'a mut OutputTarget,
    last_result: &'a mut Option<QueryResult>,
    format: OutputFormat,
}

fn handle_dot_command(line: &str, ctx: &mut ReplContext) -> DotAction {
    let trimmed = line.trim();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.is_empty() {
        return DotAction { exit: false };
    }
    let cmd = parts[0].to_lowercase();

    match cmd.as_str() {
        ".help" | "?" => {
            println!(".help / ?            Show this help message");
            println!(".exit / .quit        Exit the REPL");
            println!(
                ".connect [<name>]    Reconnect (to <name>, or current connection if omitted)"
            );
            println!(".history             Show SQL execution history");
            println!(".clear / .cls        Clear the terminal screen");
            println!(".output [<file>]     Redirect SQL output to file, or back to stdout");
            println!(
                ".save <file> [fmt]   Save last query result to file (table/json/vertical/csv)"
            );
            DotAction { exit: false }
        }

        ".exit" | ".quit" => DotAction { exit: true },

        ".history" => {
            for (i, entry) in ctx.history.iter().enumerate() {
                let preview: String = entry
                    .chars()
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .collect();
                let preview = preview.trim();
                let display = if preview.chars().count() > 80 {
                    format!("{}...", preview.chars().take(79).collect::<String>())
                } else {
                    preview.to_string()
                };
                println!("{:4}  {}", i + 1, display);
            }
            DotAction { exit: false }
        }

        ".clear" | ".cls" => {
            let mut stdout = std::io::stdout();
            let _ = write!(stdout, "\x1b[2J\x1b[H");
            let _ = stdout.flush();
            DotAction { exit: false }
        }

        ".output" => {
            match parts.len() {
                1 => {
                    *ctx.output_target = OutputTarget::Stdout;
                    println!("output reset to stdout");
                }
                _ => {
                    let file_path = parts[1..].join(" ");
                    match std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&file_path)
                    {
                        Ok(file) => {
                            *ctx.output_target = OutputTarget::File(file);
                            println!("output redirected to {} (append)", file_path);
                        }
                        Err(e) => {
                            eprintln!("error: cannot open {}: {}", file_path, e);
                        }
                    }
                }
            }
            DotAction { exit: false }
        }

        ".save" => {
            if parts.len() < 2 {
                eprintln!("error: usage: .save <file> [format]");
                return DotAction { exit: false };
            }
            let (file_path, fmt) = if parts.len() >= 3 {
                match parts[parts.len() - 1].parse::<OutputFormat>() {
                    Ok(f) => (parts[1..parts.len() - 1].join(" "), f),
                    Err(_) => (parts[1..].join(" "), ctx.format),
                }
            } else {
                (parts[1].to_string(), ctx.format)
            };
            match ctx.last_result {
                None => {
                    eprintln!("error: no previous query result to save");
                }
                Some(result) => match std::fs::File::create(&file_path) {
                    Ok(mut file) => {
                        if let Err(e) = render_result(result, &mut file, fmt) {
                            eprintln!("error: {}", e);
                        } else {
                            let fmt_name = match fmt {
                                OutputFormat::Table => "table",
                                OutputFormat::Json => "json",
                                OutputFormat::Vertical => "vertical",
                                OutputFormat::Csv => "csv",
                            };
                            println!(
                                "saved {} row(s) to {} ({})",
                                result.row_count, file_path, fmt_name
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!("error: cannot create {}: {}", file_path, e);
                    }
                },
            }
            DotAction { exit: false }
        }

        _ => {
            eprintln!("error: unknown command '{}', type .help for list", line);
            DotAction { exit: false }
        }
    }
}

// ─── History helpers ──────────────────────────────────────────────────

const HISTORY_MAX_ENTRIES: usize = 1000;

fn sanitize_history_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    match s.as_str() {
        "" | "." | ".." => "default".to_string(),
        other => other.to_string(),
    }
}

fn history_path_for(connection_name: &str) -> Option<PathBuf> {
    let dir = dirs::data_local_dir()?.join("polar-mysql").join("history");
    Some(dir.join(sanitize_history_name(connection_name)))
}

// ─── REPL loop ─────────────────────────────────────────────────────────

const PROMPT: &str = "$ ";

fn print_banner(name: &str) {
    println!("hepta_dbcli interactive -- connected to '{}'", name);
    println!("end SQL with ';' + Enter to execute (multi-line ok) .help .connect .exit");
}

fn resolve_target(
    name: Option<&str>,
    raw: &crate::config::McpRawConfig,
    statement_timeout: Option<&str>,
    connection_max_lifetime: Option<&str>,
) -> Result<(crate::config::ResolvedConnection, TimeoutConfig), String> {
    let target_name = name.unwrap_or(&raw.default_name);
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
        statement_timeout,
        connection_max_lifetime,
        Some(&target.timeout_config),
    )
    .map_err(|e| format!("Invalid timeout configuration: {}", e))?;
    Ok((target, effective_timeout))
}

async fn connect(
    target: &crate::config::ResolvedConnection,
    effective_timeout: &TimeoutConfig,
    registry: &BackendRegistry,
) -> Result<Box<dyn DbConn + Send>, String> {
    let scheme = target
        .connection_url
        .find("://")
        .map(|i| &target.connection_url[..i])
        .unwrap_or("mysql");
    let pool = registry
        .connect_with_fallback(scheme, &target.connection_url, Some(effective_timeout))
        .await
        .map_err(|e| format!("Connection failed: {}", e))?;

    let conn = pool
        .acquire()
        .await
        .map_err(|e| format!("Failed to acquire connection: {}", e))?;

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

    Ok(conn)
}

// ─── REPL audit helpers (pure, unit-tested) ─────────────────────────

fn repl_sql_event(
    conn_name: &str,
    url: &str,
    sql: &str,
    duration_ms: u64,
    row_count: u64,
) -> DraftEvent {
    let class = if is_read_only_query(sql) {
        ActionClass::Dql
    } else {
        ActionClass::Dml
    };
    DraftEvent::new(
        Channel::Repl,
        ConnectionInfo::from_url(conn_name, url, read_only_session_for(url)),
        "repl_sql",
        class,
        Decision::Allow,
    )
    .with_sql(SqlInfo::new(sql))
    .with_outcome(AuditOutcome::ok(duration_ms).with_row_count(row_count))
}

fn repl_sql_error_event(
    conn_name: &str,
    url: &str,
    sql: &str,
    duration_ms: u64,
    error: &str,
) -> DraftEvent {
    let class = if is_read_only_query(sql) {
        ActionClass::Dql
    } else {
        ActionClass::Dml
    };
    DraftEvent::new(
        Channel::Repl,
        ConnectionInfo::from_url(conn_name, url, read_only_session_for(url)),
        "repl_sql",
        class,
        Decision::Error,
    )
    .with_sql(SqlInfo::new(sql))
    .with_outcome(AuditOutcome::error(duration_ms, error))
}

pub(crate) async fn run_interactive(
    args: CliArgs,
    registry: &BackendRegistry,
    audit: &AuditSession,
) -> Result<(), String> {
    let raw = read_config(args.config_path.map(PathBuf::from))?;

    let (mut target, effective_timeout) = resolve_target(
        args.connection_name.as_deref(),
        &raw,
        args.statement_timeout.as_deref(),
        args.connection_max_lifetime.as_deref(),
    )?;
    let mut conn = connect(&target, &effective_timeout, registry).await?;

    let mut rl = Editor::<SqlHelper, DefaultHistory>::new()
        .map_err(|e| format!("failed to init editor: {}", e))?;
    let id_quote = conn.dialect().identifier_quote();
    let hash_comment = conn.dialect().supports_hash_comment();
    let dollar_quote = conn.dialect().supports_dollar_quote();
    rl.set_helper(Some(SqlHelper {
        id_quote,
        hash_comment,
        dollar_quote,
    }));

    let _ = rl.set_max_history_size(HISTORY_MAX_ENTRIES);

    let mut history_path: Option<PathBuf> = if !args.no_history {
        match history_path_for(&target.name) {
            Some(p) => {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = rl.load_history(&p);
                Some(p)
            }
            None => None,
        }
    } else {
        None
    };

    print_banner(&target.name);

    let mut output_target = OutputTarget::Stdout;
    let mut last_result: Option<QueryResult> = None;
    let format = args.format;

    loop {
        let input = match rl.readline(PROMPT) {
            Ok(input) => input,
            Err(ReadlineError::Interrupted) => {
                continue;
            }
            Err(ReadlineError::Eof) => {
                println!();
                break;
            }
            Err(e) => {
                if let Some(p) = &history_path {
                    let _ = rl.save_history(p);
                }
                return Err(format!("readline error: {}", e));
            }
        };

        let trimmed = input.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Handle .connect command
        let mut connect_parts = trimmed.split_whitespace();
        let is_connect_cmd = connect_parts
            .next()
            .map(|c| c.eq_ignore_ascii_case(".connect"))
            .unwrap_or(false);
        if is_connect_cmd {
            let name_arg = connect_parts.next().unwrap_or("");
            let resolved_name = if name_arg.is_empty() {
                target.name.clone()
            } else {
                name_arg.to_string()
            };
            match resolve_target(
                Some(&resolved_name),
                &raw,
                args.statement_timeout.as_deref(),
                args.connection_max_lifetime.as_deref(),
            ) {
                Ok((new_target, new_timeout)) => {
                    match connect(&new_target, &new_timeout, registry).await {
                        Ok(new_conn) => {
                            // Save history for old connection
                            if target.name != new_target.name {
                                if let Some(p) = &history_path {
                                    let _ = rl.save_history(p);
                                }
                            }
                            target = new_target;
                            conn = new_conn;
                            if let Some(h) = rl.helper_mut() {
                                h.id_quote = conn.dialect().identifier_quote();
                                h.hash_comment = conn.dialect().supports_hash_comment();
                                h.dollar_quote = conn.dialect().supports_dollar_quote();
                            }
                            history_path = if !args.no_history {
                                history_path_for(&target.name)
                            } else {
                                None
                            };
                            if let Some(p) = &history_path {
                                if let Some(parent) = p.parent() {
                                    let _ = std::fs::create_dir_all(parent);
                                }
                                let _ = rl.clear_history();
                                let _ = rl.load_history(p);
                            }
                            print_banner(&target.name);
                        }
                        Err(e) => eprintln!("error: {}", e),
                    }
                }
                Err(e) => eprintln!("error: {}", e),
            }
            continue;
        }

        // Handle dot commands
        if trimmed.starts_with('.') || trimmed == "?" {
            let history_snapshot: Vec<String> = rl.history().iter().cloned().collect();
            let mut ctx = ReplContext {
                history: &history_snapshot,
                output_target: &mut output_target,
                last_result: &mut last_result,
                format,
            };
            let action = handle_dot_command(&input, &mut ctx);
            if action.exit {
                break;
            }
            continue;
        }

        let _ = rl.add_history_entry(&input);

        let split = SqlTokenizer::split_statements(
            &input,
            conn.dialect().identifier_quote(),
            conn.dialect().supports_hash_comment(),
            conn.dialect().supports_dollar_quote(),
        );
        for stmt in &split.complete {
            let start = Instant::now();
            let query_result = execute_query(&mut *conn, stmt).await;
            let duration_ms = start.elapsed().as_millis() as u64;
            match &query_result {
                Ok(qr) => audit.record_best_effort(repl_sql_event(
                    &target.name,
                    &target.connection_url,
                    stmt,
                    duration_ms,
                    qr.row_count as u64,
                )),
                Err(e) => audit.record_best_effort(repl_sql_error_event(
                    &target.name,
                    &target.connection_url,
                    stmt,
                    duration_ms,
                    e,
                )),
            }
            match query_result {
                Ok(query_result) => {
                    last_result = Some(query_result.clone());
                    match &mut output_target {
                        OutputTarget::Stdout => {
                            if let Err(e) =
                                render_result(&query_result, &mut std::io::stdout(), format)
                            {
                                eprintln!("render error: {}", e);
                            }
                        }
                        OutputTarget::File(f) => {
                            if let Err(e) = render_result(&query_result, f, format) {
                                eprintln!("render error: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("error: {}", e);
                    // Apply timeout_action on query error
                    if args.timeout_action.as_deref() == Some("disconnect") {
                        eprintln!("timeout_action=disconnect: reconnecting...");
                        if let Some(kill_sql) = conn.dialect().kill_own_connection_sql() {
                            let _ = conn.query_drop(&kill_sql).await;
                        }
                        match connect(&target, &effective_timeout, registry).await {
                            Ok(new_conn) => {
                                conn = new_conn;
                            }
                            Err(reconnect_err) => {
                                eprintln!("warning: reconnect failed: {}", reconnect_err);
                            }
                        }
                    }
                }
            }
        }

        if !split.complete.is_empty() {
            println!();
        }
    }

    if let Some(p) = &history_path {
        let _ = rl.save_history(p);
    }
    Ok(())
}

// ─── Unit Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_single_statement() {
        let r = SqlTokenizer::split_statements("SELECT 1;", '`', true, false);
        assert_eq!(r.complete, vec!["SELECT 1"]);
        assert_eq!(r.remainder, "");
    }

    #[test]
    fn test_split_multiple_statements() {
        let r = SqlTokenizer::split_statements("SELECT 1; SELECT 2;", '`', true, false);
        assert_eq!(r.complete, vec!["SELECT 1", "SELECT 2"]);
        assert_eq!(r.remainder, "");
    }

    #[test]
    fn test_split_semicolon_in_quotes() {
        let r = SqlTokenizer::split_statements("SELECT ';' AS a;", '`', true, false);
        assert_eq!(r.complete, vec!["SELECT ';' AS a"]);
        assert_eq!(r.remainder, "");
    }

    #[test]
    fn test_split_backtick_quoting() {
        let r = SqlTokenizer::split_statements("SELECT `;` AS a;", '`', true, false);
        assert_eq!(r.complete, vec!["SELECT `;` AS a"]);
        assert_eq!(r.remainder, "");
    }

    #[test]
    fn test_split_hash_comment() {
        let r = SqlTokenizer::split_statements("SELECT 1 # comment\n;", '`', true, false);
        assert_eq!(r.complete, vec!["SELECT 1 # comment"]);
        assert_eq!(r.remainder, "");
    }

    #[test]
    fn test_split_incomplete_no_semicolon() {
        let r = SqlTokenizer::split_statements("SELECT 'a'", '`', true, false);
        assert!(r.complete.is_empty());
        assert_eq!(r.remainder, "SELECT 'a'");
    }

    #[test]
    fn test_split_double_semicolons() {
        let r = SqlTokenizer::split_statements(";;", '`', true, false);
        assert!(r.complete.is_empty());
        assert_eq!(r.remainder, "");
    }

    #[test]
    fn test_sanitize_history_name() {
        assert_eq!(sanitize_history_name("prod"), "prod");
        assert_eq!(sanitize_history_name("prod/shard1"), "prod_shard1");
        assert_eq!(sanitize_history_name(""), "default");
        assert_eq!(sanitize_history_name("."), "default");
    }

    #[test]
    fn test_split_double_quote_identifier() {
        let r = SqlTokenizer::split_statements("SELECT \";\" AS a;", '"', false, false);
        assert_eq!(r.complete, vec!["SELECT \";\" AS a"]);
    }

    #[test]
    fn test_split_no_hash_comment() {
        let r =
            SqlTokenizer::split_statements("SELECT 1 # not comment\nFROM t;", '"', false, false);
        assert_eq!(r.complete, vec!["SELECT 1 # not comment\nFROM t"]);
    }

    #[test]
    fn test_split_dollar_quote_untagged() {
        let sql = "SELECT $$hello; world$$ AS msg;";
        let r = SqlTokenizer::split_statements(sql, '"', false, true);
        assert_eq!(r.complete, vec!["SELECT $$hello; world$$ AS msg"]);
        assert_eq!(r.remainder, "");
    }

    #[test]
    fn test_split_dollar_quote_tagged() {
        let sql = "SELECT $body$hello; world$body$ AS msg;";
        let r = SqlTokenizer::split_statements(sql, '"', false, true);
        assert_eq!(r.complete, vec!["SELECT $body$hello; world$body$ AS msg"]);
        assert_eq!(r.remainder, "");
    }

    #[test]
    fn test_split_dollar_quote_function_definition() {
        let sql = "CREATE FUNCTION foo() RETURNS void AS $$\nBEGIN\n  RAISE NOTICE 'hi';\nEND;\n$$ LANGUAGE plpgsql;";
        let r = SqlTokenizer::split_statements(sql, '"', false, true);
        assert_eq!(r.complete.len(), 1);
        assert_eq!(r.remainder, "");
        assert!(r.complete[0].contains("CREATE FUNCTION foo()"));
    }

    #[test]
    fn test_split_dollar_quote_incomplete() {
        let sql = "CREATE FUNCTION foo() AS $$\nBEGIN\n  SELECT 1;";
        let r = SqlTokenizer::split_statements(sql, '"', false, true);
        assert!(r.complete.is_empty());
        assert_eq!(r.remainder, sql);
    }

    #[test]
    fn test_split_positional_param_not_dollar_quote() {
        let r = SqlTokenizer::split_statements("SELECT $1, $2;", '"', false, true);
        assert_eq!(r.complete, vec!["SELECT $1, $2"]);
        assert_eq!(r.remainder, "");
    }

    #[test]
    fn test_split_dollar_quote_disabled_ignores() {
        let sql = "SELECT $$a$$;";
        let r = SqlTokenizer::split_statements(sql, '`', true, false);
        assert_eq!(r.complete, vec!["SELECT $$a$$"]);
        assert_eq!(r.remainder, "");
    }

    #[test]
    fn test_split_dollar_quote_mysql_has_no_dollar() {
        let sql = "SELECT $1, $2;";
        let r = SqlTokenizer::split_statements(sql, '`', true, false);
        assert_eq!(r.complete, vec!["SELECT $1, $2"]);
        assert_eq!(r.remainder, "");
    }

    #[test]
    fn repl_sql_event_records_channel_and_outcome() {
        let ev = repl_sql_event("dev", "mysql://u:p@h:3306/db", "SELECT 1", 5, 3);
        assert_eq!(ev.channel, Channel::Repl);
        assert_eq!(ev.action, "repl_sql");
        assert_eq!(ev.class, ActionClass::Dql);
        let outcome = ev.outcome.as_ref().unwrap();
        assert!(outcome.ok);
        assert_eq!(outcome.row_count, Some(3));

        let err = repl_sql_error_event("dev", "mysql://u:p@h:3306/db", "SELECT 1", 5, "boom");
        assert_eq!(err.decision, Decision::Error);
        assert_eq!(
            err.outcome.as_ref().unwrap().error_kind.as_deref(),
            Some("boom")
        );
    }
}
