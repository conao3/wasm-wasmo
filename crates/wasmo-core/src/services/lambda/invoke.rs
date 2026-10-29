use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::esm::InFlight;
use super::versions::resolve;
use super::{
    Completion, Function, InvokeResult, Lambda, MapExt, Region, err, invalid, no_function,
    not_found, target,
};
use crate::codec::AwsError;
use crate::codec::time::format_iso8601;
use crate::service::{Arn, Ctx, Delivery, InputExt, OpResult};

const LOG_TAIL: usize = 4096;
const DEFAULT_RETRIES: i64 = 2;

pub(super) struct Pending {
    pub account: String,
    pub region: String,
    pub endpoint: String,
    pub function: String,
    pub qualifier: Option<String>,
    pub version: String,
    pub invoked_arn: String,
    pub request_id: String,
    pub started_ms: f64,
    pub timeout_s: i64,
    pub memory: i64,
    pub handler: String,
    pub payload: Vec<u8>,
    pub kind: Kind,
}

pub(super) enum Kind {
    Sync { tail: bool },
    Async { attempt: u32 },
    Esm(Box<InFlight>),
}

/// What an invocation returned, in the form the Invoke API reports it.
pub(super) struct Outcome {
    pub payload: Vec<u8>,
    pub error: Option<String>,
    pub log: String,
}

impl Outcome {
    pub fn failed(&self) -> bool {
        self.error.is_some()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.payload).unwrap_or(Value::Null)
    }
}

fn split_version(ctx: &mut Ctx<'_>, f: &Function, qualifier: Option<&str>) -> Option<String> {
    let alias = f.aliases.get(qualifier?)?;
    let weights = alias
        .get("RoutingConfig")?
        .get("AdditionalVersionWeights")?
        .as_object()?;
    let mut b = [0u8; 4];
    ctx.random_bytes(&mut b);
    let mut draw = u32::from_le_bytes(b) as f64 / u32::MAX as f64;
    for (version, w) in weights {
        let w = w.as_f64().unwrap_or(0.0);
        if draw < w {
            return Some(version.clone());
        }
        draw -= w;
    }
    None
}

fn environment(ctx: &Ctx<'_>, config: &Map<String, Value>, log_stream: &str) -> Map<String, Value> {
    let name = config.str_field("FunctionName").unwrap_or_default();
    let mut env = Map::new();
    for (k, v) in [
        ("AWS_REGION", ctx.region.clone()),
        ("AWS_DEFAULT_REGION", ctx.region.clone()),
        ("AWS_LAMBDA_FUNCTION_NAME", name.to_string()),
        (
            "AWS_LAMBDA_FUNCTION_VERSION",
            config.str_field("Version").unwrap_or("$LATEST").to_string(),
        ),
        (
            "AWS_LAMBDA_FUNCTION_MEMORY_SIZE",
            config
                .get("MemorySize")
                .and_then(Value::as_i64)
                .unwrap_or(128)
                .to_string(),
        ),
        ("AWS_LAMBDA_LOG_GROUP_NAME", format!("/aws/lambda/{name}")),
        ("AWS_LAMBDA_LOG_STREAM_NAME", log_stream.to_string()),
        (
            "AWS_EXECUTION_ENV",
            format!(
                "AWS_Lambda_{}",
                config.str_field("Runtime").unwrap_or("wasmo")
            ),
        ),
        (
            "_HANDLER",
            config.str_field("Handler").unwrap_or_default().to_string(),
        ),
    ] {
        env.insert(k.to_string(), json!(v));
    }
    if let Some(vars) = config
        .get("Environment")
        .and_then(|e| e.get("Variables"))
        .and_then(Value::as_object)
    {
        env.extend(vars.clone());
    }
    env
}

pub(super) struct Start<'a> {
    pub function: &'a str,
    pub qualifier: Option<&'a str>,
    pub payload: Vec<u8>,
    pub kind: Kind,
    pub request_id: Option<String>,
    pub client_context: Option<String>,
}

