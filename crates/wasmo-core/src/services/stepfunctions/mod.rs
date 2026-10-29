mod asl;
mod executions;
mod interp;
mod machines;
mod task;

use std::collections::{BTreeMap, HashMap, VecDeque};

use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::codec::AwsError;
use crate::codec::time::format_iso8601;
use crate::embedded_model;
use crate::service::{Ctx, Delivery, InputExt, OpResult, Service, ServiceModel};
use asl::Failure;
use interp::{Effect, Execution};

#[derive(Debug, Clone)]
struct Version {
    arn: String,
    definition: String,
    role_arn: String,
    description: Option<String>,
    created: f64,
    revision: String,
}

#[derive(Debug, Clone)]
struct Machine {
    arn: String,
    name: String,
    kind: String,
    definition: String,
    role_arn: String,
    created: f64,
    updated: Option<f64>,
    revision: String,
    description: Option<String>,
    logging: Value,
    tracing: Value,
    encryption: Value,
    versions: BTreeMap<u64, Version>,
    next_version: u64,
}

#[derive(Debug, Clone)]
struct Alias {
    arn: String,
    name: String,
    machine_arn: String,
    description: Option<String>,
    routing: Vec<Value>,
    created: f64,
    updated: f64,
}

#[derive(Debug, Clone)]
struct Activity {
    arn: String,
    name: String,
    created: f64,
    encryption: Value,
}

#[derive(Debug, Default)]
struct Region {
    machines: BTreeMap<String, Machine>,
    aliases: BTreeMap<String, Alias>,
    activities: BTreeMap<String, Activity>,
    executions: IndexMap<String, Execution>,
    tags: HashMap<String, BTreeMap<String, String>>,
    /// Activity ARN to the tasks waiting for a worker: (token, input).
    queues: HashMap<String, VecDeque<(String, String)>>,
}

struct TokenRef {
    account: String,
    region: String,
    execution: String,
    run: usize,
}

struct ChildRef {
    execution: String,
    run: usize,
    json: bool,
}

#[derive(Default)]
pub struct StepFunctions {
    regions: HashMap<(String, String), Region>,
    /// Call id to the execution and run that made it.
    calls: HashMap<String, (String, usize)>,
    tokens: HashMap<String, TokenRef>,
    /// Execution ARN to the `startExecution.sync` task waiting for it.
    children: HashMap<String, ChildRef>,
}

fn err(code: &str, message: impl Into<String>) -> AwsError {
    AwsError::new(400, code, message)
}

fn secs(ms: f64) -> Value {
    json!(ms / 1000.0)
}

fn tag_list(v: &Value) -> BTreeMap<String, String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some((
                t.get("key")?.as_str()?.to_string(),
                t.get("value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ))
        })
        .collect()
}

fn valid_name(name: &str) -> Result<(), AwsError> {
    let bad = name.is_empty()
        || name.chars().count() > 80
        || name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "<>{}[]?*\"#%\\^|~`$&,;:/".contains(c));
    if bad {
        return Err(err("InvalidName", format!("Invalid Name: '{name}'")));
    }
    Ok(())
}

fn page(items: Vec<Value>, input: &Value) -> (Vec<Value>, Option<String>) {
    let start = input
        .str_field("nextToken")
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let limit = match input.i64_field("maxResults") {
        Some(n) if n > 0 => n as usize,
        _ => 100,
    };
    let end = (start + limit).min(items.len());
    let next = (end < items.len()).then(|| end.to_string());
    (items[start..end].to_vec(), next)
}

fn paged(key: &str, items: Vec<Value>, input: &Value) -> Value {
    let (items, next) = page(items, input);
    let mut out = json!({ key: items });
    if let Some(n) = next {
        out["nextToken"] = json!(n);
    }
    out
}

impl StepFunctions {
    fn region(&mut self, ctx: &Ctx<'_>) -> &mut Region {
        self.regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default()
    }

    /// Carries out what executions asked for, following the effects that
    /// resolving one execution has on others.
    fn effects(&mut self, ctx: &mut Ctx<'_>, execution: &str, fx: Vec<Effect>) {
        let mut queue: VecDeque<(String, Vec<Effect>)> =
            VecDeque::from([(execution.to_string(), fx)]);
        while let Some((exec, fx)) = queue.pop_front() {
            for e in fx {
                match e {
                    Effect::Call { id, run, request } => {
                        self.calls.insert(id.clone(), (exec.clone(), run));
                        if request.sdk {
                            ctx.call_sdk(id, request.service, request.operation, request.input);
                        } else {
                            ctx.call(id, request.service, request.operation, request.input);
                        }
                    }
                    Effect::Token {
                        token,
                        run,
                        activity,
                    } => {
                        self.tokens.insert(
                            token.clone(),
                            TokenRef {
                                account: ctx.account.clone(),
                                region: ctx.region.clone(),
                                execution: exec.clone(),
                                run,
                            },
                        );
                        if let Some((arn, input)) = activity {
                            self.region(ctx)
                                .queues
                                .entry(arn)
                                .or_default()
                                .push_back((token, input));
                        }
                    }
                    Effect::Child { child, run, json } => {
                        let done = self
                            .region(ctx)
                            .executions
                            .get(&child)
                            .filter(|c| c.status != "RUNNING")
                            .map(|c| child_result(c, json));
                        match done {
                            Some(result) => {
                                let mut more = Vec::new();
                                if let Some(ex) = self.region(ctx).executions.get_mut(&exec) {
                                    ex.resolve(run, result, ctx, &mut more);
                                }
                                queue.push_back((exec.clone(), more));
                            }
                            None => {
                                self.children.insert(
                                    child,
                                    ChildRef {
                                        execution: exec.clone(),
                                        run,
                                        json,
                                    },
                                );
                            }
                        }
                    }
                    Effect::Finished => {
                        let parent = self.children.remove(&exec);
                        let Some(ex) = self.region(ctx).executions.get(&exec) else {
                            continue;
                        };
                        if let Some(token) = ex.sync.clone() {
                            let out = if ex.single {
                                executions::test_output(ex)
                            } else {
                                sync_output(ex)
                            };
                            ctx.settle(token, Ok(out));
                        }
                        status_event(ctx, ex);
                        let Some(parent) = parent else {
                            continue;
                        };
                        let result = child_result(ex, parent.json);
                        let mut more = Vec::new();
                        if let Some(p) = self.region(ctx).executions.get_mut(&parent.execution) {
                            p.resolve(parent.run, result, ctx, &mut more);
                        }
                        queue.push_back((parent.execution, more));
                    }
                }
            }
        }
    }

