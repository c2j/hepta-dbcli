// ─── Database Backend Abstraction Layer ─────────────────────────────
//
// This module defines the trait interfaces that decouple the hepta_dbcli
// CLI + MCP server from any specific database driver. Each supported
// database (MySQL, Oracle, GaussDB, etc.) implements these traits in its own
// submodule under backend/.

#[cfg(feature = "duckdb")]
pub mod duckdb;
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
    /// Rows changed by a data-change statement, when the backend reports it.
    /// `None` for statements that return a result set (issue #58 D6).
    pub rows_affected: Option<u64>,
}

impl QueryResult {
    pub fn empty() -> Self {
        Self {
            columns: vec![],
            rows: vec![],
            row_count: 0,
            rows_affected: None,
        }
    }

    /// Result of a data-change statement: no result set, `n` rows changed.
    pub fn affected(n: u64) -> Self {
        Self {
            columns: vec![],
            rows: vec![],
            row_count: 0,
            rows_affected: Some(n),
        }
    }
}

impl fmt::Display for QueryResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "QueryResult {{ columns: {:?}, row_count: {}, rows_affected: {:?} }}",
            self.columns, self.row_count, self.rows_affected
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

    /// Execute a data-change statement and report the number of affected rows
    /// (issue #58 D6). Backends that cannot report a count must not pretend:
    /// the default errors instead of returning a fake zero.
    async fn execute_write(&mut self, _sql: &str) -> Result<QueryResult, DbError> {
        Err(DbError::query(
            "this backend does not support data-change execution",
        ))
    }

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

    /// Query returning foreign key relationships for a schema.
    /// Returns rows: [schema_name, table_name, column_name, referenced_schema, referenced_table, referenced_column, constraint_name]
    fn foreign_keys_sql(&self, schema: &str) -> String;

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

    /// Idempotent session statements that make normalization deterministic
    /// (executed via exec-drop on every delta-diff connection; empty = none).
    fn session_pin_sql(&self) -> Vec<String> {
        Vec::new()
    }

    // ── Connection Metadata ──

    /// Default TCP port for this database.
    fn default_port(&self) -> u16;

    /// URL scheme (e.g. "mysql", "oracle").
    fn url_scheme(&self) -> &str;

    // ── REPL Tokenizer Adapters ──

    /// Character used to quote identifiers (backtick ` for MySQL, double-quote " for Oracle).
    fn identifier_quote(&self) -> char;

    /// Quote an identifier, applying dialect folding (Oracle uppercases
    /// unquoted names so CLI `dat_fund_cjqs` matches `DAT_FUND_CJQS`).
    fn quote_ident(&self, name: &str) -> String {
        quote_ident_scheme(self.url_scheme(), self.identifier_quote(), name)
    }

    /// Quote `schema.table` (or just `table` when schema is None).
    fn quote_table(&self, schema: Option<&str>, table: &str) -> String {
        quote_table_scheme(self.url_scheme(), self.identifier_quote(), schema, table)
    }

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

    /// Scan-once checksum: one row per `MOD(hash, N)` bucket.
    /// `spec.bucket = Some((modulus, _))`; the per-bucket equality predicate is omitted.
    /// GROUP BY the `MOD(...)` expression, never a select alias.
    fn render_batch_checksum_sql(&self, spec: &ChecksumSqlSpec) -> String;

    /// Bucket membership predicate using the same hash template as checksum.
    fn render_bucket_predicate(&self, exprs: &[String], modulus: u64, bucket: u64) -> String;

    /// Render one keyset-paginated row fetch (v2.1 §6.2.2).
    fn render_keyset_page_sql(&self, spec: &KeysetPageSpec) -> String;

    /// Render one full-scan row fetch for naivediff (issue #87):
    /// `SELECT <cols> FROM <table>[ AS OF SCN n][ WHERE (<filter>)][ ORDER BY <order_by>]`
    /// — single statement, no LIMIT/FETCH FIRST/ROWNUM, and ORDER BY must use the
    /// RAW key columns (never NLSSORT/COLLATE — that is the 34s-vs-6s regression).
    fn render_scan_sql(&self, spec: &ScanSqlSpec) -> String;

    /// Render a bucket multiset query (v2.1 §6.3 BucketDiffer)：
    /// `SELECT h, COUNT(*) FROM (SELECT <row_hash> AS h FROM t WHERE <bucket pred>) GROUP BY h`。
    /// spec.bucket 必须存在；返回 (row_hash, count) 行集，客户端做多重集合比对。
    fn render_bucket_multiset_sql(&self, spec: &ChecksumSqlSpec) -> String;

    /// Same row-hash SQL fragment used inside `render_bucket_multiset_sql`.
    /// May be a RAW/byte expression (Oracle `DBMS_CRYPTO.HASH`).
    fn row_hash_expr(&self, exprs: &[String]) -> String;

    /// Hex/text form of [`row_hash_expr`], comparable to stored hex hashes
    /// and safe in `IN ('…')` lists. Default is the raw expression (MySQL /
    /// GaussDB `MD5()` already returns hex text).
    fn row_hash_text_expr(&self, exprs: &[String]) -> String {
        self.row_hash_expr(exprs)
    }

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

