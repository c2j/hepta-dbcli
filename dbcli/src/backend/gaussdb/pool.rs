use std::sync::Arc;

use async_trait::async_trait;
use gaussdb::NoTls;
use native_tls::TlsConnector;

use crate::backend::error::DbError;
use crate::backend::{DbConn, DbPool};

use super::conn::GaussdbConn;
use super::GaussdbDialect;

/// 真多连接池（delta-diff Phase 2 重构）：每次 acquire() 建立独立 TCP 连接，
/// 使每连接可持有独立快照事务（v2.1 §8.2 前置项；此前为单连接 Arc<Client>
/// 共享，多连接独立事务在物理上不可能）。
/// 代价是连接建立开销，与 Oracle 后端的专连专用模式一致。
pub(crate) struct GaussdbPool {
    conn_str: String,
    tls: Option<gaussdb::native_tls::MakeTlsConnector>,
}

pub(crate) async fn create_gaussdb_pool(url: &str) -> Result<GaussdbPool, DbError> {
    let conn_str = normalize_gaussdb_url(url);
    let tls = match parse_sslmode(&conn_str) {
        Some(sslmode) => Some(build_tls(sslmode)?),
        None => None,
    };
    let pool = GaussdbPool { conn_str, tls };
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
                        DbError::connection(format!(
                            "GaussDB connect failed: {} (target: {})",
                            e,
                            redact_password(&self.conn_str)
                        ))
                    })?;
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                client
            }
            None => {
                let (client, connection) =
                    gaussdb::connect(&self.conn_str, NoTls).await.map_err(|e| {
                        DbError::connection(format!(
                            "GaussDB connect failed: {} (target: {})",
                            e,
                            redact_password(&self.conn_str)
                        ))
                    })?;
                tokio::spawn(async move {
                    let _ = connection.await;
                });
                client
            }
        };
        let _ = client
            .simple_query("SET default_transaction_read_only = ON")
            .await;
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
