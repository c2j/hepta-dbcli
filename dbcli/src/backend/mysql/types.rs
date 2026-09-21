use mysql_async::consts::ColumnType;
use mysql_async::prelude::FromValue;
use mysql_async::Row;
use serde_json::{json, Value};

pub(crate) fn format_row_value(row: &Row, idx: usize) -> Value {
    let columns = row.columns_ref();
    if idx >= columns.len() {
        return Value::Null;
    }
    let col_type = columns[idx].column_type();
    format_value_by_type(row, idx, col_type)
}

fn format_value_by_type(row: &Row, idx: usize, col_type: ColumnType) -> Value {
    match col_type {
        ColumnType::MYSQL_TYPE_NULL => Value::Null,

        ColumnType::MYSQL_TYPE_TINY => get_val::<i8>(row, idx).map_or(Value::Null, |v| json!(v)),

        ColumnType::MYSQL_TYPE_SHORT | ColumnType::MYSQL_TYPE_YEAR => {
            get_val::<i16>(row, idx).map_or(Value::Null, |v| json!(v))
        }

        ColumnType::MYSQL_TYPE_INT24 | ColumnType::MYSQL_TYPE_LONG => {
            get_val::<i32>(row, idx).map_or(Value::Null, |v| json!(v))
        }

        ColumnType::MYSQL_TYPE_LONGLONG => {
            get_val::<i64>(row, idx).map_or(Value::Null, |v| json!(v))
        }

        ColumnType::MYSQL_TYPE_FLOAT => get_val::<f32>(row, idx).map_or(Value::Null, |v| json!(v)),

        ColumnType::MYSQL_TYPE_DOUBLE => get_val::<f64>(row, idx).map_or(Value::Null, |v| json!(v)),

        ColumnType::MYSQL_TYPE_DECIMAL | ColumnType::MYSQL_TYPE_NEWDECIMAL => {
            get_str(row, idx).map_or(Value::Null, Value::String)
        }

        ColumnType::MYSQL_TYPE_BIT => get_val::<bool>(row, idx).map_or(Value::Null, |v| json!(v)),

        ColumnType::MYSQL_TYPE_DATE
        | ColumnType::MYSQL_TYPE_TIME
        | ColumnType::MYSQL_TYPE_DATETIME
        | ColumnType::MYSQL_TYPE_TIMESTAMP
        | ColumnType::MYSQL_TYPE_NEWDATE => get_str(row, idx).map_or(Value::Null, Value::String),

        ColumnType::MYSQL_TYPE_JSON => match get_str(row, idx) {
            Some(s) => match serde_json::from_str(&s) {
                Ok(v) => v,
                Err(_) => Value::String(s),
            },
            None => Value::Null,
        },

        ColumnType::MYSQL_TYPE_ENUM
        | ColumnType::MYSQL_TYPE_SET
        | ColumnType::MYSQL_TYPE_GEOMETRY => get_str(row, idx).map_or(Value::Null, Value::String),

        // All byte-carrying types share one policy (issue #101): decode as
        // UTF-8 text when the bytes are valid UTF-8, escape as hex only when
        // they are not. MySQL 8 marks information_schema string columns
        // (DATA_TYPE, COLUMN_TYPE, ...) with BINARY_FLAG, and VARBINARY comes
        // back as MYSQL_TYPE_VAR_STRING — both used to render as 0x.. or even
        // NULL, while their content is usually plain text.
        ColumnType::MYSQL_TYPE_VARCHAR
        | ColumnType::MYSQL_TYPE_VAR_STRING
        | ColumnType::MYSQL_TYPE_STRING
        | ColumnType::MYSQL_TYPE_TINY_BLOB
        | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
        | ColumnType::MYSQL_TYPE_LONG_BLOB
        | ColumnType::MYSQL_TYPE_BLOB => match get_bytes(row, idx) {
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(s) => Value::String(s),
                Err(e) => Value::String(format!("0x{}", hex_encode(e.as_bytes()))),
            },
            None => Value::Null,
        },

        _ => get_str(row, idx).map_or(Value::Null, Value::String),
    }
}

fn get_val<T: FromValue>(row: &Row, idx: usize) -> Option<T> {
    row.get_opt::<T, usize>(idx).and_then(|r| r.ok())
}

fn get_str(row: &Row, idx: usize) -> Option<String> {
    row.get_opt::<String, usize>(idx).and_then(|r| r.ok())
}

fn get_bytes(row: &Row, idx: usize) -> Option<Vec<u8>> {
    row.get_opt::<Vec<u8>, usize>(idx).and_then(|r| r.ok())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        result.push_str(&format!("{:02x}", b));
    }
    result
}
