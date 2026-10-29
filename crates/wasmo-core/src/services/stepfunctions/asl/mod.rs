//! Amazon States Language: paths, payload templates, intrinsic functions,
//! Choice rules, and definition validation.

pub mod intrinsic;
pub mod path;
pub mod validate;

use serde_json::{Map, Value};

use crate::codec::time::parse_iso8601;
use crate::service::Ctx;

/// A state's error: the name `ErrorEquals` matches and the cause text.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub error: String,
    pub cause: String,
}

impl Failure {
    pub fn new(error: impl Into<String>, cause: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            cause: cause.into(),
        }
    }

    pub fn runtime(cause: impl Into<String>) -> Self {
        Self::new("States.Runtime", cause)
    }

    /// Errors that end the execution whatever the Retry and Catch fields say.
    pub fn fatal(&self) -> bool {
        matches!(
            self.error.as_str(),
            "States.Runtime" | "States.DataLimitExceeded"
        )
    }

    /// Whether a Retry or Catch `ErrorEquals` list names this error.
    pub fn matches(&self, names: &[Value]) -> bool {
        names.iter().filter_map(Value::as_str).any(|n| match n {
            "States.ALL" => !self.fatal(),
            "States.TaskFailed" => !self.fatal() && self.error != "States.Timeout",
            n => n == self.error,
        })
    }

    pub fn json(&self) -> Value {
        serde_json::json!({ "Error": self.error, "Cause": self.cause })
    }
}

/// The data a state's paths read: the effective input, the context object
/// (`$$`), and the emulator for `States.UUID` / `States.MathRandom`.
pub struct Scope<'a, 'c> {
    pub input: &'a Value,
    pub context: &'a Value,
    pub ctx: &'a mut Ctx<'c>,
}

impl Scope<'_, '_> {
    pub fn path(&self, p: &str) -> Result<Value, Failure> {
        let (data, p2) = match p.strip_prefix("$$") {
            Some(rest) => (self.context, format!("${rest}")),
            None => (self.input, p.to_string()),
        };
        path::get(data, &p2)
            .map_err(Failure::runtime)?
            .ok_or_else(|| {
                Failure::runtime(format!(
                    "The JSONPath '{p}' could not be found in the input '{}'",
                    data
                ))
            })
    }

    pub fn uuid(&mut self) -> String {
        self.ctx.uuid()
    }

    pub fn random_u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        self.ctx.random_bytes(&mut b);
        u64::from_le_bytes(b)
    }
}

