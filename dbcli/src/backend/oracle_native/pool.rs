use async_trait::async_trait;
use std::sync::Arc;

use crate::backend::error::DbError;
use crate::backend::{DbConn, DbPool, Dialect};

use super::conn::OracleConn;
use super::dialect::OracleDialect;

fn pool_init_sql() -> Vec<String> {
    OracleDialect::new().session_pin_sql()
}

pub(crate) struct OraclePool {
    user: String,
    password: String,
    conn_str: String,
}

impl OraclePool {
    pub(crate) fn new(user: String, password: String, conn_str: String) -> Self {
        Self {
            user,
            password,
            conn_str,
        }
    }
}

#[async_trait]
impl DbPool for OraclePool {
    async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
        let user = self.user.clone();
        let password = self.password.clone();
        let conn_str = self.conn_str.clone();
        let raw = tokio::task::spawn_blocking(move || {
            oracle::Connection::connect(&user, &password, &conn_str)
                .map_err(|e| DbError::connection_with_source("Oracle connection failed", e))
        })
        .await
        .map_err(|e| DbError::connection(format!("Oracle connect task panicked: {}", e)))??;
        let mut conn = OracleConn::new(raw);
        conn.probe_capabilities().await?;
        for sql in pool_init_sql() {
            conn.query_drop(&sql).await?;
        }
        Ok(Box::new(conn) as Box<dyn DbConn + Send>)
    }
}

pub(crate) fn create_oracle_pool(
    url: &str,
    _timeout_ms: Option<u64>,
) -> Result<Arc<dyn DbPool>, DbError> {
    let (conn_str, user, password) = parse_oracle_url(url)?;
    Ok(Arc::new(OraclePool::new(user, password, conn_str)))
}

fn parse_oracle_url(url: &str) -> Result<(String, String, String), DbError> {
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

    let conn_str = format!("//{}/{}", host_port, service_name);

    Ok((conn_str, percent_decode(user), percent_decode(password)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_oracle_url() {
        assert!(parse_oracle_url("oracle://scott:tiger@localhost:1521/ORCL").is_ok());
    }

    #[test]
    fn test_parse_oracle_url_default_port() {
        assert!(parse_oracle_url("oracle://scott:tiger@localhost/FREEPDB1").is_ok());
    }

    #[test]
    fn test_parse_missing_scheme() {
        assert!(parse_oracle_url("mysql://user:pass@host/db").is_err());
    }

    #[test]
    fn test_percent_decode_simple() {
        assert_eq!(percent_decode("hello"), "hello");
    }

    #[test]
    fn test_percent_decode_special_chars() {
        assert_eq!(percent_decode("p%40ss%3Aword"), "p@ss:word");
    }

    #[test]
    fn test_percent_decode_spaces() {
        assert_eq!(percent_decode("hello+world"), "hello world");
    }

    #[test]
    fn test_parse_oracle_url_with_encoded_password() {
        let (conn_str, user, password) =
            parse_oracle_url("oracle://scott:p%40ss%3Aword@localhost:1521/ORCL").unwrap();
        assert_eq!(user, "scott");
        assert_eq!(password, "p@ss:word");
        assert_eq!(conn_str, "//localhost:1521/ORCL");
    }

    #[test]
    fn pooled_connection_init_includes_dialect_pins() {
        let sql = pool_init_sql();
        assert!(
            sql.iter()
                .any(|stmt| stmt.contains("NLS_NUMERIC_CHARACTERS")),
            "{sql:?}"
        );
        assert!(sql.iter().any(|stmt| stmt.contains("NLS_SORT")), "{sql:?}");
    }
}

#[cfg(all(feature = "oracle", feature = "integration", test))]
mod integration {
    use super::*;

    fn oracle_test_url() -> Option<String> {
        std::env::var("POLARDB_ORACLE_TEST_URL").ok()
    }

    #[tokio::test]
    async fn test_connect_and_query() {
        let url = match oracle_test_url() {
            Some(u) => u,
            None => return,
        };
        let pool = create_oracle_pool(&url, None).expect("create pool");
        let mut conn = pool.acquire().await.expect("acquire");
        let result = conn
            .query("SELECT 'ok' AS status FROM dual")
            .await
            .expect("query");
        assert_eq!(result.row_count, 1);
    }

    #[tokio::test]
    async fn test_add_limit() {
        let url = match oracle_test_url() {
            Some(u) => u,
            None => return,
        };
        let pool = create_oracle_pool(&url, None).expect("create pool");
        let mut conn = pool.acquire().await.expect("acquire");
        let sql = conn.dialect().add_limit("SELECT * FROM all_tables", 5);
        let result = conn.query(&sql).await.expect("query");
        assert!(result.row_count <= 5);
    }
}
