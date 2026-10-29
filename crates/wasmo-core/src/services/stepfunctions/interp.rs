//! The ASL interpreter. An execution is a set of runs (the top level, each
//! Parallel branch, each Map iteration) that advance until they wait on a
//! task, so an execution can wait on a Lambda invocation the host completes
//! later and resume when `Service::answer` brings the result.

use std::collections::VecDeque;
use std::rc::Rc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::asl::{self, Failure, Scope, path, template};
use super::task::{self, Kind, Pattern, Plan, Request};
use crate::codec::time::format_iso8601;
use crate::service::{Ctx, OpResult};

const MAX_EVENTS: usize = 25_000;

/// What the service does on behalf of an execution.
pub enum Effect {
    Call {
        id: String,
        run: usize,
        request: Request,
    },
    Token {
        token: String,
        run: usize,
        activity: Option<(String, String)>,
    },
    /// A `startExecution.sync` task waits for the execution `child`.
    Child {
        child: String,
        run: usize,
        json: bool,
    },
    Finished,
}

#[derive(Debug)]
struct Pending {
    plan: Plan,
    call: Option<String>,
    token: Option<String>,
    started_ms: f64,
    timeout_s: Option<f64>,
}

#[derive(Debug)]
struct Fanout {
    map: bool,
    /// The Map state's effective input, for `ItemSelector`.
    input: Value,
    items: Vec<Value>,
    results: Vec<Option<Value>>,
    children: Vec<usize>,
    next: usize,
    running: usize,
    done: usize,
    max: usize,
    failures: usize,
    tolerated: Option<(Option<u64>, Option<f64>)>,
    map_run: Option<usize>,
}

#[derive(Debug)]
enum Wait {
    None,
    Task(Box<Pending>),
    Fanout(Box<Fanout>),
    Reader { call: String, input: Value },
}

#[derive(Debug)]
struct Run {
    def: Rc<Value>,
    state: String,
    input: Value,
    parent: Option<(usize, usize)>,
    wait: Wait,
    retries: Vec<u32>,
    retrying: bool,
    entered_ms: f64,
    item: Option<(usize, Value)>,
    last_event: u64,
    done: bool,
}

#[derive(Debug, Clone)]
pub struct MapRun {
    pub arn: String,
    pub status: String,
    pub start_ms: f64,
    pub stop_ms: Option<f64>,
    pub max_concurrency: i64,
    pub tolerated_count: i64,
    pub tolerated_percentage: f64,
    pub total: usize,
    pub succeeded: usize,
    pub failed: usize,
}

#[derive(Debug)]
pub struct Execution {
    pub arn: String,
    pub machine_arn: String,
    pub machine_name: String,
    pub version_arn: Option<String>,
    pub alias_arn: Option<String>,
    pub name: String,
    pub kind: String,
    pub role_arn: String,
    pub status: String,
    pub start_ms: f64,
    pub stop_ms: Option<f64>,
    pub input: String,
    pub output: Option<String>,
    pub failure: Option<Failure>,
    pub events: Vec<Value>,
    pub map_runs: Vec<MapRun>,
    pub redrive_count: u64,
    pub definition: Rc<Value>,
    /// Token of a `StartSyncExecution` or `TestState` waiting for the end.
    pub sync: Option<String>,
    /// A `TestState` run, kept out of the listings.
    pub hidden: bool,
    /// Stops after the first state and records where it would go.
    pub single: bool,
    pub next_state: Option<String>,
    /// `TestState` mock of a Task result.
    pub mock: Option<Value>,
    runs: Vec<Run>,
    ready: VecDeque<usize>,
    /// Logical time added by Wait states and retry delays.
    offset_ms: f64,
    timeout_s: Option<f64>,
}

pub struct Start {
    pub arn: String,
    pub machine_arn: String,
    pub machine_name: String,
    pub version_arn: Option<String>,
    pub alias_arn: Option<String>,
    pub name: String,
    pub kind: String,
    pub role_arn: String,
    pub input: String,
    pub definition: Value,
}

enum Step {
    Next(String, Value),
    End(Value),
    Fail(Failure),
    Retry,
    Block,
}

