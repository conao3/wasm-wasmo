//! Event pattern matching shared by EventBridge rules and SNS filter policies.

use serde_json::{Map, Value};

pub fn parse(pattern: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(pattern)
        .map_err(|e| format!("Event pattern is not valid JSON: {e}"))?;
    validate(&v)?;
    Ok(v)
}

fn validate(p: &Value) -> Result<(), String> {
    let Value::Object(m) = p else {
        return Err("Event pattern must be a JSON object".into());
    };
    for (k, v) in m {
        match v {
            Value::Array(alts) if k == "$or" => {
                if alts.len() < 2 {
                    return Err("$or must contain at least two patterns".into());
                }
                alts.iter().try_for_each(validate)?;
            }
            Value::Object(_) => validate(v)?,
            Value::Array(items) if items.is_empty() => {
                return Err(format!("Empty arrays are not allowed for \"{k}\""));
            }
            Value::Array(items) => items.iter().try_for_each(validate_matcher)?,
            _ => return Err(format!("\"{k}\" must be an object or an array")),
        }
    }
    Ok(())
}

fn validate_matcher(m: &Value) -> Result<(), String> {
    let Value::Object(o) = m else {
        return match m {
            Value::Array(_) => Err("Match value must be a string, number, boolean or null".into()),
            _ => Ok(()),
        };
    };
    let mut ops = o.iter();
    let (Some((op, arg)), None) = (ops.next(), ops.next()) else {
        return Err("Match expressions must contain exactly one operator".into());
    };
    let ok = match op.as_str() {
        "prefix" | "suffix" => arg.is_string() || ignore_case(arg).is_some(),
        "equals-ignore-case" | "wildcard" => arg.is_string(),
        "anything-but" => match arg {
            Value::String(_) | Value::Number(_) => true,
            Value::Array(items) => {
                !items.is_empty() && items.iter().all(|i| i.is_string() || i.is_number())
            }
            Value::Object(inner) => {
                inner.len() == 1
                    && inner.iter().all(|(k, v)| {
                        matches!(
                            k.as_str(),
                            "prefix" | "suffix" | "equals-ignore-case" | "wildcard"
                        ) && (v.is_string()
                            || v.as_array().is_some_and(|a| a.iter().all(Value::is_string)))
                    })
            }
            _ => false,
        },
        "numeric" => arg.as_array().is_some_and(|a| {
            matches!(a.len(), 2 | 4)
                && a.chunks(2).all(|c| {
                    c[0].as_str()
                        .is_some_and(|o| matches!(o, "=" | "!=" | "<" | "<=" | ">" | ">="))
                        && c[1].is_number()
                })
        }),
        "exists" => arg.is_boolean(),
        "cidr" => arg.as_str().and_then(parse_cidr).is_some(),
        other => return Err(format!("Unrecognized match type {other}")),
    };
    if ok {
        Ok(())
    } else {
        Err(format!("Invalid value for the {op} match expression"))
    }
}

fn ignore_case(arg: &Value) -> Option<&str> {
    arg.as_object()
        .filter(|o| o.len() == 1)?
        .get("equals-ignore-case")?
        .as_str()
}

pub fn matches(pattern: &Value, event: &Value) -> bool {
    let Some(p) = pattern.as_object() else {
        return false;
    };
    p.iter().all(|(k, rule)| {
        if k == "$or" {
            return rule
                .as_array()
                .is_some_and(|alts| alts.iter().any(|a| matches(a, event)));
        }
        let field = event.get(k);
        match rule {
            Value::Object(_) => match field {
                Some(v @ Value::Object(_)) => matches(rule, v),
                Some(Value::Array(items)) => {
                    items.iter().any(|i| i.is_object() && matches(rule, i))
                }
                _ => matches(rule, &Value::Object(Map::new())),
            },
            Value::Array(ms) => ms.iter().any(|m| matcher(m, field)),
            _ => false,
        }
    })
}

