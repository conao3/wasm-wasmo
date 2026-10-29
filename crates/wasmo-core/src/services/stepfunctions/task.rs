//! Task state resources: optimized integrations, `aws-sdk:*`, Lambda
//! function ARNs, activities, and nested executions, mapped onto
//! cross-service calls.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use super::asl::{Failure, to_text};
use crate::codec::AwsError;
use crate::service::{Arn, OpResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pattern {
    Request,
    Sync,
    Sync2,
    Token,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// `arn:aws:states:::lambda:invoke`: the Invoke output with the payload.
    LambdaInvoke,
    /// A function ARN as the resource: the payload alone.
    LambdaArn,
    /// An optimized integration whose output is the API output.
    Service {
        prefix: &'static str,
    },
    /// `aws-sdk:<service>:<action>`.
    Sdk {
        prefix: String,
    },
    Activity,
    /// `states:startExecution` with `.sync` / `.sync:2`, run by the service.
    Child {
        json: bool,
    },
}

/// What a Task state does with its effective parameters.
#[derive(Debug, Clone)]
pub struct Plan {
    pub kind: Kind,
    pub pattern: Pattern,
    /// `resourceType` and `resource` of the TaskScheduled event.
    pub resource_type: String,
    pub resource: String,
    pub call: Option<Request>,
}

#[derive(Debug, Clone)]
pub struct Request {
    pub service: String,
    pub operation: String,
    pub input: Value,
    pub sdk: bool,
}

fn split_pattern(action: &str) -> (&str, Pattern) {
    for (suffix, p) in [
        (".sync:2", Pattern::Sync2),
        (".sync", Pattern::Sync),
        (".waitForTaskToken", Pattern::Token),
    ] {
        if let Some(a) = action.strip_suffix(suffix) {
            return (a, p);
        }
    }
    (action, Pattern::Request)
}

fn pascal(action: &str) -> String {
    let mut c = action.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

/// Model name and error prefix of an `aws-sdk:<service>` name.
fn sdk_service(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name {
        "dynamodb" => ("dynamodb", "DynamoDb"),
        "sqs" => ("sqs", "Sqs"),
        "sns" => ("sns", "Sns"),
        "s3" => ("s3", "S3"),
        "lambda" => ("lambda", "Lambda"),
        "sfn" => ("stepfunctions", "Sfn"),
        "eventbridge" => ("events", "EventBridge"),
        "kms" => ("kms", "Kms"),
        "ssm" => ("ssm", "Ssm"),
        "secretsmanager" => ("secretsmanager", "SecretsManager"),
        "kinesis" => ("kinesis", "Kinesis"),
        "firehose" => ("firehose", "Firehose"),
        "iam" => ("iam", "Iam"),
        "sts" => ("sts", "Sts"),
        "cloudwatch" => ("cloudwatch", "CloudWatch"),
        "cloudwatchlogs" => ("logs", "CloudWatchLogs"),
        "apigateway" => ("apigateway", "ApiGateway"),
        "apigatewayv2" => ("apigatewayv2", "ApiGatewayV2"),
        _ => return None,
    })
}

fn unsupported(resource: &str) -> Failure {
    Failure::runtime(format!(
        "wasmo: the Task resource '{resource}' is not supported"
    ))
}

fn as_text(v: &mut Value, key: &str) {
    if let Some(x) = v.get_mut(key)
        && !x.is_string()
        && !x.is_null()
    {
        *x = Value::String(x.to_string());
    }
}

fn call(service: &str, operation: &str, input: Value) -> Option<Request> {
    Some(Request {
        service: service.to_string(),
        operation: operation.to_string(),
        input,
        sdk: false,
    })
}

pub fn plan(resource: &str, params: &Value) -> Result<Plan, Failure> {
    let arn = Arn::parse(resource).ok_or_else(|| unsupported(resource))?;
    if arn.service == "lambda" {
        return Ok(Plan {
            kind: Kind::LambdaArn,
            pattern: Pattern::Request,
            resource_type: "lambda".into(),
            resource: resource.to_string(),
            call: call(
                "lambda",
                "Invoke",
                json!({
                    "FunctionName": resource,
                    "Payload": STANDARD.encode(params.to_string()),
                }),
            ),
        });
    }
    if arn.service != "states" {
        return Err(unsupported(resource));
    }
    if arn.resource.starts_with("activity:") {
        return Ok(Plan {
            kind: Kind::Activity,
            pattern: Pattern::Token,
            resource_type: "activity".into(),
            resource: resource.to_string(),
            call: None,
        });
    }
    let (service, action) = arn
        .resource
        .split_once(':')
        .ok_or_else(|| unsupported(resource))?;
    if service == "aws-sdk" {
        let (svc, action) = action
            .split_once(':')
            .ok_or_else(|| unsupported(resource))?;
        let (action, pattern) = split_pattern(action);
        let (model, prefix) = sdk_service(svc).ok_or_else(|| unsupported(resource))?;
        if !matches!(pattern, Pattern::Request | Pattern::Token) {
            return Err(unsupported(resource));
        }
        return Ok(Plan {
            kind: Kind::Sdk {
                prefix: prefix.to_string(),
            },
            pattern,
            resource_type: format!("aws-sdk:{svc}"),
            resource: action.to_string(),
            call: Some(Request {
                service: model.to_string(),
                operation: pascal(action),
                input: params.clone(),
                sdk: true,
            }),
        });
    }
    let (action, pattern) = split_pattern(action);
    let mut input = params.clone();
    let (kind, request) = match (service, action) {
        ("lambda", "invoke") => {
            let payload = params.get("Payload").cloned().unwrap_or(json!({}));
            let mut i = json!({
                "FunctionName": params.get("FunctionName").and_then(Value::as_str).ok_or_else(|| {
                    Failure::runtime("The field 'FunctionName' is required for lambda:invoke")
                })?,
                "Payload": STANDARD.encode(payload.to_string()),
            });
            for k in ["Qualifier", "InvocationType", "ClientContext", "LogType"] {
                if let Some(v) = params.get(k) {
                    i[k] = v.clone();
                }
            }
            if pattern == Pattern::Token {
                i["InvocationType"] = json!("Event");
            }
            (Kind::LambdaInvoke, call("lambda", "Invoke", i))
        }
        ("sqs", "sendMessage") => {
            as_text(&mut input, "MessageBody");
            (
                Kind::Service { prefix: "SQS" },
                call("sqs", "SendMessage", input),
            )
        }
        ("sns", "publish") => {
            as_text(&mut input, "Message");
            (
                Kind::Service { prefix: "SNS" },
                call("sns", "Publish", input),
            )
        }
        ("dynamodb", a @ ("getItem" | "putItem" | "updateItem" | "deleteItem")) => (
            Kind::Service { prefix: "DynamoDB" },
            call("dynamodb", &pascal(a), input),
        ),
        ("events", "putEvents") => {
            if let Some(entries) = input.get_mut("Entries").and_then(Value::as_array_mut) {
                for e in entries {
                    as_text(e, "Detail");
                }
            }
            (
                Kind::Service {
                    prefix: "EventBridge",
                },
                call("events", "PutEvents", input),
            )
        }
        ("states", "startExecution") => {
            let mut i = json!({
                "stateMachineArn": params.get("StateMachineArn").and_then(Value::as_str).ok_or_else(|| {
                    Failure::runtime("The field 'StateMachineArn' is required for states:startExecution")
                })?,
                "input": params.get("Input").map(to_text).unwrap_or_else(|| "{}".into()),
            });
            if let Some(n) = params.get("Name") {
                i["name"] = n.clone();
            }
            match pattern {
                Pattern::Sync | Pattern::Sync2 => (
                    Kind::Child {
                        json: pattern == Pattern::Sync2,
                    },
                    call("stepfunctions", "StartExecution", i),
                ),
                _ => (
                    Kind::Service {
                        prefix: "StepFunctions",
                    },
                    call("stepfunctions", "StartExecution", i),
                ),
            }
        }
        _ => return Err(unsupported(resource)),
    };
    if matches!(pattern, Pattern::Sync | Pattern::Sync2) && !matches!(kind, Kind::Child { .. }) {
        return Err(unsupported(resource));
    }
    Ok(Plan {
        kind,
        pattern,
        resource_type: service.to_string(),
        resource: arn.resource[service.len() + 1..].to_string(),
        call: request,
    })
}

/// Names the error as the AWS SDK for Java exception class: `<prefix>.<code>`
/// with an `Exception` suffix.
fn service_error(prefix: &str, e: &AwsError, service: &str) -> Failure {
    let class = if e.code.ends_with("Exception") {
        e.code.clone()
    } else {
        format!("{}Exception", e.code)
    };
    Failure::new(
        format!("{prefix}.{class}"),
        format!(
            "{} (Service: {service}, Status Code: {}, Request ID: 00000000-0000-0000-0000-000000000000)",
            e.message, e.status
        ),
    )
}

/// Turns the callee's answer into the task result.
pub fn finish(kind: &Kind, result: OpResult) -> Result<Value, Failure> {
    match kind {
        Kind::LambdaInvoke | Kind::LambdaArn => {
            let out = result.map_err(|e| service_error("Lambda", &e, "AWSLambda"))?;
            let payload = out
                .get("Payload")
                .and_then(Value::as_str)
                .and_then(|p| STANDARD.decode(p).ok())
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&payload).into_owned();
            let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if out.get("FunctionError").is_some() {
                let error = value
                    .get("errorType")
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown")
                    .to_string();
                return Err(Failure::new(error, text));
            }
            if *kind == Kind::LambdaArn {
                return Ok(value);
            }
            let version = out
                .get("ExecutedVersion")
                .cloned()
                .unwrap_or(json!("$LATEST"));
            Ok(json!({
                "ExecutedVersion": version,
                "Payload": value,
                "SdkHttpMetadata": {
                    "AllHttpHeaders": {
                        "Content-Type": ["application/json"],
                        "X-Amz-Executed-Version": [version],
                    },
                    "HttpHeaders": {
                        "Content-Type": "application/json",
                        "X-Amz-Executed-Version": version,
                    },
                    "HttpStatusCode": 200,
                },
                "SdkResponseMetadata": { "RequestId": "00000000-0000-0000-0000-000000000000" },
                "StatusCode": out.get("StatusCode").cloned().unwrap_or(json!(200)),
            }))
        }
        Kind::Service { prefix } => result
            .map(|out| {
                if *prefix == "StepFunctions" {
                    json!({
                        "ExecutionArn": out.get("executionArn"),
                        "StartDate": out.get("startDate"),
                    })
                } else {
                    out
                }
            })
            .map_err(|e| service_error(prefix, &e, prefix)),
        Kind::Sdk { prefix } => result.map_err(|e| service_error(prefix, &e, prefix)),
        Kind::Activity | Kind::Child { .. } => result.map_err(|e| Failure::new(e.code, e.message)),
    }
}
