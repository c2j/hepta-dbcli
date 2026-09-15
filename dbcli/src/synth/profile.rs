use chrono::{Datelike, NaiveDate, NaiveDateTime};
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
    /// Max number of fractional digits observed in string-encoded samples;
    /// `None` when the column is not a fixed-scale numeric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimal_scale: Option<u8>,
    /// chrono format that all datetime samples matched, if one was inferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datetime_format: Option<String>,
}

const TOP_VALUES_CAP: usize = 50;
const NUMERIC_TOP_VALUES_MAX: usize = 50;

fn is_unsupported_placeholder(s: &str) -> bool {
    s.starts_with("<unsupported type")
}

fn sql_type_base(data_type: &str) -> String {
    let base = data_type
        .split('(')
        .next()
        .unwrap_or(data_type)
        .trim()
        .to_ascii_lowercase();
    // MySQL appends integer display attributes after the bare type name
    // ("bigint unsigned", "int unsigned zerofill") and since 8.0.19 omits the
    // display width, so the parenthesized split above is not enough. Strip the
    // attribute tail; the bare type decides the logical category.
    let mut base = base.as_str();
    while let Some(stripped) = [" unsigned", " signed", " zerofill"]
        .iter()
        .find_map(|suffix| base.strip_suffix(suffix))
    {
        base = stripped;
    }
    base.to_string()
}

/// Scale `s` from a SQL `(p,s)` declaration such as `numeric(16,2)`,
/// `decimal(18, 4)`, or `NUMBER(18,4)`. One-argument forms (`float(24)`,
/// `varchar(8)`) and unparsable inner lists yield `None`.
fn sql_type_scale(data_type: &str) -> Option<u8> {
    let start = data_type.find('(')?;
    let end = data_type[start + 1..].find(')')?;
    let inner = data_type[start + 1..start + 1 + end].trim();
    let mut parts = inner.split(',');
    let _precision = parts.next()?.trim();
    let scale = parts.next()?.trim();
    if parts.next().is_some() {
        return None;
    }
    scale.parse().ok()
}

/// Fractional-digit count of a plain decimal literal: optional sign, digits,
/// at most one `.`. Exponent forms (`1.2e3`) and any other character are
/// rejected so f64 noise / scientific notation cannot inflate the scale.
fn decimal_literal_scale(s: &str) -> Option<u8> {
    let s = s.trim();
    let s = s
        .strip_prefix('+')
        .or_else(|| s.strip_prefix('-'))
        .unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    if s.bytes().any(|b| b == b'e' || b == b'E') {
        return None;
    }
    let mut seen_dot = false;
    let mut seen_digit = false;
    let mut frac: u8 = 0;
    for b in s.bytes() {
        match b {
            b'0'..=b'9' => {
                seen_digit = true;
                if seen_dot {
                    frac = frac.saturating_add(1);
                }
            }
            b'.' if !seen_dot => seen_dot = true,
            _ => return None,
        }
    }
    if !seen_digit {
        return None;
    }
    Some(if seen_dot { frac } else { 0 })
}

fn learned_decimal_scale(non_null: &[&Value], data_type: Option<&str>) -> Option<u8> {
    let sample_scale = non_null
        .iter()
        .filter_map(|v| v.as_str().and_then(decimal_literal_scale))
        .max();
    let ddl_scale = data_type.and_then(sql_type_scale);
    [sample_scale, ddl_scale].into_iter().flatten().max()
}

fn is_numeric_sql_type(data_type: &str) -> bool {
    matches!(
        sql_type_base(data_type).as_str(),
        "tinyint"
            | "smallint"
            | "mediumint"
            | "int"
            | "integer"
            | "bigint"
            | "int2"
            | "int4"
            | "int8"
            | "oid"
            | "serial"
            | "bigserial"
            | "smallserial"
            | "number"
            | "decimal"
            | "numeric"
            | "money"
            | "float"
            | "float4"
            | "float8"
            | "double"
            | "double precision"
            | "real"
            | "binary_float"
            | "binary_double"
            // DuckDB unsigned / wide integers (COLUMN_TYPE has no "unsigned"
            // suffix there, the type name itself is unsigned).
            | "utinyint"
            | "usmallint"
            | "uinteger"
            | "ubigint"
            | "uhugeint"
            | "hugeint"
    )
}

