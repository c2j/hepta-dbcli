// ─── DuckDB dialect: introspection SQL + syntax adapters ─────────────
//
// Full Dialect surface including delta-diff rendering (issue #49 phase 2):
// checksum/bucket math via '0x'||hex::UBIGINT, IBLT bit-parity columns
// matching the GaussDB contract, all verified against in-memory DuckDB in
// the tests below.

use crate::backend::error::DbError;
use crate::backend::{
    ChecksumSqlSpec, ColumnNormSpec, Dialect, IbltSqlSpec, KeysetPageSpec, NULL_SENTINEL,
};

pub(crate) struct DuckDbDialect;

impl Dialect for DuckDbDialect {
    fn database_info(&self) -> &str {
        "SELECT version() AS version, current_database() AS database, 'duckdb' AS current_user, \
         NULL AS hostname, NULL AS port, 'duckdb' AS os, 'UTF-8' AS charset, 'UTF-8' AS collation, \
         version() AS version_comment"
    }

    fn list_tables(&self) -> &str {
        "SELECT schema_name AS schema_name, table_name AS table_name, \
         'table' AS table_type, NULL AS engine, estimated_size AS row_count, \
         NULL AS total_size, comment AS comment \
         FROM duckdb_tables() WHERE internal = false \
         UNION ALL \
         SELECT schema_name, view_name, 'view', NULL, NULL, NULL, comment \
         FROM duckdb_views() WHERE internal = false \
         ORDER BY 1, 2"
    }

    fn table_columns(&self) -> &str {
        "SELECT column_name, data_type, (is_nullable = 'YES') AS nullable, \
         column_default AS default_value, ordinal_position, \
         NULL AS comment, NULL AS column_key \
         FROM information_schema.columns \
         WHERE table_schema = ? AND table_name = ? \
         ORDER BY ordinal_position"
    }

    fn table_indexes(&self) -> &str {
        // duckdb_indexes() does not expose inline PRIMARY KEYs; union in
        // duckdb_constraints(). The CTE reuses the dialect's 2-parameter
        // (schema, table) exec contract across both branches. expressions
        // is normalized to MySQL-style CSV for metadata.rs index parsing.
        "WITH tgt AS (SELECT ? AS s, ? AS t) \
         SELECT i.index_name, i.is_unique, i.is_primary, array_to_string(i.expressions, ', ') AS columns, 'ART' AS index_type \
         FROM duckdb_indexes() i, tgt \
         WHERE i.schema_name = tgt.s AND i.table_name = tgt.t \
         UNION ALL \
         SELECT 'PRIMARY', true, true, \
                (SELECT string_agg(u.c, ', ') FROM (SELECT unnest(c.constraint_column_names) AS c) u), \
                'PRIMARY KEY' \
         FROM duckdb_constraints() c, tgt \
         WHERE c.schema_name = tgt.s AND c.table_name = tgt.t AND c.constraint_type = 'PRIMARY KEY' \
         ORDER BY index_name"
    }

    fn read_only_prefixes(&self) -> &[&str] {
        // PRAGMA is excluded: some PRAGMAs (e.g. force_checkpoint) have
        // write side effects.
        &[
            "SELECT",
            "EXPLAIN",
            "WITH",
            "SHOW",
            "DESCRIBE",
            "DESC",
            "SUMMARIZE",
        ]
    }

    fn add_limit(&self, sql: &str, n: usize) -> String {
        let upper = sql.trim_start().to_uppercase();
        // Only row-limitable queries take a trailing LIMIT; SHOW / DESCRIBE /
        // SUMMARIZE / EXPLAIN do not accept one.
        if !(upper.starts_with("SELECT") || upper.starts_with("WITH")) || upper.contains("LIMIT") {
            return sql.to_string();
        }
        format!("{}\nLIMIT {}", sql, n)
    }

    fn build_explain(&self, sql: &str, analyze: bool, _format: &str) -> String {
        // DuckDB renders a text plan tree; FORMAT JSON support is
        // version-dependent, so phase 1 always emits the text form.
        if analyze {
            format!("EXPLAIN ANALYZE {sql}")
        } else {
            format!("EXPLAIN {sql}")
        }
    }

