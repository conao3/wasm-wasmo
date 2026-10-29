//! Helpers shared by the API Gateway control planes: ids, errors, JSON Patch
//! (`patchOperations`), and paging.

use serde_json::{Map, Value, json};

use crate::codec::AwsError;
use crate::service::{Ctx, InputExt};

pub type Obj = Map<String, Value>;

pub fn gen_id(ctx: &mut Ctx<'_>, len: usize) -> String {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut b = vec![0u8; len];
    ctx.random_bytes(&mut b);
    b.iter()
        .map(|x| ALPHABET[*x as usize % ALPHABET.len()] as char)
        .collect()
}

pub fn not_found(message: impl Into<String>) -> AwsError {
    AwsError::new(404, "NotFoundException", message)
}

pub fn conflict(message: impl Into<String>) -> AwsError {
    AwsError::new(409, "ConflictException", message)
}

pub fn bad_request(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "BadRequestException", message)
}

pub fn secs(ctx: &Ctx<'_>) -> Value {
    json!((ctx.now_ms() / 1000.0).floor())
}

/// Copies the members of `input` named in `keys` that are present.
pub fn pick(input: &Value, keys: &[&str]) -> Obj {
    let mut out = Map::new();
    for k in keys {
        if let Some(v) = input.get(*k).filter(|v| !v.is_null()) {
            out.insert(k.to_string(), v.clone());
        }
    }
    out
}

fn unescape(seg: &str) -> String {
    seg.replace("~1", "/").replace("~0", "~")
}

fn coerce(existing: Option<&Value>, raw: Option<&Value>) -> Value {
    let Some(raw) = raw else {
        return Value::Null;
    };
    let Some(s) = raw.as_str() else {
        return raw.clone();
    };
    match existing {
        Some(Value::Bool(_)) => json!(s.eq_ignore_ascii_case("true")),
        Some(Value::Number(_)) => s
            .parse::<i64>()
            .map(|n| json!(n))
            .or_else(|_| s.parse::<f64>().map(|f| json!(f)))
            .unwrap_or_else(|_| json!(s)),
        _ if s == "true" || s == "false" => json!(s == "true"),
        _ => json!(s),
    }
}

/// Applies API Gateway `patchOperations` (`op`, `path`, `value`, `from`) to a
/// stored object. Paths address nested members (`/variables/name`); a value
/// for a boolean or numeric member is converted from its string form.
pub fn patch(obj: &mut Obj, input: &Value) -> Result<(), AwsError> {
    for op in input.array_field("patchOperations") {
        let kind = op.str_field("op").unwrap_or("replace");
        let path = op.str_field("path").unwrap_or_default();
        let segs: Vec<String> = path
            .trim_start_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .map(unescape)
            .collect();
        let Some((last, parents)) = segs.split_last() else {
            return Err(bad_request(format!("Invalid patch path {path}")));
        };
        let mut target = &mut *obj;
        for p in parents {
            let next = target
                .entry(p.clone())
                .or_insert_with(|| Value::Object(Map::new()));
            if !next.is_object() {
                *next = Value::Object(Map::new());
            }
            target = next.as_object_mut().expect("made an object");
        }
        match kind {
            "remove" => {
                if let Some(Value::Array(a)) = target.get_mut(last.as_str())
                    && let Some(v) = op.get("value").and_then(Value::as_str)
                {
                    a.retain(|x| x.as_str() != Some(v));
                } else {
                    target.remove(last.as_str());
                }
            }
            "add" if matches!(target.get(last.as_str()), Some(Value::Array(_))) => {
                if let (Some(Value::Array(a)), Some(v)) =
                    (target.get_mut(last.as_str()), op.get("value"))
                {
                    a.push(v.clone());
                }
            }
            "add" | "replace" => {
                let value = coerce(target.get(last.as_str()), op.get("value"));
                target.insert(last.clone(), value);
            }
            "copy" | "move" => {
                let from = op
                    .str_field("from")
                    .unwrap_or_default()
                    .trim_start_matches('/')
                    .to_string();
                let v = if kind == "move" {
                    obj.remove(&from)
                } else {
                    obj.get(&from).cloned()
                };
                if let Some(v) = v {
                    obj.insert(last.clone(), v);
                }
                return Ok(());
            }
            other => {
                return Err(bad_request(format!(
                    "Invalid patch operation specified. Must be one of: [add, remove, move, test, replace, copy], got {other}"
                )));
            }
        }
    }
    Ok(())
}

/// `position` / `limit` paging over `items`, as `{item: [...], position}`.
pub fn page(items: Vec<Value>, input: &Value, key: &str, token: &str) -> Value {
    let start = input
        .str_field(token)
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let limit = match input
        .i64_field("limit")
        .or_else(|| input.i64_field("MaxResults"))
    {
        Some(n) if n > 0 => n as usize,
        _ => 500,
    };
    let end = (start + limit).min(items.len());
    let mut out = json!({ key: items[start..end] });
    if end < items.len() {
        out[token] = json!(end.to_string());
    }
    out
}
