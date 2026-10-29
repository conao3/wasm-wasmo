mod common;

use std::cell::Cell;
use std::rc::Rc;

use common::{Mock, xml_text};
use serde_json::{Value, json};

const ROLE: &str = "arn:aws:iam::000000000000:role/sfn";

fn machine(m: &mut Mock, name: &str, definition: Value, kind: &str) -> String {
    m.sfn(
        "CreateStateMachine",
        json!({ "name": name, "definition": definition.to_string(), "roleArn": ROLE, "type": kind }),
    )["stateMachineArn"]
        .as_str()
        .unwrap()
        .to_string()
}

fn run(m: &mut Mock, arn: &str, input: Value) -> Value {
    let exec = m.sfn(
        "StartExecution",
        json!({ "stateMachineArn": arn, "input": input.to_string() }),
    )["executionArn"]
        .as_str()
        .unwrap()
        .to_string();
    m.sfn("DescribeExecution", json!({ "executionArn": exec }))
}

fn output(d: &Value) -> Value {
    serde_json::from_str(d["output"].as_str().unwrap_or("null")).unwrap()
}

fn history(m: &mut Mock, exec: &str) -> Vec<String> {
    m.sfn("GetExecutionHistory", json!({ "executionArn": exec }))["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn flow_states_and_data_paths() {
    let mut m = Mock::new();
    let arn = machine(
        &mut m,
        "flow",
        json!({
            "StartAt": "Init",
            "States": {
                "Init": {
                    "Type": "Pass",
                    "Parameters": {
                        "order.$": "$.order",
                        "greeting.$": "States.Format('hi {}', $.order.customer)",
                        "exec.$": "$$.Execution.Name",
                    },
                    "ResultPath": "$.prepared",
                    "Next": "Route",
                },
                "Route": {
                    "Type": "Choice",
                    "Choices": [
                        { "And": [
                            { "Variable": "$.order.total", "NumericGreaterThanEquals": 100 },
                            { "Not": { "Variable": "$.order.vip", "BooleanEquals": false } },
                        ], "Next": "Big" },
                        { "Variable": "$.order.customer", "StringMatches": "a*", "Next": "Small" },
                    ],
                    "Default": "Reject",
                },
                "Big": { "Type": "Wait", "Seconds": 3600, "Next": "Done" },
                "Small": { "Type": "Pass", "Result": { "size": "small" }, "ResultPath": "$.size", "Next": "Done" },
                "Reject": { "Type": "Fail", "Error": "Rejected", "Cause": "no route" },
                "Done": { "Type": "Succeed", "OutputPath": "$.prepared" },
            },
        }),
        "STANDARD",
    );
    let d = run(
        &mut m,
        &arn,
        json!({ "order": { "total": 150, "vip": true, "customer": "bo" } }),
    );
    assert_eq!(d["status"], "SUCCEEDED");
    let out = output(&d);
    assert_eq!(out["greeting"], "hi bo");
    assert_eq!(out["order"]["total"], 150);
    let exec = d["executionArn"].as_str().unwrap().to_string();
    assert_eq!(out["exec"], exec.rsplit(':').next().unwrap());
    let stop = d["stopDate"].as_f64().unwrap();
    let start = d["startDate"].as_f64().unwrap();
    assert_eq!(stop - start, 3600.0, "Wait advances logical time");
    assert_eq!(
        history(&mut m, &exec),
        [
            "ExecutionStarted",
            "PassStateEntered",
            "PassStateExited",
            "ChoiceStateEntered",
            "ChoiceStateExited",
            "WaitStateEntered",
            "WaitStateExited",
            "SucceedStateEntered",
            "SucceedStateExited",
            "ExecutionSucceeded",
        ]
    );

    let d = run(
        &mut m,
        &arn,
        json!({ "order": { "total": 5, "customer": "zed" } }),
    );
    assert_eq!(d["status"], "FAILED");
    assert_eq!(d["error"], "Rejected");
    assert_eq!(d["cause"], "no route");

    let d = run(
        &mut m,
        &arn,
        json!({ "order": { "total": 5, "customer": "ann" } }),
    );
    assert_eq!(output(&d)["order"]["customer"], "ann");
}

#[test]
fn invalid_definitions_are_rejected() {
    let mut m = Mock::new();
    let (status, out) = m.json(
        "states",
        "AWSStepFunctions.CreateStateMachine",
        json!({
            "name": "bad",
            "definition": json!({ "StartAt": "A", "States": { "A": { "Type": "Pass", "Next": "B" } } }).to_string(),
            "roleArn": ROLE,
        }),
    );
    assert_eq!(status, 400);
    assert_eq!(out["__type"], "InvalidDefinition");
    let v = m.sfn(
        "ValidateStateMachineDefinition",
        json!({ "definition": "{\"StartAt\":\"X\",\"States\":{}}" }),
    );
    assert_eq!(v["result"], "FAIL");
    assert_eq!(v["diagnostics"][0]["code"], "MISSING_TRANSITION_TARGET");
}

#[test]
fn lambda_task_retries_catches_and_integrates() {
    let mut m = Mock::new();
    let flaky = m.function("flaky");
    m.function("always-fails");
    let calls = Rc::new(Cell::new(0));
    let seen = calls.clone();
    m.register("flaky", move |event| {
        seen.set(seen.get() + 1);
        if seen.get() < 3 {
            return Err(("TransientError".into(), "try again".into()));
        }
        Ok(json!({ "id": event["id"], "total": event["n"].as_i64().unwrap() * 10 }))
    });
    m.register("always-fails", |_| {
        Err(("FatalError".into(), "nope".into()))
    });
    m.ok(
        "dynamodb",
        "DynamoDB_20120810.CreateTable",
        json!({
            "TableName": "orders",
            "AttributeDefinitions": [{ "AttributeName": "id", "AttributeType": "S" }],
            "KeySchema": [{ "AttributeName": "id", "KeyType": "HASH" }],
            "BillingMode": "PAY_PER_REQUEST",
        }),
    );
    let topic = xml_text(
        &m.query(
            "sns",
            &[
                ("Action", "CreateTopic"),
                ("Name", "done"),
                ("Version", "2010-03-31"),
            ],
        ),
        "TopicArn",
    );
    let (url, queue_arn) = m.queue("done-q");
    m.query(
        "sns",
        &[
            ("Action", "Subscribe"),
            ("TopicArn", &topic),
            ("Protocol", "sqs"),
            ("Endpoint", &queue_arn),
            ("Attributes.entry.1.key", "RawMessageDelivery"),
            ("Attributes.entry.1.value", "true"),
            ("Version", "2010-03-31"),
        ],
    );
    let arn = machine(
        &mut m,
        "orders",
        json!({
            "StartAt": "Compute",
            "States": {
                "Compute": {
                    "Type": "Task",
                    "Resource": "arn:aws:states:::lambda:invoke",
                    "Parameters": { "FunctionName": flaky, "Payload.$": "$" },
                    "ResultSelector": { "total.$": "$.Payload.total" },
                    "ResultPath": "$.result",
                    "Retry": [{ "ErrorEquals": ["TransientError"], "IntervalSeconds": 2, "BackoffRate": 3, "MaxAttempts": 3 }],
                    "Next": "Risky",
                },
                "Risky": {
                    "Type": "Task",
                    "Resource": "arn:aws:lambda:us-east-1:000000000000:function:always-fails",
                    "Retry": [{ "ErrorEquals": ["States.ALL"], "MaxAttempts": 1 }],
                    "Catch": [{ "ErrorEquals": ["FatalError"], "ResultPath": "$.error", "Next": "Save" }],
                    "Next": "Save",
                },
                "Save": {
                    "Type": "Task",
                    "Resource": "arn:aws:states:::dynamodb:putItem",
                    "Parameters": {
                        "TableName": "orders",
                        "Item": { "id": { "S.$": "$.id" }, "total": { "N.$": "States.Format('{}', $.result.total)" } },
                    },
                    "ResultPath": null,
                    "Next": "Notify",
                },
                "Notify": {
                    "Type": "Task",
                    "Resource": "arn:aws:states:::sns:publish",
                    "Parameters": { "TopicArn": topic, "Message.$": "$.result" },
                    "ResultSelector": { "id.$": "$.MessageId" },
                    "ResultPath": "$.published",
                    "End": true,
                },
            },
        }),
        "STANDARD",
    );
    let d = run(&mut m, &arn, json!({ "id": "o-1", "n": 4 }));
    assert_eq!(d["status"], "SUCCEEDED", "{d}");
    let out = output(&d);
    assert_eq!(out["result"], json!({ "total": 40 }));
    assert_eq!(out["error"]["Error"], "FatalError");
    assert!(out["error"]["Cause"].as_str().unwrap().contains("nope"));
    assert!(out["published"]["id"].is_string());
    assert_eq!(calls.get(), 3);
    assert_eq!(
        m.invocations
            .iter()
            .filter(|(n, _)| n == "always-fails")
            .count(),
        2
    );
    let start = d["startDate"].as_f64().unwrap();
    assert_eq!(d["stopDate"].as_f64().unwrap() - start, 2.0 + 6.0 + 1.0);

    let item = m.ok(
        "dynamodb",
        "DynamoDB_20120810.GetItem",
        json!({ "TableName": "orders", "Key": { "id": { "S": "o-1" } } }),
    );
    assert_eq!(item["Item"]["total"], json!({ "N": "40" }));
    let msgs = m.receive(&url);
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["Body"], "{\"total\":40}");

    let events = history(&mut m, d["executionArn"].as_str().unwrap());
    assert_eq!(events.iter().filter(|e| *e == "TaskFailed").count(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|e| *e == "LambdaFunctionFailed")
            .count(),
        2
    );
    assert!(events.contains(&"TaskSucceeded".to_string()));
}

