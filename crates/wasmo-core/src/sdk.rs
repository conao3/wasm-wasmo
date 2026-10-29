//! Member names as the AWS SDKs spell them in generic integrations (Step
//! Functions `aws-sdk:*`): PascalCase whatever the API model uses. Input
//! members match the model case-insensitively; output members are
//! capitalized. Map keys and documents stay as they are.

use serde_json::{Map, Value};

use crate::model::{Operation, Registry, Shape};

pub fn input(reg: &Registry, op: &Operation, v: Value) -> Value {
    match &op.input {
        Some(i) => rename(reg, reg.shape(&i.shape), v, false),
        None => v,
    }
}

pub fn output(reg: &Registry, op: &Operation, v: Value) -> Value {
    match &op.output {
        Some(o) => rename(reg, reg.shape(&o.shape), v, true),
        None => v,
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn rename(reg: &Registry, shape: &Shape, v: Value, out: bool) -> Value {
    match (shape.kind.as_str(), v) {
        ("structure", Value::Object(o)) if !shape.document => {
            let mut res = Map::new();
            for (k, x) in o {
                let member = shape
                    .members
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(&k));
                match member {
                    Some((name, m)) => {
                        let key = if out { capitalize(name) } else { name.clone() };
                        res.insert(key, rename(reg, reg.member_shape(m), x, out));
                    }
                    None => {
                        res.insert(k, x);
                    }
                }
            }
            Value::Object(res)
        }
        ("list", Value::Array(a)) => {
            let item = reg.member_shape(shape.member.as_deref().expect("list member"));
            Value::Array(a.into_iter().map(|x| rename(reg, item, x, out)).collect())
        }
        ("map", Value::Object(o)) => {
            let value = reg.member_shape(shape.value.as_deref().expect("map value"));
            Value::Object(
                o.into_iter()
                    .map(|(k, x)| (k, rename(reg, value, x, out)))
                    .collect(),
            )
        }
        (_, v) => v,
    }
}
