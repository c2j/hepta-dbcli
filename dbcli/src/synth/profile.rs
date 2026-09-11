use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableProfile {
    pub table: String,
    pub row_count: usize,
    pub columns: HashMap<String, ColumnProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnProfile {
    pub logical_type: String,
    pub null_rate: f64,
    pub cardinality: usize,
    pub min: Option<Value>,
    pub max: Option<Value>,
    pub mean: Option<f64>,
    pub std_dev: Option<f64>,
}

impl ColumnProfile {
    pub fn from_samples(samples: &[Value]) -> Self {
        let total = samples.len();
        let null_count = samples.iter().filter(|v| v.is_null()).count();
        let non_null: Vec<&Value> = samples.iter().filter(|v| !v.is_null()).collect();

        let null_rate = if total > 0 {
            null_count as f64 / total as f64
        } else {
            0.0
        };

        let cardinality = non_null
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();

        let (logical_type, min, max, mean, std_dev) = if non_null.is_empty() {
            ("unknown".to_string(), None, None, None, None)
        } else if non_null[0].is_number() {
            let nums: Vec<f64> = non_null.iter().filter_map(|v| v.as_f64()).collect();
            let min = nums.iter().cloned().fold(f64::INFINITY, f64::min);
            let max = nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let mean = nums.iter().sum::<f64>() / nums.len() as f64;
            let variance = nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / nums.len() as f64;
            let std_dev = variance.sqrt();

            (
                "numerical".to_string(),
                Some(Value::from(min)),
                Some(Value::from(max)),
                Some(mean),
                Some(std_dev),
            )
        } else if non_null[0].is_string() {
            ("categorical".to_string(), None, None, None, None)
        } else {
            ("unknown".to_string(), None, None, None, None)
        };

        Self {
            logical_type,
            null_rate,
            cardinality,
            min,
            max,
            mean,
            std_dev,
        }
    }
}

impl TableProfile {
    pub fn from_rows(table: &str, columns: &[String], rows: &[Vec<Value>]) -> Self {
        let row_count = rows.len();
        let mut column_profiles = HashMap::new();

        for (col_idx, col_name) in columns.iter().enumerate() {
            let samples: Vec<Value> = rows
                .iter()
                .filter_map(|row| row.get(col_idx).cloned())
                .collect();
            column_profiles.insert(col_name.clone(), ColumnProfile::from_samples(&samples));
        }

        Self {
            table: table.to_string(),
            row_count,
            columns: column_profiles,
        }
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        let json =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize profile: {}", e))?;

        std::fs::write(path, json).map_err(|e| format!("write profile file: {}", e))?;

        Ok(())
    }

    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let content =
            std::fs::read_to_string(path).map_err(|e| format!("read profile file: {}", e))?;

        serde_json::from_str(&content).map_err(|e| format!("parse profile JSON: {}", e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_profile_captures_null_rate() {
        let samples = vec![
            serde_json::json!(1),
            serde_json::json!(2),
            serde_json::json!(null),
            serde_json::json!(4),
        ];

        let profile = ColumnProfile::from_samples(&samples);
        assert!((profile.null_rate - 0.25).abs() < 0.01);
        assert_eq!(profile.cardinality, 3);
    }

    #[test]
    fn column_profile_numerical_stats() {
        let samples = vec![
            serde_json::json!(10),
            serde_json::json!(20),
            serde_json::json!(30),
        ];

        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "numerical");
        assert!(profile.mean.is_some());
        assert!((profile.mean.unwrap() - 20.0).abs() < 0.01);
    }

    #[test]
    fn table_profile_from_rows() {
        let columns = vec!["id".to_string(), "name".to_string()];
        let rows = vec![
            vec![serde_json::json!(1), serde_json::json!("Alice")],
            vec![serde_json::json!(2), serde_json::json!("Bob")],
        ];

        let profile = TableProfile::from_rows("users", &columns, &rows);
        assert_eq!(profile.table, "users");
        assert_eq!(profile.row_count, 2);
        assert_eq!(profile.columns.len(), 2);
    }

    #[test]
    fn profile_json_roundtrip() {
        let profile = TableProfile {
            table: "t".to_string(),
            row_count: 100,
            columns: HashMap::new(),
        };

        let json = serde_json::to_string_pretty(&profile).unwrap();
        let loaded: TableProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.row_count, 100);
    }
}