#[test]
fn sync_execution_waits_for_the_handler() {
    let mut m = Mock::new();
    let f = m.function("double");
    m.register("double", |e| Ok(json!(e["x"].as_i64().unwrap() * 2)));
    let arn = machine(
        &mut m,
        "express",
        json!({
            "StartAt": "Fan",
            "States": {
                "Fan": {
                    "Type": "Map",
                    "ItemsPath": "$.xs",
                    "MaxConcurrency": 2,
                    "ItemSelector": { "x.$": "$$.Map.Item.Value", "i.$": "$$.Map.Item.Index" },
                    "ItemProcessor": {
                        "StartAt": "Double",
                        "States": { "Double": { "Type": "Task", "Resource": f, "End": true } },
                    },
                    "Next": "Both",
                },
                "Both": {
                    "Type": "Parallel",
                    "Branches": [
                        { "StartAt": "Sum", "States": { "Sum": { "Type": "Pass", "Parameters": { "n.$": "States.ArrayLength($)" }, "End": true } } },
                        { "StartAt": "Again", "States": { "Again": {
                            "Type": "Task",
                            "Resource": "arn:aws:states:::lambda:invoke",
                            "Parameters": { "FunctionName": "double", "Payload": { "x.$": "$[0]" } },
                            "OutputPath": "$.Payload",
                            "End": true,
                        } } },
                    ],
                    "End": true,
                },
            },
        }),
        "EXPRESS",
    );
    let out = m.sfn(
        "StartSyncExecution",
        json!({ "stateMachineArn": arn, "input": json!({ "xs": [1, 2, 3, 4, 5] }).to_string() }),
    );
    assert_eq!(out["status"], "SUCCEEDED", "{out}");
    assert_eq!(output(&out), json!([{ "n": 5 }, 4]));
    assert_eq!(m.invocations.len(), 6);

    let standard = machine(
        &mut m,
        "std",
        json!({"StartAt":"P","States":{"P":{"Type":"Pass","End":true}}}),
        "STANDARD",
    );
    let (status, err) = m.json(
        "states",
        "AWSStepFunctions.StartSyncExecution",
        json!({ "stateMachineArn": standard }),
    );
    assert_eq!(status, 400);
    assert_eq!(err["__type"], "StateMachineTypeNotSupported");
}

