// ─── delta-diff metadata: table introspection → comparison plan ────────
//
// Builds a TablePlan from live metadata (design doc §7.1): columns via
// Dialect::table_columns, primary key via Dialect::table_indexes
// (is_primary row, columns CSV). Columns whose type has no normalization
// rule (Dialect::normalize_expr → Err) are excluded with a warning,
// unless explicitly requested via --columns (then it's an error).

use serde_json::Value;

use crate::backend::{ColumnNormSpec, DbConn, DbError, Dialect, QueryResult};

// ─── TablePlan ─────────────────────────────────────────────────────────

/// Comparison plan for one table (§五): which columns form the key,
/// which participate in the checksum, and their normalization specs.
#[derive(Debug, Clone)]
pub(crate) struct TablePlan {
    /// Primary/compare key columns (PRIMARY index or --key override).
    pub key_columns: Vec<String>,
    /// Columns participating in the checksum, ordinal order.
    pub compare_columns: Vec<String>,
    /// Normalization input specs, parallel to compare_columns.
    pub norm_specs: Vec<ColumnNormSpec>,
    /// Non-fatal issues (e.g. excluded LOB/JSON columns).
    pub warnings: Vec<String>,
}

impl TablePlan {
    /// Render §九 normalized expressions in compare order.
    pub(crate) fn normalized_exprs(&self, dialect: &dyn Dialect) -> Result<Vec<String>, DbError> {
        self.norm_specs
            .iter()
            .map(|c| dialect.normalize_expr(c))
            .collect()
    }

    /// Checksum / bucket-hash expressions: key columns first (even if
    /// `--columns` excluded them), then remaining compare columns.
    pub(crate) fn identity_hash_exprs(
        &self,
        dialect: &dyn Dialect,
    ) -> Result<Vec<String>, DbError> {
        let q = dialect.identifier_quote();
        let mut exprs = Vec::new();
        for k in &self.key_columns {
            if let Some(spec) = self.norm_specs.iter().find(|s| &s.name == k) {
                exprs.push(dialect.normalize_expr(spec)?);
            } else {
                exprs.push(crate::backend::quote_ident(q, k));
            }
        }
        for spec in &self.norm_specs {
            if !self.key_columns.iter().any(|k| k == &spec.name) {
                exprs.push(dialect.normalize_expr(spec)?);
            }
        }
        Ok(exprs)
    }
}

// ─── Plan Building ─────────────────────────────────────────────────────

/// Fetch table metadata through the connection's dialect and build the
/// comparison plan. `explicit_columns`/`explicit_key` are the user's
/// --columns/--key overrides (empty = auto).
pub(crate) async fn build_table_plan(
    conn: &mut dyn DbConn,
    schema: &str,
    table: &str,
    explicit_columns: &[String],
    explicit_key: &[String],
) -> Result<TablePlan, DbError> {
    let (col_sql, idx_sql) = {
        let d = conn.dialect();
        (d.table_columns().to_string(), d.table_indexes().to_string())
    };
    let col_result = exec_or_inline(&mut *conn, &col_sql, schema, table).await?;
    if col_result.rows.is_empty() {
        return Err(DbError::config(format!(
            "delta-diff: table '{schema}.{table}' not found or has no columns"
        )));
    }
    let idx_result = exec_or_inline(&mut *conn, &idx_sql, schema, table).await?;

    let columns: Vec<ColumnRow> = col_result
        .rows
        .iter()
        .map(|r| parse_column_row(r))
        .collect();

    let key_columns = if explicit_key.is_empty() {
        primary_key_columns(&idx_result)
    } else {
        let mut keys = Vec::with_capacity(explicit_key.len());
        for k in explicit_key {
            match find_column_ci(&columns, k) {
                Some(col) => keys.push(col.name.clone()),
                None => {
                    return Err(DbError::config(format!(
                        "delta-diff: --key column '{k}' not found in '{schema}.{table}'"
                    )))
                }
            }
        }
        keys
    };

    let mut compare_columns = Vec::new();
    let mut norm_specs = Vec::new();
    let mut warnings = Vec::new();
    if explicit_columns.is_empty() {
        for col in &columns {
            let spec = col.norm_spec();
            match conn.dialect().normalize_expr(&spec) {
                Ok(_) => {
                    compare_columns.push(col.name.clone());
                    norm_specs.push(spec);
                }
                Err(e) => warnings.push(format!(
                    "column '{schema}.{table}.{}' excluded from comparison: {e}",
                    col.name
                )),
            }
        }
    } else {
        for name in explicit_columns {
            let col = find_column_ci(&columns, name).ok_or_else(|| {
                DbError::config(format!(
                    "delta-diff: --columns column '{name}' not found in '{schema}.{table}'"
                ))
            })?;
            let spec = col.norm_spec();
            // Explicitly requested columns must be comparable — propagate Err.
            conn.dialect().normalize_expr(&spec)?;
            compare_columns.push(col.name.clone());
            norm_specs.push(spec);
        }
    }

    Ok(TablePlan {
        key_columns,
        compare_columns,
        norm_specs,
        warnings,
    })
}

