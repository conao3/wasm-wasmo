//! Replays bun-bunsai's parity fixtures (`vendor/parity-fixtures/`): each
//! step is serialized into the HTTP request the AWS SDK would send, driven
//! through `Emulator::handle`, and the decoded response is compared with the
//! fixture's real-AWS-derived expectation. Fixtures of an emulated service
//! listed in `parity-passing.txt` must keep passing; the rest must be listed
//! in `parity-gaps.txt` with a reason. Run with `UPDATE_PARITY=1` to rewrite
//! the passing list and `PARITY_SHOW=<id substring>` to print failures.

use std::collections::BTreeSet;
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use crc::{CRC_32_ISO_HDLC, Crc};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use wasmo_core::Emulator;
use wasmo_core::codec::rest::{uri_encode, xml_root_name};
use wasmo_core::codec::scalar::{format_text, parse_text};
use wasmo_core::codec::xml_value::{read_structure, write_structure_members};
use wasmo_core::codec::{cbor, json};
use wasmo_core::env::FixedEnv;
use wasmo_core::model::{MemberRef, Operation, Registry, Shape};
use wasmo_core::service::ServiceModel;
use wasmo_core::services;
use wasmo_core::wire::{Request, RequestMeta, Response};
use wasmo_core::xml::{self, Element};

const HOST: &str = "wasmo.local";
const REGION: &str = "us-east-1";
const UNDEFINED: &str = "\u{0}undefined";

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn tests_file(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(name)
}

struct Client {
    signing_name: &'static str,
    model: &'static ServiceModel,
    raw_operations: Value,
}

impl Client {
    fn new(signing_name: &'static str, model: &'static ServiceModel) -> Self {
        let raw: Value = serde_json::from_str(
            &fs::read_to_string(root().join(format!("vendor/aws-models/{}.json", model.name)))
                .unwrap(),
        )
        .unwrap();
        Self {
            signing_name,
            model,
            raw_operations: raw["operations"].clone(),
        }
    }

    fn reg(&self) -> &Registry {
        &self.model.registry
    }

    fn protocol(&self) -> &str {
        &self.model.metadata.protocol
    }

