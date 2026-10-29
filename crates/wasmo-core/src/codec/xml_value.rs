use serde_json::{Map, Value};

use super::scalar::{format_text, parse_text};
use crate::model::{MemberRef, Registry, Shape};
use crate::xml::Element;

/// Name of the XML element that carries `member`.
pub fn member_element_name<'a>(name: &'a str, member: &'a MemberRef, _shape: &'a Shape) -> &'a str {
    member.location_name.as_deref().unwrap_or(name)
}

fn is_flattened(member: Option<&MemberRef>, shape: &Shape) -> bool {
    shape.flattened || member.is_some_and(|m| m.flattened)
}

/// Appends the XML for one value of `shape` under `parent` as element(s)
/// named `name`.
pub fn write_value(
    reg: &Registry,
    parent: &mut Element,
    name: &str,
    value: &Value,
    member: Option<&MemberRef>,
    shape: &Shape,
    default_ts: &str,
) {
    match shape.kind.as_str() {
        "structure" => {
            let mut el = Element::new(name);
            if let Some(attr) = member.and_then(|m| m.xml_namespace.attribute()) {
                el.attrs.push(attr);
            }
            write_structure_members(reg, &mut el, value, shape, default_ts);
            parent.children.push(el);
        }
        "list" => {
            let item_ref = shape.member.as_deref().expect("list shape has a member");
            let item_shape = reg.member_shape(item_ref);
            let items = value.as_array().map(Vec::as_slice).unwrap_or_default();
            if is_flattened(member, shape) {
                for item in items {
                    write_value(
                        reg,
                        parent,
                        name,
                        item,
                        Some(item_ref),
                        item_shape,
                        default_ts,
                    );
                }
            } else {
                let mut list = Element::new(name);
                if let Some(attr) = member.and_then(|m| m.xml_namespace.attribute()) {
                    list.attrs.push(attr);
                }
                let item_name = item_ref.location_name.as_deref().unwrap_or("member");
                for item in items {
                    write_value(
                        reg,
                        &mut list,
                        item_name,
                        item,
                        Some(item_ref),
                        item_shape,
                        default_ts,
                    );
                }
                parent.children.push(list);
            }
        }
        "map" => {
            let key_ref = shape.key.as_deref().expect("map shape has a key");
            let value_ref = shape.value.as_deref().expect("map shape has a value");
            let key_name = key_ref.location_name.as_deref().unwrap_or("key");
            let value_name = value_ref.location_name.as_deref().unwrap_or("value");
            let entries = value.as_object().cloned().unwrap_or_default();
            let flattened = is_flattened(member, shape);
            let mut map_el = Element::new(name);
            if let Some(attr) = member.and_then(|m| m.xml_namespace.attribute()) {
                map_el.attrs.push(attr);
            }
            for (k, v) in &entries {
                let mut entry = Element::new(if flattened { name } else { "entry" });
                if flattened && let Some(attr) = member.and_then(|m| m.xml_namespace.attribute()) {
                    entry.attrs.push(attr);
                }
                write_value(
                    reg,
                    &mut entry,
                    key_name,
                    &Value::String(k.clone()),
                    Some(key_ref),
                    reg.member_shape(key_ref),
                    default_ts,
                );
                write_value(
                    reg,
                    &mut entry,
                    value_name,
                    v,
                    Some(value_ref),
                    reg.member_shape(value_ref),
                    default_ts,
                );
                if flattened {
                    parent.children.push(entry);
                } else {
                    map_el.children.push(entry);
                }
            }
            if !flattened {
                parent.children.push(map_el);
            }
        }
        _ => {
            let mut el = Element::with_text(name, format_text(value, shape, member, default_ts));
            if let Some(attr) = member.and_then(|m| m.xml_namespace.attribute()) {
                el.attrs.push(attr);
            }
            parent.children.push(el);
        }
    }
}

pub fn write_structure_members(
    reg: &Registry,
    el: &mut Element,
    value: &Value,
    shape: &Shape,
    default_ts: &str,
) {
    let Some(obj) = value.as_object() else { return };
    for (name, member) in &shape.members {
        let Some(v) = obj.get(name) else { continue };
        if v.is_null() {
            continue;
        }
        let member_shape = reg.member_shape(member);
        let el_name = member_element_name(name, member, member_shape);
        if member.xml_attribute {
            el.attrs.push((
                el_name.to_string(),
                format_text(v, member_shape, Some(member), default_ts),
            ));
            continue;
        }
        write_value(reg, el, el_name, v, Some(member), member_shape, default_ts);
    }
}

/// Reads a value of `shape` from the element(s) that carry it.
pub fn read_value(
    reg: &Registry,
    nodes: &[&Element],
    member: Option<&MemberRef>,
    shape: &Shape,
    default_ts: &str,
) -> Option<Value> {
    match shape.kind.as_str() {
        "list" => {
            let item_ref = shape.member.as_deref()?;
            let item_shape = reg.member_shape(item_ref);
            let items: Vec<&Element> = if is_flattened(member, shape) {
                nodes.to_vec()
            } else {
                let item_name = item_ref.location_name.as_deref().unwrap_or("member");
                nodes.first()?.children_named(item_name).collect()
            };
            Some(Value::Array(
                items
                    .iter()
                    .filter_map(|n| read_value(reg, &[n], Some(item_ref), item_shape, default_ts))
                    .collect(),
            ))
        }
        "map" => {
            let key_ref = shape.key.as_deref()?;
            let value_ref = shape.value.as_deref()?;
            let key_name = key_ref.location_name.as_deref().unwrap_or("key");
            let value_name = value_ref.location_name.as_deref().unwrap_or("value");
            let entries: Vec<&Element> = if is_flattened(member, shape) {
                nodes.to_vec()
            } else {
                nodes.first()?.children_named("entry").collect()
            };
            let mut map = Map::new();
            for e in entries {
                let k = e
                    .child(key_name)
                    .map(|k| k.text.clone())
                    .unwrap_or_default();
                let v = e.child(value_name).and_then(|v| {
                    read_value(
                        reg,
                        &[v],
                        Some(value_ref),
                        reg.member_shape(value_ref),
                        default_ts,
                    )
                });
                map.insert(k, v.unwrap_or(Value::Null));
            }
            Some(Value::Object(map))
        }
        "structure" => Some(read_structure(reg, nodes.first()?, shape, default_ts)),
        _ => parse_text(&nodes.first()?.text, shape, member, default_ts),
    }
}

pub fn read_structure(reg: &Registry, el: &Element, shape: &Shape, default_ts: &str) -> Value {
    let mut out = Map::new();
    for (name, member) in &shape.members {
        let member_shape = reg.member_shape(member);
        let el_name = member_element_name(name, member, member_shape);
        if member.xml_attribute {
            if let Some(v) = el
                .attr(el_name)
                .and_then(|t| parse_text(t, member_shape, Some(member), default_ts))
            {
                out.insert(name.clone(), v);
            }
            continue;
        }
        let nodes: Vec<&Element> = el.children_named(el_name).collect();
        if nodes.is_empty() {
            continue;
        }
        if let Some(v) = read_value(reg, &nodes, Some(member), member_shape, default_ts) {
            out.insert(name.clone(), v);
        }
    }
    Value::Object(out)
}
