use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::backend::error::DbError;
use crate::backend::{DbConn, Dialect, QueryResult};

use super::error;
use super::types;
use super::GaussdbDialect;

pub(crate) struct GaussdbConn {
    pub(crate) client: Arc<gaussdb::Client>,
    pub(crate) dialect: GaussdbDialect,
}

fn row_to_values(row: &gaussdb::Row, col_count: usize) -> Vec<Value> {
    (0..col_count)
        .map(|i| types::format_value_at(row, i))
        .collect()
}

/// A scalar bound to a `$N` placeholder. The driver cannot coerce a Rust
/// `String` to an `int4`/`numeric`/`bool` wire type, so JSON scalars keep
/// their Rust type instead of being stringified.
#[derive(Debug)]
pub(crate) enum ParamValue {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
}

impl gaussdb::types::ToSql for ParamValue {
    fn to_sql(
        &self,
        ty: &gaussdb::types::Type,
        out: &mut gaussdb::types::private::BytesMut,
    ) -> Result<gaussdb::types::IsNull, Box<dyn std::error::Error + Sync + Send>> {
        use gaussdb::types::Type;
        match (self, ty) {
            (ParamValue::Null, _) => Option::<String>::None.to_sql(ty, out),
            (ParamValue::Int(v), &Type::INT2) => to_sql_int_as_i16(*v)?.to_sql(ty, out),
            (ParamValue::Int(v), &Type::INT4) => to_sql_int_as_i32(*v)?.to_sql(ty, out),
            (ParamValue::Int(v), &Type::INT8) => v.to_sql(ty, out),
            (ParamValue::Float(v), &Type::FLOAT4) => (*v as f32).to_sql(ty, out),
            (ParamValue::Float(v), &Type::FLOAT8) => v.to_sql(ty, out),
            (ParamValue::Bool(v), &Type::BOOL) => v.to_sql(ty, out),
            // NUMERIC has no native i64/f64 binding; go through rust_decimal
            // (driver feature `with-rust_decimal-1`).
            (ParamValue::Int(v), &Type::NUMERIC) => rust_decimal::Decimal::from(*v).to_sql(ty, out),
            (ParamValue::Float(v), &Type::NUMERIC) => {
                let d = rust_decimal::Decimal::try_from(*v)
                    .map_err(|e| format!("numeric out of range: {e}"))?;
                d.to_sql(ty, out)
            }
            (ParamValue::Text(v), &Type::NUMERIC) => {
                let d: rust_decimal::Decimal = v
                    .parse()
                    .map_err(|e| format!("invalid numeric '{v}': {e}"))?;
                d.to_sql(ty, out)
            }
            // Date/time/uuid targets need typed values; parse the text form
            // into the driver-native representation (chrono/uuid features).
            (ParamValue::Text(v), &Type::DATE) => {
                let d: chrono::NaiveDate = chrono::NaiveDate::parse_from_str(v.trim(), "%Y-%m-%d")
                    .map_err(|e| format!("invalid date '{v}': {e}"))?;
                d.to_sql(ty, out)
            }
            (ParamValue::Text(v), &Type::TIMESTAMP) => {
                let ts = parse_naive_datetime(v)?;
                ts.to_sql(ty, out)
            }
            (ParamValue::Text(v), &Type::TIMESTAMPTZ) => {
                let ts = parse_naive_datetime(v)?;
                chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(ts, chrono::Utc)
                    .to_sql(ty, out)
            }
            (ParamValue::Text(v), &Type::TIME) => {
                let t: chrono::NaiveTime = chrono::NaiveTime::parse_from_str(v.trim(), "%H:%M:%S")
                    .or_else(|_| chrono::NaiveTime::parse_from_str(v.trim(), "%H:%M"))
                    .map_err(|e| format!("invalid time '{v}': {e}"))?;
                t.to_sql(ty, out)
            }
            (ParamValue::Text(v), &Type::UUID) => {
                let u: uuid::Uuid = v.parse().map_err(|e| format!("invalid uuid '{v}': {e}"))?;
                u.to_sql(ty, out)
            }
            (ParamValue::Text(v), _) => v.to_sql(ty, out),
            (ParamValue::Int(v), _) => v.to_string().to_sql(ty, out),
            (ParamValue::Float(v), _) => v.to_string().to_sql(ty, out),
            (ParamValue::Bool(v), _) => v.to_string().to_sql(ty, out),
        }
    }