    fn request(&self, op: &Operation, input: &Value) -> Request {
        let reg = self.reg();
        let input = match op.input.as_ref() {
            Some(i) => sdk_to_params(reg, input, reg.shape(&i.shape)),
            None => Value::Object(Map::new()),
        };
        let mut headers = vec![
            ("host".to_string(), HOST.to_string()),
            (
                "authorization".to_string(),
                format!(
                    "AWS4-HMAC-SHA256 Credential=test/20260101/{REGION}/{}/aws4_request, SignedHeaders=host, Signature=0",
                    self.signing_name
                ),
            ),
        ];
        let (method, path_and_query, body) = match self.protocol() {
            "smithy-rpc-v2-cbor" => {
                let meta = &self.model.metadata;
                headers.push(("smithy-protocol".to_string(), "rpc-v2-cbor".to_string()));
                headers.push(("accept".to_string(), "application/cbor".to_string()));
                let shape = op.input.as_ref().map(|i| reg.shape(&i.shape));
                let body = shape.map_or_else(Vec::new, |s| {
                    cbor::serialize(reg, &input, Some(s), cbor::Epoch::Seconds)
                });
                if !body.is_empty() {
                    headers.push(("content-type".to_string(), "application/cbor".to_string()));
                }
                (
                    "POST".to_string(),
                    format!(
                        "/service/{}/operation/{}",
                        meta.target_prefix.as_deref().unwrap_or_default(),
                        op.name
                    ),
                    body,
                )
            }
            "json" => {
                let meta = &self.model.metadata;
                headers.push((
                    "content-type".to_string(),
                    format!(
                        "application/x-amz-json-{}",
                        meta.json_version.as_deref().unwrap_or("1.0")
                    ),
                ));
                headers.push((
                    "x-amz-target".to_string(),
                    format!(
                        "{}.{}",
                        meta.target_prefix.as_deref().unwrap_or_default(),
                        op.name
                    ),
                ));
                let shape = op.input.as_ref().map(|i| reg.shape(&i.shape));
                (
                    "POST".to_string(),
                    "/".to_string(),
                    json::serialize_output(reg, &input, shape),
                )
            }
            "query" => {
                headers.push((
                    "content-type".to_string(),
                    "application/x-www-form-urlencoded; charset=utf-8".to_string(),
                ));
                let mut form = vec![
                    ("Action".to_string(), op.name.clone()),
                    (
                        "Version".to_string(),
                        self.model.metadata.api_version.clone().unwrap_or_default(),
                    ),
                ];
                if let Some(i) = op.input.as_ref() {
                    query_params(reg, "", &input, None, reg.shape(&i.shape), &mut form);
                }
                let body = form
                    .iter()
                    .map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false)))
                    .collect::<Vec<_>>()
                    .join("&");
                ("POST".to_string(), "/".to_string(), body.into_bytes())
            }
            "rest-xml" => {
                let (path, body) = self.rest_xml_request(op, &input, &mut headers);
                (op.http.method.clone(), path, body)
            }
            other => panic!("protocol {other} has no client serializer"),
        };
        headers.push(("content-length".to_string(), body.len().to_string()));
        Request::from_meta(
            RequestMeta {
                method,
                url: format!("http://{HOST}{path_and_query}"),
                headers,
            },
            body,
        )
    }

    fn rest_xml_request(
        &self,
        op: &Operation,
        input: &Value,
        headers: &mut Vec<(String, String)>,
    ) -> (String, Vec<u8>) {
        let reg = self.reg();
        let (template, literal_query) = op
            .http
            .request_uri
            .split_once('?')
            .unwrap_or((&op.http.request_uri, ""));
        let mut path = template.to_string();
        let mut query: Vec<String> = literal_query
            .split('&')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        let mut body = Vec::new();
        let Some(input_ref) = op.input.as_ref() else {
            return (path, body);
        };
        let shape = reg.shape(&input_ref.shape);
        let mut obj = input.as_object().cloned().unwrap_or_default();
        let checksum = self.raw_operations[&op.name].get("httpChecksum");
        for (trait_name, value) in [
            ("requestAlgorithmMember", "CRC32"),
            ("requestValidationModeMember", "ENABLED"),
        ] {
            if let Some(member) = checksum.and_then(|c| c[trait_name].as_str()) {
                obj.entry(member)
                    .or_insert_with(|| Value::String(value.to_string()));
            }
        }
        for (name, member) in &shape.members {
            let Some(v) = obj.get(name).filter(|v| !v.is_null()) else {
                continue;
            };
            let ms = reg.member_shape(member);
            let wire = member.location_name.as_deref().unwrap_or(name);
            match member.location.as_deref() {
                Some("uri") => {
                    let text = format_text(v, ms, Some(member), "iso8601");
                    path = path
                        .replace(&format!("{{{wire}+}}"), &uri_encode(&text, true))
                        .replace(&format!("{{{wire}}}"), &uri_encode(&text, false));
                }
                Some("querystring") => match ms.kind.as_str() {
                    "list" => {
                        let item = ms.member.as_deref().unwrap();
                        for x in v.as_array().into_iter().flatten() {
                            let t = format_text(x, reg.member_shape(item), Some(item), "iso8601");
                            query.push(format!(
                                "{}={}",
                                uri_encode(wire, false),
                                uri_encode(&t, false)
                            ));
                        }
                    }
                    "map" => {
                        for (k, x) in v.as_object().into_iter().flatten() {
                            let t = x.as_str().map_or_else(|| x.to_string(), str::to_string);
                            query.push(format!(
                                "{}={}",
                                uri_encode(k, false),
                                uri_encode(&t, false)
                            ));
                        }
                    }
                    _ => {
                        let t = format_text(v, ms, Some(member), "iso8601");
                        query.push(format!(
                            "{}={}",
                            uri_encode(wire, false),
                            uri_encode(&t, false)
                        ));
                    }
                },
                Some("header") => {
                    let text = match ms.kind.as_str() {
                        "list" => {
                            let item = ms.member.as_deref().unwrap();
                            v.as_array()
                                .into_iter()
                                .flatten()
                                .map(|x| {
                                    format_text(x, reg.member_shape(item), Some(item), "rfc822")
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                        _ => format_text(v, ms, Some(member), "rfc822"),
                    };
                    headers.push((wire.to_ascii_lowercase(), text));
                }
                Some("headers") => {
                    for (k, x) in v.as_object().into_iter().flatten() {
                        headers.push((
                            format!("{}{}", wire.to_ascii_lowercase(), k.to_ascii_lowercase()),
                            x.as_str().unwrap_or_default().to_string(),
                        ));
                    }
                }
                _ => {}
            }
        }
        if let Some(payload) = &shape.payload {
            let member = &shape.members[payload];
            let ms = reg.member_shape(member);
            if let Some(v) = obj.get(payload).filter(|v| !v.is_null()) {
                body = match ms.kind.as_str() {
                    "blob" => STANDARD.decode(v.as_str().unwrap_or_default()).unwrap(),
                    "string" => v.as_str().unwrap_or_default().as_bytes().to_vec(),
                    _ => {
                        let name = member
                            .location_name
                            .as_deref()
                            .unwrap_or_else(|| xml_root_name(&member.shape, ms));
                        let mut el = Element::new(name);
                        if let Some(attr) = member
                            .xml_namespace
                            .attribute()
                            .or_else(|| ms.xml_namespace.attribute())
                        {
                            el.attrs.push(attr);
                        }
                        write_structure_members(reg, &mut el, v, ms, "iso8601");
                        headers.push(("content-type".to_string(), "application/xml".to_string()));
                        el.to_xml().into_bytes()
                    }
                };
            }
        }
        if checksum.is_some_and(|c| c.get("requestAlgorithmMember").is_some()) {
            let crc = Crc::<u32>::new(&CRC_32_ISO_HDLC).checksum(&body);
            headers.push((
                "x-amz-checksum-crc32".to_string(),
                STANDARD.encode(crc.to_be_bytes()),
            ));
        }
        headers.push((
            "x-amz-content-sha256".to_string(),
            Sha256::digest(&body)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        ));
        if !query.is_empty() {
            path = format!("{path}?{}", query.join("&"));
        }
        (path, body)
    }

    fn decode(&self, op: &Operation, res: &Response) -> Result<Value, (String, u16)> {
        if res.status >= 300 {
            return Err((self.error_name(res), res.status));
        }
        let reg = self.reg();
        let Some(output) = op.output.as_ref() else {
            return Ok(Value::Object(Map::new()));
        };
        let shape = reg.shape(&output.shape);
        match self.protocol() {
            "smithy-rpc-v2-cbor" => Ok(cbor::from_wire(
                reg,
                &cbor::decode(&res.body).unwrap(),
                shape,
                cbor::Epoch::Seconds,
            )),
            "json" => Ok(json::from_wire(
                reg,
                &json::parse_body(&res.body).unwrap(),
                shape,
                None,
            )),
            "query" => {
                let doc = xml::parse(&res.body).unwrap();
                let result = match &output.result_wrapper {
                    Some(w) => doc.child(w).cloned().unwrap_or_default(),
                    None => doc,
                };
                Ok(read_structure(reg, &result, shape, "iso8601"))
            }
            "rest-xml" => Ok(self.rest_xml_output(shape, res)),
            other => panic!("protocol {other} has no client deserializer"),
        }
    }

    fn rest_xml_output(&self, shape: &Shape, res: &Response) -> Value {
        let reg = self.reg();
        let mut out = Map::new();
        for (name, member) in &shape.members {
            let ms = reg.member_shape(member);
            let wire = member.location_name.as_deref().unwrap_or(name);
            match member.location.as_deref() {
                Some("statusCode") => {
                    out.insert(name.clone(), Value::from(res.status));
                }
                Some("header") => {
                    if let Some(v) = header(res, wire).and_then(|t| {
                        let t = if ms.kind == "string" { t } else { t.trim() };
                        parse_text(t, ms, Some(member), "rfc822")
                    }) {
                        out.insert(name.clone(), v);
                    }
                }
                Some("headers") => {
                    let prefix = wire.to_ascii_lowercase();
                    let map: Map<String, Value> = res
                        .headers
                        .iter()
                        .filter_map(|(k, v)| {
                            let k = k.to_ascii_lowercase();
                            k.strip_prefix(&prefix)
                                .map(|rest| (rest.to_string(), Value::String(v.clone())))
                        })
                        .collect();
                    out.insert(name.clone(), Value::Object(map));
                }
                _ => {}
            }
        }
        if let Some(payload) = &shape.payload {
            let member = &shape.members[payload];
            let ms = reg.member_shape(member);
            match ms.kind.as_str() {
                "blob" => {
                    out.insert(payload.clone(), Value::String(STANDARD.encode(&res.body)));
                }
                "string" => {
                    out.insert(
                        payload.clone(),
                        Value::String(String::from_utf8_lossy(&res.body).into_owned()),
                    );
                }
                _ if res.body.iter().all(u8::is_ascii_whitespace) => {}
                _ => {
                    let doc = xml::parse(&res.body).unwrap();
                    out.insert(payload.clone(), read_structure(reg, &doc, ms, "iso8601"));
                }
            }
        } else if !res.body.iter().all(u8::is_ascii_whitespace) {
            let doc = xml::parse(&res.body).unwrap();
            if let Value::Object(body) = read_structure(reg, &doc, shape, "iso8601") {
                for (k, v) in body {
                    if shape.members[&k].location.is_none() {
                        out.insert(k, v);
                    }
                }
            }
        }
        Value::Object(out)
    }

    fn error_name(&self, res: &Response) -> String {
        match self.protocol() {
            "smithy-rpc-v2-cbor" => {
                let body = cbor::decode(&res.body).map(|c| match c {
                    cbor::Cbor::Map(entries) => entries
                        .into_iter()
                        .find(|(k, _)| *k == cbor::Cbor::Text("__type".to_string()))
                        .map(|(_, v)| v),
                    _ => None,
                });
                let code = match body {
                    Ok(Some(cbor::Cbor::Text(t))) => t,
                    _ => String::new(),
                };
                code.rsplit_once('#')
                    .map_or(code.as_str(), |(_, c)| c)
                    .to_string()
            }
            "json" => {
                let body = json::parse_body(&res.body).unwrap_or_default();
                let code = body
                    .get("__type")
                    .and_then(Value::as_str)
                    .or_else(|| header(res, "x-amzn-errortype"))
                    .unwrap_or_default();
                let code = code.rsplit_once('#').map_or(code, |(_, c)| c);
                code.split(':').next().unwrap_or_default().to_string()
            }
            _ => {
                if res.body.iter().all(u8::is_ascii_whitespace) {
                    return match res.status {
                        400 => "BadRequest",
                        403 => "Forbidden",
                        404 => "NotFound",
                        _ => "UnknownError",
                    }
                    .to_string();
                }
                let doc = xml::parse(&res.body).unwrap();
                let error = if doc.local_name() == "Error" {
                    &doc
                } else {
                    doc.child("Error").unwrap_or(&doc)
                };
                let code = error
                    .child("Code")
                    .map(|c| c.text.clone())
                    .unwrap_or_default();
                self.model
                    .registry
                    .error_shape_for_code(&code)
                    .map_or(code, str::to_string)
            }
        }
    }
}

fn header<'a>(res: &'a Response, name: &str) -> Option<&'a str> {
    res.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Marks blob positions of a decoded response as `{"$base64": ...}` so a
/// later `${ref}` substitutes bytes, as the SDK v3 runner does with
/// `Uint8Array`.
fn tag_blobs(reg: &Registry, v: &Value, shape: &Shape) -> Value {
    match (shape.kind.as_str(), v) {
        ("blob", Value::String(s)) => serde_json::json!({ "$base64": s }),
        ("structure", Value::Object(o)) => Value::Object(
            o.iter()
                .map(|(k, x)| {
                    let x = match shape.members.get(k) {
                        Some(m) => tag_blobs(reg, x, reg.member_shape(m)),
                        None => x.clone(),
                    };
                    (k.clone(), x)
                })
                .collect(),
        ),
        ("list", Value::Array(a)) => {
            let item = reg.member_shape(shape.member.as_deref().unwrap());
            Value::Array(a.iter().map(|x| tag_blobs(reg, x, item)).collect())
        }
        ("map", Value::Object(o)) => {
            let value = reg.member_shape(shape.value.as_deref().unwrap());
            Value::Object(
                o.iter()
                    .map(|(k, x)| (k.clone(), tag_blobs(reg, x, value)))
                    .collect(),
            )
        }
        _ => v.clone(),
    }
}

/// Converts an SDK v3 input value into wasmo parameters: blobs given as a
/// string or `{"$base64": ...}` become base64 text.
fn sdk_to_params(reg: &Registry, v: &Value, shape: &Shape) -> Value {
    match (shape.kind.as_str(), v) {
        ("blob", Value::Object(o)) if o.contains_key("$base64") => o["$base64"].clone(),
        ("blob", Value::String(s)) => Value::String(STANDARD.encode(s.as_bytes())),
        ("structure", Value::Object(o)) => Value::Object(
            o.iter()
                .map(|(k, x)| {
                    let x = match shape.members.get(k) {
                        Some(m) => sdk_to_params(reg, x, reg.member_shape(m)),
                        None => x.clone(),
                    };
                    (k.clone(), x)
                })
                .collect(),
        ),
        ("list", Value::Array(a)) => {
            let item = reg.member_shape(shape.member.as_deref().unwrap());
            Value::Array(a.iter().map(|x| sdk_to_params(reg, x, item)).collect())
        }
        ("map", Value::Object(o)) => {
            let value = reg.member_shape(shape.value.as_deref().unwrap());
            Value::Object(
                o.iter()
                    .map(|(k, x)| (k.clone(), sdk_to_params(reg, x, value)))
                    .collect(),
            )
        }
        _ => v.clone(),
    }
}

fn join_key(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}.{name}")
    }
}

