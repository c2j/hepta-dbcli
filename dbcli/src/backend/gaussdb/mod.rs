pub(crate) mod conn;
pub(crate) mod error;
pub(crate) mod pool;
pub(crate) mod types;

use std::sync::Arc;

use async_trait::async_trait;

use crate::backend::error::DbError;
use crate::backend::{
    BackendFactory, ChecksumSqlSpec, ColumnNormSpec, DbPool, Dialect, KeysetPageSpec, NULL_SENTINEL,
};
use crate::config::TimeoutConfig;

use self::pool::create_gaussdb_pool;

pub struct GaussdbFactory;

#[async_trait]
impl BackendFactory for GaussdbFactory {
    fn name(&self) -> &str {
        "GaussDB"
    }

    fn scheme(&self) -> &str {
        "gaussdb"
    }

    fn create_dialect(&self) -> Box<dyn Dialect> {
        Box::new(GaussdbDialect)
    }

    async fn connect(
        &self,
        url: &str,
        _timeout_config: Option<&TimeoutConfig>,
    ) -> Result<Arc<dyn DbPool>, DbError> {
        let pool = create_gaussdb_pool(url).await?;
        Ok(Arc::new(pool))
    }
}

pub(crate) struct GaussdbDialect;

impl Dialect for GaussdbDialect {
    fn database_info(&self) -> &str {
        "SELECT version()::text AS version, current_database()::text AS database, current_user::text AS current_user, inet_server_addr()::text AS hostname, inet_server_port()::text AS port, NULL::text AS os, (SELECT setting FROM pg_settings WHERE name='server_encoding')::text AS charset, (SELECT setting FROM pg_settings WHERE name='lc_collate')::text AS collation, NULL::text AS version_comment"
    }

    fn list_tables(&self) -> &str {
        "SELECT n.nspname AS schema_name, c.relname AS table_name, CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized_view' WHEN 'f' THEN 'foreign_table' WHEN 'p' THEN 'partitioned_table' END AS table_type, NULL AS engine, c.reltuples::bigint AS row_count, pg_total_relation_size(c.oid) AS total_size, obj_description(c.oid, 'pg_class') AS comment FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE c.relkind IN ('r','v','m','f','p') AND n.nspname NOT IN ('pg_catalog','information_schema') ORDER BY n.nspname, c.relname"
    }

    fn table_columns(&self) -> &str {
        "SELECT a.attname::text AS column_name, pg_catalog.format_type(a.atttypid, a.atttypmod)::text AS data_type, NOT a.attnotnull AS nullable, pg_catalog.pg_get_expr(d.adbin, d.adrelid)::text AS default_value, a.attnum::int4 AS ordinal_position, col_description(a.attrelid, a.attnum)::text AS comment, ic.relname::text AS column_key FROM pg_catalog.pg_attribute a LEFT JOIN pg_catalog.pg_attrdef d ON (a.attrelid = d.adrelid AND a.attnum = d.adnum) LEFT JOIN (pg_catalog.pg_index ix JOIN pg_catalog.pg_class ic ON ic.oid = ix.indexrelid AND ix.indisprimary) ON (ix.indrelid = a.attrelid AND a.attnum = ANY(ix.indkey)) WHERE a.attrelid = (SELECT c.oid FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE LOWER(c.relname) = LOWER($2) AND LOWER(n.nspname) = LOWER($1) ORDER BY (c.relname = $2) DESC, (n.nspname = $1) DESC, c.oid LIMIT 1) AND NOT a.attisdropped AND attnum > 0 ORDER BY a.attnum"
    }

    fn table_indexes(&self) -> &str {
        "SELECT i.relname::text AS index_name, ix.indisunique AS is_unique, ix.indisprimary AS is_primary, pg_catalog.pg_get_indexdef(ix.indexrelid)::text AS columns, am.amname::text AS index_type FROM pg_catalog.pg_index ix JOIN pg_catalog.pg_class t ON t.oid = ix.indrelid JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid JOIN pg_catalog.pg_am am ON am.oid = i.relam WHERE t.oid = (SELECT c.oid FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE LOWER(c.relname) = LOWER($2) AND LOWER(n.nspname) = LOWER($1) ORDER BY (c.relname = $2) DESC, (n.nspname = $1) DESC, c.oid LIMIT 1) ORDER BY i.relname"
    }

