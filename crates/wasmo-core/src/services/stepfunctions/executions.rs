use serde_json::{Value, json};

use super::asl::Failure;
use super::interp::{Execution, Start};
use super::machines::{self, split_machine_arn};
use super::{StepFunctions, err, paged, secs, sync_output, valid_name};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt};

fn no_execution(arn: &str) -> AwsError {
    err(
        "ExecutionDoesNotExist",
        format!("Execution Does Not Exist: '{arn}'"),
    )
}

fn no_task() -> AwsError {
    err(
        "TaskDoesNotExist",
        "Task Timed Out: 'Provided task does not exist anymore'",
    )
}

pub(super) fn describe(ex: &Execution) -> Value {
    let mut out = json!({
        "executionArn": ex.arn,
        "stateMachineArn": ex.machine_arn,
        "name": ex.name,
        "status": ex.status,
        "startDate": secs(ex.start_ms),
        "input": ex.input,
        "inputDetails": { "included": true },
        "redriveCount": ex.redrive_count,
    });
    if let Some(s) = ex.stop_ms {
        out["stopDate"] = secs(s);
    }
    if let Some(o) = &ex.output {
        out["output"] = json!(o);
        out["outputDetails"] = json!({ "included": true });
    }
    if let Some(f) = &ex.failure {
        if !f.error.is_empty() {
            out["error"] = json!(f.error);
        }
        if !f.cause.is_empty() {
            out["cause"] = json!(f.cause);
        }
    }
    if let Some(v) = &ex.version_arn {
        out["stateMachineVersionArn"] = json!(v);
    }
    if let Some(a) = &ex.alias_arn {
        out["stateMachineAliasArn"] = json!(a);
    }
    if ex.kind == "STANDARD" {
        out["redriveStatus"] = json!(if redrivable(ex) {
            "REDRIVABLE"
        } else {
            "NOT_REDRIVABLE"
        });
    }
    out
}

fn redrivable(ex: &Execution) -> bool {
    ex.kind == "STANDARD" && matches!(ex.status.as_str(), "FAILED" | "ABORTED" | "TIMED_OUT")
}

/// Starts an execution. For `StartSyncExecution` the response waits for the
/// end when a task is still running.
pub(super) fn start(
    sfn: &mut StepFunctions,
    ctx: &mut Ctx<'_>,
    input: &Value,
    sync: bool,
) -> Result<Value, AwsError> {
    let target = input.require_str("stateMachineArn")?.to_string();
    let text = input.str_field("input").unwrap_or("{}").to_string();
    if serde_json::from_str::<Value>(&text).is_err() {
        return Err(err(
            "InvalidExecutionInput",
            "Invalid State Machine Execution Input: 'Unexpected character'",
        ));
    }
    let name = match input.str_field("name") {
        Some(n) => n.to_string(),
        None => ctx.uuid(),
    };
    valid_name(&name)?;
    let region = std::mem::take(sfn.region(ctx));
    let resolved = machines::resolve(ctx, &region, &target).map(|(m, v, alias)| {
        (
            m.arn.clone(),
            m.name.clone(),
            m.kind.clone(),
            v.map_or(&m.definition, |v| &v.definition).clone(),
            v.map_or(&m.role_arn, |v| &v.role_arn).clone(),
            v.map(|v| v.arn.clone()),
            alias,
        )
    });
    *sfn.region(ctx) = region;
    let (machine_arn, machine_name, kind, definition, role_arn, version_arn, alias_arn) = resolved?;
    if sync && kind != "EXPRESS" {
        return Err(err(
            "StateMachineTypeNotSupported",
            format!("This operation is not supported by this type of state machine: {kind}"),
        ));
    }
    let arn = if kind == "EXPRESS" {
        let id = ctx.uuid();
        ctx.arn("states", &format!("express:{machine_name}:{name}:{id}"))
    } else {
        ctx.arn("states", &format!("execution:{machine_name}:{name}"))
    };
    if let Some(existing) = sfn.region(ctx).executions.get(&arn) {
        if existing.input == text && existing.status == "RUNNING" {
            return Ok(json!({ "executionArn": arn, "startDate": secs(existing.start_ms) }));
        }
        return Err(err(
            "ExecutionAlreadyExists",
            format!("Execution Already Exists: '{arn}'"),
        ));
    }
    let mut ex = Execution::new(
        Start {
            arn: arn.clone(),
            machine_arn,
            machine_name,
            version_arn,
            alias_arn,
            name,
            kind,
            role_arn,
            input: text,
            definition: serde_json::from_str(&definition).unwrap_or(Value::Null),
        },
        ctx,
    );
    let mut fx = Vec::new();
    ex.begin(ctx, &mut fx);
    let start_ms = ex.start_ms;
    let token = (sync && ex.status == "RUNNING").then(|| ctx.uuid());
    ex.sync = token.clone();
    let done = (sync && token.is_none()).then(|| sync_output(&ex));
    sfn.region(ctx).executions.insert(arn.clone(), ex);
    sfn.effects(ctx, &arn, fx);
    if let Some(out) = done {
        return Ok(out);
    }
    if let Some(t) = token {
        ctx.awaiting = Some(t);
        return Ok(Value::Null);
    }
    Ok(json!({ "executionArn": arn, "startDate": secs(start_ms) }))
}