fn query_params(
    reg: &Registry,
    prefix: &str,
    v: &Value,
    member: Option<&MemberRef>,
    shape: &Shape,
    out: &mut Vec<(String, String)>,
) {
    match shape.kind.as_str() {
        "structure" => {
            for (name, m) in &shape.members {
                if let Some(x) = v.get(name).filter(|x| !x.is_null()) {
                    let key = join_key(prefix, m.location_name.as_deref().unwrap_or(name));
                    query_params(reg, &key, x, Some(m), reg.member_shape(m), out);
                }
            }
        }
        "list" => {
            let item = shape.member.as_deref().unwrap();
            let items = v.as_array().map(Vec::as_slice).unwrap_or_default();
            if items.is_empty() {
                out.push((prefix.to_string(), String::new()));
            }
            let flattened = shape.flattened || member.is_some_and(|m| m.flattened);
            for (i, x) in items.iter().enumerate() {
                let key = if flattened {
                    format!("{prefix}.{}", i + 1)
                } else {
                    format!(
                        "{prefix}.{}.{}",
                        item.location_name.as_deref().unwrap_or("member"),
                        i + 1
                    )
                };
                query_params(reg, &key, x, Some(item), reg.member_shape(item), out);
            }
        }
        "map" => {
            let key_ref = shape.key.as_deref().unwrap();
            let value_ref = shape.value.as_deref().unwrap();
            let flattened = shape.flattened || member.is_some_and(|m| m.flattened);
            for (i, (k, x)) in v.as_object().into_iter().flatten().enumerate() {
                let entry = if flattened {
                    format!("{prefix}.{}", i + 1)
                } else {
                    format!("{prefix}.entry.{}", i + 1)
                };
                out.push((
                    join_key(&entry, key_ref.location_name.as_deref().unwrap_or("key")),
                    k.clone(),
                ));
                query_params(
                    reg,
                    &join_key(
                        &entry,
                        value_ref.location_name.as_deref().unwrap_or("value"),
                    ),
                    x,
                    Some(value_ref),
                    reg.member_shape(value_ref),
                    out,
                );
            }
        }
        _ => out.push((prefix.to_string(), format_text(v, shape, member, "iso8601"))),
    }
}

