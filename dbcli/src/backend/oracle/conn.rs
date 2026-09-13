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

// oracle-rs 只回首个 prefetch 批（固定 100 行）；必须沿游标续取到尽，
// 否则任何超过 prefetch 的查询都被静默截断（cli/MCP/synth train 全部受影响）
const ORACLE_FETCH_SIZE: u32 = 100;

async fn drain_result(
    conn: &oracle_rs::Connection,
    mut result: oracle_rs::connection::QueryResult,
) -> Result<QueryResult, DbError> {
    if result.rows.is_empty() && !result.has_more_rows {
        return Ok(QueryResult::empty());
    }

    let columns: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    let col_count = columns.len();

    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        rows.push(types::format_oracle_row(row, col_count));
    }

    // oracle-rs 0.1.7 缺陷：execute 硬编码 prefetch=100 且从不置 has_more_rows，
    // fetch_more 亦协议损坏（返回空批并使后续请求被服务器断连——见
    // regress_oracle::oracle_query_fetches_beyond_prefetch_batch 的 ignore 原因）。
    // 此循环按 has_more_rows 契约编写，驱动修复后 >100 行查询自动生效，
    // 当前版本为无害空转。
    while result.has_more_rows {
        let more = conn
            .fetch_more(result.cursor_id, &result.columns, ORACLE_FETCH_SIZE)
            .await
            .map_err(|e| DbError::query_with_source("Oracle fetch_more failed", e))?;
        if more.rows.is_empty() {
            break;
        }
        for row in &more.rows {
            rows.push(types::format_oracle_row(row, col_count));
        }
        result = more;
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
            .query(sql, &[])
            .await
            .map_err(|e| DbError::query_with_source("Oracle query failed", e))?;
        drain_result(&self.conn, result).await
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
        drain_result(&self.conn, result).await
    }

    async fn query_drop(&mut self, sql: &str) -> Result<(), DbError> {
        match self.conn.execute(sql, &[]).await {
            Ok(_) => Ok(()),
            Err(e) if is_alter_session_decode_error(sql, &e.to_string()) => Ok(()),
            Err(e) => Err(DbError::query_with_source("Oracle query_drop failed", e)),
        }
    }

    fn dialect(&self) -> &dyn Dialect {
        &self.dialect
    }
}

fn is_alter_session_decode_error(sql: &str, error: &str) -> bool {
    sql.trim_start()
        .to_ascii_uppercase()
        .starts_with("ALTER SESSION")
        && error.starts_with("invalid length indicator:")
}

#[cfg(test)]
mod tests {
    use super::is_alter_session_decode_error;

    #[test]
    fn alter_session_tolerates_oracle_rs_post_execute_decode_bug() {
        assert!(is_alter_session_decode_error(
            "ALTER SESSION SET NLS_NUMERIC_CHARACTERS = '.,'",
            "invalid length indicator: 8"
        ));
        assert!(!is_alter_session_decode_error(
            "COMMIT",
            "invalid length indicator: 8"
        ));
        assert!(!is_alter_session_decode_error(
            "ALTER SESSION SET NLS_SORT = BINARY",
            "ORA-00922: missing or invalid option"
        ));
    }
}
