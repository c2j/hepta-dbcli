//! Reading of generated table dumps (JSONL / JSON / CSV) shared by `synth`
//! and the `load` command.

use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

// ─── Types ──────────────────────────────────────────────────────────────

/// Generated rows per table, keyed by table name.
pub(crate) type GeneratedTables = HashMap<String, Vec<Vec<Value>>>;

/// Column names of the generated rows, per table.
pub(crate) type GeneratedColumns = HashMap<String, Vec<String>>;

/// Columns plus rows of one generated table.
pub(crate) type GeneratedTable = (Vec<String>, Vec<Vec<Value>>);

// ─── Generated data input ───────────────────────────────────────────────

/// Read a table written by `synth generate` from `dir`. Tries `.jsonl`,
/// `.json` and `.csv` (in that order); returns `None` when the table has no
/// file there.
pub(crate) fn read_generated_table(
    dir: &Path,
    table: &str,
) -> Result<Option<GeneratedTable>, String> {
    let jsonl = dir.join(format!("{}.jsonl", table));
    if jsonl.is_file() {
        return read_jsonl(&jsonl).map(Some);
    }
    let json = dir.join(format!("{}.json", table));
    if json.is_file() {
        return read_json(&json).map(Some);
    }
    let csv = dir.join(format!("{}.csv", table));
    if csv.is_file() {
        return read_csv(&csv).map(Some);
    }
    Ok(None)
}

pub(crate) fn read_jsonl(path: &Path) -> Result<GeneratedTable, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let mut columns: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let object: serde_json::Map<String, Value> = serde_json::from_str(line)
            .map_err(|e| format!("parse {}:{}: {}", path.display(), line_no + 1, e))?;
        if columns.is_empty() {
            columns = object.keys().cloned().collect();
        }
        rows.push(
            columns
                .iter()
                .map(|name| object.get(name).cloned().unwrap_or(Value::Null))
                .collect(),
        );
    }
    Ok((columns, rows))
}

pub(crate) fn read_json(path: &Path) -> Result<GeneratedTable, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let objects: Vec<serde_json::Map<String, Value>> =
        serde_json::from_str(&content).map_err(|e| format!("parse {}: {}", path.display(), e))?;
    let mut columns: Vec<String> = Vec::new();
    if let Some(first) = objects.first() {
        columns = first.keys().cloned().collect();
    }
    let rows = objects
        .iter()
        .map(|object| {
            columns
                .iter()
                .map(|name| object.get(name).cloned().unwrap_or(Value::Null))
                .collect()
        })
        .collect();
    Ok((columns, rows))
}

/// CSV fields come back as strings (the exporter does not record types);
/// numeric parsing happens at scoring time. An empty unquoted field is NULL
/// while `""` is the empty string, which is why this is a small RFC 4180
/// reader over the whole file rather than `csv::Reader`: the crate cannot tell
/// those two apart, and quoted fields may contain the line break the record
/// splitter would otherwise treat as a row.
pub(crate) fn read_csv(path: &Path) -> Result<GeneratedTable, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {}", path.display(), e))?;
    let records = parse_csv_records(&content);
    let mut records = records.into_iter();
    let columns: Vec<String> = match records.next() {
        Some(header) => header
            .into_iter()
            .map(|field| field.unwrap_or_default())
            .collect(),
        None => return Ok((Vec::new(), Vec::new())),
    };
    let rows = records
        .map(|record| {
            record
                .into_iter()
                .map(|field| match field {
                    Some(text) => Value::String(text),
                    None => Value::Null,
                })
                .collect()
        })
        .collect();
    Ok((columns, rows))
}

/// Split CSV text into records of fields. `None` is an empty unquoted field
/// (NULL), `Some("")` a quoted empty string, and a trailing newline does not
/// produce an empty record.
fn parse_csv_records(content: &str) -> Vec<Vec<Option<String>>> {
    let chars: Vec<char> = content.chars().collect();
    let mut records = Vec::new();
    let mut record = Vec::new();
    let mut field = String::new();
    let mut field_is_quoted = false;
    let mut in_quotes = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if in_quotes {
            if c == '"' {
                if chars.get(i + 1) == Some(&'"') {
                    field.push('"');
                    i += 2;
                    continue;
                }
                in_quotes = false;
                i += 1;
                continue;
            }
            field.push(c);
            i += 1;
            continue;
        }
        match c {
            '"' if field.is_empty() => {
                in_quotes = true;
                field_is_quoted = true;
                i += 1;
            }
            ',' => {
                record.push(field_value(&field, field_is_quoted));
                field.clear();
                field_is_quoted = false;
                i += 1;
            }
            '\n' | '\r' => {
                // Consume CRLF as one break.
                if c == '\r' && chars.get(i + 1) == Some(&'\n') {
                    i += 1;
                }
                record.push(field_value(&field, field_is_quoted));
                records.push(std::mem::take(&mut record));
                field.clear();
                field_is_quoted = false;
                i += 1;
            }
            _ => {
                field.push(c);
                i += 1;
            }
        }
    }
    if !field.is_empty() || field_is_quoted || !record.is_empty() {
        record.push(field_value(&field, field_is_quoted));
        records.push(record);
    }
    records
}