fn is_datetime_sql_type(data_type: &str) -> bool {
    let base = sql_type_base(data_type);
    // `starts_with("timestamp")` covers `timestamp(6)` / `timestamp with time
    // zone`; `starts_with("time ")` covers `time with/without time zone`.
    matches!(
        base.as_str(),
        "date" | "time" | "timetz" | "datetime" | "timestamp" | "timestamptz" | "year" | "interval"
    ) || base.starts_with("timestamp")
        || base.starts_with("time ")
}

fn compact_yyyymmdd_number(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.len() != 8 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let date = NaiveDate::parse_from_str(s, "%Y%m%d").ok()?;
    if !(1900..=2100).contains(&date.year()) {
        return None;
    }
    s.parse().ok()
}

fn is_datetime_text(s: &str) -> bool {
    let s = s.trim();
    if compact_yyyymmdd_number(s).is_some() {
        return true;
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
        || NaiveDate::parse_from_str(s, "%Y/%m/%d").is_ok()
        || NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").is_ok()
        || NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").is_ok()
        || NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").is_ok()
        || NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
        || chrono::DateTime::parse_from_rfc3339(s).is_ok()
}

fn numerical_stats(nums: &[f64]) -> (Option<Value>, Option<Value>, Option<f64>, Option<f64>) {
    let min = nums.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let mean = nums.iter().sum::<f64>() / nums.len() as f64;
    let variance = nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / nums.len() as f64;
    (
        Some(Value::from(min)),
        Some(Value::from(max)),
        Some(mean),
        Some(variance.sqrt()),
    )
}

fn frequency_top_values(non_null: &[&Value]) -> Option<Vec<(String, f64)>> {
    if non_null.is_empty() {
        return None;
    }
    let mut counts: HashMap<String, usize> = HashMap::new();
    for v in non_null {
        let key = if let Some(s) = v.as_str() {
            Some(s.to_string())
        } else if v.is_number() {
            Some(v.to_string())
        } else {
            None
        };
        if let Some(key) = key {
            *counts.entry(key).or_insert(0) += 1;
        }
    }
    if counts.is_empty() {
        return None;
    }
    let mut entries: Vec<(String, f64)> = counts
        .into_iter()
        .map(|(k, c)| (k, c as f64 / non_null.len() as f64))
        .collect();
    entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    entries.truncate(TOP_VALUES_CAP);
    Some(entries)
}

fn lookup_type<'a>(types: Option<&'a HashMap<String, String>>, name: &str) -> Option<&'a str> {
    let types = types?;
    types
        .get(name)
        .or_else(|| {
            types
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v)
        })
        .map(String::as_str)
}

impl ColumnProfile {
    pub fn from_samples(samples: &[Value]) -> Self {
        Self::from_samples_typed(samples, None)
    }