    fn set_statement_timeout_sql(&self, _ms: u64) -> Option<String> {
        None
    }

    fn kill_own_connection_sql(&self) -> Option<String> {
        None
    }

    fn default_port(&self) -> u16 {
        0
    }

    fn url_scheme(&self) -> &str {
        "duckdb"
    }

    fn identifier_quote(&self) -> char {
        '"'
    }

    fn supports_hash_comment(&self) -> bool {
        false
    }

    fn begin_snapshot_sql(&self) -> &str {
        "BEGIN TRANSACTION"
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
            "tinyint" | "smallint" | "integer" | "bigint" | "hugeint" | "utinyint"
            | "usmallint" | "uinteger" | "ubigint" | "decimal" | "numeric" | "real" | "float"
            | "double" | "uuid" => format!("CAST({q} AS VARCHAR)"),
            "boolean" | "bool" => format!("CAST(CAST({q} AS INTEGER) AS VARCHAR)"),
            "timestamp" => format!("strftime({q}, '%Y-%m-%d %H:%M:%S.%f')"),
            "timestamp with time zone" | "timestamptz" => {
                // DuckDB stores TIMESTAMPTZ as UTC micros; rendering without
                // ICU (not in bundled builds) must avoid AT TIME ZONE. The
                // epoch-micros round-trip pins the output to UTC explicitly.
                format!(
                    "strftime(TIMESTAMP '1970-01-01 00:00:00' + to_microseconds(epoch_us({q})), '%Y-%m-%d %H:%M:%S.%f')"
                )
            }
            "date" => format!("strftime({q}, '%Y-%m-%d')"),
            "time" => format!("CAST({q} AS VARCHAR)"),
            "character" | "char" | "bpchar" if col.rtrim_fixed_char => format!("rtrim({q})"),
            "character" | "char" | "bpchar" | "character varying" | "varchar" => q.clone(),
            "text" | "json" | "blob" => {
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
        Ok(
            if col.nullable
                && col.rtrim_fixed_char
                && matches!(base.as_str(), "character" | "char" | "bpchar")
            {
                format!("COALESCE(NULLIF({inner}, ''), '{NULL_SENTINEL}')")
            } else if col.nullable {
                format!("COALESCE({inner}, '{NULL_SENTINEL}')")
            } else {
                inner
            },
        )
    }

    fn render_checksum_sql(&self, spec: &ChecksumSqlSpec) -> String {
        let row_hash = self.row_hash_expr(&spec.normalized_exprs);
        let table = quoted_table(&spec.schema, &spec.table);
        let mut conds: Vec<String> = Vec::new();
        if let (Some(key), Some((lo, hi))) = (&spec.key_column, spec.range) {
            conds.push(format!("\"{key}\" >= {lo} AND \"{key}\" < {hi}"));
        }
        if let Some((modulus, bucket)) = spec.bucket {
            conds.push(bucket_cond(&row_hash, modulus, bucket));
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
                "MOD(SUM(('0x' || SUBSTR(h, {}, 8))::UBIGINT), 18446744073709551616) AS s{i}",
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
        let row_hash = self.row_hash_expr(&spec.normalized_exprs);
        let table = quoted_table(&spec.schema, &spec.table);
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
        let (inner_select, bkt_src) = if spec.key_hash_exprs.is_empty() {
            (format!("SELECT {row_hash} AS h"), "h".to_string())
        } else {
            let key_hash = format!("MD5(concat_ws('#', {}))", spec.key_hash_exprs.join(", "));
            (
                format!("SELECT {key_hash} AS kh, {row_hash} AS h"),
                "kh".to_string(),
            )
        };
        let bkt = format!("MOD(('0x' || SUBSTR({bkt_src}, 1, 8))::UBIGINT, {modulus})");
        let slice = |i: u32| {
            format!(
                "MOD(SUM(('0x' || SUBSTR(h, {}, 8))::UBIGINT), 18446744073709551616) AS s{i}",
                (i - 1) * 8 + 1
            )
        };
        format!(
            "SELECT {bkt} AS bkt,\n  COUNT(*) AS cnt,\n  {},\n  {},\n  {},\n  {}\nFROM (\n  {inner_select}\n  FROM {table}{where_clause}\n) t\nGROUP BY {bkt}",
            slice(1),
            slice(2),
            slice(3),
            slice(4)
        )
    }

    fn render_bucket_predicate(&self, exprs: &[String], modulus: u64, bucket: u64) -> String {
        bucket_cond(&self.row_hash_expr(exprs), modulus, bucket)
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
        let table = quoted_table(&spec.schema, &spec.table);
        let mut conds = crate::backend::keyset_key_conds('"', spec, false, "duckdb");
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
            crate::backend::keyset_order_by('"', spec, "duckdb"),
            spec.page_size
        )
    }

    fn row_hash_expr(&self, exprs: &[String]) -> String {
        format!("MD5(concat_ws('#', {}))", exprs.join(", "))
    }

    fn render_bucket_multiset_sql(&self, spec: &ChecksumSqlSpec) -> String {
        let Some((modulus, bucket)) = spec.bucket else {
            return String::from("-- error: bucket spec required for multiset query");
        };
        let row_hash = self.row_hash_expr(&spec.normalized_exprs);
        let table = quoted_table(&spec.schema, &spec.table);
        let mut conds = vec![bucket_cond(&row_hash, modulus, bucket)];
        if let Some(f) = &spec.filter {
            conds.push(format!("({f})"));
        }
        format!(
            "SELECT h, COUNT(*) AS cnt\nFROM (\n  SELECT {row_hash} AS h\n  FROM {table}\n  WHERE {}\n) t\nGROUP BY h",
            conds.join("\n    AND ")
        )
    }

    fn render_iblt_sql(&self, spec: &IbltSqlSpec) -> Result<String, DbError> {
        // 与 GaussDB 相同的逐位奇偶结构（客户端按 kx_*/vx* 列名消费）：
        // XOR 第 i 位 = SUM((val >> i) & 1) mod 2；key 64 位 + val 4×32 位共 192 列。
        let m = spec.cells_per_subtable;
        let row_hash = self.row_hash_expr(&spec.normalized_exprs);
        let table = quoted_table(&spec.schema, &spec.table);
        let where_clause = spec
            .filter
            .as_ref()
            .map(|f| format!("\n  WHERE ({f})"))
            .unwrap_or_default();
        let mut cols = Vec::with_capacity(196);
        cols.push("COUNT(*) AS cnt".to_string());
        for b in 0..64 {
            cols.push(format!("MOD(SUM(((k::BIGINT >> {b}) & 1)), 2) AS kx_{b}"));
        }
        for s_idx in 1..=4u32 {
            for b in 0..32 {
                cols.push(format!(
                    "MOD(SUM(((('0x' || SUBSTR(h, {}, 8))::UBIGINT >> {b}) & 1)), 2) AS vx{s_idx}_{b}",
                    s_idx * 8 - 7
                ));
            }
        }
        Ok(format!(
            "SELECT g.grp AS grp,\n       MOD(('0x' || SUBSTR(h, g.grp * 8 - 7, 8))::UBIGINT, {m}) AS cell,\n       {}\nFROM (\n  SELECT {row_hash} AS h, {key} AS k\n  FROM {table}{where_clause}\n) t\nCROSS JOIN (SELECT 1 AS grp UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4) g\nGROUP BY g.grp, cell",
            cols.join(",\n       "),
            key = spec.key_expr
        ))
    }
}

fn quoted_table(schema: &Option<String>, table: &str) -> String {
    match schema {
        Some(s) => format!("\"{s}\".\"{table}\""),
        None => format!("\"{table}\""),
    }
}

fn bucket_cond(row_hash: &str, modulus: u64, bucket: u64) -> String {
    format!("MOD(('0x' || SUBSTR({row_hash}, 1, 8))::UBIGINT, {modulus}) = {bucket}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::NULL_SENTINEL;

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnNormSpec {
        ColumnNormSpec {
            name: name.to_string(),
            data_type: ty.to_string(),
            nullable,
            rtrim_fixed_char: false,
        }
    }

    #[test]
    fn read_only_prefixes_exclude_pragma() {
        let d = DuckDbDialect;
        let prefixes = d.read_only_prefixes();
        assert!(prefixes.contains(&"SUMMARIZE"));
        assert!(prefixes.contains(&"SHOW"));
        assert!(!prefixes.contains(&"PRAGMA"));
    }

    #[test]
    fn add_limit_appends_only_to_select_shaped_queries() {
        let d = DuckDbDialect;
        assert_eq!(d.add_limit("SELECT 1", 100), "SELECT 1\nLIMIT 100");
        assert_eq!(
            d.add_limit("  with x as (select 1) select * from x", 100),
            "  with x as (select 1) select * from x\nLIMIT 100"
        );
        assert_eq!(d.add_limit("SELECT 1 LIMIT 5", 100), "SELECT 1 LIMIT 5");
        assert_eq!(d.add_limit("SHOW TABLES", 100), "SHOW TABLES");
        assert_eq!(d.add_limit("SUMMARIZE t", 100), "SUMMARIZE t");
        assert_eq!(d.add_limit("DESCRIBE t", 100), "DESCRIBE t");
        assert_eq!(d.add_limit("EXPLAIN SELECT 1", 100), "EXPLAIN SELECT 1");
    }

    #[test]
    fn explain_forms() {
        let d = DuckDbDialect;
        assert_eq!(
            d.build_explain("SELECT 1", false, "json"),
            "EXPLAIN SELECT 1"
        );
        assert_eq!(
            d.build_explain("SELECT 1", true, "text"),
            "EXPLAIN ANALYZE SELECT 1"
        );
    }

    #[test]
    fn normalize_expr_matrix() {
        let d = DuckDbDialect;
        assert_eq!(
            d.normalize_expr(&col("id", "BIGINT", false)).unwrap(),
            "CAST(\"id\" AS VARCHAR)"
        );
        assert_eq!(
            d.normalize_expr(&col("amount", "DECIMAL(10,2)", true))
                .unwrap(),
            format!("COALESCE(CAST(\"amount\" AS VARCHAR), '{NULL_SENTINEL}')")
        );
        assert_eq!(
            d.normalize_expr(&col("ts", "TIMESTAMP", true)).unwrap(),
            format!("COALESCE(strftime(\"ts\", '%Y-%m-%d %H:%M:%S.%f'), '{NULL_SENTINEL}')")
        );
        assert_eq!(
            d.normalize_expr(&col("tz", "TIMESTAMP WITH TIME ZONE", false)).unwrap(),
            "strftime(TIMESTAMP '1970-01-01 00:00:00' + to_microseconds(epoch_us(\"tz\")), '%Y-%m-%d %H:%M:%S.%f')"
        );
        assert_eq!(
            d.normalize_expr(&col("d", "DATE", true)).unwrap(),
            format!("COALESCE(strftime(\"d\", '%Y-%m-%d'), '{NULL_SENTINEL}')")
        );
        assert_eq!(
            d.normalize_expr(&col("flag", "BOOLEAN", false)).unwrap(),
            "CAST(CAST(\"flag\" AS INTEGER) AS VARCHAR)"
        );
        assert_eq!(
            d.normalize_expr(&col("u", "UUID", true)).unwrap(),
            format!("COALESCE(CAST(\"u\" AS VARCHAR), '{NULL_SENTINEL}')")
        );
        let mut fixed = col("code", "CHARACTER(12)", true);
        assert_eq!(
            d.normalize_expr(&fixed).unwrap(),
            format!("COALESCE(\"code\", '{NULL_SENTINEL}')")
        );
        fixed.rtrim_fixed_char = true;
        assert_eq!(
            d.normalize_expr(&fixed).unwrap(),
            format!("COALESCE(NULLIF(rtrim(\"code\"), ''), '{NULL_SENTINEL}')")
        );
        for lob in ["JSON", "BLOB", "TEXT"] {
            let err = d.normalize_expr(&col("x", lob, true)).unwrap_err();
            assert!(
                err.to_string().contains("--columns"),
                "{lob} must be excluded with --columns hint: {err}"
            );
        }
        assert!(d.normalize_expr(&col("x", "STRUCT(a INT)", false)).is_err());
    }

    #[test]
    fn iblt_sql_shape_matches_gaussdb_contract() {
        let d = DuckDbDialect;
        let spec = IbltSqlSpec {
            schema: Some("main".into()),
            table: "t".into(),
            key_expr: "\"id\"".into(),
            normalized_exprs: vec!["CAST(\"id\" AS VARCHAR)".into()],
            cells_per_subtable: 3,
            filter: Some("x=1".into()),
            scn: None,
        };
        let sql = d.render_iblt_sql(&spec).expect("render");
        assert!(sql.contains("CROSS JOIN"), "sql={sql}");
        assert!(sql.contains("GROUP BY g.grp, cell"), "sql={sql}");
        assert!(
            sql.contains("AS kx_0") && sql.contains("AS kx_63"),
            "sql={sql}"
        );
        assert!(
            sql.contains("AS vx1_0") && sql.contains("AS vx4_31"),
            "sql={sql}"
        );
        assert!(
            sql.contains("MOD(('0x' || SUBSTR(h, g.grp * 8 - 7, 8))::UBIGINT, 3)"),
            "sql={sql}"
        );
        assert!(sql.contains("(x=1)"), "sql={sql}");
        assert!(sql.contains("\"main\".\"t\""), "sql={sql}");
        assert!(
            sql.contains("MD5(concat_ws('#', CAST(\"id\" AS VARCHAR)))"),
            "sql={sql}"
        );
    }

    #[test]
    fn checksum_sql_shape() {
        let d = DuckDbDialect;
        let spec = ChecksumSqlSpec {
            schema: None,
            table: "orders".into(),
            key_column: Some("id".into()),
            range: Some((0, 1000)),
            bucket: None,
            filter: None,
            scn: None,
            normalized_exprs: vec!["CAST(\"id\" AS VARCHAR)".into()],
            key_hash_exprs: vec![],
        };
        let sql = d.render_checksum_sql(&spec);
        assert!(sql.contains("('0x' || SUBSTR(h, 1, 8))::UBIGINT"));
        assert!(sql.contains("18446744073709551616"));
        assert!(sql.contains("\"id\" >= 0 AND \"id\" < 1000"));
        assert!(sql.contains("MD5(concat_ws('#', CAST(\"id\" AS VARCHAR)))"));
    }

    #[test]
    fn keyset_page_sql_composite_next_page() {
        let d = DuckDbDialect;
        let spec = crate::backend::KeysetPageSpec {
            schema: None,
            table: "t".into(),
            columns: vec!["k1".into(), "k2".into()],
            raw_exprs: false,
            key_columns: vec!["k1".into(), "k2".into()],
            string_key: vec![false, true],
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
        assert!(sql.contains("LIMIT 50"));
    }

    #[test]
    fn session_defaults_are_inert() {
        let d = DuckDbDialect;
        assert!(d.session_pin_sql().is_empty());
        assert!(d.set_statement_timeout_sql(1000).is_none());
        assert!(d.kill_own_connection_sql().is_none());
        assert_eq!(d.default_port(), 0);
        assert_eq!(d.url_scheme(), "duckdb");
        assert_eq!(d.identifier_quote(), '"');
        assert!(!d.supports_hash_comment());
        assert!(!d.supports_dollar_quote());
        assert_eq!(d.begin_snapshot_sql(), "BEGIN TRANSACTION");
        assert_eq!(d.hash_capability(), crate::backend::HashCapability::Md5);
    }
}

// ─── Live tests against in-memory DuckDB (embedded — no service needed) ──

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::backend::duckdb::pool::create_duckdb_pool;
    use crate::backend::{DbPool, NULL_SENTINEL};

    async fn memory_pool() -> Box<dyn DbPool> {
        let pool = create_duckdb_pool("duckdb://:memory:").await.expect("pool");
        Box::new(pool)
    }

    #[tokio::test]
    async fn query_roundtrip_types() {
        let mut conn = memory_pool().await.acquire().await.expect("acquire");
        let r = conn
            .query(
                "SELECT 42 AS i, 'txt' AS s, 1.5 AS f, true AS b, NULL AS n, \
                 DATE '2024-01-02' AS d, TIMESTAMP '2024-01-02 03:04:05' AS ts",
            )
            .await
            .expect("query");
        assert_eq!(r.columns.len(), 7);
        assert_eq!(r.rows[0][0], serde_json::json!(42));
        assert_eq!(r.rows[0][1], serde_json::json!("txt"));
        assert_eq!(r.rows[0][2], serde_json::json!(1.5));
        assert_eq!(r.rows[0][3], serde_json::json!(true));
        assert_eq!(r.rows[0][4], serde_json::Value::Null);
        assert_eq!(r.rows[0][5], serde_json::json!("2024-01-02"));
        assert_eq!(r.rows[0][6], serde_json::json!("2024-01-02 03:04:05"));
    }

    #[tokio::test]
    async fn introspection_sql_executes_with_contract_columns() {
        let pool = memory_pool().await;
        let mut conn = pool.acquire().await.expect("acquire");
        conn.query_drop(
            "CREATE TABLE t_live (id BIGINT PRIMARY KEY, name VARCHAR, amount DECIMAL(10,2))",
        )
        .await
        .expect("create");

        let d = DuckDbDialect;
        let tables = conn.query(d.list_tables()).await.expect("list_tables");
        let schema_col = tables
            .columns
            .iter()
            .position(|c| c == "schema_name")
            .unwrap();
        let name_col = tables
            .columns
            .iter()
            .position(|c| c == "table_name")
            .unwrap();
        assert!(
            tables
                .rows
                .iter()
                .any(|r| r[name_col] == serde_json::json!("t_live")),
            "t_live must be listed: {:?}",
            tables.rows
        );
        let _ = schema_col;

        let cols = conn
            .exec(
                d.table_columns(),
                &[serde_json::json!("main"), serde_json::json!("t_live")],
            )
            .await
            .expect("table_columns");
        for expected in [
            "column_name",
            "data_type",
            "nullable",
            "default_value",
            "ordinal_position",
            "comment",
            "column_key",
        ] {
            assert!(
                cols.columns.iter().any(|c| c == expected),
                "missing column {expected}: {:?}",
                cols.columns
            );
        }
        let name_pos = cols
            .columns
            .iter()
            .position(|c| c == "column_name")
            .unwrap();
        assert!(
            cols.rows
                .iter()
                .any(|r| r[name_pos] == serde_json::json!("name")),
            "column 'name' must be listed: {:?}",
            cols.rows
        );

        let idx = conn
            .exec(
                d.table_indexes(),
                &[serde_json::json!("main"), serde_json::json!("t_live")],
            )
            .await
            .expect("table_indexes");
        assert!(idx.columns.contains(&"index_name".to_string()));
    }

    #[tokio::test]
    async fn normalize_expr_covers_all_column_types_live() {
        let pool = memory_pool().await;
        let mut conn = pool.acquire().await.expect("acquire");
        conn.query_drop(
            "CREATE TABLE norm_probe (
                c_tiny TINYINT, c_small SMALLINT, c_int INTEGER, c_big BIGINT,
                c_huge HUGEINT, c_utiny UTINYINT, c_double DOUBLE, c_float FLOAT,
                c_dec DECIMAL(10,2), c_bool BOOLEAN, c_ts TIMESTAMP,
                c_tstz TIMESTAMP WITH TIME ZONE, c_date DATE, c_time TIME,
                c_var VARCHAR(32), c_char CHARACTER(8), c_uuid UUID,
                c_blob BLOB
            )",
        )
        .await
        .expect("create probe");
        conn.query_drop(
            "INSERT INTO norm_probe VALUES (
                1, 2, 3, 4, 5, 6, 1.5, 2.5, 9.99, true,
                TIMESTAMP '2024-01-02 03:04:05', TIMESTAMPTZ '2024-01-02 03:04:05+00',
                DATE '2024-01-02', TIME '03:04:05', 'txt', 'chr',
                '1b3e4567-e89b-12d3-a456-426614174000', '\\xAA'::BLOB
            )",
        )
        .await
        .expect("insert probe");

        let cols_sql = DuckDbDialect.table_columns().to_string();
        let cols = conn
            .exec(
                &cols_sql,
                &[serde_json::json!("main"), serde_json::json!("norm_probe")],
            )
            .await
            .expect("table_columns");
        let name_pos = cols
            .columns
            .iter()
            .position(|c| c == "column_name")
            .unwrap();
        let type_pos = cols.columns.iter().position(|c| c == "data_type").unwrap();
        assert_eq!(cols.row_count, 18);

        let excluded = ["c_blob"];
        for row in &cols.rows {
            let name = row[name_pos].as_str().unwrap().to_string();
            let data_type = row[type_pos].as_str().unwrap().to_string();
            let spec = ColumnNormSpec {
                name: name.clone(),
                data_type: data_type.clone(),
                nullable: true,
                rtrim_fixed_char: name == "c_char",
            };
            let expr = match DuckDbDialect.normalize_expr(&spec) {
                Ok(e) => e,
                Err(e) => {
                    assert!(
                        excluded.contains(&name.as_str()),
                        "column '{name}' type '{data_type}' must normalize: {e}"
                    );
                    continue;
                }
            };
            assert!(
                !excluded.contains(&name.as_str()),
                "column '{name}' type '{data_type}' must be excluded, got: {expr}"
            );
            let sql = format!("SELECT {expr} AS v FROM norm_probe");
            if let Err(e) = conn.query(&sql).await {
                panic!("column '{name}' type '{data_type}' expr failed to execute: {e}\n{sql}");
            }
        }
    }

