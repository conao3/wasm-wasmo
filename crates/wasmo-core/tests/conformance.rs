//! Runs botocore's protocol test suite in the server direction:
//! `input/*` cases parse the serialized request and compare against
//! `params`; `output/*` cases serialize `result` (or the error) and compare
//! against the expected response. Cases listed in `conformance-passing.txt`
//! must keep passing; run with `UPDATE_CONFORMANCE=1` to rewrite the list.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use wasmo_core::codec::AwsError;
use wasmo_core::codec::json;
use wasmo_core::codec::query::{self, Dialect};
use wasmo_core::model::{Metadata, Operation, Registry};
use wasmo_core::xml::{self, Element};

const FILES: &[&str] = &[
    "input/query.json",
    "input/ec2.json",
    "input/json.json",
    "input/json_1_0.json",
    "input/rest-json.json",
    "input/rest-xml.json",
    "output/query.json",
    "output/ec2.json",
    "output/json.json",
    "output/json_1_0.json",
    "output/rest-json.json",
    "output/rest-xml.json",
];

fn vendor_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/botocore-protocol-tests")
}

fn passing_file() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/conformance-passing.txt")
}

fn normalize(v: &Value) -> Value {
    match v {
        Value::Number(n) => match n.as_f64() {
            Some(f) if n.as_i64().is_none() && f.fract() == 0.0 && f.abs() < 1e15 => {
                Value::from(f as i64)
            }
            _ => v.clone(),
        },
        Value::Array(a) => Value::Array(a.iter().map(normalize).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, v)| (k.clone(), normalize(v))).collect())
        }
        _ => v.clone(),
    }
}

const IGNORED_ELEMENTS: &[&str] = &["RequestId", "requestId", "RequestID", "ResponseMetadata"];

fn canonical(el: &Element) -> Element {
    let mut attrs = el.attrs.clone();
    attrs.sort();
    Element {
        name: el.name.clone(),
        attrs,
        text: el.text.trim().to_string(),
        children: el
            .children
            .iter()
            .filter(|c| !IGNORED_ELEMENTS.contains(&c.name.as_str()))
            .map(canonical)
            .collect(),
    }
}

fn compare_xml(actual: &Element, expected: &str) -> Result<(), String> {
    let expected =
        xml::parse(expected.as_bytes()).map_err(|e| format!("expected body is not xml: {e}"))?;
    let (a, e) = (canonical(actual), canonical(&expected));
    if a == e {
        Ok(())
    } else {
        Err(format!(
            "xml mismatch\n  actual:   {}\n  expected: {}",
            a.to_xml(),
            e.to_xml()
        ))
    }
}

fn form_body(case: &Value) -> Vec<(String, String)> {
    let body = case["serialized"]["body"].as_str().unwrap_or_default();
    wasmo_core::query::parse_form(body.as_bytes())
}

