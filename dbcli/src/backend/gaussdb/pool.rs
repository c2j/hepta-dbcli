use std::sync::Arc;

use async_trait::async_trait;
use gaussdb::NoTls;
use native_tls::TlsConnector;

use crate::backend::error::DbError;
use crate::backend::{DbConn, DbPool, Dialect};

use super::conn::GaussdbConn;
use super::error;
use super::GaussdbDialect;

/// Session SQL sent on every new GaussDB connection.
///
/// `read_only` is the default; `--allow-write` (issue #58) turns the engine
/// guard off for that process. The dialect's own session pins (timezone,
/// float digits) always apply.
pub(crate) fn connect_init_sql(read_only: bool) -> Vec<String> {
    let guard = if read_only {
        "SET default_transaction_read_only = ON"
    } else {
        // Explicit OFF: the server or role may default to read-only.
        "SET default_transaction_read_only = OFF"
    };
    let mut sql = vec![guard.to_string()];
    sql.extend(GaussdbDialect.session_pin_sql());
    sql
}

/// 真多连接池（delta-diff Phase 2 重构）：每次 acquire() 建立独立 TCP 连接，
/// 使每连接可持有独立快照事务（v2.1 §8.2 前置项；此前为单连接 Arc<Client>
/// 共享，多连接独立事务在物理上不可能）。
/// 代价是连接建立开销，与 Oracle 后端的专连专用模式一致。
pub(crate) struct GaussdbPool {
    conn_str: String,
    tls: Option<gaussdb::native_tls::MakeTlsConnector>,
    /// Session-level engine read-only guard; false only under `--allow-write`.
    read_only: bool,
}

impl std::fmt::Debug for GaussdbPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GaussdbPool")
            .field("conn_str", &redact_password(&self.conn_str))
            .field("tls", &self.tls.is_some())
            .field("read_only", &self.read_only)
            .finish()
    }
}

pub(crate) async fn create_gaussdb_pool(
    url: &str,
    read_only: bool,
) -> Result<GaussdbPool, DbError> {
    let conn_str = normalize_gaussdb_url(url);
    let tls = match parse_sslmode(&conn_str) {
        Some(sslmode) => Some(build_tls(sslmode)?),
        None => None,
    };
    let pool = GaussdbPool {
        conn_str,
        tls,
        read_only,
    };
    // 建池即验证连通性（对齐 connect_with_fallback 的 acquire 验证语义）
    let _ = pool.connect_one().await?;
    Ok(pool)
}

impl GaussdbPool {
    async fn connect_one(&self) -> Result<gaussdb::Client, DbError> {
        let client = match &self.tls {
            Some(tls) => {
                let (client, connection) = gaussdb::connect(&self.conn_str, tls.clone())
                    .await
                    .map_err(|e| {
                        error::wrap_gaussdb_connect_error(e, &redact_password(&self.conn_str))
                    })?;
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                client
            }
            None => {
                let (client, connection) =
                    gaussdb::connect(&self.conn_str, NoTls).await.map_err(|e| {
                        error::wrap_gaussdb_connect_error(e, &redact_password(&self.conn_str))
                    })?;
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                client
            }
        };
        // 防御性兜底（issue #27）：驱动启动握手已协商 client_encoding=UTF8
        // （rust-opengauss connect_raw.rs），此处再显式 SET，防止未来驱动行为
        // 变化时服务端回落到库默认编码，误读本驱动按 UTF-8 发送的字面量。
        if let Err(e) = client.simple_query("SET client_encoding = 'UTF8'").await {
            tracing::warn!("failed to set client_encoding=UTF8: {e}");
        }
        for sql in connect_init_sql(self.read_only) {
            client
                .simple_query(&sql)
                .await
                .map_err(|e| DbError::query_with_source("GaussDB session pin failed", e))?;
        }
        Ok(client)
    }
}

/// Convert gaussdb:// URL to postgres:// so tokio-postgres's
/// built-in config parser handles host, port, sslmode, and
/// percent-decoded credentials correctly.
fn normalize_gaussdb_url(url: &str) -> String {
    if let Some(rest) = url.strip_prefix("gaussdb://") {
        format!("postgres://{}", rest)
    } else {
        url.to_string()
    }
}

