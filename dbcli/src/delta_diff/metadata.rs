// ─── delta-diff metadata: table introspection → comparison plan ────────
//
// Builds a TablePlan from live metadata (design doc §7.1): columns via
// Dialect::table_columns, primary key via Dialect::table_indexes
// (is_primary row, columns CSV). Columns whose type has no normalization
// rule (Dialect::normalize_expr → Err) are excluded with a warning,
// unless explicitly requested via --columns (then it's an error).

use serde_json::Value;

use crate::backend::{ColumnNormSpec, DbConn, DbError, Dialect, QueryResult};
use crate::delta_diff::pairing::find_unique_ci;

// ─── TablePlan ─────────────────────────────────────────────────────────

/// Comparison plan for one table (§五): which columns form the key,
/// which participate in the checksum, and their normalization specs.
#[derive(Debug, Clone)]
pub(crate) struct TablePlan {
    /// Backend URL scheme used to select cross-database normalization checks.
    pub(crate) url_scheme: String,
    /// Primary/compare key columns (PRIMARY index or --key override).
    pub key_columns: Vec<String>,
    /// Columns participating in the checksum, ordinal order.
    pub compare_columns: Vec<String>,
    /// Normalization input specs, parallel to compare_columns.
    pub norm_specs: Vec<ColumnNormSpec>,
    /// Normalization input specs for the key columns, parallel to
    /// `key_columns`. Populated from the table's columns, so a key keeps its
    /// declared type even when --exclude-columns drops it from the compare
    /// set: routing (`is_int_key`), probe gating and key hashing must not
    /// treat an excluded key as a column of unknown type.
    pub key_specs: Vec<ColumnNormSpec>,
    /// Non-fatal issues (e.g. excluded LOB/JSON columns).
    pub warnings: Vec<String>,
}

impl TablePlan {
    pub(crate) fn is_numeric_type(data_type: &str) -> bool {
        let base = data_type
            .split('(')
            .next()
            .unwrap_or(data_type)
            .trim()
            .to_ascii_lowercase();
        matches!(
            base.as_str(),
            "tinyint"
                | "smallint"
                | "mediumint"
                | "int"
                | "integer"
                | "bigint"
                | "int2"
                | "int4"
                | "int8"
                | "oid"
                | "number"
                | "decimal"
                | "numeric"
                | "money"
        )
    }

