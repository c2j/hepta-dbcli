use crate::backend::error::DbError;
use crate::backend::{
    ChecksumSqlSpec, ColumnNormSpec, Dialect, HashCapability, KeysetPageSpec, NULL_SENTINEL,
};

pub(crate) struct MySqlDialect;

impl Dialect for MySqlDialect {
    fn database_info(&self) -> &str {
        "SELECT VERSION() AS version, DATABASE() AS `database`, CURRENT_USER() AS `current_user`, \
         @@hostname AS hostname, @@port AS port, @@version_compile_os AS os, \
         @@character_set_server AS charset, @@collation_server AS collation, \
         @@version_comment AS version_comment"
    }

    fn list_tables(&self) -> &str {
        "SELECT t.TABLE_SCHEMA AS schema_name, t.TABLE_NAME AS table_name, \
         t.TABLE_TYPE AS table_type, t.ENGINE AS engine, t.TABLE_ROWS AS row_count, \
         t.DATA_LENGTH + t.INDEX_LENGTH AS total_size, t.TABLE_COMMENT AS `comment` \
         FROM information_schema.TABLES t \
         WHERE t.TABLE_SCHEMA NOT IN ('mysql', 'information_schema', 'performance_schema', 'sys') \
         ORDER BY t.TABLE_SCHEMA, t.TABLE_NAME"
    }

    fn table_columns(&self) -> &str {
        "SELECT c.COLUMN_NAME AS column_name, c.COLUMN_TYPE AS data_type, \
         IF(c.IS_NULLABLE = 'YES', true, false) AS nullable, \
         c.COLUMN_DEFAULT AS default_value, c.ORDINAL_POSITION AS ordinal_position, \
         c.COLUMN_COMMENT AS `comment`, c.COLUMN_KEY AS column_key \
         FROM information_schema.COLUMNS c \
         WHERE c.TABLE_SCHEMA = ? AND c.TABLE_NAME = ? \
         ORDER BY c.ORDINAL_POSITION"
    }

    fn table_indexes(&self) -> &str {
        "SELECT s.INDEX_NAME AS index_name, NOT s.NON_UNIQUE AS is_unique, \
         IF(s.INDEX_NAME = 'PRIMARY', true, false) AS is_primary, \
         GROUP_CONCAT(s.COLUMN_NAME ORDER BY s.SEQ_IN_INDEX SEPARATOR ', ') AS columns, \
         s.INDEX_TYPE AS index_type \
         FROM information_schema.STATISTICS s \
         WHERE s.TABLE_SCHEMA = ? AND s.TABLE_NAME = ? \
         GROUP BY s.INDEX_NAME, s.NON_UNIQUE, s.INDEX_TYPE \
         ORDER BY s.INDEX_NAME"
    }

    fn read_only_prefixes(&self) -> &[&str] {
        &["SELECT", "EXPLAIN", "SHOW", "DESC", "DESCRIBE"]
    }

    fn add_limit(&self, sql: &str, n: usize) -> String {
        let upper = sql.trim().to_uppercase();
        if upper.contains("LIMIT") || upper.contains("TOP ") {
            sql.trim().to_string()
        } else {
            format!("{} LIMIT {}", sql.trim(), n)
        }
    }

    fn build_explain(&self, sql: &str, analyze: bool, format: &str) -> String {
        if analyze {
            format!("EXPLAIN ANALYZE {}", sql)
        } else {
            let format_clause = match format.to_uppercase().as_str() {
                "JSON" => "FORMAT=JSON",
                _ => "",
            };
            if format_clause.is_empty() {
                format!("EXPLAIN {}", sql)
            } else {
                format!("EXPLAIN {} {}", format_clause, sql)
            }
        }
    }

    fn set_statement_timeout_sql(&self, ms: u64) -> Option<String> {
        Some(format!("SET max_execution_time = {}", ms))
    }

    fn kill_own_connection_sql(&self) -> Option<String> {
        Some("KILL CONNECTION CONNECTION_ID()".to_string())
    }

    fn default_port(&self) -> u16 {
        3306
    }

    fn url_scheme(&self) -> &str {
        "mysql"
    }

    fn identifier_quote(&self) -> char {
        '`'
    }

    fn supports_hash_comment(&self) -> bool {
        true
    }

