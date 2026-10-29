//! HTTP bindings shared by `rest-json` and `rest-xml`: URI labels, query
//! strings, headers, prefix headers, payloads, and status codes.

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};

use super::scalar::{format_text, parse_text};
use super::xml_value::{read_structure, write_structure_members};
use super::{AwsError, json};
use crate::model::{MemberRef, Operation, Registry, Shape};
use crate::xml::{self, Element};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BodyFormat {
    Json,
    Xml,
}

pub struct HttpRequest<'a> {
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

#[derive(Debug, Clone, Default)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub fn uri_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b'/' if keep_slash => out.push('/'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Matches `path` against a request URI template (`/a/{b}/{c+}`) and returns
/// the decoded labels.
pub fn match_uri(template: &str, path: &str) -> Option<HashMap<String, String>> {
    let template = template.split_once('?').map_or(template, |(p, _)| p);
    let tsegs: Vec<&str> = template.trim_start_matches('/').split('/').collect();
    let psegs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut labels = HashMap::new();
    let mut pi = 0;
    for (ti, t) in tsegs.iter().enumerate() {
        if let Some(name) = t.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
            if let Some(greedy) = name.strip_suffix('+') {
                let remaining_t = tsegs.len() - ti - 1;
                let end = psegs.len().checked_sub(remaining_t)?;
                if end <= pi {
                    return None;
                }
                labels.insert(
                    greedy.to_string(),
                    psegs[pi..end]
                        .iter()
                        .map(|s| uri_decode(s))
                        .collect::<Vec<_>>()
                        .join("/"),
                );
                pi = end;
            } else {
                labels.insert(name.to_string(), uri_decode(psegs.get(pi)?));
                pi += 1;
            }
        } else {
            if psegs.get(pi) != Some(t) {
                return None;
            }
            pi += 1;
        }
    }
    (pi == psegs.len()).then_some(labels)
}

pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (uri_decode(k), uri_decode(v))
        })
        .collect()
}

/// Splits a header list value on commas, honouring double-quoted items.
pub fn split_header_list(value: &str, shape: &Shape) -> Vec<String> {
    if shape.kind == "timestamp" {
        let parts: Vec<&str> = value.split(',').map(str::trim).collect();
        if parts
            .iter()
            .any(|p| matches!(*p, "Mon" | "Tue" | "Wed" | "Thu" | "Fri" | "Sat" | "Sun"))
        {
            return parts.chunks(2).map(|c| c.join(", ")).collect();
        }
        return parts.into_iter().map(str::to_string).collect();
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = value.chars().peekable();
    let mut in_quotes = false;
    let mut quoted = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if cur.trim().is_empty() && !in_quotes => {
                in_quotes = true;
                quoted = true;
                cur.clear();
            }
            '"' if in_quotes => in_quotes = false,
            '\\' if in_quotes => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            ',' if !in_quotes => {
                out.push(if quoted {
                    std::mem::take(&mut cur)
                } else {
                    std::mem::take(&mut cur).trim().to_string()
                });
                quoted = false;
            }
            c => cur.push(c),
        }
    }
    out.push(if quoted { cur } else { cur.trim().to_string() });
    out
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn header_value(text: &str, shape: &Shape, member: &MemberRef) -> Option<Value> {
    if shape.kind == "string" && member.jsonvalue {
        return Some(Value::String(
            String::from_utf8_lossy(&STANDARD.decode(text).ok()?).into_owned(),
        ));
    }
    let text = if shape.kind == "string" {
        text
    } else {
        text.trim()
    };
    parse_text(text, shape, Some(member), "rfc822")
}

