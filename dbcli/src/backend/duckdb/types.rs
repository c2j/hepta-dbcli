// ─── DuckDB value → JSON conversion ──────────────────────────────────
//
// Maps duckdb::types::ValueRef (arrow-backed row cells) to serde_json::Value.
// Complex container types (LIST/STRUCT/MAP/UNION/ARRAY) render a visible
// placeholder instead of a silent NULL — same philosophy as the GaussDB
// raw-bytes fallback. Phase 2 can upgrade them to real JSON structures.

use duckdb::types::{TimeUnit, ValueRef};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json::{json, Value};

pub(crate) fn value_ref_to_json(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Boolean(b) => json!(b),
        ValueRef::TinyInt(i) => json!(i64::from(i)),
        ValueRef::SmallInt(i) => json!(i64::from(i)),
        ValueRef::Int(i) => json!(i64::from(i)),
        ValueRef::BigInt(i) => json!(i),
        ValueRef::HugeInt(i) => i128_to_json(i),
        ValueRef::UTinyInt(i) => json!(u64::from(i)),
        ValueRef::USmallInt(i) => json!(u64::from(i)),
        ValueRef::UInt(i) => json!(u64::from(i)),
        ValueRef::UBigInt(i) => json!(i),
        ValueRef::Float(f) => f64_to_json(f64::from(f)),
        ValueRef::Double(f) => f64_to_json(f),
        ValueRef::Decimal(d) => decimal_to_json(d.to_string()),
        ValueRef::Timestamp(tu, raw) => timestamp_to_json(tu, raw),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(format!("\\x{}", hex_bytes(b))),
        ValueRef::Geometry(wkb) => Value::String(format!("\\x{}", hex_bytes(wkb))),
        ValueRef::Date32(days) => date32_to_json(days),
        ValueRef::Time64(tu, raw) => time64_to_json(tu, raw),
        ValueRef::Interval {
            months,
            days,
            nanos,
        } => json!(format_interval(months, days, nanos)),
        ValueRef::Enum(..) => match v.as_str() {
            Ok(s) => Value::String(s.to_owned()),
            Err(_) => Value::String(format!("{v:?}")),
        },
        // Complex containers (LIST/STRUCT/MAP/ARRAY/UNION) and any future
        // variants: visible placeholder instead of silent NULL.
        _ => Value::String(format!("{v:?}")),
    }
}

fn i128_to_json(i: i128) -> Value {
    match i64::try_from(i) {
        Ok(n) => json!(n),
        Err(_) => Value::String(i.to_string()),
    }
}

fn f64_to_json(f: f64) -> Value {
    serde_json::Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or_else(|| Value::String(f.to_string()))
}

/// Render a DuckDB DECIMAL (width ≤ 38) as JSON. rust_decimal caps at ~28
/// significant digits, so wider decimals keep their exact text form.
fn decimal_to_json(s: String) -> Value {
    let d = match Decimal::from_str_exact(&s) {
        Ok(d) => d,
        Err(_) => return Value::String(s),
    };
    if d.is_integer() {
        if let Some(i) = d.to_i64() {
            return json!(i);
        }
        if let Some(u) = d.to_u64() {
            return json!(u);
        }
        return Value::String(s);
    }
    let int_digits = integer_digit_count(&d);
    if int_digits + d.scale() as usize > 15 {
        return Value::String(s);
    }
    match d.to_f64() {
        Some(f) => f64_to_json(f),
        None => Value::String(s),
    }
}

fn integer_digit_count(d: &Decimal) -> usize {
    if d.is_zero() {
        return 1;
    }
    d.abs().to_string().split('.').next().unwrap_or("").len()
}

fn unit_to_micros(tu: TimeUnit, raw: i64) -> i64 {
    match tu {
        TimeUnit::Second => raw * 1_000_000,
        TimeUnit::Millisecond => raw * 1_000,
        TimeUnit::Microsecond => raw,
        TimeUnit::Nanosecond => raw / 1_000,
    }
}

fn timestamp_to_json(tu: TimeUnit, raw: i64) -> Value {
    let micros = unit_to_micros(tu, raw);
    match chrono::DateTime::from_timestamp_micros(micros) {
        Some(dt) => Value::String(dt.naive_utc().to_string()),
        None => Value::String(format!("{micros}us")),
    }
}