    pub(crate) fn numeric_value_flags_for(&self, columns: &[String]) -> Vec<bool> {
        columns
            .iter()
            .map(|column| {
                // `spec_for` consults the key list first: an excluded key still
                // has to compare numerically, otherwise the client-side merge
                // orders keys as text ("10" < "2") while SQL ORDER BY is
                // numeric and the walk desynchronizes.
                self.spec_for(column)
                    .map(|spec| Self::is_numeric_type(&spec.data_type))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Whether a column's declared type can supply an integer key domain
    /// (issue #108: the bucketdiff PK-range path). Columns with no
    /// normalization spec report `true` — unknown types are left to the
    /// probe itself instead of being guessed away here.
    pub(crate) fn column_type_may_be_integer(&self, name: &str) -> bool {
        self.spec_for(name)
            .map(|spec| Self::is_numeric_type(&spec.data_type))
            .unwrap_or(true)
    }

    /// Declared-type spec for a column: the key list is consulted first so a
    /// key excluded from the compare set (`--columns`/`--exclude-columns`)
    /// keeps its type instead of looking like an unknown column.
    pub(crate) fn spec_for(&self, name: &str) -> Option<&ColumnNormSpec> {
        find_unique_ci(&self.key_specs, name, |spec| &spec.name)
            .or_else(|| find_unique_ci(&self.norm_specs, name, |spec| &spec.name))
    }

    /// Render §九 normalized expressions in compare order.
    pub(crate) fn normalized_exprs(&self, dialect: &dyn Dialect) -> Result<Vec<String>, DbError> {
        self.norm_specs
            .iter()
            .map(|c| dialect.normalize_expr(c))
            .collect()
    }

    /// Checksum / bucket-hash expressions: key columns first (even if
    /// `--columns` excluded them), then remaining compare columns.
    pub(crate) fn key_is_string(data_type: &str) -> bool {
        let b = data_type
            .split('(')
            .next()
            .unwrap_or(data_type)
            .trim()
            .to_lowercase();
        matches!(
            b.as_str(),
            "char"
                | "varchar"
                | "varchar2"
                | "nvarchar"
                | "nvarchar2"
                | "text"
                | "tinytext"
                | "mediumtext"
                | "longtext"
                | "clob"
                | "nclob"
                | "bpchar"
                | "name"
                | "character"
                | "character varying"
        )
    }

    pub(crate) fn string_key_flags(&self) -> Vec<bool> {
        self.string_key_flags_for(&self.key_columns)
    }

    pub(crate) fn string_key_flags_for(&self, keys: &[String]) -> Vec<bool> {
        keys.iter()
            .map(|k| {
                self.spec_for(k)
                    .map(|s| Self::key_is_string(&s.data_type))
                    .unwrap_or(false)
            })
            .collect()
    }

    pub(crate) fn key_hash_exprs(&self, dialect: &dyn Dialect) -> Result<Vec<String>, DbError> {
        let q = dialect.identifier_quote();
        let mut exprs = Vec::new();
        for k in &self.key_columns {
            // Prefer the key's own spec; an unnormalizable key type falls back
            // to the raw identifier, as before.
            if let Some(expr) = self
                .spec_for(k)
                .and_then(|spec| dialect.normalize_expr(spec).ok())
            {
                exprs.push(expr);
            } else {
                exprs.push(crate::backend::quote_ident(q, k));
            }
        }
        Ok(exprs)
    }

    pub(crate) fn identity_hash_exprs(
        &self,
        dialect: &dyn Dialect,
    ) -> Result<Vec<String>, DbError> {
        let mut exprs = self.key_hash_exprs(dialect)?;
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
/// --columns/--key overrides (empty = auto); `exclude_columns` is the
/// --exclude-columns deny list, applied on top of the resulting compare set.
pub(crate) async fn build_table_plan(
    conn: &mut dyn DbConn,
    schema: &str,
    table: &str,
    explicit_columns: &[String],
    explicit_key: &[String],
    rtrim_char_columns: bool,
    exclude_columns: &[String],
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
        // Drop tokens that are not real table columns (partition names leaked
        // from pg_get_indexdef LOCAL(...) tails, TABLESPACE, etc.).
        primary_key_columns(&idx_result)
            .into_iter()
            .filter_map(|k| find_column_ci(&columns, &k).map(|c| c.name.clone()))
            .collect()
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
            let spec = col.norm_spec(rtrim_char_columns);
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
            let spec = col.norm_spec(rtrim_char_columns);
            // Explicitly requested columns must be comparable — propagate Err.
            conn.dialect().normalize_expr(&spec)?;
            compare_columns.push(col.name.clone());
            norm_specs.push(spec);
        }
    }

    // --exclude-columns (issue #109): a deny list applied on top of the
    // discovered/--columns set. Names resolve against the table's real
    // columns (case-insensitive) so typos fail loudly instead of silently
    // comparing a column the user meant to skip.
    let mut excluded: Vec<String> = Vec::new();
    for name in exclude_columns {
        let col = find_column_ci(&columns, name).ok_or_else(|| {
            DbError::config(format!(
                "delta-diff: --exclude-columns column '{name}' not found in '{schema}.{table}'"
            ))
        })?;
        if explicit_columns
            .iter()
            .any(|c| c.eq_ignore_ascii_case(&col.name))
        {
            return Err(DbError::config(format!(
                "delta-diff: column '{}' is listed in both --columns and --exclude-columns",
                col.name
            )));
        }
        if !excluded.contains(&col.name) {
            excluded.push(col.name.clone());
        }
    }
    if !excluded.is_empty() {
        let mut kept_columns = Vec::with_capacity(compare_columns.len());
        let mut kept_specs = Vec::with_capacity(norm_specs.len());
        for (name, spec) in compare_columns.into_iter().zip(norm_specs) {
            if excluded.contains(&name) {
                continue;
            }
            kept_columns.push(name);
            kept_specs.push(spec);
        }
        compare_columns = kept_columns;
        norm_specs = kept_specs;
        warnings.push(format!(
            "column(s) excluded from comparison by --exclude-columns: {}",
            excluded.join(", ")
        ));
        for key in &key_columns {
            if excluded.contains(key) {
                warnings.push(format!(
                    "column '{schema}.{table}.{key}' is part of the row key: it still \
                     identifies rows, only its value comparison was excluded"
                ));
            }
        }
        if compare_columns.is_empty() {
            return Err(DbError::config(format!(
                "delta-diff: no columns left to compare in '{schema}.{table}' after \
                 --exclude-columns"
            )));
        }
    }

    push_key_shape_warnings(
        &mut warnings,
        &key_columns,
        &columns,
        &idx_result,
        !explicit_key.is_empty(),
    );

    // Key specs come from the discovered columns rather than from the compare
    // set: a key that --columns/--exclude-columns removed still has to answer
    // "what type is this key?" for routing, probe gating and key hashing.
    let key_specs: Vec<ColumnNormSpec> = key_columns
        .iter()
        .filter_map(|k| find_column_ci(&columns, k).map(|col| col.norm_spec(rtrim_char_columns)))
        .collect();

    Ok(TablePlan {
        url_scheme: conn.dialect().url_scheme().to_string(),
        key_columns,
        compare_columns,
        norm_specs,
        key_specs,
        warnings,
    })
}

fn is_temporal_type(ty: &str) -> bool {
    let b = ty.split('(').next().unwrap_or(ty).trim().to_lowercase();
    matches!(
        b.as_str(),
        "date"
            | "datetime"
            | "timestamp"
            | "timestamptz"
            | "time"
            | "timetz"
            | "year"
            | "timestamp without time zone"
            | "timestamp with time zone"
    )
}

fn index_covers_unique(idx: &QueryResult, keys: &[String]) -> bool {
    let mut want: Vec<String> = keys.iter().map(|k| k.to_ascii_lowercase()).collect();
    want.sort();
    for r in &idx.rows {
        if !(value_bool(r.get(1)) || value_bool(r.get(2))) {
            continue;
        }
        let mut have: Vec<String> = parse_index_columns(&value_str(r.get(3)))
            .into_iter()
            .map(|c| c.to_ascii_lowercase())
            .collect();
        have.sort();
        if have == want {
            return true;
        }
    }
    false
}

fn push_key_shape_warnings(
    warnings: &mut Vec<String>,
    keys: &[String],
    columns: &[ColumnRow],
    idx: &QueryResult,
    explicit_key: bool,
) {
    for k in keys {
        if let Some(col) = find_column_ci(columns, k) {
            if col.nullable {
                warnings.push(format!(
                    "key column '{k}' is nullable; NULL keys are skipped by keyset pagination"
                ));
            }
            if is_temporal_type(&col.data_type) {
                warnings.push(format!(
                    "key column '{k}' is temporal; Oracle keyset pagination depends on NLS_DATE_FORMAT"
                ));
            }
        }
    }
    if explicit_key && !keys.is_empty() && !index_covers_unique(idx, keys) {
        warnings.push(
            "explicit --key is not backed by a unique/primary index; \
             duplicate keys can drop rows at page boundaries"
                .to_string(),
        );
    }
}

// ─── Metadata Row Parsing ──────────────────────────────────────────────

/// 优先 exec 预编译绑定；PolarDB-X 对 information_schema 的预编译查询报
/// "unknown NPE"（CN 缺陷）时回退为内联字面量的文本查询。
/// schema/table 经单引号转义（'' 规则），与 --where 的原样拼接语义不同。
pub(crate) async fn exec_or_inline(
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
    fn norm_spec(&self, rtrim_fixed_char: bool) -> ColumnNormSpec {
        ColumnNormSpec {
            name: self.name.clone(),
            data_type: self.data_type.clone(),
            nullable: self.nullable,
            rtrim_fixed_char,
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

/// Map column_name → data_type from a table_columns result.
pub(crate) fn column_name_and_type(
    result: &QueryResult,
) -> std::collections::HashMap<String, String> {
    result
        .rows
        .iter()
        .map(|row| {
            let parsed = parse_column_row(row);
            (parsed.name, parsed.data_type)
        })
        .filter(|(name, _)| !name.is_empty())
        .collect()
}

fn find_column_ci<'a>(columns: &'a [ColumnRow], name: &str) -> Option<&'a ColumnRow> {
    find_unique_ci(columns, name, |column| &column.name)
}

/// Extract primary key columns from a table_indexes result: the
/// is_primary=true row's columns field, CSV-parsed.
pub(crate) fn primary_key_columns(result: &QueryResult) -> Vec<String> {
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
/// first balanced parenthesis group holds the column list.
///
/// Must not use the last `)` in the string: partitioned local indexes append
/// `LOCAL(PARTITION part_…, …)` after the column list, and `rfind(')')`
/// would swallow every partition name as a fake key column.
fn parse_index_columns(raw: &str) -> Vec<String> {
    let s = raw.trim();
    let csv = first_balanced_paren_inner(s).unwrap_or(s);
    csv.split(',')
        .filter_map(|p| {
            let token = p
                .trim()
                .trim_matches(|c| c == '"' || c == '`' || c == '\'')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_matches(|c| c == '"' || c == '`' || c == '\'');
            if token.is_empty() {
                None
            } else {
                Some(token.to_string())
            }
        })
        .collect()
}

/// Inner text of the first balanced `(...)` group, or None if none exists.
fn first_balanced_paren_inner(s: &str) -> Option<&str> {
    let start = s.find('(')?;
    let mut depth = 0usize;
    for (offset, ch) in s[start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&s[start + 1..start + offset]);
                }
            }
            _ => {}
        }
    }
    None
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
            url_scheme: "mysql".into(),
            key_columns: vec!["id".into()],
            compare_columns: vec!["c_int".into()],
            norm_specs: vec![ColumnNormSpec {
                name: "c_int".into(),
                data_type: "int".into(),
                nullable: false,
                rtrim_fixed_char: false,
            }],
            warnings: vec![],
            key_specs: vec![],
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
        let key = plan.key_hash_exprs(&MySqlDialect).unwrap();
        assert_eq!(&exprs[..key.len()], &key[..]);
    }

