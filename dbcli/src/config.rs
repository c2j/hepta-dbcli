use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use crate::backend::{default_port_for_scheme, ssl_url_param_for_scheme};

// ─── Constants ─────────────────────────────────────────────────────────

pub(crate) const KEYRING_SERVICE: &str = "hepta-dbcli";
pub(crate) const OLD_KEYRING_SERVICE: &str = "polar-mysql";
pub(crate) const KEYRING_SENTINEL: &str = "keyring";
pub(crate) const DEFAULT_CONFIG_FILENAME: &str = "hepta-dbcli.toml";
pub(crate) const OLD_DEFAULT_CONFIG_FILENAME: &str = "polardb-mysql.toml";
pub(crate) const ENV_VAR_URL: &str = "HEPTA_DBCLI_URL";

// ─── Keyring account helpers ────────────────────────────────────────

/// Compute a stable 8-char hex hash from the canonical config path.
/// Uses djb2 hash (deterministic across platforms and Rust versions).
fn config_path_hash(config_path: Option<&Path>) -> String {
    let path_str = config_path
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let hash = path_str
        .bytes()
        .fold(5381u64, |h, b| h.wrapping_mul(33).wrapping_add(b as u64));
    format!("{:08x}", hash as u32)
}

// ─── Password Source ───────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) enum PasswordSource {
    None,
    Plaintext(String),
    Keyring,
    EnvVar,
}

// ─── Timeout Config ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TimeoutConfig {
    pub statement_timeout: Option<Duration>,
    pub connection_max_lifetime: Option<Duration>,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            statement_timeout: Some(Duration::from_secs(30)),
            connection_max_lifetime: Some(Duration::from_secs(3600)),
        }
    }
}

impl TimeoutConfig {
    pub(crate) fn from_overrides(
        statement_timeout: Option<&str>,
        connection_max_lifetime: Option<&str>,
        base: Option<&TimeoutConfig>,
    ) -> Result<Self, String> {
        let default_tc = TimeoutConfig::default();
        let base = base.unwrap_or(&default_tc);

        let statement_timeout = match statement_timeout {
            Some(s) => {
                Some(parse_duration(s).map_err(|e| format!("Invalid statement_timeout: {}", e))?)
            }
            None => base.statement_timeout,
        };

        let connection_max_lifetime = match connection_max_lifetime {
            Some(s) => Some(
                parse_duration(s).map_err(|e| format!("Invalid connection_max_lifetime: {}", e))?,
            ),
            None => base.connection_max_lifetime,
        };

        Ok(Self {
            statement_timeout,
            connection_max_lifetime,
        })
    }
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim().to_lowercase();
    if let Some(v) = s.strip_suffix("ms") {
        let ms: u64 = v.parse().map_err(|_| format!("invalid number: {}", v))?;
        Ok(Duration::from_millis(ms))
    } else if let Some(v) = s.strip_suffix('s') {
        let secs: f64 = v.parse().map_err(|_| format!("invalid number: {}", v))?;
        Ok(Duration::from_secs_f64(secs))
    } else if let Some(v) = s.strip_suffix("min") {
        let mins: f64 = v.parse().map_err(|_| format!("invalid number: {}", v))?;
        Ok(Duration::from_secs_f64(mins * 60.0))
    } else if let Some(v) = s.strip_suffix('h') {
        let hours: f64 = v.parse().map_err(|_| format!("invalid number: {}", v))?;
        Ok(Duration::from_secs_f64(hours * 3600.0))
    } else {
        // Try plain number as seconds
        let secs: f64 = s
            .parse()
            .map_err(|_| format!("cannot parse '{}': expected e.g. 30s, 5min, 2h, 500ms", s))?;
        Ok(Duration::from_secs_f64(secs))
    }
}

// ─── Named Connection (TOML model) ────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct NamedConnection {
    #[serde(default)]
    pub name: String,
    pub url: Option<String>,
    pub driver: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    #[serde(alias = "dbname")]
    pub database: Option<String>,
    /// Default schema for tools that take a `--schema` (synth train/
    /// rules-draft/report). Explicit CLI flags still win; unset falls back to
    /// the driver probe (`current_schema()` / URL database / user name).
    #[serde(default)]
    pub schema: Option<String>,
    pub sslmode: Option<String>,
    pub statement_timeout: Option<String>,
    pub connection_max_lifetime: Option<String>,
}

impl NamedConnection {
    pub(crate) fn keyring_username(&self, config_path: Option<&Path>) -> String {
        format!("{}#{}", self.name, config_path_hash(config_path))
    }

    pub(crate) fn old_keyring_username(&self) -> String {
        format!("{}/{}", self.user.as_deref().unwrap_or("root"), self.name)
    }
}

// ─── MultiConfig (TOML model) ─────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct MultiConfig {
    #[serde(default)]
    pub default_connection: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<String>,
    pub database: Option<String>,
    pub sslmode: Option<String>,
    pub statement_timeout: Option<String>,
    pub connection_max_lifetime: Option<String>,
    #[serde(default)]
    pub connections: Option<std::collections::HashMap<String, NamedConnection>>,
    /// Root directory confining MCP `delta_diff` export/checkpoint writes.
    /// Unset means the system temp directory.
    #[serde(default)]
    pub delta_diff_export_root: Option<String>,
}

// ─── Resolved Connection ──────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct ResolvedConnection {
    pub name: String,
    pub connection_url: String,
    pub password_source: PasswordSource,
    pub keyring_username: String,
    pub config_path: Option<PathBuf>,
    pub plaintext_password: Option<String>,
    pub timeout_config: TimeoutConfig,
    /// Connection-level default schema (`[connections.X] schema = "..."`).
    /// `None` for env-var connections and legacy constructors.
    pub default_schema: Option<String>,
}

// ─── Lazy Connection Entry ───────────────────────────────────────────