    pub fn from_samples_typed(samples: &[Value], data_type: Option<&str>) -> Self {
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

        let schema_datetime = data_type.map(is_datetime_sql_type).unwrap_or(false);
        let schema_numeric = data_type.map(is_numeric_sql_type).unwrap_or(false);

        let all_compact_dates = !non_null.is_empty()
            && !schema_numeric
            && non_null.iter().all(|v| {
                v.as_str()
                    .map(|s| compact_yyyymmdd_number(s).is_some())
                    .unwrap_or(false)
            });
        let all_datetime_text = !non_null.is_empty()
            && !schema_numeric
            && non_null
                .iter()
                .all(|v| v.as_str().map(is_datetime_text).unwrap_or(false));

        let numeric_strings: Option<Vec<f64>> = if !non_null.is_empty() && non_null[0].is_string() {
            let parsed: Option<Vec<f64>> = non_null
                .iter()
                .map(|v| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
                .collect();
            parsed.filter(|nums| !nums.is_empty())
        } else {
            None
        };

        let (logical_type, mut min, mut max, mut mean, mut std_dev) = if non_null
            .iter()
            .any(|v| v.as_str().map(is_unsupported_placeholder).unwrap_or(false))
        {
            ("unsupported".to_string(), None, None, None, None)
        } else if non_null.is_empty() {
            if schema_datetime {
                ("datetime".to_string(), None, None, None, None)
            } else if schema_numeric {
                ("numerical".to_string(), None, None, None, None)
            } else {
                ("unknown".to_string(), None, None, None, None)
            }
        } else if schema_datetime || all_compact_dates || all_datetime_text {
            if all_compact_dates || non_null[0].is_number() || numeric_strings.is_some() {
                let nums: Vec<f64> = if all_compact_dates {
                    non_null
                        .iter()
                        .filter_map(|v| v.as_str().and_then(compact_yyyymmdd_number))
                        .collect()
                } else if non_null[0].is_number() {
                    non_null.iter().filter_map(|v| v.as_f64()).collect()
                } else {
                    numeric_strings.clone().unwrap_or_default()
                };
                let (min, max, mean, std_dev) = if nums.is_empty() {
                    (None, None, None, None)
                } else {
                    numerical_stats(&nums)
                };
                ("datetime".to_string(), min, max, mean, std_dev)
            } else {
                ("datetime".to_string(), None, None, None, None)
            }
        } else if non_null[0].is_number() || numeric_strings.is_some() {
            let nums: Vec<f64> = if non_null[0].is_number() {
                non_null.iter().filter_map(|v| v.as_f64()).collect()
            } else {
                numeric_strings.clone().unwrap()
            };
            let (min, max, mean, std_dev) = numerical_stats(&nums);
            ("numerical".to_string(), min, max, mean, std_dev)
        } else if non_null[0].is_string() {
            ("categorical".to_string(), None, None, None, None)
        } else {
            ("unknown".to_string(), None, None, None, None)
        };

        let parsed_nums: Vec<f64> = non_null
            .iter()
            .filter_map(|v| {
                v.as_f64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))
            })
            .collect();
        let is_numeric_datetime = logical_type == "datetime"
            && (all_compact_dates
                || non_null.first().is_some_and(|v| v.is_number())
                || numeric_strings.is_some());

        let mut datetime_format = None;
        if logical_type == "datetime" && !non_null.is_empty() && !is_numeric_datetime {
            let samples: Vec<Value> = non_null.iter().map(|v| (*v).clone()).collect();
            datetime_format = super::datetime::infer_format(&samples);
            if let Some(fmt) = datetime_format.as_deref() {
                let epochs: Vec<f64> = non_null
                    .iter()
                    .filter_map(|v| super::datetime::parse_to_epoch(v, Some(fmt)))
                    .collect();
                if !epochs.is_empty() {
                    (min, max, mean, std_dev) = numerical_stats(&epochs);
                }
            }
        }

        let is_integer = matches!(logical_type.as_str(), "numerical" | "datetime")
            && parsed_nums.len() == non_null.len()
            && !parsed_nums.is_empty()
            && parsed_nums.iter().all(|f| f.fract() == 0.0);

        let is_repeated_low_cardinality_numeric = logical_type == "numerical"
            && cardinality > 1
            && cardinality < non_null.len()
            && cardinality <= NUMERIC_TOP_VALUES_MAX;
        let top_values = if logical_type == "categorical"
            || is_repeated_low_cardinality_numeric
            || (logical_type == "datetime" && !non_null.is_empty() && !is_numeric_datetime)
        {
            frequency_top_values(&non_null)
        } else {
            None
        };

        let decimal_scale = if logical_type == "numerical" && !is_integer {
            learned_decimal_scale(&non_null, data_type)
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
            decimal_scale,
            datetime_format,
        }
    }
}

impl TableProfile {
    pub fn from_rows(table: &str, columns: &[String], rows: &[Vec<Value>]) -> Self {
        Self::from_rows_typed(table, columns, rows, None)
    }

