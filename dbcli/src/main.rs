#![allow(dead_code)]

mod audit;
mod backend;
mod cli;
mod config;
mod connection;
mod delta_diff;
mod graph;
mod interactive;
mod logger;
mod output;
mod queries;
mod server;
#[cfg(feature = "synth")]
mod synth;
mod tabular;

use clap::{Parser, Subcommand};
use keyring::Entry;
use mysql_async::prelude::*;
use rmcp::{transport::stdio, ServiceExt};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{error, info, warn};

use crate::backend::factory::BackendRegistry;
use crate::backend::mysql::MySqlFactory;
use crate::backend::DbConn;
use crate::config::{
    default_config_path, read_config, resolve_all_connections_lazy, resolve_env_var_connection,
    resolve_single_connection, rewrite_password_to_sentinel, store_keyring_password,
    LazyConnectionEntry, PasswordSource, ResolvedConnection, KEYRING_SERVICE,
};
use crate::server::{format_error_chain, DbMcp};

// ─── CLI Structure ─────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "hepta_dbcli", version, about = concat!("CLI and MCP server for MySQL/PolarDB-X/Oracle database introspection — v", env!("CARGO_PKG_VERSION")),
    after_long_help = concat!(
        "CONFIGURATION:\n",
        "  Default config:  ~/.hepta-dbcli.toml (also reads legacy ~/.polardb-mysql.toml)\n",
        "  Environment var: HEPTA_DBCLI_URL=mysql://user:pass@host:port/database\n",
        "\n",
        "  Example config:\n",
        "    host = \"127.0.0.1\"\n",
        "    port = 3306\n",
        "    user = \"root\"\n",
        "    password = \"secret\"      # auto-migrated to OS keychain\n",
        "    database = \"mysql\"\n",
        "\n",
        "  Multi-connection (TOML sections):\n",
        "    default_connection = \"dev\"\n",
        "    [connections.dev]\n",
        "    host = \"127.0.0.1\"\n",
        "    user = \"root\"\n",
        "    password = \"keyring\"\n",
        "\n",
        "  Password stored via: hepta_dbcli store-password [--name <connection>]\n",
        "  Test connectivity:   hepta_dbcli check [--name <connection>] [--verbose]\n",
    ))]
struct Cli {
    /// Path to config file
    #[arg(long, global = true)]
    config: Option<String>,

    /// Target connection name
    #[arg(long, global = true)]
    name: Option<String>,

    /// Connection URL for ad-hoc use (e.g. duckdb:///tmp/shop.duckdb).
    /// Skips config file and keyring entirely; conflicts with --name.
    #[arg(long, global = true, conflicts_with = "name")]
    url: Option<String>,

    /// Directory for the JSONL audit log (default: <data-dir>/hepta-dbcli/audit)
    #[arg(long, global = true)]
    audit_dir: Option<String>,

    /// Also audit high-noise meta tools (list_tables, get_table_metadata, ...)
    #[arg(long, global = true)]
    audit_meta: bool,

    /// Audit log retention in days (0 = keep forever)
    #[arg(long, global = true, default_value_t = 30)]
    audit_retention_days: u32,

    /// Allow CLI/REPL data changes (INSERT/UPDATE/DELETE and CALL).
    /// Destructive DDL stays refused; MCP is unaffected and stays read-only.
    /// Pair this with a low-privilege database account.
    #[arg(long, global = true)]
    allow_write: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Run as MCP server (default when no subcommand given)
    Mcp,

    /// Test database connectivity and exit
    Check {
        /// Show detailed connection info
        #[arg(short, long)]
        verbose: bool,
    },

    /// Store password in OS keychain
    StorePassword {},

    /// Compare table data between two connections
    DeltaDiff {
        #[command(flatten)]
        args: Box<delta_diff::cmd::DeltaDiffArgs>,
    },

    /// Synthetic data generation (train / rules-draft / generate / validate)
    #[cfg(feature = "synth")]
    Synth {
        #[command(flatten)]
        args: Box<synth::cmd::SynthArgs>,
    },

    /// Execute SQL from command line
    Cli {
        /// SQL statement to execute
        #[arg(short, long)]
        sql: Option<String>,

        /// Read SQL from file
        #[arg(short, long)]
        file: Option<String>,

        /// Test database connectivity without executing SQL
        #[arg(long)]
        check_connection: bool,

        /// Show detailed connection info (use with --check-connection)
        #[arg(short, long)]
        verbose: bool,

        /// Output format: table, json, vertical, csv
        #[arg(long, default_value = "table")]
        format: String,

        /// Statement timeout (e.g. "30s", "5min"). Overrides config.
        #[arg(long)]
        statement_timeout: Option<String>,

        /// Connection max lifetime before reconnect (e.g. "10min").
        #[arg(long)]
        connection_max_lifetime: Option<String>,

        /// Enter interactive REPL mode
        #[arg(short, long)]
        interactive: bool,

        /// Do not read or write persistent per-connection SQL history
        #[arg(long)]
        no_history: bool,

        /// Timeout action: "cancel" (default, keep connection alive) or "disconnect" (recycle connection)
        #[arg(long)]
        timeout_action: Option<String>,
    },
}

// ─── Keyring helpers ───────────────────────────────────────────────────

fn check_keyring_available(username: &str) -> Result<(), String> {
    let test_key = "__polar_mysql_keyring_test__";
    let entry = Entry::new(KEYRING_SERVICE, username)
        .map_err(|e| format!("keyring entry creation failed: {}", e))?;
    entry
        .set_password(test_key)
        .map_err(|e| format!("keyring write failed: {}", e))?;
    let read_back = entry
        .get_password()
        .map_err(|e| format!("keyring read-back failed: {}", e))?;
    if read_back != test_key {
        return Err("keyring read-back mismatch".to_string());
    }
    Ok(())
}

