mod esm;
mod functions;
mod invoke;
mod layers;
mod versions;

use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::codec::AwsError;
use crate::codec::time::format_iso8601;
use crate::embedded_model;
use crate::service::{Ctx, Delivery, InputExt, OpResult, Service, ServiceModel};

pub use esm::{Batch, Poll};

/// A host's answer to an invocation, read from the `wasmo_complete` meta.
#[derive(Debug, Clone, Deserialize)]
pub struct InvokeResult {
    pub id: String,
    #[serde(default = "registered")]
    pub registered: bool,
    #[serde(default)]
    pub error: Option<FunctionError>,
    #[serde(default)]
    pub log: String,
    #[serde(skip)]
    pub payload: Vec<u8>,
}

fn registered() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct FunctionError {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
    #[serde(default, rename = "stackTrace")]
    pub stack_trace: Option<Vec<String>>,
}

impl InvokeResult {
    pub fn parse(meta: &[u8], payload: Vec<u8>) -> Result<Self, String> {
        let mut r: Self = serde_json::from_slice(meta).map_err(|e| e.to_string())?;
        r.payload = payload;
        Ok(r)
    }
}

/// Effects of a completed invocation that reach beyond the Lambda state.
#[derive(Default)]
pub struct Completion {
    /// Output of the `Invoke` call that waited for this invocation.
    pub response: Option<OpResult>,
    pub sqs_deletes: Vec<SqsDelete>,
}

pub struct SqsDelete {
    pub queue_arn: String,
    pub receipt_handles: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct Function {
    config: Map<String, Value>,
    versions: BTreeMap<u64, Map<String, Value>>,
    last_version: u64,
    aliases: BTreeMap<String, Map<String, Value>>,
    policies: BTreeMap<String, Vec<Value>>,
    reserved: Option<i64>,
    provisioned: BTreeMap<String, Map<String, Value>>,
    event_invoke: BTreeMap<String, Map<String, Value>>,
    urls: BTreeMap<String, Map<String, Value>>,
    recursion: Option<String>,
    runtime_management: Option<Map<String, Value>>,
    code_signing: Option<String>,
    scaling: BTreeMap<String, Value>,
}

impl Function {
    fn name(&self) -> &str {
        self.config.str_field("FunctionName").unwrap_or_default()
    }

    fn arn(&self) -> &str {
        self.config.str_field("FunctionArn").unwrap_or_default()
    }
}

#[derive(Debug, Default)]
struct Region {
    functions: BTreeMap<String, Function>,
    esms: BTreeMap<String, esm::Esm>,
    layers: BTreeMap<String, layers::Layer>,
    signing_configs: BTreeMap<String, Map<String, Value>>,
    tags: HashMap<String, BTreeMap<String, String>>,
}

#[derive(Default)]
pub struct Lambda {
    regions: HashMap<(String, String), Region>,
    pending: HashMap<String, invoke::Pending>,
    tick: u64,
}

impl Lambda {
    fn region(&mut self, ctx: &Ctx<'_>) -> &mut Region {
        self.regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default()
    }

