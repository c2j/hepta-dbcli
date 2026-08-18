use async_trait::async_trait;
use mysql_async::prelude::Queryable;
use serde_json::Value;

use crate::backend::error::DbError;
use crate::backend::{DbConn, Dialect, QueryResult};

use super::dialect::MySqlDialect;
use super::types;

static MYSQL_DIALECT: MySqlDialect = MySqlDialect;

pub(crate) struct MySqlConn {
    conn: mysql_async::Conn,
}

impl MySqlConn {
    pub(crate) fn new(conn: mysql_async::Conn) -> Self {
        Self { conn }
    }

    fn rows_to_query_result(rows: Vec<mysql_async::Row>) -> QueryResult {
        if rows.is_empty() {
            return QueryResult::empty();
        }

        let columns: Vec<String> = rows[0]
            .columns_ref()
            .iter()
            .map(|c| c.name_str().to_string())
            .collect();

        let mut result_rows: Vec<Vec<Value>> = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut result_row: Vec<Value> = Vec::with_capacity(columns.len());
            for idx in 0..columns.len() {
                result_row.push(types::format_row_value(row, idx));
            }
            result_rows.push(result_row);
        }

        QueryResult {
            columns,
            rows: result_rows,
            row_count: rows.len(),
        }
    }
}

#[async_trait]
impl DbConn for MySqlConn {
    async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let rows: Vec<mysql_async::Row> = self
            .conn
            .query(sql)
            .await
            .map_err(|e| DbError::query_with_source("Query failed", e))?;
        Ok(Self::rows_to_query_result(rows))
    }

    async fn exec(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult, DbError> {
        if params.is_empty() {
            let rows: Vec<mysql_async::Row> = self
                .conn
                .query(sql)
                .await
                .map_err(|e| DbError::query_with_source("Exec query failed", e))?;
            return Ok(Self::rows_to_query_result(rows));
        }

        let bound: Vec<mysql_async::Value> = params
            .iter()
            .map(|v| match v {
                Value::Null => mysql_async::Value::NULL,
                Value::Bool(b) => mysql_async::Value::from(*b),
                Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        mysql_async::Value::from(i)
                    } else if let Some(u) = n.as_u64() {
                        mysql_async::Value::from(u)
                    } else {
                        mysql_async::Value::from(n.as_f64().unwrap_or(f64::NAN))
                    }
                }
                Value::String(s) => mysql_async::Value::from(s.as_bytes()),
                other => mysql_async::Value::from(other.to_string().into_bytes()),
            })
            .collect();

        let rows: Vec<mysql_async::Row> = self
            .conn
            .exec(sql, mysql_async::Params::Positional(bound))
            .await
            .map_err(|e| DbError::query_with_source("Exec failed", e))?;

        Ok(Self::rows_to_query_result(rows))
    }

    async fn query_drop(&mut self, sql: &str) -> Result<(), DbError> {
        self.conn
            .query_drop(sql)
            .await
            .map_err(|e| DbError::query_with_source("Query drop failed", e))
    }

    fn dialect(&self) -> &dyn Dialect {
        &MYSQL_DIALECT
    }
}