    fn dispatch(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> OpResult {
        if let Some(out) = machines::call(self, ctx, operation, input)? {
            return Ok(out);
        }
        if let Some(out) = machines::aliases(self, ctx, operation, input)? {
            return Ok(out);
        }
        if let Some(out) = executions::call(self, ctx, operation, input)? {
            return Ok(out);
        }
        if let Some(out) = executions::tasks(self, ctx, operation, input)? {
            return Ok(out);
        }
        Err(err(
            "ValidationException",
            format!("wasmo: {operation} is not implemented"),
        ))
    }
}

/// The `startExecution.sync` task result for a finished execution.
fn child_result(ex: &Execution, json: bool) -> Result<Value, Failure> {
    let mut d = executions::describe(ex);
    if ex.status != "SUCCEEDED" {
        return Err(Failure::new("States.TaskFailed", sdk_keys(d).to_string()));
    }
    if json && let Some(out) = ex.output.as_deref() {
        d["output"] = serde_json::from_str(out).unwrap_or(Value::Null);
    }
    Ok(sdk_keys(d))
}

/// Capitalizes the top-level keys, as the integration reports the
/// `DescribeExecution` output.
fn sdk_keys(v: Value) -> Value {
    let Value::Object(o) = v else {
        return v;
    };
    Value::Object(
        o.into_iter()
            .map(|(k, v)| {
                let mut c = k.chars();
                let key = c
                    .next()
                    .map(|f| f.to_uppercase().chain(c).collect())
                    .unwrap_or_default();
                (key, v)
            })
            .collect(),
    )
}

fn sync_output(ex: &Execution) -> Value {
    let mut out = executions::describe(ex);
    out["billingDetails"] = json!({
        "billedMemoryUsedInMB": 64,
        "billedDurationInMilliseconds": ((ex.stop_ms.unwrap_or(ex.start_ms) - ex.start_ms).ceil().max(100.0)) as i64,
    });
    out
}

/// `Step Functions Execution Status Change` on the default event bus.
fn status_event(ctx: &mut Ctx<'_>, ex: &Execution) {
    if ex.kind != "STANDARD" {
        return;
    }
    let now = ctx.now_ms() / 1000.0;
    let event_id = ctx.uuid();
    let stop = ex.stop_ms.map(|s| json!(s as i64));
    ctx.deliver(Delivery {
        target: ctx.arn("events", "event-bus/default"),
        source: ex.arn.clone(),
        body: json!({
            "version": "0",
            "id": event_id,
            "detail-type": "Step Functions Execution Status Change",
            "source": "aws.states",
            "account": ctx.account,
            "time": format_iso8601(now.floor()),
            "region": ctx.region,
            "resources": [ex.arn],
            "detail": {
                "executionArn": ex.arn,
                "stateMachineArn": ex.machine_arn,
                "name": ex.name,
                "status": ex.status,
                "startDate": ex.start_ms as i64,
                "stopDate": stop,
                "input": ex.input,
                "output": ex.output,
                "error": ex.failure.as_ref().map(|f| f.error.clone()),
                "cause": ex.failure.as_ref().map(|f| f.cause.clone()),
            },
        })
        .to_string(),
        ..Default::default()
    });
}

impl Service for StepFunctions {
    fn signing_name(&self) -> &'static str {
        "states"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("stepfunctions")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        self.dispatch(ctx, operation, &input)
    }

    fn answer(&mut self, ctx: &mut Ctx<'_>, id: &str, result: OpResult) {
        let Some((exec, run)) = self.calls.remove(id) else {
            return;
        };
        let mut fx = Vec::new();
        if let Some(ex) = self.region(ctx).executions.get_mut(&exec) {
            ex.answer(run, id, result, ctx, &mut fx);
        }
        self.effects(ctx, &exec, fx);
    }

    fn deliver(&mut self, ctx: &mut Ctx<'_>, resource: &str, delivery: &Delivery) -> bool {
        if !resource.starts_with("stateMachine:") {
            return false;
        }
        let input = json!({
            "stateMachineArn": ctx.arn("states", resource),
            "input": delivery.body,
        });
        executions::start(self, ctx, &input, false).is_ok()
    }
}
