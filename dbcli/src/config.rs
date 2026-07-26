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

// ─── Config Helpers ───────────────────────────────────────────────────

pub(crate) fn default_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|p| p.join(format!(".{}", DEFAULT_CONFIG_FILENAME)))
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
    let url = if let Some(ref u) = conn.url {
        u.clone()
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

    let password_source = match conn.password.as_deref() {
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
        if let Some(ref u) = conn.url {
            u.clone()
        } else {
            let pw = std::env::var("HEPTA_DBCLI_PASSWORD").unwrap_or_default();
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
    })
}

fn replace_password_in_url(url: &str, new_password: &str) -> String {
    // Find the @ sign to locate user:password portion
    if let Some(at_pos) = url.find('@') {
        let user_part = &url[..at_pos];
        // Find the start of credentials (after scheme://)
        let cred_start = user_part.find("://").map(|i| i + 3).unwrap_or(0);
        let creds = &user_part[cred_start..];
        let encoded = urlencode(new_password);
        // Look for colon in the credentials portion (not in the scheme)
        if let Some(colon_pos) = creds.rfind(':') {
            let abs_colon = cred_start + colon_pos;
            return format!("{}:{}@{}", &url[..abs_colon], encoded, &url[at_pos + 1..]);
        }
        // No colon before @: add password
        return format!("{}:{}@{}", &url[..at_pos], encoded, &url[at_pos + 1..]);
    }
    url.to_string()
}

pub(crate) fn resolve_env_var_connection(url: String) -> ResolvedConnection {
    let timeout_config = TimeoutConfig::default();
    ResolvedConnection {
        name: "default".to_string(),
        connection_url: url,
        password_source: PasswordSource::EnvVar,
        keyring_username: format!("default#{}", config_path_hash(None)),
        config_path: None,
        plaintext_password: None,
        timeout_config,
    }
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

    if is_plaintext || conn.url.is_some() {
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
            _ => unreachable!(),
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
        let resolved = resolve_env_var_connection(url);
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
}