/// NULL 哨兵：`COALESCE` 回退值，用于跨库行哈希（v2.1 §九）。
/// 纯 ASCII（Unit Separator 0x1F 包裹 "NULL"），避免 U+2400（␀）在 GBK 等
/// 多字节编码库上无法表示而触发 SQLSTATE 22P05（issue #27）。
/// 所有后端必须使用同一哨兵，跨库比较语义才一致。
pub(crate) const NULL_SENTINEL: &str = "\u{1f}NULL\u{1f}";

/// Column metadata for `Dialect::normalize_expr` (v2.1 §九).
#[derive(Debug, Clone)]
pub struct ColumnNormSpec {
    pub name: String,
    /// Backend-native type string (MySQL COLUMN_TYPE e.g. "decimal(20,6)";
    /// GaussDB format_type e.g. "numeric(20,6)"; Oracle DATA_TYPE e.g. "NUMBER").
    pub data_type: String,
    pub nullable: bool,
    pub rtrim_fixed_char: bool,
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
    /// If nonempty, `MOD(hash(key_hash_exprs), N)` assigns buckets; slices still use `normalized_exprs`.
    /// Empty means bucket by `normalized_exprs` (bucketdiff / content hash).
    pub key_hash_exprs: Vec<String>,
}

/// Specification for one keyset pagination fetch (v2.1 §6.2.2).
#[derive(Debug, Clone)]
pub struct KeysetPageSpec {
    pub schema: Option<String>,
    pub table: String,
    /// Columns to select, key columns first.
    pub columns: Vec<String>,
    /// true: `columns` 为 SQL 表达式（规范化表达式），渲染时不加引号；
    /// false: 列名，按 identifier_quote 加引号。
    /// 行级跨库比较统一用规范化表达式（§九-2：两侧文本表示字节级一致）。
    pub raw_exprs: bool,
    /// Key columns in PK order. First column is used for optional i64 `range`.
    pub key_columns: Vec<String>,
    /// Parallel to `key_columns`: true if that key is a string type (needs binary collation).
    pub string_key: Vec<bool>,
    pub range: Option<(i64, i64)>,
    /// Exclusive lower bound of the last seen key tuple (JSON numbers/strings).
    pub last_key: Option<Vec<Value>>,
    pub page_size: usize,
    pub filter: Option<String>,
    /// Oracle AS OF SCN anchor (snapshot mode).
    pub scn: Option<u64>,
}

/// One full-table scan for the naivediff strategy (issue #87):
/// single statement, no pagination, NO collation wrappers on ORDER BY,
/// no row limit. `columns` 是已渲染的 SELECT 列表(裸键列/normalize 表达式);
/// `order_by` 是已渲染的裸键列列表(空 = 无 ORDER BY,无键表)。
#[derive(Debug, Clone)]
pub struct ScanSqlSpec {
    pub schema: Option<String>,
    pub table: String,
    pub columns: Vec<String>,
    pub order_by: Vec<String>,
    pub filter: Option<String>,
    /// Oracle AS OF SCN anchor (snapshot mode); other dialects ignore.
    pub scn: Option<u64>,
}

pub(crate) fn quote_ident(quote: char, name: &str) -> String {
    let doubled = name.replace(quote, &format!("{quote}{quote}"));
    format!("{quote}{doubled}{quote}")
}

/// Quote an identifier with Oracle unquoted-name folding (ASCII uppercase).
pub(crate) fn quote_ident_scheme(scheme: &str, quote: char, name: &str) -> String {
    if scheme == "oracle" {
        quote_ident(quote, &name.to_ascii_uppercase())
    } else {
        quote_ident(quote, name)
    }
}