fn matcher(m: &Value, field: Option<&Value>) -> bool {
    if let Some(exists) = m.get("exists").and_then(Value::as_bool) {
        return field.is_some() == exists;
    }
    match field {
        None => false,
        Some(Value::Array(items)) => items.iter().any(|v| leaf(m, v)),
        Some(v) => leaf(m, v),
    }
}

fn equal(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) if a.is_number() && b.is_number() => x == y,
        _ => a == b,
    }
}

fn text_op(op: &str, arg: &Value, v: &str) -> bool {
    let (want, fold) = match ignore_case(arg) {
        Some(s) => (s.to_string(), true),
        None => (arg.as_str().unwrap_or_default().to_string(), false),
    };
    let (v, want) = if fold || op == "equals-ignore-case" {
        (v.to_lowercase(), want.to_lowercase())
    } else {
        (v.to_string(), want)
    };
    match op {
        "prefix" => v.starts_with(&want),
        "suffix" => v.ends_with(&want),
        "wildcard" => wildcard(&want, &v),
        _ => v == want,
    }
}

fn leaf(m: &Value, v: &Value) -> bool {
    let Some((op, arg)) = m.as_object().and_then(|o| o.iter().next()) else {
        return equal(m, v);
    };
    match op.as_str() {
        "prefix" | "suffix" | "equals-ignore-case" | "wildcard" => {
            v.as_str().is_some_and(|s| text_op(op, arg, s))
        }
        "anything-but" => match arg {
            Value::Array(items) => !items.iter().any(|i| equal(i, v)),
            Value::Object(inner) => v.as_str().is_some_and(|s| {
                !inner.iter().any(|(op, arg)| match arg {
                    Value::Array(alts) => alts.iter().any(|a| text_op(op, a, s)),
                    _ => text_op(op, arg, s),
                })
            }),
            _ => !equal(arg, v),
        },
        "numeric" => v.as_f64().filter(|_| v.is_number()).is_some_and(|n| {
            arg.as_array().is_some_and(|a| {
                a.chunks(2).all(|c| {
                    let bound = c.get(1).and_then(Value::as_f64).unwrap_or(f64::NAN);
                    match c[0].as_str().unwrap_or_default() {
                        "=" => n == bound,
                        "!=" => n != bound,
                        "<" => n < bound,
                        "<=" => n <= bound,
                        ">" => n > bound,
                        ">=" => n >= bound,
                        _ => false,
                    }
                })
            })
        }),
        "cidr" => match (
            arg.as_str().and_then(parse_cidr),
            v.as_str().and_then(parse_ipv4),
        ) {
            (Some((net, mask)), Some(ip)) => ip & mask == net & mask,
            _ => false,
        },
        _ => false,
    }
}

fn wildcard(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || s.len() < first.len() + last.len() || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    true
}

fn parse_ipv4(s: &str) -> Option<u32> {
    let octets: Vec<u8> = s
        .split('.')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    (octets.len() == 4).then(|| u32::from_be_bytes([octets[0], octets[1], octets[2], octets[3]]))
}