fn redact_password(conn_str: &str) -> String {
    if let Some(at) = conn_str.find('@') {
        if let Some(scheme_end) = conn_str.find("://") {
            if let Some(colon) = conn_str[scheme_end + 3..at].rfind(':') {
                let abs_colon = scheme_end + 3 + colon;
                return format!("{}:****@{}", &conn_str[..abs_colon], &conn_str[at + 1..]);
            }
        }
    }
    conn_str.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SslMode {
    /// Encrypt only — no certificate or hostname verification.
    Require,
    /// Verify the server certificate against trusted CAs (hostname not verified).
    VerifyCa,
    /// Full verification: certificate chain AND hostname.
    VerifyFull,
}

/// Parse `sslmode` query parameter from a connection URL.
/// Values are matched case-insensitively (PostgreSQL libpq convention).
fn parse_sslmode(url: &str) -> Option<SslMode> {
    let query = url.split('?').nth(1)?;
    for part in query.split('&') {
        let val = part.strip_prefix("sslmode=")?;
        match val.to_ascii_lowercase().as_str() {
            "require" => return Some(SslMode::Require),
            "verify-ca" => return Some(SslMode::VerifyCa),
            "verify-full" => return Some(SslMode::VerifyFull),
            _ => {}
        }
    }
    None
}

/// Build a TLS connector for the given sslmode and return a MakeTlsConnector.
/// Handles the three standard PostgreSQL sslmode TLS levels.
fn build_tls(sslmode: SslMode) -> Result<gaussdb::native_tls::MakeTlsConnector, DbError> {
    let connector = match sslmode {
        SslMode::Require => TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build(),
        SslMode::VerifyCa => TlsConnector::builder()
            .danger_accept_invalid_hostnames(true)
            .build(),
        SslMode::VerifyFull => TlsConnector::new(),
    }
    .map_err(|e| DbError::connection(format!("GaussDB TLS setup failed: {}", e)))?;
    Ok(gaussdb::native_tls::MakeTlsConnector::new(connector))
}

#[async_trait]
impl DbPool for GaussdbPool {
    async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
        let client = self.connect_one().await?;
        Ok(Box::new(GaussdbConn {
            client: Arc::new(client),
            dialect: GaussdbDialect,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_pin_read_only_session_by_default() {
        let sql = connect_init_sql(true);
        assert!(
            sql.iter()
                .any(|s| s == "SET default_transaction_read_only = ON"),
            "expected the engine guard: {sql:?}"
        );
    }

    #[test]
    fn should_explicitly_disable_the_guard_when_writes_are_allowed() {
        // Issue #58 D5: an explicit OFF, not merely omitting the SET, because
        // the role or server may default to read-only.
        let sql = connect_init_sql(false);
        assert!(
            sql.iter()
                .any(|s| s == "SET default_transaction_read_only = OFF"),
            "expected an explicit OFF: {sql:?}"
        );
        assert!(
            !sql.iter()
                .any(|s| s.contains("default_transaction_read_only = ON")),
            "write mode must not keep the guard: {sql:?}"
        );
    }

    #[test]
    fn should_always_keep_the_dialect_session_pins() {
        for read_only in [true, false] {
            let sql = connect_init_sql(read_only);
            for pin in GaussdbDialect.session_pin_sql() {
                assert!(sql.contains(&pin), "missing session pin {pin:?}");
            }
        }
    }

    #[test]
    fn test_normalize_gaussdb_url_rewrites_scheme() {
        assert_eq!(
            normalize_gaussdb_url("gaussdb://u:p@h:5432/db"),
            "postgres://u:p@h:5432/db"
        );
        assert_eq!(
            normalize_gaussdb_url("postgres://u@h/db"),
            "postgres://u@h/db"
        );
    }

    #[test]
    fn test_parse_sslmode_values() {
        assert_eq!(
            parse_sslmode("gaussdb://h/db?sslmode=require"),
            Some(SslMode::Require)
        );
        assert_eq!(
            parse_sslmode("gaussdb://h/db?sslmode=verify-ca"),
            Some(SslMode::VerifyCa)
        );
        assert_eq!(
            parse_sslmode("gaussdb://h/db?sslmode=verify-full"),
            Some(SslMode::VerifyFull)
        );
        assert_eq!(parse_sslmode("gaussdb://h/db?sslmode=disable"), None);
        assert_eq!(parse_sslmode("gaussdb://h/db"), None);
    }

    #[test]
    fn test_redact_password_url() {
        assert_eq!(
            redact_password("postgres://myuser:s3cret@db.example.com:8000/mydb"),
            "postgres://myuser:****@db.example.com:8000/mydb"
        );
    }

    #[test]
    fn pooled_connection_init_includes_dialect_pins() {
        let sql = connect_init_sql(true);
        assert!(sql.iter().any(|stmt| stmt.contains("TimeZone")), "{sql:?}");
        assert!(
            sql.iter().any(|stmt| stmt.contains("extra_float_digits")),
            "{sql:?}"
        );
    }
}