fn lookup<'a>(saved: &'a Map<String, Value>, reference: &str) -> Option<&'a Value> {
    let mut parts = reference.split('.');
    let mut cur = saved.get(parts.next()?)?;
    for key in parts {
        cur = cur.as_object()?.get(key)?;
    }
    Some(cur)
}

fn is_reference(inner: &str) -> bool {
    let parts: Vec<&str> = inner.split('.').collect();
    parts.len() >= 2
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
}

fn substitute(v: &Value, saved: &Map<String, Value>) -> Value {
    match v {
        Value::String(s) => {
            if let Some(inner) = s.strip_prefix("${").and_then(|r| r.strip_suffix('}'))
                && is_reference(inner)
            {
                return lookup(saved, inner)
                    .cloned()
                    .unwrap_or_else(|| Value::String(String::new()));
            }
            let mut out = String::new();
            let mut rest = s.as_str();
            while let Some(start) = rest.find("${") {
                out.push_str(&rest[..start]);
                let after = &rest[start + 2..];
                match after.find('}') {
                    Some(end) if is_reference(&after[..end]) => {
                        match lookup(saved, &after[..end]) {
                            Some(Value::String(x)) => out.push_str(x),
                            Some(x) => out.push_str(&x.to_string()),
                            None => {}
                        }
                        rest = &after[end + 1..];
                    }
                    _ => {
                        out.push_str("${");
                        rest = after;
                    }
                }
            }
            out.push_str(rest);
            Value::String(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(|x| substitute(x, saved)).collect()),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, x)| (k.clone(), substitute(x, saved)))
                .collect(),
        ),
        _ => v.clone(),
    }
}