    #[tokio::test]
    async fn checksum_sql_executes_on_duckdb() {
        let pool = memory_pool().await;
        let mut conn = pool.acquire().await.expect("acquire");
        conn.query_drop("CREATE TABLE ck (id BIGINT, v VARCHAR)")
            .await
            .expect("create");
        conn.query_drop("INSERT INTO ck VALUES (1,'a'),(2,'b'),(3,'c')")
            .await
            .expect("insert");

        let d = DuckDbDialect;
        let spec = ChecksumSqlSpec {
            schema: None,
            table: "ck".into(),
            key_column: Some("id".into()),
            range: None,
            bucket: Some((4, 1)),
            filter: None,
            scn: None,
            normalized_exprs: vec![
                "CAST(\"id\" AS VARCHAR)".into(),
                format!("COALESCE(CAST(\"v\" AS VARCHAR), '{NULL_SENTINEL}')"),
            ],
            key_hash_exprs: vec![],
        };
        let sql = d.render_checksum_sql(&spec);
        let r = conn.query(&sql).await.expect("checksum sql must execute");
        assert_eq!(r.columns.first().map(String::as_str), Some("cnt"));
        assert_eq!(r.row_count, 1);

        let multiset = d.render_bucket_multiset_sql(&spec);
        let r2 = conn
            .query(&multiset)
            .await
            .expect("multiset sql must execute");
        assert!(r2.columns.contains(&"cnt".to_string()));

        let page = KeysetPageSpec {
            schema: None,
            table: "ck".into(),
            columns: vec!["id".into(), "v".into()],
            raw_exprs: false,
            key_columns: vec!["id".into()],
            string_key: vec![false],
            range: None,
            last_key: Some(vec![serde_json::json!(1)]),
            page_size: 2,
            filter: None,
            scn: None,
        };
        let page_sql = d.render_keyset_page_sql(&page);
        let r3 = conn
            .query(&page_sql)
            .await
            .expect("keyset sql must execute");
        assert_eq!(r3.row_count, 2);
    }

