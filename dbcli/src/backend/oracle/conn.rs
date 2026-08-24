use async_trait::async_trait;
use serde_json::Value;

use crate::backend::error::DbError;
use crate::backend::{DbConn, Dialect, QueryResult};

use super::dialect::OracleDialect;
use super::types;

pub(crate) struct OracleConn {
    conn: oracle_rs::Connection,
    dialect: OracleDialect,
}

impl OracleConn {
    pub(crate) fn new(conn: oracle_rs::Connection) -> Self {
        Self {
            conn,
            dialect: OracleDialect::new(),
        }
    }

    pub(crate) async fn probe_capabilities(&mut self) -> Result<(), DbError> {
        self.dialect = probe_oracle_dialect(self).await?;
        Ok(())
    }
}

pub(crate) const PROBE_STANDARD_HASH: &str = "SELECT STANDARD_HASH('a','MD5') FROM dual";
pub(crate) const PROBE_DBMS_CRYPTO: &str =
    "SELECT DBMS_CRYPTO.HASH(UTL_RAW.CAST_TO_RAW('a'), 2) FROM dual";

async fn probe_oracle_dialect(conn: &mut OracleConn) -> Result<OracleDialect, DbError> {
    if conn.query(PROBE_STANDARD_HASH).await.is_ok() {
        return Ok(OracleDialect::new());
    }
    if conn.query(PROBE_DBMS_CRYPTO).await.is_ok() {
        return Ok(OracleDialect::oracle11());
    }
    Err(DbError::unsupported(
        "this Oracle has no STANDARD_HASH (12c+) and DBMS_CRYPTO.HASH failed; \
         on 11g grant EXECUTE ON SYS.DBMS_CRYPTO to the connected user",
    ))
}

fn oracle_result_to_query_result(result: oracle_rs::connection::QueryResult) -> QueryResult {
    if result.rows.is_empty() {
        return QueryResult::empty();
    }

    let columns: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    let col_count = columns.len();
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        rows.push(types::format_oracle_row(row, col_count));
    }

    let row_count = rows.len();
    QueryResult {
        columns,
        rows,
        row_count,
    }
}

#[async_trait]
impl DbConn for OracleConn {
    async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let result = self
            .conn
            .query(sql, &[])
            .await
            .map_err(|e| DbError::query_with_source("Oracle query failed", e))?;
        Ok(oracle_result_to_query_result(result))
    }

    async fn exec(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult, DbError> {
        let string_params: Vec<String> = params
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();

        let param_refs: Vec<oracle_rs::Value> = string_params
            .iter()
            .map(|s| oracle_rs::Value::String(s.clone()))
            .collect();

        let result = self
            .conn
            .query(sql, &param_refs)
            .await
            .map_err(|e| DbError::query_with_source("Oracle exec failed", e))?;
        Ok(oracle_result_to_query_result(result))
    }

    async fn query_drop(&mut self, sql: &str) -> Result<(), DbError> {
        self.conn
            .execute(sql, &[])
            .await
            .map_err(|e| DbError::query_with_source("Oracle query_drop failed", e))?;
        Ok(())
    }

    fn dialect(&self) -> &dyn Dialect {
        &self.dialect
    }
}