pub fn parse_input(
    reg: &Registry,
    op: &Operation,
    req: &HttpRequest<'_>,
    format: BodyFormat,
) -> Result<Value, AwsError> {
    let Some(input) = op.input.as_ref() else {
        return Ok(Value::Object(Map::new()));
    };
    let shape = reg.shape(&input.shape);
    let labels = match_uri(&op.http.request_uri, req.path).unwrap_or_default();
    let query = parse_query(req.query);
    let mut out = Map::new();
    for (name, member) in &shape.members {
        let ms = reg.member_shape(member);
        let wire = member.location_name.as_deref().unwrap_or(name);
        let value = match member.location.as_deref() {
            Some("uri") => labels
                .get(wire)
                .and_then(|t| parse_text(t, ms, Some(member), "iso8601")),
            Some("querystring") => parse_query_member(reg, &query, wire, member, ms),
            Some("header") => header(req.headers, wire).and_then(|v| match ms.kind.as_str() {
                "list" => {
                    let item = ms.member.as_deref()?;
                    let item_shape = reg.member_shape(item);
                    if v.trim().is_empty() {
                        return Some(Value::Array(vec![]));
                    }
                    Some(Value::Array(
                        split_header_list(v, item_shape)
                            .iter()
                            .filter_map(|t| header_value(t, item_shape, item))
                            .collect(),
                    ))
                }
                _ => header_value(v, ms, member),
            }),
            Some("headers") => {
                let value_ref = ms.value.as_deref().expect("prefix header map value");
                let value_shape = reg.member_shape(value_ref);
                let prefix = wire.to_ascii_lowercase();
                let map: Map<String, Value> = req
                    .headers
                    .iter()
                    .filter(|(k, _)| k.to_ascii_lowercase().starts_with(&prefix))
                    .filter_map(|(k, v)| {
                        Some((
                            k[prefix.len()..].to_string(),
                            header_value(v, value_shape, value_ref)?,
                        ))
                    })
                    .collect();
                (!map.is_empty()).then_some(Value::Object(map))
            }
            _ => None,
        };
        if let Some(v) = value {
            out.insert(name.clone(), v);
        }
    }
    let body_members = parse_body(reg, shape, req.body, format)?;
    for (k, v) in body_members {
        out.insert(k, v);
    }
    Ok(Value::Object(out))
}

fn parse_query_member(
    reg: &Registry,
    query: &[(String, String)],
    wire: &str,
    member: &MemberRef,
    ms: &Shape,
) -> Option<Value> {
    match ms.kind.as_str() {
        "map" => {
            let value_ref = ms.value.as_deref()?;
            let value_shape = reg.member_shape(value_ref);
            let mut map = Map::new();
            for (k, v) in query {
                if value_shape.kind == "list" {
                    let item = value_shape.member.as_deref()?;
                    let parsed = parse_text(v, reg.member_shape(item), Some(item), "iso8601")?;
                    match map.entry(k.clone()).or_insert_with(|| Value::Array(vec![])) {
                        Value::Array(a) => a.push(parsed),
                        _ => unreachable!(),
                    }
                } else if !map.contains_key(k) {
                    map.insert(
                        k.clone(),
                        parse_text(v, value_shape, Some(value_ref), "iso8601")?,
                    );
                }
            }
            (!map.is_empty()).then_some(Value::Object(map))
        }
        "list" => {
            let item = ms.member.as_deref()?;
            let item_shape = reg.member_shape(item);
            let items: Vec<Value> = query
                .iter()
                .filter(|(k, _)| k == wire)
                .filter_map(|(_, v)| parse_text(v, item_shape, Some(item), "iso8601"))
                .collect();
            (!items.is_empty()).then_some(Value::Array(items))
        }
        _ => query
            .iter()
            .find(|(k, _)| k == wire)
            .and_then(|(_, v)| parse_text(v, ms, Some(member), "iso8601")),
    }
}

fn parse_body(
    reg: &Registry,
    shape: &Shape,
    body: &[u8],
    format: BodyFormat,
) -> Result<Map<String, Value>, AwsError> {
    let mut out = Map::new();
    if let Some(payload) = &shape.payload {
        let member = &shape.members[payload];
        let ms = reg.member_shape(member);
        match ms.kind.as_str() {
            "blob" => {
                if !body.is_empty() {
                    out.insert(payload.clone(), Value::String(STANDARD.encode(body)));
                }
            }
            "string" => {
                if !body.is_empty() {
                    out.insert(
                        payload.clone(),
                        Value::String(String::from_utf8_lossy(body).into_owned()),
                    );
                }
            }
            _ if body.iter().all(u8::is_ascii_whitespace) => {}
            _ => {
                let v = match format {
                    BodyFormat::Json => {
                        json::from_wire(reg, &json::parse_body(body)?, ms, Some(member))
                    }
                    BodyFormat::Xml => read_structure(reg, &parse_xml(body)?, ms, "iso8601"),
                };
                if v.as_object().is_none_or(|o| !o.is_empty()) {
                    out.insert(payload.clone(), v);
                }
            }
        }
        return Ok(out);
    }
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(out);
    }
    let parsed = match format {
        BodyFormat::Json => json::from_wire(reg, &json::parse_body(body)?, shape, None),
        BodyFormat::Xml => read_structure(reg, &parse_xml(body)?, shape, "iso8601"),
    };
    if let Value::Object(obj) = parsed {
        for (k, v) in obj {
            if shape.members[&k].location.is_none() {
                out.insert(k, v);
            }
        }
    }
    Ok(out)
}

fn parse_xml(body: &[u8]) -> Result<Element, AwsError> {
    xml::parse(body).map_err(|e| AwsError::new(400, "MalformedXML", e))
}