fn read_password_secure() -> Result<String, String> {
    use std::io::IsTerminal;

    if std::io::stdin().is_terminal() {
        let pw1 = rpassword::prompt_password("Enter password: ")
            .map_err(|e| format!("failed to read password: {}", e))?;
        if pw1.is_empty() {
            return Err("password cannot be empty".to_string());
        }
        let pw2 = rpassword::prompt_password("Confirm password: ")
            .map_err(|e| format!("failed to read password: {}", e))?;
        if pw1 != pw2 {
            return Err("passwords do not match".to_string());
        }
        Ok(pw1)
    } else {
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("failed to read password from stdin: {}", e))?;
        let pw = input.trim_end_matches(['\r', '\n']).to_string();
        if pw.is_empty() {
            return Err("password from stdin cannot be empty".to_string());
        }
        Ok(pw)
    }
}

fn handle_store_password(
    name: Option<String>,
    config_path: Option<String>,
    audit: &audit::AuditSession,
) {
    let password = read_password_secure().unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(1);
    });

    let config_path = config_path
        .map(PathBuf::from)
        .or_else(default_config_path)
        .unwrap_or_else(|| {
            eprintln!("error: no config file specified and no default found");
            std::process::exit(1);
        });

    if !config_path.exists() {
        eprintln!("error: config file not found: {}", config_path.display());
        std::process::exit(1);
    }

    let content = std::fs::read_to_string(&config_path).unwrap_or_else(|e| {
        eprintln!("error: failed to read {}: {}", config_path.display(), e);
        std::process::exit(1);
    });

    let multi: config::MultiConfig = toml::from_str(&content).unwrap_or_else(|e| {
        eprintln!("error: failed to parse {}: {}", config_path.display(), e);
        std::process::exit(1);
    });

    // Resolve connections from config
    let connections = crate::config::resolve_named_connections(&multi);
    let default_name = multi
        .default_connection
        .clone()
        .or_else(|| connections.first().map(|c| c.name.clone()))
        .unwrap_or_else(|| "default".to_string());

    let target = if let Some(ref name) = name {
        connections
            .iter()
            .find(|c| c.name == *name)
            .unwrap_or_else(|| {
                eprintln!("error: connection '{}' not found in config", name);
                eprintln!(
                    "  available: {:?}",
                    connections.iter().map(|c| &c.name).collect::<Vec<_>>()
                );
                std::process::exit(1);
            })
    } else {
        let default = default_name.clone();
        connections
            .iter()
            .find(|c| c.name == default)
            .unwrap_or_else(|| {
                eprintln!(
                    "error: default connection '{}' not found in config",
                    default
                );
                std::process::exit(1);
            })
    };

    let keyring_user = target.keyring_username(Some(&config_path));

    // `--audit-meta`: record the action, never the password (issue #57 §5).
    let record = |decision| {
        if audit.meta_enabled() {
            audit.record_best_effort(cli::action_event(
                audit::event::Channel::Cli,
                "store_password",
                &target.name,
                "",
                audit::event::ActionClass::Meta,
                decision,
            ));
        }
    };

    if let Err(e) = store_keyring_password(&keyring_user, &password) {
        record(audit::event::Decision::Error);
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
    record(audit::event::Decision::Allow);

    println!(
        "Password stored in OS keychain for '{}' (connection: '{}').",
        keyring_user, target.name
    );
}

// ─── Connection Diagnostics ────────────────────────────────────────────

struct VerboseDetails {
    server_version: Option<String>,
    current_user: Option<String>,
    current_database: Option<String>,
    charset: Option<String>,
    collation: Option<String>,
    elapsed: Duration,
}

async fn query_verbose_details(
    url: &str,
    elapsed: Duration,
) -> Result<VerboseDetails, Box<dyn std::error::Error + Send + Sync>> {
    use crate::connection::do_connect;

    let (_pool, mut conn) = do_connect(url, None).await?;

    let row: mysql_async::Row = conn.query_first(queries::DATABASE_INFO).await?.unwrap();

    Ok(VerboseDetails {
        server_version: output::get_column_string(&row, 0),
        current_database: output::get_column_string(&row, 1),
        current_user: output::get_column_string(&row, 2),
        charset: output::get_column_string(&row, 6),
        collation: output::get_column_string(&row, 7),
        elapsed,
    })
}

fn print_verbose_details(details: &VerboseDetails) {
    eprintln!("  [verbose] Connection Details:");
    eprintln!(
        "    {:24} {}",
        "server_version",
        details.server_version.as_deref().unwrap_or("--")
    );
    eprintln!(
        "    {:24} {}",
        "current_user",
        details.current_user.as_deref().unwrap_or("--")
    );
    eprintln!(
        "    {:24} {}",
        "current_database",
        details.current_database.as_deref().unwrap_or("--")
    );
    eprintln!(
        "    {:24} {}",
        "charset",
        details.charset.as_deref().unwrap_or("--")
    );
    eprintln!(
        "    {:24} {}",
        "collation",
        details.collation.as_deref().unwrap_or("--")
    );
    eprintln!(
        "    {:24} {}ms",
        "connect_time",
        details.elapsed.as_millis()
    );
}

#[allow(dead_code)]
struct TlsCheckResult {
    mode: &'static str,
    success: bool,
    version: Option<String>,
    elapsed_ms: u128,
}

