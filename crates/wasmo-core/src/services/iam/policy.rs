//! IAM policy documents: validation and the evaluation behind
//! SimulatePrincipalPolicy and SimulateCustomPolicy.

use std::collections::BTreeMap;

use serde_json::{Value, json};

pub type Context = BTreeMap<String, Vec<String>>;

pub fn parse(doc: &str) -> Result<Value, String> {
    let v: Value = serde_json::from_str(doc).map_err(|_| "Syntax errors in policy.".to_string())?;
    let statements = statements(&v);
    if statements.is_empty() {
        return Err("Syntax errors in policy.".to_string());
    }
    for s in statements {
        match s.get("Effect").and_then(Value::as_str) {
            Some("Allow" | "Deny") => {}
            _ => return Err("Policy statement must contain a valid Effect.".to_string()),
        }
        if s.get("Action").is_none() && s.get("NotAction").is_none() {
            return Err("Policy statement must contain actions.".to_string());
        }
    }
    Ok(v)
}

fn statements(doc: &Value) -> Vec<&Value> {
    match doc.get("Statement") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(s @ Value::Object(_)) => vec![s],
        _ => Vec::new(),
    }
}

fn list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => vec![s.clone()],
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

/// Glob match with `*` and `?`; `fold` compares case-insensitively.
pub fn glob(pattern: &str, value: &str, fold: bool) -> bool {
    let (p, v): (Vec<char>, Vec<char>) = if fold {
        (
            pattern.to_lowercase().chars().collect(),
            value.to_lowercase().chars().collect(),
        )
    } else {
        (pattern.chars().collect(), value.chars().collect())
    };
    let (mut pi, mut vi, mut star, mut mark) = (0, 0, None, 0);
    while vi < v.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == v[vi]) {
            pi += 1;
            vi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = vi;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            vi = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

fn condition(op: &str, key: &str, expected: &[String], ctx: &Context) -> bool {
    let (op, if_exists) = match op.strip_suffix("IfExists") {
        Some(o) => (o, true),
        None => (op, false),
    };
    let (op, quantifier) = match op.split_once(':') {
        Some((q, o)) => (o, Some(q)),
        None => (op, None),
    };
    let actual = ctx
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_slice());
    if op == "Null" {
        let is_null = actual.is_none_or(<[String]>::is_empty);
        return expected.iter().any(|e| (e == "true") == is_null);
    }
    let Some(actual) = actual.filter(|a| !a.is_empty()) else {
        return if_exists || op.contains("Not") && quantifier.is_some();
    };
    let num = |s: &str| s.parse::<f64>().ok();
    let test = |a: &str, e: &str| -> bool {
        match op {
            "StringEquals" => a == e,
            "StringNotEquals" => a != e,
            "StringEqualsIgnoreCase" => a.eq_ignore_ascii_case(e),
            "StringNotEqualsIgnoreCase" => !a.eq_ignore_ascii_case(e),
            "StringLike" => glob(e, a, false),
            "StringNotLike" => !glob(e, a, false),
            "ArnEquals" | "ArnLike" => glob(e, a, false),
            "ArnNotEquals" | "ArnNotLike" => !glob(e, a, false),
            "Bool" => a.eq_ignore_ascii_case(e),
            "NumericEquals" => num(a) == num(e),
            "NumericNotEquals" => num(a) != num(e),
            "NumericLessThan" => num(a).zip(num(e)).is_some_and(|(a, e)| a < e),
            "NumericLessThanEquals" => num(a).zip(num(e)).is_some_and(|(a, e)| a <= e),
            "NumericGreaterThan" => num(a).zip(num(e)).is_some_and(|(a, e)| a > e),
            "NumericGreaterThanEquals" => num(a).zip(num(e)).is_some_and(|(a, e)| a >= e),
            "DateEquals"
            | "DateLessThan"
            | "DateGreaterThan"
            | "DateLessThanEquals"
            | "DateGreaterThanEquals"
            | "DateNotEquals" => {
                let (x, y) = (a.cmp(e), op);
                match y {
                    "DateEquals" => x.is_eq(),
                    "DateNotEquals" => !x.is_eq(),
                    "DateLessThan" => x.is_lt(),
                    "DateGreaterThan" => x.is_gt(),
                    "DateLessThanEquals" => x.is_le(),
                    _ => x.is_ge(),
                }
            }
            "IpAddress" => a == e || e.split('/').next() == Some(a),
            "NotIpAddress" => !(a == e || e.split('/').next() == Some(a)),
            _ => false,
        }
    };
    let negated = op.contains("Not");
    let matches_one = |a: &str| {
        if negated {
            expected.iter().all(|e| test(a, e))
        } else {
            expected.iter().any(|e| test(a, e))
        }
    };
    match quantifier {
        Some("ForAllValues") => actual.iter().all(|a| matches_one(a)),
        _ => actual.iter().any(|a| matches_one(a)),
    }
}

