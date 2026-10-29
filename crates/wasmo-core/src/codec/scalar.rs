use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;

use super::time;
use crate::model::{MemberRef, Shape};

pub fn timestamp_format<'a>(
    member: Option<&'a MemberRef>,
    shape: &'a Shape,
    default: &'a str,
) -> &'a str {
    member
        .and_then(|m| m.timestamp_format.as_deref())
        .or(shape.timestamp_format.as_deref())
        .unwrap_or(default)
}

/// Converts a wire string into the parameter value botocore expects for
/// `shape`. Returns `None` when the text does not fit the type.
pub fn parse_text(
    text: &str,
    shape: &Shape,
    member: Option<&MemberRef>,
    default_ts: &str,
) -> Option<Value> {
    Some(match shape.kind.as_str() {
        "string" => Value::String(text.to_string()),
        "boolean" => Value::Bool(match text {
            "true" => true,
            "false" => false,
            _ => return None,
        }),
        "integer" | "long" | "short" | "byte" => Value::from(text.trim().parse::<i64>().ok()?),
        "float" | "double" => parse_float(text.trim())?,
        "timestamp" => time::epoch_value(time::parse_any(
            text.trim(),
            timestamp_format(member, shape, default_ts),
        )?),
        "blob" => {
            STANDARD.decode(text.trim()).ok()?;
            Value::String(text.trim().to_string())
        }
        _ => Value::String(text.to_string()),
    })
}

pub fn parse_float(text: &str) -> Option<Value> {
    match text {
        "NaN" | "Infinity" | "-Infinity" => Some(Value::String(text.to_string())),
        _ => Some(Value::from(text.parse::<f64>().ok()?)),
    }
}

/// Renders a parameter value as wire text for `shape`.
pub fn format_text(
    value: &Value,
    shape: &Shape,
    member: Option<&MemberRef>,
    default_ts: &str,
) -> String {
    match shape.kind.as_str() {
        "timestamp" => {
            let epoch = time::value_epoch(value).unwrap_or_default();
            match timestamp_format(member, shape, default_ts) {
                "unixTimestamp" => time::format_epoch(epoch),
                "rfc822" => time::format_http_date(epoch),
                _ => time::format_iso8601(epoch),
            }
        }
        "blob" => value.as_str().unwrap_or_default().to_string(),
        _ => match value {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => format_number(n),
            Value::Null => String::new(),
            other => other.to_string(),
        },
    }
}

pub fn format_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or_default();
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{:.1}", f)
    } else {
        f.to_string()
    }
}
