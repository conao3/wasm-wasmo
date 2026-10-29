use serde_json::{Map, Value};

use crate::service::InputExt;

/// Resolves a JSONPath of the form `$`, `$.a.b`, `$.a[0]`, or `$['a']`.
pub fn path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut rest = path.strip_prefix('$')?;
    let mut cur = root;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            let end = r.find(['.', '[']).unwrap_or(r.len());
            cur = cur.get(&r[..end])?;
            rest = &r[end..];
        } else {
            let r = rest.strip_prefix('[')?;
            let end = r.find(']')?;
            let key = &r[..end];
            cur = match key.trim_matches(['\'', '"']) {
                k if k.len() != key.len() => cur.get(k)?,
                k => cur.get(k.parse::<usize>().ok()?)?,
            };
            rest = &r[end + 1..];
        }
    }
    Some(cur)
}

/// Renders the payload a target receives for `event`, or `None` when the
/// target's `InputPath` selects nothing.
pub fn target_input(
    event: &Value,
    target: &Value,
    rule_arn: &str,
    rule_name: &str,
) -> Option<String> {
    if let Some(input) = target.get("Input").and_then(Value::as_str) {
        return Some(input.to_string());
    }
    if let Some(p) = target.str_field("InputPath") {
        return path(event, p).map(Value::to_string);
    }
    let Some(t) = target.obj_field("InputTransformer") else {
        return Some(event.to_string());
    };
    let template = t
        .get("InputTemplate")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut vars: Map<String, Value> = t
        .get("InputPathsMap")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(k, p)| {
            let v = p
                .as_str()
                .and_then(|p| path(event, p))
                .cloned()
                .unwrap_or(Value::Null);
            (k.clone(), v)
        })
        .collect();
    vars.insert("aws.events.rule-arn".into(), Value::String(rule_arn.into()));
    vars.insert(
        "aws.events.rule-name".into(),
        Value::String(rule_name.into()),
    );
    vars.insert(
        "aws.events.event.ingestion-time".into(),
        event.get("time").cloned().unwrap_or(Value::Null),
    );
    vars.insert("aws.events.event".into(), event.clone());
    vars.insert("aws.events.event.json".into(), event.clone());
    Some(render(template, &vars))
}

fn render(template: &str, vars: &Map<String, Value>) -> String {
    let json_template = matches!(template.trim_start().as_bytes().first(), Some(b'{' | b'['));
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('>').map(|end| (&after[..end], end)) {
            Some((name, end)) if vars.contains_key(name) => {
                let quoted = out.ends_with('"') && after[end + 1..].starts_with('"');
                out.push_str(&substitute(&vars[name], json_template, quoted));
                rest = &after[end + 1..];
            }
            _ => {
                out.push('<');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn substitute(v: &Value, json_template: bool, quoted: bool) -> String {
    match v {
        Value::String(s) if json_template && quoted => {
            let encoded = Value::String(s.clone()).to_string();
            encoded[1..encoded.len() - 1].to_string()
        }
        Value::String(s) if json_template => Value::String(s.clone()).to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event() -> Value {
        json!({"id": "e1", "source": "app", "time": "2026-01-01T00:00:00Z", "detail": {"order": {"id": 7, "items": ["a", "b"]}, "note": "say \"hi\""}})
    }

    #[test]
    fn json_paths() {
        let e = event();
        assert_eq!(path(&e, "$.detail.order.id"), Some(&json!(7)));
        assert_eq!(path(&e, "$.detail.order.items[1]"), Some(&json!("b")));
        assert_eq!(path(&e, "$['source']"), Some(&json!("app")));
        assert_eq!(path(&e, "$"), Some(&e));
        assert_eq!(path(&e, "$.missing.x"), None);
    }

    #[test]
    fn input_selection() {
        let e = event();
        assert_eq!(
            target_input(&e, &json!({"Input": "const"}), "", "").unwrap(),
            "const"
        );
        assert_eq!(
            target_input(&e, &json!({"InputPath": "$.detail.order"}), "", "").unwrap(),
            r#"{"id":7,"items":["a","b"]}"#
        );
        assert_eq!(target_input(&e, &json!({}), "", "").unwrap(), e.to_string());
    }

    #[test]
    fn input_transformer() {
        let e = event();
        let t = json!({"InputTransformer": {
            "InputPathsMap": {"id": "$.detail.order.id", "src": "$.source", "note": "$.detail.note", "items": "$.detail.order.items"},
            "InputTemplate": r#"{"order": <id>, "source": "<src>", "note": "<note>", "items": <items>, "rule": "<aws.events.rule-name>"}"#,
        }});
        let out: Value =
            serde_json::from_str(&target_input(&e, &t, "arn:r", "r1").unwrap()).unwrap();
        assert_eq!(
            out,
            json!({"order": 7, "source": "app", "note": "say \"hi\"", "items": ["a", "b"], "rule": "r1"})
        );
        let plain = json!({"InputTransformer": {
            "InputPathsMap": {"src": "$.source"},
            "InputTemplate": "Event from <src> <unknown>",
        }});
        assert_eq!(
            target_input(&e, &plain, "", "").unwrap(),
            "Event from app <unknown>"
        );
    }
}