    #[test]
    fn string_key_flags_follow_requested_key_order() {
        let plan = TablePlan {
            url_scheme: "oracle".into(),
            key_columns: vec!["A".into(), "B".into()],
            compare_columns: vec!["A".into(), "B".into()],
            norm_specs: vec![
                ColumnNormSpec {
                    name: "A".into(),
                    data_type: "NUMBER".into(),
                    nullable: false,
                    rtrim_fixed_char: false,
                },
                ColumnNormSpec {
                    name: "B".into(),
                    data_type: "VARCHAR2".into(),
                    nullable: false,
                    rtrim_fixed_char: false,
                },
            ],
            warnings: vec![],
            key_specs: vec![],
        };

        assert_eq!(
            plan.string_key_flags_for(&["B".into(), "A".into()]),
            vec![true, false]
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
            rows_affected: None,
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
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &[])
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
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &[])
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
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &[])
            .await
            .unwrap();
        assert_eq!(plan.key_columns, vec!["id"]);
    }

    #[tokio::test]
    async fn no_primary_index_yields_empty_key() {
        let mut conn = mock(verify_columns(), as_result(vec![]));
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &[])
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
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &[])
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
        let err = build_table_plan(&mut conn, "verify", "verify_t", &explicit, &[], false, &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("doc"), "{err}");
    }

    #[tokio::test]
    async fn explicit_columns_unknown_column_errors() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let explicit = vec!["id".to_string(), "nope".to_string()];
        let err = build_table_plan(&mut conn, "verify", "verify_t", &explicit, &[], false, &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn nullable_key_adds_warning() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let plan = build_table_plan(
            &mut conn,
            "verify",
            "verify_t",
            &[],
            &["c_int".into()],
            false,
            &[],
        )
        .await
        .unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("nullable")),
            "{:?}",
            plan.warnings
        );
    }

    #[tokio::test]
    async fn date_key_adds_warning() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let plan = build_table_plan(
            &mut conn,
            "verify",
            "verify_t",
            &[],
            &["c_dt".into()],
            false,
            &[],
        )
        .await
        .unwrap();
        assert!(
            plan.warnings.iter().any(|w| w.contains("temporal")),
            "{:?}",
            plan.warnings
        );
    }

    #[tokio::test]
    async fn non_unique_explicit_key_adds_warning() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let plan = build_table_plan(
            &mut conn,
            "verify",
            "verify_t",
            &[],
            &["c_vc".into()],
            false,
            &[],
        )
        .await
        .unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("unique/primary index")),
            "{:?}",
            plan.warnings
        );
    }

    #[tokio::test]
    async fn primary_key_has_no_uniqueness_warning() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &[])
            .await
            .unwrap();
        assert!(
            plan.warnings
                .iter()
                .all(|w| !w.contains("unique/primary index")),
            "{:?}",
            plan.warnings
        );
    }

    #[tokio::test]
    async fn explicit_key_overrides_discovery() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let key = vec!["c_int".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &key, false, &[])
            .await
            .unwrap();
        assert_eq!(plan.key_columns, vec!["c_int"]);
    }

    #[tokio::test]
    async fn explicit_key_unknown_column_errors() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let key = vec!["nope".to_string()];
        let err = build_table_plan(&mut conn, "verify", "verify_t", &[], &key, false, &[])
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
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &key, false, &[])
            .await
            .unwrap();
        assert_eq!(plan.key_columns, vec!["id"]);
    }

    #[tokio::test]
    async fn explicit_columns_case_insensitive_matches() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let explicit = vec!["ID".to_string(), "C_INT".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &explicit, &[], false, &[])
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
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &key, false, &[])
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
        let err = build_table_plan(&mut conn, "verify", "verify_t", &[], &key, false, &[])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("id"), "{err}");
    }

    #[tokio::test]
    async fn missing_table_errors() {
        let mut conn = mock(as_result(vec![]), as_result(vec![]));
        let err = build_table_plan(&mut conn, "verify", "nope", &[], &[], false, &[])
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

    #[test]
    fn parse_gauss_partitioned_local_indexdef_keeps_only_key_columns() {
        let def = "CREATE UNIQUE INDEX dat_fund_cjqs_pkey ON bigfund.dat_fund_cjqs \
             USING btree (xwdm, security_id, scdm, fund_code, trade_type, bs, \
             pay_type, stock_kind, bcrq, etf_flag, gddm, gddmzm, check_type, mom_fund) \
             LOCAL(PARTITION part_202401_xwdm_security_id_scdm_fund_code_trade_type_bs_p_idx, \
             PARTITION part_202402_xwdm_security_id_scdm_fund_code_trade_type_bs_p_idx, \
             PARTITION part_202601_xwdm_security_id_scdm_fund_code_trade_type_bs_p_idx)";
        assert_eq!(
            parse_index_columns(def),
            vec![
                "xwdm",
                "security_id",
                "scdm",
                "fund_code",
                "trade_type",
                "bs",
                "pay_type",
                "stock_kind",
                "bcrq",
                "etf_flag",
                "gddm",
                "gddmzm",
                "check_type",
                "mom_fund",
            ]
        );
    }

    #[test]
    fn parse_index_columns_ignores_tablespace_and_desc() {
        assert_eq!(
            parse_index_columns(
                "CREATE UNIQUE INDEX t_pkey ON public.t USING btree (id, user_id DESC) TABLESPACE pg_default"
            ),
            vec!["id", "user_id"]
        );
    }

    #[tokio::test]
    async fn partitioned_indexdef_junk_is_dropped_against_table_columns() {
        let cols = as_result(vec![
            col_row("xwdm", "varchar(6)", false, "PRI"),
            col_row("security_id", "varchar(20)", false, "PRI"),
            col_row("accrual", "numeric(16,2)", true, ""),
        ]);
        let def = "CREATE UNIQUE INDEX t_pkey ON public.t USING btree (xwdm, security_id) \
             LOCAL(PARTITION part_202401_xwdm_security_id, PARTITION part_202402_xwdm_security_id)";
        let mut conn = mock(cols, primary_index(def));
        let plan = build_table_plan(&mut conn, "public", "t", &[], &[], false, &[])
            .await
            .unwrap();
        assert_eq!(plan.key_columns, vec!["xwdm", "security_id"]);
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

    // ── --exclude-columns (issue #109) ──

    #[tokio::test]
    async fn exclude_columns_drops_them_from_the_compare_set() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let exclude = vec!["c_vc".to_string(), "c_dt".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &exclude)
            .await
            .unwrap();
        assert_eq!(
            plan.compare_columns,
            vec!["id", "c_int", "c_dec", "c_bool", "c_null"]
        );
        assert_eq!(plan.norm_specs.len(), plan.compare_columns.len());
        assert_eq!(
            plan.norm_specs
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "c_int", "c_dec", "c_bool", "c_null"]
        );
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("exclude-columns") && w.contains("c_dt") && w.contains("c_vc")),
            "warnings must name the excluded columns: {:?}",
            plan.warnings
        );
    }

    #[tokio::test]
    async fn exclude_columns_matches_case_insensitively() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let exclude = vec!["C_VC".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &exclude)
            .await
            .unwrap();
        assert!(!plan.compare_columns.contains(&"c_vc".to_string()));
        assert_eq!(plan.compare_columns.len(), 6);
        assert!(
            plan.warnings.iter().any(|w| w.contains("c_vc")),
            "the physical name is reported: {:?}",
            plan.warnings
        );
    }

    #[tokio::test]
    async fn unknown_exclude_column_is_rejected() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let exclude = vec!["nope".to_string()];
        let err = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &exclude)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--exclude-columns"), "{err}");
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn excluding_a_key_column_keeps_the_key_and_warns() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let exclude = vec!["id".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &exclude)
            .await
            .unwrap();
        assert_eq!(plan.key_columns, vec!["id"]);
        assert!(!plan.compare_columns.contains(&"id".to_string()));
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("row key") && w.contains("id")),
            "excluding a key column must be reported: {:?}",
            plan.warnings
        );
    }

    #[tokio::test]
    async fn excluding_every_column_leaves_nothing_to_compare() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let exclude: Vec<String> = ["id", "c_int", "c_dec", "c_dt", "c_vc", "c_bool", "c_null"]
            .iter()
            .map(|c| (*c).to_string())
            .collect();
        let err = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &exclude)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no columns left"), "{err}");
    }

    #[tokio::test]
    async fn exclude_column_also_listed_in_columns_is_rejected() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let explicit = vec!["id".to_string(), "c_int".to_string()];
        let exclude = vec!["C_INT".to_string()];
        let err = build_table_plan(
            &mut conn,
            "verify",
            "verify_t",
            &explicit,
            &[],
            false,
            &exclude,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("both --columns"), "{err}");
    }

    #[tokio::test]
    async fn exclude_columns_outside_an_explicit_list_is_a_no_op() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        // Overlap with --columns is rejected, so a legal --columns +
        // --exclude-columns pair can only name columns that were not going to
        // be compared anyway: the compare set stays as --columns left it.
        let explicit = vec!["id".to_string(), "c_int".to_string()];
        let exclude = vec!["c_dec".to_string()];
        let plan = build_table_plan(
            &mut conn,
            "verify",
            "verify_t",
            &explicit,
            &[],
            false,
            &exclude,
        )
        .await
        .unwrap();
        assert_eq!(plan.compare_columns, vec!["id", "c_int"]);
        assert_eq!(plan.norm_specs.len(), 2);
        assert!(
            plan.warnings
                .iter()
                .any(|w| w.contains("exclude-columns") && w.contains("c_dec")),
            "{:?}",
            plan.warnings
        );
    }

    // ── an excluded key keeps its declared type (review of #109) ──

    #[tokio::test]
    async fn excluded_non_integer_key_is_still_known_to_be_non_integer() {
        let mut cols = verify_columns();
        cols.rows.push(col_row("payload", "text", true, ""));
        cols.row_count += 1;
        let mut conn = mock(cols, primary_index("payload"));
        let exclude = vec!["payload".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &exclude)
            .await
            .unwrap();

        assert_eq!(plan.key_columns, vec!["payload"]);
        assert!(!plan.compare_columns.contains(&"payload".to_string()));
        assert!(
            plan.key_specs
                .iter()
                .any(|s| s.name == "payload" && s.data_type == "text"),
            "the key spec must outlive the exclusion: {:?}",
            plan.key_specs
        );
        assert!(
            !plan.column_type_may_be_integer("payload"),
            "a text key cannot supply an integer key domain"
        );
    }

    #[tokio::test]
    async fn excluded_string_key_still_flags_as_string() {
        let mut cols = verify_columns();
        cols.rows.push(col_row("skey", "varchar(64)", false, ""));
        cols.row_count += 1;
        let mut conn = mock(cols, primary_index("skey"));
        let exclude = vec!["skey".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &exclude)
            .await
            .unwrap();

        assert_eq!(
            plan.string_key_flags(),
            vec![true],
            "keyset paging needs the string flag even when the key is not compared"
        );
    }

    #[tokio::test]
    async fn excluded_integer_key_still_flags_as_numeric() {
        let mut conn = mock(verify_columns(), primary_index("id"));
        let exclude = vec!["id".to_string()];
        let plan = build_table_plan(&mut conn, "verify", "verify_t", &[], &[], false, &exclude)
            .await
            .unwrap();

        assert_eq!(
            plan.numeric_value_flags_for(&["id".to_string(), "c_int".to_string()]),
            vec![true, true],
            "the client-side merge must order an excluded integer key numerically"
        );
    }

    #[tokio::test]
    async fn excluded_key_hashes_like_the_compared_key() {
        let exclude = vec!["id".to_string()];
        let mut with_exclusion = mock(verify_columns(), primary_index("id"));
        let excluded = build_table_plan(
            &mut with_exclusion,
            "verify",
            "verify_t",
            &[],
            &[],
            false,
            &exclude,
        )
        .await
        .unwrap();
        let mut without_exclusion = mock(verify_columns(), primary_index("id"));
        let compared = build_table_plan(
            &mut without_exclusion,
            "verify",
            "verify_t",
            &[],
            &[],
            false,
            &[],
        )
        .await
        .unwrap();

        let dialect = MySqlDialect;
        assert_eq!(
            excluded.key_hash_exprs(&dialect).unwrap(),
            compared.key_hash_exprs(&dialect).unwrap(),
            "the key hash must not degrade to a raw identifier when the key is excluded"
        );
    }
}
