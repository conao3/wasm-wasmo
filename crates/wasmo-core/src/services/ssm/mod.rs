mod filter;
mod params;
mod pattern;
mod tags;

use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Ctx, InputExt, OpResult, Service, ServiceModel};

pub(crate) const DEFAULT_KEY_ID: &str = "alias/aws/ssm";

#[derive(Debug, Clone)]
pub(crate) struct Version {
    pub number: u64,
    pub value: String,
    pub kind: String,
    pub description: Option<String>,
    pub allowed_pattern: Option<String>,
    pub key_id: Option<String>,
    pub tier: String,
    pub data_type: String,
    pub policies: Vec<Value>,
    pub labels: Vec<String>,
    pub modified_ms: f64,
    pub user: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Parameter {
    pub name: String,
    pub versions: Vec<Version>,
    pub tags: indexmap::IndexMap<String, String>,
}

impl Parameter {
    pub fn latest(&self) -> &Version {
        self.versions.last().expect("a parameter has a version")
    }

    pub fn version(&self, number: u64) -> Option<&Version> {
        self.versions.iter().find(|v| v.number == number)
    }

    pub fn labelled(&self, label: &str) -> Option<&Version> {
        self.versions
            .iter()
            .find(|v| v.labels.iter().any(|l| l == label))
    }

    pub fn path_name(&self) -> String {
        rooted(&self.name)
    }
}

pub(crate) fn rooted(name: &str) -> String {
    format!("/{}", name.trim_start_matches('/'))
}

#[derive(Debug, Default)]
pub(crate) struct Region {
    pub params: BTreeMap<String, Parameter>,
}

impl Region {
    pub fn resolve_name(&self, name: &str) -> String {
        let Some(rest) = name.strip_prefix("arn:") else {
            return name.to_string();
        };
        let resource = rest.splitn(5, ':').nth(4).unwrap_or_default();
        let Some(path) = resource.strip_prefix("parameter") else {
            return name.to_string();
        };
        let bare = path.trim_start_matches('/');
        if !bare.contains('/') && self.params.contains_key(bare) {
            bare.to_string()
        } else {
            path.to_string()
        }
    }
}

#[derive(Default)]
pub struct Ssm {
    regions: HashMap<(String, String), Region>,
}

pub(crate) fn err(code: &str, message: impl Into<String>) -> AwsError {
    AwsError::new(400, code, message)
}

pub(crate) fn validation(message: impl Into<String>) -> AwsError {
    err("ValidationException", message)
}

pub(crate) fn not_found(name: &str) -> AwsError {
    err("ParameterNotFound", format!("Parameter {name} not found."))
}

pub(crate) fn version_not_found(name: &str, version: u64) -> AwsError {
    err(
        "ParameterVersionNotFound",
        format!(
            "Systems Manager could not find version {version} of {name}. Verify the version and try again."
        ),
    )
}

pub(crate) fn missing(key: &str) -> AwsError {
    validation(format!(
        "1 validation error detected: Value null at '{}' failed to satisfy constraint: Member must not be null",
        lower_first(key)
    ))
}

pub(crate) fn require<'a>(input: &'a Value, key: &str) -> Result<&'a str, AwsError> {
    input.str_field(key).ok_or_else(|| missing(key))
}

pub(crate) fn lower_first(key: &str) -> String {
    let mut chars = key.chars();
    chars.next().map_or_else(String::new, |c| {
        c.to_lowercase().chain(chars).collect::<String>()
    })
}

pub(crate) fn strings(input: &Value, key: &str) -> Vec<String> {
    input
        .array_field(key)
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

pub(crate) fn arn(ctx: &Ctx<'_>, name: &str) -> String {
    ctx.arn(
        "ssm",
        &format!("parameter/{}", name.trim_start_matches('/')),
    )
}

pub(crate) fn ciphertext(v: &Version) -> String {
    let key = v.key_id.as_deref().unwrap_or(DEFAULT_KEY_ID);
    let mut bytes = format!("wasmo-kms\0{key}\0").into_bytes();
    bytes.extend(v.value.bytes().rev());
    STANDARD.encode(bytes)
}

pub(crate) fn shown_value(v: &Version, decrypt: bool) -> String {
    if v.kind == "SecureString" && !decrypt {
        ciphertext(v)
    } else {
        v.value.clone()
    }
}

pub(crate) fn seconds(ms: f64) -> Value {
    json!(ms / 1000.0)
}

pub(crate) fn page<T>(
    items: Vec<T>,
    input: &Value,
    default: usize,
    max: usize,
) -> Result<(Vec<T>, Option<String>), AwsError> {
    let limit = match input.i64_field("MaxResults") {
        None => default,
        Some(n) if n >= 1 && n as usize <= max => n as usize,
        Some(n) => {
            return Err(validation(format!(
                "1 validation error detected: Value '{n}' at 'maxResults' failed to satisfy constraint: Member must have value less than or equal to {max}"
            )));
        }
    };
    let start = match input.str_field("NextToken") {
        None => 0,
        Some(token) => STANDARD
            .decode(token)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.strip_prefix("wasmo:")?.parse::<usize>().ok())
            .filter(|n| *n <= items.len())
            .ok_or_else(|| err("InvalidNextToken", "The specified token isn't valid."))?,
    };
    let end = (start + limit).min(items.len());
    let next = (end < items.len()).then(|| STANDARD.encode(format!("wasmo:{end}")));
    Ok((
        items.into_iter().skip(start).take(end - start).collect(),
        next,
    ))
}

impl Service for Ssm {
    fn signing_name(&self) -> &'static str {
        "ssm"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("ssm")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let region = self
            .regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default();
        let input = &input;
        match operation {
            "PutParameter" => params::put(region, ctx, input),
            "GetParameter" => params::get(region, ctx, input),
            "GetParameters" => params::get_many(region, ctx, input),
            "DeleteParameter" => params::delete(region, input),
            "DeleteParameters" => params::delete_many(region, input),
            "GetParameterHistory" => params::history(region, input),
            "LabelParameterVersion" => params::label(region, input),
            "UnlabelParameterVersion" => params::unlabel(region, input),
            "GetParametersByPath" => filter::by_path(region, ctx, input),
            "DescribeParameters" => filter::describe(region, ctx, input),
            "AddTagsToResource" => tags::add(region, input),
            "RemoveTagsFromResource" => tags::remove(region, input),
            "ListTagsForResource" => tags::list(region, input),
            other => Err(err(
                "UnknownOperationException",
                format!("wasmo: {other} is not implemented"),
            )),
        }
    }
}
