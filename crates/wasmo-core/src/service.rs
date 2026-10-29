use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::codec::AwsError;
use crate::codec::rest::match_uri;
use crate::env::Env;
use crate::model::{Metadata, Operation, Registry};
use crate::wire::{Request, Response};

#[derive(Debug)]
pub struct ServiceModel {
    pub metadata: Metadata,
    pub operations: HashMap<String, Operation>,
    pub registry: Registry,
    query_compatible: bool,
}

#[derive(Deserialize)]
struct RawModel {
    metadata: Value,
    operations: HashMap<String, Operation>,
    shapes: Value,
}

impl ServiceModel {
    pub fn parse(json: &str) -> Self {
        let raw: RawModel = serde_json::from_str(json).expect("embedded model parses");
        let query_compatible = raw.metadata.get("awsQueryCompatible").is_some();
        Self {
            metadata: serde_json::from_value(raw.metadata).expect("model metadata parses"),
            operations: raw.operations,
            registry: Registry::from_value(&raw.shapes).expect("model shapes parse"),
            query_compatible,
        }
    }

    pub fn is_query_compatible(&self) -> bool {
        self.query_compatible
    }

    /// Picks the operation whose `requestUri` matches `req` most specifically.
    pub fn resolve_rest(&self, req: &Request) -> Option<&Operation> {
        let query_keys: Vec<(String, String)> = crate::codec::rest::parse_query(&req.query);
        self.operations
            .values()
            .filter(|op| op.http.method.eq_ignore_ascii_case(&req.method))
            .filter_map(|op| {
                match_uri(&op.http.request_uri, &req.path)?;
                let required = op.http.request_uri.split_once('?').map_or("", |(_, q)| q);
                let mut score = op
                    .http
                    .request_uri
                    .split('?')
                    .next()
                    .unwrap_or("")
                    .split('/')
                    .filter(|s| !s.is_empty() && !s.starts_with('{'))
                    .count()
                    * 10;
                for literal in required.split('&').filter(|l| !l.is_empty()) {
                    let (k, v) = literal
                        .split_once('=')
                        .map_or((literal, None), |(k, v)| (k, Some(v)));
                    if !query_keys
                        .iter()
                        .any(|(qk, qv)| qk == k && v.is_none_or(|v| qv == v))
                    {
                        return None;
                    }
                    score += 100;
                }
                Some((score, op))
            })
            .max_by_key(|(score, op)| (*score, std::cmp::Reverse(op.name.clone())))
            .map(|(_, op)| op)
    }
}

pub struct Ctx<'a> {
    pub account: String,
    pub region: String,
    /// Scheme and host the client used, e.g. `http://wasmo.local`.
    pub endpoint: String,
    /// `X-Amz-Security-Token` the request was signed with.
    pub security_token: Option<String>,
    pub env: &'a mut dyn Env,
}

impl Ctx<'_> {
    pub fn now_ms(&self) -> f64 {
        self.env.now_ms()
    }

    pub fn uuid(&mut self) -> String {
        let mut b = [0u8; 16];
        self.env.random_fill(&mut b);
        b[6] = (b[6] & 0x0f) | 0x40;
        b[8] = (b[8] & 0x3f) | 0x80;
        let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
        format!(
            "{}-{}-{}-{}-{}",
            &h[0..8],
            &h[8..12],
            &h[12..16],
            &h[16..20],
            &h[20..32]
        )
    }
}

pub type OpResult = Result<Value, AwsError>;

pub trait Service {
    /// Lower-case SigV4 signing name (`sqs`, `dynamodb`, ...).
    fn signing_name(&self) -> &'static str;
    fn model(&self) -> &'static ServiceModel;
    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult;

    /// Whether requests may address a bucket through the host name.
    fn virtual_hosted(&self) -> bool {
        false
    }

    /// Overrides operation resolution where the model alone is ambiguous.
    fn resolve(&self, _req: &Request) -> Option<&'static Operation> {
        None
    }

    /// Adjusts a serialized successful response.
    fn finish(&self, _operation: &str, _output: &Value, _res: &mut Response) {}
}

/// Accessors for operation input.
pub trait InputExt {
    fn str_field(&self, key: &str) -> Option<&str>;
    fn require_str(&self, key: &str) -> Result<&str, AwsError>;
    fn i64_field(&self, key: &str) -> Option<i64>;
    fn obj_field(&self, key: &str) -> Option<&Map<String, Value>>;
    fn array_field(&self, key: &str) -> &[Value];
}

impl InputExt for Value {
    fn str_field(&self, key: &str) -> Option<&str> {
        self.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    }

    fn require_str(&self, key: &str) -> Result<&str, AwsError> {
        self.str_field(key).ok_or_else(|| {
            AwsError::new(
                400,
                "MissingParameter",
                format!("The request must contain the parameter {key}."),
            )
        })
    }

    fn i64_field(&self, key: &str) -> Option<i64> {
        match self.get(key)? {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    fn obj_field(&self, key: &str) -> Option<&Map<String, Value>> {
        self.get(key).and_then(Value::as_object)
    }

    fn array_field(&self, key: &str) -> &[Value] {
        self.get(key)
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

#[macro_export]
macro_rules! embedded_model {
    ($name:literal) => {{
        static MODEL: std::sync::OnceLock<$crate::service::ServiceModel> =
            std::sync::OnceLock::new();
        MODEL.get_or_init(|| {
            $crate::service::ServiceModel::parse(include_str!(concat!(
                env!("OUT_DIR"),
                "/models/",
                $name,
                ".json"
            )))
        })
    }};
}
