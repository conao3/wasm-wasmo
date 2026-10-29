use std::any::Any;
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
    /// File name of the model under `vendor/aws-models/` (botocore service name).
    pub name: &'static str,
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
    pub fn parse(name: &'static str, json: &str) -> Self {
        let raw: RawModel = serde_json::from_str(json).expect("embedded model parses");
        let query_compatible = raw.metadata.get("awsQueryCompatible").is_some();
        Self {
            name,
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

/// A message bound for another resource, routed by the dispatcher after the
/// triggering call succeeds.
#[derive(Debug, Clone, Default)]
pub struct Delivery {
    pub target: String,
    pub source: String,
    pub body: String,
    /// Binary payload for targets that store blobs (Kinesis, Firehose); `body` is
    /// used when unset.
    pub bytes: Option<Vec<u8>>,
    pub subject: Option<String>,
    /// SQS/SNS-style attributes: `{name: {DataType, StringValue | BinaryValue}}`.
    pub attributes: Map<String, Value>,
    pub group_id: Option<String>,
    pub dedup_id: Option<String>,
    /// Where the delivery goes when the target does not accept it.
    pub dead_letter: Option<String>,
}

/// A request from one service to another inside the emulator, in the same
/// account and region, answered through `Service::answer` with the callee's
/// output or error. A callee that sets `Ctx::awaiting` (a synchronous Lambda
/// invocation, a `StartSyncExecution`) answers once that token settles.
#[derive(Debug, Clone)]
pub struct Call {
    /// Chosen by the caller; `Service::answer` receives it back.
    pub id: String,
    /// Model name of the callee (`lambda`, `dynamodb`, `stepfunctions`, ...).
    pub service: String,
    pub operation: String,
    pub input: Value,
    pub account: String,
    pub region: String,
    /// Member names follow the SDK convention (`sdk` module) instead of the
    /// callee's model.
    pub sdk: bool,
}

/// The value a token set in `Ctx::awaiting` settles with: an operation result
/// for SDK requests and calls, or a raw HTTP response for `execute-api`.
#[derive(Debug)]
pub enum Settlement {
    Output(OpResult),
    Http(Response),
}

/// What a service call left for the dispatcher besides its result.
#[derive(Debug, Default)]
pub struct Effects {
    pub outbox: Vec<Delivery>,
    pub calls: Vec<Call>,
    pub settled: Vec<(String, Settlement)>,
    pub awaiting: Option<String>,
}

pub struct Arn<'a> {
    pub partition: &'a str,
    pub service: &'a str,
    pub region: &'a str,
    pub account: &'a str,
    pub resource: &'a str,
}

impl<'a> Arn<'a> {
    pub fn parse(arn: &'a str) -> Option<Self> {
        let mut parts = arn.splitn(6, ':');
        (parts.next()? == "arn").then_some(())?;
        Some(Self {
            partition: parts.next()?,
            service: parts.next()?,
            region: parts.next()?,
            account: parts.next()?,
            resource: parts.next()?,
        })
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
    pub outbox: Vec<Delivery>,
    /// Set by an operation whose response waits for a token to settle: a
    /// Lambda invocation id, or a token the service settles later through
    /// `settle` / `respond`.
    pub awaiting: Option<String>,
    pub calls: Vec<Call>,
    pub settled: Vec<(String, Settlement)>,
}

impl<'a> Ctx<'a> {
    pub fn new(
        account: impl Into<String>,
        region: impl Into<String>,
        endpoint: impl Into<String>,
        env: &'a mut dyn Env,
    ) -> Self {
        Self {
            account: account.into(),
            region: region.into(),
            endpoint: endpoint.into(),
            security_token: None,
            env,
            outbox: Vec::new(),
            awaiting: None,
            calls: Vec::new(),
            settled: Vec::new(),
        }
    }

    /// Asks another service for `operation`; the result comes back through
    /// `Service::answer` with the same `id`.
    pub fn call(
        &mut self,
        id: impl Into<String>,
        service: impl Into<String>,
        operation: impl Into<String>,
        input: Value,
    ) {
        self.calls.push(Call {
            id: id.into(),
            service: service.into(),
            operation: operation.into(),
            input,
            account: self.account.clone(),
            region: self.region.clone(),
            sdk: false,
        });
    }

    /// Like `call`, with SDK-style member names (`aws-sdk:*` integrations).
    pub fn call_sdk(
        &mut self,
        id: impl Into<String>,
        service: impl Into<String>,
        operation: impl Into<String>,
        input: Value,
    ) {
        self.call(id, service, operation, input);
        if let Some(c) = self.calls.last_mut() {
            c.sdk = true;
        }
    }

    /// Settles a token an earlier operation left in `awaiting`.
    pub fn settle(&mut self, token: impl Into<String>, result: OpResult) {
        self.settled
            .push((token.into(), Settlement::Output(result)));
    }

    /// Answers a raw HTTP request that waited on `token`.
    pub fn respond(&mut self, token: impl Into<String>, response: Response) {
        self.settled
            .push((token.into(), Settlement::Http(response)));
    }

    pub fn effects(&mut self) -> Effects {
        Effects {
            outbox: std::mem::take(&mut self.outbox),
            calls: std::mem::take(&mut self.calls),
            settled: std::mem::take(&mut self.settled),
            awaiting: self.awaiting.take(),
        }
    }

    pub fn deliver(&mut self, delivery: Delivery) {
        self.outbox.push(delivery);
    }

    pub fn random_bytes(&mut self, buf: &mut [u8]) {
        self.env.random_fill(buf);
    }

    pub fn arn(&self, service: &str, resource: &str) -> String {
        format!(
            "arn:aws:{service}:{}:{}:{resource}",
            self.region, self.account
        )
    }
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

pub trait Service: Any {
    /// Lower-case SigV4 signing name (`sqs`, `dynamodb`, ...).
    fn signing_name(&self) -> &'static str;
    fn model(&self) -> &'static ServiceModel;

    /// Service segment of the ARNs this service owns, when it differs from the
    /// signing name (`cloudwatch` for `monitoring`).
    fn arn_service(&self) -> &'static str {
        self.signing_name()
    }
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

    /// Accepts a delivery addressed to one of this service's resources (the ARN
    /// resource part). Returns false when the target does not exist or refuses it.
    fn deliver(&mut self, _ctx: &mut Ctx<'_>, _resource: &str, _delivery: &Delivery) -> bool {
        false
    }

    /// Receives the result of a `Ctx::call` this service made.
    fn answer(&mut self, _ctx: &mut Ctx<'_>, _id: &str, _result: OpResult) {}

    /// Whether a request signed for this service's signing name is meant for
    /// it, when several services share the name (`apigateway` v1 and v2).
    fn accepts(&self, _req: &Request) -> bool {
        true
    }

    /// Answers a request to an `execute-api` endpoint of `api_id`. `req.path`
    /// starts with the stage. Returns None when the API is not this service's.
    /// A response that waits sets `Ctx::awaiting` and is answered with
    /// `Ctx::respond`.
    fn serve(&mut self, _ctx: &mut Ctx<'_>, _api_id: &str, _req: &Request) -> Option<Response> {
        None
    }
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
            $crate::service::ServiceModel::parse(
                $name,
                include_str!(concat!(env!("OUT_DIR"), "/models/", $name, ".json")),
            )
        })
    }};
}