fn date32_to_json(days: i32) -> Value {
    let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1);
    match epoch.and_then(|e| e.checked_add_signed(chrono::Duration::days(i64::from(days)))) {
        Some(d) => Value::String(d.to_string()),
        None => Value::String(days.to_string()),
    }
}

fn time64_to_json(tu: TimeUnit, raw: i64) -> Value {
    let micros = unit_to_micros(tu, raw);
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if frac == 0 {
        Value::String(format!("{h:02}:{m:02}:{s:02}"))
    } else {
        Value::String(format!("{h:02}:{m:02}:{s:02}.{frac:06}"))
    }
}

fn format_interval(months: i32, days: i32, nanos: i64) -> String {
    let mut parts: Vec<String> = Vec::new();
    if months != 0 {
        let years = months / 12;
        let mons = months % 12;
        if years != 0 {
            parts.push(format!("{years} years"));
        }
        if mons != 0 {
            parts.push(format!("{mons} mons"));
        }
    }
    if days != 0 {
        parts.push(format!("{days} days"));
    }
    let micros = nanos / 1_000;
    if micros != 0 {
        let secs = micros.div_euclid(1_000_000);
        let frac = micros.rem_euclid(1_000_000);
        let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
        if frac > 0 {
            parts.push(format!("{h:02}:{m:02}:{s:02}.{frac:06}"));
        } else {
            parts.push(format!("{h:02}:{m:02}:{s:02}"));
        }
    }
    if parts.is_empty() {
        "00:00:00".to_string()
    } else {
        parts.join(" ")
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        result.push_str(&format!("{b:02x}"));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hugeint_within_i64_is_number() {
        assert_eq!(i128_to_json(42), json!(42i64));
        assert_eq!(i128_to_json(i64::MAX as i128), json!(i64::MAX));
    }

    #[test]
    fn hugeint_beyond_i64_is_exact_text() {
        assert_eq!(
            i128_to_json(170141183460469231731687303715884105727),
            Value::String("170141183460469231731687303715884105727".into())
        );
    }

    #[test]
    fn double_nan_renders_visible_text() {
        assert_eq!(f64_to_json(f64::NAN), Value::String("NaN".into()));
    }

    #[test]
    fn decimal_integer_becomes_number() {
        assert_eq!(decimal_to_json("42".into()), json!(42i64));
        assert_eq!(decimal_to_json("42.50".into()), json!(42.5));
    }

    #[test]
    fn decimal_high_precision_keeps_text() {
        let s = "1.1234567890123456789";
        assert_eq!(decimal_to_json(s.into()), Value::String(s.into()));
    }

    #[test]
    fn decimal_beyond_rust_decimal_keeps_text() {
        let s = "12345678901234567890123456789012345678.5";
        assert_eq!(decimal_to_json(s.into()), Value::String(s.into()));
    }

    #[test]
    fn timestamp_micros_formats_naive_utc() {
        // 2024-01-02 03:04:05 UTC
        let micros = 1_704_164_645 * 1_000_000;
        assert_eq!(
            timestamp_to_json(TimeUnit::Microsecond, micros),
            Value::String("2024-01-02 03:04:05".into())
        );
    }

    #[test]
    fn date32_epoch_zero_is_1970_01_01() {
        assert_eq!(date32_to_json(0), Value::String("1970-01-01".into()));
    }

    #[test]
    fn time64_with_fraction_renders_micros() {
        assert_eq!(
            time64_to_json(TimeUnit::Microsecond, 3_603_000_005),
            Value::String("01:00:03.000005".into())
        );
    }

    #[test]
    fn interval_formats_all_parts() {
        assert_eq!(
            format_interval(14, 3, 3_600_000_000_000),
            "1 years 2 mons 3 days 01:00:00"
        );
        assert_eq!(format_interval(0, 0, 0), "00:00:00");
    }

    #[test]
    fn hex_bytes_pads_single_digits() {
        assert_eq!(hex_bytes(&[0x00, 0x01, 0xff]), "0001ff");
    }
}
