use serde_json::{json, Value};

pub(crate) fn format_oracle_row(row: &oracle_rs::Row, col_count: usize) -> Vec<Value> {
    let mut result = Vec::with_capacity(col_count);
    for idx in 0..col_count {
        result.push(value_at(row, idx));
    }
    result
}

fn value_at(row: &oracle_rs::Row, idx: usize) -> Value {
    if let Some(v) = row.get_i64(idx) {
        return json!(v);
    }
    if let Some(v) = row.get_f64(idx) {
        if v.fract() == 0.0 && v >= (i64::MIN as f64) && v <= (i64::MAX as f64) {
            return json!(v as i64);
        }
        return json!(v);
    }
    if let Some(s) = row.get_string(idx) {
        // Digit-only VARCHAR2 (e.g. '55958') is valid JSON; do not parse it as a
        // number — keyset pagination would then emit NLSSORT(55958) (ORA-01722).
        return Value::String(s.to_string());
    }
    Value::Null
}