pub(crate) enum LazyConnectionEntry {
    Ready(ResolvedConnection),
    Pending {
        name: String,
        resolver: Arc<dyn (Fn() -> Result<String, String>) + Send + Sync>,
        timeout_config: TimeoutConfig,
    },
}

// ─── McpRawConfig ─────────────────────────────────────────────────────

pub(crate) struct McpRawConfig {
    pub connections: Vec<NamedConnection>,
    pub default_name: String,
    pub config_path: Option<PathBuf>,
    pub base_timeout: Option<TimeoutConfig>,
    pub is_env_var: bool,
}

impl McpRawConfig {
    /// Empty connection table for inline-URL paths (`--url`, per-side
    /// `--left-url` / `--right-url`): no toml, no keyring, no env var.
    pub(crate) fn empty() -> Self {
        Self {
            connections: Vec::new(),
            default_name: "default".to_string(),
            config_path: None,
            base_timeout: None,
            is_env_var: false,
        }
    }
}

// ─── Config Helpers ───────────────────────────────────────────────────

pub(crate) fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|p| p.join(format!(".{}", DEFAULT_CONFIG_FILENAME)))
}

/// True when neither the new (`.{DEFAULT_CONFIG_FILENAME}`) nor the legacy
/// (`.{OLD_DEFAULT_CONFIG_FILENAME}`) default config exists under `home`.
/// Used by the MCP bootstrap to distinguish "no config at all" (degradable
/// to an empty connection table) from "config exists but is broken"
/// (fail closed with exit 1).
pub(crate) fn no_config_in_home(home: Option<&Path>) -> bool {
    let Some(home) = home else {
        return true;
    };
    let new_cfg = home.join(format!(".{}", DEFAULT_CONFIG_FILENAME));
    if new_cfg.exists() {
        return false;
    }
    let old_cfg = home.join(format!(".{}", OLD_DEFAULT_CONFIG_FILENAME));
    !old_cfg.exists()
}

/// `no_config_in_home` against the real user home.
pub(crate) fn no_config_file_exists() -> bool {
    no_config_in_home(dirs::home_dir().as_deref())
}

pub(crate) fn find_config_path(opt: Option<PathBuf>) -> Result<PathBuf, String> {
    match opt {
        Some(p) => Ok(p),
        None => {
            if let Some(ref p) = default_config_path() {
                if p.exists() {
                    return Ok(p.clone());
                }
            }
            if let Some(old_path) =
                dirs::home_dir().map(|p| p.join(format!(".{}", OLD_DEFAULT_CONFIG_FILENAME)))
            {
                if old_path.exists() {
                    return Ok(old_path);
                }
            }
            Err(format!(
                "No connection configuration found. Use one of:\n\
                 \n\
                 1. Set {env} environment variable\n\
                    export {env}=\"mysql://user:password@host:port/database\"\n\
                 \n\
                 2. Create ~/.{name} config file:\n\
                    host = \"127.0.0.1\"\n\
                    user = \"root\"\n\
                    password = \"secret\"\n\
                    database = \"mysql\"\n\
                 \n\
                 3. Pass --config <path> to specify a config file\n\
                 \n\
                 Password will be migrated to OS keychain on first successful connection.",
                env = ENV_VAR_URL,
                name = DEFAULT_CONFIG_FILENAME
            ))
        }
    }
}

/// Read the configured `delta_diff_export_root` from the same config file the
/// connection resolver loads. Returns `None` when the file is missing, has no
/// such key, or cannot be parsed (the caller falls back to the system temp dir).
pub(crate) fn load_delta_diff_export_root(config_path: Option<&Path>) -> Option<PathBuf> {
    let path = find_config_path(config_path.map(Path::to_path_buf)).ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    let multi: MultiConfig = toml::from_str(&content).ok()?;
    multi.delta_diff_export_root.map(PathBuf::from)
}

pub(crate) fn read_config(config_path: Option<PathBuf>) -> Result<McpRawConfig, String> {
    if let Ok(url) = std::env::var(ENV_VAR_URL) {
        let conn = NamedConnection {
            name: "default".to_string(),
            url: Some(url),
            driver: None,
            host: None,
            port: None,
            user: None,
            password: None,
            database: None,
            schema: None,
            sslmode: None,
            statement_timeout: None,
            connection_max_lifetime: None,
        };
        return Ok(McpRawConfig {
            connections: vec![conn],
            default_name: "default".to_string(),
            config_path: None,
            base_timeout: None,
            is_env_var: true,
        });
    }

    let config_path = find_config_path(config_path)?;
    let content = std::fs::read_to_string(&config_path).map_err(|e| {
        format!(
            "failed to read config file {}: {}",
            config_path.display(),
            e
        )
    })?;

    let multi: MultiConfig = toml::from_str(&content).map_err(|e| {
        format!(
            "failed to parse config file {}: {}",
            config_path.display(),
            e
        )
    })?;

    let base_tc = TimeoutConfig::from_overrides(
        multi.statement_timeout.as_deref(),
        multi.connection_max_lifetime.as_deref(),
        None,
    )
    .ok();

    let connections = resolve_named_connections(&multi);
    let default_name = multi
        .default_connection
        .clone()
        .or_else(|| connections.first().map(|c| c.name.clone()))
        .unwrap_or_else(|| "default".to_string());

    Ok(McpRawConfig {
        connections,
        default_name,
        config_path: Some(config_path),
        base_timeout: base_tc,
        is_env_var: false,
    })
}