    fn read_only_prefixes(&self) -> &[&str] {
        &["SELECT", "EXPLAIN", "WITH"]
    }

    fn add_limit(&self, sql: &str, n: usize) -> String {
        if sql.to_uppercase().contains("LIMIT") {
            return sql.to_string();
        }
        format!("{}\nLIMIT {}", sql, n)
    }

    fn build_explain(&self, sql: &str, analyze: bool, format: &str) -> String {
        let fmt = match format.to_uppercase().as_str() {
            "TEXT" | "XML" | "JSON" | "YAML" => format.to_uppercase(),
            _ => "TEXT".to_string(),
        };
        if analyze {
            format!("EXPLAIN (ANALYZE, BUFFERS, FORMAT {}) {}", fmt, sql)
        } else {
            format!("EXPLAIN (FORMAT {}) {}", fmt, sql)
        }
    }

    fn set_statement_timeout_sql(&self, ms: u64) -> Option<String> {
        Some(format!("SET statement_timeout = {}", ms))
    }

    fn kill_own_connection_sql(&self) -> Option<String> {
        None
    }

    fn default_port(&self) -> u16 {
        5432
    }

    fn url_scheme(&self) -> &str {
        "gaussdb"
    }

    fn identifier_quote(&self) -> char {
        '"'
    }

    fn supports_hash_comment(&self) -> bool {
        false
    }

    fn supports_dollar_quote(&self) -> bool {
        true
    }

    fn begin_snapshot_sql(&self) -> &str {
        "BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY"
    }

    fn normalize_expr(&self, col: &ColumnNormSpec) -> Result<String, DbError> {
        let q = format!("\"{}\"", col.name.replace('"', "\"\""));
        let base = col
            .data_type
            .split('(')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let inner = match base.as_str() {
            "int2" | "int4" | "int8" | "smallint" | "integer" | "int" | "bigint" | "oid" => {
                format!("{q}::text")
            }
            "numeric" | "decimal" | "money" => format!("{q}::text"),
            "real" | "double precision" | "float4" | "float8" => format!("{q}::text"),
            "boolean" | "bool" => format!("{q}::int::text"),
            "timestamp without time zone" | "timestamp" => {
                format!("to_char({q}, 'YYYY-MM-DD HH24:MI:SS.US')")
            }
            "timestamp with time zone" | "timestamptz" => {
                format!("to_char({q} AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS.US')")
            }
            "date" => format!("to_char({q}, 'YYYY-MM-DD')"),
            "time without time zone" | "time" => format!("{q}::text"),
            "character" | "character varying" | "char" | "varchar" | "name" | "bpchar" => q.clone(),
            "bytea" => format!("encode({q}, 'hex')"),
            "text" | "json" | "jsonb" | "xml" | "clob" => {
                return Err(DbError::unsupported(format!(
                    "column '{}' type '{}' is excluded from checksum normalization (LOB/JSON); \
                     use --columns to select comparable columns",
                    col.name, col.data_type
                )));
            }
            other => {
                return Err(DbError::unsupported(format!(
                    "column '{}' type '{}' has no normalization rule",
                    col.name, other
                )));
            }
        };
        Ok(if col.nullable {
            format!("COALESCE({inner}, '{NULL_SENTINEL}')")
        } else {
            inner
        })
    }

    fn render_checksum_sql(&self, spec: &ChecksumSqlSpec) -> String {
        let concat = spec.normalized_exprs.join(", ");
        let row_hash = format!("MD5(concat_ws('#', {concat}))");
        let table = match &spec.schema {
            Some(s) => format!("\"{}\".\"{}\"", s, spec.table),
            None => format!("\"{}\"", spec.table),
        };
        let mut conds: Vec<String> = Vec::new();
        if let (Some(key), Some((lo, hi))) = (&spec.key_column, spec.range) {
            conds.push(format!("\"{key}\" >= {lo} AND \"{key}\" < {hi}"));
        }
        if let Some((modulus, bucket)) = spec.bucket {
            conds.push(format!(
                "MOD(('x' || SUBSTR({row_hash}, 1, 8))::bit(32)::bigint, {modulus}) = {bucket}"
            ));
        }
        if let Some(f) = &spec.filter {
            conds.push(format!("({f})"));
        }
        let where_clause = if conds.is_empty() {
            String::new()
        } else {
            format!("\n  WHERE {}", conds.join("\n    AND "))
        };
        let slice = |i: u32| {
            format!(
                "MOD(SUM(('x' || SUBSTR(h, {:2}, 8))::bit(32)::bigint), 18446744073709551616) AS s{i}",
                (i - 1) * 8 + 1
            )
        };
        format!(
            "SELECT COUNT(*) AS cnt,\n  {},\n  {},\n  {},\n  {}\nFROM (\n  SELECT {row_hash} AS h\n  FROM {table}{where_clause}\n) t",
            slice(1),
            slice(2),
            slice(3),
            slice(4)
        )
    }