/// A string as itself, anything else as its JSON text.
pub fn to_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Evaluates a payload template (`Parameters`, `ResultSelector`,
/// `ItemSelector`): keys ending in `.$` take a path or an intrinsic function.
pub fn template(t: &Value, scope: &mut Scope<'_, '_>) -> Result<Value, Failure> {
    match t {
        Value::Object(o) => {
            let mut out = Map::new();
            for (k, v) in o {
                match k.strip_suffix(".$") {
                    Some(name) => {
                        let expr = v.as_str().ok_or_else(|| {
                            Failure::runtime(format!(
                                "The value of the field '{k}' is not a string"
                            ))
                        })?;
                        let value = if intrinsic::is_call(expr) {
                            intrinsic::eval(expr, scope)?
                        } else {
                            scope.path(expr).map_err(|f| {
                                Failure::runtime(format!(
                                    "The JSONPath '{expr}' specified for the field '{k}' could not be found in the input: {}",
                                    f.cause
                                ))
                            })?
                        };
                        out.insert(name.to_string(), value);
                    }
                    None => {
                        out.insert(k.clone(), template(v, scope)?);
                    }
                }
            }
            Ok(Value::Object(out))
        }
        Value::Array(a) => a
            .iter()
            .map(|v| template(v, scope))
            .collect::<Result<_, _>>()
            .map(Value::Array),
        other => Ok(other.clone()),
    }
}

/// Applies `InputPath` / `OutputPath`: absent keeps the value, null gives `{}`.
pub fn select(state: &Value, key: &str, data: &Value, context: &Value) -> Result<Value, Failure> {
    match state.get(key) {
        None => Ok(data.clone()),
        Some(Value::Null) => Ok(Value::Object(Map::new())),
        Some(Value::String(p)) => {
            let (src, p2) = match p.strip_prefix("$$") {
                Some(rest) => (context, format!("${rest}")),
                None => (data, p.clone()),
            };
            path::get(src, &p2)
                .map_err(Failure::runtime)?
                .ok_or_else(|| {
                    Failure::runtime(format!(
                        "Invalid path '{p}' : No results for path: $['{}']",
                        p.trim_start_matches("$.")
                    ))
                })
        }
        Some(_) => Err(Failure::runtime(format!("{key} must be a string or null"))),
    }
}

/// Applies `ResultPath`: absent replaces the input, null keeps it, a path
/// writes the result into it.
pub fn place(state: &Value, input: &Value, result: Value) -> Result<Value, Failure> {
    match state.get("ResultPath") {
        None => Ok(result),
        Some(Value::Null) => Ok(input.clone()),
        Some(Value::String(p)) => path::set(input.clone(), p, result).map_err(|cause| Failure {
            error: "States.ResultPathMatchFailure".into(),
            cause,
        }),
        Some(_) => Err(Failure::runtime("ResultPath must be a string or null")),
    }
}

/// Whether a Choice rule holds for `scope.input`.
pub fn choice(rule: &Value, scope: &Scope<'_, '_>) -> Result<bool, Failure> {
    if let Some(all) = rule.get("And").and_then(Value::as_array) {
        for r in all {
            if !choice(r, scope)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    if let Some(any) = rule.get("Or").and_then(Value::as_array) {
        for r in any {
            if choice(r, scope)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    if let Some(not) = rule.get("Not") {
        return Ok(!choice(not, scope)?);
    }
    let var = rule
        .get("Variable")
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::runtime("A Choice rule must have a Variable"))?;
    let value = scope.path(var).ok();
    if let Some(want) = rule.get("IsPresent").and_then(Value::as_bool) {
        return Ok(value.is_some() == want);
    }
    let value = value.ok_or_else(|| {
        Failure::runtime(format!(
            "Invalid path '{var}': The choice state's condition path references an invalid value."
        ))
    })?;
    for (op, operand) in rule.as_object().into_iter().flatten() {
        if op == "Variable" || op == "Next" || op == "Comment" {
            continue;
        }
        let operand = match op.strip_suffix("Path") {
            Some(_) if !op.starts_with("Is") => {
                let p = operand
                    .as_str()
                    .ok_or_else(|| Failure::runtime(format!("{op} must be a path")))?;
                scope.path(p)?
            }
            _ => operand.clone(),
        };
        let base = op.strip_suffix("Path").unwrap_or(op);
        return Ok(compare(base, &value, &operand));
    }
    Err(Failure::runtime("A Choice rule must have a comparison"))
}

fn compare(op: &str, v: &Value, x: &Value) -> bool {
    use std::cmp::Ordering::*;
    let test = |o: std::cmp::Ordering, suffix: &str| match suffix {
        "Equals" => o == Equal,
        "LessThan" => o == Less,
        "GreaterThan" => o == Greater,
        "LessThanEquals" => o != Greater,
        "GreaterThanEquals" => o != Less,
        _ => false,
    };
    if let Some(s) = op.strip_prefix("String") {
        if s == "Matches" {
            return matches!((v, x), (Value::String(v), Value::String(p)) if wildcard(p, v));
        }
        return match (v, x) {
            (Value::String(a), Value::String(b)) => test(a.cmp(b), s),
            _ => false,
        };
    }
    if let Some(s) = op.strip_prefix("Numeric") {
        return match (v.as_f64(), x.as_f64()) {
            (Some(a), Some(b)) if v.is_number() && x.is_number() => {
                a.partial_cmp(&b).is_some_and(|o| test(o, s))
            }
            _ => false,
        };
    }
    if let Some(s) = op.strip_prefix("Timestamp") {
        return match (timestamp(v), timestamp(x)) {
            (Some(a), Some(b)) => a.partial_cmp(&b).is_some_and(|o| test(o, s)),
            _ => false,
        };
    }
    match op {
        "BooleanEquals" => v.is_boolean() && v == x,
        "IsNull" => v.is_null() == (x == &Value::Bool(true)),
        "IsNumeric" => v.is_number() == (x == &Value::Bool(true)),
        "IsString" => v.is_string() == (x == &Value::Bool(true)),
        "IsBoolean" => v.is_boolean() == (x == &Value::Bool(true)),
        "IsTimestamp" => timestamp(v).is_some() == (x == &Value::Bool(true)),
        _ => false,
    }
}

pub fn timestamp(v: &Value) -> Option<f64> {
    let s = v.as_str()?;
    s.contains('T').then(|| parse_iso8601(s)).flatten()
}

/// `StringMatches`: `*` matches any run of characters; `\*` and `\\` are
/// literal.
fn wildcard(pattern: &str, s: &str) -> bool {
    enum Tok {
        Lit(char),
        Star,
    }
    let mut toks = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => toks.push(Tok::Lit(chars.next().unwrap_or('\\'))),
            '*' => toks.push(Tok::Star),
            c => toks.push(Tok::Lit(c)),
        }
    }
    let s: Vec<char> = s.chars().collect();
    let (mut ti, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        match toks.get(ti) {
            Some(Tok::Lit(c)) if *c == s[si] => {
                ti += 1;
                si += 1;
            }
            Some(Tok::Star) => {
                star = Some(ti);
                mark = si;
                ti += 1;
            }
            _ => match star {
                Some(st) => {
                    ti = st + 1;
                    mark += 1;
                    si = mark;
                }
                None => return false,
            },
        }
    }
    toks[ti..].iter().all(|t| matches!(t, Tok::Star))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::FixedEnv;
    use serde_json::json;

    fn eval(t: Value, input: Value) -> Result<Value, Failure> {
        let mut env = FixedEnv {
            now_ms: 0.0,
            seed: 3,
        };
        let mut ctx = Ctx::new("000000000000", "us-east-1", "http://wasmo.local", &mut env);
        let context = json!({"Execution": {"Name": "e1"}});
        let mut scope = Scope {
            input: &input,
            context: &context,
            ctx: &mut ctx,
        };
        template(&t, &mut scope)
    }

    #[test]
    fn templates_and_intrinsics() {
        let out = eval(
            json!({
                "greeting.$": "States.Format('Hello, {}! You are {}.', $.name, $.age)",
                "parsed.$": "States.StringToJson($.raw)",
                "text.$": "States.JsonToString($.obj)",
                "arr.$": "States.Array(1, 'two', $.age)",
                "parts.$": "States.ArrayPartition($.nums, 2)",
                "has.$": "States.ArrayContains($.nums, 3)",
                "range.$": "States.ArrayRange(1, 9, 4)",
                "item.$": "States.ArrayGetItem($.nums, 1)",
                "len.$": "States.ArrayLength($.nums)",
                "uniq.$": "States.ArrayUnique(States.Array(1, 1, 2))",
                "sum.$": "States.MathAdd($.age, -2)",
                "b64.$": "States.Base64Encode('hi')",
                "plain.$": "States.Base64Decode('aGk=')",
                "md5.$": "States.Hash('abc', 'MD5')",
                "sha.$": "States.Hash('abc', 'SHA-256')",
                "merged.$": "States.JsonMerge($.obj, States.StringToJson('{\"b\":2}'), false)",
                "split.$": "States.StringSplit('a,b;c', ',;')",
                "exec.$": "$$.Execution.Name",
                "esc.$": "States.Format('it\\'s \\{\\}')",
                "nested": [{"n.$": "$.age"}],
                "fixed": 1
            }),
            json!({"name": "Ann", "age": 30, "raw": "{\"a\":1}", "obj": {"a": 1}, "nums": [1, 2, 3, 4, 5]}),
        )
        .unwrap();
        assert_eq!(out["greeting"], "Hello, Ann! You are 30.");
        assert_eq!(out["parsed"], json!({"a": 1}));
        assert_eq!(out["text"], "{\"a\":1}");
        assert_eq!(out["arr"], json!([1, "two", 30]));
        assert_eq!(out["parts"], json!([[1, 2], [3, 4], [5]]));
        assert_eq!(out["has"], true);
        assert_eq!(out["range"], json!([1, 5, 9]));
        assert_eq!(out["item"], 2);
        assert_eq!(out["len"], 5);
        assert_eq!(out["uniq"], json!([1, 2]));
        assert_eq!(out["sum"], 28);
        assert_eq!(out["b64"], "aGk=");
        assert_eq!(out["plain"], "hi");
        assert_eq!(out["md5"], "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            out["sha"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(out["merged"], json!({"a": 1, "b": 2}));
        assert_eq!(out["split"], json!(["a", "b", "c"]));
        assert_eq!(out["exec"], "e1");
        assert_eq!(out["esc"], "it's {}");
        assert_eq!(out["nested"], json!([{"n": 30}]));
        assert_eq!(out["fixed"], 1);
        let e = eval(json!({"x.$": "$.missing"}), json!({})).unwrap_err();
        assert_eq!(e.error, "States.Runtime");
        let e = eval(
            json!({"x.$": "States.ArrayGetItem($.a, 5)"}),
            json!({"a": []}),
        )
        .unwrap_err();
        assert_eq!(e.error, "States.IntrinsicFailure");
    }

    #[test]
    fn choice_rules() {
        let mut env = FixedEnv {
            now_ms: 0.0,
            seed: 3,
        };
        let mut ctx = Ctx::new("000000000000", "us-east-1", "http://wasmo.local", &mut env);
        let input = json!({"n": 5, "s": "log-2024.txt", "b": true, "t": "2024-01-02T00:00:00Z", "m": 5, "nil": null});
        let context = json!({});
        let scope = Scope {
            input: &input,
            context: &context,
            ctx: &mut ctx,
        };
        let check = |r: Value| choice(&r, &scope).unwrap();
        assert!(check(json!({"Variable": "$.n", "NumericEquals": 5})));
        assert!(check(
            json!({"Variable": "$.n", "NumericEqualsPath": "$.m"})
        ));
        assert!(check(
            json!({"Variable": "$.n", "NumericLessThanEquals": 5})
        ));
        assert!(!check(json!({"Variable": "$.n", "NumericGreaterThan": 5})));
        assert!(check(
            json!({"Variable": "$.s", "StringMatches": "log-*.txt"})
        ));
        assert!(!check(
            json!({"Variable": "$.s", "StringMatches": "log-\\*.txt"})
        ));
        assert!(check(
            json!({"Variable": "$.s", "StringGreaterThan": "log"})
        ));
        assert!(check(json!({"Variable": "$.b", "BooleanEquals": true})));
        assert!(check(
            json!({"Variable": "$.t", "TimestampLessThan": "2024-02-01T00:00:00Z"})
        ));
        assert!(check(json!({"Variable": "$.t", "IsTimestamp": true})));
        assert!(check(json!({"Variable": "$.nil", "IsNull": true})));
        assert!(check(json!({"Variable": "$.x", "IsPresent": false})));
        assert!(check(json!({"Variable": "$.n", "IsNumeric": true})));
        assert!(!check(json!({"Variable": "$.s", "NumericEquals": 5})));
        assert!(check(json!({"And": [
            {"Variable": "$.n", "NumericGreaterThan": 1},
            {"Not": {"Variable": "$.s", "StringEquals": "x"}}
        ]})));
        assert!(check(json!({"Or": [
            {"Variable": "$.n", "NumericLessThan": 1},
            {"Variable": "$.b", "BooleanEquals": true}
        ]})));
        assert!(choice(&json!({"Variable": "$.x", "StringEquals": "a"}), &scope).is_err());
    }
}
