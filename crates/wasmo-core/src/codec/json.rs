//! `json` (awsJson 1.0 / 1.1) protocol and the JSON document mapping shared
//! with `rest-json`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};

use super::AwsError;
use super::scalar::{parse_float, timestamp_format};
use super::time;
use crate::model::{MemberRef, Registry, Shape};

const DEFAULT_TS: &str = "unixTimestamp";

fn wire_name<'a>(name: &'a str, member: &'a MemberRef) -> &'a str {
    member.location_name.as_deref().unwrap_or(name)
}

/// Converts a wire JSON value into botocore parameters for `shape`.
pub fn from_wire(reg: &Registry, v: &Value, shape: &Shape, member: Option<&MemberRef>) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    match shape.kind.as_str() {
        "structure" if shape.document => v.clone(),
        "structure" => {
            let Some(obj) = v.as_object() else {
                return v.clone();
            };
            let mut out = Map::new();
            for (name, m) in &shape.members {
                if let Some(x) = obj.get(wire_name(name, m)) {
                    if x.is_null() {
                        continue;
                    }
                    out.insert(
                        name.clone(),
                        from_wire(reg, x, reg.member_shape(m), Some(m)),
                    );
                }
            }
            Value::Object(out)
        }
        "list" => {
            let item = shape.member.as_deref().expect("list member");
            let item_shape = reg.member_shape(item);
            Value::Array(
                v.as_array()
                    .map(|a| {
                        a.iter()
                            .map(|x| from_wire(reg, x, item_shape, Some(item)))
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        }
        "map" => {
            let value_ref = shape.value.as_deref().expect("map value");
            let value_shape = reg.member_shape(value_ref);
            Value::Object(
                v.as_object()
                    .map(|o| {
                        o.iter()
                            .map(|(k, x)| {
                                (k.clone(), from_wire(reg, x, value_shape, Some(value_ref)))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        }
        "timestamp" => match v {
            Value::String(s) => time::parse_any(s, timestamp_format(member, shape, DEFAULT_TS))
                .map_or(v.clone(), time::epoch_value),
            Value::Number(n) => n.as_f64().map_or(v.clone(), time::epoch_value),
            _ => v.clone(),
        },
        "blob" => match v.as_str() {
            Some(s) => STANDARD.decode(s).map_or(v.clone(), |b| {
                Value::String(String::from_utf8_lossy(&b).into_owned())
            }),
            None => v.clone(),
        },
        "float" | "double" => match v {
            Value::String(s) => parse_float(s).unwrap_or_else(|| v.clone()),
            _ => v.clone(),
        },
        _ => v.clone(),
    }
}

/// Converts botocore parameters for `shape` into wire JSON.
pub fn to_wire(reg: &Registry, v: &Value, shape: &Shape, member: Option<&MemberRef>) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    match shape.kind.as_str() {
        "structure" if shape.document => v.clone(),
        "structure" => {
            let Some(obj) = v.as_object() else {
                return v.clone();
            };
            let mut out = Map::new();
            for (name, m) in &shape.members {
                if m.location.is_some() {
                    continue;
                }
                if let Some(x) = obj.get(name) {
                    if x.is_null() {
                        continue;
                    }
                    out.insert(
                        wire_name(name, m).to_string(),
                        to_wire(reg, x, reg.member_shape(m), Some(m)),
                    );
                }
            }
            Value::Object(out)
        }
        "list" => {
            let item = shape.member.as_deref().expect("list member");
            let item_shape = reg.member_shape(item);
            Value::Array(
                v.as_array()
                    .map(|a| {
                        a.iter()
                            .map(|x| to_wire(reg, x, item_shape, Some(item)))
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        }
        "map" => {
            let value_ref = shape.value.as_deref().expect("map value");
            let value_shape = reg.member_shape(value_ref);
            Value::Object(
                v.as_object()
                    .map(|o| {
                        o.iter()
                            .map(|(k, x)| {
                                (k.clone(), to_wire(reg, x, value_shape, Some(value_ref)))
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            )
        }
        "timestamp" => {
            let epoch = time::value_epoch(v).unwrap_or_default();
            match timestamp_format(member, shape, DEFAULT_TS) {
                "iso8601" => Value::String(time::format_iso8601(epoch)),
                "rfc822" => Value::String(time::format_http_date(epoch)),
                _ => time::epoch_value(epoch),
            }
        }
        "blob" => Value::String(STANDARD.encode(v.as_str().map(str::as_bytes).unwrap_or_default())),
        _ => v.clone(),
    }
}

pub fn parse_body(body: &[u8]) -> Result<Value, AwsError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_slice(body)
        .map_err(|e| AwsError::new(400, "SerializationException", e.to_string()))
}

pub fn parse_input(reg: &Registry, body: &[u8], shape: Option<&Shape>) -> Result<Value, AwsError> {
    let v = parse_body(body)?;
    Ok(shape.map_or_else(
        || Value::Object(Map::new()),
        |s| from_wire(reg, &v, s, None),
    ))
}

pub fn serialize_output(reg: &Registry, result: &Value, shape: Option<&Shape>) -> Vec<u8> {
    let v = shape.map_or_else(
        || Value::Object(Map::new()),
        |s| to_wire(reg, result, s, None),
    );
    serde_json::to_vec(&v).expect("json serializes")
}

/// Body of an awsJson / restJson error: `__type`, `message`, and the
/// modeled members.
pub fn error_body(reg: &Registry, error: &AwsError, shape: Option<&Shape>) -> Value {
    let mut out = Map::new();
    out.insert("__type".to_string(), Value::String(error.code.clone()));
    if !error.message.is_empty() {
        let key = shape
            .and_then(|s| {
                s.members
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case("message"))
            })
            .map_or("message", |(n, m)| wire_name(n, m));
        out.insert(key.to_string(), Value::String(error.message.clone()));
    }
    if let Some(Value::Object(members)) =
        shape.map(|s| to_wire(reg, &Value::Object((*error.data).clone()), s, None))
    {
        for (k, v) in members {
            if !k.eq_ignore_ascii_case("message") {
                out.insert(k, v);
            }
        }
    }
    Value::Object(out)
}
