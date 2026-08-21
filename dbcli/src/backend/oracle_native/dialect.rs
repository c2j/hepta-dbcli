use crate::backend::error::DbError;
use crate::backend::{ChecksumSqlSpec, ColumnNormSpec, Dialect, KeysetPageSpec, NULL_SENTINEL};

pub(crate) struct OracleDialect;

impl Dialect for OracleDialect {
    fn database_info(&self) -> &str {
        "SELECT \
         (SELECT banner FROM v$version WHERE banner LIKE 'Oracle%' AND ROWNUM = 1) AS version, \
         SYS_CONTEXT('USERENV','CURRENT_SCHEMA') AS database, \
         SYS_CONTEXT('USERENV','SESSION_USER') AS current_user, \
         SYS_CONTEXT('USERENV','HOST') AS hostname, \
         CAST(NULL AS VARCHAR2(1)) AS port, \
         CAST(NULL AS VARCHAR2(1)) AS os, \
         (SELECT value FROM nls_database_parameters WHERE parameter = 'NLS_CHARACTERSET') AS charset, \
         (SELECT value FROM nls_database_parameters WHERE parameter = 'NLS_SORT') AS collation, \
         (SELECT banner FROM v$version WHERE banner LIKE 'Oracle%' AND ROWNUM = 1) AS version_comment \
         FROM dual"
    }

    fn list_tables(&self) -> &str {
        "SELECT \
         t.OWNER AS schema_name, \
         t.TABLE_NAME AS table_name, \
         t.TABLE_TYPE AS table_type, \
         NULL AS engine, \
         t.NUM_ROWS AS row_count, \
         s.BYTES AS total_size, \
         c.COMMENTS AS comment \
         FROM all_tables t \
         LEFT JOIN all_tab_comments c ON c.OWNER = t.OWNER AND c.TABLE_NAME = t.TABLE_NAME \
         LEFT JOIN dba_segments s ON s.OWNER = t.OWNER AND s.SEGMENT_NAME = t.TABLE_NAME \
         WHERE t.OWNER NOT IN ('SYS','SYSTEM','OUTLN','DBSNMP','XDB','CTXSYS','MDSYS','ORDSYS') \
         ORDER BY t.OWNER, t.TABLE_NAME"
    }

    fn table_columns(&self) -> &str {
        "SELECT \
         c.COLUMN_NAME AS column_name, \
         c.DATA_TYPE || \
           CASE \
             WHEN c.DATA_TYPE IN ('VARCHAR2','NVARCHAR2','CHAR','NCHAR','RAW') \
               THEN '(' || c.DATA_LENGTH || ')' \
             WHEN c.DATA_TYPE = 'NUMBER' \
               THEN '(' || NVL(TO_CHAR(c.DATA_PRECISION),'*') || ',' || NVL(TO_CHAR(c.DATA_SCALE),'*') || ')' \
             ELSE '' \
           END AS data_type, \
         CASE WHEN c.NULLABLE = 'Y' THEN 1 ELSE 0 END AS nullable, \
         c.DATA_DEFAULT AS default_value, \
         c.COLUMN_ID AS ordinal_position, \
         com.COMMENTS AS comment, \
         (SELECT LISTAGG(cc.CONSTRAINT_TYPE, ',') WITHIN GROUP (ORDER BY cc.CONSTRAINT_TYPE) \
          FROM all_cons_columns acc \
          JOIN all_constraints cc ON cc.CONSTRAINT_NAME = acc.CONSTRAINT_NAME \
            AND cc.OWNER = acc.OWNER \
          WHERE acc.OWNER = c.OWNER \
            AND acc.TABLE_NAME = c.TABLE_NAME \
            AND acc.COLUMN_NAME = c.COLUMN_NAME \
            AND cc.CONSTRAINT_TYPE IN ('P','U','R')) AS column_key \
         FROM all_tab_columns c \
         LEFT JOIN all_col_comments com \
           ON com.OWNER = c.OWNER AND com.TABLE_NAME = c.TABLE_NAME AND com.COLUMN_NAME = c.COLUMN_NAME \
         WHERE UPPER(c.OWNER) = UPPER(:1) AND UPPER(c.TABLE_NAME) = UPPER(:2) \
         ORDER BY c.COLUMN_ID"
    }