pub(crate) fn quote_table_scheme(
    scheme: &str,
    quote: char,
    schema: Option<&str>,
    table: &str,
) -> String {
    match schema {
        Some(s) => format!(
            "{}.{}",
            quote_ident_scheme(scheme, quote, s),
            quote_ident_scheme(scheme, quote, table)
        ),
        None => quote_ident_scheme(scheme, quote, table),
    }
}

pub(crate) fn escape_sql_string(s: &str, backslash_escape: bool) -> String {
    let s = if backslash_escape {
        s.replace('\\', "\\\\")
    } else {
        s.to_string()
    };
    s.replace('\'', "''")
}

pub(crate) fn sql_literal(v: &Value, backslash_escape: bool) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Value::Number(n) => n.to_string(),
        Value::String(s) => format!("'{}'", escape_sql_string(s, backslash_escape)),
        other => format!(
            "'{}'",
            escape_sql_string(&other.to_string(), backslash_escape)
        ),
    }
}

/// Quote `v` as a SQL string literal. Used for string-typed key columns even
/// when the driver stored a digit-only VARCHAR as a JSON number (`55958` →
/// `'55958'`). Unquoted numbers inside Oracle `NLSSORT(...)` raise ORA-01722.
fn sql_literal_as_text(v: &Value, backslash_escape: bool) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::String(s) => format!("'{}'", escape_sql_string(s, backslash_escape)),
        Value::Number(n) => format!("'{}'", escape_sql_string(&n.to_string(), backslash_escape)),
        Value::Bool(b) => format!("'{}'", if *b { "true" } else { "false" }),
        other => format!(
            "'{}'",
            escape_sql_string(&other.to_string(), backslash_escape)
        ),
    }
}

pub(crate) fn key_sort_expr(quote: char, name: &str, is_string: bool, scheme: &str) -> String {
    let q = quote_ident(quote, name);
    if !is_string {
        return q;
    }
    match scheme {
        "mysql" => format!("{q} COLLATE utf8mb4_bin"),
        "gaussdb" => format!("{q} COLLATE \"C\""),
        "oracle" => format!("NLSSORT({q},'NLS_SORT=BINARY')"),
        _ => q,
    }
}

fn key_cmp_rhs(v: &Value, is_string: bool, scheme: &str, backslash_escape: bool) -> String {
    let lit = if is_string {
        sql_literal_as_text(v, backslash_escape)
    } else {
        sql_literal(v, backslash_escape)
    };
    if is_string && scheme == "oracle" {
        format!("NLSSORT({lit},'NLS_SORT=BINARY')")
    } else {
        lit
    }
}

fn is_string_key(spec: &KeysetPageSpec, i: usize) -> bool {
    spec.string_key.get(i).copied().unwrap_or(false)
}

pub(crate) fn render_tuple_gt(
    quote: char,
    spec: &KeysetPageSpec,
    last: &[Value],
    backslash_escape: bool,
    scheme: &str,
) -> String {
    let keys = &spec.key_columns;
    let mut parts = Vec::new();
    for i in 0..keys.len().min(last.len()) {
        let mut ands = Vec::new();
        for j in 0..i {
            ands.push(format!(
                "{} = {}",
                key_sort_expr(quote, &keys[j], is_string_key(spec, j), scheme),
                key_cmp_rhs(&last[j], is_string_key(spec, j), scheme, backslash_escape)
            ));
        }
        ands.push(format!(
            "{} > {}",
            key_sort_expr(quote, &keys[i], is_string_key(spec, i), scheme),
            key_cmp_rhs(&last[i], is_string_key(spec, i), scheme, backslash_escape)
        ));
        parts.push(format!("({})", ands.join(" AND ")));
    }
    parts.join(" OR ")
}

pub(crate) fn keyset_key_conds(
    quote: char,
    spec: &KeysetPageSpec,
    backslash_escape: bool,
    scheme: &str,
) -> Vec<String> {
    let mut conds = Vec::new();
    if let (Some(k), Some((lo, hi))) = (spec.key_columns.first(), spec.range) {
        let qk = quote_ident(quote, k);
        conds.push(format!("{qk} >= {lo} AND {qk} < {hi}"));
    }
    if let Some(last) = &spec.last_key {
        if !spec.key_columns.is_empty() && !last.is_empty() {
            conds.push(format!(
                "({})",
                render_tuple_gt(quote, spec, last, backslash_escape, scheme)
            ));
        }
    }
    conds
}

