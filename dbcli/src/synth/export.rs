use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;

pub struct ExportPayload<'a> {
    pub tables: &'a HashMap<String, Vec<Vec<Value>>>,
    pub columns: &'a HashMap<String, Vec<String>>,
    pub dialect: &'a str,
}

pub enum ExportFormat {
    Csv,
    Jsonl,
    Json,
    Sql,
}

pub fn export(
    payload: &ExportPayload<'_>,
    format: &ExportFormat,
    output_dir: &std::path::Path,
) -> Result<(), String> {
    match format {
        ExportFormat::Csv => export_csv(payload, output_dir),
        ExportFormat::Jsonl => export_jsonl(payload, output_dir),
        ExportFormat::Json => export_json(payload, output_dir),
        ExportFormat::Sql => export_sql(payload, output_dir),
    }
}

fn columns_for(payload: &ExportPayload<'_>, table: &str, row_len: usize) -> Vec<String> {
    if let Some(names) = payload.columns.get(table) {
        if names.len() == row_len {
            return names.clone();
        }
    }
    (0..row_len).map(|i| format!("col_{}", i)).collect()
}

fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn export_csv(payload: &ExportPayload<'_>, output_dir: &std::path::Path) -> Result<(), String> {
    for (table_name, rows) in payload.tables {
        let path = output_dir.join(format!("{}.csv", table_name));
        let mut file =
            std::fs::File::create(&path).map_err(|e| format!("create CSV file: {}", e))?;

        if let Some(first_row) = rows.first() {
            let header: Vec<String> = columns_for(payload, table_name, first_row.len())
                .iter()
                .map(|c| csv_field(c))
                .collect();
            writeln!(file, "{}", header.join(","))
                .map_err(|e| format!("write CSV header: {}", e))?;
        }

        for row in rows {
            let values: Vec<String> = row
                .iter()
                .map(|v| match v {
                    Value::String(s) => csv_field(s),
                    Value::Null => "null".to_string(),
                    _ => csv_field(&v.to_string()),
                })
                .collect();
            writeln!(file, "{}", values.join(",")).map_err(|e| format!("write CSV row: {}", e))?;
        }
    }
    Ok(())
}

fn export_jsonl(payload: &ExportPayload<'_>, output_dir: &std::path::Path) -> Result<(), String> {
    for (table_name, rows) in payload.tables {
        let path = output_dir.join(format!("{}.jsonl", table_name));
        let mut file =
            std::fs::File::create(&path).map_err(|e| format!("create JSONL file: {}", e))?;

        for row in rows {
            let names = columns_for(payload, table_name, row.len());
            let obj: serde_json::Map<String, Value> = names
                .iter()
                .zip(row.iter())
                .map(|(name, v)| (name.clone(), v.clone()))
                .collect();
            let json =
                serde_json::to_string(&obj).map_err(|e| format!("serialize JSONL row: {}", e))?;
            writeln!(file, "{}", json).map_err(|e| format!("write JSONL row: {}", e))?;
        }
    }
    Ok(())
}

fn export_json(payload: &ExportPayload<'_>, output_dir: &std::path::Path) -> Result<(), String> {
    for (table_name, rows) in payload.tables {
        let path = output_dir.join(format!("{}.json", table_name));
        let documents: Vec<serde_json::Map<String, Value>> = rows
            .iter()
            .map(|row| {
                columns_for(payload, table_name, row.len())
                    .iter()
                    .zip(row.iter())
                    .map(|(name, v)| (name.clone(), v.clone()))
                    .collect()
            })
            .collect();
        let json = serde_json::to_string_pretty(&documents)
            .map_err(|e| format!("serialize JSON: {}", e))?;
        std::fs::write(&path, json).map_err(|e| format!("write JSON file: {}", e))?;
    }
    Ok(())
}