fn quote_header_item(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.starts_with(' ') || s.ends_with(' ') {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

fn format_header(v: &Value, shape: &Shape, member: &MemberRef) -> String {
    if shape.kind == "string" && member.jsonvalue {
        return STANDARD.encode(
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string()),
        );
    }
    format_text(v, shape, Some(member), "rfc822")
}

/// Root element name of an XML document carrying `shape`.
pub fn xml_root_name<'a>(shape_name: &'a str, shape: &'a Shape) -> &'a str {
    shape.location_name.as_deref().unwrap_or(shape_name)
}

pub fn serialize_output(
    reg: &Registry,
    op: &Operation,
    result: &Value,
    format: BodyFormat,
) -> HttpResponse {
    let mut res = HttpResponse {
        status: op.http.response_code.unwrap_or(200),
        ..Default::default()
    };
    let Some(output) = op.output.as_ref() else {
        return res;
    };
    let shape = reg.shape(&output.shape);
    let obj = result.as_object().cloned().unwrap_or_default();
    for (name, member) in &shape.members {
        let Some(v) = obj.get(name).filter(|v| !v.is_null()) else {
            continue;
        };
        let ms = reg.member_shape(member);
        let wire = member.location_name.as_deref().unwrap_or(name);
        match member.location.as_deref() {
            Some("statusCode") => res.status = v.as_u64().unwrap_or(200) as u16,
            Some("header") => {
                let text = match ms.kind.as_str() {
                    "list" => {
                        let item = ms.member.as_deref().expect("list member");
                        let item_shape = reg.member_shape(item);
                        v.as_array()
                            .map(|a| {
                                a.iter()
                                    .map(|x| {
                                        let t = format_header(x, item_shape, item);
                                        if item_shape.kind == "string" {
                                            quote_header_item(&t)
                                        } else {
                                            t
                                        }
                                    })
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default()
                    }
                    _ => format_header(v, ms, member),
                };
                res.headers.push((wire.to_string(), text));
            }
            Some("headers") => {
                let value_ref = ms.value.as_deref().expect("prefix header map value");
                let value_shape = reg.member_shape(value_ref);
                for (k, x) in v.as_object().into_iter().flatten() {
                    res.headers.push((
                        format!("{wire}{k}"),
                        format_header(x, value_shape, value_ref),
                    ));
                }
            }
            _ => {}
        }
    }
    let (body, content_type) = serialize_body(reg, &output.shape, shape, &obj, format);
    res.body = body;
    if !res
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("content-length"))
    {
        res.headers
            .push(("Content-Length".to_string(), res.body.len().to_string()));
    }
    if let Some(ct) = content_type
        && !res
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
    {
        res.headers.push(("Content-Type".to_string(), ct));
    }
    res
}

fn serialize_body(
    reg: &Registry,
    shape_name: &str,
    shape: &Shape,
    obj: &Map<String, Value>,
    format: BodyFormat,
) -> (Vec<u8>, Option<String>) {
    if let Some(payload) = &shape.payload {
        let member = &shape.members[payload];
        let ms = reg.member_shape(member);
        let Some(v) = obj.get(payload).filter(|v| !v.is_null()) else {
            return (Vec::new(), None);
        };
        return match ms.kind.as_str() {
            "blob" => (
                STANDARD
                    .decode(v.as_str().unwrap_or_default())
                    .unwrap_or_default(),
                Some("application/octet-stream".to_string()),
            ),
            "string" => (
                v.as_str().unwrap_or_default().as_bytes().to_vec(),
                Some("text/plain".to_string()),
            ),
            _ => match format {
                BodyFormat::Json => (
                    serde_json::to_vec(&json::to_wire(reg, v, ms, Some(member)))
                        .expect("json serializes"),
                    Some("application/json".to_string()),
                ),
                BodyFormat::Xml => {
                    let root_name = member
                        .location_name
                        .as_deref()
                        .filter(|n| *n != payload.as_str())
                        .unwrap_or_else(|| xml_root_name(&member.shape, ms));
                    let mut root = Element::new(root_name);
                    if let Some(attr) = ms
                        .xml_namespace
                        .attribute()
                        .or_else(|| member.xml_namespace.attribute())
                    {
                        root.attrs.push(attr);
                    }
                    write_structure_members(reg, &mut root, v, ms, "iso8601");
                    (
                        root.to_xml().into_bytes(),
                        Some("application/xml".to_string()),
                    )
                }
            },
        };
    }
    let body_members: Map<String, Value> = obj
        .iter()
        .filter(|(k, _)| shape.members.get(*k).is_some_and(|m| m.location.is_none()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    match format {
        BodyFormat::Json => (
            serde_json::to_vec(&json::to_wire(
                reg,
                &Value::Object(body_members),
                shape,
                None,
            ))
            .expect("json serializes"),
            Some("application/json".to_string()),
        ),
        BodyFormat::Xml if body_members.is_empty() => {
            (Vec::new(), Some("application/xml".to_string()))
        }
        BodyFormat::Xml => {
            let mut root = Element::new(xml_root_name(shape_name, shape));
            if let Some(attr) = shape.xml_namespace.attribute() {
                root.attrs.push(attr);
            }
            write_structure_members(
                reg,
                &mut root,
                &Value::Object(body_members),
                shape,
                "iso8601",
            );
            (
                root.to_xml().into_bytes(),
                Some("application/xml".to_string()),
            )
        }
    }
}

pub fn serialize_error(
    reg: &Registry,
    error: &AwsError,
    shape: Option<&Shape>,
    format: BodyFormat,
    request_id: &str,
) -> HttpResponse {
    let mut res = HttpResponse {
        status: error.status,
        ..Default::default()
    };
    if let Some(shape) = shape {
        for (name, member) in &shape.members {
            if member.location.as_deref() != Some("header") {
                continue;
            }
            if let Some(v) = error.data.get(name) {
                let ms = reg.member_shape(member);
                res.headers.push((
                    member.location_name.clone().unwrap_or_else(|| name.clone()),
                    format_header(v, ms, member),
                ));
            }
        }
    }
    match format {
        BodyFormat::Json => {
            res.headers
                .push(("X-Amzn-Errortype".to_string(), error.code.clone()));
            res.headers
                .push(("Content-Type".to_string(), "application/json".to_string()));
            let mut body = json::error_body(reg, error, shape);
            if let Value::Object(b) = &mut body {
                b.remove("__type");
            }
            if let (Value::Object(b), Some(shape)) = (&mut body, shape) {
                b.retain(|k, _| {
                    k.eq_ignore_ascii_case("message")
                        || shape.members.iter().any(|(n, m)| {
                            m.location.is_none() && m.location_name.as_deref().unwrap_or(n) == k
                        })
                });
            }
            res.body = serde_json::to_vec(&body).expect("json serializes");
        }
        BodyFormat::Xml => {
            let mut err = Element::new("Error");
            err.children.push(Element::with_text(
                "Type",
                if error.sender_fault {
                    "Sender"
                } else {
                    "Receiver"
                },
            ));
            err.children
                .push(Element::with_text("Code", error.code.clone()));
            if !error.message.is_empty() {
                err.children
                    .push(Element::with_text("Message", error.message.clone()));
            }
            if let Some(shape) = shape {
                let body: Map<String, Value> = error
                    .data
                    .iter()
                    .filter(|(k, _)| {
                        shape.members.get(*k).is_some_and(|m| m.location.is_none())
                            && !k.eq_ignore_ascii_case("message")
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                write_structure_members(reg, &mut err, &Value::Object(body), shape, "iso8601");
            }
            let mut root = Element::new("ErrorResponse");
            root.children.push(err);
            root.children
                .push(Element::with_text("RequestId", request_id));
            res.headers
                .push(("Content-Type".to_string(), "application/xml".to_string()));
            res.body = root.to_xml().into_bytes();
        }
    }
    res
}

/// S3-style error document: `<Error>` at the root carrying the code, the
/// message, every extra field, and the request id.
pub fn serialize_unwrapped_error(error: &AwsError, request_id: &str) -> HttpResponse {
    let mut err = Element::new("Error");
    err.children
        .push(Element::with_text("Code", error.code.clone()));
    err.children
        .push(Element::with_text("Message", error.message.clone()));
    for (k, v) in error.data.iter() {
        let text = v.as_str().map_or_else(|| v.to_string(), str::to_string);
        err.children.push(Element::with_text(k.clone(), text));
    }
    err.children
        .push(Element::with_text("RequestId", request_id));
    HttpResponse {
        status: error.status,
        headers: vec![("Content-Type".to_string(), "application/xml".to_string())],
        body: err.to_xml().into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_labels_and_greedy() {
        let l = match_uri("/a/{foo}/baz/{baz+}", "/a/hello%2Fescape/baz/there/guy").unwrap();
        assert_eq!(l["foo"], "hello/escape");
        assert_eq!(l["baz"], "there/guy");
        assert!(match_uri("/a/{foo}", "/b/x").is_none());
    }

    #[test]
    fn splits_quoted_header_lists() {
        let s = Shape {
            kind: "string".into(),
            ..Default::default()
        };
        assert_eq!(
            split_header_list("\"b,c\", \"\\\"def\\\"\", a", &s),
            vec!["b,c", "\"def\"", "a"]
        );
    }
}
