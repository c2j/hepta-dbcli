use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::backend::error::DbError;
use crate::backend::{DbConn, Dialect, QueryResult};

use super::error;
use super::types;
use super::GaussdbDialect;

pub(crate) struct GaussdbConn {
    pub(crate) client: Arc<gaussdb::Client>,
    pub(crate) dialect: GaussdbDialect,
}

fn row_to_values(row: &gaussdb::Row, col_count: usize) -> Vec<Value> {
    (0..col_count)
        .map(|i| types::format_value_at(row, i))
        .collect()
}

#[async_trait]
impl DbConn for GaussdbConn {
    async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let rows = self
            .client
            .query(sql, &[])
            .await
            .map_err(|e| error::wrap_gaussdb_error("query", e))?;

        if rows.is_empty() {
            return Ok(QueryResult::empty());
        }

        let columns: Vec<String> = rows[0]
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();
        let col_count = columns.len();

        let result_rows: Vec<Vec<Value>> = rows
            .iter()
            .map(|row| row_to_values(row, col_count))
            .collect();
        let row_count = result_rows.len();

        Ok(QueryResult {
            columns,
            rows: result_rows,
            row_count,
            rows_affected: None,
        })
    }

    async fn exec(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult, DbError> {
        let str_params: Vec<String> = params
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        let param_refs: Vec<&(dyn gaussdb::types::ToSql + Sync)> = str_params
            .iter()
            .map(|s| s as &(dyn gaussdb::types::ToSql + Sync))
            .collect();
        let rows = self
            .client
            .query(sql, &param_refs)
            .await
            .map_err(|e| error::wrap_gaussdb_error("exec", e))?;

        if rows.is_empty() {
            return Ok(QueryResult::empty());
        }

        let columns: Vec<String> = rows[0]
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();
        let col_count = columns.len();

        let result_rows: Vec<Vec<Value>> = rows
            .iter()
            .map(|row| row_to_values(row, col_count))
            .collect();
        let row_count = result_rows.len();

        Ok(QueryResult {
            columns,
            rows: result_rows,
            row_count,
            rows_affected: None,
        })
    }

    async fn query_drop(&mut self, sql: &str) -> Result<(), DbError> {
        self.client
            .simple_query(sql)
            .await
            .map_err(|e| error::wrap_gaussdb_error("query_drop", e))?;
        Ok(())
    }

    async fn execute_write(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let affected = self
            .client
            .execute(sql, &[])
            .await
            .map_err(|e| error::wrap_gaussdb_error("execute_write", e))?;
        Ok(QueryResult::affected(affected))
    }

    fn dialect(&self) -> &dyn Dialect {
        &self.dialect
    }
}