    pub fn from_rows_typed(
        table: &str,
        columns: &[String],
        rows: &[Vec<Value>],
        data_types: Option<&HashMap<String, String>>,
    ) -> Self {
        let row_count = rows.len();
        let mut column_profiles = HashMap::new();

        for (col_idx, col_name) in columns.iter().enumerate() {
            let samples: Vec<Value> = rows
                .iter()
                .filter_map(|row| row.get(col_idx).cloned())
                .collect();
            column_profiles.insert(
                col_name.clone(),
                ColumnProfile::from_samples_typed(&samples, lookup_type(data_types, col_name)),
            );
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
    fn should_capture_top_values_for_low_cardinality_numeric() {
        let samples: Vec<Value> = (0..190).map(|i| Value::from((i % 19) as i64)).collect();

        let profile = ColumnProfile::from_samples(&samples);
        let top = profile.top_values.expect("top_values captured");

        assert_eq!(profile.logical_type, "numerical");
        assert_eq!(top.len(), 19);
        assert!(top.len() <= NUMERIC_TOP_VALUES_MAX);
        assert!(top
            .iter()
            .all(|(_, weight)| (*weight - 1.0 / 19.0).abs() < 1e-12));
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
    fn column_profile_detects_yyyymmdd_strings_as_datetime() {
        // VARCHAR(8) 存 YYYYMMDD 时驱动给出数字字符串，不得当成 numerical
        let samples: Vec<Value> = ["20240101", "20240315", "20241231"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "datetime");
        assert!(profile.is_integer);
        assert!(profile.mean.is_some());
    }

    #[test]
    fn column_profile_detects_iso_date_strings_as_datetime() {
        let samples: Vec<Value> = ["2024-01-01", "2024-03-15", "2024-12-31"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "datetime");
        assert!(!profile.is_integer);
        assert!(profile.top_values.is_some());
    }

    #[test]
    fn should_infer_iso_datetime_format_and_epoch_stats() {
        let samples: Vec<Value> = ["2024-01-01", "2024-03-15", "2024-12-31"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let profile =
            ColumnProfile::from_samples_typed(&samples, Some("timestamp without time zone"));
        assert_eq!(profile.logical_type, "datetime");
        assert_eq!(profile.datetime_format.as_deref(), Some("%Y-%m-%d"));
        let min = crate::synth::datetime::parse_to_epoch(&samples[0], Some("%Y-%m-%d")).unwrap();
        let max = crate::synth::datetime::parse_to_epoch(&samples[2], Some("%Y-%m-%d")).unwrap();
        assert_eq!(profile.min.as_ref().and_then(Value::as_f64), Some(min));
        assert_eq!(profile.max.as_ref().and_then(Value::as_f64), Some(max));
        assert!(profile.mean.is_some());
        assert!(profile.std_dev.is_some());
        assert!(profile.mean.unwrap() > min && profile.mean.unwrap() < max);
    }

    #[test]
    fn should_leave_compact_yyyymmdd_without_datetime_format() {
        let samples: Vec<Value> = ["20240101", "20240315", "20241231"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "datetime");
        assert!(profile.datetime_format.is_none());
        assert!(profile.mean.unwrap() > 20_000_000.0);
    }

    #[test]
    fn should_not_infer_format_for_oracle_style_dates() {
        let samples: Vec<Value> = ["15-JAN-24", "16-JAN-24", "17-JAN-24"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let profile = ColumnProfile::from_samples_typed(&samples, Some("date"));
        assert_eq!(profile.logical_type, "datetime");
        assert!(profile.datetime_format.is_none());
        assert!(profile.mean.is_none());
        assert!(profile.top_values.is_some());
    }

    #[test]
    fn column_profile_eight_digit_non_dates_stay_numerical() {
        let samples: Vec<Value> = ["12345678", "10000001", "99999999"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "numerical");
    }

    #[test]
    fn column_profile_varchar_schema_still_detects_compact_dates() {
        let samples: Vec<Value> = ["20240101", "20240315"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let profile = ColumnProfile::from_samples_typed(&samples, Some("character varying(8)"));
        assert_eq!(profile.logical_type, "datetime");
    }

    #[test]
    fn column_profile_all_null_numeric_schema_is_numerical() {
        let samples = vec![Value::Null, Value::Null, Value::Null];
        let profile = ColumnProfile::from_samples_typed(&samples, Some("numeric(16,2)"));
        assert_eq!(profile.logical_type, "numerical");
        assert_eq!(profile.null_rate, 1.0);
        assert!(profile.mean.is_none());
    }

    #[test]
    fn column_profile_unsigned_integer_schema_is_numerical() {
        // MySQL 8.0.19+ drops the display width, so COLUMN_TYPE is
        // "bigint unsigned" and the modifier must be stripped.
        let samples = vec![Value::Null, Value::Null];
        for ty in [
            "bigint unsigned",
            "int unsigned",
            "smallint unsigned",
            "double unsigned",
            "int unsigned zerofill",
        ] {
            let profile = ColumnProfile::from_samples_typed(&samples, Some(ty));
            assert_eq!(profile.logical_type, "numerical", "type '{ty}'");
        }
    }

    #[test]
    fn column_profile_duckdb_unsigned_types_are_numerical() {
        let samples = vec![Value::Null, Value::Null];
        for ty in ["UBIGINT", "UINTEGER", "USMALLINT", "UTINYINT", "HUGEINT"] {
            let profile = ColumnProfile::from_samples_typed(&samples, Some(ty));
            assert_eq!(profile.logical_type, "numerical", "type '{ty}'");
        }
    }

    #[test]
    fn column_profile_unsigned_is_not_datetime() {
        let samples: Vec<Value> = ["20240101", "20240315"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let profile = ColumnProfile::from_samples_typed(&samples, Some("bigint unsigned"));
        assert_eq!(profile.logical_type, "numerical");
    }

    #[test]
    fn column_profile_all_null_without_schema_stays_unknown() {
        let samples = vec![Value::Null, Value::Null];
        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "unknown");
    }

    #[test]
    fn column_profile_date_schema_overrides_empty_values() {
        let samples = vec![Value::Null, Value::Null];
        let profile =
            ColumnProfile::from_samples_typed(&samples, Some("timestamp without time zone"));
        assert_eq!(profile.logical_type, "datetime");
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
    fn table_profile_from_rows_typed_uses_schema() {
        let columns = vec!["amt".to_string(), "biz_date".to_string()];
        let rows = vec![
            vec![Value::Null, Value::from("20240101")],
            vec![Value::Null, Value::from("20240315")],
        ];
        let mut types = HashMap::new();
        types.insert("amt".to_string(), "numeric(16,2)".to_string());
        types.insert("biz_date".to_string(), "character varying(8)".to_string());

        let profile = TableProfile::from_rows_typed("t", &columns, &rows, Some(&types));
        assert_eq!(profile.columns["amt"].logical_type, "numerical");
        assert_eq!(profile.columns["biz_date"].logical_type, "datetime");
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

    #[test]
    fn should_take_max_scale_on_mixed_sample_scales() {
        let samples: Vec<Value> = ["1.2", "3.456", "7.89", "-0.10"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let profile = ColumnProfile::from_samples(&samples);
        assert_eq!(profile.logical_type, "numerical");
        assert!(!profile.is_integer);
        assert_eq!(profile.decimal_scale, Some(3));
    }

    #[test]
    fn should_learn_scale_from_ddl_type_when_samples_are_numeric() {
        let samples = vec![
            serde_json::json!(10.5),
            serde_json::json!(20.25),
            serde_json::json!(30.0),
        ];
        let profile = ColumnProfile::from_samples_typed(&samples, Some("numeric(16,2)"));
        assert_eq!(profile.logical_type, "numerical");
        assert!(!profile.is_integer);
        assert_eq!(profile.decimal_scale, Some(2));
    }

    #[test]
    fn should_not_learn_scale_for_integer_or_datetime_columns() {
        let ints = vec![
            serde_json::json!(1),
            serde_json::json!(2),
            serde_json::json!(3),
        ];
        let int_profile = ColumnProfile::from_samples_typed(&ints, Some("decimal(18,4)"));
        assert_eq!(int_profile.logical_type, "numerical");
        assert!(int_profile.is_integer);
        assert_eq!(int_profile.decimal_scale, None);

        let dates: Vec<Value> = ["2024-01-01", "2024-03-15", "2024-12-31"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        let date_profile = ColumnProfile::from_samples(&dates);
        assert_eq!(date_profile.logical_type, "datetime");
        assert_eq!(date_profile.decimal_scale, None);
    }
}