fn kind_of(state: &Value) -> &str {
    state
        .get("Type")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn transition(state: &Value, out: Value) -> Step {
    match state.get("Next").and_then(Value::as_str) {
        Some(next) if state.get("End") != Some(&Value::Bool(true)) => {
            Step::Next(next.to_string(), out)
        }
        _ => Step::End(out),
    }
}

impl Execution {
    pub fn new(s: Start, ctx: &Ctx<'_>) -> Self {
        let definition = Rc::new(s.definition);
        let timeout_s = definition.get("TimeoutSeconds").and_then(Value::as_f64);
        Self {
            arn: s.arn,
            machine_arn: s.machine_arn,
            machine_name: s.machine_name,
            version_arn: s.version_arn,
            alias_arn: s.alias_arn,
            name: s.name,
            kind: s.kind,
            role_arn: s.role_arn,
            status: "RUNNING".into(),
            start_ms: ctx.now_ms(),
            stop_ms: None,
            input: s.input,
            output: None,
            failure: None,
            events: Vec::new(),
            map_runs: Vec::new(),
            redrive_count: 0,
            definition,
            sync: None,
            hidden: false,
            single: false,
            next_state: None,
            mock: None,
            runs: Vec::new(),
            ready: VecDeque::new(),
            offset_ms: 0.0,
            timeout_s,
        }
    }

    fn now(&self, ctx: &Ctx<'_>) -> f64 {
        ctx.now_ms() + self.offset_ms
    }

    fn event(
        &mut self,
        run: Option<usize>,
        kind: &str,
        detail: Option<(&str, Value)>,
        ctx: &Ctx<'_>,
    ) {
        let id = self.events.len() as u64 + 1;
        let previous = run.map_or(id - 1, |r| self.runs[r].last_event);
        let mut e = json!({
            "timestamp": self.now(ctx) / 1000.0,
            "type": kind,
            "id": id,
            "previousEventId": previous,
        });
        if let Some((key, value)) = detail {
            e[key] = value;
        }
        self.events.push(e);
        if let Some(r) = run {
            self.runs[r].last_event = id;
        }
    }

    pub fn begin(&mut self, ctx: &mut Ctx<'_>, fx: &mut Vec<Effect>) {
        let input: Value = serde_json::from_str(&self.input).unwrap_or(Value::Null);
        self.event(
            None,
            "ExecutionStarted",
            Some((
                "executionStartedEventDetails",
                json!({
                    "input": self.input,
                    "inputDetails": { "truncated": false },
                    "roleArn": self.role_arn,
                }),
            )),
            ctx,
        );
        if self.definition.get("QueryLanguage").and_then(Value::as_str) == Some("JSONata") {
            self.finish(
                Err(Failure::runtime(
                    "wasmo: the JSONata query language is not supported",
                )),
                ctx,
                fx,
            );
            return;
        }
        let start = self
            .definition
            .get("StartAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let def = self.definition.clone();
        self.spawn(def, start, input, None, None);
        self.drive(ctx, fx);
    }

    fn spawn(
        &mut self,
        def: Rc<Value>,
        state: String,
        input: Value,
        parent: Option<(usize, usize)>,
        item: Option<(usize, Value)>,
    ) -> usize {
        let last_event = parent.map_or(self.events.len() as u64, |(p, _)| self.runs[p].last_event);
        self.runs.push(Run {
            def,
            state,
            input,
            parent,
            wait: Wait::None,
            retries: Vec::new(),
            retrying: false,
            entered_ms: 0.0,
            item,
            last_event,
            done: false,
        });
        let id = self.runs.len() - 1;
        self.ready.push_back(id);
        id
    }

    pub fn drive(&mut self, ctx: &mut Ctx<'_>, fx: &mut Vec<Effect>) {
        while let Some(r) = self.ready.pop_front() {
            if self.status != "RUNNING" {
                self.ready.clear();
                return;
            }
            self.advance(r, ctx, fx);
        }
    }

    fn advance(&mut self, r: usize, ctx: &mut Ctx<'_>, fx: &mut Vec<Effect>) {
        loop {
            if self.status != "RUNNING"
                || self.runs[r].done
                || !matches!(self.runs[r].wait, Wait::None)
            {
                return;
            }
            if self.events.len() > MAX_EVENTS {
                self.finish(
                    Err(Failure::runtime(
                        "The execution reached the maximum number of history events (25000).",
                    )),
                    ctx,
                    fx,
                );
                return;
            }
            if let Some(t) = self.timeout_s
                && self.now(ctx) - self.start_ms > t * 1000.0
            {
                self.time_out(ctx, fx);
                return;
            }
            let step = match self.state(r) {
                Some(state) => {
                    let res = self.enter(r, &state, ctx, fx);
                    res.unwrap_or_else(|f| self.recover(r, &state, f, ctx))
                }
                None => Step::Fail(Failure::runtime(format!(
                    "State '{}' is not defined in the state machine",
                    self.runs[r].state
                ))),
            };
            if !self.apply(r, step, ctx, fx) {
                return;
            }
        }
    }

    /// Applies a state's outcome. Returns whether the run moved on to
    /// another state.
    fn apply(&mut self, r: usize, step: Step, ctx: &mut Ctx<'_>, fx: &mut Vec<Effect>) -> bool {
        match step {
            Step::Next(next, out) if self.single && r == 0 => {
                self.next_state = Some(next);
                self.end_run(r, Ok(out), ctx, fx);
                false
            }
            Step::Next(next, out) => {
                let run = &mut self.runs[r];
                run.state = next;
                run.input = out;
                run.retries.clear();
                run.wait = Wait::None;
                true
            }
            Step::Retry => {
                self.runs[r].wait = Wait::None;
                true
            }
            Step::End(out) => {
                self.end_run(r, Ok(out), ctx, fx);
                false
            }
            Step::Fail(f) => {
                self.end_run(r, Err(f), ctx, fx);
                false
            }
            Step::Block => false,
        }
    }

    fn state(&self, r: usize) -> Option<Value> {
        let run = &self.runs[r];
        run.def.get("States")?.get(&run.state).cloned()
    }

    fn context(&self, r: usize, token: Option<&str>) -> Value {
        let run = &self.runs[r];
        let mut c = json!({
            "Execution": {
                "Id": self.arn,
                "Input": serde_json::from_str::<Value>(&self.input).unwrap_or(Value::Null),
                "Name": self.name,
                "RoleArn": self.role_arn,
                "StartTime": iso(self.start_ms),
                "RedriveCount": self.redrive_count,
            },
            "State": {
                "EnteredTime": iso(run.entered_ms),
                "Name": run.state,
                "RetryCount": run.retries.iter().sum::<u32>(),
            },
            "StateMachine": { "Id": self.machine_arn, "Name": self.machine_name },
        });
        if let Some(t) = token {
            c["Task"] = json!({ "Token": t });
        }
        let item = std::iter::successors(Some(r), |&x| self.runs[x].parent.map(|(p, _)| p))
            .find_map(|x| self.runs[x].item.as_ref());
        if let Some((index, value)) = item {
            c["Map"] = json!({ "Item": { "Index": index, "Value": value } });
        }
        c
    }

    fn enter(
        &mut self,
        r: usize,
        state: &Value,
        ctx: &mut Ctx<'_>,
        fx: &mut Vec<Effect>,
    ) -> Result<Step, Failure> {
        let kind = kind_of(state).to_string();
        let name = self.runs[r].state.clone();
        let raw = self.runs[r].input.clone();
        if !self.runs[r].retrying {
            self.runs[r].entered_ms = self.now(ctx);
            self.event(
                Some(r),
                &format!("{kind}StateEntered"),
                Some((
                    "stateEnteredEventDetails",
                    json!({ "name": name, "input": raw.to_string(), "inputDetails": { "truncated": false } }),
                )),
                ctx,
            );
        }
        self.runs[r].retrying = false;
        if state.get("QueryLanguage").and_then(Value::as_str) == Some("JSONata") {
            return Err(Failure::runtime(
                "wasmo: the JSONata query language is not supported",
            ));
        }
        let context = self.context(r, None);
        match kind.as_str() {
            "Pass" => {
                let eff = asl::select(state, "InputPath", &raw, &context)?;
                let eff = parameters(state, "Parameters", eff, &context, ctx)?;
                let result = state.get("Result").cloned().unwrap_or(eff);
                let out = asl::place(state, &raw, result)?;
                let out = asl::select(state, "OutputPath", &out, &context)?;
                self.exit(r, &kind, &name, &out, ctx);
                Ok(transition(state, out))
            }
            "Choice" => {
                let eff = asl::select(state, "InputPath", &raw, &context)?;
                let scope = Scope {
                    input: &eff,
                    context: &context,
                    ctx,
                };
                let mut next = None;
                for rule in state
                    .get("Choices")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if asl::choice(rule, &scope)? {
                        next = rule.get("Next").and_then(Value::as_str).map(str::to_string);
                        break;
                    }
                }
                let Some(next) = next.or_else(|| {
                    state
                        .get("Default")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }) else {
                    return Ok(Step::Fail(Failure::new(
                        "States.NoChoiceMatched",
                        "No Matches!",
                    )));
                };
                let out = asl::select(state, "OutputPath", &eff, &context)?;
                self.exit(r, &kind, &name, &out, ctx);
                Ok(Step::Next(next, out))
            }
            "Wait" => {
                let eff = asl::select(state, "InputPath", &raw, &context)?;
                self.wait(state, &eff, ctx)?;
                let out = asl::select(state, "OutputPath", &eff, &context)?;
                self.exit(r, &kind, &name, &out, ctx);
                Ok(transition(state, out))
            }
            "Succeed" => {
                let eff = asl::select(state, "InputPath", &raw, &context)?;
                let out = asl::select(state, "OutputPath", &eff, &context)?;
                self.exit(r, &kind, &name, &out, ctx);
                Ok(Step::End(out))
            }
            "Fail" => {
                let mut text = |key: &str, path_key: &str| -> Result<String, Failure> {
                    match state.get(path_key).and_then(Value::as_str) {
                        Some(p) => {
                            let mut sc = Scope {
                                input: &raw,
                                context: &context,
                                ctx: &mut *ctx,
                            };
                            let v = if asl::intrinsic::is_call(p) {
                                asl::intrinsic::eval(p, &mut sc)?
                            } else {
                                sc.path(p)?
                            };
                            Ok(asl::to_text(&v))
                        }
                        None => Ok(state
                            .get(key)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()),
                    }
                };
                let error = text("Error", "ErrorPath")?;
                let cause = text("Cause", "CausePath")?;
                Ok(Step::Fail(Failure::new(error, cause)))
            }
            "Task" => self.start_task(r, state, &raw, ctx, fx),
            "Parallel" => self.start_parallel(r, state, &raw, &context, ctx),
            "Map" => self.start_map(r, state, &raw, &context, ctx, fx),
            other => Err(Failure::runtime(format!("Unknown state type '{other}'"))),
        }
    }

    fn exit(&mut self, r: usize, kind: &str, name: &str, out: &Value, ctx: &Ctx<'_>) {
        self.event(
            Some(r),
            &format!("{kind}StateExited"),
            Some((
                "stateExitedEventDetails",
                json!({ "name": name, "output": out.to_string(), "outputDetails": { "truncated": false } }),
            )),
            ctx,
        );
    }

    fn wait(&mut self, state: &Value, eff: &Value, ctx: &Ctx<'_>) -> Result<(), Failure> {
        let lookup = |key: &str| -> Result<Option<Value>, Failure> {
            match state.get(key).and_then(Value::as_str) {
                Some(p) => path::get(eff, p)
                    .map_err(Failure::runtime)?
                    .map(Some)
                    .ok_or_else(|| Failure::runtime(format!("Invalid path '{p}' for {key}"))),
                None => Ok(None),
            }
        };
        let seconds =
            match state.get("Seconds").cloned().or(lookup("SecondsPath")?) {
                Some(s) => Some(s.as_f64().filter(|s| *s >= 0.0).ok_or_else(|| {
                    Failure::runtime("Wait seconds must be a non-negative number")
                })?),
                None => None,
            };
        let until = match state.get("Timestamp").cloned().or(lookup("TimestampPath")?) {
            Some(t) => Some(
                asl::timestamp(&t).ok_or_else(|| {
                    Failure::runtime("Wait timestamp must be an ISO-8601 timestamp")
                })? * 1000.0,
            ),
            None => None,
        };
        let now = self.now(ctx);
        let advance = match (seconds, until) {
            (Some(s), _) => s * 1000.0,
            (None, Some(t)) => (t - now).max(0.0),
            (None, None) => {
                return Err(Failure::runtime("A Wait state needs Seconds or Timestamp"));
            }
        };
        self.offset_ms += advance;
        Ok(())
    }

    fn start_task(
        &mut self,
        r: usize,
        state: &Value,
        raw: &Value,
        ctx: &mut Ctx<'_>,
        fx: &mut Vec<Effect>,
    ) -> Result<Step, Failure> {
        let resource = state
            .get("Resource")
            .and_then(Value::as_str)
            .ok_or_else(|| Failure::runtime("A Task state needs a Resource"))?
            .to_string();
        let is_activity = resource.contains(":activity:");
        let token =
            (is_activity || resource.ends_with(".waitForTaskToken")).then(|| task_token(ctx));
        let context = self.context(r, token.as_deref());
        let eff = asl::select(state, "InputPath", raw, &context)?;
        let params = parameters(state, "Parameters", eff, &context, ctx)?;
        let plan = task::plan(&resource, &params)?;
        let text = params.to_string();
        match plan.kind {
            Kind::Activity => self.event(
                Some(r),
                "ActivityScheduled",
                Some(("activityScheduledEventDetails", json!({ "resource": resource, "input": text, "inputDetails": { "truncated": false } }))),
                ctx,
            ),
            Kind::LambdaArn => {
                self.event(
                    Some(r),
                    "LambdaFunctionScheduled",
                    Some(("lambdaFunctionScheduledEventDetails", json!({ "resource": resource, "input": text, "inputDetails": { "truncated": false } }))),
                    ctx,
                );
                self.event(Some(r), "LambdaFunctionStarted", None, ctx);
            }
            _ => {
                let mut d = json!({
                    "resourceType": plan.resource_type,
                    "resource": plan.resource,
                    "region": ctx.region,
                    "parameters": text,
                });
                if let Some(t) = state.get("TimeoutSeconds") {
                    d["timeoutInSeconds"] = t.clone();
                }
                self.event(Some(r), "TaskScheduled", Some(("taskScheduledEventDetails", d)), ctx);
                self.event(
                    Some(r),
                    "TaskStarted",
                    Some(("taskStartedEventDetails", json!({ "resourceType": plan.resource_type, "resource": plan.resource }))),
                    ctx,
                );
            }
        }
        if let Some(mock) = self.mock.clone() {
            let res = match (
                mock.get("result").and_then(Value::as_str),
                mock.get("errorOutput"),
            ) {
                (Some(r), _) => serde_json::from_str(r)
                    .map_err(|e| Failure::runtime(format!("Invalid mock result: {e}"))),
                (None, Some(e)) => Err(Failure::new(
                    e.get("error").and_then(Value::as_str).unwrap_or_default(),
                    e.get("cause").and_then(Value::as_str).unwrap_or_default(),
                )),
                (None, None) => Err(Failure::runtime("The mock needs result or errorOutput")),
            };
            self.runs[r].wait = Wait::Task(Box::new(Pending {
                plan,
                call: None,
                token: None,
                started_ms: ctx.now_ms(),
                timeout_s: None,
            }));
            self.task_done(r, res, ctx, fx);
            return Ok(Step::Block);
        }
        let call = plan.call.clone().map(|request| {
            let id = ctx.uuid();
            fx.push(Effect::Call {
                id: id.clone(),
                run: r,
                request,
            });
            id
        });
        if let Some(t) = &token {
            fx.push(Effect::Token {
                token: t.clone(),
                run: r,
                activity: is_activity.then(|| (resource.clone(), text.clone())),
            });
        }
        self.runs[r].wait = Wait::Task(Box::new(Pending {
            plan,
            call,
            token,
            started_ms: ctx.now_ms(),
            timeout_s: state.get("TimeoutSeconds").and_then(Value::as_f64),
        }));
        Ok(Step::Block)
    }

    /// Delivers the answer of a call a run made.
    pub fn answer(
        &mut self,
        r: usize,
        id: &str,
        result: OpResult,
        ctx: &mut Ctx<'_>,
        fx: &mut Vec<Effect>,
    ) {
        if self.status != "RUNNING" || r >= self.runs.len() || self.runs[r].done {
            return;
        }
        match &self.runs[r].wait {
            Wait::Reader { call, .. } if call == id => self.read_done(r, result, ctx, fx),
            Wait::Task(p) if p.call.as_deref() == Some(id) => self.call_done(r, result, ctx, fx),
            _ => return,
        }
        self.drive(ctx, fx);
    }

    fn read_done(&mut self, r: usize, result: OpResult, ctx: &mut Ctx<'_>, fx: &mut Vec<Effect>) {
        let Wait::Reader { input, .. } = std::mem::replace(&mut self.runs[r].wait, Wait::None)
        else {
            return;
        };
        let state = self.state(r).unwrap_or(Value::Null);
        let step = match self.read_items(r, result) {
            Ok(items) => self
                .start_fanout(r, &state, input, items, ctx)
                .unwrap_or_else(|f| self.recover(r, &state, f, ctx)),
            Err(f) => self.recover(r, &state, f, ctx),
        };
        if self.apply(r, step, ctx, fx) {
            self.ready.push_back(r);
        }
    }

    fn call_done(&mut self, r: usize, result: OpResult, ctx: &mut Ctx<'_>, fx: &mut Vec<Effect>) {
        let Wait::Task(p) = &mut self.runs[r].wait else {
            return;
        };
        p.call = None;
        let kind = p.plan.kind.clone();
        let pattern = p.plan.pattern;
        let (resource_type, resource) = (p.plan.resource_type.clone(), p.plan.resource.clone());
        match (pattern, &kind, result) {
            (Pattern::Token, _, Ok(out)) => {
                let d = json!({
                    "resourceType": resource_type,
                    "resource": resource,
                    "output": out.to_string(),
                    "outputDetails": { "truncated": false },
                });
                self.event(
                    Some(r),
                    "TaskSubmitted",
                    Some(("taskSubmittedEventDetails", d)),
                    ctx,
                );
            }
            (_, Kind::Child { json }, Ok(out)) => {
                let child = out
                    .get("executionArn")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                fx.push(Effect::Child {
                    child,
                    run: r,
                    json: *json,
                });
            }
            (_, Kind::Child { .. }, Err(e)) => {
                let res = task::finish(
                    &Kind::Service {
                        prefix: "StepFunctions",
                    },
                    Err(e),
                );
                self.task_done(r, res, ctx, fx);
            }
            (_, kind, result) => {
                let res = task::finish(kind, result);
                self.task_done(r, res, ctx, fx);
            }
        }
    }

    /// Whether the run waits on the task token.
    pub fn holds_token(&self, r: usize, token: &str) -> bool {
        self.status == "RUNNING"
            && self.runs.get(r).is_some_and(|run| {
                !run.done && matches!(&run.wait, Wait::Task(p) if p.token.as_deref() == Some(token))
            })
    }

    /// Ends a task waiting on its token (`SendTaskSuccess` / `SendTaskFailure`)
    /// or on a nested execution.
    pub fn resolve(
        &mut self,
        r: usize,
        result: Result<Value, Failure>,
        ctx: &mut Ctx<'_>,
        fx: &mut Vec<Effect>,
    ) {
        if self.status != "RUNNING" || self.runs.get(r).is_none_or(|run| run.done) {
            return;
        }
        if !matches!(self.runs[r].wait, Wait::Task(_)) {
            return;
        }
        self.task_done(r, result, ctx, fx);
        self.drive(ctx, fx);
    }

    /// Records the `ActivityStarted` event when a worker takes the task.
    pub fn activity_started(&mut self, r: usize, worker: Option<&str>, ctx: &Ctx<'_>) {
        let d = worker.map(|w| ("activityStartedEventDetails", json!({ "workerName": w })));
        self.event(Some(r), "ActivityStarted", d, ctx);
    }

    fn task_done(
        &mut self,
        r: usize,
        res: Result<Value, Failure>,
        ctx: &mut Ctx<'_>,
        fx: &mut Vec<Effect>,
    ) {
        let Wait::Task(p) = std::mem::replace(&mut self.runs[r].wait, Wait::None) else {
            return;
        };
        let res = match (res, p.timeout_s) {
            (Ok(_), Some(t)) if ctx.now_ms() - p.started_ms > t * 1000.0 => Err(Failure::new(
                "States.Timeout",
                format!("The task timed out after {t} seconds"),
            )),
            (res, _) => res,
        };
        let (kind, prefix, key) = match p.plan.kind {
            Kind::Activity => ("Activity", "activity", true),
            Kind::LambdaArn => ("LambdaFunction", "lambdaFunction", true),
            _ => ("Task", "task", false),
        };
        let mut d = if key {
            json!({})
        } else {
            json!({ "resourceType": p.plan.resource_type, "resource": p.plan.resource })
        };
        let suffix = match &res {
            Ok(out) => {
                d["output"] = json!(out.to_string());
                d["outputDetails"] = json!({ "truncated": false });
                "Succeeded"
            }
            Err(f) => {
                d["error"] = json!(f.error);
                d["cause"] = json!(f.cause);
                if f.error == "States.Timeout" {
                    "TimedOut"
                } else {
                    "Failed"
                }
            }
        };
        self.event(
            Some(r),
            &format!("{kind}{suffix}"),
            Some((&format!("{prefix}{suffix}EventDetails"), d)),
            ctx,
        );
        let step = self.conclude(r, res, ctx);
        if self.apply(r, step, ctx, fx) {
            self.ready.push_back(r);
        }
    }

    /// Finishes a Task, Parallel, or Map state with its result: selector,
    /// result path, output path, then the transition; or Retry and Catch.
    fn conclude(&mut self, r: usize, res: Result<Value, Failure>, ctx: &mut Ctx<'_>) -> Step {
        let Some(state) = self.state(r) else {
            return Step::Fail(Failure::runtime("State is not defined"));
        };
        let raw = self.runs[r].input.clone();
        let out = res.and_then(|result| {
            let context = self.context(r, None);
            let selected = parameters(&state, "ResultSelector", result, &context, ctx)?;
            let placed = asl::place(&state, &raw, selected)?;
            asl::select(&state, "OutputPath", &placed, &context)
        });
        match out {
            Ok(out) => {
                let name = self.runs[r].state.clone();
                self.exit(r, kind_of(&state), &name, &out, ctx);
                transition(&state, out)
            }
            Err(f) => self.recover(r, &state, f, ctx),
        }
    }

    fn recover(&mut self, r: usize, state: &Value, f: Failure, ctx: &mut Ctx<'_>) -> Step {
        if f.fatal() || !matches!(kind_of(state), "Task" | "Parallel" | "Map") {
            return Step::Fail(f);
        }
        let retriers = state
            .get("Retry")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some((i, retrier)) = retriers.iter().enumerate().find(|(_, x)| {
            f.matches(
                x.get("ErrorEquals")
                    .and_then(Value::as_array)
                    .map_or(&[][..], Vec::as_slice),
            )
        }) {
            let run = &mut self.runs[r];
            if run.retries.len() < retriers.len() {
                run.retries.resize(retriers.len(), 0);
            }
            let attempt = run.retries[i];
            let max = retrier
                .get("MaxAttempts")
                .and_then(Value::as_u64)
                .unwrap_or(3) as u32;
            if attempt < max {
                run.retries[i] += 1;
                run.retrying = true;
                let interval = retrier
                    .get("IntervalSeconds")
                    .and_then(Value::as_f64)
                    .unwrap_or(1.0);
                let rate = retrier
                    .get("BackoffRate")
                    .and_then(Value::as_f64)
                    .unwrap_or(2.0);
                let mut delay = interval * rate.powi(attempt as i32);
                if let Some(cap) = retrier.get("MaxDelaySeconds").and_then(Value::as_f64) {
                    delay = delay.min(cap);
                }
                if retrier.get("JitterStrategy").and_then(Value::as_str) == Some("FULL") {
                    let mut b = [0u8; 4];
                    ctx.random_bytes(&mut b);
                    delay *= u32::from_le_bytes(b) as f64 / u32::MAX as f64;
                }
                self.offset_ms += delay * 1000.0;
                return Step::Retry;
            }
        }
        for catcher in state
            .get("Catch")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let names = catcher
                .get("ErrorEquals")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice);
            if !f.matches(names) {
                continue;
            }
            let raw = self.runs[r].input.clone();
            return match asl::place(catcher, &raw, f.json()) {
                Ok(out) => {
                    let name = self.runs[r].state.clone();
                    self.exit(r, kind_of(state), &name, &out, ctx);
                    match catcher.get("Next").and_then(Value::as_str) {
                        Some(next) => Step::Next(next.to_string(), out),
                        None => Step::Fail(Failure::runtime("A Catch needs Next")),
                    }
                }
                Err(e) => Step::Fail(e),
            };
        }
        Step::Fail(f)
    }

    fn start_parallel(
        &mut self,
        r: usize,
        state: &Value,
        raw: &Value,
        context: &Value,
        ctx: &mut Ctx<'_>,
    ) -> Result<Step, Failure> {
        let eff = asl::select(state, "InputPath", raw, context)?;
        let eff = parameters(state, "Parameters", eff, context, ctx)?;
        let branches = state
            .get("Branches")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.event(Some(r), "ParallelStateStarted", None, ctx);
        let mut children = Vec::new();
        for (i, b) in branches.into_iter().enumerate() {
            let start = b
                .get("StartAt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            children.push(self.spawn(Rc::new(b), start, eff.clone(), Some((r, i)), None));
        }
        let n = children.len();
        self.runs[r].wait = Wait::Fanout(Box::new(Fanout {
            map: false,
            input: eff,
            items: Vec::new(),
            results: vec![None; n],
            children,
            next: n,
            running: n,
            done: 0,
            max: 0,
            failures: 0,
            tolerated: None,
            map_run: None,
        }));
        if n == 0 {
            return Ok(self.fanout_done(r, ctx));
        }
        Ok(Step::Block)
    }

    fn start_map(
        &mut self,
        r: usize,
        state: &Value,
        raw: &Value,
        context: &Value,
        ctx: &mut Ctx<'_>,
        fx: &mut Vec<Effect>,
    ) -> Result<Step, Failure> {
        let eff = asl::select(state, "InputPath", raw, context)?;
        if let Some(reader) = state.get("ItemReader") {
            let resource = reader
                .get("Resource")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let params = parameters(reader, "Parameters", eff.clone(), context, ctx)?;
            let operation = match resource {
                "arn:aws:states:::s3:getObject" => "GetObject",
                "arn:aws:states:::s3:listObjectsV2" => "ListObjectsV2",
                other => {
                    return Err(Failure::runtime(format!(
                        "wasmo: the ItemReader resource '{other}' is not supported"
                    )));
                }
            };
            let id = ctx.uuid();
            fx.push(Effect::Call {
                id: id.clone(),
                run: r,
                request: Request {
                    service: "s3".into(),
                    operation: operation.into(),
                    input: params,
                    sdk: false,
                },
            });
            self.runs[r].wait = Wait::Reader {
                call: id,
                input: eff,
            };
            return Ok(Step::Block);
        }
        let items_path = state
            .get("ItemsPath")
            .and_then(Value::as_str)
            .unwrap_or("$");
        let items = match path::get(&eff, items_path).map_err(Failure::runtime)? {
            Some(Value::Array(a)) => a,
            _ => {
                return Err(Failure::runtime(format!(
                    "Invalid path '{items_path}': the ItemsPath must point to an array"
                )));
            }
        };
        self.start_fanout(r, state, eff, items, ctx)
    }

    fn read_items(&self, r: usize, result: OpResult) -> Result<Vec<Value>, Failure> {
        let out = result.map_err(|e| Failure::new(format!("S3.{}", e.code), e.message))?;
        let state = self.state(r).unwrap_or(Value::Null);
        let config = state
            .get("ItemReader")
            .and_then(|x| x.get("ReaderConfig"))
            .cloned()
            .unwrap_or(Value::Null);
        let mut items = if let Some(contents) = out.get("Contents") {
            contents.as_array().cloned().unwrap_or_default()
        } else {
            let body = out
                .get("Body")
                .and_then(Value::as_str)
                .and_then(|b| STANDARD.decode(b).ok())
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&body);
            match config
                .get("InputType")
                .and_then(Value::as_str)
                .unwrap_or("JSON")
            {
                "CSV" => csv_items(&text, &config),
                "JSONL" => text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(serde_json::from_str)
                    .collect::<Result<_, _>>()
                    .map_err(|e| Failure::new("States.ItemReaderFailed", e.to_string()))?,
                _ => match serde_json::from_str(&text) {
                    Ok(Value::Array(a)) => a,
                    _ => {
                        return Err(Failure::new(
                            "States.ItemReaderFailed",
                            "The object is not a JSON array",
                        ));
                    }
                },
            }
        };
        if let Some(max) = config.get("MaxItems").and_then(Value::as_u64) {
            items.truncate(max as usize);
        }
        Ok(items)
    }

    fn start_fanout(
        &mut self,
        r: usize,
        state: &Value,
        input: Value,
        items: Vec<Value>,
        ctx: &mut Ctx<'_>,
    ) -> Result<Step, Failure> {
        let items = match state.get("ItemBatcher") {
            Some(b) => {
                let size = b
                    .get("MaxItemsPerBatch")
                    .and_then(Value::as_u64)
                    .unwrap_or(items.len().max(1) as u64)
                    .max(1) as usize;
                let batch_input = b.get("BatchInput").cloned();
                items
                    .chunks(size)
                    .map(|c| {
                        let mut v = json!({ "Items": c });
                        if let Some(bi) = &batch_input {
                            v["BatchInput"] = bi.clone();
                        }
                        v
                    })
                    .collect()
            }
            None => items,
        };
        let n = items.len();
        self.event(
            Some(r),
            "MapStateStarted",
            Some(("mapStateStartedEventDetails", json!({ "length": n }))),
            ctx,
        );
        let distributed = state
            .get("ItemProcessor")
            .and_then(|p| p.get("ProcessorConfig"))
            .and_then(|c| c.get("Mode"))
            .and_then(Value::as_str)
            == Some("DISTRIBUTED");
        let count = state.get("ToleratedFailureCount").and_then(Value::as_u64);
        let pct = state
            .get("ToleratedFailurePercentage")
            .and_then(Value::as_f64);
        let max = state
            .get("MaxConcurrency")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let map_run = distributed.then(|| {
            let arn = format!(
                "{}/{}:{}",
                self.machine_arn.replacen(":stateMachine:", ":mapRun:", 1),
                self.runs[r].state,
                ctx.uuid()
            );
            self.event(
                Some(r),
                "MapRunStarted",
                Some(("mapRunStartedEventDetails", json!({ "mapRunArn": arn }))),
                ctx,
            );
            self.map_runs.push(MapRun {
                arn,
                status: "RUNNING".into(),
                start_ms: self.now(ctx),
                stop_ms: None,
                max_concurrency: max as i64,
                tolerated_count: count.unwrap_or(0) as i64,
                tolerated_percentage: pct.unwrap_or(0.0),
                total: n,
                succeeded: 0,
                failed: 0,
            });
            self.map_runs.len() - 1
        });
        self.runs[r].wait = Wait::Fanout(Box::new(Fanout {
            map: true,
            input,
            items,
            results: vec![None; n],
            children: Vec::new(),
            next: 0,
            running: 0,
            done: 0,
            max,
            failures: 0,
            tolerated: (count.is_some() || pct.is_some()).then_some((count, pct)),
            map_run,
        }));
        if n == 0 {
            return Ok(self.fanout_done(r, ctx));
        }
        self.fill(r, &state.clone(), ctx)?;
        Ok(Step::Block)
    }

    /// Starts Map iterations up to the concurrency limit.
    fn fill(&mut self, r: usize, state: &Value, ctx: &mut Ctx<'_>) -> Result<(), Failure> {
        let processor = Rc::new(
            state
                .get("ItemProcessor")
                .or_else(|| state.get("Iterator"))
                .cloned()
                .unwrap_or(Value::Null),
        );
        let start = processor
            .get("StartAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let selector = state
            .get("ItemSelector")
            .or_else(|| state.get("Parameters"))
            .cloned();
        let name = self.runs[r].state.clone();
        loop {
            let Wait::Fanout(f) = &mut self.runs[r].wait else {
                return Ok(());
            };
            if f.next >= f.items.len() || (f.max > 0 && f.running >= f.max) {
                return Ok(());
            }
            let index = f.next;
            f.next += 1;
            f.running += 1;
            let value = f.items[index].clone();
            let map_input = f.input.clone();
            let child_input = match &selector {
                Some(sel) => {
                    let mut context = self.context(r, None);
                    context["Map"] = json!({ "Item": { "Index": index, "Value": value } });
                    let mut scope = Scope {
                        input: &map_input,
                        context: &context,
                        ctx,
                    };
                    template(sel, &mut scope)?
                }
                None => value.clone(),
            };
            self.event(
                Some(r),
                "MapIterationStarted",
                Some((
                    "mapIterationStartedEventDetails",
                    json!({ "name": name, "index": index }),
                )),
                ctx,
            );
            let child = self.spawn(
                processor.clone(),
                start.clone(),
                child_input,
                Some((r, index)),
                Some((index, value)),
            );
            if let Wait::Fanout(f) = &mut self.runs[r].wait {
                f.children.push(child);
            }
        }
    }

    fn end_run(
        &mut self,
        r: usize,
        res: Result<Value, Failure>,
        ctx: &mut Ctx<'_>,
        fx: &mut Vec<Effect>,
    ) {
        self.runs[r].done = true;
        let Some((p, slot)) = self.runs[r].parent else {
            self.finish(res, ctx, fx);
            return;
        };
        let step = self.child_done(p, slot, res, ctx);
        if let Some(step) = step
            && self.apply(p, step, ctx, fx)
        {
            self.ready.push_back(p);
        }
    }

    fn child_done(
        &mut self,
        p: usize,
        slot: usize,
        res: Result<Value, Failure>,
        ctx: &mut Ctx<'_>,
    ) -> Option<Step> {
        let name = self.runs[p].state.clone();
        let Wait::Fanout(f) = &mut self.runs[p].wait else {
            return None;
        };
        let map = f.map;
        f.running -= 1;
        f.done += 1;
        let failure = match res {
            Ok(v) => {
                f.results[slot] = Some(v);
                if let Some(i) = f.map_run {
                    self.map_runs[i].succeeded += 1;
                }
                None
            }
            Err(e) => {
                f.failures += 1;
                f.results[slot] = Some(e.json());
                if let Some(i) = f.map_run {
                    self.map_runs[i].failed += 1;
                }
                Some(e)
            }
        };
        let (done_kind, ok) = match &failure {
            None => ("MapIterationSucceeded", true),
            Some(_) => ("MapIterationFailed", false),
        };
        if map {
            self.event(
                Some(p),
                done_kind,
                Some((
                    if ok {
                        "mapIterationSucceededEventDetails"
                    } else {
                        "mapIterationFailedEventDetails"
                    },
                    json!({ "name": name, "index": slot }),
                )),
                ctx,
            );
        }
        if let Some(e) = failure {
            let Wait::Fanout(f) = &self.runs[p].wait else {
                return None;
            };
            let exceeded = match f.tolerated {
                None => true,
                Some((count, pct)) => {
                    count.is_some_and(|c| f.failures as u64 > c)
                        || pct.is_some_and(|x| {
                            f.failures as f64 * 100.0 / f.items.len().max(1) as f64 > x
                        })
                }
            };
            if exceeded {
                let error = if f.tolerated.is_some() {
                    Failure::new(
                        "States.ExceedToleratedFailureThreshold",
                        "The specified tolerated failure threshold was exceeded",
                    )
                } else {
                    e
                };
                return Some(self.fanout_failed(p, error, ctx));
            }
        }
        let state = self.state(p).unwrap_or(Value::Null);
        if let Err(e) = self.fill(p, &state, ctx) {
            return Some(self.fanout_failed(p, e, ctx));
        }
        let Wait::Fanout(f) = &self.runs[p].wait else {
            return None;
        };
        let total = if f.map {
            f.items.len()
        } else {
            f.results.len()
        };
        (f.done == total).then(|| self.fanout_done(p, ctx))
    }

    fn fanout_done(&mut self, p: usize, ctx: &mut Ctx<'_>) -> Step {
        let Wait::Fanout(f) = std::mem::replace(&mut self.runs[p].wait, Wait::None) else {
            return Step::Block;
        };
        let results: Vec<Value> = f
            .results
            .into_iter()
            .map(|v| v.unwrap_or(Value::Null))
            .collect();
        if let Some(i) = f.map_run {
            let now = self.now(ctx);
            let run = &mut self.map_runs[i];
            run.status = "SUCCEEDED".into();
            run.stop_ms = Some(now);
            self.event(Some(p), "MapRunSucceeded", None, ctx);
        }
        self.event(
            Some(p),
            if f.map {
                "MapStateSucceeded"
            } else {
                "ParallelStateSucceeded"
            },
            None,
            ctx,
        );
        self.conclude(p, Ok(Value::Array(results)), ctx)
    }

    fn fanout_failed(&mut self, p: usize, e: Failure, ctx: &mut Ctx<'_>) -> Step {
        let Wait::Fanout(f) = std::mem::replace(&mut self.runs[p].wait, Wait::None) else {
            return Step::Block;
        };
        for c in f.children.iter().copied() {
            self.abort(c);
        }
        if let Some(i) = f.map_run {
            let now = self.now(ctx);
            self.map_runs[i].status = "FAILED".into();
            self.map_runs[i].stop_ms = Some(now);
            self.event(
                Some(p),
                "MapRunFailed",
                Some((
                    "mapRunFailedEventDetails",
                    json!({ "error": e.error, "cause": e.cause }),
                )),
                ctx,
            );
        }
        self.event(
            Some(p),
            if f.map {
                "MapStateFailed"
            } else {
                "ParallelStateFailed"
            },
            None,
            ctx,
        );
        self.conclude(p, Err(e), ctx)
    }

    fn abort(&mut self, r: usize) {
        self.runs[r].done = true;
        if let Wait::Fanout(f) = &self.runs[r].wait {
            for c in f.children.clone() {
                self.abort(c);
            }
        }
    }

    fn finish(&mut self, res: Result<Value, Failure>, ctx: &Ctx<'_>, fx: &mut Vec<Effect>) {
        if self.status != "RUNNING" {
            return;
        }
        match res {
            Ok(out) => {
                let text = out.to_string();
                self.event(
                    None,
                    "ExecutionSucceeded",
                    Some((
                        "executionSucceededEventDetails",
                        json!({ "output": text, "outputDetails": { "truncated": false } }),
                    )),
                    ctx,
                );
                self.status = "SUCCEEDED".into();
                self.output = Some(text);
            }
            Err(f) => {
                self.event(
                    None,
                    "ExecutionFailed",
                    Some((
                        "executionFailedEventDetails",
                        json!({ "error": f.error, "cause": f.cause }),
                    )),
                    ctx,
                );
                self.status = "FAILED".into();
                self.failure = Some(f);
            }
        }
        self.stop(ctx, fx);
    }

    fn stop(&mut self, ctx: &Ctx<'_>, fx: &mut Vec<Effect>) {
        self.stop_ms = Some(self.now(ctx));
        for r in 0..self.runs.len() {
            self.runs[r].done = true;
        }
        self.ready.clear();
        fx.push(Effect::Finished);
    }

    fn time_out(&mut self, ctx: &Ctx<'_>, fx: &mut Vec<Effect>) {
        let f = Failure::new("States.Timeout", "The execution timed out");
        self.event(
            None,
            "ExecutionTimedOut",
            Some((
                "executionTimedOutEventDetails",
                json!({ "error": f.error, "cause": f.cause }),
            )),
            ctx,
        );
        self.status = "TIMED_OUT".into();
        self.failure = Some(f);
        self.stop(ctx, fx);
    }

    /// `RedriveExecution`: restarts the top-level run at the state where it
    /// stopped.
    pub fn redrive(&mut self, ctx: &mut Ctx<'_>, fx: &mut Vec<Effect>) {
        self.redrive_count += 1;
        self.status = "RUNNING".into();
        self.failure = None;
        self.stop_ms = None;
        self.event(
            None,
            "ExecutionRedriven",
            Some((
                "executionRedrivenEventDetails",
                json!({ "redriveCount": self.redrive_count }),
            )),
            ctx,
        );
        let root = &mut self.runs[0];
        root.done = false;
        root.wait = Wait::None;
        root.retries.clear();
        root.retrying = false;
        self.ready.push_back(0);
        self.drive(ctx, fx);
    }

    /// `StopExecution`.
    pub fn abort_execution(
        &mut self,
        error: Option<&str>,
        cause: Option<&str>,
        ctx: &Ctx<'_>,
        fx: &mut Vec<Effect>,
    ) {
        let mut d = json!({});
        if let Some(e) = error {
            d["error"] = json!(e);
        }
        if let Some(c) = cause {
            d["cause"] = json!(c);
        }
        self.event(
            None,
            "ExecutionAborted",
            Some(("executionAbortedEventDetails", d)),
            ctx,
        );
        self.status = "ABORTED".into();
        self.failure = Some(Failure::new(
            error.unwrap_or_default(),
            cause.unwrap_or_default(),
        ));
        self.stop(ctx, fx);
    }
}