pub(crate) fn resolve_named_connections(multi: &MultiConfig) -> Vec<NamedConnection> {
    if let Some(ref conns) = multi.connections {
        let mut result: Vec<NamedConnection> = conns
            .iter()
            .map(|(name, conn)| NamedConnection {
                name: name.clone(),
                url: conn.url.clone(),
                driver: conn.driver.clone(),
                host: conn.host.clone(),
                port: conn.port,
                user: conn.user.clone(),
                password: conn.password.clone(),
                database: conn.database.clone(),
                schema: conn.schema.clone(),
                sslmode: conn.sslmode.clone(),
                statement_timeout: conn
                    .statement_timeout
                    .clone()
                    .or(multi.statement_timeout.clone()),
                connection_max_lifetime: conn
                    .connection_max_lifetime
                    .clone()
                    .or(multi.connection_max_lifetime.clone()),
            })
            .collect();

        // If there are top-level fields and no named connection matches them,
        // also create a default from top-level
        if multi.host.is_some() || multi.user.is_some() {
            let has_named_default = result.iter().any(|c| c.name == "default");
            if !has_named_default {
                result.push(NamedConnection {
                    name: "default".to_string(),
                    url: None,
                    driver: None,
                    host: multi.host.clone(),
                    port: multi.port,
                    user: multi.user.clone(),
                    password: multi.password.clone(),
                    database: multi.database.clone(),
                    schema: None,
                    sslmode: multi.sslmode.clone(),
                    statement_timeout: multi.statement_timeout.clone(),
                    connection_max_lifetime: multi.connection_max_lifetime.clone(),
                });
            }
        }

        result
    } else {
        // Single connection from top-level fields
        vec![NamedConnection {
            name: "default".to_string(),
            url: None,
            driver: None,
            host: multi.host.clone(),
            port: multi.port,
            user: multi.user.clone(),
            password: multi.password.clone(),
            database: multi.database.clone(),
            schema: None,
            sslmode: multi.sslmode.clone(),
            statement_timeout: multi.statement_timeout.clone(),
            connection_max_lifetime: multi.connection_max_lifetime.clone(),
        }]
    }
}

// ─── URL Building ────────────────────────────────────────────────────

fn build_mysql_url(
    host: &str,
    port: u16,
    user: &str,
    password: Option<&str>,
    database: Option<&str>,
    sslmode: Option<&str>,
) -> String {
    build_db_url("mysql", host, port, user, password, database, sslmode)
}

fn build_db_url(
    scheme: &str,
    host: &str,
    port: u16,
    user: &str,
    password: Option<&str>,
    database: Option<&str>,
    sslmode: Option<&str>,
) -> String {
    let encoded_user = urlencode(user);
    let auth_part = match password {
        Some(pw) => format!("{}:{}@", encoded_user, urlencode(pw)),
        None => format!("{}@", encoded_user),
    };
    let db_part = match database {
        Some(db) => format!("/{}", db),
        None => String::new(),
    };
    let ssl_part = match sslmode {
        Some(mode) if scheme.eq_ignore_ascii_case("gaussdb") => gaussdb_ssl_url_param(mode),
        Some(mode)
            if mode.eq_ignore_ascii_case("require")
                || mode.eq_ignore_ascii_case("required")
                || mode.eq_ignore_ascii_case("true")
                || mode.eq_ignore_ascii_case("1")
                || mode.eq_ignore_ascii_case("yes") =>
        {
            ssl_url_param_for_scheme(scheme)
        }
        _ => "",
    };
    format!(
        "{}://{}{}:{}{}{}",
        scheme, auth_part, host, port, db_part, ssl_part
    )
}

fn gaussdb_ssl_url_param(mode: &str) -> &'static str {
    match mode.to_ascii_lowercase().as_str() {
        "disable" | "disabled" | "false" | "0" | "no" | "none" => "?sslmode=disable",
        "require" | "required" | "true" | "1" | "yes" => "?sslmode=require",
        "verify-ca" | "verify_ca" => "?sslmode=verify-ca",
        "verify-full" | "verify_full" => "?sslmode=verify-full",
        _ => "",
    }
}

pub(crate) fn is_duckdb_driver(driver: Option<&str>) -> bool {
    driver.is_some_and(|d| d.eq_ignore_ascii_case("duckdb"))
}

/// DuckDB is embedded: `database` holds the file path or `:memory:`.
/// No host/port/user/password is involved.
pub(crate) fn build_duckdb_url(database: Option<&str>) -> Result<String, String> {
    match database.map(str::trim) {
        Some(db) if !db.is_empty() => Ok(format!("duckdb://{db}")),
        _ => {
            Err("duckdb connection requires `database` to be a file path or ':memory:'".to_string())
        }
    }
}

fn urlencode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'~' => {
                result.push(b as char);
            }
            b' ' => result.push_str("%20"),
            _ => {
                result.push_str(&format!("%{:02X}", b));
            }
        }
    }
    result
}

// ─── Keyring Operations ──────────────────────────────────────────────

pub(crate) fn store_keyring_password(username: &str, password: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, username)
        .map_err(|e| format!("keyring entry creation failed: {}", e))?;
    entry
        .set_password(password)
        .map_err(|e| format!("failed to store password in keychain: {}", e))
}

pub(crate) fn read_keyring_password(
    new_username: &str,
    old_username: &str,
) -> Result<String, String> {
    let new_result = try_read_keyring(KEYRING_SERVICE, new_username);
    if let Ok(pw) = new_result {
        return Ok(pw);
    }
    let old_result = try_read_keyring(OLD_KEYRING_SERVICE, old_username);
    if let Ok(pw) = old_result {
        if let Err(e) = store_keyring_password(new_username, &pw) {
            warn!(
                "keyring migration failed for '{}': {} (password still usable from old entry)",
                new_username, e
            );
        }
        return Ok(pw);
    }
    let last_err = old_result
        .err()
        .or_else(|| new_result.err())
        .unwrap_or_default();
    Err(format!(
        "keyring password not found for '{}'. Store it first:\n  \
         hepta_dbcli store-password --name <connection>\n  \
         or set password in config file as plaintext (will be migrated automatically).\n  \
         (also checked old keyring entry '{}' — not found; keyring error: {})",
        new_username, old_username, last_err
    ))
}

fn try_read_keyring(service: &str, username: &str) -> Result<String, String> {
    let entry = keyring::Entry::new(service, username)
        .map_err(|e| format!("keyring entry creation failed: {}", e))?;
    entry
        .get_password()
        .map_err(|e| format!("keyring read failed: {}", e))
}