    fn table_indexes(&self) -> &str {
        "SELECT \
         i.INDEX_NAME AS index_name, \
         CASE WHEN i.UNIQUENESS = 'UNIQUE' THEN 1 ELSE 0 END AS is_unique, \
         CASE WHEN c.CONSTRAINT_TYPE = 'P' THEN 1 ELSE 0 END AS is_primary, \
         (SELECT LISTAGG(ic.COLUMN_NAME, ', ') WITHIN GROUP (ORDER BY ic.COLUMN_POSITION) \
          FROM all_ind_columns ic \
          WHERE ic.INDEX_OWNER = i.OWNER AND ic.INDEX_NAME = i.INDEX_NAME) AS columns, \
         i.INDEX_TYPE AS index_type \
         FROM all_indexes i \
         LEFT JOIN all_constraints c \
           ON c.OWNER = i.OWNER \
           AND c.INDEX_NAME = i.INDEX_NAME \
           AND c.CONSTRAINT_TYPE = 'P' \
         WHERE UPPER(i.OWNER) = UPPER(:1) AND UPPER(i.TABLE_NAME) = UPPER(:2) \
         ORDER BY i.INDEX_NAME"
    }

    fn read_only_prefixes(&self) -> &[&str] {
        &["SELECT", "EXPLAIN", "WITH"]
    }

    fn add_limit(&self, sql: &str, n: usize) -> String {
        let upper = sql.trim().to_uppercase();
        if upper.contains("FETCH FIRST") || upper.contains("ROWNUM") {
            sql.trim().to_string()
        } else {
            format!("{} FETCH FIRST {} ROWS ONLY", sql.trim(), n)
        }
    }

    fn build_explain(&self, sql: &str, analyze: bool, format: &str) -> String {
        if analyze {
            format!(
                "EXPLAIN PLAN SET STATEMENT_ID = 'polar_explain' FOR {}; \
                 SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY(NULL, 'polar_explain', 'ALL'))",
                sql
            )
        } else {
            let fmt = match format.to_uppercase().as_str() {
                "JSON" => "BASIC",
                _ => "TYPICAL",
            };
            format!(
                "EXPLAIN PLAN SET STATEMENT_ID = 'polar_explain' FOR {}; \
                 SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY(NULL, 'polar_explain', '{}'))",
                sql, fmt
            )
        }
    }

    fn set_statement_timeout_sql(&self, _ms: u64) -> Option<String> {
        None
    }

    fn kill_own_connection_sql(&self) -> Option<String> {
        None
    }

    fn default_port(&self) -> u16 {
        1521
    }

    fn url_scheme(&self) -> &str {
        "oracle"
    }

    fn identifier_quote(&self) -> char {
        '"'
    }

    fn supports_hash_comment(&self) -> bool {
        false
    }

    fn begin_snapshot_sql(&self) -> &str {
        "SET TRANSACTION READ ONLY"
    }