    #[tokio::test]
    async fn iblt_summary_sql_executes_live() {
        let pool = memory_pool().await;
        let mut conn = pool.acquire().await.expect("acquire");
        conn.query_drop("CREATE TABLE iblt_probe (id BIGINT PRIMARY KEY, v VARCHAR)")
            .await
            .expect("create");
        conn.query_drop("INSERT INTO iblt_probe VALUES (1,'a'),(2,'b'),(3,'c'),(4,'d'),(5,'e')")
            .await
            .expect("insert");

        let spec = IbltSqlSpec {
            schema: None,
            table: "iblt_probe".into(),
            key_expr: "\"id\"".into(),
            normalized_exprs: vec![
                "CAST(\"id\" AS VARCHAR)".into(),
                format!("COALESCE(CAST(\"v\" AS VARCHAR), '{NULL_SENTINEL}')"),
            ],
            cells_per_subtable: 1,
            filter: None,
            scn: None,
        };
        let sql = DuckDbDialect.render_iblt_sql(&spec).expect("render");
        let r = conn
            .query(&sql)
            .await
            .expect("iblt summary SQL must execute");
        assert_eq!(r.row_count, 4, "one row per hash subtable");
        for expected in ["grp", "cell", "cnt", "kx_0", "kx_63", "vx1_0", "vx4_31"] {
            assert!(
                r.columns.iter().any(|c| c == expected),
                "missing column {expected}: {:?}",
                r.columns
            );
        }
    }