    fn accepts(ty: &gaussdb::types::Type) -> bool {
        use gaussdb::types::Type;
        matches!(
            *ty,
            Type::BOOL
                | Type::INT2
                | Type::INT4
                | Type::INT8
                | Type::FLOAT4
                | Type::FLOAT8
                | Type::NUMERIC
                | Type::TEXT
                | Type::VARCHAR
                | Type::BPCHAR
                | Type::NAME
                | Type::UNKNOWN
                | Type::DATE
                | Type::TIME
                | Type::TIMESTAMP
                | Type::TIMESTAMPTZ
                | Type::JSON
                | Type::JSONB
                | Type::UUID
        )
    }

    gaussdb::types::to_sql_checked!();
}

/// Convert serde_json params into driver-bindable scalars. Strings stay text
/// (introspection SQL passes schema/table names), numbers become i64/f64 so
/// the driver picks the matching wire type, and arrays/objects stringify as
/// before (JSON columns accept their text form).
fn bind_params(params: &[Value]) -> Result<Vec<ParamValue>, String> {
    params
        .iter()
        .map(|v| {
            Ok(match v {
                Value::Null => ParamValue::Null,
                Value::Bool(b) => ParamValue::Bool(*b),
                Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        ParamValue::Int(i)
                    } else if let Some(u) = n.as_u64() {
                        // A u64 above i64::MAX cannot be represented; fail the
                        // statement instead of silently clamping to i64::MAX.
                        ParamValue::Int(i64::try_from(u).map_err(|_| {
                            format!("integer {u} out of range for 64-bit signed binding")
                        })?)
                    } else {
                        ParamValue::Float(n.as_f64().unwrap_or_default())
                    }
                }
                Value::String(s) => ParamValue::Text(s.clone()),
                other => ParamValue::Text(other.to_string()),
            })
        })
        .collect()
}

/// Narrow an i64 to i16 for INT2 binding, rejecting silent truncation.
fn to_sql_int_as_i16(v: i64) -> Result<i16, String> {
    i16::try_from(v).map_err(|_| format!("integer {v} out of range for smallint (i16)"))
}

/// Narrow an i64 to i32 for INT4 binding, rejecting silent truncation.
fn to_sql_int_as_i32(v: i64) -> Result<i32, String> {
    i32::try_from(v).map_err(|_| format!("integer {v} out of range for integer (i32)"))
}

/// Parse a timestamp text form into a NaiveDateTime: full RFC3339, the
/// `YYYY-MM-DD HH:MM:SS` dump form, or a bare date (midnight).
fn parse_naive_datetime(
    v: &str,
) -> Result<chrono::NaiveDateTime, Box<dyn std::error::Error + Sync + Send>> {
    let s = v.trim();
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.naive_utc())
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).expect("midnight is valid"))
        })
        .map_err(|e| format!("invalid timestamp '{v}': {e}").into())
}

#[async_trait]
impl DbConn for GaussdbConn {
    async fn query(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let rows = self
            .client
            .query(sql, &[])
            .await
            .map_err(|e| error::wrap_gaussdb_error("query", e))?;

        if rows.is_empty() {
            return Ok(QueryResult::empty());
        }

        let columns: Vec<String> = rows[0]
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();
        let col_count = columns.len();

        let result_rows: Vec<Vec<Value>> = rows
            .iter()
            .map(|row| row_to_values(row, col_count))
            .collect();
        let row_count = result_rows.len();

        Ok(QueryResult {
            columns,
            rows: result_rows,
            row_count,
            rows_affected: None,
        })
    }