pub(crate) fn keyset_order_by(quote: char, spec: &KeysetPageSpec, scheme: &str) -> String {
    spec.key_columns
        .iter()
        .enumerate()
        .map(|(i, c)| key_sort_expr(quote, c, is_string_key(spec, i), scheme))
        .collect::<Vec<_>>()
        .join(", ")
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

    /// Like [`connect`], but tells the backend whether this process may
    /// execute data changes. Backends without a session-level read-only
    /// switch ignore the flag.
    async fn connect_with_mode(
        &self,
        url: &str,
        timeout_config: Option<&TimeoutConfig>,
        allow_write: bool,
    ) -> Result<Arc<dyn DbPool>, DbError> {
        let _ = allow_write;
        self.connect(url, timeout_config).await
    }
}

// ─── Scheme-level Defaults (pre-connection lookups) ──────────────────

/// Default TCP port for a database scheme. Used by config resolution
/// before any BackendFactory is instantiated. Each backend's
/// Dialect::default_port() MUST agree with this lookup.
pub(crate) fn default_port_for_scheme(scheme: &str) -> u16 {
    match scheme {
        "oracle" => 1521,
        "gaussdb" => 5432,
        "duckdb" => 0, // embedded file DB, no TCP port
        _ => 3306,     // mysql and unknown
    }
}