impl Lambda {
    /// Hands an invocation of the function to the host and records what to do
    /// with its answer. Returns the invocation id and the executed version.
    pub(super) fn start(
        &mut self,
        ctx: &mut Ctx<'_>,
        s: Start<'_>,
    ) -> Result<(String, String), AwsError> {
        let region = self
            .regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default();
        let f = region
            .functions
            .get(s.function)
            .ok_or_else(|| no_function(ctx, s.function, s.qualifier))?;
        let routed = split_version(ctx, f, s.qualifier);
        let config = resolve(ctx, f, routed.as_deref().or(s.qualifier))?;
        let version = config.str_field("Version").unwrap_or("$LATEST").to_string();
        let invoked_arn = match s.qualifier {
            Some(q) => format!("{}:{q}", f.arn()),
            None => f.arn().to_string(),
        };
        let id = ctx.uuid();
        let request_id = s.request_id.unwrap_or_else(|| ctx.uuid());
        let now = ctx.now_ms();
        let day = format_iso8601((now / 1000.0).floor());
        let log_stream = format!(
            "{}/{}/{}/[{version}]{}",
            &day[0..4],
            &day[5..7],
            &day[8..10],
            id.replace('-', "")
        );
        let timeout_s = config.get("Timeout").and_then(Value::as_i64).unwrap_or(3);
        let memory = config
            .get("MemorySize")
            .and_then(Value::as_i64)
            .unwrap_or(128);
        let invocation_type = match s.kind {
            Kind::Sync { .. } => "RequestResponse",
            _ => "Event",
        };
        let mut meta = json!({
            "id": id,
            "functionName": s.function,
            "functionVersion": version,
            "invokedFunctionArn": invoked_arn,
            "memoryLimitInMB": memory,
            "timeoutSeconds": timeout_s,
            "deadlineMs": now + timeout_s as f64 * 1000.0,
            "awsRequestId": request_id,
            "logGroupName": format!("/aws/lambda/{}", s.function),
            "logStreamName": log_stream,
            "invocationType": invocation_type,
            "environment": environment(ctx, &config, &log_stream),
            "account": ctx.account,
            "region": ctx.region,
        });
        if let Some(c) = s.client_context {
            meta["clientContext"] = json!(c);
        }
        let handler = config.str_field("Handler").unwrap_or("handler").to_string();
        ctx.env.invoke(meta.to_string().as_bytes(), &s.payload);
        self.pending.insert(
            id.clone(),
            Pending {
                account: ctx.account.clone(),
                region: ctx.region.clone(),
                endpoint: ctx.endpoint.clone(),
                function: s.function.to_string(),
                qualifier: s.qualifier.map(str::to_string),
                version: version.clone(),
                invoked_arn,
                request_id,
                started_ms: now,
                timeout_s,
                memory,
                handler,
                payload: s.payload,
                kind: s.kind,
            },
        );
        Ok((id, version))
    }

    pub fn complete(&mut self, ctx: &mut Ctx<'_>, result: InvokeResult) -> Completion {
        let Some(p) = self.pending.remove(&result.id) else {
            return Completion::default();
        };
        let mut p = p;
        let outcome = outcome(ctx, &p, result);
        match std::mem::replace(&mut p.kind, Kind::Async { attempt: 0 }) {
            Kind::Sync { tail } => {
                let mut out = json!({
                    "StatusCode": 200,
                    "Payload": STANDARD.encode(&outcome.payload),
                    "ExecutedVersion": p.version,
                });
                if outcome.failed() {
                    out["FunctionError"] = json!("Unhandled");
                }
                if tail {
                    let log = outcome.log.as_bytes();
                    let start = log.len().saturating_sub(LOG_TAIL);
                    out["LogResult"] = json!(STANDARD.encode(&log[start..]));
                }
                Completion {
                    response: Some(Ok(out)),
                    ..Completion::default()
                }
            }
            Kind::Async { attempt } => {
                self.settle_async(ctx, p, attempt, &outcome);
                Completion::default()
            }
            Kind::Esm(inflight) => Completion {
                sqs_deletes: self.settle_esm(ctx, *inflight, &p, &outcome),
                ..Completion::default()
            },
        }
    }

    fn event_config(&self, p: &Pending) -> Option<Map<String, Value>> {
        let f = self
            .regions
            .get(&(p.account.clone(), p.region.clone()))?
            .functions
            .get(&p.function)?;
        let key = p.qualifier.clone().unwrap_or_else(|| "$LATEST".into());
        f.event_invoke.get(&key).cloned()
    }