    async fn exec(&mut self, sql: &str, params: &[Value]) -> Result<QueryResult, DbError> {
        let bound = bind_params(params).map_err(|e| DbError::query(format!("exec: bind: {e}")))?;
        let param_refs: Vec<&(dyn gaussdb::types::ToSql + Sync)> = bound
            .iter()
            .map(|p| p as &(dyn gaussdb::types::ToSql + Sync))
            .collect();
        let rows = self
            .client
            .query(sql, &param_refs)
            .await
            .map_err(|e| error::wrap_gaussdb_error("exec", e))?;

        if rows.is_empty() {
            return Ok(QueryResult::empty());
        }

        let columns: Vec<String> = rows[0]
            .columns()
            .iter()
            .map(|c| c.name().to_string())
            .collect();
        let col_count = columns.len();

        let result_rows: Vec<Vec<Value>> = rows
            .iter()
            .map(|row| row_to_values(row, col_count))
            .collect();
        let row_count = result_rows.len();

        Ok(QueryResult {
            columns,
            rows: result_rows,
            row_count,
            rows_affected: None,
        })
    }

    async fn query_drop(&mut self, sql: &str) -> Result<(), DbError> {
        self.client
            .simple_query(sql)
            .await
            .map_err(|e| error::wrap_gaussdb_error("query_drop", e))?;
        Ok(())
    }

    async fn execute_write(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let affected = self
            .client
            .execute(sql, &[])
            .await
            .map_err(|e| error::wrap_gaussdb_error("execute_write", e))?;
        Ok(QueryResult::affected(affected))
    }

    fn dialect(&self) -> &dyn Dialect {
        &self.dialect
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_params_preserve_scalar_types_for_binding() {
        // The driver rejects a String bound against int4/numeric/bool targets,
        // so exec must hand it scalars of the right Rust type: JSON numbers as
        // i64/f64, bools as bool, NULL as None, strings as text.
        let params: Vec<Value> = vec![
            serde_json::json!(7),
            serde_json::json!(-3),
            serde_json::json!(1.5),
            serde_json::json!(true),
            serde_json::json!(null),
            serde_json::json!("text"),
        ];
        let bound = bind_params(&params).expect("plain scalars must bind");
        assert!(matches!(bound[0], ParamValue::Int(7)));
        assert!(matches!(bound[1], ParamValue::Int(-3)));
        assert!(matches!(bound[2], ParamValue::Float(f) if f == 1.5));
        assert!(matches!(bound[3], ParamValue::Bool(true)));
        assert!(matches!(bound[4], ParamValue::Null));
        assert!(matches!(bound[5], ParamValue::Text(ref s) if s == "text"));
    }
    #[test]
    fn bind_params_reject_u64_above_i64_max() {
        let params = vec![serde_json::json!(18446744073709551615u64)];
        let err = bind_params(&params).expect_err("u64 above i64::MAX must error");
        assert!(err.contains("out of range"), "got: {err}");
    }

    #[test]
    fn to_sql_int_narrowing_rejects_overflow() {
        // i64 value that does not fit i16 bound to an INT2 column must error,
        // not silently truncate to the low 16 bits.
        let err = to_sql_int_as_i16(70_000).expect_err("70000 does not fit i16");
        assert!(err.contains("out of range"), "got: {err}");
        assert!(to_sql_int_as_i16(32_767).is_ok());
        assert!(to_sql_int_as_i16(-32_768).is_ok());

        let err = to_sql_int_as_i32(3_000_000_000).expect_err("3e9 does not fit i32");
        assert!(err.contains("out of range"), "got: {err}");
        assert!(to_sql_int_as_i32(2_147_483_647).is_ok());
    }
}