fn run_case(direction: &str, meta: &Metadata, reg: &Registry, case: &Value) -> Result<(), String> {
    let op: Operation =
        serde_json::from_value(case["given"].clone()).map_err(|e| format!("given: {e}"))?;
    let dialect = match meta.protocol.as_str() {
        "query" => Dialect::Query,
        "ec2" => Dialect::Ec2,
        "json" => return run_json_case(direction, reg, &op, case),
        other => return Err(format!("protocol {other} not implemented")),
    };
    if direction == "input" {
        let shape = op.input.as_ref().map(|i| reg.shape(&i.shape));
        let actual = normalize(&query::parse_input(&form_body(case), reg, shape, dialect));
        let expected = normalize(
            case.get("params")
                .unwrap_or(&Value::Object(Default::default())),
        );
        return if actual == expected {
            Ok(())
        } else {
            Err(format!(
                "params mismatch\n  actual:   {actual}\n  expected: {expected}"
            ))
        };
    }
    let expected_body = case["response"]["body"].as_str().unwrap_or_default();
    if let Some(code) = case.get("errorCode").and_then(Value::as_str) {
        let error_shape_name = op
            .errors
            .iter()
            .map(|e| e.shape.as_str())
            .find(|name| query::error_code(name, reg.shape(name)) == code)
            .or_else(|| op.errors.first().map(|e| e.shape.as_str()));
        let mut data = case["error"].as_object().cloned().unwrap_or_default();
        let message = case
            .get("errorMessage")
            .and_then(Value::as_str)
            .or_else(|| data.get("Message").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string();
        data.remove("Message");
        let mut error = AwsError::new(
            case["response"]["status_code"].as_u64().unwrap_or(400) as u16,
            code,
            message,
        );
        *error.data = data;
        let shape = error_shape_name.map(|n| reg.shape(n));
        if let Some(shape) = shape {
            error.sender_fault = shape
                .error
                .as_ref()
                .is_none_or(|e| e.sender_fault || e.http_status_code.is_none_or(|s| s < 500));
        }
        let el = query::serialize_error(reg, &error, shape, "request-id", dialect);
        return compare_xml(&el, expected_body);
    }
    let ns = meta
        .xml_namespace
        .as_deref()
        .unwrap_or("https://example.com/");
    let result = case
        .get("result")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let el = query::serialize_output(reg, &op, &result, Some(ns), "request-id", dialect);
    if expected_body.trim().is_empty() {
        return Ok(());
    }
    compare_xml(&el, expected_body)
}

#[test]
fn botocore_protocol_conformance() {
    let mut passing = BTreeSet::new();
    let mut failures = Vec::new();
    let mut summary = Vec::new();
    for file in FILES {
        let direction = file.split('/').next().unwrap();
        let suites: Vec<Value> =
            serde_json::from_str(&fs::read_to_string(vendor_dir().join(file)).unwrap()).unwrap();
        let (mut pass, mut total) = (0, 0);
        for suite in &suites {
            let meta: Metadata = serde_json::from_value(suite["metadata"].clone()).unwrap();
            let reg = Registry::from_value(
                suite
                    .get("shapes")
                    .unwrap_or(&Value::Object(Default::default())),
            )
            .unwrap();
            for case in suite["cases"].as_array().unwrap() {
                total += 1;
                let id = format!("{file}:{}", case["id"].as_str().unwrap());
                let outcome = std::panic::catch_unwind(|| run_case(direction, &meta, &reg, case))
                    .unwrap_or_else(|_| Err("panicked".to_string()));
                match outcome {
                    Ok(()) => {
                        pass += 1;
                        passing.insert(id);
                    }
                    Err(e) => failures.push((id, e)),
                }
            }
        }
        summary.push(format!("{file:24} {pass:4}/{total}"));
    }
    println!("{}", summary.join("\n"));

    if std::env::var_os("UPDATE_CONFORMANCE").is_some() {
        let body: String = passing.iter().map(|id| format!("{id}\n")).collect();
        fs::write(passing_file(), body).unwrap();
        return;
    }
    let expected: BTreeSet<String> = fs::read_to_string(passing_file())
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let regressions: Vec<_> = failures
        .iter()
        .filter(|(id, _)| expected.contains(id))
        .collect();
    let newly = passing.difference(&expected).count();
    if newly > 0 {
        println!("{newly} newly passing cases; rerun with UPDATE_CONFORMANCE=1 to record them");
    }
    if let Ok(filter) = std::env::var("CONFORMANCE_SHOW") {
        for (id, e) in failures.iter().filter(|(id, _)| id.contains(&filter)) {
            println!("FAIL {id}: {e}");
        }
    }
    assert!(
        regressions.is_empty(),
        "regressions:\n{}",
        regressions
            .iter()
            .map(|(id, e)| format!("{id}: {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn compare_params(actual: &Value, case: &Value) -> Result<(), String> {
    let actual = normalize(actual);
    let expected = normalize(
        case.get("params")
            .unwrap_or(&Value::Object(Default::default())),
    );
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "params mismatch\n  actual:   {actual}\n  expected: {expected}"
        ))
    }
}

fn compare_json(actual: &Value, expected_body: &str) -> Result<(), String> {
    if expected_body.trim().is_empty() {
        return Ok(());
    }
    let expected: Value = serde_json::from_str(expected_body)
        .map_err(|e| format!("expected body is not json: {e}"))?;
    let (a, e) = (normalize(actual), normalize(&expected));
    if a == e {
        Ok(())
    } else {
        Err(format!("json mismatch\n  actual:   {a}\n  expected: {e}"))
    }
}

fn case_error(
    reg: &Registry,
    op: &Operation,
    case: &Value,
    code_of: impl Fn(&str) -> String,
) -> Option<(AwsError, Option<String>)> {
    let code = case.get("errorCode").and_then(Value::as_str)?;
    let shape_name = op
        .errors
        .iter()
        .map(|e| e.shape.clone())
        .find(|name| code_of(name) == code)
        .or_else(|| op.errors.first().map(|e| e.shape.clone()));
    let mut data = case["error"].as_object().cloned().unwrap_or_default();
    let message = case
        .get("errorMessage")
        .and_then(Value::as_str)
        .or_else(|| data.get("Message").and_then(Value::as_str))
        .or_else(|| data.get("message").and_then(Value::as_str))
        .unwrap_or_default()
        .to_string();
    data.remove("Message");
    data.remove("message");
    let mut error = AwsError::new(
        case["response"]["status_code"].as_u64().unwrap_or(400) as u16,
        code,
        message,
    );
    *error.data = data;
    let _ = reg;
    Some((error, shape_name))
}

fn run_json_case(
    direction: &str,
    reg: &Registry,
    op: &Operation,
    case: &Value,
) -> Result<(), String> {
    if direction == "input" {
        let body = case["serialized"]["body"].as_str().unwrap_or_default();
        let shape = op.input.as_ref().map(|i| reg.shape(&i.shape));
        let actual = json::parse_input(reg, body.as_bytes(), shape)
            .map_err(|e| format!("{}: {}", e.code, e.message))?;
        return compare_params(&actual, case);
    }
    let expected_body = case["response"]["body"].as_str().unwrap_or_default();
    if let Some((error, shape_name)) = case_error(reg, op, case, |name| {
        query::error_code(name, reg.shape(name))
    }) {
        let shape = shape_name.as_deref().map(|n| reg.shape(n));
        return compare_json(&json::error_body(reg, &error, shape), expected_body);
    }
    let result = case
        .get("result")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    let shape = op.output.as_ref().map(|o| reg.shape(&o.shape));
    let body: Value = serde_json::from_slice(&json::serialize_output(reg, &result, shape)).unwrap();
    compare_json(&body, expected_body)
}
