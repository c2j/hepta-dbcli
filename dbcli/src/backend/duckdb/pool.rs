// ─── DuckDB pool: one Database instance, try_clone per acquire ───────
//
// DuckDB is embedded: there is no network server. A pool holds the single
// opened Database instance (as one Connection) and hands out clones that
// share the instance — the documented pattern for multi-connection access.

use std::sync::{Arc, Mutex};

use duckdb::AccessMode;

use crate::backend::error::DbError;
use crate::backend::{DbConn, DbPool};

use super::conn::DuckDbConn;
use super::dialect::DuckDbDialect;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DuckDbTarget {
    Memory,
    File { path: String, read_only: bool },
}

/// Parse `duckdb://{path}[?mode=ro|rw]`. The path is everything between
/// `duckdb://` and the query string; `:memory:` (or `memory`) opens an
/// in-memory database.
pub(crate) fn parse_duckdb_url(url: &str) -> Result<DuckDbTarget, DbError> {
    let rest = url
        .strip_prefix("duckdb://")
        .ok_or_else(|| DbError::config(format!("not a duckdb URL: {url}")))?;
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    let read_only = match query {
        None => false,
        Some(q) => {
            let mut ro = false;
            for part in q.split('&') {
                match part {
                    "mode=ro" | "mode=readonly" => ro = true,
                    "mode=rw" | "mode=automatic" => {}
                    other => {
                        return Err(DbError::config(format!(
                            "unsupported duckdb URL query parameter: {other}"
                        )))
                    }
                }
            }
            ro
        }
    };
    if path.is_empty() {
        return Err(DbError::config(
            "duckdb URL requires a file path or ':memory:'",
        ));
    }
    if path == ":memory:" || path == "memory" {
        return Ok(DuckDbTarget::Memory);
    }
    Ok(DuckDbTarget::File {
        path: path.to_string(),
        read_only,
    })
}

pub(crate) struct DuckDbPool {
    base: Arc<Mutex<duckdb::Connection>>,
}

impl std::fmt::Debug for DuckDbPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuckDbPool").finish_non_exhaustive()
    }
}

pub(crate) async fn create_duckdb_pool(url: &str) -> Result<DuckDbPool, DbError> {
    let target = parse_duckdb_url(url)?;
    let conn = tokio::task::spawn_blocking(move || open_target(&target))
        .await
        .map_err(|e| DbError::connection(format!("DuckDB open task failed: {e}")))??;
    Ok(DuckDbPool {
        base: Arc::new(Mutex::new(conn)),
    })
}

fn open_target(target: &DuckDbTarget) -> Result<duckdb::Connection, DbError> {
    match target {
        DuckDbTarget::Memory => duckdb::Connection::open_in_memory()
            .map_err(|e| DbError::connection_with_source("DuckDB in-memory open failed", e)),
        DuckDbTarget::File { path, read_only } => {
            // Connection::open would silently CREATE a missing file; for a
            // connection-validation tool that must be an error instead.
            if !std::path::Path::new(path).exists() {
                return Err(DbError::connection(format!(
                    "DuckDB database file not found: {path}"
                )));
            }
            let config = duckdb::Config::default()
                .access_mode(if *read_only {
                    AccessMode::ReadOnly
                } else {
                    AccessMode::Automatic
                })
                .map_err(|e| DbError::config(format!("DuckDB config rejected: {e}")))?;
            duckdb::Connection::open_with_flags(path, config).map_err(|e| {
                DbError::connection_with_source(format!("DuckDB open failed: {path}"), e)
            })
        }
    }
}

#[async_trait::async_trait]
impl DbPool for DuckDbPool {
    async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError> {
        let base = Arc::clone(&self.base);
        let conn = tokio::task::spawn_blocking(move || {
            let guard = base
                .lock()
                .map_err(|e| DbError::connection(format!("DuckDB pool mutex poisoned: {e}")))?;
            guard
                .try_clone()
                .map_err(|e| DbError::connection_with_source("DuckDB try_clone failed", e))
        })
        .await
        .map_err(|e| DbError::connection(format!("DuckDB acquire task failed: {e}")))??;
        Ok(Box::new(DuckDbConn {
            conn: Arc::new(Mutex::new(conn)),
            dialect: DuckDbDialect,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_path_absolute() {
        assert_eq!(
            parse_duckdb_url("duckdb:///data/shop.duckdb").unwrap(),
            DuckDbTarget::File {
                path: "/data/shop.duckdb".into(),
                read_only: false
            }
        );
    }

    #[test]
    fn parse_memory() {
        assert_eq!(
            parse_duckdb_url("duckdb://:memory:").unwrap(),
            DuckDbTarget::Memory
        );
        assert_eq!(
            parse_duckdb_url("duckdb://memory").unwrap(),
            DuckDbTarget::Memory
        );
    }

    #[test]
    fn parse_read_only_mode() {
        assert_eq!(
            parse_duckdb_url("duckdb:///data/a.duckdb?mode=ro").unwrap(),
            DuckDbTarget::File {
                path: "/data/a.duckdb".into(),
                read_only: true
            }
        );
        assert_eq!(
            parse_duckdb_url("duckdb:///data/a.duckdb?mode=rw").unwrap(),
            DuckDbTarget::File {
                path: "/data/a.duckdb".into(),
                read_only: false
            }
        );
    }

    #[test]
    fn rejects_unknown_query_params_and_empty_path() {
        assert!(parse_duckdb_url("duckdb:///a.duckdb?sslmode=require").is_err());
        assert!(parse_duckdb_url("duckdb://").is_err());
        assert!(parse_duckdb_url("mysql://h/db").is_err());
    }
}
