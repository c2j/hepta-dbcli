// ─── DuckDB connection: async trait over a sync driver ───────────────
//
// duckdb::Connection is synchronous and Send-but-not-Sync. Every call runs
// inside tokio::task::spawn_blocking; the Connection lives behind
// Arc<Mutex<>> so the blocking closure can borrow it without taking
// ownership (no take/restore dance, safe against future cancellation).

use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use duckdb::types::{ToSql, ToSqlOutput, ValueRef};
use serde_json::Value;

use crate::backend::error::DbError;
use crate::backend::{DbConn, Dialect, QueryResult};

use super::dialect::DuckDbDialect;
use super::types;

pub(crate) struct DuckDbConn {
    pub(crate) conn: Arc<Mutex<duckdb::Connection>>,
    pub(crate) dialect: DuckDbDialect,
}

/// JSON parameter → duckdb bind value. DuckDB has no implicit type
/// coercion for bound parameters, so each JSON kind maps to a concrete
/// SQL type (string/int/float/bool/NULL).
pub(crate) enum Bind {
    S(String),
    I(i64),
    F(f64),
    B(bool),
    N,
}

impl Bind {
    fn from_json(v: &Value) -> Bind {
        match v {
            Value::Null => Bind::N,
            Value::Bool(b) => Bind::B(*b),
            Value::String(s) => Bind::S(s.clone()),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Bind::I(i)
                } else {
                    match n.as_f64() {
                        Some(f) => Bind::F(f),
                        None => Bind::S(n.to_string()),
                    }
                }
            }
            other => Bind::S(other.to_string()),
        }
    }
}

impl ToSql for Bind {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        Ok(match self {
            Bind::S(s) => s.to_sql()?,
            Bind::I(i) => i.to_sql()?,
            Bind::F(f) => f.to_sql()?,
            Bind::B(b) => b.to_sql()?,
            Bind::N => ToSqlOutput::Borrowed(ValueRef::Null),
        })
    }
}

fn lock(
    conn: &Arc<Mutex<duckdb::Connection>>,
) -> Result<MutexGuard<'_, duckdb::Connection>, DbError> {
    conn.lock()
        .map_err(|e| DbError::connection(format!("DuckDB conn mutex poisoned: {e}")))
}

fn run_query(
    conn: &Arc<Mutex<duckdb::Connection>>,
    sql: &str,
    binds: &[Bind],
) -> Result<QueryResult, DbError> {
    let guard = lock(conn)?;
    let mut stmt = guard
        .prepare(sql)
        .map_err(|e| DbError::query_with_source("DuckDB prepare failed", e))?;
    let bind_refs: Vec<&dyn ToSql> = binds.iter().map(|b| b as &dyn ToSql).collect();
    // DuckDB exposes column metadata only after execution — query first,
    // then read the schema (shared borrows coexist with `rows`).
    let mut rows = stmt
        .query(bind_refs.as_slice())
        .map_err(|e| DbError::query_with_source("DuckDB query failed", e))?;
    // DuckDB exposes column metadata only after execution — read it off the
    // executed statement, then drop the borrow before streaming rows.
    let (col_count, columns) = {
        let s = rows
            .as_ref()
            .ok_or_else(|| DbError::query("DuckDB query produced no statement"))?;
        let c = s.column_count();
        let names: Vec<String> = (0..c)
            .map(|i| {
                s.column_name(i)
                    .cloned()
                    .map_err(|e| DbError::query_with_source("DuckDB column name failed", e))
            })
            .collect::<Result<Vec<_>, _>>()?;
        (c, names)
    };

    let mut out: Vec<Vec<Value>> = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|e| DbError::query_with_source("DuckDB row fetch failed", e))?
    {
        let mut r = Vec::with_capacity(col_count);
        for i in 0..col_count {
            let vref = row
                .get_ref(i)
                .map_err(|e| DbError::query_with_source("DuckDB value fetch failed", e))?;
            r.push(types::value_ref_to_json(vref));
        }
        out.push(r);
    }
    let row_count = out.len();
    Ok(QueryResult {
        columns,
        rows: out,
        row_count,
    })
}

fn run_drop(conn: &Arc<Mutex<duckdb::Connection>>, sql: &str) -> Result<(), DbError> {
    let guard = lock(conn)?;
    guard
        .execute_batch(sql)
        .map_err(|e| DbError::query_with_source("DuckDB execute failed", e))
}

#[async_trait]
impl DbConn for DuckDbConn {
    async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let sql = sql.to_string();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || run_query(&conn, &sql, &[]))
            .await
            .map_err(|e| DbError::query(format!("DuckDB query task failed: {e}")))?
    }

    async fn exec(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult, DbError> {
        let sql = sql.to_string();
        let binds: Vec<Bind> = params.iter().map(Bind::from_json).collect();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || run_query(&conn, &sql, &binds))
            .await
            .map_err(|e| DbError::query(format!("DuckDB exec task failed: {e}")))?
    }

    async fn query_drop(&mut self, sql: &str) -> Result<(), DbError> {
        let sql = sql.to_string();
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || run_drop(&conn, &sql))
            .await
            .map_err(|e| DbError::query(format!("DuckDB execute task failed: {e}")))?
    }

    fn dialect(&self) -> &dyn Dialect {
        &self.dialect
    }
}