    fn render_batch_checksum_sql(&self, spec: &ChecksumSqlSpec) -> String {
        let concat = spec.normalized_exprs.join(", ");
        let row_hash = format!("MD5(concat_ws('#', {concat}))");
        let table = match &spec.schema {
            Some(s) => format!("\"{}\".\"{}\"", s, spec.table),
            None => format!("\"{}\"", spec.table),
        };
        let mut conds: Vec<String> = Vec::new();
        if let (Some(key), Some((lo, hi))) = (&spec.key_column, spec.range) {
            conds.push(format!("\"{key}\" >= {lo} AND \"{key}\" < {hi}"));
        }
        if let Some(f) = &spec.filter {
            conds.push(format!("({f})"));
        }
        let where_clause = if conds.is_empty() {
            String::new()
        } else {
            format!("\n  WHERE {}", conds.join("\n    AND "))
        };
        let modulus = spec.bucket.map(|(m, _)| m).unwrap_or(1);
        let bkt = format!("MOD(('x' || SUBSTR(h, 1, 8))::bit(32)::bigint, {modulus})");
        let slice = |i: u32| {
            format!(
                "MOD(SUM(('x' || SUBSTR(h, {:2}, 8))::bit(32)::bigint), 18446744073709551616) AS s{i}",
                (i - 1) * 8 + 1
            )
        };
        format!(
            "SELECT {bkt} AS bkt,\n  COUNT(*) AS cnt,\n  {},\n  {},\n  {},\n  {}\nFROM (\n  SELECT {row_hash} AS h\n  FROM {table}{where_clause}\n) t\nGROUP BY {bkt}",
            slice(1),
            slice(2),
            slice(3),
            slice(4)
        )
    }

    fn render_bucket_predicate(&self, exprs: &[String], modulus: u64, bucket: u64) -> String {
        let concat = exprs.join(", ");
        let row_hash = format!("MD5(concat_ws('#', {concat}))");
        format!("MOD(('x' || SUBSTR({row_hash}, 1, 8))::bit(32)::bigint, {modulus}) = {bucket}")
    }

    fn render_keyset_page_sql(&self, spec: &KeysetPageSpec) -> String {
        let cols: Vec<String> = if spec.raw_exprs {
            spec.columns.clone()
        } else {
            spec.columns
                .iter()
                .map(|c| crate::backend::quote_ident('"', c))
                .collect()
        };
        let table = match &spec.schema {
            Some(s) => format!("\"{}\".\"{}\"", s, spec.table),
            None => format!("\"{}\"", spec.table),
        };
        let mut conds = crate::backend::keyset_key_conds('"', spec);
        if let Some(f) = &spec.filter {
            conds.push(format!("({f})"));
        }
        let where_clause = if conds.is_empty() {
            String::new()
        } else {
            format!("\nWHERE {}", conds.join("\n  AND "))
        };
        format!(
            "SELECT {}\nFROM {table}{where_clause}\nORDER BY {}\nLIMIT {}",
            cols.join(", "),
            crate::backend::keyset_order_by('"', spec),
            spec.page_size
        )
    }

    fn render_bucket_multiset_sql(&self, spec: &ChecksumSqlSpec) -> String {
        let Some((modulus, bucket)) = spec.bucket else {
            return String::from("-- error: bucket spec required for multiset query");
        };
        let concat = spec.normalized_exprs.join(", ");
        let row_hash = format!("MD5(concat_ws('#', {concat}))");
        let table = match &spec.schema {
            Some(s) => format!("\"{}\".\"{}\"", s, spec.table),
            None => format!("\"{}\"", spec.table),
        };
        let mut conds = vec![format!(
            "MOD(('x' || SUBSTR({row_hash}, 1, 8))::bit(32)::bigint, {modulus}) = {bucket}"
        )];
        if let Some(f) = &spec.filter {
            conds.push(format!("({f})"));
        }
        format!(
            "SELECT h, COUNT(*) AS cnt\nFROM (\n  SELECT {row_hash} AS h\n  FROM {table}\n  WHERE {}\n) t\nGROUP BY h",
            conds.join("\n    AND ")
        )
    }

