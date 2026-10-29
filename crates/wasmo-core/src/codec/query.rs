//! `query` (awsQuery) and `ec2` protocols: form-encoded requests, XML responses.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::AwsError;
use super::scalar::parse_text;
use super::xml_value::{write_structure_members, write_value};
use crate::model::{MemberRef, Operation, Registry, Shape};
use crate::xml::Element;

const DEFAULT_TS: &str = "iso8601";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Query,
    Ec2,
}

struct Form {
    entries: BTreeMap<String, String>,
}

impl Form {
    fn get(&self, key: &str) -> Option<&str> {
        self.entries.get(key).map(String::as_str)
    }

    fn has_prefix(&self, prefix: &str) -> bool {
        if self.entries.contains_key(prefix) {
            return true;
        }
        let dotted = format!("{prefix}.");
        self.entries
            .range(dotted.clone()..)
            .next()
            .is_some_and(|(k, _)| k.starts_with(&dotted))
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn member_key(dialect: Dialect, name: &str, member: &MemberRef) -> String {
    match dialect {
        Dialect::Query => member
            .location_name
            .clone()
            .unwrap_or_else(|| name.to_string()),
        Dialect::Ec2 => member
            .query_name
            .clone()
            .unwrap_or_else(|| capitalize(member.location_name.as_deref().unwrap_or(name))),
    }
}

pub fn parse_input(
    form: &[(String, String)],
    reg: &Registry,
    shape: Option<&Shape>,
    dialect: Dialect,
) -> Value {
    let form = Form {
        entries: form.iter().cloned().collect(),
    };
    match shape {
        Some(shape) => parse_structure(&form, reg, "", shape, dialect),
        None => Value::Object(Map::new()),
    }
}

fn parse_structure(
    form: &Form,
    reg: &Registry,
    prefix: &str,
    shape: &Shape,
    dialect: Dialect,
) -> Value {
    let mut out = Map::new();
    for (name, member) in &shape.members {
        let key = join(prefix, &member_key(dialect, name, member));
        if let Some(v) = parse_member(
            form,
            reg,
            &key,
            Some(member),
            reg.member_shape(member),
            dialect,
        ) {
            out.insert(name.clone(), v);
        }
    }
    Value::Object(out)
}

fn parse_member(
    form: &Form,
    reg: &Registry,
    key: &str,
    member: Option<&MemberRef>,
    shape: &Shape,
    dialect: Dialect,
) -> Option<Value> {
    match shape.kind.as_str() {
        "structure" => form
            .has_prefix(key)
            .then(|| parse_structure(form, reg, key, shape, dialect)),
        "list" => {
            let item_ref = shape.member.as_deref()?;
            let item_shape = reg.member_shape(item_ref);
            let flattened = shape.flattened || member.is_some_and(|m| m.flattened);
            let base = match dialect {
                Dialect::Ec2 => key.to_string(),
                Dialect::Query if flattened => key.to_string(),
                Dialect::Query => join(key, item_ref.location_name.as_deref().unwrap_or("member")),
            };
            let mut items = Vec::new();
            for i in 1.. {
                let item_key = format!("{base}.{i}");
                if !form.has_prefix(&item_key) {
                    break;
                }
                items.push(
                    parse_member(form, reg, &item_key, Some(item_ref), item_shape, dialect)
                        .unwrap_or(Value::Null),
                );
            }
            if items.is_empty() && form.get(key).is_none() {
                return None;
            }
            Some(Value::Array(items))
        }
        "map" => {
            let key_ref = shape.key.as_deref()?;
            let value_ref = shape.value.as_deref()?;
            let flattened = shape.flattened || member.is_some_and(|m| m.flattened);
            let base = if flattened {
                key.to_string()
            } else {
                join(key, "entry")
            };
            let key_name = key_ref.location_name.as_deref().unwrap_or("key");
            let value_name = value_ref.location_name.as_deref().unwrap_or("value");
            let mut map = Map::new();
            for i in 1.. {
                let entry = format!("{base}.{i}");
                if !form.has_prefix(&entry) {
                    break;
                }
                let k = form
                    .get(&join(&entry, key_name))
                    .unwrap_or_default()
                    .to_string();
                let v = parse_member(
                    form,
                    reg,
                    &join(&entry, value_name),
                    Some(value_ref),
                    reg.member_shape(value_ref),
                    dialect,
                );
                map.insert(k, v.unwrap_or(Value::Null));
            }
            if map.is_empty() && form.get(key).is_none() {
                return None;
            }
            Some(Value::Object(map))
        }
        _ => parse_text(form.get(key)?, shape, member, DEFAULT_TS),
    }
}

pub fn serialize_output(
    reg: &Registry,
    op: &Operation,
    result: &Value,
    xml_namespace: Option<&str>,
    request_id: &str,
    dialect: Dialect,
) -> Element {
    let mut root = Element::new(format!("{}Response", op.name));
    if let Some(ns) = xml_namespace {
        root.attrs.push(("xmlns".to_string(), ns.to_string()));
    }
    let output_shape = op.output.as_ref().map(|o| reg.shape(&o.shape));
    match dialect {
        Dialect::Ec2 => {
            root.children
                .push(Element::with_text("requestId", request_id));
            if let Some(shape) = output_shape {
                write_structure_members(reg, &mut root, result, shape, DEFAULT_TS);
            }
        }
        Dialect::Query => {
            if let (Some(shape), Some(out)) = (output_shape, op.output.as_ref()) {
                let wrapper = out
                    .result_wrapper
                    .clone()
                    .unwrap_or_else(|| format!("{}Result", op.name));
                let mut result_el = Element::new(wrapper);
                write_structure_members(reg, &mut result_el, result, shape, DEFAULT_TS);
                root.children.push(result_el);
            }
            let mut meta = Element::new("ResponseMetadata");
            meta.children
                .push(Element::with_text("RequestId", request_id));
            root.children.push(meta);
        }
    }
    root
}

pub fn serialize_error(
    reg: &Registry,
    error: &AwsError,
    shape: Option<&Shape>,
    request_id: &str,
    dialect: Dialect,
) -> Element {
    let mut err = Element::new("Error");
    if dialect == Dialect::Query {
        err.children.push(Element::with_text(
            "Type",
            if error.sender_fault {
                "Sender"
            } else {
                "Receiver"
            },
        ));
    }
    err.children
        .push(Element::with_text("Code", error.code.clone()));
    if !error.message.is_empty() {
        err.children
            .push(Element::with_text("Message", error.message.clone()));
    }
    if let Some(shape) = shape {
        for (name, member) in &shape.members {
            if name.eq_ignore_ascii_case("message") {
                continue;
            }
            if let Some(v) = error.data.get(name) {
                let member_shape = reg.member_shape(member);
                let el_name = member.location_name.as_deref().unwrap_or(name);
                write_value(
                    reg,
                    &mut err,
                    el_name,
                    v,
                    Some(member),
                    member_shape,
                    DEFAULT_TS,
                );
            }
        }
    }
    match dialect {
        Dialect::Query => {
            let mut root = Element::new("ErrorResponse");
            root.children.push(err);
            root.children
                .push(Element::with_text("RequestId", request_id));
            root
        }
        Dialect::Ec2 => {
            let mut root = Element::new("Response");
            let mut errors = Element::new("Errors");
            errors.children.push(err);
            root.children.push(errors);
            root.children
                .push(Element::with_text("RequestID", request_id));
            root
        }
    }
}

/// Error code to put on the wire for an error shape.
pub fn error_code(shape_name: &str, shape: &Shape) -> String {
    shape
        .error
        .as_ref()
        .and_then(|e| e.code.clone())
        .unwrap_or_else(|| shape_name.to_string())
}
