// ─── Database Backend Abstraction Layer ─────────────────────────────
//
// This module defines the trait interfaces that decouple the hepta_dbcli
// CLI + MCP server from any specific database driver. Each supported
// database (MySQL, Oracle, GaussDB, etc.) implements these traits in its own
// submodule under backend/.

pub mod error;
pub mod factory;
#[cfg(feature = "gaussdb")]
pub mod gaussdb;
pub mod mysql;
#[cfg(feature = "oracle-rs")]
pub mod oracle;
#[cfg(feature = "oracle")]
pub mod oracle_native;

use async_trait::async_trait;
use serde_json::Value;
use std::fmt;
use std::sync::Arc;

use crate::config::TimeoutConfig;
pub use error::DbError;

// ─── QueryResult ────────────────────────────────────────────────────

/// Unified query result — database-agnostic, normalized to JSON values.
/// This is the single data type that flows from any backend to the CLI/server layer.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub row_count: usize,
}

impl QueryResult {
    pub fn empty() -> Self {
        Self {
            columns: vec![],
            rows: vec![],
            row_count: 0,
        }
    }
}

impl fmt::Display for QueryResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "QueryResult {{ columns: {:?}, row_count: {} }}",
            self.columns, self.row_count
        )
    }
}

// ─── DbConn — Single Database Connection ────────────────────────────

/// A single database connection obtained from a pool.
/// All query methods consume `&mut self` because mysql_async requires it;
/// other backends (oracle-rs, etc.) get auto-deref and it's a no-op.
#[async_trait]
pub trait DbConn: Send {
    /// Execute a SQL query and return normalized results.
    /// The returned QueryResult has columns as strings and rows as Vec<serde_json::Value>.
    async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError>;

    /// Execute a parameterized SQL query (e.g. for introspection with ? or :1 bindings).
    /// Parameters are passed as JSON values; each backend converts to native bind format.
    async fn exec(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult, DbError>;

    /// Execute a SQL statement that returns no rows (SET, ALTER SESSION, etc.).
    async fn query_drop(&mut self, sql: &str) -> Result<(), DbError>;

    /// Return a reference to the dialect associated with this connection.
    fn dialect(&self) -> &dyn Dialect;
}

// ─── DbPool — Connection Pool ───────────────────────────────────────

/// A pool of database connections. Each call to `acquire()` returns a
/// fresh or recycled connection from the pool.
#[async_trait]
pub trait DbPool: Send + Sync {
    /// Obtain a connection from the pool.
    async fn acquire(&self) -> Result<Box<dyn DbConn + Send>, DbError>;
}

// ─── Dialect — SQL Syntax & Introspection Adapter ───────────────────

/// Encapsulates all database-specific SQL syntax differences:
/// introspection queries, keyword lists, LIMIT/EXPLAIN generation,
/// connection parameters, and REPL tokenizer rules.
pub trait Dialect: Send + Sync {
    // ── Introspection SQL (returned as &str — all are compile-time constants) ──

    /// Query returning [version, database, current_user, hostname, port, os, charset, collation, version_comment]
    fn database_info(&self) -> &str;

    /// Query returning [schema_name, table_name, table_type, engine, row_count, total_size, comment]
    fn list_tables(&self) -> &str;

    /// Parameterized query (schema_name, table_name) returning
    /// [column_name, data_type, nullable, default_value, ordinal_position, comment, column_key]
    fn table_columns(&self) -> &str;

    /// Parameterized query (schema_name, table_name) returning
    /// [index_name, is_unique, is_primary, columns, index_type]
    fn table_indexes(&self) -> &str;

    // ── Syntax Adapters ──

    /// SQL statement prefixes that are considered read-only for MCP enforcement.
    fn read_only_prefixes(&self) -> &[&str];

    /// Append a row-limiting clause to a SELECT query if it doesn't already have one.
    fn add_limit(&self, sql: &str, n: usize) -> String;

    /// Build an EXPLAIN (or EXPLAIN ANALYZE) statement in the requested format.
    fn build_explain(&self, sql: &str, analyze: bool, format: &str) -> String;

    /// Return the SQL to set per-statement timeout, or None if not supported.
    /// Called before each MCP query to apply the per-call timeout_ms.
    fn set_statement_timeout_sql(&self, ms: u64) -> Option<String>;

    /// Return the SQL to kill the current connection, or None if not supported.
    /// Used for timeout_action=disconnect to force pool recycling.
    fn kill_own_connection_sql(&self) -> Option<String>;

    // ── Connection Metadata ──

    /// Default TCP port for this database.
    fn default_port(&self) -> u16;