fn field_value(field: &str, quoted: bool) -> Option<String> {
    if field.is_empty() && !quoted {
        None
    } else {
        Some(field.to_string())
    }
}

// ─── Target-type coercion (issue #98) ───────────────────────────────────

/// Convert a reader value into the JSON shape each backend binds natively
/// for the target column type. `data_type` is the raw `data_type` string as
/// returned by `dialect.table_columns()`; family detection is case-insensitive
/// and ignores parenthesized suffixes (`int(11)`, `decimal(10,2)`, ...).
///
/// CSV fields arrive as `Value::String`, so this is where they become integers
/// or floats; decimals deliberately stay strings to preserve precision.
pub(crate) fn coerce_value_for_column(v: &Value, data_type: &str) -> Result<Value, String> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    let lowered = data_type.trim().to_ascii_lowercase();
    let name = lowered.split(['(', ' ']).next().unwrap_or_default();
    match name {
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint" | "int2" | "int4"
        | "int8" => coerce_integer(v, data_type),
        "decimal" | "numeric" | "number" | "dec" => coerce_decimal(v, data_type),
        "float" | "double" | "real" => coerce_float(v, data_type),
        "bool" | "boolean" => coerce_bool(v, data_type),
        "datetime" | "timestamp" | "date" | "time" | "timestamptz" => coerce_datetime(v, data_type),
        // Text/binary/json/enum/geometric/unknown: every backend coerces or
        // binds these natively, so the value passes through untouched.
        _ => Ok(v.clone()),
    }
}

/// Coerce one row column-by-column, naming the column, its type and the
/// underlying reason on failure. Row-level context (row number, table) is
/// added by the caller.
pub(crate) fn coerce_row(
    row: &[Value],
    types: &[String],
    columns: &[String],
) -> Result<Vec<Value>, String> {
    row.iter()
        .zip(types.iter())
        .zip(columns.iter())
        .map(|((v, ty), col)| {
            coerce_value_for_column(v, ty)
                .map_err(|reason| format!("column '{col}' (type {ty}): {reason}"))
        })
        .collect()
}

fn coerce_integer(v: &Value, ty: &str) -> Result<Value, String> {
    match v {
        // Already-integer numbers (including u64 > i64::MAX) pass through.
        Value::Number(n) if n.is_i64() || n.is_u64() => Ok(v.clone()),
        Value::Number(_) => Err(format!("{v} is not an integer")),
        // tinyint(1) booleans are common in dumps.
        Value::Bool(b) => Ok(Value::Number(i64::from(*b).into())),
        // Strict: no surrounding whitespace, empty string is an error.
        Value::String(s) => s
            .parse::<i64>()
            .map(|n| Value::Number(n.into()))
            .map_err(|_| format!("'{s}' is not a valid integer for {ty}")),
        _ => Err(format!("unsupported value {v} for integer type {ty}")),
    }
}

fn coerce_decimal(v: &Value, ty: &str) -> Result<Value, String> {
    match v {
        // Keep the string form: precision must survive the round-trip.
        Value::String(_) => Ok(v.clone()),
        // serde_json renders the number's original representation.
        Value::Number(n) => Ok(Value::String(n.to_string())),
        _ => Err(format!("unsupported value {v} for decimal type {ty}")),
    }
}

fn coerce_float(v: &Value, ty: &str) -> Result<Value, String> {
    match v {
        Value::Number(_) => Ok(v.clone()),
        // Strict: no surrounding whitespace.
        Value::String(s) => serde_json::Number::from_f64(
            s.parse::<f64>()
                .map_err(|_| format!("'{s}' is not a valid float for {ty}"))?,
        )
        // JSON cannot represent NaN or infinity ("inf" parses to f64).
        .map(Value::Number)
        .ok_or_else(|| format!("'{s}' is not a finite float for {ty}")),
        _ => Err(format!("unsupported value {v} for float type {ty}")),
    }
}

