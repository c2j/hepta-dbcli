use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde_json::Value;

const CANDIDATE_FORMATS: &[&str] = &[
    "%Y-%m-%d",
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y-%m-%d %H:%M:%S%.3f",
    "%Y-%m-%dT%H:%M:%S",
    "%Y-%m-%dT%H:%M:%S%.f",
    "%Y-%m-%dT%H:%M:%S%:z",
    "%Y/%m/%d",
];

/// Parse a JSON value as a datetime and return seconds since Unix epoch.
///
/// Naive datetimes are interpreted as UTC (`NaiveDateTime::and_utc()`); a
/// date-only value is midnight UTC. Timezone-aware strings keep their own
/// absolute instant.
pub(crate) fn parse_to_epoch(value: &Value, fmt: Option<&str>) -> Option<f64> {
    let s = value.as_str()?;
    if let Some(fmt) = fmt {
        return parse_with_fmt(s, fmt).map(epoch_of);
    }
    for candidate in CANDIDATE_FORMATS {
        if let Some(dt) = parse_with_fmt(s, candidate) {
            return Some(epoch_of(dt));
        }
    }
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| epoch_of(dt.with_timezone(&Utc)))
}

/// Infer a chrono format that round-trips every sample byte-identically.
///
/// Compact `YYYYMMDD` is intentionally absent from the candidate list so those
/// columns keep integer-Normal modelling.
pub(crate) fn infer_format(samples: &[Value]) -> Option<String> {
    let mut strings = Vec::new();
    for value in samples {
        if value.is_null() {
            continue;
        }
        strings.push(value.as_str()?);
    }
    if strings.is_empty() {
        return None;
    }
    for fmt in CANDIDATE_FORMATS {
        let all_match = strings.iter().all(|s| {
            parse_to_epoch(&Value::from(*s), Some(fmt))
                .and_then(|epoch| format_epoch(epoch, fmt))
                .as_deref()
                == Some(*s)
        });
        if all_match {
            return Some((*fmt).to_string());
        }
    }
    None
}

/// Render an epoch (UTC seconds) with the given chrono format.
pub(crate) fn format_epoch(epoch: f64, fmt: &str) -> Option<String> {
    Some(datetime_from_epoch(epoch)?.format(fmt).to_string())
}

fn parse_with_fmt(s: &str, fmt: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_str(s, fmt) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(ndt) = NaiveDateTime::parse_from_str(s, fmt) {
        return Some(ndt.and_utc());
    }
    if let Ok(date) = NaiveDate::parse_from_str(s, fmt) {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc());
    }
    None
}

fn epoch_of(dt: DateTime<Utc>) -> f64 {
    // Integer microseconds stay exact in f64 until ~year 2255 (2^53 µs).
    dt.timestamp_micros() as f64 / 1_000_000.0
}

fn datetime_from_epoch(epoch: f64) -> Option<DateTime<Utc>> {
    if !epoch.is_finite() {
        return None;
    }
    let micros_f = (epoch * 1_000_000.0).round();
    if micros_f < i64::MIN as f64 || micros_f > i64::MAX as f64 {
        return None;
    }
    let micros = micros_f as i64;
    let secs = micros.div_euclid(1_000_000);
    let nsecs = u32::try_from(micros.rem_euclid(1_000_000) * 1000).ok()?;
    DateTime::<Utc>::from_timestamp(secs, nsecs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_round_trip_naive_datetime_as_utc() {
        let samples = [
            ("2024-03-15", "%Y-%m-%d"),
            ("2024-03-15 10:30:00", "%Y-%m-%d %H:%M:%S"),
            ("2024-03-15 10:30:00.123", "%Y-%m-%d %H:%M:%S%.f"),
            ("2024-03-15 10:30:00.123", "%Y-%m-%d %H:%M:%S%.3f"),
            ("2024-03-15T10:30:00", "%Y-%m-%dT%H:%M:%S"),
            ("2024-03-15T10:30:00.123456", "%Y-%m-%dT%H:%M:%S%.f"),
            ("2024-03-15T10:30:00+00:00", "%Y-%m-%dT%H:%M:%S%:z"),
            ("2024/03/15", "%Y/%m/%d"),
        ];
        for (s, fmt) in samples {
            let epoch = parse_to_epoch(&Value::from(s), Some(fmt))
                .unwrap_or_else(|| panic!("parse {s:?} with {fmt}"));
            let rendered =
                format_epoch(epoch, fmt).unwrap_or_else(|| panic!("format {epoch} with {fmt}"));
            assert_eq!(rendered, s, "round-trip {s:?} via {fmt}");
        }

        let date_epoch = parse_to_epoch(&Value::from("2024-03-15"), Some("%Y-%m-%d")).unwrap();
        let midnight = NaiveDate::from_ymd_opt(2024, 3, 15)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp() as f64;
        assert!((date_epoch - midnight).abs() < 1e-9);
    }

    #[test]
    fn should_infer_format_only_when_every_sample_round_trips() {
        let dates: Vec<Value> = ["2024-01-01", "2024-03-15", "2024-12-31"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        assert_eq!(infer_format(&dates).as_deref(), Some("%Y-%m-%d"));

        let ts: Vec<Value> = ["2024-01-01 00:00:00", "2024-06-15 12:30:00"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        assert_eq!(infer_format(&ts).as_deref(), Some("%Y-%m-%d %H:%M:%S"));
    }

    #[test]
    fn should_return_none_for_mixed_formats() {
        let mixed = vec![
            Value::from("2024-01-01"),
            Value::from("2024-01-01 12:00:00"),
        ];
        assert_eq!(infer_format(&mixed), None);
    }

    #[test]
    fn should_preserve_tz_aware_offset_as_absolute_instant() {
        let aware = "2024-01-15T12:34:56+08:00";
        let epoch = parse_to_epoch(&Value::from(aware), Some("%Y-%m-%dT%H:%M:%S%:z"))
            .expect("tz-aware parse");
        let naive = parse_to_epoch(
            &Value::from("2024-01-15T12:34:56"),
            Some("%Y-%m-%dT%H:%M:%S"),
        )
        .expect("naive parse");
        assert!(
            (naive - epoch - 8.0 * 3600.0).abs() < 1e-6,
            "offset must shift the instant, naive={naive} aware={epoch}"
        );
        let rfc = DateTime::parse_from_rfc3339(aware).unwrap().timestamp() as f64;
        assert!((epoch - rfc).abs() < 1e-6);
    }

    #[test]
    fn should_return_none_for_unparseable_values() {
        assert_eq!(
            parse_to_epoch(&Value::from("not-a-date"), Some("%Y-%m-%d")),
            None
        );
        assert_eq!(parse_to_epoch(&Value::from("not-a-date"), None), None);
        assert_eq!(parse_to_epoch(&Value::Null, Some("%Y-%m-%d")), None);
        assert_eq!(format_epoch(f64::NAN, "%Y-%m-%d"), None);
        assert_eq!(infer_format(&[Value::from("15-JAN-24")]), None);
    }

    #[test]
    fn should_not_infer_compact_yyyymmdd_format() {
        let samples: Vec<Value> = ["20240101", "20240315", "20241231"]
            .iter()
            .map(|s| Value::from(*s))
            .collect();
        assert_eq!(infer_format(&samples), None);
        assert!(!CANDIDATE_FORMATS.contains(&"%Y%m%d"));
    }
}