// ─── Metadata Row Parsing ──────────────────────────────────────────────

/// 优先 exec 预编译绑定；PolarDB-X 对 information_schema 的预编译查询报
/// "unknown NPE"（CN 缺陷）时回退为内联字面量的文本查询。
/// schema/table 经单引号转义（'' 规则），与 --where 的原样拼接语义不同。
async fn exec_or_inline(
    conn: &mut dyn DbConn,
    sql: &str,
    schema: &str,
    table: &str,
) -> Result<QueryResult, DbError> {
    let params = [
        Value::String(schema.to_string()),
        Value::String(table.to_string()),
    ];
    match conn.exec(sql, &params).await {
        Ok(r) => Ok(r),
        Err(exec_err) => {
            let inlined = inline_schema_table(sql, schema, table, conn.dialect().url_scheme());
            // 回退也失败时报回退错误（更接近真实原因，如 table not found）；
            // 仅在回退不可能表达时保留原始 exec 错误上下文
            match conn.query(&inlined).await {
                Ok(r) => Ok(r),
                Err(query_err) => Err(DbError::query(format!(
                    "exec failed ({exec_err}); inline fallback failed ({query_err})"
                ))),
            }
        }
    }
}

fn inline_schema_table(sql: &str, schema: &str, table: &str, scheme: &str) -> String {
    let esc = |v: &str| format!("'{}'", v.replace('\'', "''"));
    match scheme {
        "oracle" => sql
            .replacen(":1", &esc(schema), 1)
            .replacen(":2", &esc(table), 1),
        // $1/$2 may repeat (LOWER($2) in WHERE + ORDER BY exact-match tiebreak).
        "gaussdb" => sql.replace("$1", &esc(schema)).replace("$2", &esc(table)),
        _ => sql
            .replacen('?', &esc(schema), 1)
            .replacen('?', &esc(table), 1),
    }
}

/// Row layout per Dialect::table_columns contract:
/// [column_name, data_type, nullable, default_value, ordinal_position, comment, column_key]
struct ColumnRow {
    name: String,
    data_type: String,
    nullable: bool,
}

impl ColumnRow {
    fn norm_spec(&self) -> ColumnNormSpec {
        ColumnNormSpec {
            name: self.name.clone(),
            data_type: self.data_type.clone(),
            nullable: self.nullable,
        }
    }
}

fn parse_column_row(row: &[Value]) -> ColumnRow {
    ColumnRow {
        name: value_str(row.first()),
        data_type: value_str(row.get(1)),
        nullable: value_bool(row.get(2)),
    }
}

fn find_column_ci<'a>(columns: &'a [ColumnRow], name: &str) -> Option<&'a ColumnRow> {
    if let Some(c) = columns.iter().find(|c| c.name == name) {
        return Some(c);
    }
    let mut ci = columns.iter().filter(|c| c.name.eq_ignore_ascii_case(name));
    match (ci.next(), ci.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// Extract primary key columns from a table_indexes result: the
/// is_primary=true row's columns field, CSV-parsed.
fn primary_key_columns(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .find(|r| value_bool(r.get(2)))
        .map(|r| parse_index_columns(&value_str(r.get(3))))
        .unwrap_or_default()
}

/// Parse the columns field of a PRIMARY index row. MySQL returns a CSV
/// ("id, user_id" from GROUP_CONCAT); GaussDB returns pg_get_indexdef
/// output ("CREATE UNIQUE INDEX ... USING btree (id, user_id)") where the
/// parenthesized tail holds the column list.
fn parse_index_columns(raw: &str) -> Vec<String> {
    let s = raw.trim();
    let csv = match (s.find('('), s.rfind(')')) {
        (Some(open), Some(close)) if open < close => &s[open + 1..close],
        _ => s,
    };
    csv.split(',')
        .map(|p| {
            p.trim()
                .trim_matches(|c| c == '"' || c == '`' || c == '\'')
                .to_string()
        })
        .filter(|p| !p.is_empty())
        .collect()
}

fn value_str(v: Option<&Value>) -> String {
    let raw = match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => return String::new(),
    };
    decode_blob_hex(&raw)
}

