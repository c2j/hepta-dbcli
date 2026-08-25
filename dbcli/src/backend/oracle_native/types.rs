use oracle::sql_type::OracleType;
use serde_json::Value;

pub(crate) fn format_oracle_row(row: &oracle::Row, col_count: usize) -> Vec<Value> {
    let mut result = Vec::with_capacity(col_count);
    for idx in 0..col_count {
        result.push(value_at(row, idx));
    }
    result
}

fn is_text_oracle_type(ty: &OracleType) -> bool {
    matches!(
        ty,
        OracleType::Varchar2(_)
            | OracleType::NVarchar2(_)
            | OracleType::Char(_)
            | OracleType::NChar(_)
            | OracleType::Long
            | OracleType::CLOB
            | OracleType::NCLOB
            | OracleType::Json
            | OracleType::Xml
            | OracleType::Rowid
    )
}

fn value_at(row: &oracle::Row, idx: usize) -> Value {
    // rust-oracle parses CHAR/VARCHAR2 via str::parse into i64/f64. Digit-only
    // keys must stay strings or keyset last-keys lose leading zeros / spaces.
    if let Some(info) = row.column_info().get(idx) {
        if is_text_oracle_type(info.oracle_type()) {
            return match row.get::<_, Option<String>>(idx) {
                Ok(Some(v)) => Value::String(v),
                _ => Value::Null,
            };
        }
    }
    if let Ok(v) = row.get::<_, i64>(idx) {
        return Value::Number(serde_json::Number::from(v));
    }
    if let Ok(v) = row.get::<_, f64>(idx) {
        if let Some(n) = serde_json::Number::from_f64(v) {
            return Value::Number(n);
        }
    }
    if let Ok(v) = row.get::<_, String>(idx) {
        return Value::String(v);
    }
    Value::Null
}