    #[tokio::test]
    async fn table_indexes_exposes_primary_key_live() {
        let pool = memory_pool().await;
        let mut conn = pool.acquire().await.expect("acquire");
        conn.query_drop(
            "CREATE TABLE pk_probe (id BIGINT PRIMARY KEY, name VARCHAR, code VARCHAR)",
        )
        .await
        .expect("create");
        conn.query_drop("CREATE INDEX idx_name ON pk_probe (name)")
            .await
            .expect("create index");

        let idx_sql = DuckDbDialect.table_indexes().to_string();
        let idx = conn
            .exec(
                &idx_sql,
                &[serde_json::json!("main"), serde_json::json!("pk_probe")],
            )
            .await
            .expect("table_indexes");
        let primary: Vec<&Vec<serde_json::Value>> = idx
            .rows
            .iter()
            .filter(|r| r[2] == serde_json::json!(true))
            .collect();
        assert!(
            !primary.is_empty(),
            "PRIMARY KEY must be visible via table_indexes; rows={:?}",
            idx.rows
        );
        assert!(
            primary[0][3].as_str().unwrap_or("").contains("id"),
            "primary index columns must contain 'id': {:?}",
            primary[0]
        );
    }

    #[tokio::test]
    async fn read_only_file_rejects_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("ro.duckdb");
        let path_str = path.to_string_lossy().to_string();

