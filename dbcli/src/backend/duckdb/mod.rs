pub(crate) mod conn;
pub(crate) mod dialect;
pub(crate) mod pool;
pub(crate) mod types;

use std::sync::Arc;

use async_trait::async_trait;

use crate::backend::error::DbError;
use crate::backend::{BackendFactory, DbPool, Dialect};
use crate::config::TimeoutConfig;

use self::pool::create_duckdb_pool;

pub struct DuckDbFactory;

#[async_trait]
impl BackendFactory for DuckDbFactory {
    fn name(&self) -> &str {
        "DuckDB"
    }

    fn scheme(&self) -> &str {
        "duckdb"
    }

    fn create_dialect(&self) -> Box<dyn Dialect> {
        Box::new(dialect::DuckDbDialect)
    }

    async fn connect(
        &self,
        url: &str,
        _timeout_config: Option<&TimeoutConfig>,
    ) -> Result<Arc<dyn DbPool>, DbError> {
        let pool = create_duckdb_pool(url).await?;
        Ok(Arc::new(pool))
    }
}