fn parse_cidr(s: &str) -> Option<(u32, u32)> {
    let (net, len) = s.split_once('/')?;
    let len: u32 = len.parse().ok().filter(|l| *l <= 32)?;
    let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
    Some((parse_ipv4(net)?, mask))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hit(pattern: Value, event: Value) -> bool {
        validate(&pattern).unwrap();
        matches(&pattern, &event)
    }

    #[test]
    fn literals_and_nesting() {
        let p = json!({"source": ["app"], "detail": {"state": ["ok", 3]}});
        assert!(hit(
            p.clone(),
            json!({"source": "app", "detail": {"state": "ok"}})
        ));
        assert!(hit(
            p.clone(),
            json!({"source": "app", "detail": {"state": 3.0}})
        ));
        assert!(!hit(
            p.clone(),
            json!({"source": "app", "detail": {"state": "bad"}})
        ));
        assert!(!hit(
            p,
            json!({"source": "other", "detail": {"state": "ok"}})
        ));
        assert!(hit(json!({"tags": ["a"]}), json!({"tags": ["b", "a"]})));
    }

    #[test]
    fn string_operators() {
        assert!(hit(
            json!({"k": [{"prefix": "img/"}]}),
            json!({"k": "img/1.png"})
        ));
        assert!(hit(
            json!({"k": [{"prefix": {"equals-ignore-case": "IMG/"}}]}),
            json!({"k": "img/1.png"})
        ));
        assert!(hit(
            json!({"k": [{"suffix": ".png"}]}),
            json!({"k": "img/1.png"})
        ));
        assert!(!hit(
            json!({"k": [{"suffix": ".jpg"}]}),
            json!({"k": "img/1.png"})
        ));
        assert!(hit(
            json!({"k": [{"equals-ignore-case": "OK"}]}),
            json!({"k": "ok"})
        ));
        assert!(hit(
            json!({"k": [{"wildcard": "a*c*e"}]}),
            json!({"k": "abcde"})
        ));
        assert!(!hit(
            json!({"k": [{"wildcard": "a*c*e"}]}),
            json!({"k": "abcd"})
        ));
    }

    #[test]
    fn anything_but() {
        let p = json!({"k": [{"anything-but": ["a", "b"]}]});
        assert!(hit(p.clone(), json!({"k": "c"})));
        assert!(!hit(p.clone(), json!({"k": "a"})));
        assert!(!hit(p, json!({})));
        assert!(hit(json!({"k": [{"anything-but": 5}]}), json!({"k": 6})));
        let prefix = json!({"k": [{"anything-but": {"prefix": "tmp"}}]});
        assert!(!hit(prefix.clone(), json!({"k": "tmp-1"})));
        assert!(hit(prefix, json!({"k": "prod"})));
    }

    #[test]
    fn numeric_exists_and_cidr() {
        let p = json!({"n": [{"numeric": [">", 0, "<=", 5]}]});
        assert!(hit(p.clone(), json!({"n": 5})));
        assert!(!hit(p.clone(), json!({"n": 0})));
        assert!(!hit(p, json!({"n": "3"})));
        assert!(hit(json!({"k": [{"exists": false}]}), json!({})));
        assert!(!hit(json!({"k": [{"exists": false}]}), json!({"k": 1})));
        assert!(hit(json!({"k": [{"exists": true}]}), json!({"k": 1})));
        assert!(hit(json!({"d": {"k": [{"exists": false}]}}), json!({})));
        assert!(hit(
            json!({"ip": [{"cidr": "10.0.0.0/24"}]}),
            json!({"ip": "10.0.0.7"})
        ));
        assert!(!hit(
            json!({"ip": [{"cidr": "10.0.0.0/24"}]}),
            json!({"ip": "10.0.1.7"})
        ));
    }

    #[test]
    fn or_combinations() {
        let p = json!({"$or": [{"a": [1]}, {"b": [2]}]});
        assert!(hit(p.clone(), json!({"b": 2})));
        assert!(!hit(p, json!({"a": 2})));
    }

    #[test]
    fn rejects_invalid_patterns() {
        assert!(parse("[]").is_err());
        assert!(parse(r#"{"k": "v"}"#).is_err());
        assert!(parse(r#"{"k": []}"#).is_err());
        assert!(parse(r#"{"k": [{"bogus": 1}]}"#).is_err());
        assert!(parse(r#"{"k": [{"numeric": [">", "x"]}]}"#).is_err());
        assert!(parse(r#"{"k": [{"prefix": "a", "suffix": "b"}]}"#).is_err());
        assert!(parse(r#"{"k": [{"numeric": [">=", 1, "<", 2]}]}"#).is_ok());
    }
}