// ─── Resolve Single Connection ───────────────────────────────────────

pub(crate) fn resolve_single_connection(
    conn: &NamedConnection,
    config_path: Option<PathBuf>,
    base_tc: Option<&TimeoutConfig>,
) -> Result<ResolvedConnection, String> {
    let is_duckdb = is_duckdb_driver(conn.driver.as_deref());

    let url = if let Some(ref u) = conn.url {
        u.clone()
    } else if is_duckdb {
        build_duckdb_url(conn.database.as_deref())?
    } else {
        let host = conn
            .host
            .as_deref()
            .ok_or_else(|| format!("connection '{}' has no host or url", conn.name))?;
        let scheme = conn.driver.as_deref().unwrap_or("mysql");
        let port = conn.port.unwrap_or(default_port_for_scheme(scheme));
        let user = conn
            .user
            .as_deref()
            .ok_or_else(|| format!("connection '{}' has no user or url", conn.name))?;
        let password = conn.password.as_deref();
        let database = conn.database.as_deref();
        let sslmode = conn.sslmode.as_deref();
        build_db_url(scheme, host, port, user, password, database, sslmode)
    };

    let password_source = if is_duckdb {
        // Embedded DB: no authentication. user/password fields (including
        // the keyring sentinel) are ignored, so no keyring or env lookup.
        PasswordSource::None
    } else {
        match conn.password.as_deref() {
            Some(p) if p == KEYRING_SENTINEL => PasswordSource::Keyring,
            Some(p) => PasswordSource::Plaintext(p.to_string()),
            None => {
                // Check env var
                if let Ok(_pw) = std::env::var("HEPTA_DBCLI_PASSWORD") {
                    PasswordSource::EnvVar
                } else {
                    PasswordSource::None
                }
            }
        }
    };

    let plaintext_password = match &password_source {
        PasswordSource::Plaintext(p) => Some(p.clone()),
        _ => None,
    };

    let driver_scheme = conn.driver.as_deref().unwrap_or("mysql");

    // If password source is keyring, resolve it now
    let connection_url = if matches!(password_source, PasswordSource::Keyring) {
        let pw = read_keyring_password(
            &conn.keyring_username(config_path.as_deref()),
            &conn.old_keyring_username(),
        )?;
        if let Some(ref u) = conn.url {
            replace_password_in_url(u, &pw)
        } else {
            let host = conn.host.as_deref().unwrap();
            let port = conn.port.unwrap_or(default_port_for_scheme(driver_scheme));
            let user = conn.user.as_deref().unwrap();
            let database = conn.database.as_deref();
            let sslmode = conn.sslmode.as_deref();
            build_db_url(
                driver_scheme,
                host,
                port,
                user,
                Some(&pw),
                database,
                sslmode,
            )
        }
    } else if matches!(password_source, PasswordSource::EnvVar) {
        let pw = std::env::var("HEPTA_DBCLI_PASSWORD").unwrap_or_default();
        if let Some(ref u) = conn.url {
            inject_password_into_url(u, &pw)?
        } else {
            let host = conn.host.as_deref().unwrap();
            let port = conn.port.unwrap_or(default_port_for_scheme(driver_scheme));
            let user = conn.user.as_deref().unwrap();
            let database = conn.database.as_deref();
            let sslmode = conn.sslmode.as_deref();
            build_db_url(
                driver_scheme,
                host,
                port,
                user,
                Some(&pw),
                database,
                sslmode,
            )
        }
    } else {
        url
    };

    let timeout_config = TimeoutConfig::from_overrides(
        conn.statement_timeout.as_deref(),
        conn.connection_max_lifetime.as_deref(),
        base_tc,
    )?;

    Ok(ResolvedConnection {
        name: conn.name.clone(),
        connection_url,
        password_source,
        keyring_username: conn.keyring_username(config_path.as_deref()),
        config_path,
        plaintext_password,
        timeout_config,
        default_schema: conn.schema.clone(),
    })
}

fn replace_password_in_url(url: &str, new_password: &str) -> String {
    // Legacy best-effort wrapper: URLs without userinfo are returned unchanged.
    inject_password_into_url(url, new_password).unwrap_or_else(|_| url.to_string())
}

/// Inject (or replace) the password inside a URL's userinfo component.
///
/// Returns an explicit error when the URL has no userinfo (no `@`), because a
/// password cannot be attached in that case — silently returning a
/// password-less URL would hide a misconfiguration.
pub(crate) fn inject_password_into_url(url: &str, password: &str) -> Result<String, String> {
    let Some(at_pos) = url.find('@') else {
        return Err(format!(
            "cannot inject password: URL '{}' has no userinfo (expected scheme://user@host)",
            url
        ));
    };
    // Start of the credentials portion (after scheme://), if any.
    let cred_start = url[..at_pos].find("://").map(|i| i + 3).unwrap_or(0);
    let encoded = urlencode(password);
    // Look for a colon in the credentials portion (not in the scheme).
    if let Some(colon_pos) = url[cred_start..at_pos].rfind(':') {
        let abs_colon = cred_start + colon_pos;
        return Ok(format!(
            "{}:{}@{}",
            &url[..abs_colon],
            encoded,
            &url[at_pos + 1..]
        ));
    }
    // No colon before @: add the password.
    Ok(format!(
        "{}:{}@{}",
        &url[..at_pos],
        encoded,
        &url[at_pos + 1..]
    ))
}

/// True when the URL already carries a password in its userinfo (`user:pw@`).
fn url_userinfo_has_password(url: &str) -> bool {
    match url.find('@') {
        Some(at_pos) => {
            let cred_start = url[..at_pos].find("://").map(|i| i + 3).unwrap_or(0);
            url[cred_start..at_pos].contains(':')
        }
        None => false,
    }
}

