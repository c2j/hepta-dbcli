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

fn read_jsonl(path: &Path) -> Result<GeneratedTable, String> {
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

fn read_json(path: &Path) -> Result<GeneratedTable, String> {
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
fn read_csv(path: &Path) -> Result<GeneratedTable, String> {
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
}
