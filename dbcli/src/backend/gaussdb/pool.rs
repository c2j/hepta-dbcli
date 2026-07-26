use std::sync::Arc;

use async_trait::async_trait;
use gaussdb::NoTls;
use native_tls::TlsConnector;

use crate::backend::error::DbError;
use crate::backend::{DbConn, DbPool};

use super::conn::GaussdbConn;
use super::GaussdbDialect;

/// Single-connection pool — wraps one Arc<gaussdb::Client> with
/// multiplexed queries over one TCP connection. acquire() returns a
/// new wrapper pointing at the same underlying client. If the TCP
/// connection dies, the pool must be recreated (no auto-reconnect).
pub(crate) struct GaussdbPool {
    client: Arc<gaussdb::Client>,
}

pub(crate) async fn create_gaussdb_pool(url: &str) -> Result<GaussdbPool, DbError> {
    let conn_str = normalize_gaussdb_url(url);

    let client = match parse_sslmode(&conn_str) {
        Some(sslmode) => {
            let tls = build_tls(sslmode)?;
            let (client, connection) = gaussdb::connect(&conn_str, tls).await.map_err(|e| {
                DbError::connection(format!(
                    "GaussDB connect failed: {} (target: {}, sslmode={:?})",
                    e,
                    redact_password(&conn_str),
                    sslmode,
                ))
            })?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            client
        }
        None => {
            let (client, connection) = gaussdb::connect(&conn_str, NoTls).await.map_err(|e| {
                DbError::connection(format!(
                    "GaussDB connect failed: {} (target: {})",
                    e,
                    redact_password(&conn_str)
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
    Ok(GaussdbPool {
        client: Arc::new(client),
    })
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
        Ok(Box::new(GaussdbConn {
            client: Arc::clone(&self.client),
            dialect: GaussdbDialect,
        }))
    }
}