    fn begin_snapshot_sql(&self) -> &str {
        "START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY"
    }

    fn begin_snapshot_sql_polardbx(&self) -> Option<[&'static str; 2]> {
        Some([
            "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            "START TRANSACTION READ ONLY",
        ])
    }

    fn hash_capability(&self) -> HashCapability {
        HashCapability::Md5
    }

    fn normalize_expr(&self, col: &ColumnNormSpec) -> Result<String, DbError> {
        let q = format!("`{}`", col.name.replace('`', "``"));
        let base = col
            .data_type
            .split('(')
            .next()
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let inner = match base.as_str() {
            "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "year"
            | "bit" => format!("CAST({q} AS CHAR)"),
            "decimal" | "numeric" => format!("CAST({q} AS CHAR)"),
            "float" | "double" | "real" => format!("CAST({q} AS CHAR)"),
            "datetime" | "timestamp" => {
                format!("DATE_FORMAT({q}, '%Y-%m-%d %H:%i:%s.%f')")
            }
            "date" => format!("DATE_FORMAT({q}, '%Y-%m-%d')"),
            "time" => format!("CAST({q} AS CHAR)"),
            "char" | "varchar" | "enum" | "set" => q.clone(),
            "binary" | "varbinary" | "blob" | "tinyblob" | "mediumblob" | "longblob" => {
                format!("HEX({q})")
            }
            "tinytext" | "text" | "mediumtext" | "longtext" | "json" | "geometry" => {
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
        let row_hash = format!("MD5(CONCAT_WS('#', {concat}))");
        let table = match &spec.schema {
            Some(s) => format!("`{}`.`{}`", s, spec.table),
            None => format!("`{}`", spec.table),
        };
        let mut conds: Vec<String> = Vec::new();
        if let (Some(key), Some((lo, hi))) = (&spec.key_column, spec.range) {
            conds.push(format!("`{key}` >= {lo} AND `{key}` < {hi}"));
        }
        if let Some((modulus, bucket)) = spec.bucket {
            conds.push(format!(
                "MOD(CONV(SUBSTRING({row_hash}, 1, 8), 16, 10), {modulus}) = {bucket}"
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
                "MOD(SUM(CONV(SUBSTRING(h, {:2}, 8), 16, 10)), 18446744073709551616) AS s{i}",
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
        let row_hash = format!("MD5(CONCAT_WS('#', {concat}))");
        let table = match &spec.schema {
            Some(s) => format!("`{}`.`{}`", s, spec.table),
            None => format!("`{}`", spec.table),
        };
        let mut conds: Vec<String> = Vec::new();
        if let (Some(key), Some((lo, hi))) = (&spec.key_column, spec.range) {
            conds.push(format!("`{key}` >= {lo} AND `{key}` < {hi}"));
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
            let key_hash = format!("MD5(CONCAT_WS('#', {}))", spec.key_hash_exprs.join(", "));
            (
                format!("SELECT {key_hash} AS kh, {row_hash} AS h"),
                "kh".to_string(),
            )
        };
        let bkt = format!("MOD(CONV(SUBSTRING({bkt_src}, 1, 8), 16, 10), {modulus})");
        let slice = |i: u32| {
            format!(
                "MOD(SUM(CONV(SUBSTRING(h, {:2}, 8), 16, 10)), 18446744073709551616) AS s{i}",
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
        let concat = exprs.join(", ");
        let row_hash = format!("MD5(CONCAT_WS('#', {concat}))");
        format!("MOD(CONV(SUBSTRING({row_hash}, 1, 8), 16, 10), {modulus}) = {bucket}")
    }

    fn render_keyset_page_sql(&self, spec: &KeysetPageSpec) -> String {
        let cols: Vec<String> = if spec.raw_exprs {
            spec.columns.clone()
        } else {
            spec.columns
                .iter()
                .map(|c| crate::backend::quote_ident('`', c))
                .collect()
        };
        let table = match &spec.schema {
            Some(s) => format!("`{}`.`{}`", s, spec.table),
            None => format!("`{}`", spec.table),
        };
        let mut conds = crate::backend::keyset_key_conds('`', spec, true, "mysql");
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
            crate::backend::keyset_order_by('`', spec, "mysql"),
            spec.page_size
        )
    }

    fn render_bucket_multiset_sql(&self, spec: &ChecksumSqlSpec) -> String {
        let Some((modulus, bucket)) = spec.bucket else {
            return String::from("-- error: bucket spec required for multiset query");
        };
        let concat = spec.normalized_exprs.join(", ");
        let row_hash = format!("MD5(CONCAT_WS('#', {concat}))");
        let table = match &spec.schema {
            Some(s) => format!("`{}`.`{}`", s, spec.table),
            None => format!("`{}`", spec.table),
        };
        let mut conds = vec![format!(
            "MOD(CONV(SUBSTRING({row_hash}, 1, 8), 16, 10), {modulus}) = {bucket}"
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
        let m = spec.cells_per_subtable;
        let concat = spec.normalized_exprs.join(", ");
        let row_hash = format!("MD5(CONCAT_WS('#', {concat}))");
        let table = match &spec.schema {
            Some(s) => format!("`{}`.`{}`", s, spec.table),
            None => format!("`{}`", spec.table),
        };
        let where_clause = spec
            .filter
            .as_ref()
            .map(|f| format!("\n  WHERE ({f})"))
            .unwrap_or_default();
        let val_xor = |i: u32| {
            format!(
                "BIT_XOR(CONV(SUBSTRING(h, {:2}, 8), 16, 10)) AS val_xor_{i}",
                (i - 1) * 8 + 1
            )
        };
        Ok(format!(
            "SELECT g.grp AS grp,\n       MOD(CONV(SUBSTRING(h, g.grp * 8 - 7, 8), 16, 10), {m}) AS cell,\n       COUNT(*) AS cnt,\n       BIT_XOR(CAST(k AS UNSIGNED)) AS key_xor,\n       {},\n       {},\n       {},\n       {}\nFROM (\n  SELECT {row_hash} AS h, {key} AS k\n  FROM {table}{where_clause}\n) t\nCROSS JOIN (SELECT 1 AS grp UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4) g\nGROUP BY g.grp, cell",
            val_xor(1),
            val_xor(2),
            val_xor(3),
            val_xor(4),
            key = spec.key_expr
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::is_polardbx_version;

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnNormSpec {
        ColumnNormSpec {
            name: name.to_string(),
            data_type: ty.to_string(),
            nullable,
        }
    }

    #[test]
    fn snapshot_sql_variants() {
        let d = MySqlDialect;
        assert_eq!(
            d.begin_snapshot_sql(),
            "START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY"
        );
        let pdx = d.begin_snapshot_sql_polardbx().expect("mysql family");
        assert_eq!(pdx[0], "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ");
        assert_eq!(pdx[1], "START TRANSACTION READ ONLY");
        assert!(d.snapshot_scn_sql().is_none());
    }

    #[test]
    fn polardbx_version_detection() {
        assert!(is_polardbx_version("5.6.29-PXC-5.4.19-SNAPSHOT"));
        assert!(is_polardbx_version("8.0.30-pxc-cluster"));
        assert!(!is_polardbx_version("8.0.36-0ubuntu0.22.04.1"));
        assert!(!is_polardbx_version("5.7.44-log"));
    }

    #[test]
    fn normalize_expr_matrix() {
        let d = MySqlDialect;
        assert_eq!(
            d.normalize_expr(&col("id", "int", false)).unwrap(),
            "CAST(`id` AS CHAR)"
        );
        assert_eq!(
            d.normalize_expr(&col("amount", "decimal(20,6)", true))
                .unwrap(),
            "COALESCE(CAST(`amount` AS CHAR), '\u{1f}NULL\u{1f}')"
        );
        assert_eq!(
            d.normalize_expr(&col("create_time", "datetime", true))
                .unwrap(),
            "COALESCE(DATE_FORMAT(`create_time`, '%Y-%m-%d %H:%i:%s.%f'), '\u{1f}NULL\u{1f}')"
        );
        assert_eq!(
            d.normalize_expr(&col("name", "varchar(32)", false))
                .unwrap(),
            "`name`"
        );
        assert_eq!(
            d.normalize_expr(&col("flag", "tinyint(1)", false)).unwrap(),
            "CAST(`flag` AS CHAR)"
        );
        assert_eq!(
            d.normalize_expr(&col("data", "blob", true)).unwrap(),
            "COALESCE(HEX(`data`), '\u{1f}NULL\u{1f}')"
        );
        assert!(d.normalize_expr(&col("doc", "text", true)).is_err());
        assert!(d.normalize_expr(&col("j", "json", true)).is_err());
    }

    #[test]
    fn checksum_sql_full_table() {
        let d = MySqlDialect;
        let spec = ChecksumSqlSpec {
            schema: Some("test".into()),
            table: "orders".into(),
            key_column: None,
            range: None,
            bucket: None,
            filter: None,
            scn: None,
            normalized_exprs: vec!["CAST(`id` AS CHAR)".into(), "`name`".into()],
            key_hash_exprs: vec![],
        };
        let sql = d.render_checksum_sql(&spec);
        assert!(
            sql.contains("MOD(SUM(CONV(SUBSTRING(h,  1, 8), 16, 10)), 18446744073709551616) AS s1")
        );
        assert!(
            sql.contains("MOD(SUM(CONV(SUBSTRING(h, 25, 8), 16, 10)), 18446744073709551616) AS s4")
        );
        assert!(sql.contains("MD5(CONCAT_WS('#', CAST(`id` AS CHAR), `name`))"));
        assert!(sql.contains("FROM `test`.`orders`"));
        assert!(!sql.contains("WHERE"));
        assert!(
            !sql.contains("CAST(SUM"),
            "saturating CAST form must not appear"
        );
    }

    #[test]
    fn checksum_sql_range_bucket_filter() {
        let d = MySqlDialect;
        let spec = ChecksumSqlSpec {
            schema: None,
            table: "orders".into(),
            key_column: Some("id".into()),
            range: Some((0, 1000)),
            bucket: Some((8, 3)),
            filter: Some("status = 'paid'".into()),
            scn: None,
            normalized_exprs: vec!["CAST(`id` AS CHAR)".into()],
            key_hash_exprs: vec![],
        };
        let sql = d.render_checksum_sql(&spec);
        assert!(sql.contains("`id` >= 0 AND `id` < 1000"));
        assert!(sql.contains("MOD(CONV(SUBSTRING(MD5("));
        assert!(sql.contains(", 8) = 3"));
        assert!(sql.contains("(status = 'paid')"));
    }

    #[test]
    fn keyset_page_sql() {
        let d = MySqlDialect;
        let spec = KeysetPageSpec {
            schema: Some("test".into()),
            table: "orders".into(),
            columns: vec!["id".into(), "amount".into()],
            raw_exprs: false,
            key_columns: vec!["id".into()],
            string_key: vec![false],
            range: Some((0, 100000)),
            last_key: Some(vec![serde_json::json!(8191)]),
            page_size: 8192,
            filter: None,
            scn: None,
        };
        let sql = d.render_keyset_page_sql(&spec);
        assert!(sql.contains("SELECT `id`, `amount`"));
        assert!(sql.contains("`id` >= 0 AND `id` < 100000"));
        assert!(sql.contains("`id` > 8191"));
        assert!(sql.contains("ORDER BY `id`\nLIMIT 8192"));
    }

    #[test]
    fn keyset_page_sql_composite_first_page() {
        let d = MySqlDialect;
        let spec = KeysetPageSpec {
            schema: Some("test".into()),
            table: "t".into(),
            columns: vec!["k1".into(), "k2".into(), "payload".into()],
            raw_exprs: false,
            key_columns: vec!["k1".into(), "k2".into()],
            string_key: vec![false, false],
            range: None,
            last_key: None,
            page_size: 100,
            filter: Some("bcrq='20260114'".into()),
            scn: None,
        };
        let sql = d.render_keyset_page_sql(&spec);
        assert!(sql.contains("SELECT `k1`, `k2`, `payload`"));
        assert!(sql.contains("(bcrq='20260114')"));
        assert!(sql.contains("ORDER BY `k1`, `k2`"));
        assert!(sql.contains("LIMIT 100"));
        assert!(!sql.contains("`k1` >"));
    }

    #[test]
    fn keyset_page_sql_composite_next_page() {
        let d = MySqlDialect;
        let spec = KeysetPageSpec {
            schema: None,
            table: "t".into(),
            columns: vec!["k1".into(), "k2".into()],
            raw_exprs: false,
            key_columns: vec!["k1".into(), "k2".into()],
            string_key: vec![false, false],
            range: None,
            last_key: Some(vec![serde_json::json!(10), serde_json::json!("ab")]),
            page_size: 50,
            filter: None,
            scn: None,
        };
        let sql = d.render_keyset_page_sql(&spec);
        assert!(
            sql.contains("(`k1` > 10) OR (`k1` = 10 AND `k2` > 'ab')"),
            "sql={sql}"
        );
        assert!(sql.contains("ORDER BY `k1`, `k2`"));
    }

    #[test]
    fn keyset_order_by_string_key_uses_utf8mb4_bin() {
        let spec = KeysetPageSpec {
            schema: None,
            table: "t".into(),
            columns: vec!["code".into()],
            raw_exprs: false,
            key_columns: vec!["code".into()],
            string_key: vec![true],
            range: None,
            last_key: None,
            page_size: 10,
            filter: None,
            scn: None,
        };
        let sql = MySqlDialect.render_keyset_page_sql(&spec);
        assert!(
            sql.contains("ORDER BY `code` COLLATE utf8mb4_bin"),
            "sql={sql}"
        );
    }

    #[test]
    fn keyset_order_by_int_key_has_no_collate() {
        let spec = KeysetPageSpec {
            schema: None,
            table: "t".into(),
            columns: vec!["id".into()],
            raw_exprs: false,
            key_columns: vec!["id".into()],
            string_key: vec![false],
            range: None,
            last_key: None,
            page_size: 10,
            filter: None,
            scn: None,
        };
        let sql = MySqlDialect.render_keyset_page_sql(&spec);
        assert!(!sql.contains("COLLATE"), "sql={sql}");
    }

    #[test]
    fn batch_checksum_sql_buckets_by_key_hash_when_set() {
        let spec = ChecksumSqlSpec {
            schema: None,
            table: "t".into(),
            key_column: None,
            range: None,
            bucket: Some((8, 0)),
            filter: None,
            scn: None,
            normalized_exprs: vec!["CAST(`id` AS CHAR)".into(), "`payload`".into()],
            key_hash_exprs: vec!["CAST(`id` AS CHAR)".into()],
        };
        let sql = MySqlDialect.render_batch_checksum_sql(&spec);
        assert!(
            sql.contains("MD5(CONCAT_WS('#', CAST(`id` AS CHAR))) AS kh"),
            "sql={sql}"
        );
        assert!(
            sql.contains("MD5(CONCAT_WS('#', CAST(`id` AS CHAR), `payload`)) AS h"),
            "sql={sql}"
        );
        assert!(
            sql.contains("GROUP BY MOD(CONV(SUBSTRING(kh, 1, 8), 16, 10), 8)"),
            "sql={sql}"
        );
        assert!(
            !sql.contains("GROUP BY MOD(CONV(SUBSTRING(h, 1, 8)"),
            "sql={sql}"
        );
    }

    #[test]
    fn batch_checksum_sql_groups_by_mod_expression() {
        let d = MySqlDialect;
        let spec = ChecksumSqlSpec {
            schema: None,
            table: "orders".into(),
            key_column: None,
            range: None,
            bucket: Some((8, 0)),
            filter: Some("x=1".into()),
            scn: None,
            normalized_exprs: vec!["CAST(`id` AS CHAR)".into()],
            key_hash_exprs: vec![],
        };
        let sql = d.render_batch_checksum_sql(&spec);
        assert!(
            sql.contains("GROUP BY MOD(CONV(SUBSTRING(h, 1, 8), 16, 10), 8)"),
            "sql={sql}"
        );
        assert!(!sql.contains("GROUP BY bkt"), "must not GROUP BY alias");
        assert!(sql.contains("(x=1)"));
        assert!(sql.contains("COUNT(*) AS cnt"));
        assert!(sql.contains("AS s1"));
    }

    #[test]
    fn bucket_predicate_matches_checksum_hash() {
        let d = MySqlDialect;
        let pred = d.render_bucket_predicate(&["CAST(`id` AS CHAR)".into()], 8, 3);
        assert!(pred.contains(
            "MOD(CONV(SUBSTRING(MD5(CONCAT_WS('#', CAST(`id` AS CHAR))), 1, 8), 16, 10), 8) = 3"
        ));
    }
}