    fn dead_letter(&self, p: &Pending) -> Option<String> {
        let f = self
            .regions
            .get(&(p.account.clone(), p.region.clone()))?
            .functions
            .get(&p.function)?;
        let config = resolve_quiet(f, p.qualifier.as_deref())?;
        config
            .get("DeadLetterConfig")?
            .get("TargetArn")?
            .as_str()
            .map(str::to_string)
    }

    fn settle_async(&mut self, ctx: &mut Ctx<'_>, p: Pending, attempt: u32, outcome: &Outcome) {
        let config = self.event_config(&p).unwrap_or_default();
        let retries = config
            .get("MaximumRetryAttempts")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_RETRIES);
        if outcome.failed() && (attempt as i64) < retries {
            let _ = self.start(
                ctx,
                Start {
                    function: &p.function,
                    qualifier: p.qualifier.as_deref(),
                    payload: p.payload.clone(),
                    kind: Kind::Async {
                        attempt: attempt + 1,
                    },
                    request_id: Some(p.request_id.clone()),
                    client_context: None,
                },
            );
            return;
        }
        let (condition, which, detail) = if outcome.failed() {
            (
                "RetriesExhausted",
                "OnFailure",
                "Lambda Function Invocation Result - Failure",
            )
        } else {
            (
                "Success",
                "OnSuccess",
                "Lambda Function Invocation Result - Success",
            )
        };
        let destination = config
            .get("DestinationConfig")
            .and_then(|d| d.get(which))
            .and_then(|d| d.get("Destination"))
            .and_then(Value::as_str);
        if let Some(target) = destination {
            let record = record(ctx, &p, outcome, condition, attempt + 1);
            send_record(ctx, &p, target, record, detail);
        }
        if outcome.failed()
            && let Some(dlq) = self.dead_letter(&p)
        {
            let mut attributes = Map::new();
            for (k, v) in [
                ("RequestID", p.request_id.clone()),
                ("ErrorCode", "200".to_string()),
                (
                    "ErrorMessage",
                    outcome
                        .json()
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                ),
            ] {
                attributes.insert(
                    k.to_string(),
                    json!({ "DataType": "String", "StringValue": v }),
                );
            }
            ctx.deliver(Delivery {
                target: dlq,
                source: p.invoked_arn.clone(),
                body: String::from_utf8_lossy(&p.payload).into_owned(),
                attributes,
                ..Delivery::default()
            });
        }
    }
}

fn resolve_quiet(f: &Function, qualifier: Option<&str>) -> Option<Map<String, Value>> {
    match qualifier {
        None | Some("$LATEST") => Some(f.config.clone()),
        Some(q) => match q.parse::<u64>() {
            Ok(n) => f.versions.get(&n).cloned(),
            Err(_) => resolve_quiet(f, f.aliases.get(q)?.str_field("FunctionVersion")),
        },
    }
}

/// The invocation record Lambda sends to async and event source mapping
/// destinations.
pub(super) fn record(
    ctx: &Ctx<'_>,
    p: &Pending,
    outcome: &Outcome,
    condition: &str,
    count: u32,
) -> Value {
    let function_arn = if p.qualifier.is_some() {
        p.invoked_arn.clone()
    } else {
        format!("{}:$LATEST", p.invoked_arn)
    };
    let mut response = json!({ "statusCode": 200, "executedVersion": p.version });
    if outcome.failed() {
        response["functionError"] = json!("Unhandled");
    }
    json!({
        "version": "1.0",
        "timestamp": format_iso8601(ctx.now_ms() / 1000.0),
        "requestContext": {
            "requestId": p.request_id,
            "functionArn": function_arn,
            "condition": condition,
            "approximateInvokeCount": count,
        },
        "requestPayload": serde_json::from_slice::<Value>(&p.payload).unwrap_or(Value::Null),
        "responseContext": response,
        "responsePayload": outcome.json(),
    })
}

pub(super) fn send_record(
    ctx: &mut Ctx<'_>,
    p: &Pending,
    target: &str,
    record: Value,
    detail: &str,
) {
    let body = if Arn::parse(target).is_some_and(|a| a.service == "events") {
        json!({
            "Source": "lambda",
            "DetailType": detail,
            "Detail": record.to_string(),
            "Resources": [p.invoked_arn],
        })
        .to_string()
    } else {
        record.to_string()
    };
    ctx.deliver(Delivery {
        target: target.to_string(),
        source: p.invoked_arn.clone(),
        body,
        ..Delivery::default()
    });
}