    /// Account, region, and client endpoint of a pending invocation.
    pub fn scope(&self, id: &str) -> Option<(String, String, String)> {
        self.pending
            .get(id)
            .map(|p| (p.account.clone(), p.region.clone(), p.endpoint.clone()))
    }
}

trait MapExt {
    fn str_field(&self, key: &str) -> Option<&str>;
}

impl MapExt for Map<String, Value> {
    fn str_field(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }
}

fn err(status: u16, code: &str, message: impl Into<String>) -> AwsError {
    let mut e = AwsError::new(status, code, message);
    e.data.insert("Type".into(), json!("User"));
    e
}

fn invalid(message: impl Into<String>) -> AwsError {
    err(400, "InvalidParameterValueException", message)
}

fn not_found(message: impl Into<String>) -> AwsError {
    err(404, "ResourceNotFoundException", message)
}

fn conflict(message: impl Into<String>) -> AwsError {
    err(409, "ResourceConflictException", message)
}

fn function_arn(ctx: &Ctx<'_>, name: &str) -> String {
    ctx.arn("lambda", &format!("function:{name}"))
}

fn no_function(ctx: &Ctx<'_>, name: &str, qualifier: Option<&str>) -> AwsError {
    let arn = function_arn(ctx, name);
    not_found(match qualifier {
        Some(q) => format!("Function not found: {arn}:{q}"),
        None => format!("Function not found: {arn}"),
    })
}

/// Splits a function name, partial ARN, or ARN into the name and the
/// qualifier appended to it.
fn split_name(raw: &str) -> (String, Option<String>) {
    let parts: Vec<&str> = raw.split(':').collect();
    let rest: &[&str] = match parts.as_slice() {
        ["arn", _, _, _, _, "function", rest @ ..] => rest,
        [_, "function", rest @ ..] => rest,
        rest => rest,
    };
    (
        rest.first().copied().unwrap_or_default().to_string(),
        rest.get(1).map(|q| q.to_string()),
    )
}

/// Name and qualifier of the `FunctionName` member, merged with an explicit
/// `Qualifier`.
fn target(input: &Value) -> Result<(String, Option<String>), AwsError> {
    let raw = input.require_str("FunctionName")?;
    let (name, inline) = split_name(raw);
    if name.is_empty() {
        return Err(invalid("FunctionName is required."));
    }
    Ok((
        name,
        input.str_field("Qualifier").map(str::to_string).or(inline),
    ))
}

fn now_iso(ctx: &Ctx<'_>) -> String {
    let ms = ctx.now_ms();
    let s = format_iso8601((ms / 1000.0).floor());
    format!("{}.{:03}+0000", &s[..19], (ms as u64) % 1000)
}

fn sha256_b64(data: &[u8]) -> String {
    STANDARD.encode(Sha256::digest(data))
}

fn string_map(v: Option<&Map<String, Value>>) -> BTreeMap<String, String> {
    v.into_iter()
        .flatten()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}

fn page(items: Vec<Value>, input: &Value, key: &str) -> (Vec<Value>, Option<String>) {
    let start = input
        .str_field("Marker")
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let limit = input
        .i64_field(key)
        .map_or(items.len(), |l| l.max(1) as usize);
    let end = (start + limit).min(items.len());
    let next = (end < items.len()).then(|| end.to_string());
    (items[start..end].to_vec(), next)
}

fn tag_resource(ctx: &mut Ctx<'_>, region: &mut Region, op: &str, input: &Value) -> OpResult {
    let arn = input.require_str("Resource")?.to_string();
    let known = match split_arn_kind(&arn) {
        Some(("function", name)) => region.functions.contains_key(&split_name(name).0),
        Some(("event-source-mapping", id)) => region.esms.contains_key(id),
        Some(("code-signing-config", _)) => region.signing_configs.contains_key(&arn),
        _ => false,
    };
    if !known {
        return Err(not_found(format!("Resource not found: {arn}")));
    }
    let key = match split_arn_kind(&arn) {
        Some(("function", name)) => function_arn(ctx, &split_name(name).0),
        _ => arn,
    };
    let tags = region.tags.entry(key).or_default();
    Ok(match op {
        "TagResource" => {
            tags.extend(string_map(input.obj_field("Tags")));
            json!({})
        }
        "UntagResource" => {
            for k in input
                .array_field("TagKeys")
                .iter()
                .filter_map(Value::as_str)
            {
                tags.remove(k);
            }
            json!({})
        }
        _ => json!({ "Tags": tags }),
    })
}

/// `(kind, rest)` of a Lambda ARN resource, e.g. `("function", "name:1")`.
fn split_arn_kind(arn: &str) -> Option<(&str, &str)> {
    let resource = arn.splitn(6, ':').nth(5)?;
    resource.split_once(':')
}

impl Service for Lambda {
    fn signing_name(&self) -> &'static str {
        "lambda"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("lambda")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        if matches!(operation, "Invoke" | "InvokeAsync") {
            return invoke::call(self, ctx, operation, &input);
        }
        let mut region = std::mem::take(self.region(ctx));
        let result = (|| {
            if matches!(operation, "TagResource" | "UntagResource" | "ListTags") {
                return tag_resource(ctx, &mut region, operation, &input);
            }
            for part in [
                functions::call,
                functions::settings,
                versions::call,
                invoke::config,
                esm::call,
                layers::call,
            ] {
                if let Some(out) = part(ctx, &mut region, operation, &input)? {
                    return Ok(out);
                }
            }
            Err(AwsError::new(
                400,
                "InvalidParameterValueException",
                format!("wasmo: {operation} is not implemented"),
            ))
        })();
        *self.region(ctx) = region;
        result
    }

    fn deliver(&mut self, ctx: &mut Ctx<'_>, resource: &str, delivery: &Delivery) -> bool {
        let Some(raw) = resource.strip_prefix("function:") else {
            return false;
        };
        let (name, qualifier) = split_name(raw);
        invoke::event(
            self,
            ctx,
            &name,
            qualifier.as_deref(),
            delivery.body.as_bytes(),
        )
        .is_ok()
    }
}