fn parameters(
    state: &Value,
    key: &str,
    input: Value,
    context: &Value,
    ctx: &mut Ctx<'_>,
) -> Result<Value, Failure> {
    match state.get(key) {
        Some(t) => {
            let mut scope = Scope {
                input: &input,
                context,
                ctx,
            };
            template(t, &mut scope)
        }
        None => Ok(input),
    }
}

fn task_token(ctx: &mut Ctx<'_>) -> String {
    let mut b = [0u8; 48];
    ctx.random_bytes(&mut b);
    STANDARD.encode(b)
}

pub fn iso(ms: f64) -> String {
    let s = format_iso8601((ms / 1000.0).floor());
    format!("{}.{:03}Z", &s[..19], (ms as u64) % 1000)
}

fn csv_items(text: &str, config: &Value) -> Vec<Value> {
    let mut rows = text.lines().filter(|l| !l.is_empty()).map(split_csv);
    let headers: Vec<String> = match config.get("CSVHeaderLocation").and_then(Value::as_str) {
        Some("GIVEN") => config
            .get("CSVHeaders")
            .and_then(Value::as_array)
            .map(|h| h.iter().map(asl::to_text).collect())
            .unwrap_or_default(),
        _ => rows.next().unwrap_or_default(),
    };
    rows.map(|row| {
        let mut o = Map::new();
        for (i, h) in headers.iter().enumerate() {
            o.insert(h.clone(), json!(row.get(i).cloned().unwrap_or_default()));
        }
        Value::Object(o)
    })
    .collect()
}

fn split_csv(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}