fn undefined() -> Value {
    Value::String(UNDEFINED.to_string())
}

/// The runner's `deepNormalize`: `<ANY>` matches anything, objects compare
/// only the expected keys, arrays only the expected positions.
fn deep_normalize(actual: Option<&Value>, expected: &Value) -> (Option<Value>, Option<Value>) {
    if expected == "<ANY>" {
        return (None, None);
    }
    match (actual, expected) {
        (Some(Value::Array(a)), Value::Array(e)) => {
            let (mut ao, mut eo) = (Vec::new(), Vec::new());
            for (i, ev) in e.iter().enumerate() {
                let (na, ne) = deep_normalize(a.get(i), ev);
                ao.push(na.unwrap_or_else(undefined));
                eo.push(ne.unwrap_or_else(undefined));
            }
            (Some(Value::Array(ao)), Some(Value::Array(eo)))
        }
        (Some(Value::Object(a)), Value::Object(e)) => {
            let (mut ao, mut eo) = (Map::new(), Map::new());
            for (k, ev) in e {
                let (na, ne) = deep_normalize(a.get(k), ev);
                if let Some(na) = na {
                    ao.insert(k.clone(), na);
                }
                if let Some(ne) = ne {
                    eo.insert(k.clone(), ne);
                }
            }
            (Some(Value::Object(ao)), Some(Value::Object(eo)))
        }
        _ => (actual.cloned(), Some(expected.clone())),
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, p)| y.get(k).is_some_and(|q| same(p, q)))
        }
        _ => a == b,
    }
}