#[test]
fn activities_and_task_tokens() {
    let mut m = Mock::new();
    let activity = m.sfn("CreateActivity", json!({ "name": "approve" }))["activityArn"]
        .as_str()
        .unwrap()
        .to_string();
    let (url, _) = m.queue("tokens");
    let arn = machine(
        &mut m,
        "approval",
        json!({
            "StartAt": "Ask",
            "States": {
                "Ask": { "Type": "Task", "Resource": activity, "ResultPath": "$.approved", "Next": "Queue" },
                "Queue": {
                    "Type": "Task",
                    "Resource": "arn:aws:states:::sqs:sendMessage.waitForTaskToken",
                    "Parameters": { "QueueUrl": url, "MessageBody": { "token.$": "$$.Task.Token", "who.$": "$.who" } },
                    "ResultPath": "$.reply",
                    "End": true,
                },
            },
        }),
        "STANDARD",
    );
    let exec = m.sfn(
        "StartExecution",
        json!({ "stateMachineArn": arn, "input": "{\"who\":\"kim\"}" }),
    )["executionArn"]
        .as_str()
        .unwrap()
        .to_string();
    let task = m.sfn(
        "GetActivityTask",
        json!({ "activityArn": activity, "workerName": "w1" }),
    );
    assert_eq!(task["input"], "{\"who\":\"kim\"}");
    let token = task["taskToken"].as_str().unwrap().to_string();
    assert_eq!(
        m.sfn("GetActivityTask", json!({ "activityArn": activity })),
        json!({})
    );
    m.sfn("SendTaskHeartbeat", json!({ "taskToken": token }));
    m.sfn(
        "SendTaskSuccess",
        json!({ "taskToken": token, "output": "true" }),
    );
    let (status, e) = m.json(
        "states",
        "AWSStepFunctions.SendTaskSuccess",
        json!({ "taskToken": token, "output": "1" }),
    );
    assert_eq!(
        (status, e["__type"].as_str()),
        (400, Some("TaskDoesNotExist"))
    );

    let msgs = m.receive(&url);
    let body: Value = serde_json::from_str(msgs[0]["Body"].as_str().unwrap()).unwrap();
    assert_eq!(body["who"], "kim");
    let d = m.sfn("DescribeExecution", json!({ "executionArn": exec }));
    assert_eq!(d["status"], "RUNNING");
    m.sfn(
        "SendTaskFailure",
        json!({ "taskToken": body["token"], "error": "Denied", "cause": "manager said no" }),
    );
    let d = m.sfn("DescribeExecution", json!({ "executionArn": exec }));
    assert_eq!(
        (d["status"].as_str(), d["error"].as_str()),
        (Some("FAILED"), Some("Denied"))
    );
    let events = history(&mut m, &exec);
    assert!(events.contains(&"ActivityStarted".to_string()));
    assert!(events.contains(&"TaskSubmitted".to_string()));
}

