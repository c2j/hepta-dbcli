use async_trait::async_trait;
use serde_json::Value;

use crate::backend::error::DbError;
use crate::backend::{DbConn, Dialect, QueryResult};

use super::dialect::OracleDialect;
use super::types;

pub(crate) struct OracleConn {
    conn: oracle::Connection,
    dialect: OracleDialect,
}

impl OracleConn {
    pub(crate) fn new(conn: oracle::Connection) -> Self {
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

const PROBE_STANDARD_HASH: &str = "SELECT STANDARD_HASH('a','MD5') FROM dual";
const PROBE_DBMS_CRYPTO: &str = "SELECT DBMS_CRYPTO.HASH(UTL_RAW.CAST_TO_RAW('a'), 2) FROM dual";

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

fn result_set_to_query_result(
    result: oracle::ResultSet<oracle::Row>,
) -> Result<QueryResult, DbError> {
    let col_info = result.column_info().to_vec();
    let columns: Vec<String> = col_info.iter().map(|c| c.name().to_string()).collect();
    let col_count = columns.len();

    let mut rows: Vec<Vec<Value>> = Vec::new();
    for row_result in result {
        let row =
            row_result.map_err(|e| DbError::query_with_source("Oracle row fetch failed", e))?;
        rows.push(types::format_oracle_row(&row, col_count));
    }

    let row_count = rows.len();
    Ok(QueryResult {
        columns,
        rows,
        row_count,
    })
}

#[async_trait]
impl DbConn for OracleConn {
    async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let result = self
            .conn
            .query(sql, &[] as &[&dyn oracle::sql_type::ToSql])
            .map_err(|e| DbError::query_with_source("Oracle query failed", e))?;
        result_set_to_query_result(result)
    }

    async fn exec(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult, DbError> {
        let string_params: Vec<String> = params
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect();
        let param_refs: Vec<&dyn oracle::sql_type::ToSql> = string_params
            .iter()
            .map(|s| s as &dyn oracle::sql_type::ToSql)
            .collect();

        let result = self
            .conn
            .query(sql, &param_refs)
            .map_err(|e| DbError::query_with_source("Oracle exec failed", e))?;
        result_set_to_query_result(result)
    }

    async fn query_drop(&mut self, sql: &str) -> Result<(), DbError> {
        self.conn
            .execute(sql, &[] as &[&dyn oracle::sql_type::ToSql])
            .map(|_| ())
            .map_err(|e| DbError::query_with_source("Oracle query_drop failed", e))
    }

    fn dialect(&self) -> &dyn Dialect {
        &self.dialect
    }
}
