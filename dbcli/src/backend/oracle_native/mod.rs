pub(crate) mod conn;
pub(crate) mod dialect;
pub(crate) mod pool;
pub(crate) mod types;

use async_trait::async_trait;
use std::sync::Arc;

use crate::backend::error::DbError;
use crate::backend::{BackendFactory, DbPool, Dialect};
use crate::config::TimeoutConfig;

use self::dialect::OracleDialect;
use self::pool::create_oracle_pool;

pub struct OracleFactory;

#[async_trait]
impl BackendFactory for OracleFactory {
    fn name(&self) -> &str {
        "Oracle"
    }

    fn scheme(&self) -> &str {
        "oracle"
    }

    fn create_dialect(&self) -> Box<dyn Dialect> {
        Box::new(OracleDialect::new())
    }

    async fn connect(
        &self,
        url: &str,
        timeout_config: Option<&TimeoutConfig>,
    ) -> Result<Arc<dyn DbPool>, DbError> {
        let timeout_ms = timeout_config
            .and_then(|tc| tc.statement_timeout)
            .map(|d| d.as_millis() as u64);

        create_oracle_pool(url, timeout_ms)
    }
}