fn conditions_hold(stmt: &Value, ctx: &Context) -> bool {
    stmt.get("Condition")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .all(|(op, block)| {
            block.as_object().into_iter().flatten().all(|(key, v)| {
                let expected = match v {
                    Value::Array(a) => a
                        .iter()
                        .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_string))
                        .collect(),
                    Value::String(s) => vec![s.clone()],
                    other => vec![other.to_string()],
                };
                condition(op, key, &expected, ctx)
            })
        })
}

fn statement_applies(stmt: &Value, action: &str, resource: &str, ctx: &Context) -> bool {
    let action_ok = match stmt.get("Action") {
        Some(a) => list(Some(a)).iter().any(|p| glob(p, action, true)),
        None => !list(stmt.get("NotAction"))
            .iter()
            .any(|p| glob(p, action, true)),
    };
    let resource_ok = match (stmt.get("Resource"), stmt.get("NotResource")) {
        (Some(r), _) => list(Some(r)).iter().any(|p| glob(p, resource, false)),
        (None, Some(n)) => !list(Some(n)).iter().any(|p| glob(p, resource, false)),
        (None, None) => true,
    };
    action_ok && resource_ok && conditions_hold(stmt, ctx)
}

pub struct Source {
    pub id: String,
    pub kind: String,
    pub doc: Value,
}

pub struct Outcome {
    pub decision: &'static str,
    pub matched: Vec<Value>,
}

/// Evaluates identity policies, then the permissions boundary when there is one.
pub fn evaluate(
    policies: &[Source],
    boundary: Option<&Source>,
    action: &str,
    resource: &str,
    ctx: &Context,
) -> Outcome {
    let mut matched = Vec::new();
    let (mut allow, mut deny) = (false, false);
    for p in policies {
        for s in statements(&p.doc) {
            if !statement_applies(s, action, resource, ctx) {
                continue;
            }
            matched.push(json!({
                "SourcePolicyId": p.id,
                "SourcePolicyType": p.kind,
                "StartPosition": { "Line": 1, "Column": 1 },
                "EndPosition": { "Line": 1, "Column": 1 },
            }));
            match s.get("Effect").and_then(Value::as_str) {
                Some("Deny") => deny = true,
                _ => allow = true,
            }
        }
    }
    let decision = if deny {
        "explicitDeny"
    } else if !allow {
        "implicitDeny"
    } else if let Some(b) = boundary {
        let applies: Vec<&Value> = statements(&b.doc)
            .into_iter()
            .filter(|s| statement_applies(s, action, resource, ctx))
            .collect();
        if applies
            .iter()
            .any(|s| s.get("Effect").and_then(Value::as_str) == Some("Deny"))
        {
            "explicitDeny"
        } else if applies.is_empty() {
            "implicitDeny"
        } else {
            "allowed"
        }
    } else {
        "allowed"
    };
    Outcome { decision, matched }
}