        // Bootstrap: pools never create files, so create the DB directly.
        {
            let boot = duckdb::Connection::open(&path).expect("bootstrap create");
            boot.execute_batch("CREATE TABLE t (id INTEGER)")
                .expect("bootstrap table");
        }

        let ro_pool = create_duckdb_pool(&format!("duckdb://{path_str}?mode=ro"))
            .await
            .expect("ro pool");
        let mut ro_conn = ro_pool.acquire().await.expect("ro acquire");
        let read = ro_conn.query("SELECT COUNT(*) FROM t").await;
        assert!(read.is_ok(), "read must work in ro mode: {read:?}");
        let write = ro_conn.query_drop("INSERT INTO t VALUES (1)").await;
        assert!(write.is_err(), "write must fail in ro mode");
    }

    #[tokio::test]
    async fn missing_file_is_an_error_not_a_create() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("missing.duckdb");
        let err = create_duckdb_pool(&format!("duckdb://{}", path.to_string_lossy()))
            .await
            .expect_err("must fail on missing file");
        assert!(err.to_string().contains("not found"), "{err}");
        assert!(!path.exists(), "must not create the file");
    }

    #[tokio::test]
    async fn exec_binds_parameters() {
        let pool = memory_pool().await;
        let mut conn = pool.acquire().await.expect("acquire");
        conn.query_drop("CREATE TABLE b (id INTEGER, s VARCHAR)")
            .await
            .expect("create");
        conn.query_drop("INSERT INTO b VALUES (1,'x'),(2,'y')")
            .await
            .expect("insert");
        let r = conn
            .exec("SELECT s FROM b WHERE id = ?", &[serde_json::json!(2)])
            .await
            .expect("exec");
        assert_eq!(r.rows[0][0], serde_json::json!("y"));
    }
}