    /// URL scheme (e.g. "mysql", "oracle").
    fn url_scheme(&self) -> &str;

    // ── REPL Tokenizer Adapters ──

    /// Character used to quote identifiers (backtick ` for MySQL, double-quote " for Oracle).
    fn identifier_quote(&self) -> char;

    /// Whether this database supports # as a line-comment token (MySQL yes, Oracle no).
    fn supports_hash_comment(&self) -> bool;

    /// Whether this database supports $...$ dollar-quoting (PostgreSQL/GaussDB yes, MySQL/Oracle no).
    fn supports_dollar_quote(&self) -> bool {
        false
    }

    // ── delta-diff Adapters (docs/delta-diff 设计文档 §7.2) ──

    /// SQL statement(s) opening a snapshot transaction (v2.1 §8.2).
    /// PolarDB-X must be detected at runtime via `is_polardbx_version` and use
    /// `begin_snapshot_sql_polardbx` instead (§16.3-F5).
    fn begin_snapshot_sql(&self) -> &str;

    /// PolarDB-X fallback: two statements (SET isolation + START TRANSACTION READ ONLY).
    /// Only the MySQL-family dialect returns Some.
    fn begin_snapshot_sql_polardbx(&self) -> Option<[&'static str; 2]> {
        None
    }

    /// Oracle only: SQL to read the current SCN for AS OF SCN flashback anchoring.
    fn snapshot_scn_sql(&self) -> Option<&'static str> {
        None
    }

    /// Hash function capability for row hashing (v2.1 §11.3-5).
    fn hash_capability(&self) -> HashCapability {
        HashCapability::Md5
    }

    /// Normalize a column value to its canonical text form for cross-db hashing
    /// (v2.1 §九). Returns Err for unmappable/unsupported column types.
    fn normalize_expr(&self, col: &ColumnNormSpec) -> Result<String, DbError>;

    /// Render the order-independent bit-slice checksum SQL (v2.1 §十).
    fn render_checksum_sql(&self, spec: &ChecksumSqlSpec) -> String;

    /// Render one keyset-paginated row fetch (v2.1 §6.2.2).
    fn render_keyset_page_sql(&self, spec: &KeysetPageSpec) -> String;

    /// Render a bucket multiset query (v2.1 §6.3 BucketDiffer)：
    /// `SELECT h, COUNT(*) FROM (SELECT <row_hash> AS h FROM t WHERE <bucket pred>) GROUP BY h`。
    /// spec.bucket 必须存在；返回 (row_hash, count) 行集，客户端做多重集合比对。
    fn render_bucket_multiset_sql(&self, spec: &ChecksumSqlSpec) -> String;

    /// Render an IBLT summary SQL（Addendum A v1.1 §三，j=4 哈希子表）。
    /// 返回行集 (grp, cell, cnt, key_xor, val_xor_1..4)；桶位 j 取 val_xor 第 j 切片
    /// （对齐约束 §1.4）。GaussDB 无 bit_xor 聚合 → 逐位奇偶 SUM（宽列）；
    /// Oracle <21c 无 BIT_XOR_AGG → Err(Unsupported)。
    fn render_iblt_sql(&self, spec: &IbltSqlSpec) -> Result<String, DbError>;
}

/// Specification for an IBLT summary query（Addendum A v1.1 §三）。
#[derive(Debug, Clone)]
pub struct IbltSqlSpec {
    pub schema: Option<String>,
    pub table: String,
    /// key 表达式（定长整数：数值列或 epoch 转换），用于 key_xor
    pub key_expr: String,
    /// §九 规范化表达式（row_hash = MD5(concat_ws('#', ...))）
    pub normalized_exprs: Vec<String>,
    /// 每个哈希子表的桶数（m = ⌈3d/4⌉，总桶数 k=4m≈3d）
    pub cells_per_subtable: u64,
    pub filter: Option<String>,
    /// Oracle AS OF SCN anchor（快照模式）
    pub scn: Option<u64>,
}

/// Hash function capability of a backend (v2.1 §7.2).
/// GaussDB stays on Md5 — hash_any_extended probed missing on openGauss 5.0.0 (§16.3-F4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashCapability {
    Md5,
    Crc32Chain,
}

/// PolarDB-X detection from a VERSION() string (v2.1 §16.3-F5).
pub(crate) fn is_polardbx_version(version: &str) -> bool {
    version.to_uppercase().contains("PXC")
}