/// Condition keys a policy document references.
pub fn context_keys(doc: &Value) -> Vec<String> {
    let mut keys: Vec<String> = statements(doc)
        .iter()
        .filter_map(|s| s.get("Condition").and_then(Value::as_object))
        .flat_map(|c| c.values())
        .filter_map(Value::as_object)
        .flat_map(|b| b.keys().cloned())
        .collect();
    keys.sort_by_key(|k| k.to_lowercase());
    keys.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(doc: &str) -> Source {
        Source {
            id: "p".into(),
            kind: "IAMPolicy".into(),
            doc: parse(doc).unwrap(),
        }
    }

    #[test]
    fn globs() {
        assert!(glob("s3:Get*", "s3:GetObject", true));
        assert!(glob("S3:get*", "s3:GetObject", true));
        assert!(!glob("s3:Get*", "s3:PutObject", true));
        assert!(glob(
            "arn:aws:s3:::bucket/*",
            "arn:aws:s3:::bucket/a/b",
            false
        ));
        assert!(!glob(
            "arn:aws:s3:::bucket/*",
            "arn:aws:s3:::other/a",
            false
        ));
        assert!(glob("a?c", "abc", false));
        assert!(glob("*", "", false));
    }

    #[test]
    fn deny_overrides_allow_and_boundaries_cap_permissions() {
        let allow = src(
            r#"{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":"s3:*","Resource":"*"}]}"#,
        );
        let deny = src(
            r#"{"Statement":{"Effect":"Deny","Action":"s3:DeleteObject","Resource":"arn:aws:s3:::prod/*"}}"#,
        );
        let ctx = Context::new();
        assert_eq!(
            evaluate(&[], None, "s3:GetObject", "*", &ctx).decision,
            "implicitDeny"
        );
        let both = [allow, deny];
        assert_eq!(
            evaluate(&both, None, "s3:GetObject", "arn:aws:s3:::prod/x", &ctx).decision,
            "allowed"
        );
        assert_eq!(
            evaluate(&both, None, "s3:DeleteObject", "arn:aws:s3:::prod/x", &ctx).decision,
            "explicitDeny"
        );
        assert_eq!(
            evaluate(&both, None, "s3:DeleteObject", "arn:aws:s3:::dev/x", &ctx).decision,
            "allowed"
        );
        let boundary =
            src(r#"{"Statement":[{"Effect":"Allow","Action":"s3:Get*","Resource":"*"}]}"#);
        assert_eq!(
            evaluate(&both, Some(&boundary), "s3:GetObject", "*", &ctx).decision,
            "allowed"
        );
        assert_eq!(
            evaluate(&both, Some(&boundary), "s3:PutObject", "*", &ctx).decision,
            "implicitDeny"
        );
        assert_eq!(
            evaluate(&both, None, "s3:GetObject", "*", &ctx)
                .matched
                .len(),
            1
        );
    }

    #[test]
    fn not_action_and_conditions() {
        let p = src(r#"{"Statement":[
            {"Effect":"Allow","NotAction":"iam:*","Resource":"*"},
            {"Effect":"Deny","Action":"ec2:*","Resource":"*","Condition":{"StringNotEquals":{"aws:RequestedRegion":["us-east-1","eu-west-1"]}}},
            {"Effect":"Allow","Action":"iam:GetUser","Resource":"*","Condition":{"Bool":{"aws:MultiFactorAuthPresent":"true"}}}
        ]}"#);
        let mut ctx = Context::new();
        assert_eq!(
            evaluate(std::slice::from_ref(&p), None, "iam:GetUser", "*", &ctx).decision,
            "implicitDeny"
        );
        ctx.insert("aws:MultiFactorAuthPresent".into(), vec!["true".into()]);
        assert_eq!(
            evaluate(std::slice::from_ref(&p), None, "iam:GetUser", "*", &ctx).decision,
            "allowed"
        );
        ctx.insert("aws:RequestedRegion".into(), vec!["ap-northeast-1".into()]);
        assert_eq!(
            evaluate(
                std::slice::from_ref(&p),
                None,
                "ec2:RunInstances",
                "*",
                &ctx
            )
            .decision,
            "explicitDeny"
        );
        ctx.insert("aws:RequestedRegion".into(), vec!["us-east-1".into()]);
        assert_eq!(
            evaluate(
                std::slice::from_ref(&p),
                None,
                "ec2:RunInstances",
                "*",
                &ctx
            )
            .decision,
            "allowed"
        );
        assert_eq!(
            context_keys(&p.doc),
            ["aws:MultiFactorAuthPresent", "aws:RequestedRegion"]
        );
    }

    #[test]
    fn validates_documents() {
        assert!(parse("nope").is_err());
        assert!(parse(r#"{"Statement":[]}"#).is_err());
        assert!(parse(r#"{"Statement":[{"Effect":"Maybe","Action":"*"}]}"#).is_err());
        assert!(parse(r#"{"Statement":[{"Effect":"Allow"}]}"#).is_err());
    }
}