async fn try_connect_plain(
    url: &str,
) -> Result<(mysql_async::Conn, Duration), Box<dyn std::error::Error + Send + Sync>> {
    let start = Instant::now();
    let (_pool, conn) = connection::do_connect(url, None).await?;
    let elapsed = start.elapsed();
    Ok((conn, elapsed))
}

async fn try_connect_tls_skip_verify(
    url: &str,
) -> Result<(mysql_async::Conn, Duration), Box<dyn std::error::Error + Send + Sync>> {
    let separator = if url.contains('?') { "&" } else { "?" };
    let tls_url = format!(
        "{}?require_ssl=true&verify_ca=false&verify_identity=false",
        url
    );
    // If URL already has query params, replace the leading ? with &
    let tls_url = if separator == "&" {
        tls_url.replacen('?', "&", 1)
    } else {
        tls_url
    };
    let start = Instant::now();
    let (_pool, conn) = connection::do_connect(&tls_url, None).await?;
    let elapsed = start.elapsed();
    Ok((conn, elapsed))
}

async fn try_connect_tls_verify(
    url: &str,
) -> Result<(mysql_async::Conn, Duration), Box<dyn std::error::Error + Send + Sync>> {
    let separator = if url.contains('?') { "&" } else { "?" };
    let tls_url = format!("{}?require_ssl=true", url);
    let tls_url = if separator == "&" {
        tls_url.replacen('?', "&", 1)
    } else {
        tls_url
    };
    let start = Instant::now();
    let (_pool, conn) = connection::do_connect(&tls_url, None).await?;
    let elapsed = start.elapsed();
    Ok((conn, elapsed))
}

async fn do_oracle_check(resolved: &ResolvedConnection, registry: &BackendRegistry, verbose: bool) {
    let base_url = &resolved.connection_url;

    eprintln!("Connection: {}", resolved.name);
    eprintln!();
    print_password_status(resolved);

    eprintln!("[1/1] Connecting to Oracle ...");
    let start = Instant::now();
    match registry
        .connect_with_fallback("oracle", base_url, None, false)
        .await
    {
        Ok(pool) => {
            let elapsed = start.elapsed();
            match pool.acquire().await {
                Ok(mut conn) => {
                    let ver = {
                        let sql = conn.dialect().database_info().to_string();
                        conn.query(&sql).await.ok().and_then(|r| {
                            r.rows.first().and_then(|row| {
                                row.first().and_then(|v| v.as_str().map(|s| s.to_string()))
                            })
                        })
                    };
                    eprintln!(
                        "  ✓ Oracle  — {}ms  {}",
                        elapsed.as_millis(),
                        ver.as_deref().unwrap_or("(unknown)")
                    );
                    if verbose {
                        print_oracle_verbose(&mut *conn, elapsed).await;
                    }
                    migrate_password_if_needed(resolved);
                }
                Err(e) => {
                    eprintln!("  ✗ Oracle  — FAILED: {}", e);
                }
            }
        }
        Err(e) => {
            eprintln!("  ✗ Oracle  — FAILED: {}", e);
        }
    }
}

async fn print_oracle_verbose(conn: &mut dyn DbConn, elapsed: Duration) {
    let sql = { conn.dialect().database_info().to_string() };
    match conn.query(&sql).await {
        Ok(result) => {
            if let Some(row) = result.rows.first() {
                eprintln!("  [verbose] Connection Details:");
                eprintln!("    {:<24} {}", "server_version", col_str_or(row, 0));
                eprintln!("    {:<24} {}", "current_database", col_str_or(row, 1));
                eprintln!("    {:<24} {}", "current_user", col_str_or(row, 2));
                eprintln!("    {:<24} {}", "hostname", col_str_or(row, 3));
                eprintln!("    {:<24} {}", "os", col_str_or(row, 5));
                eprintln!("    {:<24} {}", "charset", col_str_or(row, 6));
                eprintln!("    {:<24} {}", "collation", col_str_or(row, 7));
                eprintln!("    {:<24} {}ms", "connect_time", elapsed.as_millis());
            }
        }
        Err(e) => eprintln!("  [verbose] Failed to get details: {}", e),
    }
}

fn col_str_or(row: &[serde_json::Value], idx: usize) -> &str {
    row.get(idx).and_then(|v| v.as_str()).unwrap_or("--")
}

fn print_password_status(resolved: &ResolvedConnection) {
    match resolved.password_source {
        PasswordSource::Keyring => {
            eprintln!(
                "[Keyring] Password read from OS keychain (user: {})",
                resolved.keyring_username
            );
            let entry_result = Entry::new(KEYRING_SERVICE, &resolved.keyring_username)
                .and_then(|e| e.get_password());
            match entry_result {
                Ok(pw) => {
                    if pw.is_empty() {
                        eprintln!("  WARNING: keyring returned empty password");
                    } else {
                        eprintln!(
                            "  Keyring accessible, password retrieved ({} chars)",
                            pw.len()
                        );
                    }
                }
                Err(e) => eprintln!(
                    "  Keyring read-back failed: {} (password may still be in old keyring entry -- migration pending)",
                    e
                ),
            }
            eprintln!();
        }
        PasswordSource::Plaintext(_) => {
            eprintln!("[Keyring] Password from config file (plaintext)");
            match check_keyring_available(&resolved.keyring_username) {
                Ok(()) => eprintln!("  OS keychain is available -- password will be migrated on first successful connection"),
                Err(e) => eprintln!("  OS keychain NOT available: {}", e),
            }
            eprintln!();
        }
        PasswordSource::EnvVar => {
            eprintln!("[Keyring] Password from environment variable (no keyring involved)");
            eprintln!();
        }
        PasswordSource::None => {
            eprintln!("[Keyring] No password configured");
            eprintln!();
        }
    }
}