fn outcome(ctx: &Ctx<'_>, p: &Pending, r: InvokeResult) -> Outcome {
    let elapsed = (ctx.now_ms() - p.started_ms).max(0.0);
    let mut log = format!("START RequestId: {} Version: {}\n", p.request_id, p.version);
    log.push_str(&r.log);
    if !r.log.is_empty() && !r.log.ends_with('\n') {
        log.push('\n');
    }
    let error = if !r.registered {
        log.push_str(&format!(
            "wasmo: no handler is registered for the function \"{}\"\n",
            p.function
        ));
        Some(json!({
            "errorType": "Runtime.HandlerNotFound",
            "errorMessage": format!("{} is undefined or not exported", p.handler),
        }))
    } else if elapsed > p.timeout_s as f64 * 1000.0 {
        Some(json!({
            "errorType": "Sandbox.Timedout",
            "errorMessage": format!(
                "RequestId: {} Error: Task timed out after {}.00 seconds",
                p.request_id, p.timeout_s
            ),
        }))
    } else {
        r.error.map(|e| {
            let mut v = json!({ "errorMessage": e.message, "errorType": e.kind });
            if let Some(t) = e.stack_trace {
                v["stackTrace"] = json!(t);
            }
            v
        })
    };
    log.push_str(&format!(
        "END RequestId: {}\nREPORT RequestId: {}\tDuration: {elapsed:.2} ms\tBilled Duration: {} ms\tMemory Size: {} MB\tMax Memory Used: {} MB\t\n",
        p.request_id,
        p.request_id,
        elapsed.ceil().max(1.0) as u64,
        p.memory,
        p.memory,
    ));
    match error {
        Some(e) => Outcome {
            payload: e.to_string().into_bytes(),
            error: Some(e["errorType"].as_str().unwrap_or_default().to_string()),
            log,
        },
        None => Outcome {
            payload: if r.payload.is_empty() {
                b"null".to_vec()
            } else {
                r.payload
            },
            error: None,
            log,
        },
    }
}

fn throttled() -> AwsError {
    let mut e = err(429, "TooManyRequestsException", "Rate Exceeded.");
    e.data.insert(
        "Reason".into(),
        json!("ReservedFunctionConcurrentInvocationLimitExceeded"),
    );
    e
}

fn reserved(lambda: &mut Lambda, ctx: &Ctx<'_>, name: &str) -> Option<i64> {
    lambda.region(ctx).functions.get(name)?.reserved
}

/// Queues an asynchronous invocation of a function, as an event source or a
/// destination does.
pub(super) fn event(
    lambda: &mut Lambda,
    ctx: &mut Ctx<'_>,
    name: &str,
    qualifier: Option<&str>,
    payload: &[u8],
) -> Result<(), AwsError> {
    if reserved(lambda, ctx, name) == Some(0) {
        return lambda
            .region(ctx)
            .functions
            .contains_key(name)
            .then_some(())
            .ok_or_else(|| no_function(ctx, name, qualifier));
    }
    lambda
        .start(
            ctx,
            Start {
                function: name,
                qualifier,
                payload: payload.to_vec(),
                kind: Kind::Async { attempt: 0 },
                request_id: None,
                client_context: None,
            },
        )
        .map(|_| ())
}

fn payload_of(input: &Value, key: &str) -> Result<Vec<u8>, AwsError> {
    let bytes = match input.str_field(key) {
        Some(p) => STANDARD.decode(p).map_err(|_| {
            err(
                400,
                "InvalidRequestContentException",
                "Could not parse request body into json",
            )
        })?,
        None => b"{}".to_vec(),
    };
    if serde_json::from_slice::<Value>(&bytes).is_err() {
        return Err(err(
            400,
            "InvalidRequestContentException",
            "Could not parse request body into json: Could not parse payload into json: Unexpected character: was expecting a JSON value",
        ));
    }
    Ok(bytes)
}