fn coerce_bool(v: &Value, ty: &str) -> Result<Value, String> {
    match v {
        Value::Bool(_) => Ok(v.clone()),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "1" => Ok(Value::Bool(true)),
            "false" | "0" => Ok(Value::Bool(false)),
            _ => Err(format!("'{s}' is not a valid boolean for {ty}")),
        },
        Value::Number(n) => match n.as_i64() {
            Some(1) => Ok(Value::Bool(true)),
            Some(0) => Ok(Value::Bool(false)),
            _ => Err(format!("only 0/1 coerce to boolean, got {v} for {ty}")),
        },
        _ => Err(format!("unsupported value {v} for boolean type {ty}")),
    }
}

fn coerce_datetime(v: &Value, ty: &str) -> Result<Value, String> {
    match v {
        // Keep verbatim; backends parse ISO strings themselves. Format
        // normalization is out of scope for the MVP.
        Value::String(_) => Ok(v.clone()),
        _ => Err(format!(
            "epoch/numeric values are not accepted for {ty}; use an ISO string"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_reader_reads_jsonl_null_and_empty_distinctly() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("t.jsonl"),
            "{\"a\":null,\"b\":\"\"}\n{\"a\":\"x\"}\n",
        )
        .unwrap();

        let (columns, rows) = read_generated_table(dir.path(), "t").unwrap().unwrap();

        // Column order follows the first line's key order.
        assert_eq!(
            columns,
            vec!["a".to_string(), "b".to_string()],
            "columns must follow the first line"
        );
        assert_eq!(rows.len(), 2);
        // JSON null and the empty string are distinct values.
        assert_eq!(rows[0][0], Value::Null);
        assert_eq!(rows[0][1], Value::String(String::new()));
        // A later line missing a first-line key maps to NULL, not "".
        assert_eq!(rows[1][0], Value::String("x".to_string()));
        assert_eq!(rows[1][1], Value::Null);
    }

    #[test]
    fn parse_csv_records_handles_quotes_nulls_and_embedded_newlines() {
        let records = parse_csv_records("1,\"a,b\",\"\",2\nx,\"say \"\"hi\"\"\",,\n");
        assert_eq!(
            records,
            vec![
                vec![
                    Some("1".to_string()),
                    Some("a,b".to_string()),
                    Some(String::new()),
                    Some("2".to_string())
                ],
                vec![
                    Some("x".to_string()),
                    Some(r#"say "hi""#.to_string()),
                    None,
                    None
                ],
            ]
        );

        // A newline inside a quoted field is part of the value, not a record
        // break (the exporter quotes such fields).
        let records = parse_csv_records("a,b\n\"line1\nline2\",2\n");
        assert_eq!(records.len(), 2);
        assert_eq!(records[1][0], Some("line1\nline2".to_string()));
        assert_eq!(records[1][1], Some("2".to_string()));

        // CRLF files and a missing trailing newline both parse to one record.
        assert_eq!(parse_csv_records("a,b\r\n1,2").len(), 2);
    }

    #[test]
    fn read_generated_table_prefers_jsonl_then_csv() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("a.jsonl"),
            "{\"x\":1,\"y\":\"p\"}\n{\"x\":2,\"y\":\"q\"}\n",
        )
        .unwrap();
        let (columns, rows) = read_generated_table(dir.path(), "a").unwrap().unwrap();
        assert!(columns.contains(&"x".to_string()));
        assert_eq!(rows.len(), 2);

        std::fs::write(dir.path().join("b.csv"), "x,y\n,\"\"\n3,r\n").unwrap();
        let (columns, rows) = read_generated_table(dir.path(), "b").unwrap().unwrap();
        assert_eq!(columns, vec!["x".to_string(), "y".to_string()]);
        assert!(rows[0][0].is_null(), "empty unquoted field is NULL");
        assert_eq!(rows[0][1], Value::String(String::new()));
        assert_eq!(rows[1][0], Value::String("3".to_string()));

        assert!(read_generated_table(dir.path(), "missing")
            .unwrap()
            .is_none());
    }

    // ─── Target-type coercion (issue #98) ───────────────────────────────

    use serde_json::json;

    #[test]
    fn should_coerce_csv_string_to_i64_for_integer_types() {
        for ty in [
            "bigint",
            "int",
            "int unsigned",
            "int(11)",
            "tinyint(1)",
            "smallint",
            "mediumint",
            "int4",
            "int8",
        ] {
            assert_eq!(
                coerce_value_for_column(&json!("42"), ty).unwrap(),
                json!(42),
                "type {ty}"
            );
            assert_eq!(
                coerce_value_for_column(&json!("-7"), ty).unwrap(),
                json!(-7),
                "type {ty}"
            );
            assert_eq!(
                coerce_value_for_column(&json!("+7"), ty).unwrap(),
                json!(7),
                "type {ty}"
            );
        }
        // Already-integer numbers pass through untouched, including values
        // above i64::MAX that only fit u64.
        assert_eq!(
            coerce_value_for_column(&json!(5), "bigint").unwrap(),
            json!(5)
        );
        let big = 18_446_744_073_709_551_615u64;
        assert_eq!(
            coerce_value_for_column(&Value::Number(big.into()), "bigint unsigned").unwrap(),
            Value::Number(big.into())
        );
        // Overflow is an error, not a silent wrap.
        assert!(coerce_value_for_column(&json!("9223372036854775808"), "bigint").is_err());
    }

    #[test]
    fn should_reject_non_integer_number_for_integer_column() {
        assert!(coerce_value_for_column(&json!(3.5), "int").is_err());
        assert!(coerce_value_for_column(&json!(0.5), "bigint").is_err());
    }

    #[test]
    fn should_reject_empty_string_for_integer_column_as_error_not_null() {
        let empty = coerce_value_for_column(&Value::String(String::new()), "int");
        assert!(empty.is_err(), "empty string must be an error");
        assert_ne!(empty.unwrap_err(), "", "the error must carry a reason");
        // Strictness: surrounding whitespace is not trimmed into an integer.
        assert!(coerce_value_for_column(&json!(" 42"), "int").is_err());
    }

    #[test]
    fn should_accept_bool_as_0_1_for_tinyint() {
        assert_eq!(
            coerce_value_for_column(&json!(true), "tinyint(1)").unwrap(),
            json!(1)
        );
        assert_eq!(
            coerce_value_for_column(&json!(false), "tinyint(1)").unwrap(),
            json!(0)
        );
        assert_eq!(
            coerce_value_for_column(&json!(true), "bigint").unwrap(),
            json!(1)
        );
    }

    #[test]
    fn should_keep_decimal_as_string_with_precision_intact() {
        let s = "12345678901234567890.123456789";
        assert_eq!(
            coerce_value_for_column(&json!(s), "decimal(10,2)").unwrap(),
            json!(s)
        );
        assert_eq!(
            coerce_value_for_column(&json!(s), "numeric(20,6)").unwrap(),
            json!(s)
        );
        // Numbers become their canonical string form (no f64 artifacts).
        assert_eq!(
            coerce_value_for_column(&json!(12345678901234567890u64), "decimal(20,0)").unwrap(),
            json!("12345678901234567890")
        );
    }

    #[test]
    fn should_coerce_true_false_and_0_1_for_boolean() {
        for ty in ["boolean", "BOOLEAN", "bool"] {
            assert_eq!(
                coerce_value_for_column(&json!("true"), ty).unwrap(),
                json!(true),
                "type {ty}"
            );
            assert_eq!(
                coerce_value_for_column(&json!("FALSE"), ty).unwrap(),
                json!(false),
                "type {ty}"
            );
            assert_eq!(
                coerce_value_for_column(&json!("1"), ty).unwrap(),
                json!(true),
                "type {ty}"
            );
            assert_eq!(
                coerce_value_for_column(&json!("0"), ty).unwrap(),
                json!(false),
                "type {ty}"
            );
            assert_eq!(
                coerce_value_for_column(&json!(1), ty).unwrap(),
                json!(true),
                "type {ty}"
            );
            assert_eq!(
                coerce_value_for_column(&json!(0), ty).unwrap(),
                json!(false),
                "type {ty}"
            );
            assert!(coerce_value_for_column(&json!("yes"), ty).is_err(), "{ty}");
            assert!(coerce_value_for_column(&json!(2), ty).is_err(), "{ty}");
        }
        assert_eq!(
            coerce_value_for_column(&json!(false), "bool").unwrap(),
            json!(false)
        );
    }

    #[test]
    fn should_keep_datetime_string_verbatim() {
        for &(ty, s) in &[
            ("datetime", "2024-01-02 03:04:05"),
            ("timestamp", "2024-01-02T03:04:05Z"),
            ("timestamp without time zone", "2024-01-02 03:04:05.123"),
            ("timestamp with time zone", "2024-01-02 03:04:05+08"),
            ("timestamptz", "2024-01-02 03:04:05+08"),
            ("date", "2024-01-02"),
            ("time", "03:04:05"),
        ] {
            assert_eq!(
                coerce_value_for_column(&Value::String(s.to_string()), ty).unwrap(),
                Value::String(s.to_string()),
                "type {ty}"
            );
            // Epoch numbers are rejected in the MVP.
            assert!(
                coerce_value_for_column(&json!(1_700_000_000i64), ty).is_err(),
                "type {ty}"
            );
        }
    }

    #[test]
    fn should_pass_through_null_regardless_of_type() {
        for ty in [
            "bigint",
            "decimal(10,2)",
            "BOOLEAN",
            "timestamp",
            "varchar(64)",
            "weirdtype",
        ] {
            assert_eq!(
                coerce_value_for_column(&Value::Null, ty).unwrap(),
                Value::Null,
                "type {ty}"
            );
        }
    }

    #[test]
    fn should_handle_gaussdb_numeric_and_int8_names() {
        assert_eq!(
            coerce_value_for_column(&json!("42"), "int8").unwrap(),
            json!(42)
        );
        assert_eq!(
            coerce_value_for_column(&json!("7"), "int4").unwrap(),
            json!(7)
        );
        assert_eq!(
            coerce_value_for_column(&json!("-1"), "int2").unwrap(),
            json!(-1)
        );
        let s = "123456789.654321";
        assert_eq!(
            coerce_value_for_column(&json!(s), "numeric(20,6)").unwrap(),
            json!(s)
        );
        assert_eq!(
            coerce_value_for_column(&json!("true"), "bool").unwrap(),
            json!(true)
        );
        assert_eq!(
            coerce_value_for_column(&json!("2024-01-02 03:04:05"), "timestamp without time zone")
                .unwrap(),
            json!("2024-01-02 03:04:05")
        );
        assert_eq!(
            coerce_value_for_column(&json!("hi"), "character varying").unwrap(),
            json!("hi")
        );
    }

    #[test]
    fn should_be_case_insensitive_for_duckdb_types() {
        assert_eq!(
            coerce_value_for_column(&json!("42"), "BIGINT").unwrap(),
            json!(42)
        );
        assert_eq!(
            coerce_value_for_column(&json!("42"), "INTEGER").unwrap(),
            json!(42)
        );
        assert_eq!(
            coerce_value_for_column(&json!("2.5"), "DOUBLE").unwrap(),
            json!(2.5)
        );
        // Strictness: no surrounding whitespace for floats either.
        assert!(coerce_value_for_column(&json!(" 1.5"), "DOUBLE").is_err());
        let s = "12345678901234567890.123456789";
        assert_eq!(
            coerce_value_for_column(&json!(s), "Decimal(10,2)").unwrap(),
            json!(s)
        );
        assert_eq!(
            coerce_value_for_column(&json!("true"), "BOOLEAN").unwrap(),
            json!(true)
        );
        assert_eq!(
            coerce_value_for_column(&json!("2024-01-02"), "TIMESTAMP").unwrap(),
            json!("2024-01-02")
        );
        assert!(coerce_value_for_column(&json!("x"), "BIGINT").is_err());
    }

    #[test]
    fn should_pass_through_text_family_untouched() {
        for ty in [
            "varchar(64)",
            "char(1)",
            "text",
            "enum('a','b')",
            "set('x')",
            "json",
            "binary(16)",
            "blob",
            "bytea",
            "uuid",
            "geometry",
            "money",
            "",
        ] {
            assert_eq!(
                coerce_value_for_column(&json!("plain"), ty).unwrap(),
                json!("plain"),
                "type {ty}"
            );
            // Numbers stay numbers; every backend coerces number -> text.
            assert_eq!(
                coerce_value_for_column(&json!(12), ty).unwrap(),
                json!(12),
                "type {ty}"
            );
        }
    }

    #[test]
    fn should_error_message_names_column_type_and_reason() {
        let err =
            coerce_row(&[json!("abc")], &["int".to_string()], &["id".to_string()]).unwrap_err();
        assert!(err.contains("column 'id'"), "got: {err}");
        assert!(err.contains("(type int)"), "got: {err}");
        assert!(err.contains(": "), "reason missing: {err}");

        // Success path zips positionally and coerces each column.
        let row = coerce_row(
            &[json!("1"), json!("x"), Value::Null],
            &[
                "int".to_string(),
                "varchar(8)".to_string(),
                "decimal(5,2)".to_string(),
            ],
            &["a".to_string(), "b".to_string(), "c".to_string()],
        )
        .unwrap();
        assert_eq!(row, vec![json!(1), json!("x"), Value::Null]);
    }
}