#[test]
fn nested_executions_sdk_integrations_and_item_reader() {
    let mut m = Mock::new();
    let child = machine(
        &mut m,
        "child",
        json!({ "StartAt": "P", "States": { "P": { "Type": "Pass", "Parameters": { "doubled.$": "States.MathAdd($.n, $.n)" }, "End": true } } }),
        "STANDARD",
    );
    m.rest("s3", "PUT", "/items", Value::Null);
    let res = m.request(
        "s3",
        "PUT",
        "/items/list.csv",
        vec![],
        b"name,qty\napple,2\npear,5\n".to_vec(),
    );
    assert_eq!(res.status, 200);
    let parent = machine(
        &mut m,
        "parent",
        json!({
            "StartAt": "Child",
            "States": {
                "Child": {
                    "Type": "Task",
                    "Resource": "arn:aws:states:::states:startExecution.sync:2",
                    "Parameters": { "StateMachineArn": child, "Input": { "n.$": "$.n" } },
                    "ResultSelector": { "out.$": "$.Output", "status.$": "$.Status" },
                    "ResultPath": "$.child",
                    "Next": "Describe",
                },
                "Describe": {
                    "Type": "Task",
                    "Resource": "arn:aws:states:::aws-sdk:sfn:describeStateMachine",
                    "Parameters": { "StateMachineArn": child },
                    "ResultSelector": { "name.$": "$.Name", "type.$": "$.Type" },
                    "ResultPath": "$.described",
                    "Next": "Missing",
                },
                "Missing": {
                    "Type": "Task",
                    "Resource": "arn:aws:states:::aws-sdk:sfn:describeStateMachine",
                    "Parameters": { "StateMachineArn": "arn:aws:states:us-east-1:000000000000:stateMachine:nope" },
                    "Catch": [{ "ErrorEquals": ["Sfn.StateMachineDoesNotExistException"], "ResultPath": "$.missing", "Next": "Read" }],
                    "Next": "Read",
                },
                "Read": {
                    "Type": "Map",
                    "ItemReader": {
                        "Resource": "arn:aws:states:::s3:getObject",
                        "ReaderConfig": { "InputType": "CSV", "CSVHeaderLocation": "FIRST_ROW" },
                        "Parameters": { "Bucket": "items", "Key": "list.csv" },
                    },
                    "ItemProcessor": {
                        "ProcessorConfig": { "Mode": "DISTRIBUTED", "ExecutionType": "STANDARD" },
                        "StartAt": "Name",
                        "States": { "Name": { "Type": "Pass", "OutputPath": "$.name", "End": true } },
                    },
                    "ResultPath": "$.names",
                    "End": true,
                },
            },
        }),
        "STANDARD",
    );
    let d = run(&mut m, &parent, json!({ "n": 21 }));
    assert_eq!(d["status"], "SUCCEEDED", "{d}");
    let out = output(&d);
    assert_eq!(
        out["child"],
        json!({ "out": { "doubled": 42 }, "status": "SUCCEEDED" })
    );
    assert_eq!(
        out["described"],
        json!({ "name": "child", "type": "STANDARD" })
    );
    assert!(
        out["missing"]["Error"]
            .as_str()
            .unwrap()
            .starts_with("Sfn.")
    );
    assert_eq!(out["names"], json!(["apple", "pear"]));
    let runs = m.sfn("ListMapRuns", json!({ "executionArn": d["executionArn"] }));
    let map_run = runs["mapRuns"][0]["mapRunArn"]
        .as_str()
        .unwrap()
        .to_string();
    let mr = m.sfn("DescribeMapRun", json!({ "mapRunArn": map_run }));
    assert_eq!(mr["status"], "SUCCEEDED");
    assert_eq!(mr["itemCounts"]["succeeded"], 2);
    let listed = m.sfn("ListExecutions", json!({ "stateMachineArn": child }));
    assert_eq!(listed["executions"].as_array().unwrap().len(), 1);
}

