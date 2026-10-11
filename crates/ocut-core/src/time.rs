//! Time helpers shared by the sources.

use chrono::{DateTime, Datelike, TimeZone, Utc};
use serde_json::Value;

/// Normalises a `resets_at` value that may be epoch seconds, epoch
/// milliseconds, or an RFC 3339 / ISO 8601 string. `null` or anything
/// unparseable yields `None` ("no window open"). Years outside
/// 1970..=9999 are rejected: they cannot be real readings, and RFC 3339
/// (and the snapshot schema's timestamp pattern) has four-digit years.
pub fn parse_timestamp(v: &Value) -> Option<DateTime<Utc>> {
    parse_timestamp_any(v).filter(|t| (1970..=9999).contains(&t.year()))
}

fn parse_timestamp_any(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::Number(n) => {
            let x = n.as_f64()?;
            if !x.is_finite() || x < 0.0 {
                return None;
            }
            // Heuristic: values beyond year ~5138 in seconds are milliseconds.
            let ms = if x >= 1e11 { x } else { x * 1000.0 };
            Utc.timestamp_millis_opt(ms as i64).single()
        }
        Value::String(s) => parse_timestamp_str(s),
        _ => None,
    }
}

/// Parses an RFC 3339 string, also accepting a missing offset (assumed UTC)
/// and a purely numeric epoch string.
pub fn parse_timestamp_str(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(n) = s.parse::<f64>() {
        return parse_timestamp(&serde_json::json!(n));
    }
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|n| n.and_utc())
}

/// Whole seconds from `now` until `at`, never negative.
pub fn seconds_until(now: DateTime<Utc>, at: DateTime<Utc>) -> i64 {
    (at - now).num_seconds().max(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_all_shapes() {
        let want = Utc.with_ymd_and_hms(2026, 10, 11, 12, 0, 0).unwrap();
        assert_eq!(parse_timestamp(&json!(want.timestamp())), Some(want));
        assert_eq!(parse_timestamp(&json!(want.timestamp_millis())), Some(want));
        assert_eq!(parse_timestamp(&json!("2026-10-11T12:00:00Z")), Some(want));
        assert_eq!(
            parse_timestamp(&json!("2026-10-11T14:00:00+02:00")),
            Some(want)
        );
        assert_eq!(
            parse_timestamp(&json!("2026-10-11T12:00:00.000")),
            Some(want)
        );
        assert_eq!(parse_timestamp(&json!("1791720000")), Some(want));
        assert_eq!(parse_timestamp(&Value::Null), None);
        assert_eq!(parse_timestamp(&json!("soon")), None);
        assert_eq!(parse_timestamp(&json!(-5)), None);
        assert_eq!(parse_timestamp(&json!(1e15)), None, "year 33658");
        assert_eq!(parse_timestamp(&json!("+12026-10-11T12:00:00Z")), None);
    }
}