fn migrate_password_if_needed(resolved: &ResolvedConnection) {
    if let (Some(path), Some(plaintext)) = (&resolved.config_path, &resolved.plaintext_password) {
        info!(
            "migrating plaintext password to OS keychain for '{}'",
            resolved.keyring_username
        );
        if let Err(e) = store_keyring_password(&resolved.keyring_username, plaintext) {
            warn!("failed to store password in keychain: {}", e);
        } else if let Err(e) = rewrite_password_to_sentinel(path, &resolved.name) {
            warn!("failed to update config file: {}", e);
        } else {
            info!(
                "password migrated to OS keychain for '{}'",
                resolved.keyring_username
            );
        }
    }
}

#[cfg(feature = "gaussdb")]
async fn do_gaussdb_check(
    resolved: &ResolvedConnection,
    registry: &BackendRegistry,
    verbose: bool,
) {
    let base_url = &resolved.connection_url;

    eprintln!("Connection: {}", resolved.name);
    eprintln!();
    print_password_status(resolved);

    eprintln!("[1/1] Connecting to GaussDB ...");
    let start = Instant::now();
    match registry
        .connect_with_fallback("gaussdb", base_url, None, false)
        .await
    {
        Ok(pool) => {
            let elapsed = start.elapsed();
            match pool.acquire().await {
                Ok(mut conn) => {
                    let ver = {
                        let sql = conn.dialect().database_info().to_string();
                        conn.query(&sql).await.ok().and_then(|r| {
                            r.rows.first().and_then(|row| {
                                row.first().and_then(|v| v.as_str().map(|s| s.to_string()))
                            })
                        })
                    };
                    eprintln!(
                        "  \u{2713} GaussDB  — {}ms  {}",
                        elapsed.as_millis(),
                        ver.as_deref().unwrap_or("(unknown)")
                    );
                    if verbose {
                        if let Ok(result) =
                            conn.query("SELECT version()::text, current_database()::text, current_user::text, inet_server_addr()::text, inet_server_port()::text")
                                .await
                        {
                            if let Some(row) = result.rows.first() {
                                eprintln!(
                                    "  Version    : {}",
                                    row[0].as_str().unwrap_or("(unknown)")
                                );
                                eprintln!(
                                    "  Database   : {}",
                                    row[1].as_str().unwrap_or("(unknown)")
                                );
                                eprintln!(
                                    "  User       : {}",
                                    row[2].as_str().unwrap_or("(unknown)")
                                );
                                eprintln!(
                                    "  Server     : {}:{}",
                                    row[3].as_str().unwrap_or("(unknown)"),
                                    row[4]
                                        .as_i64()
                                        .map(|p| p.to_string())
                                        .unwrap_or_else(|| "(unknown)".to_string())
                                );
                            }
                        }
                    }
                    migrate_password_if_needed(resolved);
                }
                Err(e) => {
                    eprintln!("  \u{2717} GaussDB  — FAILED to acquire: {}", e);
                    let msg = e.to_string();
                    for hint in gaussdb_failure_hints(&msg) {
                        eprintln!("    hint: {}", hint);
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("  \u{2717} GaussDB  — FAILED: {}", e);
            let msg = e.to_string();
            for hint in gaussdb_failure_hints(&msg) {
                eprintln!("    hint: {}", hint);
            }
        }
    }
}

#[cfg(feature = "gaussdb")]
fn gaussdb_failure_hints(err_msg: &str) -> Vec<&'static str> {
    let lower = err_msg.to_ascii_lowercase();
    let mut hints: Vec<&'static str> = Vec::new();
    if err_msg.contains("28P01") || lower.contains("password authentication failed") {
        hints.push(
            "password rejected: verify it, or re-run `hepta_dbcli store-password --name <connection>`",
        );
        hints.push(
            "keyring namespaces differ: hepta uses service `hepta-dbcli` (account `{name}#{hash}`), \
             the standalone `gaussdb` CLI uses `gaussdb`/`gaussdb-mcp` (account `{user}@{host}/{dbname}`) — they are not shared",
        );
    }
    if err_msg.contains("3D000") || err_msg.contains("3F000") {
        hints.push(
            "database does not exist: check `database` (alias `dbname`); do not set both fields",
        );
    }
    if lower.contains("tls") || lower.contains("ssl") {
        hints.push(
            "if the server has no TLS, set `sslmode = \"disable\"` in the connection section",
        );
    }
    if hints.is_empty() {
        hints.push("check host, port, user, password, and network reachability");
        hints.push("if the server has no TLS, set `sslmode = \"disable\"`");
    }
    hints
}

#[cfg(feature = "duckdb")]
async fn do_duckdb_check(resolved: &ResolvedConnection, registry: &BackendRegistry, verbose: bool) {
    let base_url = &resolved.connection_url;

    eprintln!("Connection: {}", resolved.name);
    eprintln!();
    print_password_status(resolved);

    eprintln!("[1/1] Opening DuckDB database ...");
    let start = Instant::now();
    match registry
        .connect_with_fallback("duckdb", base_url, None, false)
        .await
    {
        Ok(pool) => {
            let elapsed = start.elapsed();
            match pool.acquire().await {
                Ok(mut conn) => {
                    let ver = {
                        let sql = conn.dialect().database_info().to_string();
                        conn.query(&sql).await.ok().and_then(|r| {
                            r.rows.first().and_then(|row| {
                                row.first().and_then(|v| v.as_str().map(|s| s.to_string()))
                            })
                        })
                    };
                    eprintln!(
                        "  \u{2713} DuckDB  — {}ms  {}",
                        elapsed.as_millis(),
                        ver.as_deref().unwrap_or("(unknown)")
                    );
                    if verbose {
                        if let Ok(result) = conn.query("SELECT current_database()").await {
                            if let Some(row) = result.rows.first() {
                                eprintln!(
                                    "  Database   : {}",
                                    row[0].as_str().unwrap_or("(unknown)")
                                );
                                eprintln!("  Mode       : embedded (no server process)");
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("  \u{2717} DuckDB  — FAILED to acquire: {}", e);
                    let msg = e.to_string();
                    for hint in duckdb_failure_hints(&msg) {
                        eprintln!("    hint: {}", hint);
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("  \u{2717} DuckDB  — FAILED: {}", e);
            let msg = e.to_string();
            for hint in duckdb_failure_hints(&msg) {
                eprintln!("    hint: {}", hint);
            }
        }
    }
}

#[cfg(feature = "duckdb")]
fn duckdb_failure_hints(err_msg: &str) -> Vec<&'static str> {
    let lower = err_msg.to_ascii_lowercase();
    let mut hints: Vec<&'static str> = Vec::new();
    if lower.contains("not found") {
        hints.push(
            "database file does not exist; DuckDB never creates files implicitly — check the `database` path",
        );
    }
    if lower.contains("lock") {
        hints.push(
            "file is locked by another process (single-writer): close the other DuckDB instance, or open with ?mode=ro",
        );
    }
    if hints.is_empty() {
        hints.push("verify the `database` path and that the file is a valid DuckDB database");
    }
    hints
}

async fn handle_check_connection(
    resolved: &ResolvedConnection,
    verbose: bool,
    registry: &BackendRegistry,
) {
    let scheme = resolved
        .connection_url
        .find("://")
        .map(|i| &resolved.connection_url[..i])
        .unwrap_or("mysql");

    if scheme == "oracle" {
        do_oracle_check(resolved, registry, verbose).await;
        return;
    }

    #[cfg(feature = "gaussdb")]
    if scheme == "gaussdb" {
        do_gaussdb_check(resolved, registry, verbose).await;
        return;
    }

    #[cfg(feature = "duckdb")]
    if scheme == "duckdb" {
        do_duckdb_check(resolved, registry, verbose).await;
        return;
    }

    // ── MySQL path (existing TLS probing) ──
    let base_url = &resolved.connection_url;

    print_password_status(resolved);

    let mut results: Vec<TlsCheckResult> = Vec::new();

    // Pass 1: No TLS
    eprintln!("[1/3] Connecting without TLS (plain TCP) ...");
    match try_connect_plain(base_url).await {
        Ok((mut conn, elapsed)) => {
            let ver: Option<String> = match conn.query_first("SELECT VERSION()").await {
                Ok(Some(v)) => Some(v),
                _ => None,
            };
            eprintln!(
                "  ✓ NoTls  — {}ms  {}",
                elapsed.as_millis(),
                ver.as_deref().unwrap_or("(unknown)")
            );
            results.push(TlsCheckResult {
                mode: "NoTls",
                success: true,
                version: ver,
                elapsed_ms: elapsed.as_millis(),
            });
            if verbose {
                match query_verbose_details(base_url, elapsed).await {
                    Ok(details) => print_verbose_details(&details),
                    Err(e) => eprintln!("  [verbose] Failed to get details: {}", e),
                }
            }
        }
        Err(e) => {
            let chain = format_error_chain(e.as_ref());
            eprintln!("  ✗ NoTls  — FAILED: {}", chain);
            results.push(TlsCheckResult {
                mode: "NoTls",
                success: false,
                version: None,
                elapsed_ms: 0,
            });
        }
    }

    // Migrate plaintext password to keychain on first success
    if results.iter().any(|r| r.success) {
        if let (Some(path), Some(plaintext)) = (&resolved.config_path, &resolved.plaintext_password)
        {
            info!(
                "migrating plaintext password to OS keychain for '{}'",
                resolved.keyring_username
            );
            if let Err(e) = store_keyring_password(&resolved.keyring_username, plaintext) {
                warn!("failed to store password in keychain: {}", e);
            } else if let Err(e) = rewrite_password_to_sentinel(path, &resolved.name) {
                warn!("failed to update config file: {}", e);
            } else {
                info!(
                    "password migrated to OS keychain for '{}'",
                    resolved.keyring_username
                );
            }
        }
    }

    // Pass 2: TLS with skip verify
    eprintln!("[2/3] Connecting with TLS (skip cert verify) ...");
    match try_connect_tls_skip_verify(base_url).await {
        Ok((mut conn, elapsed)) => {
            let ver: Option<String> = match conn.query_first("SELECT VERSION()").await {
                Ok(Some(v)) => Some(v),
                _ => None,
            };
            eprintln!(
                "  ✓ TLS(skip-verify)  — {}ms  {}",
                elapsed.as_millis(),
                ver.as_deref().unwrap_or("(unknown)")
            );
            results.push(TlsCheckResult {
                mode: "TLS-skip-verify",
                success: true,
                version: ver,
                elapsed_ms: elapsed.as_millis(),
            });
        }
        Err(e) => {
            let chain = format_error_chain(e.as_ref());
            eprintln!("  ✗ TLS(skip-verify)  — FAILED: {}", chain);
            results.push(TlsCheckResult {
                mode: "TLS-skip-verify",
                success: false,
                version: None,
                elapsed_ms: 0,
            });
        }
    }

    // Pass 3: TLS with verify
    eprintln!("[3/3] Connecting with TLS (verify cert) ...");
    match try_connect_tls_verify(base_url).await {
        Ok((mut conn, elapsed)) => {
            let ver: Option<String> = match conn.query_first("SELECT VERSION()").await {
                Ok(Some(v)) => Some(v),
                _ => None,
            };
            eprintln!(
                "  ✓ TLS(verify)  — {}ms  {}",
                elapsed.as_millis(),
                ver.as_deref().unwrap_or("(unknown)")
            );
            results.push(TlsCheckResult {
                mode: "TLS-verify",
                success: true,
                version: ver,
                elapsed_ms: elapsed.as_millis(),
            });
        }
        Err(e) => {
            let chain = format_error_chain(e.as_ref());
            eprintln!("  ✗ TLS(verify)  — FAILED: {}", chain);
            results.push(TlsCheckResult {
                mode: "TLS-verify",
                success: false,
                version: None,
                elapsed_ms: 0,
            });
        }
    }

    eprintln!();

    // Summary
    let any_success = results.iter().any(|r| r.success);
    if any_success {
        let working = results.iter().find(|r| r.success).unwrap();
        eprintln!("  ✓ Connection successful (mode: {})", working.mode);
        if let Some(ref ver) = working.version {
            eprintln!("  Database Version: {}", ver);
        }
        eprintln!();
        if working.mode != "NoTls" {
            eprintln!("  Recommendation: use ssl-mode in your config URL.");
            eprintln!("    Example: ?ssl-mode=REQUIRED");
        }
    } else {
        eprintln!("  ✗ All connection methods failed.");
        eprintln!();
        eprintln!("  Possible causes:");
        eprintln!("  - Database server is not running or not reachable");
        eprintln!("  - Firewall blocking port");
        eprintln!("  - Wrong host, port, user, or password");
        std::process::exit(1);
    }
}

async fn handle_check_connection_cmd(
    conn_arg: Option<String>,
    inline_url: Option<String>,
    verbose: bool,
    config_path: Option<PathBuf>,
    registry: &BackendRegistry,
    audit: &audit::AuditSession,
) {
    // --url short-circuits config resolution (same priority as cli/REPL).
    if let Some(u) = inline_url {
        let resolved =
            crate::config::resolve_inline_url_connection_result(&u).unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(1);
            });
        if audit.meta_enabled() {
            audit.record_best_effort(cli::action_event(
                audit::event::Channel::Cli,
                "check",
                &resolved.name,
                &resolved.connection_url,
                audit::event::ActionClass::Meta,
                audit::event::Decision::Allow,
            ));
        }
        handle_check_connection(&resolved, verbose, registry).await;
        return;
    }

    let raw = read_config(config_path).unwrap_or_else(|e| {
        eprintln!("error: {}", e);
        std::process::exit(1);
    });

    let target_name = conn_arg.as_deref().unwrap_or(&raw.default_name);

    let target_conn = match raw.connections.iter().find(|c| c.name == target_name) {
        Some(c) => c,
        None => {
            eprintln!("error: connection '{}' not found", target_name);
            eprintln!(
                "  available: {:?}",
                raw.connections.iter().map(|c| &c.name).collect::<Vec<_>>()
            );
            std::process::exit(1);
        }
    };

    let resolved = if raw.is_env_var {
        resolve_env_var_connection(target_conn.url.clone().unwrap())
    } else {
        resolve_single_connection(
            target_conn,
            raw.config_path.clone(),
            raw.base_timeout.as_ref(),
        )
        .unwrap_or_else(|e| {
            eprintln!("error: {}", e);
            std::process::exit(1);
        })
    };

    // `--audit-meta`: record the action (issue #57 §5). `check` never carries
    // credentials; the probe output itself stays on the terminal.
    if audit.meta_enabled() {
        audit.record_best_effort(cli::action_event(
            audit::event::Channel::Cli,
            "check",
            &resolved.name,
            &resolved.connection_url,
            audit::event::ActionClass::Meta,
            audit::event::Decision::Allow,
        ));
    }

    handle_check_connection(&resolved, verbose, registry).await;
}

// ─── Process Lifecycle Helpers ─────────────────────────────────────────

async fn await_shutdown_signal() -> &'static str {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                tokio::select! {
                    _ = ctrl_c => "SIGINT",
                    _ = sigterm.recv() => "SIGTERM",
                }
            }
            Err(e) => {
                warn!("failed to install SIGTERM handler: {e}, relying on SIGINT only");
                let _ = ctrl_c.await;
                "SIGINT"
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
        "SIGINT"
    }
}

#[cfg(unix)]
async fn parent_death_watchdog(interval: std::time::Duration) {
    unsafe extern "C" {
        fn getppid() -> i32;
    }
    let original_ppid = unsafe { getppid() };
    loop {
        tokio::time::sleep(interval).await;
        let current_ppid = unsafe { getppid() };
        if current_ppid != original_ppid {
            info!(
                "parent process exited (PPID {} -> {}), initiating self-shutdown",
                original_ppid, current_ppid
            );
            return;
        }
    }
}

#[cfg(not(unix))]
async fn parent_death_watchdog(_interval: std::time::Duration) {
    std::future::pending::<()>().await;
}

// ─── Backend Registry ────────────────────────────────────────────────

fn create_registry() -> BackendRegistry {
    let mut registry = BackendRegistry::new();
    registry.register(Arc::new(MySqlFactory));
    #[cfg(feature = "oracle-rs")]
    registry.register(Arc::new(crate::backend::oracle::OracleFactory));
    #[cfg(feature = "oracle")]
    registry.register(Arc::new(crate::backend::oracle_native::OracleFactory));
    #[cfg(feature = "gaussdb")]
    registry.register(Arc::new(crate::backend::gaussdb::GaussdbFactory));
    #[cfg(feature = "duckdb")]
    registry.register(Arc::new(crate::backend::duckdb::DuckDbFactory));
    registry
}

// ─── MCP Server ────────────────────────────────────────────────────────

async fn run_mcp_server(
    config_path: Option<String>,
    registry: Arc<BackendRegistry>,
    audit: Arc<audit::AuditSession>,
) {
    let config_path_buf = config_path.map(PathBuf::from);
    let export_root = crate::config::load_delta_diff_export_root(config_path_buf.as_deref())
        .unwrap_or_else(std::env::temp_dir);

    let (lazy_entries, default_name) =
        resolve_all_connections_lazy(config_path_buf.clone()).unwrap_or_else(|e| {
            // Only "no config requested and none on disk" degrades to an
            // empty connection table (inline-URL tools like delta_diff
            // left_url/right_url still work). An explicit --config (broken
            // toml, unreadable, missing path) must fail closed: MCP clients
            // rarely surface stderr, so a silent fail-open would turn every
            // named-connection tool call into a confusing per-call
            // unknown_connection error.
            if config_path_buf.is_none()
                && config::no_config_file_exists()
                && std::env::var_os("HEPTA_DBCLI_URL").is_none()
            {
                eprintln!(
                    "warning: no connection configuration found; only inline-URL tools (delta_diff left_url/right_url) are available"
                );
                (Vec::new(), "default".to_string())
            } else {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        });

    let mut eager_entries = Vec::new();
    let mut lazy_resolvers = Vec::new();

    for entry in lazy_entries {
        match entry {
            LazyConnectionEntry::Ready(resolved) => {
                eager_entries.push((resolved.name, Some(resolved.connection_url)));
            }
            LazyConnectionEntry::Pending { name, resolver, .. } => {
                lazy_resolvers.push((name, resolver));
            }
        }
    }

    let server = if !eager_entries.is_empty() && lazy_resolvers.is_empty() {
        DbMcp::new(
            Arc::clone(&registry),
            eager_entries,
            default_name,
            Arc::clone(&audit),
        )
    } else if !lazy_resolvers.is_empty() {
        let all_lazy = eager_entries
            .into_iter()
            .map(|(name, url)| {
                let url = url.unwrap_or_default();
                (
                    name,
                    Arc::new(move || Ok(url.clone()))
                        as Arc<dyn (Fn() -> Result<String, String>) + Send + Sync>,
                )
            })
            .chain(lazy_resolvers)
            .collect();
        DbMcp::new_with_lazy(
            Arc::clone(&registry),
            Vec::new(),
            all_lazy,
            default_name,
            Arc::clone(&audit),
        )
    } else {
        DbMcp::new_empty(Arc::clone(&registry), default_name, Arc::clone(&audit))
    };

    let server = Arc::new(server.with_export_root(export_root));

    tokio::spawn(async {
        let sig = await_shutdown_signal().await;
        info!("received {sig}, shutting down");
        std::process::exit(0);
    });
    tokio::spawn(async {
        parent_death_watchdog(std::time::Duration::from_secs(5)).await;
        std::process::exit(0);
    });

    let probe = Arc::clone(&server);
    tokio::spawn(async move {
        probe.try_connect().await;
    });

    info!("starting MCP server on stdio");

    let service = match Arc::clone(&server).serve(stdio()).await {
        Ok(s) => s,
        Err(e) => {
            error!("MCP server start failed: {e}");
            std::process::exit(1);
        }
    };

    info!("MCP server ready");

    match service.waiting().await {
        Ok(reason) => info!("MCP server stopped: {reason:?}"),
        Err(e) => error!("MCP server task join error: {e}"),
    }

    info!("MCP server exiting");
    std::process::exit(0);
}

// ─── Entry Point ───────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    logger::init_logging();

    let cli = Cli::parse();
    let registry = Arc::new(create_registry());

    let audit_config = audit::AuditConfig {
        dir: cli.audit_dir.as_deref().map(PathBuf::from),
        // The ledger is not optional (issue #57 review). An unwritable
        // directory degrades with a stderr warning; it is never switched off.
        enabled: true,
        fsync: false,
        meta: cli.audit_meta,
        retention_days: cli.audit_retention_days,
    };

    match cli.command {
        None | Some(Commands::Mcp) => {
            if cli.allow_write {
                eprintln!(
                    "error: --allow-write is not valid for the MCP server; MCP execute_query stays read-only"
                );
                std::process::exit(2);
            }
            if cli.url.is_some() {
                eprintln!(
                    "error: --url is not supported by the MCP server; use delta_diff left_url/right_url tool arguments instead"
                );
                std::process::exit(2);
            }
            let audit = Arc::new(audit::AuditSession::new(&audit_config));
            run_mcp_server(cli.config, Arc::clone(&registry), audit).await;
        }
        Some(Commands::Check { verbose }) => {
            let config_path = cli.config.map(PathBuf::from);
            let audit = audit::AuditSession::new(&audit_config);
            handle_check_connection_cmd(cli.name, cli.url, verbose, config_path, &registry, &audit)
                .await;
        }
        Some(Commands::StorePassword {}) => {
            if cli.url.is_some() {
                eprintln!(
                    "error: --url is not supported by store-password; it needs a named connection from the config file"
                );
                std::process::exit(2);
            }
            let audit = audit::AuditSession::new(&audit_config);
            handle_store_password(cli.name, cli.config, &audit);
        }
        Some(Commands::DeltaDiff { args }) => {
            if cli.url.is_some() {
                eprintln!(
                    "error: --url is not supported by delta-diff; use --left-url / --right-url per side"
                );
                std::process::exit(2);
            }
            let audit = audit::AuditSession::new(&audit_config);
            let code = delta_diff::run(*args, cli.config, &audit).await;
            std::process::exit(code);
        }
        #[cfg(feature = "synth")]
        Some(Commands::Synth { args }) => {
            if cli.url.is_some() {
                eprintln!(
                    "error: --url is not supported by synth; add the connection to the config file or set HEPTA_DBCLI_URL"
                );
                std::process::exit(2);
            }
            let audit = audit::AuditSession::new(&audit_config);
            let code = synth::run(*args, cli.config, &audit).await;
            std::process::exit(code);
        }
        Some(Commands::Cli {
            sql,
            file,
            check_connection,
            verbose,
            format,
            statement_timeout,
            connection_max_lifetime,
            interactive,
            no_history,
            timeout_action,
        }) => {
            if check_connection {
                let config_path = cli.config.map(PathBuf::from);
                let audit = audit::AuditSession::new(&audit_config);
                handle_check_connection_cmd(
                    cli.name,
                    cli.url,
                    verbose,
                    config_path,
                    &registry,
                    &audit,
                )
                .await;
            } else if interactive {
                let fmt: cli::OutputFormat = format.parse().unwrap_or(cli::OutputFormat::Table);
                let args = cli::CliArgs {
                    sql,
                    file,
                    connection_name: cli.name,
                    url: cli.url,
                    config_path: cli.config,
                    format: fmt,
                    statement_timeout,
                    connection_max_lifetime,
                    no_history,
                    timeout_action,
                    allow_write: cli.allow_write,
                };
                let audit = audit::AuditSession::new(&audit_config);
                if let Err(e) = interactive::run_interactive(args, &registry, &audit).await {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            } else {
                let fmt: cli::OutputFormat = format.parse().unwrap_or(cli::OutputFormat::Table);
                let args = cli::CliArgs {
                    sql,
                    file,
                    connection_name: cli.name,
                    url: cli.url,
                    config_path: cli.config,
                    format: fmt,
                    statement_timeout,
                    connection_max_lifetime,
                    no_history,
                    timeout_action,
                    allow_write: cli.allow_write,
                };
                let audit = audit::AuditSession::new(&audit_config);
                if let Err(e) = cli::run_cli(args, &registry, &audit).await {
                    eprintln!("error: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}

#[cfg(all(test, feature = "gaussdb"))]
mod gaussdb_hint_tests {
    use super::gaussdb_failure_hints;

    #[test]
    fn hint_for_bad_password() {
        let hints = gaussdb_failure_hints(
            "GaussDB connect failed: [SQLSTATE 28P01] password authentication failed",
        );
        assert!(hints.iter().any(|h| h.contains("store-password")));
        // password auth failure reached the server; TLS was never the problem
        assert!(!hints.iter().any(|h| h.contains("sslmode")));
    }

    #[test]
    fn hint_for_missing_database() {
        let hints = gaussdb_failure_hints("[SQLSTATE 3D000] database \"mydb\" does not exist");
        assert!(hints.iter().any(|h| h.contains("database does not exist")));
    }

    #[test]
    fn hint_for_missing_role_is_not_database() {
        let hints = gaussdb_failure_hints("FATAL: role \"foo\" does not exist");
        assert!(!hints.iter().any(|h| h.contains("database does not exist")));
    }

    #[test]
    fn hint_for_tls_failure() {
        let hints = gaussdb_failure_hints("GaussDB connect failed: error performing TLS handshake");
        assert!(hints.iter().any(|h| h.contains("sslmode")));
    }

    #[test]
    fn hint_fallback_non_empty() {
        let hints =
            gaussdb_failure_hints("GaussDB connect failed: error communicating with the server");
        assert!(!hints.is_empty());
    }
}

#[cfg(test)]
mod inline_url_tests {
    use super::Cli;
    use clap::Parser;

    fn parse(argv: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(argv)
    }

    #[test]
    fn global_url_flag_parses_for_cli_subcommand() {
        let cli = parse(&[
            "hepta_dbcli",
            "--url",
            "duckdb:///tmp/shop.duckdb",
            "cli",
            "--sql",
            "SELECT 1",
        ])
        .expect("global --url should parse");
        assert_eq!(cli.url.as_deref(), Some("duckdb:///tmp/shop.duckdb"));
    }

    #[test]
    fn global_url_reaches_delta_diff_subcommand() {
        let cli = parse(&[
            "hepta_dbcli",
            "--url",
            "duckdb://:memory:",
            "delta-diff",
            "--left-url",
            "duckdb://:memory:",
            "--right-url",
            "duckdb://:memory:",
            "--table",
            "t",
        ])
        .expect("--url should parse before subcommand");
        assert_eq!(cli.url.as_deref(), Some("duckdb://:memory:"));
    }

    #[test]
    fn url_conflicts_with_name() {
        let err = parse(&[
            "hepta_dbcli",
            "--url",
            "duckdb://:memory:",
            "--name",
            "prod",
            "check",
        ])
        .expect_err("--url and --name must conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