pub(super) fn call(
    sfn: &mut StepFunctions,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    Ok(Some(match operation {
        "StartExecution" => start(sfn, ctx, input, false)?,
        "StartSyncExecution" => start(sfn, ctx, input, true)?,
        "DescribeExecution" => {
            let arn = input.require_str("executionArn")?;
            let ex = sfn
                .region(ctx)
                .executions
                .get(arn)
                .filter(|e| !e.hidden)
                .ok_or_else(|| no_execution(arn))?;
            describe(ex)
        }
        "DescribeStateMachineForExecution" => {
            let arn = input.require_str("executionArn")?;
            let region = sfn.region(ctx);
            let ex = region
                .executions
                .get(arn)
                .filter(|e| !e.hidden)
                .ok_or_else(|| no_execution(arn))?;
            let m = region.machines.get(&ex.machine_arn);
            let mut out = json!({
                "stateMachineArn": ex.version_arn.as_ref().unwrap_or(&ex.machine_arn),
                "name": ex.machine_name,
                "definition": ex.definition.to_string(),
                "roleArn": ex.role_arn,
                "updateDate": secs(m.map_or(ex.start_ms, |m| m.updated.unwrap_or(m.created))),
            });
            if let Some(m) = m {
                out["loggingConfiguration"] = m.logging.clone();
                out["tracingConfiguration"] = m.tracing.clone();
                out["revisionId"] = json!(m.revision);
                out["encryptionConfiguration"] = m.encryption.clone();
            }
            out
        }
        "ListExecutions" => {
            let status = input.str_field("statusFilter");
            let machine = input.str_field("stateMachineArn");
            let map_run = input.str_field("mapRunArn");
            if machine.is_none() && map_run.is_none() {
                return Err(err(
                    "ValidationException",
                    "Must provide a StateMachine ARN or MapRun ARN",
                ));
            }
            let base = machine.map(split_machine_arn).transpose()?.map(|(b, _)| b);
            let region = sfn.region(ctx);
            if let Some(b) = &base
                && !region.machines.contains_key(b)
            {
                return Err(machines::no_machine(b));
            }
            let items = region
                .executions
                .values()
                .rev()
                .filter(|e| !e.hidden)
                .filter(|e| base.as_ref().is_none_or(|b| &e.machine_arn == b))
                .filter(|e| map_run.is_none_or(|m| e.map_runs.iter().any(|r| r.arn == m)))
                .filter(|e| status.is_none_or(|s| e.status == s))
                .map(|e| {
                    let mut item = json!({
                        "executionArn": e.arn,
                        "stateMachineArn": e.machine_arn,
                        "name": e.name,
                        "status": e.status,
                        "startDate": secs(e.start_ms),
                        "redriveCount": e.redrive_count,
                    });
                    if let Some(s) = e.stop_ms {
                        item["stopDate"] = secs(s);
                    }
                    if let Some(v) = &e.version_arn {
                        item["stateMachineVersionArn"] = json!(v);
                    }
                    if let Some(a) = &e.alias_arn {
                        item["stateMachineAliasArn"] = json!(a);
                    }
                    item
                })
                .collect();
            paged("executions", items, input)
        }
        "StopExecution" => {
            let arn = input.require_str("executionArn")?.to_string();
            let mut fx = Vec::new();
            let ex = sfn
                .region(ctx)
                .executions
                .get_mut(&arn)
                .ok_or_else(|| no_execution(&arn))?;
            if ex.status != "RUNNING" {
                return Err(err(
                    "ValidationException",
                    format!("Execution is not running: '{arn}'"),
                ));
            }
            ex.abort_execution(
                input.str_field("error"),
                input.str_field("cause"),
                ctx,
                &mut fx,
            );
            let stop = ex.stop_ms.unwrap_or_default();
            sfn.effects(ctx, &arn, fx);
            json!({ "stopDate": secs(stop) })
        }
        "GetExecutionHistory" => {
            let arn = input.require_str("executionArn")?;
            let ex = sfn
                .region(ctx)
                .executions
                .get(arn)
                .filter(|e| !e.hidden)
                .ok_or_else(|| no_execution(arn))?;
            let mut events = ex.events.clone();
            if input.get("reverseOrder").and_then(Value::as_bool) == Some(true) {
                events.reverse();
            }
            if input.get("includeExecutionData").and_then(Value::as_bool) == Some(false) {
                for e in &mut events {
                    strip_data(e);
                }
            }
            paged("events", events, input)
        }
        "RedriveExecution" => {
            let arn = input.require_str("executionArn")?.to_string();
            let mut fx = Vec::new();
            let ex = sfn
                .region(ctx)
                .executions
                .get_mut(&arn)
                .ok_or_else(|| no_execution(&arn))?;
            if !redrivable(ex) {
                return Err(err(
                    "ExecutionNotRedrivable",
                    format!("Execution with the ARN '{arn}' cannot be redriven"),
                ));
            }
            ex.redrive(ctx, &mut fx);
            let date = ctx.now_ms();
            sfn.effects(ctx, &arn, fx);
            json!({ "redriveDate": secs(date) })
        }
        "ListMapRuns" => {
            let arn = input.require_str("executionArn")?;
            let ex = sfn
                .region(ctx)
                .executions
                .get(arn)
                .ok_or_else(|| no_execution(arn))?;
            let items = ex
                .map_runs
                .iter()
                .map(|r| {
                    let mut v = json!({
                        "executionArn": ex.arn,
                        "mapRunArn": r.arn,
                        "stateMachineArn": ex.machine_arn,
                        "startDate": secs(r.start_ms),
                    });
                    if let Some(s) = r.stop_ms {
                        v["stopDate"] = secs(s);
                    }
                    v
                })
                .collect();
            paged("mapRuns", items, input)
        }
        "DescribeMapRun" | "UpdateMapRun" => map_run(sfn, ctx, operation, input)?,
        "TestState" => test_state(sfn, ctx, input)?,
        _ => return Ok(None),
    }))
}