fn compare(actual: &Value, step: &Value) -> Result<(), String> {
    let mut a = actual.as_object().cloned().unwrap_or_default();
    let mut e = step["expected"].as_object().cloned().unwrap_or_default();
    for path in step["ignorePaths"].as_array().into_iter().flatten() {
        let path = path.as_str().unwrap_or_default();
        a.remove(path);
        e.remove(path);
    }
    let (na, ne) = deep_normalize(Some(&Value::Object(a)), &Value::Object(e));
    let (na, ne) = (na.unwrap(), ne.unwrap());
    if same(&na, &ne) {
        Ok(())
    } else {
        Err(format!("actual:   {na}\n    expected: {ne}"))
    }
}

fn run_fixture(client: &Client, fixture: &Value) -> Result<(), String> {
    let mut emu = Emulator::new(FixedEnv {
        now_ms: 1_767_225_600_000.0,
        seed: 0x5eed,
    });
    let mut saved = Map::new();
    for (i, step) in fixture["steps"].as_array().unwrap().iter().enumerate() {
        let command = step["command"].as_str().unwrap();
        let fail = |msg: String| format!("step {i} {command}: {msg}");
        let name = command.strip_suffix("Command").unwrap_or(command);
        let op = client
            .model
            .operations
            .get(name)
            .ok_or_else(|| fail("operation is not in the model".to_string()))?;
        let input = substitute(&step["input"], &saved);
        let res = emu.handle(&client.request(op, &input));
        let decoded = client.decode(op, &res);
        if let Some(expected) = step.get("expectedError") {
            let want = (
                expected["name"].as_str().unwrap_or_default().to_string(),
                expected["statusCode"].as_u64().unwrap_or_default() as u16,
            );
            match decoded {
                Err(got) if got == want => continue,
                Err(got) => return Err(fail(format!("error {got:?}, expected {want:?}"))),
                Ok(v) => return Err(fail(format!("succeeded with {v}, expected {want:?}"))),
            }
        }
        let result = decoded.map_err(|(name, status)| {
            fail(format!(
                "{name} ({status}): {}",
                String::from_utf8_lossy(&res.body)
            ))
        })?;
        if let Some(key) = step["saveAs"].as_str() {
            let reg = &client.model.registry;
            let saved_value = match &op.output {
                Some(o) => tag_blobs(reg, &result, reg.shape(&o.shape)),
                None => result.clone(),
            };
            saved.insert(key.to_string(), saved_value);
        }
        compare(&result, step).map_err(fail)?;
    }
    Ok(())
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "json") {
            out.push(path);
        }
    }
}