fn export_ident(dialect: &str, name: &str) -> String {
    if dialect == "mysql" {
        format!("`{}`", name.replace('`', "``"))
    } else if dialect.starts_with("oracle") {
        // Oracle 将未加引号的标识符折叠为大写存储；导出须同步折叠才能命中
        format!("\"{}\"", name.to_ascii_uppercase().replace('"', "\"\""))
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

fn export_sql(payload: &ExportPayload<'_>, output_dir: &std::path::Path) -> Result<(), String> {
    for (table_name, rows) in payload.tables {
        let path = output_dir.join(format!("{}.sql", table_name));
        let mut file =
            std::fs::File::create(&path).map_err(|e| format!("create SQL file: {}", e))?;

        let table_ident = export_ident(payload.dialect, table_name);
        let column_list: Option<String> = rows.first().map(|first_row| {
            columns_for(payload, table_name, first_row.len())
                .iter()
                .map(|c| export_ident(payload.dialect, c))
                .collect::<Vec<_>>()
                .join(", ")
        });

        for row in rows {
            let values: Vec<String> = row
                .iter()
                .map(|v| match v {
                    Value::String(s) => format!("'{}'", s.replace('\'', "''")),
                    Value::Null => "NULL".to_string(),
                    Value::Number(n) => n.to_string(),
                    Value::Bool(b) => b.to_string(),
                    _ => format!("'{}'", v),
                })
                .collect();
            let columns_sql = column_list
                .as_deref()
                .map(|c| format!(" ({})", c))
                .unwrap_or_default();
            writeln!(
                file,
                "INSERT INTO {}{} VALUES ({});",
                table_ident,
                columns_sql,
                values.join(", ")
            )
            .map_err(|e| format!("write SQL row: {}", e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_export_uses_real_column_names() {
        let mut tables = HashMap::new();
        tables.insert(
            "users".to_string(),
            vec![
                vec![Value::from(1), Value::from("hello")],
                vec![Value::from(2), Value::from("world")],
            ],
        );
        let mut columns = HashMap::new();
        columns.insert(
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );

        let payload = ExportPayload {
            tables: &tables,
            columns: &columns,
            dialect: "mysql",
        };
        let temp_dir = std::env::temp_dir().join("synth_test_csv");
        std::fs::create_dir_all(&temp_dir).unwrap();

        export(&payload, &ExportFormat::Csv, &temp_dir).unwrap();

        let content = std::fs::read_to_string(temp_dir.join("users.csv")).unwrap();
        assert!(content.starts_with("id,name\n"));
        assert!(content.contains("1,hello\n"));
        assert!(content.contains("2,world\n"));

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn csv_export_escapes_special_headers() {
        let mut tables = HashMap::new();
        tables.insert("t".to_string(), vec![vec![Value::from(1), Value::from(2)]]);
        let mut columns = HashMap::new();
        columns.insert("t".to_string(), vec!["a,b".to_string(), "c\"d".to_string()]);

        let payload = ExportPayload {
            tables: &tables,
            columns: &columns,
            dialect: "mysql",
        };
        let temp_dir = std::env::temp_dir().join("synth_test_csv_esc");
        std::fs::create_dir_all(&temp_dir).unwrap();

        export(&payload, &ExportFormat::Csv, &temp_dir).unwrap();

        let content = std::fs::read_to_string(temp_dir.join("t.csv")).unwrap();
        assert!(content.starts_with("\"a,b\",\"c\"\"d\"\n"));

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn csv_export_falls_back_to_col_n_without_column_info() {
        let mut tables = HashMap::new();
        tables.insert("t".to_string(), vec![vec![Value::from(1)]]);

        let payload = ExportPayload {
            tables: &tables,
            columns: &HashMap::new(),
            dialect: "mysql",
        };
        let temp_dir = std::env::temp_dir().join("synth_test_csv_fallback");
        std::fs::create_dir_all(&temp_dir).unwrap();

        export(&payload, &ExportFormat::Csv, &temp_dir).unwrap();

        let content = std::fs::read_to_string(temp_dir.join("t.csv")).unwrap();
        assert!(content.starts_with("col_0\n"));

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn jsonl_export_keys_by_column_name() {
        let mut tables = HashMap::new();
        tables.insert(
            "users".to_string(),
            vec![vec![Value::from(1), Value::from("hello")]],
        );
        let mut columns = HashMap::new();
        columns.insert(
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );

        let payload = ExportPayload {
            tables: &tables,
            columns: &columns,
            dialect: "mysql",
        };
        let temp_dir = std::env::temp_dir().join("synth_test_jsonl");
        std::fs::create_dir_all(&temp_dir).unwrap();

        export(&payload, &ExportFormat::Jsonl, &temp_dir).unwrap();

        let content = std::fs::read_to_string(temp_dir.join("users.jsonl")).unwrap();
        assert!(content.contains("\"id\":1"));
        assert!(content.contains("\"name\":\"hello\""));

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn json_export_is_array_of_named_objects() {
        let mut tables = HashMap::new();
        tables.insert("users".to_string(), vec![vec![Value::from(1)]]);

        let mut columns = HashMap::new();
        columns.insert("users".to_string(), vec!["id".to_string()]);

        let payload = ExportPayload {
            tables: &tables,
            columns: &columns,
            dialect: "mysql",
        };
        let temp_dir = std::env::temp_dir().join("synth_test_json");
        std::fs::create_dir_all(&temp_dir).unwrap();

        export(&payload, &ExportFormat::Json, &temp_dir).unwrap();

        let content = std::fs::read_to_string(temp_dir.join("users.json")).unwrap();
        assert!(content.contains("\"id\": 1"));

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn sql_export_includes_quoted_column_list() {
        let mut tables = HashMap::new();
        tables.insert(
            "users".to_string(),
            vec![vec![Value::from(1), Value::from("hello")]],
        );
        let mut columns = HashMap::new();
        columns.insert(
            "users".to_string(),
            vec!["id".to_string(), "name".to_string()],
        );

        let payload = ExportPayload {
            tables: &tables,
            columns: &columns,
            dialect: "mysql",
        };
        let temp_dir = std::env::temp_dir().join("synth_test_sql");
        std::fs::create_dir_all(&temp_dir).unwrap();

        export(&payload, &ExportFormat::Sql, &temp_dir).unwrap();

        let content = std::fs::read_to_string(temp_dir.join("users.sql")).unwrap();
        let line = content.lines().next().unwrap();
        assert_eq!(
            line,
            "INSERT INTO `users` (`id`, `name`) VALUES (1, 'hello');"
        );

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn sql_export_uses_double_quotes_for_non_mysql() {
        let mut tables = HashMap::new();
        tables.insert("t".to_string(), vec![vec![Value::from(1)]]);

        let mut columns = HashMap::new();
        columns.insert("t".to_string(), vec!["id".to_string()]);

        let payload = ExportPayload {
            tables: &tables,
            columns: &columns,
            dialect: "gaussdb",
        };
        let temp_dir = std::env::temp_dir().join("synth_test_sql_ansi");
        std::fs::create_dir_all(&temp_dir).unwrap();

        export(&payload, &ExportFormat::Sql, &temp_dir).unwrap();

        let content = std::fs::read_to_string(temp_dir.join("t.sql")).unwrap();
        assert!(content.contains("INSERT INTO \"t\" (\"id\") VALUES (1);"));

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn sql_export_folds_oracle_identifiers_to_uppercase() {
        let mut tables = HashMap::new();
        tables.insert("users".to_string(), vec![vec![Value::from(1)]]);

        let mut columns = HashMap::new();
        columns.insert("users".to_string(), vec!["id".to_string()]);

        let payload = ExportPayload {
            tables: &tables,
            columns: &columns,
            dialect: "oracle",
        };
        let temp_dir = std::env::temp_dir().join("synth_test_sql_oracle");
        std::fs::create_dir_all(&temp_dir).unwrap();

        export(&payload, &ExportFormat::Sql, &temp_dir).unwrap();

        let content = std::fs::read_to_string(temp_dir.join("users.sql")).unwrap();
        assert!(content.contains("INSERT INTO \"USERS\" (\"ID\") VALUES (1);"));

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn sql_export_escapes_single_quotes() {
        let mut tables = HashMap::new();
        tables.insert("t".to_string(), vec![vec![Value::from("o'brien")]]);

        let payload = ExportPayload {
            tables: &tables,
            columns: &HashMap::new(),
            dialect: "mysql",
        };
        let temp_dir = std::env::temp_dir().join("synth_test_sql_quote");
        std::fs::create_dir_all(&temp_dir).unwrap();

        export(&payload, &ExportFormat::Sql, &temp_dir).unwrap();

        let content = std::fs::read_to_string(temp_dir.join("t.sql")).unwrap();
        assert!(content.contains("'o''brien'"));

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }
}
