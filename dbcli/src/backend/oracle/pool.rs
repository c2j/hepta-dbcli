use async_trait::async_trait;
use std::sync::Arc;

use crate::backend::error::DbError;
use crate::backend::{DbConn, DbPool, Dialect};

use super::conn::OracleConn;
use super::dialect::OracleDialect;

fn pool_init_sql() -> Vec<String> {
    OracleDialect::new().session_pin_sql()
}

pub(crate) struct OraclePool;

impl OraclePool {
    pub(crate) fn new() -> Self {
        Self
    }
}

#[async_trait]
impl DbPool for OraclePool {
    async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
        Err(DbError::unsupported("OraclePool not yet connected"))
    }
}

pub(crate) fn create_oracle_pool(
    url: &str,
    _timeout_ms: Option<u64>,
) -> Result<Arc<dyn DbPool>, DbError> {
    let config = parse_oracle_url(url)?;
    Ok(Arc::new(OracleRsPool::new(config)))
}

struct OracleRsPool {
    config: oracle_rs::Config,
}

impl OracleRsPool {
    fn new(config: oracle_rs::Config) -> Self {
        Self { config }
    }
}

#[async_trait]
impl DbPool for OracleRsPool {
    async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
        let conn = oracle_rs::Connection::connect_with_config(self.config.clone())
            .await
            .map_err(|e| DbError::connection_with_source("Oracle connection failed", e))?;
        let mut conn = OracleConn::new(conn);
        conn.probe_capabilities().await?;
        for sql in pool_init_sql() {
            conn.query_drop(&sql).await?;
        }
        Ok(Box::new(conn))
    }
}

fn parse_oracle_url(url: &str) -> Result<oracle_rs::Config, DbError> {
    let without_scheme = url
        .strip_prefix("oracle://")
        .ok_or_else(|| DbError::config("Oracle URL must start with oracle://"))?;

    let (credentials, host_port_service) = without_scheme
        .split_once('@')
        .ok_or_else(|| DbError::config("Oracle URL missing @ separator"))?;

    let (user, password) = credentials
        .split_once(':')
        .ok_or_else(|| DbError::config("Oracle URL missing password"))?;

    let (host_port, service_name) = host_port_service
        .split_once('/')
        .ok_or_else(|| DbError::config("Oracle URL missing service name"))?;

    let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
        (h, p.parse::<u16>().unwrap_or(1521))
    } else {
        (host_port, 1521u16)
    };

    Ok(oracle_rs::Config::new(
        host,
        port,
        service_name,
        percent_decode(user),
        percent_decode(password),
    ))
}

fn percent_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next().unwrap_or(b'0');
            let lo = chars.next().unwrap_or(b'0');
            if let Ok(decoded) = u8::from_str_radix(&format!("{}{}", hi as char, lo as char), 16) {
                result.push(decoded as char);
            } else {
                result.push('%');
                result.push(hi as char);
                result.push(lo as char);
            }
        } else if b == b'+' {
            result.push(' ');
        } else {
            result.push(b as char);
        }
    }
    result
}