fn strip_data(e: &mut Value) {
    let Some(o) = e.as_object_mut() else {
        return;
    };
    for (_, d) in o.iter_mut().filter(|(k, _)| k.ends_with("EventDetails")) {
        if let Some(d) = d.as_object_mut() {
            for k in ["input", "output", "parameters"] {
                d.remove(k);
            }
        }
    }
}

fn map_run(
    sfn: &mut StepFunctions,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> Result<Value, AwsError> {
    let arn = input.require_str("mapRunArn")?.to_string();
    let missing = || {
        err(
            "ResourceNotFound",
            format!("Map Run Does Not Exist: '{arn}'"),
        )
    };
    let region = sfn.region(ctx);
    let ex = region
        .executions
        .values_mut()
        .find(|e| e.map_runs.iter().any(|r| r.arn == arn))
        .ok_or_else(missing)?;
    let exec_arn = ex.arn.clone();
    let run = ex
        .map_runs
        .iter_mut()
        .find(|r| r.arn == arn)
        .ok_or_else(missing)?;
    if operation == "UpdateMapRun" {
        if let Some(n) = input.i64_field("maxConcurrency") {
            run.max_concurrency = n;
        }
        if let Some(n) = input.i64_field("toleratedFailureCount") {
            run.tolerated_count = n;
        }
        if let Some(p) = input
            .get("toleratedFailurePercentage")
            .and_then(Value::as_f64)
        {
            run.tolerated_percentage = p;
        }
        return Ok(json!({}));
    }
    let running = if run.status == "RUNNING" {
        run.total - run.succeeded - run.failed
    } else {
        0
    };
    let counts = json!({
        "pending": 0,
        "running": running,
        "succeeded": run.succeeded,
        "failed": run.failed,
        "timedOut": 0,
        "aborted": 0,
        "total": run.total,
        "resultsWritten": 0,
        "failuresNotRedrivable": 0,
        "pendingRedrive": 0,
    });
    let mut out = json!({
        "mapRunArn": run.arn,
        "executionArn": exec_arn,
        "status": run.status,
        "startDate": secs(run.start_ms),
        "maxConcurrency": run.max_concurrency,
        "toleratedFailurePercentage": run.tolerated_percentage,
        "toleratedFailureCount": run.tolerated_count,
        "itemCounts": counts,
        "executionCounts": counts,
        "redriveCount": 0,
    });
    if let Some(s) = run.stop_ms {
        out["stopDate"] = secs(s);
    }
    Ok(out)
}

/// `TestState`: runs one state in a hidden execution and reports where it
/// would go next.
fn test_state(
    sfn: &mut StepFunctions,
    ctx: &mut Ctx<'_>,
    input: &Value,
) -> Result<Value, AwsError> {
    let definition: Value =
        serde_json::from_str(input.require_str("definition")?).map_err(|_| {
            err(
                "InvalidDefinition",
                "Invalid State Machine Definition: 'INVALID_JSON_DESCRIPTION'",
            )
        })?;
    let text = input.str_field("input").unwrap_or("{}").to_string();
    if serde_json::from_str::<Value>(&text).is_err() {
        return Err(err("InvalidExecutionInput", "Invalid input"));
    }
    let name = input
        .str_field("stateName")
        .unwrap_or("TestState")
        .to_string();
    let mut machine = if definition.get("States").is_some() {
        if definition["States"].get(&name).is_none() {
            return Err(err(
                "InvalidDefinition",
                format!("Invalid State Machine Definition: 'State {name} not found'"),
            ));
        }
        definition
    } else {
        let mut states = serde_json::Map::new();
        states.insert(name.clone(), definition);
        json!({ "States": states })
    };
    machine["StartAt"] = json!(name);
    let id = ctx.uuid();
    let arn = ctx.arn("states", &format!("execution:TestState:{id}"));
    let mut ex = Execution::new(
        Start {
            arn: arn.clone(),
            machine_arn: ctx.arn("states", "stateMachine:TestState"),
            machine_name: "TestState".into(),
            version_arn: None,
            alias_arn: None,
            name: id,
            kind: "EXPRESS".into(),
            role_arn: input.str_field("roleArn").unwrap_or_default().to_string(),
            input: text,
            definition: machine,
        },
        ctx,
    );
    ex.hidden = true;
    ex.single = true;
    ex.mock = input.get("mock").cloned();
    let mut fx = Vec::new();
    ex.begin(ctx, &mut fx);
    let token = (ex.status == "RUNNING").then(|| ctx.uuid());
    ex.sync = token.clone();
    let done = token.is_none().then(|| test_output(&ex));
    sfn.region(ctx).executions.insert(arn.clone(), ex);
    sfn.effects(ctx, &arn, fx);
    if let Some(out) = done {
        return Ok(out);
    }
    ctx.awaiting = token;
    Ok(Value::Null)
}

pub(super) fn test_output(ex: &Execution) -> Value {
    let mut out =
        json!({ "status": if ex.status == "SUCCEEDED" { "SUCCEEDED" } else { "FAILED" } });
    if let Some(o) = &ex.output {
        out["output"] = json!(o);
    }
    if let Some(n) = &ex.next_state {
        out["nextState"] = json!(n);
    }
    if let Some(Failure { error, cause }) = &ex.failure {
        out["error"] = json!(error);
        out["cause"] = json!(cause);
    }
    out["inspectionData"] = json!({ "input": ex.input });
    out
}

pub(super) fn tasks(
    sfn: &mut StepFunctions,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    Ok(Some(match operation {
        "GetActivityTask" => {
            let arn = input.require_str("activityArn")?.to_string();
            let worker = input.str_field("workerName").map(str::to_string);
            let region = sfn.region(ctx);
            if !region.activities.contains_key(&arn) {
                return Err(err(
                    "ActivityDoesNotExist",
                    format!("Activity Does Not Exist: '{arn}'"),
                ));
            }
            loop {
                let Some((token, task_input)) = sfn
                    .region(ctx)
                    .queues
                    .get_mut(&arn)
                    .and_then(|q| q.pop_front())
                else {
                    break json!({});
                };
                let Some(t) = sfn.tokens.get(&token) else {
                    continue;
                };
                let (exec, run) = (t.execution.clone(), t.run);
                let Some(ex) = sfn.region(ctx).executions.get_mut(&exec) else {
                    continue;
                };
                if !ex.holds_token(run, &token) {
                    continue;
                }
                ex.activity_started(run, worker.as_deref(), ctx);
                break json!({ "taskToken": token, "input": task_input });
            }
        }
        "SendTaskSuccess" | "SendTaskFailure" | "SendTaskHeartbeat" => {
            let token = input.require_str("taskToken")?.to_string();
            let t = sfn.tokens.get(&token).ok_or_else(no_task)?;
            let (account, region, exec, run) = (
                t.account.clone(),
                t.region.clone(),
                t.execution.clone(),
                t.run,
            );
            let key = (account, region);
            let live = sfn
                .regions
                .get(&key)
                .and_then(|r| r.executions.get(&exec))
                .is_some_and(|e| e.holds_token(run, &token));
            if !live {
                sfn.tokens.remove(&token);
                return Err(no_task());
            }
            if operation == "SendTaskHeartbeat" {
                return Ok(Some(json!({})));
            }
            let result = if operation == "SendTaskSuccess" {
                let output = input.require_str("output")?;
                let v: Value = serde_json::from_str(output)
                    .map_err(|_| err("InvalidOutput", format!("Invalid Output: '{output}'")))?;
                Ok(v)
            } else {
                Err(Failure::new(
                    input.str_field("error").unwrap_or_default(),
                    input.str_field("cause").unwrap_or_default(),
                ))
            };
            sfn.tokens.remove(&token);
            let mut fx = Vec::new();
            if let Some(ex) = sfn
                .regions
                .get_mut(&key)
                .and_then(|r| r.executions.get_mut(&exec))
            {
                ex.resolve(run, result, ctx, &mut fx);
            }
            sfn.effects(ctx, &exec, fx);
            json!({})
        }
        _ => return Ok(None),
    }))
}