    fn render_iblt_sql(&self, spec: &crate::backend::IbltSqlSpec) -> Result<String, DbError> {
        // openGauss 5.0.0 无 bit_xor 聚合（§16.3-F4）→ 逐位奇偶 SUM：
        // XOR 第 i 位 = SUM((val >> i) & 1) mod 2；key 64 位 + val 4×32 位共 192 列。
        let m = spec.cells_per_subtable;
        let concat = spec.normalized_exprs.join(", ");
        let row_hash = format!("MD5(concat_ws('#', {concat}))");
        let table = match &spec.schema {
            Some(s) => format!("\"{}\".\"{}\"", s, spec.table),
            None => format!("\"{}\"", spec.table),
        };
        let where_clause = spec
            .filter
            .as_ref()
            .map(|f| format!("\n  WHERE ({f})"))
            .unwrap_or_default();
        let mut cols = Vec::with_capacity(196);
        cols.push("COUNT(*) AS cnt".to_string());
        for b in 0..64 {
            cols.push(format!("MOD(SUM(((k::bigint >> {b}) & 1)), 2) AS kx_{b}"));
        }
        for s_idx in 1..=4u32 {
            for b in 0..32 {
                cols.push(format!(
                    "MOD(SUM(((('x' || SUBSTR(h, {:2}, 8))::bit(32)::bigint >> {b}) & 1)), 2) AS vx{s_idx}_{b}",
                    (s_idx - 1) * 8 + 1
                ));
            }
        }
        Ok(format!(
            "SELECT g.grp AS grp,\n       MOD(('x' || SUBSTR(h, g.grp * 8 - 7, 8))::bit(32)::bigint, {m}) AS cell,\n       {}\nFROM (\n  SELECT {row_hash} AS h, {key} AS k\n  FROM {table}{where_clause}\n) t\nCROSS JOIN (SELECT 1 AS grp UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4) g\nGROUP BY g.grp, cell",
            cols.join(",\n       "),
            key = spec.key_expr
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnNormSpec {
        ColumnNormSpec {
            name: name.to_string(),
            data_type: ty.to_string(),
            nullable,
        }
    }

    #[test]
    fn table_columns_case_insensitive_schema_table() {
        let d = GaussdbDialect;
        let sql = d.table_columns();
        assert!(sql.contains("LOWER(c.relname) = LOWER($2)"));
        assert!(sql.contains("LOWER(n.nspname) = LOWER($1)"));
        assert!(sql.contains("ORDER BY (c.relname = $2) DESC"));
        assert!(sql.contains("LIMIT 1"));
    }

    #[test]
    fn table_indexes_case_insensitive_schema_table() {
        let d = GaussdbDialect;
        let sql = d.table_indexes();
        assert!(sql.contains("t.oid = (SELECT c.oid"));
        assert!(sql.contains("LOWER(c.relname) = LOWER($2)"));
        assert!(sql.contains("LOWER(n.nspname) = LOWER($1)"));
        assert!(sql.contains("ORDER BY (c.relname = $2) DESC"));
        assert!(sql.contains("LIMIT 1"));
    }

    #[test]
    fn snapshot_sql_rr_read_only() {
        let d = GaussdbDialect;
        assert_eq!(
            d.begin_snapshot_sql(),
            "BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY"
        );
        assert!(d.snapshot_scn_sql().is_none());
        assert!(d.begin_snapshot_sql_polardbx().is_none());
    }

    #[test]
    fn normalize_expr_matrix() {
        let d = GaussdbDialect;
        assert_eq!(
            d.normalize_expr(&col("id", "int4", false)).unwrap(),
            "\"id\"::text"
        );
        assert_eq!(
            d.normalize_expr(&col("amount", "numeric(20,6)", true))
                .unwrap(),
            "COALESCE(\"amount\"::text, '\u{1f}NULL\u{1f}')"
        );
        assert_eq!(
            d.normalize_expr(&col("ts", "timestamp without time zone", true))
                .unwrap(),
            "COALESCE(to_char(\"ts\", 'YYYY-MM-DD HH24:MI:SS.US'), '\u{1f}NULL\u{1f}')"
        );
        assert_eq!(
            d.normalize_expr(&col("tz", "timestamp with time zone", false))
                .unwrap(),
            "to_char(\"tz\" AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS.US')"
        );
        assert_eq!(
            d.normalize_expr(&col("flag", "boolean", false)).unwrap(),
            "\"flag\"::int::text"
        );
        assert_eq!(
            d.normalize_expr(&col("data", "bytea", true)).unwrap(),
            "COALESCE(encode(\"data\", 'hex'), '\u{1f}NULL\u{1f}')"
        );
        assert!(d.normalize_expr(&col("doc", "text", true)).is_err());
    }

    #[test]
    fn checksum_sql_shape() {
        let d = GaussdbDialect;
        let spec = ChecksumSqlSpec {
            schema: None,
            table: "orders".into(),
            key_column: Some("id".into()),
            range: Some((0, 1000)),
            bucket: None,
            filter: None,
            scn: None,
            normalized_exprs: vec!["\"id\"::text".into()],
        };
        let sql = d.render_checksum_sql(&spec);
        assert!(sql.contains("('x' || SUBSTR(h,  1, 8))::bit(32)::bigint"));
        assert!(sql.contains("MOD(SUM(("));
        assert!(sql.contains("18446744073709551616"));
        assert!(sql.contains("\"id\" >= 0 AND \"id\" < 1000"));
        assert!(sql.contains("MD5(concat_ws('#', \"id\"::text))"));
    }

    #[test]
    fn keyset_page_sql_composite_next_page() {
        let d = GaussdbDialect;
        let spec = crate::backend::KeysetPageSpec {
            schema: None,
            table: "t".into(),
            columns: vec!["k1".into(), "k2".into()],
            raw_exprs: false,
            key_columns: vec!["k1".into(), "k2".into()],
            range: None,
            last_key: Some(vec![serde_json::json!(10), serde_json::json!("ab")]),
            page_size: 50,
            filter: None,
            scn: None,
        };
        let sql = d.render_keyset_page_sql(&spec);
        assert!(
            sql.contains("(\"k1\" > 10) OR (\"k1\" = 10 AND \"k2\" > 'ab')"),
            "sql={sql}"
        );
        assert!(sql.contains("ORDER BY \"k1\", \"k2\""));
        assert!(!sql.contains("(k1,k2) >"));
    }

    #[test]
    fn batch_checksum_sql_groups_by_mod_expression() {
        let d = GaussdbDialect;
        let spec = ChecksumSqlSpec {
            schema: None,
            table: "orders".into(),
            key_column: None,
            range: None,
            bucket: Some((8, 0)),
            filter: Some("x=1".into()),
            scn: None,
            normalized_exprs: vec!["\"id\"::text".into()],
        };
        let sql = d.render_batch_checksum_sql(&spec);
        assert!(
            sql.contains("GROUP BY MOD(('x' || SUBSTR(h, 1, 8))::bit(32)::bigint, 8)"),
            "sql={sql}"
        );
        assert!(!sql.contains("GROUP BY bkt"));
        assert!(sql.contains("(x=1)"));
        assert!(sql.contains("COUNT(*) AS cnt"));
    }
}

#[cfg(all(test, feature = "integration"))]
mod integration_tests {
    use crate::backend::{DbPool, Dialect};

    #[tokio::test]
    async fn gaussdb_connect_and_select_one() {
        let Ok(url) = std::env::var("GAUSSDB_TEST_URL") else {
            return;
        };
        let (client, connection) = gaussdb::connect(&url, gaussdb::NoTls)
            .await
            .expect("GaussDB connect failed; check GAUSSDB_TEST_URL");
        tokio::spawn(async move {
            let _ = connection.await;
        });

        let row = client
            .query_one("SELECT 1::int4 AS val", &[])
            .await
            .expect("query failed");
        let val: i32 = row.get(0);
        assert_eq!(val, 1);
    }

    /// Phase 2 池重构验收：两次 acquire 得到独立连接，可并发持有各自快照事务。
    #[tokio::test]
    async fn gaussdb_pool_independent_snapshot_sessions() {
        let Ok(url) = std::env::var("GAUSSDB_TEST_URL") else {
            return;
        };
        let pool = super::pool::create_gaussdb_pool(&url)
            .await
            .expect("pool creation failed");
        let mut c1 = pool.acquire().await.expect("acquire c1");
        let mut c2 = pool.acquire().await.expect("acquire c2");

        c1.query_drop("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await
            .expect("c1 begin");
        c2.query_drop("BEGIN TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
            .await
            .expect("c2 begin");

        let n1 = c1.query("SELECT COUNT(*) FROM pg_catalog.pg_class").await;
        let n2 = c2.query("SELECT COUNT(*) FROM pg_catalog.pg_class").await;
        assert!(
            n1.is_ok() && n2.is_ok(),
            "concurrent snapshot queries must not bleed session state"
        );

        c1.query_drop("COMMIT").await.expect("c1 commit");
        c2.query_drop("COMMIT").await.expect("c2 commit");
    }

    /// 活库执行生产形态 IBLT 摘要 SQL（4 子表 CROSS JOIN 分组），
    /// 防止裸 JOIN 等方言语法差异回潮（PR#24 评审）。
    #[tokio::test]
    async fn gaussdb_iblt_summary_sql_executes_live() {
        let Ok(url) = std::env::var("GAUSSDB_TEST_URL") else {
            return;
        };
        let (client, connection) = gaussdb::connect(&url, gaussdb::NoTls)
            .await
            .expect("GaussDB connect failed; check GAUSSDB_TEST_URL");
        tokio::spawn(async move {
            let _ = connection.await;
        });

        client
            .simple_query("DROP TABLE IF EXISTS iblt_probe")
            .await
            .expect("drop probe");
        client
            .simple_query("CREATE TABLE iblt_probe (id bigint PRIMARY KEY, v varchar(32))")
            .await
            .expect("create probe");
        client
            .simple_query(
                "INSERT INTO iblt_probe VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e')",
            )
            .await
            .expect("insert probe");

        let spec = crate::backend::IbltSqlSpec {
            schema: None,
            table: "iblt_probe".to_string(),
            key_expr: "\"id\"".to_string(),
            normalized_exprs: vec![
                "\"id\"::text".to_string(),
                "COALESCE(\"v\"::text, '\\N')".to_string(),
            ],
            cells_per_subtable: 1,
            filter: None,
            scn: None,
        };
        let sql = super::GaussdbDialect
            .render_iblt_sql(&spec)
            .expect("render");
        let result = client.simple_query(&sql).await;
        client
            .simple_query("DROP TABLE IF EXISTS iblt_probe")
            .await
            .expect("cleanup probe");
        let msgs = result.expect("iblt summary SQL must execute on openGauss");
        let n_rows = msgs
            .iter()
            .filter(|m| matches!(m, gaussdb::SimpleQueryMessage::Row(_)))
            .count();
        assert_eq!(n_rows, 4, "expect one row per hash subtable");
    }

    /// 连接失败必须暴露 SQLSTATE + 服务端消息，而非裸 "db error"（issue #25）。
    #[tokio::test]
    async fn gaussdb_connect_error_surfaces_sqlstate() {
        let Ok(url) = std::env::var("GAUSSDB_TEST_URL") else {
            return;
        };
        // GAUSSDB_TEST_URL 为 libpq key=value 形式；用错误密码替换原值以触发认证失败。
        // URL 形式（gaussdb://…）不含 "password="，无法安全注入，直接跳过。
        let Some(pos) = url.find("password=") else {
            return;
        };
        let value_start = pos + "password=".len();
        let value_end = url[value_start..]
            .find(char::is_whitespace)
            .map(|i| value_start + i)
            .unwrap_or(url.len());
        let bad = format!("{}__wrong__{}", &url[..value_start], &url[value_end..]);
        let err = super::pool::create_gaussdb_pool(&bad).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("SQLSTATE") || msg.contains("password authentication failed"),
            "connect error must surface server reason, got: {msg}"
        );
        assert!(
            !msg.starts_with("GaussDB connect failed: db error"),
            "must not be bare 'db error', got: {msg}"
        );
    }
}
