//! Static checks of a state machine definition, reported the way
//! `ValidateStateMachineDefinition` and `CreateStateMachine` do.

use serde_json::Value;

use super::intrinsic;
use crate::jsonpath as path;

const TYPES: &[&str] = &[
    "Pass", "Task", "Choice", "Wait", "Succeed", "Fail", "Parallel", "Map",
];

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub location: String,
}

pub fn check(definition: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    match serde_json::from_str::<Value>(definition) {
        Ok(v) if v.is_object() => machine(&v, "", &mut out),
        _ => out.push(Diagnostic {
            code: "INVALID_JSON_DESCRIPTION",
            message: "Definition must be a JSON object".into(),
            location: String::new(),
        }),
    }
    out
}

fn schema(out: &mut Vec<Diagnostic>, location: String, message: impl Into<String>) {
    out.push(Diagnostic {
        code: "SCHEMA_VALIDATION_FAILED",
        message: message.into(),
        location,
    });
}

fn machine(def: &Value, at: &str, out: &mut Vec<Diagnostic>) {
    let Some(states) = def.get("States").and_then(Value::as_object) else {
        schema(
            out,
            format!("{at}/States"),
            "The field 'States' is required",
        );
        return;
    };
    match def.get("StartAt").and_then(Value::as_str) {
        None => schema(
            out,
            format!("{at}/StartAt"),
            "The field 'StartAt' is required",
        ),
        Some(s) if !states.contains_key(s) => out.push(Diagnostic {
            code: "MISSING_TRANSITION_TARGET",
            message: format!("Missing 'Next' target: {s}"),
            location: format!("{at}/StartAt"),
        }),
        _ => {}
    }
    let target = |out: &mut Vec<Diagnostic>, next: Option<&Value>, location: String| {
        if let Some(n) = next {
            match n.as_str() {
                Some(n) if states.contains_key(n) => {}
                _ => out.push(Diagnostic {
                    code: "MISSING_TRANSITION_TARGET",
                    message: format!("Missing 'Next' target: {}", super::to_text(n)),
                    location,
                }),
            }
        }
    };
    for (name, state) in states {
        let loc = format!("{at}/States/{name}");
        let kind = state
            .get("Type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !TYPES.contains(&kind) {
            schema(
                out,
                format!("{loc}/Type"),
                format!("Unknown state type '{kind}'"),
            );
            continue;
        }
        let terminal = matches!(kind, "Choice" | "Succeed" | "Fail");
        let end = state.get("End").and_then(Value::as_bool) == Some(true);
        let next = state.get("Next");
        if !terminal && end == next.is_some() {
            schema(
                out,
                loc.clone(),
                "A state must have exactly one of 'Next' or 'End': true",
            );
        }
        target(out, next, format!("{loc}/Next"));
        for key in ["InputPath", "OutputPath", "ItemsPath"] {
            if let Some(Value::String(p)) = state.get(key)
                && !path::valid(p)
            {
                schema(
                    out,
                    format!("{loc}/{key}"),
                    format!("Invalid JSONPath '{p}'"),
                );
            }
        }
        if let Some(Value::String(p)) = state.get("ResultPath")
            && !path::is_reference(p)
        {
            schema(
                out,
                format!("{loc}/ResultPath"),
                format!("Invalid ReferencePath '{p}'"),
            );
        }
        for key in ["Parameters", "ResultSelector", "ItemSelector"] {
            if let Some(t) = state.get(key) {
                template(t, &format!("{loc}/{key}"), out);
            }
        }
        for (i, c) in state
            .get("Catch")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            target(out, c.get("Next"), format!("{loc}/Catch[{i}]/Next"));
        }
        match kind {
            "Task" if state.get("Resource").and_then(Value::as_str).is_none() => {
                schema(
                    out,
                    format!("{loc}/Resource"),
                    "The field 'Resource' is required",
                );
            }
            "Choice" => {
                let choices = state.get("Choices").and_then(Value::as_array);
                if choices.is_none_or(Vec::is_empty) {
                    schema(
                        out,
                        format!("{loc}/Choices"),
                        "The field 'Choices' must be a non-empty array",
                    );
                }
                for (i, c) in choices.into_iter().flatten().enumerate() {
                    if c.get("Next").is_none() {
                        schema(
                            out,
                            format!("{loc}/Choices[{i}]"),
                            "A choice rule must have 'Next'",
                        );
                    }
                    target(out, c.get("Next"), format!("{loc}/Choices[{i}]/Next"));
                }
                target(out, state.get("Default"), format!("{loc}/Default"));
            }
            "Parallel" => match state.get("Branches").and_then(Value::as_array) {
                Some(b) if !b.is_empty() => {
                    for (i, branch) in b.iter().enumerate() {
                        machine(branch, &format!("{loc}/Branches[{i}]"), out);
                    }
                }
                _ => schema(
                    out,
                    format!("{loc}/Branches"),
                    "The field 'Branches' must be a non-empty array",
                ),
            },
            "Map" => match state.get("ItemProcessor").or_else(|| state.get("Iterator")) {
                Some(p) => machine(p, &format!("{loc}/ItemProcessor"), out),
                None => schema(
                    out,
                    format!("{loc}/ItemProcessor"),
                    "The field 'ItemProcessor' is required",
                ),
            },
            _ => {}
        }
    }
}

fn template(t: &Value, at: &str, out: &mut Vec<Diagnostic>) {
    match t {
        Value::Object(o) => {
            for (k, v) in o {
                let loc = format!("{at}/{k}");
                if k.ends_with(".$") {
                    let ok = v.as_str().is_some_and(|s| {
                        if intrinsic::is_call(s) {
                            intrinsic::parses(s)
                        } else {
                            path::valid(s)
                        }
                    });
                    if !ok {
                        schema(
                            out,
                            loc,
                            format!(
                                "The value for the field '{k}' must be a valid JSONPath or a valid intrinsic function call"
                            ),
                        );
                    }
                } else {
                    template(v, &loc, out);
                }
            }
        }
        Value::Array(a) => {
            for (i, v) in a.iter().enumerate() {
                template(v, &format!("{at}[{i}]"), out);
            }
        }
        _ => {}
    }
}