    fn snapshot_scn_sql(&self) -> Option<&'static str> {
        Some("SELECT CURRENT_SCN FROM V$DATABASE")
    }

    fn normalize_expr(&self, col: &ColumnNormSpec) -> Result<String, DbError> {
        let q = format!("\"{}\"", col.name.replace('"', "\"\""));
        let base = col.data_type.trim().to_uppercase();
        let inner = match base.as_str() {
            s if s.starts_with("TIMESTAMP") => {
                format!("TO_CHAR({q}, 'YYYY-MM-DD HH24:MI:SS.FF6')")
            }
            "DATE" => format!("TO_CHAR({q}, 'YYYY-MM-DD')"),
            "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" => q.clone(),
            "RAW" | "LONG RAW" => format!("RAWTOHEX({q})"),
            "CLOB" | "NCLOB" | "BLOB" | "LONG" | "BFILE" => {
                return Err(DbError::unsupported(format!(
                    "column '{}' type '{}' is excluded from checksum normalization (LOB); \
                     use --columns to select comparable columns",
                    col.name, col.data_type
                )));
            }
            s if s.starts_with("NUMBER")
                || s.starts_with("DECIMAL")
                || s.starts_with("NUMERIC") =>
            {
                // 标度保留掩码（§九 + 评审）：TO_CHAR 默认丢前导零/尾零，
                // NUMBER(p,s) 须按声明标度补齐以与 MySQL CAST(DECIMAL) 对齐
                match oracle_number_scale(&base) {
                    Some(scale) if scale > 0 => {
                        let mask = format!("FM{}0D{}", "9".repeat(30), "0".repeat(scale as usize));
                        format!("TO_CHAR({q}, '{mask}')")
                    }
                    _ => format!("TO_CHAR({q})"),
                }
            }
            "INTEGER" | "SMALLINT" | "FLOAT" | "BINARY_FLOAT" | "BINARY_DOUBLE" => {
                format!("TO_CHAR({q})")
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
        let concat = spec.normalized_exprs.join(" || '#' || ");
        let row_hash = format!("STANDARD_HASH({concat}, 'MD5')");
        let mut table = match &spec.schema {
            Some(s) => format!("\"{}\".\"{}\"", s, spec.table),
            None => format!("\"{}\"", spec.table),
        };
        if let Some(scn) = spec.scn {
            table.push_str(&format!(" AS OF SCN {scn}"));
        }
        let mut conds: Vec<String> = Vec::new();
        if let (Some(key), Some((lo, hi))) = (&spec.key_column, spec.range) {
            conds.push(format!("\"{key}\" >= {lo} AND \"{key}\" < {hi}"));
        }
        if let Some((modulus, bucket)) = spec.bucket {
            conds.push(format!(
                "MOD(TO_NUMBER(SUBSTR(RAWTOHEX({row_hash}), 1, 8), 'XXXXXXXX'), {modulus}) = {bucket}"
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
                "TO_CHAR(MOD(SUM(TO_NUMBER(SUBSTR(RAWTOHEX(h), {:2}, 8), 'XXXXXXXX')), POWER(2,64))) AS s{i}",
                (i - 1) * 8 + 1
            )
        };
        format!(
            "SELECT TO_CHAR(COUNT(*)) AS cnt,\n  {},\n  {},\n  {},\n  {}\nFROM (\n  SELECT {row_hash} AS h\n  FROM {table}{where_clause}\n) t",
            slice(1),
            slice(2),
            slice(3),
            slice(4)
        )
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
        let mut table = match &spec.schema {
            Some(s) => format!("\"{}\".\"{}\"", s, spec.table),
            None => format!("\"{}\"", spec.table),
        };
        if let Some(scn) = spec.scn {
            table.push_str(&format!(" AS OF SCN {scn}"));
        }
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
            "SELECT {}\nFROM {table}{where_clause}\nORDER BY {}\nFETCH FIRST {} ROWS ONLY",
            cols.join(", "),
            crate::backend::keyset_order_by('"', spec),
            spec.page_size
        )
    }

    fn render_bucket_multiset_sql(&self, spec: &ChecksumSqlSpec) -> String {
        let Some((modulus, bucket)) = spec.bucket else {
            return String::from("-- error: bucket spec required for multiset query");
        };
        let concat = spec.normalized_exprs.join(" || '#' || ");
        let row_hash = format!("STANDARD_HASH({concat}, 'MD5')");
        let mut table = match &spec.schema {
            Some(s) => format!("\"{}\".\"{}\"", s, spec.table),
            None => format!("\"{}\"", spec.table),
        };
        if let Some(scn) = spec.scn {
            table.push_str(&format!(" AS OF SCN {scn}"));
        }
        let mut conds = vec![format!(
            "MOD(TO_NUMBER(SUBSTR(RAWTOHEX({row_hash}), 1, 8), 'XXXXXXXX'), {modulus}) = {bucket}"
        )];
        if let Some(f) = &spec.filter {
            conds.push(format!("({f})"));
        }
        format!(
            "SELECT h, TO_CHAR(COUNT(*)) AS cnt\nFROM (\n  SELECT LOWER(RAWTOHEX({row_hash})) AS h\n  FROM {table}\n  WHERE {}\n) t\nGROUP BY h",
            conds.join("\n    AND ")
        )
    }
    fn render_iblt_sql(&self, spec: &crate::backend::IbltSqlSpec) -> Result<String, DbError> {
        // Oracle 21c+/23ai 原生 BIT_XOR_AGG（§16.3-F8 实测）；19c 无聚合 →
        // 路由层在版本探测后降级 hashdiff，不在此生成奇偶模板。
        let m = spec.cells_per_subtable;
        let concat = spec.normalized_exprs.join(" || '#' || ");
        let row_hash = format!("STANDARD_HASH({concat}, 'MD5')");
        let mut table = match &spec.schema {
            Some(s) => format!("\"{}\".\"{}\"", s, spec.table),
            None => format!("\"{}\"", spec.table),
        };
        if let Some(scn) = spec.scn {
            table.push_str(&format!(" AS OF SCN {scn}"));
        }
        let where_clause = spec
            .filter
            .as_ref()
            .map(|f| format!("\n  WHERE ({f})"))
            .unwrap_or_default();
        let val_xor = |i: u32| {
            format!(
                "TO_CHAR(BIT_XOR_AGG(TO_NUMBER(SUBSTR(RAWTOHEX(h), {:2}, 8), 'XXXXXXXX'))) AS val_xor_{i}",
                (i - 1) * 8 + 1
            )
        };
        Ok(format!(
            "SELECT g.grp AS grp,\n       MOD(TO_NUMBER(SUBSTR(RAWTOHEX(h), g.grp * 8 - 7, 8), 'XXXXXXXX'), {m}) AS cell,\n       TO_CHAR(COUNT(*)) AS cnt,\n       TO_CHAR(BIT_XOR_AGG(k)) AS key_xor,\n       {},\n       {},\n       {},\n       {}\nFROM (\n  SELECT {row_hash} AS h, {key} AS k\n  FROM {table}{where_clause}\n) t\nCROSS JOIN (SELECT 1 AS grp FROM dual UNION ALL SELECT 2 FROM dual UNION ALL SELECT 3 FROM dual UNION ALL SELECT 4 FROM dual) g\nGROUP BY g.grp, cell",
            val_xor(1),
            val_xor(2),
            val_xor(3),
            val_xor(4),
            key = spec.key_expr
        ))
    }
}

/// 从 "NUMBER(p,s)" 形态解析标度 s（无括号或无标度 → None）。
fn oracle_number_scale(data_type_upper: &str) -> Option<u32> {
    let open = data_type_upper.find('(')?;
    let close = data_type_upper.rfind(')')?;
    let inner = &data_type_upper[open + 1..close];
    let scale = inner.split(',').nth(1)?.trim();
    if scale == "*" {
        return None;
    }
    scale.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Dialect;

    #[test]
    fn test_database_info_contains_keywords() {
        let d = OracleDialect;
        let sql = d.database_info();
        assert!(sql.contains("v$version"));
        assert!(sql.contains("SYS_CONTEXT"));
        assert!(sql.contains("dual"));
    }

    #[test]
    fn test_list_tables_contains_all_tables() {
        let d = OracleDialect;
        let sql = d.list_tables();
        assert!(sql.contains("all_tables"));
        assert!(sql.contains("SYS"));
        assert!(sql.contains("SYSTEM"));
    }

    #[test]
    fn test_table_columns_uses_bind_params() {
        let d = OracleDialect;
        let sql = d.table_columns();
        assert!(sql.contains(":1"));
        assert!(sql.contains(":2"));
        assert!(sql.contains("all_tab_columns"));
    }

    #[test]
    fn test_table_indexes_uses_listagg() {
        let d = OracleDialect;
        let sql = d.table_indexes();
        assert!(sql.contains("LISTAGG"));
        assert!(sql.contains("all_indexes"));
        assert!(sql.contains("all_ind_columns"));
    }

    #[test]
    fn test_table_columns_case_insensitive_owner_table() {
        let d = OracleDialect;
        let sql = d.table_columns();
        assert!(sql.contains("UPPER(c.OWNER) = UPPER(:1)"));
        assert!(sql.contains("UPPER(c.TABLE_NAME) = UPPER(:2)"));
    }

    #[test]
    fn test_table_indexes_case_insensitive_owner_table() {
        let d = OracleDialect;
        let sql = d.table_indexes();
        assert!(sql.contains("UPPER(i.OWNER) = UPPER(:1)"));
        assert!(sql.contains("UPPER(i.TABLE_NAME) = UPPER(:2)"));
    }

    #[test]
    fn test_read_only_prefixes_no_show_describe() {
        let d = OracleDialect;
        let prefixes = d.read_only_prefixes();
        assert!(prefixes.contains(&"SELECT"));
        assert!(prefixes.contains(&"EXPLAIN"));
        assert!(!prefixes.contains(&"SHOW"));
        assert!(!prefixes.contains(&"DESCRIBE"));
    }

    #[test]
    fn test_add_limit_fetch_first() {
        let d = OracleDialect;
        let result = d.add_limit("SELECT * FROM dual", 10);
        assert!(result.contains("FETCH FIRST 10 ROWS ONLY"));
    }

    #[test]
    fn test_add_limit_no_double_limit() {
        let d = OracleDialect;
        let result = d.add_limit("SELECT * FROM dual FETCH FIRST 5 ROWS ONLY", 10);
        assert_eq!(result, "SELECT * FROM dual FETCH FIRST 5 ROWS ONLY");
    }

    #[test]
    fn test_build_explain_contains_dbms_xplan() {
        let d = OracleDialect;
        let sql = d.build_explain("SELECT * FROM dual", false, "TYPICAL");
        assert!(sql.contains("EXPLAIN PLAN"));
        assert!(sql.contains("DBMS_XPLAN.DISPLAY"));
        assert!(sql.contains("polar_explain"));
    }

    #[test]
    fn test_build_explain_analyze() {
        let d = OracleDialect;
        let sql = d.build_explain("SELECT * FROM dual", true, "TEXT");
        assert!(sql.contains("EXPLAIN PLAN"));
        assert!(sql.contains("ALL"));
    }

    #[test]
    fn test_no_statement_timeout() {
        let d = OracleDialect;
        assert!(d.set_statement_timeout_sql(1000).is_none());
    }

    #[test]
    fn test_no_kill_own_connection() {
        let d = OracleDialect;
        assert!(d.kill_own_connection_sql().is_none());
    }

    #[test]
    fn test_default_port() {
        assert_eq!(OracleDialect.default_port(), 1521);
    }

    #[test]
    fn test_url_scheme() {
        assert_eq!(OracleDialect.url_scheme(), "oracle");
    }

    #[test]
    fn test_identifier_quote_is_double_quote() {
        assert_eq!(OracleDialect.identifier_quote(), '"');
    }

    #[test]
    fn test_no_hash_comment() {
        assert!(!OracleDialect.supports_hash_comment());
    }

    fn col(name: &str, ty: &str, nullable: bool) -> crate::backend::ColumnNormSpec {
        crate::backend::ColumnNormSpec {
            name: name.to_string(),
            data_type: ty.to_string(),
            nullable,
        }
    }

    #[test]
    fn test_normalize_expr_matrix() {
        let d = OracleDialect;
        assert_eq!(
            d.normalize_expr(&col("ID", "NUMBER", false)).unwrap(),
            "TO_CHAR(\"ID\")"
        );
        assert_eq!(
            d.normalize_expr(&col("AMOUNT", "NUMBER", true)).unwrap(),
            "COALESCE(TO_CHAR(\"AMOUNT\"), '\u{1f}NULL\u{1f}')"
        );
        assert_eq!(
            d.normalize_expr(&col("CREATED", "DATE", false)).unwrap(),
            "TO_CHAR(\"CREATED\", 'YYYY-MM-DD')"
        );
        assert_eq!(
            d.normalize_expr(&col("TS", "TIMESTAMP(6)", false)).unwrap(),
            "TO_CHAR(\"TS\", 'YYYY-MM-DD HH24:MI:SS.FF6')"
        );
        assert_eq!(
            d.normalize_expr(&col("DATA", "RAW", true)).unwrap(),
            "COALESCE(RAWTOHEX(\"DATA\"), '\u{1f}NULL\u{1f}')"
        );
        assert!(d.normalize_expr(&col("DOC", "CLOB", true)).is_err());
    }
}