/// Column metadata for `Dialect::normalize_expr` (v2.1 §九).
#[derive(Debug, Clone)]
pub struct ColumnNormSpec {
    pub name: String,
    /// Backend-native type string (MySQL COLUMN_TYPE e.g. "decimal(20,6)";
    /// GaussDB format_type e.g. "numeric(20,6)"; Oracle DATA_TYPE e.g. "NUMBER").
    pub data_type: String,
    pub nullable: bool,
}

/// Specification for one bit-slice checksum query (v2.1 §十).
#[derive(Debug, Clone)]
pub struct ChecksumSqlSpec {
    pub schema: Option<String>,
    pub table: String,
    /// Key column used for range predicates (hashdiff); unused in pure bucket mode.
    pub key_column: Option<String>,
    /// Key range [lo, hi).
    pub range: Option<(i64, i64)>,
    /// Content bucket predicate: (modulus, bucket).
    pub bucket: Option<(u64, u64)>,
    /// Extra WHERE condition (user --where), appended as-is.
    pub filter: Option<String>,
    /// Oracle AS OF SCN anchor (snapshot mode).
    pub scn: Option<u64>,
    /// Normalized per-column expressions (from normalize_expr), select order.
    pub normalized_exprs: Vec<String>,
}

/// Specification for one keyset pagination fetch (v2.1 §6.2.2).
#[derive(Debug, Clone)]
pub struct KeysetPageSpec {
    pub schema: Option<String>,
    pub table: String,
    /// Columns to select, key column first.
    pub columns: Vec<String>,
    /// true: `columns` 为 SQL 表达式（规范化表达式），渲染时不加引号；
    /// false: 列名，按 identifier_quote 加引号。
    /// 行级跨库比较统一用规范化表达式（§九-2：两侧文本表示字节级一致）。
    pub raw_exprs: bool,
    pub key_column: String,
    pub range: Option<(i64, i64)>,
    pub last_key: Option<i64>,
    pub page_size: usize,
    pub filter: Option<String>,
    /// Oracle AS OF SCN anchor (snapshot mode).
    pub scn: Option<u64>,
}

// ─── BackendFactory — Creates Backends from Configuration ───────────

/// A factory that creates DbPool instances and Dialect objects for a
/// specific database backend. One factory exists per supported database
/// type and can create many connections/pools.
#[async_trait]
pub trait BackendFactory: Send + Sync {
    /// Human-readable name (e.g. "MySQL", "Oracle").
    fn name(&self) -> &str;

    /// URL scheme this factory handles (e.g. "mysql", "oracle").
    fn scheme(&self) -> &str;

    /// Create a dialect instance for this backend.
    fn create_dialect(&self) -> Box<dyn Dialect>;

    /// Create a connection pool from a fully-resolved connection URL.
    async fn connect(
        &self,
        url: &str,
        timeout_config: Option<&TimeoutConfig>,
    ) -> Result<Arc<dyn DbPool>, DbError>;
}

// ─── Scheme-level Defaults (pre-connection lookups) ──────────────────

/// Default TCP port for a database scheme. Used by config resolution
/// before any BackendFactory is instantiated. Each backend's
/// Dialect::default_port() MUST agree with this lookup.
pub(crate) fn default_port_for_scheme(scheme: &str) -> u16 {
    match scheme {
        "oracle" => 1521,
        "gaussdb" => 5432,
        _ => 3306, // mysql and unknown
    }
}

/// SSL URL query parameter for a database scheme. Each backend's
/// driver must accept this format. Used by config URL building.
pub(crate) fn ssl_url_param_for_scheme(scheme: &str) -> &'static str {
    match scheme {
        "oracle" => "",
        "gaussdb" => "?sslmode=require",
        _ => "?ssl-mode=REQUIRED", // mysql
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_port_mysql() {
        assert_eq!(default_port_for_scheme("mysql"), 3306);
    }

    #[test]
    fn test_default_port_oracle() {
        assert_eq!(default_port_for_scheme("oracle"), 1521);
    }

    #[test]
    fn test_default_port_gaussdb() {
        assert_eq!(default_port_for_scheme("gaussdb"), 5432);
    }

    #[test]
    fn test_default_port_unknown_fallsback() {
        assert_eq!(default_port_for_scheme("unknown_db"), 3306);
    }

    #[test]
    fn test_ssl_url_param_mysql() {
        assert_eq!(ssl_url_param_for_scheme("mysql"), "?ssl-mode=REQUIRED");
    }

    #[test]
    fn test_ssl_url_param_oracle() {
        assert_eq!(ssl_url_param_for_scheme("oracle"), "");
    }

    #[test]
    fn test_ssl_url_param_gaussdb() {
        assert_eq!(ssl_url_param_for_scheme("gaussdb"), "?sslmode=require");
    }
}
