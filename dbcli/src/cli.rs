use std::path::PathBuf;
use std::str::FromStr;

use serde_json::Value;
use tracing::{info, warn};

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

#[allow(dead_code)]
fn is_read_only_query(sql: &str) -> bool {
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
        writeln!(writer, "(0 rows)").map_err(|e| format!("write error: {}", e))?;
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

pub(crate) async fn run_cli(args: CliArgs, registry: &BackendRegistry) -> Result<(), String> {
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
    let pool = registry
        .connect_with_fallback(scheme, &target.connection_url, Some(&effective_timeout))
        .await
        .map_err(|e| format!("Connection failed: {}", e))?;

    let mut conn = pool
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

    let result = execute_query(&mut *conn, &sql).await?;
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
}