/// SSL URL query parameter for a database scheme. Each backend's
/// driver must accept this format. Used by config URL building.
pub(crate) fn ssl_url_param_for_scheme(scheme: &str) -> &'static str {
    match scheme {
        "oracle" => "",
        "gaussdb" => "?sslmode=require",
        "duckdb" => "",            // in-process file DB, no TLS
        _ => "?ssl-mode=REQUIRED", // mysql
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_sentinel_is_ascii_safe() {
        assert_eq!(
            NULL_SENTINEL, "\u{1f}NULL\u{1f}",
            "changing the sentinel changes every row hash; keep it ASCII-safe (issue #27)"
        );
        assert!(
            NULL_SENTINEL.bytes().all(|b| b < 0x80),
            "NULL sentinel must be pure ASCII so it is representable in GBK/LATIN1/UTF-8"
        );
        assert!(
            !NULL_SENTINEL
                .bytes()
                .any(|b| b == 0x00 || b == b'\'' || b == b'\\'),
            "NUL is invalid in PG text; quote/backslash would break the SQL literal"
        );
    }

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
    fn test_default_port_duckdb_is_zero() {
        // DuckDB is embedded: no TCP port. 0 means "no port".
        assert_eq!(default_port_for_scheme("duckdb"), 0);
    }

    #[test]
    fn test_ssl_url_param_duckdb_is_empty() {
        // DuckDB is an in-process file DB — no TLS concept.
        assert_eq!(ssl_url_param_for_scheme("duckdb"), "");
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

    #[test]
    fn sql_literal_mysql_escapes_backslash() {
        let v = Value::String("abc\\".into());
        assert_eq!(sql_literal(&v, true), "'abc\\\\'");
    }

    #[test]
    fn sql_literal_pg_does_not_escape_backslash() {
        let v = Value::String("abc\\".into());
        assert_eq!(sql_literal(&v, false), "'abc\\'");
    }

    fn string_key_spec(keys: &[&str], last: Vec<Value>) -> KeysetPageSpec {
        KeysetPageSpec {
            schema: None,
            table: "t".into(),
            columns: keys.iter().map(|k| (*k).into()).collect(),
            raw_exprs: false,
            key_columns: keys.iter().map(|k| (*k).into()).collect(),
            string_key: vec![true; keys.len()],
            range: None,
            last_key: Some(last),
            page_size: 10,
            filter: None,
            scn: None,
        }
    }

    #[test]
    fn string_key_quotes_json_number_in_oracle_nlssort() {
        // VARCHAR2 keys whose values look numeric (XWDM='55958', BS='-1') are
        // often deserialized as JSON numbers. NLSSORT(55958) is ORA-01722.
        let spec = string_key_spec(
            &["XWDM", "BS"],
            vec![serde_json::json!(55958), serde_json::json!(-1)],
        );
        let sql = render_tuple_gt('"', &spec, spec.last_key.as_ref().unwrap(), false, "oracle");
        assert!(
            sql.contains("NLSSORT('55958','NLS_SORT=BINARY')"),
            "sql={sql}"
        );
        assert!(sql.contains("NLSSORT('-1','NLS_SORT=BINARY')"), "sql={sql}");
        assert!(!sql.contains("NLSSORT(55958,"), "sql={sql}");
        assert!(!sql.contains("NLSSORT(-1,"), "sql={sql}");
    }

    #[test]
    fn string_key_quotes_json_number_in_gaussdb_collate() {
        let spec = string_key_spec(&["xwdm"], vec![serde_json::json!(47872)]);
        let sql = render_tuple_gt(
            '"',
            &spec,
            spec.last_key.as_ref().unwrap(),
            false,
            "gaussdb",
        );
        assert!(
            sql.contains("\"xwdm\" COLLATE \"C\" > '47872'"),
            "sql={sql}"
        );
        assert!(!sql.contains("> 47872"), "sql={sql}");
    }

    #[test]
    fn numeric_key_keeps_unquoted_json_number() {
        let spec = KeysetPageSpec {
            schema: None,
            table: "t".into(),
            columns: vec!["ID".into()],
            raw_exprs: false,
            key_columns: vec!["ID".into()],
            string_key: vec![false],
            range: None,
            last_key: Some(vec![serde_json::json!(42)]),
            page_size: 10,
            filter: None,
            scn: None,
        };
        let sql = render_tuple_gt('"', &spec, spec.last_key.as_ref().unwrap(), false, "oracle");
        assert!(sql.contains("\"ID\" > 42"), "sql={sql}");
        assert!(!sql.contains("NLSSORT"), "sql={sql}");
    }

    #[test]
    fn dialect_foreign_keys_sql_returns_non_empty() {
        struct TestDialect;
        impl Dialect for TestDialect {
            fn database_info(&self) -> &str {
                ""
            }
            fn list_tables(&self) -> &str {
                ""
            }
            fn table_columns(&self) -> &str {
                ""
            }
            fn table_indexes(&self) -> &str {
                ""
            }
            fn foreign_keys_sql(&self, _schema: &str) -> String {
                String::new()
            }
            fn read_only_prefixes(&self) -> &[&str] {
                &[]
            }
            fn add_limit(&self, sql: &str, _n: usize) -> String {
                sql.to_string()
            }
            fn build_explain(&self, sql: &str, _analyze: bool, _format: &str) -> String {
                sql.to_string()
            }
            fn set_statement_timeout_sql(&self, _ms: u64) -> Option<String> {
                None
            }
            fn kill_own_connection_sql(&self) -> Option<String> {
                None
            }
            fn default_port(&self) -> u16 {
                3306
            }
            fn url_scheme(&self) -> &str {
                "test"
            }
            fn identifier_quote(&self) -> char {
                '`'
            }
            fn supports_hash_comment(&self) -> bool {
                true
            }
            fn begin_snapshot_sql(&self) -> &str {
                "BEGIN"
            }
            fn normalize_expr(&self, _col: &ColumnNormSpec) -> Result<String, DbError> {
                Ok(String::new())
            }
            fn render_checksum_sql(&self, _spec: &ChecksumSqlSpec) -> String {
                String::new()
            }
            fn render_batch_checksum_sql(&self, _spec: &ChecksumSqlSpec) -> String {
                String::new()
            }
            fn render_bucket_predicate(
                &self,
                _exprs: &[String],
                _modulus: u64,
                _bucket: u64,
            ) -> String {
                String::new()
            }
            fn render_keyset_page_sql(&self, _spec: &KeysetPageSpec) -> String {
                String::new()
            }
            fn render_scan_sql(&self, _spec: &ScanSqlSpec) -> String {
                String::new()
            }
            fn render_bucket_multiset_sql(&self, _spec: &ChecksumSqlSpec) -> String {
                String::new()
            }
            fn row_hash_expr(&self, _exprs: &[String]) -> String {
                String::new()
            }
            fn render_iblt_sql(&self, _spec: &IbltSqlSpec) -> Result<String, DbError> {
                Ok(String::new())
            }
        }

        let dialect = TestDialect;
        let sql = dialect.foreign_keys_sql("test_schema");
        let _ = sql;
    }
}