fn read_ids(name: &str) -> BTreeSet<String> {
    fs::read_to_string(tests_file(name))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split('\t').next())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

#[test]
fn parity_fixtures() {
    let dir = root().join("vendor/parity-fixtures");
    let mut paths = Vec::new();
    collect(&dir, &mut paths);
    paths.sort();
    let clients: Vec<Client> = services::all()
        .iter()
        .map(|s| Client::new(s.signing_name(), s.model()))
        .collect();

    let mut passing = BTreeSet::new();
    let mut failures = Vec::new();
    let mut per_service: Vec<(String, usize, usize)> = Vec::new();
    let mut skipped = 0;
    for path in &paths {
        let id = path
            .strip_prefix(&dir)
            .unwrap()
            .with_extension("")
            .to_string_lossy()
            .into_owned();
        let fixture: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let package = fixture["clientPackage"].as_str().unwrap_or_default();
        let service = package.strip_prefix("@aws-sdk/client-").unwrap_or(package);
        let normalized: String = service.chars().filter(|c| *c != '-').collect();
        let Some(client) = clients.iter().find(|c| {
            c.signing_name == service
                || c.model.metadata.service_id.as_deref().is_some_and(|id| {
                    id.chars()
                        .filter(|c| !c.is_whitespace())
                        .collect::<String>()
                        .eq_ignore_ascii_case(&normalized)
                })
        }) else {
            skipped += 1;
            continue;
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| run_fixture(client, &fixture)))
            .unwrap_or_else(|_| Err("panicked".to_string()));
        let entry = match per_service.iter_mut().find(|(s, _, _)| s == service) {
            Some(e) => e,
            None => {
                per_service.push((service.to_string(), 0, 0));
                per_service.last_mut().unwrap()
            }
        };
        entry.2 += 1;
        match outcome {
            Ok(()) => {
                entry.1 += 1;
                passing.insert(id);
            }
            Err(e) => failures.push((id, e)),
        }
    }
    for (service, pass, total) in &per_service {
        println!("{service:10} {pass:3}/{total}");
    }
    println!("skipped    {skipped} fixtures of services wasmo does not emulate");

    if std::env::var_os("UPDATE_PARITY").is_some() {
        let body: String = passing.iter().map(|id| format!("{id}\n")).collect();
        fs::write(tests_file("parity-passing.txt"), body).unwrap();
        return;
    }
    let expected = read_ids("parity-passing.txt");
    let gaps = read_ids("parity-gaps.txt");
    for id in passing.intersection(&gaps) {
        println!("gap now passes, remove it from parity-gaps.txt: {id}");
    }
    let newly = passing.difference(&expected).count();
    if newly > 0 {
        println!("{newly} newly passing fixtures; rerun with UPDATE_PARITY=1 to record them");
    }
    if let Ok(filter) = std::env::var("PARITY_SHOW") {
        for (id, e) in failures.iter().filter(|(id, _)| id.contains(&filter)) {
            println!("FAIL {id}: {e}");
        }
    }
    let regressions: Vec<_> = failures
        .iter()
        .filter(|(id, _)| expected.contains(id) || !gaps.contains(id))
        .collect();
    assert!(
        regressions.is_empty(),
        "failing fixtures that are neither recorded gaps nor allowed to fail:\n{}",
        regressions
            .iter()
            .map(|(id, e)| format!("{id}: {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
