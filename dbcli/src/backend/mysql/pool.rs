use async_trait::async_trait;
use mysql_async::prelude::Queryable;
use std::sync::Arc;

use crate::backend::error::DbError;
use crate::backend::{DbPool, Dialect};

use super::conn::MySqlConn;
use super::dialect::MySqlDialect;

fn pool_init_sql() -> Vec<String> {
    MySqlDialect.session_pin_sql()
}

pub(crate) struct MySqlPool {
    pool: mysql_async::Pool,
    timeout_ms: Option<u64>,
}

impl MySqlPool {
    pub(crate) fn new(pool: mysql_async::Pool, timeout_ms: Option<u64>) -> Self {
        Self { pool, timeout_ms }
    }
}

#[async_trait]
impl DbPool for MySqlPool {
    async fn acquire(&self) -> Result<Box<dyn super::super::DbConn + Send>, DbError> {
        let mut conn = self
            .pool
            .get_conn()
            .await
            .map_err(|e| DbError::connection_with_source("Failed to acquire connection", e))?;

        if let Some(ms) = self.timeout_ms {
            let set_sql = format!("SET max_execution_time = {}", ms);
            let _ = conn.query_drop(&set_sql).await;
        }
        for sql in pool_init_sql() {
            conn.query_drop(&sql)
                .await
                .map_err(|e| DbError::query_with_source("MySQL session pin failed", e))?;
        }

        Ok(Box::new(MySqlConn::new(conn)))
    }
}

pub(crate) fn create_mysql_pool(
    url: &str,
    timeout_ms: Option<u64>,
) -> Result<Arc<dyn DbPool>, DbError> {
    let opts = mysql_async::OptsBuilder::from_opts(
        mysql_async::Opts::from_url(url)
            .map_err(|e| DbError::connection_with_source("Invalid MySQL URL", e))?,
    );

    let pool = mysql_async::Pool::new(opts);
    Ok(Arc::new(MySqlPool::new(pool, timeout_ms)))
}
