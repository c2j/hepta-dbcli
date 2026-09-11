use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;

pub enum ExportFormat {
    Csv,
    Jsonl,
    Json,
    Sql,
}

pub fn export(
    data: &HashMap<String, Vec<Vec<Value>>>,
    format: &ExportFormat,
    output_dir: &std::path::Path,
) -> Result<(), String> {
    match format {
        ExportFormat::Csv => export_csv(data, output_dir),
        ExportFormat::Jsonl => export_jsonl(data, output_dir),
        ExportFormat::Json => export_json(data, output_dir),
        ExportFormat::Sql => export_sql(data, output_dir),
    }
}

fn export_csv(
    data: &HashMap<String, Vec<Vec<Value>>>,
    output_dir: &std::path::Path,
) -> Result<(), String> {
    for (table_name, rows) in data {
        let path = output_dir.join(format!("{}.csv", table_name));
        let mut file =
            std::fs::File::create(&path).map_err(|e| format!("create CSV file: {}", e))?;

        if let Some(first_row) = rows.first() {
            let header: Vec<String> = (0..first_row.len()).map(|i| format!("col_{}", i)).collect();
            writeln!(file, "{}", header.join(","))
                .map_err(|e| format!("write CSV header: {}", e))?;
        }

        for row in rows {
            let values: Vec<String> = row
                .iter()
                .map(|v| match v {
                    Value::String(s) => format!("\"{}\"", s.replace('"', "\"\"")),
                    Value::Null => "null".to_string(),
                    _ => v.to_string(),
                })
                .collect();
            writeln!(file, "{}", values.join(",")).map_err(|e| format!("write CSV row: {}", e))?;
        }
    }
    Ok(())
}

fn export_jsonl(
    data: &HashMap<String, Vec<Vec<Value>>>,
    output_dir: &std::path::Path,
) -> Result<(), String> {
    for (table_name, rows) in data {
        let path = output_dir.join(format!("{}.jsonl", table_name));
        let mut file =
            std::fs::File::create(&path).map_err(|e| format!("create JSONL file: {}", e))?;

        for row in rows {
            let obj: HashMap<String, Value> = row
                .iter()
                .enumerate()
                .map(|(i, v)| (format!("col_{}", i), v.clone()))
                .collect();
            let json =
                serde_json::to_string(&obj).map_err(|e| format!("serialize JSONL row: {}", e))?;
            writeln!(file, "{}", json).map_err(|e| format!("write JSONL row: {}", e))?;
        }
    }
    Ok(())
}

fn export_json(
    data: &HashMap<String, Vec<Vec<Value>>>,
    output_dir: &std::path::Path,
) -> Result<(), String> {
    for (table_name, rows) in data {
        let path = output_dir.join(format!("{}.json", table_name));
        let json =
            serde_json::to_string_pretty(rows).map_err(|e| format!("serialize JSON: {}", e))?;
        std::fs::write(&path, json).map_err(|e| format!("write JSON file: {}", e))?;
    }
    Ok(())
}

fn export_sql(
    data: &HashMap<String, Vec<Vec<Value>>>,
    output_dir: &std::path::Path,
) -> Result<(), String> {
    for (table_name, rows) in data {
        let path = output_dir.join(format!("{}.sql", table_name));
        let mut file =
            std::fs::File::create(&path).map_err(|e| format!("create SQL file: {}", e))?;

        if let Some(first_row) = rows.first() {
            let placeholders = vec!["?"; first_row.len()].join(", ");
            let insert = format!("INSERT INTO {} VALUES ({});", table_name, placeholders);

            for row in rows {
                let values: Vec<String> = row
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => format!("'{}'", s.replace('\'', "''")),
                        Value::Null => "NULL".to_string(),
                        Value::Number(n) => n.to_string(),
                        _ => format!("'{}'", v),
                    })
                    .collect();
                let sql = insert.replace("?", &values.join(", "));
                writeln!(file, "{}", sql).map_err(|e| format!("write SQL row: {}", e))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_export_creates_file() {
        let mut data = HashMap::new();
        data.insert(
            "test".to_string(),
            vec![
                vec![Value::from(1), Value::from("hello")],
                vec![Value::from(2), Value::from("world")],
            ],
        );

        let temp_dir = std::env::temp_dir().join("synth_test_csv");
        std::fs::create_dir_all(&temp_dir).unwrap();

        export(&data, &ExportFormat::Csv, &temp_dir).unwrap();

        let path = temp_dir.join("test.csv");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("col_0,col_1"));
        assert!(content.contains("1,\"hello\""));

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn jsonl_export_creates_file() {
        let mut data = HashMap::new();
        data.insert(
            "test".to_string(),
            vec![vec![Value::from(1), Value::from("hello")]],
        );

        let temp_dir = std::env::temp_dir().join("synth_test_jsonl");
        std::fs::create_dir_all(&temp_dir).unwrap();

        export(&data, &ExportFormat::Jsonl, &temp_dir).unwrap();

        let path = temp_dir.join("test.jsonl");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("col_0"));
        assert!(content.contains("hello"));

        std::fs::remove_dir_all(&temp_dir).unwrap();
    }
}