pub(crate) fn resolve_env_var_connection(url: String) -> Result<ResolvedConnection, String> {
    let env_password = std::env::var("HEPTA_DBCLI_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty());
    resolve_env_var_connection_inner(url, env_password)
}

/// Testable seam for [`resolve_env_var_connection`]: the environment password
/// is passed in explicitly so tests never depend on the process environment.
fn resolve_env_var_connection_inner(
    url: String,
    env_password: Option<String>,
) -> Result<ResolvedConnection, String> {
    let connection_url = match env_password {
        Some(pw) if !url_userinfo_has_password(&url) => inject_password_into_url(&url, &pw)?,
        _ => url,
    };
    let timeout_config = TimeoutConfig::default();
    Ok(ResolvedConnection {
        name: "default".to_string(),
        connection_url,
        password_source: PasswordSource::EnvVar,
        keyring_username: format!("default#{}", config_path_hash(None)),
        config_path: None,
        plaintext_password: None,
        timeout_config,
        default_schema: None,
    })
}

/// Resolve a connection given directly as a URL (e.g. `--url`, delta-diff
/// `--left-url/--right-url`, MCP `left_url/right_url`). The URL is used as-is;
/// no keyring, env-var or config lookup happens. Embedded databases (DuckDB)
/// carry no credentials at all; for user:pass URLs the credentials stay inside
/// the URL (audit redaction handles DSN scrubbing).
pub(crate) fn resolve_inline_url_connection_result(
    url: &str,
) -> Result<ResolvedConnection, String> {
    let trimmed = url.trim();
    if trimmed.is_empty() || !trimmed.contains("://") {
        return Err(format!(
            "invalid connection URL '{}': expected scheme://... (e.g. duckdb:///tmp/shop.duckdb)",
            url
        ));
    }
    let scheme = trimmed.split("://").next().unwrap_or_default();
    Ok(ResolvedConnection {
        name: format!("inline-{}", scheme.to_lowercase()),
        connection_url: trimmed.to_string(),
        password_source: PasswordSource::None,
        keyring_username: format!(
            "inline-{}#{}",
            scheme.to_lowercase(),
            config_path_hash(None)
        ),
        config_path: None,
        plaintext_password: None,
        timeout_config: TimeoutConfig::default(),
        default_schema: None,
    })
}

/// Panicking convenience wrapper for tests and call sites that already
/// validated the URL shape.
pub(crate) fn resolve_inline_url_connection(url: &str) -> ResolvedConnection {
    resolve_inline_url_connection_result(url).expect("valid inline connection URL")
}

// ─── Lazy Resolver ───────────────────────────────────────────────────

pub(crate) fn build_lazy_resolver(
    conn: &NamedConnection,
    config_path: Option<PathBuf>,
    base_timeout: Option<&TimeoutConfig>,
) -> Result<LazyConnectionEntry, String> {
    let is_sentinel = conn.password.as_deref() == Some(KEYRING_SENTINEL);
    let is_plaintext = conn
        .password
        .as_ref()
        .is_some_and(|p| p != KEYRING_SENTINEL);

    // A password supplied via HEPTA_DBCLI_PASSWORD needs no laziness: reading
    // the environment is cheap, so resolve eagerly (issue #115).
    let env_password_present = std::env::var("HEPTA_DBCLI_PASSWORD")
        .ok()
        .is_some_and(|p| !p.is_empty());

    // DuckDB is embedded and has no keyring dependency — resolve eagerly.
    if is_plaintext
        || conn.url.is_some()
        || is_duckdb_driver(conn.driver.as_deref())
        || (conn.password.is_none() && env_password_present)
    {
        let resolved = resolve_single_connection(conn, config_path, base_timeout)?;
        return Ok(LazyConnectionEntry::Ready(resolved));
    }

    let password_source = if is_sentinel {
        PasswordSource::Keyring
    } else {
        PasswordSource::None
    };

    let host = conn.host.clone();
    let port = conn.port;
    let user = conn.user.clone();
    let database = conn.database.clone();
    let sslmode = conn.sslmode.clone();
    let driver = conn.driver.clone();
    let name = conn.name.clone();
    let name_clone = conn.name.clone();
    let keyring_user = conn.keyring_username(config_path.as_deref());
    let old_keyring_user = conn.old_keyring_username();

    let resolver = Arc::new(move || {
        let password = match password_source {
            PasswordSource::Keyring => {
                Some(read_keyring_password(&keyring_user, &old_keyring_user)?)
            }
            PasswordSource::None => None,
            _ => {
                return Err(format!(
                    "connection '{}' has an unsupported password source",
                    name_clone
                ))
            }
        };

        let host = host
            .as_deref()
            .ok_or_else(|| format!("connection '{}' has no host or url", name_clone))?;
        let scheme = driver.as_deref().unwrap_or("mysql");
        let port = port.unwrap_or(default_port_for_scheme(scheme));
        let user = user
            .as_deref()
            .ok_or_else(|| format!("connection '{}' has no user or url", name_clone))?;
        let database = database.as_deref();
        let sslmode = sslmode.as_deref();

        Ok(build_db_url(
            scheme,
            host,
            port,
            user,
            password.as_deref(),
            database,
            sslmode,
        ))
    });

    let timeout_config = TimeoutConfig::from_overrides(
        conn.statement_timeout.as_deref(),
        conn.connection_max_lifetime.as_deref(),
        base_timeout,
    )?;

    Ok(LazyConnectionEntry::Pending {
        name,
        resolver,
        timeout_config,
    })
}

pub(crate) fn resolve_all_connections_lazy(
    config_path: Option<PathBuf>,
) -> Result<(Vec<LazyConnectionEntry>, String), String> {
    if let Ok(url) = std::env::var(ENV_VAR_URL) {
        let resolved = resolve_env_var_connection(url)?;
        return Ok((
            vec![LazyConnectionEntry::Ready(resolved)],
            "default".to_string(),
        ));
    }

    let raw = read_config(config_path)?;
    let mut entries = Vec::with_capacity(raw.connections.len());
    for conn in &raw.connections {
        entries.push(build_lazy_resolver(
            conn,
            raw.config_path.clone(),
            raw.base_timeout.as_ref(),
        )?);
    }

    Ok((entries, raw.default_name))
}

pub(crate) fn rewrite_password_to_sentinel(
    config_path: &Path,
    connection_name: &str,
) -> Result<(), String> {
    let content = std::fs::read_to_string(config_path)
        .map_err(|e| format!("failed to read config: {}", e))?;

    // Parse as TOML and modify
    let mut value: toml::Value = content
        .parse()
        .map_err(|e| format!("failed to parse config: {}", e))?;

    let mut modified = false;

    if connection_name == "default" || connection_name.is_empty() {
        if let Some(password) = value.get_mut("password") {
            *password = toml::Value::String(KEYRING_SENTINEL.to_string());
            modified = true;
        }
    } else if let Some(connections) = value.get_mut("connections") {
        if let Some(conn) = connections.get_mut(connection_name) {
            if let Some(password) = conn.get_mut("password") {
                *password = toml::Value::String(KEYRING_SENTINEL.to_string());
                modified = true;
            }
        }
    }

    if modified {
        let new_content =
            toml::to_string(&value).map_err(|e| format!("failed to serialize config: {}", e))?;
        std::fs::write(config_path, new_content)
            .map_err(|e| format!("failed to write config: {}", e))?;
        Ok(())
    } else {
        Err(format!(
            "could not find password field for connection '{}'",
            connection_name
        ))
    }
}

// ─── Unit Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_config_in_home_detects_missing_new_and_legacy_files() {
        // 空目录：新旧两个默认文件名都不存在 => true。
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(
            no_config_in_home(Some(tmp.path())),
            "empty home => no default config file"
        );

        // 新默认文件存在 => false。
        let new_cfg = tmp.path().join(format!(".{}", DEFAULT_CONFIG_FILENAME));
        std::fs::write(&new_cfg, "[connections.dev]\nhost = \"h\"\n").expect("write");
        assert!(
            !no_config_in_home(Some(tmp.path())),
            "default config exists"
        );

        // 只剩旧默认文件（≤0.2.7 迁移场景）=> false。
        std::fs::remove_file(&new_cfg).expect("cleanup");
        let old_cfg = tmp.path().join(format!(".{}", OLD_DEFAULT_CONFIG_FILENAME));
        std::fs::write(&old_cfg, "[connections.dev]\nhost = \"h\"\n").expect("write");
        assert!(!no_config_in_home(Some(tmp.path())), "legacy config exists");

        // 无 home（罕见环境）=> 按缺配置处理。
        assert!(no_config_in_home(None), "no home => treat as no config");
    }

    #[test]
    fn named_connection_accepts_schema_field() {
        // 已知限制 #2 修复：TOML `[connections.X]` 支持 `schema` 字段，
        // 未显式传 --schema 时作为默认 schema 使用（不再被静默忽略）。
        let toml = r#"
[connections.dev]
host = "127.0.0.1"
user = "root"
database = "db"
schema = "staging"
"#;
        let cfg: MultiConfig = toml::from_str(toml).expect("parse");
        let conn = cfg.connections.as_ref().unwrap().get("dev").unwrap();
        assert_eq!(conn.schema.as_deref(), Some("staging"));
    }

    #[test]
    fn named_connection_without_schema_still_parses() {
        let toml = r#"
[connections.dev]
host = "127.0.0.1"
user = "root"
database = "db"
"#;
        let cfg: MultiConfig = toml::from_str(toml).expect("parse");
        let conn = cfg.connections.as_ref().unwrap().get("dev").unwrap();
        assert!(conn.schema.is_none());
    }

    #[test]
    fn test_build_url_no_password() {
        let url = build_mysql_url("127.0.0.1", 3306, "root", None, Some("mysql"), None);
        assert_eq!(url, "mysql://root@127.0.0.1:3306/mysql");
    }

    #[test]
    fn test_build_url_with_password() {
        let url = build_mysql_url(
            "127.0.0.1",
            3306,
            "mcp",
            Some("pass"),
            Some("prototype"),
            None,
        );
        assert_eq!(url, "mysql://mcp:pass@127.0.0.1:3306/prototype");
    }

    #[test]
    fn test_build_url_ssl_require() {
        let url = build_mysql_url("127.0.0.1", 3306, "root", None, None, Some("require"));
        assert!(url.contains("?ssl-mode=REQUIRED"));
    }

    #[test]
    fn test_build_url_special_chars() {
        let url = build_mysql_url("127.0.0.1", 3306, "user", Some("p@ss:w0rd"), None, None);
        assert_eq!(url, "mysql://user:p%40ss%3Aw0rd@127.0.0.1:3306");
    }

    #[test]
    fn test_build_duckdb_url_from_database_path() {
        let url = build_duckdb_url(Some("/data/shop.duckdb")).unwrap();
        assert_eq!(url, "duckdb:///data/shop.duckdb");
    }

    #[test]
    fn test_build_duckdb_url_memory() {
        let url = build_duckdb_url(Some(":memory:")).unwrap();
        assert_eq!(url, "duckdb://:memory:");
    }

    #[test]
    fn test_build_duckdb_url_requires_database() {
        assert!(build_duckdb_url(None).is_err());
        assert!(build_duckdb_url(Some("")).is_err());
        assert!(build_duckdb_url(Some("  ")).is_err());
    }

    #[test]
    fn test_resolve_inline_url_connection_keeps_url_and_skips_credentials() {
        let resolved = resolve_inline_url_connection("duckdb:///tmp/shop.duckdb");
        assert_eq!(resolved.connection_url, "duckdb:///tmp/shop.duckdb");
        assert!(matches!(resolved.password_source, PasswordSource::None));
        assert_eq!(resolved.plaintext_password, None);
        assert_eq!(resolved.default_schema, None);
    }

    #[test]
    fn test_resolve_inline_url_connection_names_side_from_scheme() {
        let resolved = resolve_inline_url_connection("mysql://u:p@127.0.0.1:3306/db");
        assert_eq!(resolved.name, "inline-mysql");
        assert_eq!(resolved.connection_url, "mysql://u:p@127.0.0.1:3306/db");
    }

    #[test]
    fn test_resolve_inline_url_connection_rejects_scheme_less_string() {
        assert!(resolve_inline_url_connection_result("/tmp/shop.duckdb").is_err());
        assert!(resolve_inline_url_connection_result("").is_err());
    }

    #[test]
    fn test_resolve_duckdb_connection_needs_no_host_or_user() {
        let conn = NamedConnection {
            name: "duck".into(),
            url: None,
            driver: Some("duckdb".into()),
            host: None,
            port: None,
            user: None,
            password: Some("ignored".into()),
            database: Some("/tmp/shop.duckdb".into()),
            schema: None,
            sslmode: None,
            statement_timeout: None,
            connection_max_lifetime: None,
        };
        let resolved = resolve_single_connection(&conn, None, None).unwrap();
        assert_eq!(resolved.connection_url, "duckdb:///tmp/shop.duckdb");
        assert!(matches!(resolved.password_source, PasswordSource::None));
    }

    #[test]
    fn test_resolve_duckdb_url_passthrough() {
        let conn = NamedConnection {
            name: "duck".into(),
            url: Some("duckdb://:memory:".into()),
            driver: None,
            host: None,
            port: None,
            user: None,
            password: None,
            database: None,
            schema: None,
            sslmode: None,
            statement_timeout: None,
            connection_max_lifetime: None,
        };
        let resolved = resolve_single_connection(&conn, None, None).unwrap();
        assert_eq!(resolved.connection_url, "duckdb://:memory:");
    }

    #[test]
    fn test_lazy_resolver_duckdb_is_ready_not_pending() {
        let conn = NamedConnection {
            name: "duck".into(),
            url: None,
            driver: Some("duckdb".into()),
            host: None,
            port: None,
            user: None,
            password: None,
            database: Some(":memory:".into()),
            schema: None,
            sslmode: None,
            statement_timeout: None,
            connection_max_lifetime: None,
        };
        let entry = build_lazy_resolver(&conn, None, None).unwrap();
        match entry {
            LazyConnectionEntry::Ready(r) => {
                assert_eq!(r.connection_url, "duckdb://:memory:");
            }
            LazyConnectionEntry::Pending { .. } => {
                panic!("duckdb has no keyring dependency; must resolve eagerly");
            }
        }
    }

    #[test]
    fn test_parse_duration_seconds() {
        let d = parse_duration("30s").unwrap();
        assert_eq!(d, Duration::from_secs(30));
    }

    #[test]
    fn test_parse_duration_minutes() {
        let d = parse_duration("5min").unwrap();
        assert_eq!(d, Duration::from_secs(300));
    }

    #[test]
    fn test_parse_duration_hours() {
        let d = parse_duration("2h").unwrap();
        assert_eq!(d, Duration::from_secs(7200));
    }

    #[test]
    fn test_parse_duration_ms() {
        let d = parse_duration("500ms").unwrap();
        assert_eq!(d, Duration::from_millis(500));
    }

    #[test]
    fn test_parse_duration_plain_number() {
        let d = parse_duration("60").unwrap();
        assert_eq!(d, Duration::from_secs(60));
    }

    #[test]
    fn test_parse_duration_invalid() {
        assert!(parse_duration("xyz").is_err());
    }

    #[test]
    fn test_replace_password_in_url() {
        let url = "mysql://user:oldpass@host:3306/db";
        let result = replace_password_in_url(url, "newpass");
        assert_eq!(result, "mysql://user:newpass@host:3306/db");
    }

    #[test]
    fn test_replace_password_in_url_no_password() {
        let url = "mysql://user@host:3306/db";
        let result = replace_password_in_url(url, "newpass");
        assert_eq!(result, "mysql://user:newpass@host:3306/db");
    }

    // ─── inject_password_into_url regression tests (issue #115) ─────────

    #[test]
    fn inject_password_replaces_existing_password() {
        let url = "mysql://user:oldpass@host:3306/db";
        assert_eq!(
            inject_password_into_url(url, "newpass").unwrap(),
            "mysql://user:newpass@host:3306/db"
        );
    }

    #[test]
    fn inject_password_adds_password_when_userinfo_has_none() {
        let url = "mysql://user@host:3306/db";
        assert_eq!(
            inject_password_into_url(url, "newpass").unwrap(),
            "mysql://user:newpass@host:3306/db"
        );
    }

    #[test]
    fn inject_password_errors_without_userinfo() {
        let err = inject_password_into_url("mysql://host:3306/db", "newpass").unwrap_err();
        assert!(err.contains("userinfo"), "unexpected error: {err}");
    }

    #[test]
    fn inject_password_urlencodes_special_chars() {
        let url = "mysql://user@host:3306/db";
        assert_eq!(
            inject_password_into_url(url, "p@ss:w0rd").unwrap(),
            "mysql://user:p%40ss%3Aw0rd@host:3306/db"
        );
    }

    // ─── resolve_env_var_connection regression tests (issue #115) ───────

    #[test]
    fn env_var_url_without_password_is_injected() {
        let resolved = resolve_env_var_connection_inner(
            "mysql://user@host:3306/db".to_string(),
            Some("s3cret".to_string()),
        )
        .unwrap();
        assert_eq!(resolved.connection_url, "mysql://user:s3cret@host:3306/db");
        assert!(matches!(resolved.password_source, PasswordSource::EnvVar));
    }

    #[test]
    fn env_var_url_without_userinfo_errors() {
        let err = resolve_env_var_connection_inner(
            "mysql://host:3306/db".to_string(),
            Some("s3cret".to_string()),
        )
        .unwrap_err();
        assert!(err.contains("userinfo"), "unexpected error: {err}");
    }

    #[test]
    fn env_var_url_keeps_existing_inline_password() {
        let resolved = resolve_env_var_connection_inner(
            "mysql://user:inurl@host:3306/db".to_string(),
            Some("s3cret".to_string()),
        )
        .unwrap();
        assert_eq!(resolved.connection_url, "mysql://user:inurl@host:3306/db");
    }

    #[test]
    fn env_var_url_unchanged_without_env_password() {
        let resolved =
            resolve_env_var_connection_inner("mysql://user@host:3306/db".to_string(), None)
                .unwrap();
        assert_eq!(resolved.connection_url, "mysql://user@host:3306/db");
    }

    #[test]
    fn test_keyring_username() {
        let conn = NamedConnection {
            name: "dev".to_string(),
            url: None,
            driver: None,
            host: Some("localhost".to_string()),
            port: Some(3306),
            user: Some("root".to_string()),
            password: None,
            database: None,
            schema: None,
            sslmode: None,
            statement_timeout: None,
            connection_max_lifetime: None,
        };
        assert_eq!(conn.keyring_username(None), "dev#00001505");
        assert_eq!(conn.old_keyring_username(), "root/dev");
    }

    // ─── rewrite_password_to_sentinel regression tests ─────────────────

    fn write_temp_config(name: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "polar-mysql-test-{}-{}.toml",
            std::process::id(),
            name
        ));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_rewrite_sentinel_single_connection_roundtrip() {
        let path = write_temp_config(
            "single",
            r#"
host = "127.0.0.1"
port = 3306
user = "root"
password = "hunter2"
database = "mysql"
"#,
        );

        rewrite_password_to_sentinel(&path, "default").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = content
            .parse()
            .expect("rewritten config should be valid TOML");

        let password = parsed
            .get("password")
            .and_then(|v| v.as_str())
            .expect("password field should exist after rewrite");
        assert_eq!(password, "keyring");

        assert_eq!(
            parsed.get("host").and_then(|v| v.as_str()),
            Some("127.0.0.1")
        );
        assert_eq!(parsed.get("port").and_then(|v| v.as_integer()), Some(3306));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_rewrite_sentinel_multi_connection_roundtrip() {
        let path = write_temp_config(
            "multi",
            r#"
default_connection = "dev"

[connections.dev]
name = "dev"
host = "127.0.0.1"
port = 3306
user = "root"
password = "secret123"
database = "mydb"

[connections.prod]
name = "prod"
host = "prod.example.com"
port = 3306
user = "readonly"
password = "prod-secret"
database = "mydb"
"#,
        );

        rewrite_password_to_sentinel(&path, "dev").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = content
            .parse()
            .expect("rewritten multi-connection config should be valid TOML");

        let dev_password = parsed
            .get("connections")
            .and_then(|v| v.get("dev"))
            .and_then(|v| v.get("password"))
            .and_then(|v| v.as_str())
            .expect("dev password should exist");
        assert_eq!(dev_password, "keyring");

        let prod_password = parsed
            .get("connections")
            .and_then(|v| v.get("prod"))
            .and_then(|v| v.get("password"))
            .and_then(|v| v.as_str())
            .expect("prod password should still exist");
        assert_eq!(prod_password, "prod-secret");

        let dev_host = parsed
            .get("connections")
            .and_then(|v| v.get("dev"))
            .and_then(|v| v.get("host"))
            .and_then(|v| v.as_str());
        assert_eq!(dev_host, Some("127.0.0.1"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_rewrite_sentinel_nonexistent_connection() {
        let path = write_temp_config(
            "nonexistent",
            r#"
host = "127.0.0.1"
port = 3306
user = "root"
password = "hunter2"
database = "mysql"
"#,
        );

        let result = rewrite_password_to_sentinel(&path, "nonexistent");
        assert!(result.is_err(), "should error for nonexistent connection");
        assert!(result
            .unwrap_err()
            .contains("could not find password field"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_gaussdb_ssl_url_param() {
        assert_eq!(gaussdb_ssl_url_param("disable"), "?sslmode=disable");
        assert_eq!(gaussdb_ssl_url_param("DISABLED"), "?sslmode=disable");
        assert_eq!(gaussdb_ssl_url_param("require"), "?sslmode=require");
        assert_eq!(gaussdb_ssl_url_param("verify-ca"), "?sslmode=verify-ca");
        assert_eq!(gaussdb_ssl_url_param("verify-full"), "?sslmode=verify-full");
        assert_eq!(gaussdb_ssl_url_param("bogus"), "");
    }

    #[test]
    fn test_build_db_url_gaussdb_sslmode_passthrough() {
        let url = build_db_url(
            "gaussdb",
            "db.example.com",
            8000,
            "myuser",
            Some("p@ss"),
            Some("mydb"),
            Some("disable"),
        );
        assert!(url.starts_with("gaussdb://myuser:p%40ss@db.example.com:8000/mydb?sslmode=disable"));

        let url = build_db_url("gaussdb", "h", 5432, "u", None, None, Some("verify-full"));
        assert!(url.ends_with("?sslmode=verify-full"));
    }

    #[test]
    fn test_load_delta_diff_export_root_reads_configured_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        std::fs::write(
            &path,
            "delta_diff_export_root = \"/var/tmp/hepta-exports\"\n",
        )
        .unwrap();

        assert_eq!(
            load_delta_diff_export_root(Some(&path)),
            Some(PathBuf::from("/var/tmp/hepta-exports"))
        );
    }

    #[test]
    fn test_load_delta_diff_export_root_absent_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        std::fs::write(&path, "host = \"127.0.0.1\"\n").unwrap();

        assert_eq!(load_delta_diff_export_root(Some(&path)), None);
    }

    #[test]
    fn test_load_delta_diff_export_root_missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        assert_eq!(load_delta_diff_export_root(Some(&path)), None);
    }
}
