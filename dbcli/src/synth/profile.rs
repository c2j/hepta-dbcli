use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableProfile {
    pub table: String,
    pub row_count: usize,
    #[serde(default)]
    pub column_order: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_values: Option<Vec<(String, f64)>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_integer: bool,
}

const TOP_VALUES_CAP: usize = 50;

fn is_unsupported_placeholder(s: &str) -> bool {
    s.starts_with("<unsupported type")
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

        let numeric_strings: Option<Vec<f64>> = if !non_null.is_empty() && non_null[0].is_string() {
            let parsed: Option<Vec<f64>> = non_null
                .iter()
                .map(|v| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
                .collect();
            parsed.filter(|nums| !nums.is_empty())
        } else {
            None
        };

        let (logical_type, min, max, mean, std_dev) = if non_null.is_empty() {
            ("unknown".to_string(), None, None, None, None)
        } else if non_null[0].is_number() || numeric_strings.is_some() {
            let nums: Vec<f64> = if non_null[0].is_number() {
                non_null.iter().filter_map(|v| v.as_f64()).collect()
            } else {
                numeric_strings.clone().unwrap()
            };
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
            if non_null
                .iter()
                .any(|v| v.as_str().map(is_unsupported_placeholder).unwrap_or(false))
            {
                ("unsupported".to_string(), None, None, None, None)
            } else {
                ("categorical".to_string(), None, None, None, None)
            }
        } else {
            ("unknown".to_string(), None, None, None, None)
        };

        let is_integer = logical_type == "numerical"
            && non_null
                .iter()
                .filter_map(|v| {
                    v.as_f64()
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
                })
                .all(|f| f.fract() == 0.0);

        let top_values = if logical_type == "categorical" {
            let mut counts: HashMap<String, usize> = HashMap::new();
            for v in &non_null {
                if let Some(s) = v.as_str() {
                    *counts.entry(s.to_string()).or_insert(0) += 1;
                }
            }
            let mut entries: Vec<(String, f64)> = counts
                .into_iter()
                .map(|(k, c)| (k, c as f64 / non_null.len() as f64))
                .collect();
            entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            entries.truncate(TOP_VALUES_CAP);
            Some(entries)
        } else {
            None
        };

        Self {
            logical_type,
            null_rate,
            cardinality,
            min,
            max,
            mean,
            std_dev,
            top_values,
            is_integer,
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
            column_order: columns.to_vec(),
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
    fn column_profile_captures_top_values_for_categorical() {
        let samples: Vec<Value> = ["a", "a", "a", "b", "b", "c"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();

        let profile = ColumnProfile::from_samples(&samples);
        let top = profile.top_values.expect("top_values captured");
        assert_eq!(top.len(), 3);
        assert_eq!(top[0], ("a".to_string(), 0.5));
        assert_eq!(top[1], ("b".to_string(), 1.0 / 3.0));
        assert_eq!(top[2], ("c".to_string(), 1.0 / 6.0));
    }

    #[test]
    fn column_profile_top_values_capped_at_50() {
        let samples: Vec<Value> = (0..80)
            .map(|i| Value::from(format!("v{}", i)))
            .chain(std::iter::repeat_n(Value::from("common"), 20))
            .collect();

        let profile = ColumnProfile::from_samples(&samples);
        let top = profile.top_values.expect("top_values captured");
        assert_eq!(top.len(), 50);
        assert_eq!(top[0], ("common".to_string(), 0.2));
    }

    #[test]
    fn column_profile_detects_integer_columns() {
        let samples = vec![
            serde_json::json!(1),
            serde_json::json!(2),
            serde_json::json!(3),
        ];
        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "numerical");
        assert!(profile.is_integer);
    }

    #[test]
    fn column_profile_detects_fractional_numerical() {
        let samples = vec![serde_json::json!(1.5), serde_json::json!(2.0)];
        let profile = ColumnProfile::from_samples(&samples);
        assert!(!profile.is_integer);
    }

    #[test]
    fn column_profile_detects_numeric_strings_as_numerical() {
        // DECIMAL/NUMBER 经常见驱动反序列化为字符串，须按数值训练
        let samples: Vec<Value> = ["44.40", "31.70", "40.72", "39.73"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();

        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "numerical");
        assert!(!profile.is_integer);
        assert!((profile.mean.unwrap() - 39.1375).abs() < 1e-9);
    }

    #[test]
    fn column_profile_detects_integer_strings_as_integer() {
        let samples: Vec<Value> = ["1", "2", "3", "4"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "numerical");
        assert!(profile.is_integer);
    }

    #[test]
    fn column_profile_stays_categorical_when_strings_unparseable() {
        let samples: Vec<Value> = ["A", "B", "C", "A"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "categorical");
    }

    #[test]
    fn column_profile_marks_driver_placeholder_unsupported() {
        let placeholder = "<unsupported type timestamptz>: \\x0002b0cf204c2000";
        let samples = vec![Value::from(placeholder), Value::from(placeholder)];
        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "unsupported");
        assert!(profile.top_values.is_none());
    }

    #[test]
    fn column_profile_numerical_has_no_top_values() {
        let samples = vec![serde_json::json!(1), serde_json::json!(2)];
        let profile = ColumnProfile::from_samples(&samples);
        assert!(profile.top_values.is_none());
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
            column_order: vec![],
            columns: HashMap::new(),
        };

        let json = serde_json::to_string_pretty(&profile).unwrap();
        let loaded: TableProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.row_count, 100);
    }

    #[test]
    fn profile_column_order_follows_input_not_hash() {
        let columns = vec!["zeta".to_string(), "alpha".to_string(), "mid".to_string()];
        let rows = vec![vec![
            serde_json::json!(1),
            serde_json::json!(2),
            serde_json::json!(3),
        ]];

        let profile = TableProfile::from_rows("t", &columns, &rows);
        assert_eq!(profile.column_order, vec!["zeta", "alpha", "mid"]);
    }

    #[test]
    fn profile_json_roundtrip_preserves_column_order() {
        let columns = vec!["b".to_string(), "a".to_string()];
        let rows = vec![vec![serde_json::json!(1), serde_json::json!(2)]];
        let profile = TableProfile::from_rows("t", &columns, &rows);

        let json = serde_json::to_string(&profile).unwrap();
        let loaded: TableProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.column_order, vec!["b", "a"]);
    }

    #[test]
    fn profile_json_without_column_order_defaults_empty() {
        let json = r#"{"table": "t", "row_count": 3, "columns": {}}"#;
        let loaded: TableProfile = serde_json::from_str(json).unwrap();
        assert_eq!(loaded.table, "t");
        assert!(loaded.column_order.is_empty());
    }
}