pub(super) fn call(
    lambda: &mut Lambda,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> OpResult {
    let (name, qualifier) = target(input)?;
    if !lambda.region(ctx).functions.contains_key(&name) {
        return Err(no_function(ctx, &name, None));
    }
    if operation == "InvokeAsync" {
        let payload = payload_of(input, "InvokeArgs")?;
        event(lambda, ctx, &name, qualifier.as_deref(), &payload)?;
        return Ok(json!({ "Status": 202 }));
    }
    let payload = payload_of(input, "Payload")?;
    let invocation = input
        .str_field("InvocationType")
        .unwrap_or("RequestResponse");
    {
        let f = &lambda.region(ctx).functions[&name];
        resolve(ctx, f, qualifier.as_deref())?;
    }
    if invocation != "DryRun" && reserved(lambda, ctx, &name) == Some(0) {
        return Err(throttled());
    }
    match invocation {
        "DryRun" => Ok(json!({ "StatusCode": 204 })),
        "Event" => {
            event(lambda, ctx, &name, qualifier.as_deref(), &payload)?;
            Ok(json!({ "StatusCode": 202 }))
        }
        "RequestResponse" => {
            let client_context = input
                .str_field("ClientContext")
                .and_then(|c| STANDARD.decode(c).ok())
                .map(|c| String::from_utf8_lossy(&c).into_owned());
            let (id, _) = lambda.start(
                ctx,
                Start {
                    function: &name,
                    qualifier: qualifier.as_deref(),
                    payload,
                    kind: Kind::Sync {
                        tail: input.str_field("LogType") == Some("Tail"),
                    },
                    request_id: None,
                    client_context,
                },
            )?;
            ctx.awaiting = Some(id);
            Ok(Value::Null)
        }
        other => Err(invalid(format!(
            "1 validation error detected: Value '{other}' at 'invocationType' failed to satisfy constraint: Member must satisfy enum value set: [Event, RequestResponse, DryRun]"
        ))),
    }
}

fn config_key(f: &Function, qualifier: Option<&str>) -> (String, String) {
    let q = qualifier.unwrap_or("$LATEST").to_string();
    (q.clone(), format!("{}:{q}", f.arn()))
}

fn missing_config(arn: &str) -> AwsError {
    not_found(format!(
        "The function {arn} doesn't have an EventInvokeConfig"
    ))
}

pub(super) fn config(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    if !operation.contains("FunctionEventInvokeConfig") {
        return Ok(None);
    }
    let (name, qualifier) = target(input)?;
    let f = region
        .functions
        .get_mut(&name)
        .ok_or_else(|| no_function(ctx, &name, None))?;
    if let Some(q) = qualifier.as_deref() {
        resolve(ctx, f, Some(q))?;
    }
    let (key, arn) = config_key(f, qualifier.as_deref());
    let now = (ctx.now_ms() / 1000.0).floor();
    Ok(Some(match operation {
        "PutFunctionEventInvokeConfig" | "UpdateFunctionEventInvokeConfig" => {
            let mut c = if operation == "UpdateFunctionEventInvokeConfig" {
                f.event_invoke.get(&key).cloned().unwrap_or_default()
            } else {
                Map::new()
            };
            for k in [
                "MaximumRetryAttempts",
                "MaximumEventAgeInSeconds",
                "DestinationConfig",
            ] {
                if let Some(v) = input.get(k).filter(|v| !v.is_null()) {
                    c.insert(k.into(), v.clone());
                }
            }
            c.insert("FunctionArn".into(), json!(arn));
            c.insert("LastModified".into(), json!(now));
            c.entry("DestinationConfig")
                .or_insert(json!({ "OnSuccess": {}, "OnFailure": {} }));
            f.event_invoke.insert(key, c.clone());
            Value::Object(c)
        }
        "GetFunctionEventInvokeConfig" => Value::Object(
            f.event_invoke
                .get(&key)
                .cloned()
                .ok_or_else(|| missing_config(&arn))?,
        ),
        "DeleteFunctionEventInvokeConfig" => {
            f.event_invoke
                .remove(&key)
                .ok_or_else(|| missing_config(&arn))?;
            json!({})
        }
        _ => {
            let items: Vec<Value> = f
                .event_invoke
                .values()
                .cloned()
                .map(Value::Object)
                .collect();
            json!({ "FunctionEventInvokeConfigs": items })
        }
    }))
}