#[test]
fn versions_aliases_stop_redrive_and_events() {
    let mut m = Mock::new();
    let created = m.sfn(
        "CreateStateMachine",
        json!({
            "name": "v",
            "definition": json!({"StartAt":"P","States":{"P":{"Type":"Pass","Result":1,"End":true}}}).to_string(),
            "roleArn": ROLE,
            "publish": true,
        }),
    );
    let arn = created["stateMachineArn"].as_str().unwrap().to_string();
    let v1 = created["stateMachineVersionArn"]
        .as_str()
        .unwrap()
        .to_string();
    let updated = m.sfn(
        "UpdateStateMachine",
        json!({
            "stateMachineArn": arn,
            "definition": json!({"StartAt":"P","States":{"P":{"Type":"Pass","Result":2,"End":true}}}).to_string(),
            "publish": true,
        }),
    );
    let v2 = updated["stateMachineVersionArn"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(v2, format!("{arn}:2"));
    let alias = m.sfn(
        "CreateStateMachineAlias",
        json!({ "name": "live", "routingConfiguration": [{ "stateMachineVersionArn": v1, "weight": 100 }] }),
    )["stateMachineAliasArn"]
        .as_str()
        .unwrap()
        .to_string();
    let d = run(&mut m, &alias, json!({}));
    assert_eq!(output(&d), json!(1));
    assert_eq!(d["stateMachineAliasArn"], alias);
    assert_eq!(output(&run(&mut m, &v2, json!({}))), json!(2));
    assert_eq!(output(&run(&mut m, &arn, json!({}))), json!(2));
    let versions = m.sfn(
        "ListStateMachineVersions",
        json!({ "stateMachineArn": arn }),
    );
    assert_eq!(
        versions["stateMachineVersions"][0]["stateMachineVersionArn"],
        v2
    );

    let (url, queue_arn) = m.queue("status");
    m.ok(
        "events",
        "AWSEvents.PutRule",
        json!({ "Name": "sfn", "EventPattern": json!({ "source": ["aws.states"], "detail": { "status": ["ABORTED"] } }).to_string() }),
    );
    m.ok(
        "events",
        "AWSEvents.PutTargets",
        json!({ "Rule": "sfn", "Targets": [{ "Id": "q", "Arn": queue_arn }] }),
    );
    let waiting = machine(
        &mut m,
        "waiting",
        json!({ "StartAt": "T", "States": { "T": { "Type": "Task", "Resource": "arn:aws:states:::sqs:sendMessage.waitForTaskToken", "Parameters": { "QueueUrl": "http://wasmo.local/000000000000/nowhere", "MessageBody.$": "$$.Task.Token" }, "End": true } } }),
        "STANDARD",
    );
    let d = run(&mut m, &waiting, json!({}));
    assert_eq!(d["status"], "FAILED");
    assert_eq!(d["error"], "SQS.QueueDoesNotExistException");
    let exec = d["executionArn"].as_str().unwrap().to_string();
    let (status, e) = m.json(
        "states",
        "AWSStepFunctions.StopExecution",
        json!({ "executionArn": exec }),
    );
    assert_eq!(
        (status, e["__type"].as_str()),
        (400, Some("ValidationException"))
    );
    m.sfn("RedriveExecution", json!({ "executionArn": exec }));
    let d = m.sfn("DescribeExecution", json!({ "executionArn": exec }));
    assert_eq!(
        (d["status"].as_str(), d["redriveCount"].as_i64()),
        (Some("FAILED"), Some(1))
    );

    let (q2, _) = m.queue("nowhere");
    let d = run(&mut m, &waiting, json!({}));
    assert_eq!(d["status"], "RUNNING");
    let exec = d["executionArn"].as_str().unwrap().to_string();
    assert_eq!(m.receive(&q2).len(), 1);
    m.sfn(
        "StopExecution",
        json!({ "executionArn": exec, "error": "Halt", "cause": "test" }),
    );
    let d = m.sfn("DescribeExecution", json!({ "executionArn": exec }));
    assert_eq!(
        (d["status"].as_str(), d["error"].as_str()),
        (Some("ABORTED"), Some("Halt"))
    );
    let msgs = m.receive(&url);
    assert_eq!(msgs.len(), 1);
    let event: Value = serde_json::from_str(msgs[0]["Body"].as_str().unwrap()).unwrap();
    assert_eq!(
        event["detail-type"],
        "Step Functions Execution Status Change"
    );
    assert_eq!(event["detail"]["executionArn"], exec);

    m.sfn(
        "TagResource",
        json!({ "resourceArn": arn, "tags": [{ "key": "team", "value": "core" }] }),
    );
    assert_eq!(
        m.sfn("ListTagsForResource", json!({ "resourceArn": arn }))["tags"],
        json!([{ "key": "team", "value": "core" }])
    );
}

#[test]
fn eventbridge_targets_start_executions_and_test_state() {
    let mut m = Mock::new();
    let arn = machine(
        &mut m,
        "on-event",
        json!({ "StartAt": "P", "States": { "P": { "Type": "Pass", "InputPath": "$.detail", "End": true } } }),
        "STANDARD",
    );
    m.ok(
        "events",
        "AWSEvents.PutRule",
        json!({ "Name": "r", "EventPattern": "{\"source\":[\"app\"]}" }),
    );
    m.ok(
        "events",
        "AWSEvents.PutTargets",
        json!({ "Rule": "r", "Targets": [{ "Id": "sfn", "Arn": arn, "RoleArn": ROLE }] }),
    );
    m.ok(
        "events",
        "AWSEvents.PutEvents",
        json!({ "Entries": [{ "Source": "app", "DetailType": "t", "Detail": "{\"k\":7}" }] }),
    );
    let listed = m.sfn("ListExecutions", json!({ "stateMachineArn": arn }));
    let exec = listed["executions"][0]["executionArn"].clone();
    let d = m.sfn("DescribeExecution", json!({ "executionArn": exec }));
    assert_eq!(output(&d), json!({ "k": 7 }));

    let t = m.sfn(
        "TestState",
        json!({
            "definition": json!({ "Type": "Choice", "Choices": [{ "Variable": "$.n", "NumericGreaterThan": 1, "Next": "Big" }], "Default": "Small" }).to_string(),
            "input": "{\"n\":2}",
            "roleArn": ROLE,
        }),
    );
    assert_eq!(
        (t["status"].as_str(), t["nextState"].as_str()),
        (Some("SUCCEEDED"), Some("Big"))
    );
    let t = m.sfn(
        "TestState",
        json!({
            "definition": json!({ "Type": "Task", "Resource": "arn:aws:states:::lambda:invoke", "Parameters": { "FunctionName": "f", "Payload.$": "$" }, "ResultSelector": { "v.$": "$.Payload" }, "Next": "After" }).to_string(),
            "input": "{}",
            "mock": { "result": "{\"Payload\":3}" },
        }),
    );
    assert_eq!(t["output"], "{\"v\":3}");
    assert_eq!(t["nextState"], "After");
}