/// MySQL wire protocol reports GROUP_CONCAT and many information_schema
/// string columns as MYSQL_TYPE_BLOB; mysql_async surfaces them as bytes
/// and mysql/types.rs renders them as `"0x" + hex`. Decode that form back
/// to UTF-8 so we see the real column type / index column name. Passes
/// through non-hex / non-prefixed strings unchanged.
fn decode_blob_hex(s: &str) -> String {
    let Some(rest) = s.strip_prefix("0x") else {
        return s.to_string();
    };
    if rest.is_empty() {
        return String::new();
    }
    if rest.len() % 2 != 0 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return s.to_string();
    }
    match hex_decode_ascii(rest) {
        Ok(bytes) => String::from_utf8(bytes).unwrap_or_else(|_| s.to_string()),
        Err(_) => s.to_string(),
    }
}

fn hex_decode_ascii(s: &str) -> Result<Vec<u8>, ()> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i + 1 < bytes.len() {
        let hi = hex_nibble(bytes[i]).ok_or(())?;
        let lo = hex_nibble(bytes[i + 1]).ok_or(())?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn value_bool(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().map(|i| i != 0).unwrap_or(false),
        Some(Value::String(s)) => {
            matches!(
                s.to_ascii_lowercase().as_str(),
                "true" | "t" | "yes" | "y" | "1"
            )
        }
        _ => false,
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::mysql::dialect::MySqlDialect;
    use async_trait::async_trait;
    use serde_json::json;

    #[test]
    fn identity_hash_exprs_includes_key_excluded_from_columns() {
        let plan = TablePlan {
            key_columns: vec!["id".into()],
            compare_columns: vec!["c_int".into()],
            norm_specs: vec![ColumnNormSpec {
                name: "c_int".into(),
                data_type: "int".into(),
                nullable: false,
            }],
            warnings: vec![],
        };
        let exprs = plan
            .identity_hash_exprs(&MySqlDialect)
            .expect("identity exprs");
        assert!(
            exprs.iter().any(|e| e.contains("`id`")),
            "key must be in checksum hash: {exprs:?}"
        );
        assert!(
            exprs.iter().any(|e| e.contains("c_int")),
            "compare col must remain: {exprs:?}"
        );
    }

    // ── Mock connection serving canned metadata results ──

    struct MockConn {
        dialect: MySqlDialect,
        columns: QueryResult,
        indexes: QueryResult,
    }

    #[async_trait]
    impl DbConn for MockConn {
        async fn query(&mut self, _sql: &str) -> Result<QueryResult, DbError> {
            Err(DbError::unsupported("mock: query not supported"))
        }

        async fn exec(&mut self, sql: &str, _params: &[Value]) -> Result<QueryResult, DbError> {
            if sql == self.dialect.table_columns() {
                Ok(self.columns.clone())
            } else if sql == self.dialect.table_indexes() {
                Ok(self.indexes.clone())
            } else {
                Err(DbError::query(format!("mock: unexpected sql: {sql}")))
            }
        }

        async fn query_drop(&mut self, _sql: &str) -> Result<(), DbError> {
            Err(DbError::unsupported("mock: query_drop not supported"))
        }

        fn dialect(&self) -> &dyn Dialect {
            &self.dialect
        }
    }

    fn col_row(name: &str, ty: &str, nullable: bool, column_key: &str) -> Vec<Value> {
        vec![
            json!(name),
            json!(ty),
            json!(nullable),
            Value::Null,
            json!(1),
            Value::Null,
            json!(column_key),
        ]
    }

    fn as_result(rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: vec![],
            row_count: rows.len(),
            rows,
        }
    }

    /// verify_t fixture shape (tests/delta-diff-verify/gen_fixture.py).
    fn verify_columns() -> QueryResult {
        as_result(vec![
            col_row("id", "int", false, "PRI"),
            col_row("c_int", "int", true, ""),
            col_row("c_dec", "decimal(20,6)", true, ""),
            col_row("c_dt", "datetime", true, ""),
            col_row("c_vc", "varchar(64)", true, ""),
            col_row("c_bool", "tinyint(1)", true, ""),
            col_row("c_null", "int", true, ""),
        ])
    }

    fn primary_index(columns_csv: &str) -> QueryResult {
        as_result(vec![vec![
            json!("PRIMARY"),
            json!(true),
            json!(true),
            json!(columns_csv),
            json!("BTREE"),
        ]])
    }

    fn mock(columns: QueryResult, indexes: QueryResult) -> MockConn {
        MockConn {
            dialect: MySqlDialect,
            columns,
            indexes,
        }
    }

    // ── Plan building ──

    #[tokio::test]
    async fn plan_from_mysql_metadata() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[])
            .await
            .unwrap();
        assert_eq!(plan.key_columns, vec!["id"]);
        assert_eq!(
            plan.compare_columns,
            vec!["id", "c_int", "c_dec", "c_dt", "c_vc", "c_bool", "c_null"]
        );
        assert_eq!(plan.norm_specs.len(), 7);
        assert!(plan.warnings.is_empty());
        // Every planned column must normalize under the same dialect.
        let exprs = plan.normalized_exprs(conn.dialect()).unwrap();
        assert_eq!(exprs.len(), 7);
        assert_eq!(exprs[0], "CAST(`id` AS CHAR)");
    }

    #[tokio::test]
    async fn composite_primary_key_csv_parsed() {
        let mut conn = mock(verify_columns(), primary_index("id, c_int"));
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[])
            .await
            .unwrap();
        assert_eq!(plan.key_columns, vec!["id", "c_int"]);
    }

    #[tokio::test]
    async fn non_primary_indexes_ignored() {
        let mut idx = primary_index("id");
        idx.rows.push(vec![
            json!("idx_c_int"),
            json!(false),
            json!(false),
            json!("c_int"),
            json!("BTREE"),
        ]);
        idx.row_count += 1;
        let mut conn = mock(verify_columns(), idx);
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[])
            .await
            .unwrap();
        assert_eq!(plan.key_columns, vec!["id"]);
    }

    #[tokio::test]
    async fn no_primary_index_yields_empty_key() {
        let mut conn = mock(verify_columns(), as_result(vec![]));
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[])
            .await
            .unwrap();
        assert!(plan.key_columns.is_empty());
    }

    #[tokio::test]
    async fn unnormalizable_column_excluded_with_warning() {
        let mut cols = verify_columns();
        cols.rows.push(col_row("doc", "text", true, ""));
        cols.row_count += 1;
        let mut conn = mock(cols, primary_index("id"));
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[])
            .await
            .unwrap();
        assert!(!plan.compare_columns.contains(&"doc".to_string()));
        assert_eq!(plan.compare_columns.len(), 7);
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.warnings[0].contains("doc"), "{}", plan.warnings[0]);
    }

    #[tokio::test]
    async fn explicit_columns_with_unnormalizable_type_errors() {
        let mut cols = verify_columns();
        cols.rows.push(col_row("doc", "text", true, ""));
        cols.row_count += 1;
        let mut conn = mock(cols, primary_index("id"));
        let explicit = vec!["id".to_string(), "doc".to_string()];
        let err = build_table_plan(&mut conn, "verify", "verify_t", &explicit, &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("doc"), "{err}");
    }

    #[tokio::test]
    async fn explicit_columns_unknown_column_errors() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let explicit = vec!["id".to_string(), "nope".to_string()];
        let err = build_table_plan(&mut conn, "verify", "verify_t", &explicit, &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn explicit_key_overrides_discovery() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let key = vec!["c_int".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &key)
            .await
            .unwrap();
        assert_eq!(plan.key_columns, vec!["c_int"]);
    }

    #[tokio::test]
    async fn explicit_key_unknown_column_errors() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let key = vec!["nope".to_string()];
        let err = build_table_plan(&mut conn, "verify", "verify_t", &[], &key)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn explicit_key_case_insensitive_resolves_to_catalog_case() {
        // --key "ID" (user uppercase) must match catalog "id" and canonicalize
        // to the catalog's case, since downstream SQL double-quotes the key.
        let mut conn = mock(verify_columns(), primary_index("id"));
        let key = vec!["ID".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &key)
            .await
            .unwrap();
        assert_eq!(plan.key_columns, vec!["id"]);
    }

    #[tokio::test]
    async fn explicit_columns_case_insensitive_matches() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let explicit = vec!["ID".to_string(), "C_INT".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &explicit, &[])
            .await
            .unwrap();
        assert_eq!(plan.compare_columns, vec!["id", "c_int"]);
    }

    #[tokio::test]
    async fn explicit_key_prefers_exact_case_match() {
        let cols = as_result(vec![
            col_row("id", "int", false, "PRI"),
            col_row("ID", "int", true, ""),
        ]);
        let mut conn = mock(cols, primary_index("id"));
        let key = vec!["ID".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &key)
            .await
            .unwrap();
        assert_eq!(plan.key_columns, vec!["ID"]);
    }

    #[tokio::test]
    async fn explicit_key_ambiguous_case_errors() {
        let cols = as_result(vec![
            col_row("ID", "int", false, "PRI"),
            col_row("Id", "int", true, ""),
        ]);
        let mut conn = mock(cols, primary_index("ID"));
        let key = vec!["id".to_string()];
        let err = build_table_plan(&mut conn, "verify", "verify_t", &[], &key)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("id"), "{err}");
    }

    #[tokio::test]
    async fn missing_table_errors() {
        let mut conn = mock(as_result(vec![]), as_result(vec![]));
        let err = build_table_plan(&mut conn, "verify", "nope", &[], &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    // ── Index columns parsing ──

    #[test]
    fn parse_mysql_csv_columns() {
        assert_eq!(parse_index_columns("id"), vec!["id"]);
        assert_eq!(parse_index_columns("id, user_id"), vec!["id", "user_id"]);
    }

    #[test]
    fn parse_gauss_indexdef_columns() {
        assert_eq!(
            parse_index_columns(
                "CREATE UNIQUE INDEX verify_t_pkey ON public.verify_t USING btree (id)"
            ),
            vec!["id"]
        );
        assert_eq!(
            parse_index_columns("CREATE UNIQUE INDEX t_pkey ON public.t USING btree (id, user_id)"),
            vec!["id", "user_id"]
        );
    }

    #[test]
    fn parse_index_columns_strips_quotes() {
        assert_eq!(
            parse_index_columns("`id`, `user_id`"),
            vec!["id", "user_id"]
        );
    }

    // ── Value coercion ──

    #[test]
    fn value_coercion_forms() {
        assert_eq!(value_str(Some(&json!("abc"))), "abc");
        assert_eq!(value_str(Some(&json!(42))), "42");
        assert_eq!(value_str(Some(&Value::Null)), "");
        assert_eq!(value_str(None), "");

        assert!(value_bool(Some(&json!(true))));
        assert!(value_bool(Some(&json!(1))));
        assert!(value_bool(Some(&json!("true"))));
        assert!(value_bool(Some(&json!("YES"))));
        assert!(!value_bool(Some(&json!(false))));
        assert!(!value_bool(Some(&json!(0))));
        assert!(!value_bool(Some(&json!(""))));
        assert!(!value_bool(None));
    }

    #[test]
    fn decode_blob_hex_unwraps_mysql_b_blob_typed_strings() {
        // MySQL wire protocol reports information_schema string columns
        // and GROUP_CONCAT results as MYSQL_TYPE_BLOB; mysql/types.rs
        // surfaces them as the `"0x" + hex` form. Decode to UTF-8.
        assert_eq!(value_str(Some(&json!("0x696e74"))), "int");
        assert_eq!(
            value_str(Some(&json!("0x646563696d616c2832302c3629"))),
            "decimal(20,6)"
        );
        assert_eq!(value_str(Some(&json!("0x"))), "");
        assert_eq!(value_str(Some(&json!("PRI"))), "PRI");
        assert_eq!(value_str(Some(&json!("0xZZ"))), "0xZZ");
        assert_eq!(value_str(Some(&json!("0x6"))), "0x6");
    }

    // ── Inline fallback ──

    #[test]
    fn inline_schema_table_replaces_all_gaussdb_markers() {
        let sql = "WHERE LOWER(n.nspname) = LOWER($1) AND LOWER(c.relname) = LOWER($2) \
                   ORDER BY (c.relname = $2) DESC, (n.nspname = $1) DESC LIMIT 1";
        let out = inline_schema_table(sql, "bigfund", "dat_fund_cjqs", "gaussdb");
        assert!(!out.contains("$1"));
        assert!(!out.contains("$2"));
        assert!(out.contains("'bigfund'"));
        assert!(out.contains("'dat_fund_cjqs'"));
    }
}
