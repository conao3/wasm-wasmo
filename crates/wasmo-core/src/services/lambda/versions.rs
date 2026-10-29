use serde_json::{Map, Value, json};

use super::{
    Function, MapExt, Region, conflict, err, invalid, no_function, not_found, page, target,
};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt};

/// Snapshots `$LATEST` as the next numbered version.
pub(super) fn publish(
    ctx: &mut Ctx<'_>,
    f: &mut Function,
    description: Option<&str>,
) -> Map<String, Value> {
    f.last_version += 1;
    let n = f.last_version;
    let mut v = f.config.clone();
    v.insert("Version".into(), json!(n.to_string()));
    v.insert("FunctionArn".into(), json!(format!("{}:{n}", f.arn())));
    v.insert("RevisionId".into(), json!(ctx.uuid()));
    if let Some(d) = description {
        v.insert("Description".into(), json!(d));
    }
    f.versions.insert(n, v.clone());
    v
}

/// Configuration of the version a qualifier (version number, alias, or
/// `$LATEST`) points at.
pub(super) fn resolve(
    ctx: &Ctx<'_>,
    f: &Function,
    qualifier: Option<&str>,
) -> Result<Map<String, Value>, AwsError> {
    let missing = || no_function(ctx, f.name(), qualifier);
    match qualifier {
        None | Some("$LATEST") => Ok(f.config.clone()),
        Some(q) if q.bytes().all(|b| b.is_ascii_digit()) => q
            .parse::<u64>()
            .ok()
            .and_then(|n| f.versions.get(&n))
            .cloned()
            .ok_or_else(missing),
        Some(q) => {
            let alias = f.aliases.get(q).ok_or_else(missing)?;
            resolve(ctx, f, alias.str_field("FunctionVersion"))
        }
    }
}

fn check_version(ctx: &Ctx<'_>, f: &Function, version: &str) -> Result<(), AwsError> {
    if version == "$LATEST"
        || version
            .parse::<u64>()
            .is_ok_and(|n| f.versions.contains_key(&n))
    {
        Ok(())
    } else {
        Err(no_function(ctx, f.name(), Some(version)))
    }
}

fn routing(ctx: &Ctx<'_>, f: &Function, input: &Value) -> Result<Option<Value>, AwsError> {
    let Some(rc) = input.obj_field("RoutingConfig") else {
        return Ok(None);
    };
    let weights = rc
        .get("AdditionalVersionWeights")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (version, w) in &weights {
        check_version(ctx, f, version)?;
        if !w.as_f64().is_some_and(|w| (0.0..=1.0).contains(&w)) {
            return Err(invalid(format!(
                "1 validation error detected: Value '{w}' at 'routingConfig.additionalVersionWeights' failed to satisfy constraint: Map value must satisfy constraint: [Member must have value less than or equal to 1.0, Member must have value greater than or equal to 0.0]"
            )));
        }
    }
    Ok(Some(json!({ "AdditionalVersionWeights": weights })))
}

fn alias_missing(ctx: &Ctx<'_>, f: &Function, name: &str) -> AwsError {
    not_found(format!(
        "Alias not found: {}:{name}",
        super::function_arn(ctx, f.name())
    ))
}

fn aliases(
    ctx: &mut Ctx<'_>,
    f: &mut Function,
    operation: &str,
    input: &Value,
) -> Result<Value, AwsError> {
    if operation == "ListAliases" {
        let version = input.str_field("FunctionVersion");
        let items: Vec<Value> = f
            .aliases
            .values()
            .filter(|a| version.is_none_or(|v| a.str_field("FunctionVersion") == Some(v)))
            .cloned()
            .map(Value::Object)
            .collect();
        let (items, next) = page(items, input, "MaxItems");
        let mut out = json!({ "Aliases": items });
        if let Some(n) = next {
            out["NextMarker"] = json!(n);
        }
        return Ok(out);
    }
    let name = input.require_str("Name")?.to_string();
    match operation {
        "CreateAlias" => {
            if name.bytes().all(|b| b.is_ascii_digit()) || name == "$LATEST" {
                return Err(invalid(format!(
                    "1 validation error detected: Value '{name}' at 'name' failed to satisfy constraint: Member must satisfy regular expression pattern: (?!^[0-9]+$)([a-zA-Z0-9-_]+)"
                )));
            }
            if f.aliases.contains_key(&name) {
                return Err(conflict(format!(
                    "Alias already exists: {}:{name}",
                    f.arn()
                )));
            }
            let version = input.require_str("FunctionVersion")?;
            check_version(ctx, f, version)?;
            let mut a = Map::new();
            a.insert("AliasArn".into(), json!(format!("{}:{name}", f.arn())));
            a.insert("Name".into(), json!(name));
            a.insert("FunctionVersion".into(), json!(version));
            a.insert(
                "Description".into(),
                json!(input.str_field("Description").unwrap_or_default()),
            );
            if let Some(r) = routing(ctx, f, input)? {
                a.insert("RoutingConfig".into(), r);
            }
            a.insert("RevisionId".into(), json!(ctx.uuid()));
            f.aliases.insert(name, a.clone());
            Ok(Value::Object(a))
        }
        "GetAlias" => f
            .aliases
            .get(&name)
            .cloned()
            .map(Value::Object)
            .ok_or_else(|| alias_missing(ctx, f, &name)),
        "DeleteAlias" => {
            f.aliases.remove(&name);
            Ok(json!({}))
        }
        _ => {
            let version = input.str_field("FunctionVersion").map(str::to_string);
            if let Some(v) = &version {
                check_version(ctx, f, v)?;
            }
            let route = routing(ctx, f, input)?;
            let revision = ctx.uuid();
            let missing = alias_missing(ctx, f, &name);
            let a = f.aliases.get_mut(&name).ok_or(missing)?;
            if let Some(r) = input.str_field("RevisionId")
                && Some(r) != a.str_field("RevisionId")
            {
                return Err(err(
                    412,
                    "PreconditionFailedException",
                    "The Revision Id provided does not match the latest Revision Id. Call the GetFunction/GetAlias API to retrieve the latest Revision Id",
                ));
            }
            if let Some(v) = version {
                a.insert("FunctionVersion".into(), json!(v));
            }
            if let Some(d) = input.str_field("Description") {
                a.insert("Description".into(), json!(d));
            }
            if let Some(r) = route {
                a.insert("RoutingConfig".into(), r);
            }
            a.insert("RevisionId".into(), json!(revision));
            Ok(Value::Object(a.clone()))
        }
    }
}

fn principal(p: &str) -> Value {
    if p == "*" {
        json!("*")
    } else if p.ends_with(".amazonaws.com") || p.ends_with(".amazonaws.com.cn") {
        json!({ "Service": p })
    } else if p.bytes().all(|b| b.is_ascii_digit()) {
        json!({ "AWS": format!("arn:aws:iam::{p}:root") })
    } else {
        json!({ "AWS": p })
    }
}

fn permissions(
    ctx: &mut Ctx<'_>,
    f: &mut Function,
    qualifier: Option<String>,
    operation: &str,
    input: &Value,
) -> Result<Value, AwsError> {
    let resource = match &qualifier {
        Some(q) => {
            resolve(ctx, f, Some(q))?;
            format!("{}:{q}", f.arn())
        }
        None => f.arn().to_string(),
    };
    let key = qualifier.unwrap_or_default();
    let revision = f
        .config
        .str_field("RevisionId")
        .unwrap_or_default()
        .to_string();
    match operation {
        "AddPermission" => {
            let sid = input.require_str("StatementId")?.to_string();
            let statements = f.policies.entry(key).or_default();
            if statements.iter().any(|s| s.str_field("Sid") == Some(&sid)) {
                return Err(conflict(format!(
                    "The statement id ({sid}) provided already exists. Please provide a new statement id, or remove the existing statement."
                )));
            }
            let mut s = json!({
                "Sid": sid,
                "Effect": "Allow",
                "Principal": principal(input.require_str("Principal")?),
                "Action": input.require_str("Action")?,
                "Resource": resource,
            });
            let mut condition = Map::new();
            if let Some(a) = input.str_field("SourceArn") {
                condition.insert("ArnLike".into(), json!({ "AWS:SourceArn": a }));
            }
            let mut equals = Map::new();
            if let Some(a) = input.str_field("SourceAccount") {
                equals.insert("AWS:SourceAccount".into(), json!(a));
            }
            if let Some(o) = input.str_field("PrincipalOrgID") {
                equals.insert("aws:PrincipalOrgID".into(), json!(o));
            }
            if let Some(t) = input.str_field("EventSourceToken") {
                equals.insert("lambda:EventSourceToken".into(), json!(t));
            }
            if let Some(t) = input.str_field("FunctionUrlAuthType") {
                equals.insert("lambda:FunctionUrlAuthType".into(), json!(t));
            }
            if !equals.is_empty() {
                condition.insert("StringEquals".into(), Value::Object(equals));
            }
            if !condition.is_empty() {
                s["Condition"] = Value::Object(condition);
            }
            statements.push(s.clone());
            Ok(json!({ "Statement": s.to_string() }))
        }
        "RemovePermission" => {
            let sid = input.require_str("StatementId")?;
            let statements = f.policies.entry(key.clone()).or_default();
            let before = statements.len();
            statements.retain(|s| s.str_field("Sid") != Some(sid));
            if statements.len() == before {
                return Err(not_found(format!(
                    "Statement {sid} is not found in resource policy."
                )));
            }
            if statements.is_empty() {
                f.policies.remove(&key);
            }
            Ok(json!({}))
        }
        _ => {
            let statements = f
                .policies
                .get(&key)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| not_found("The resource you requested does not exist."))?;
            let policy =
                json!({ "Version": "2012-10-17", "Id": "default", "Statement": statements });
            Ok(json!({ "Policy": policy.to_string(), "RevisionId": revision }))
        }
    }
}

pub(super) fn call(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    if !matches!(
        operation,
        "PublishVersion"
            | "ListVersionsByFunction"
            | "CreateAlias"
            | "GetAlias"
            | "UpdateAlias"
            | "DeleteAlias"
            | "ListAliases"
            | "AddPermission"
            | "RemovePermission"
            | "GetPolicy"
    ) {
        return Ok(None);
    }
    let (name, qualifier) = target(input)?;
    let f = region
        .functions
        .get_mut(&name)
        .ok_or_else(|| no_function(ctx, &name, None))?;
    Ok(Some(match operation {
        "PublishVersion" => {
            if let Some(sha) = input.str_field("CodeSha256")
                && Some(sha) != f.config.str_field("CodeSha256")
            {
                return Err(invalid(format!(
                    "CodeSHA256 ({sha}) is different from current CodeSHA256 in $LATEST. Please try again with the CodeSHA256 in $LATEST."
                )));
            }
            if let Some(r) = input.str_field("RevisionId")
                && Some(r) != f.config.str_field("RevisionId")
            {
                return Err(err(
                    412,
                    "PreconditionFailedException",
                    "The Revision Id provided does not match the latest Revision Id. Call the GetFunction/GetAlias API to retrieve the latest Revision Id",
                ));
            }
            Value::Object(publish(ctx, f, input.str_field("Description")))
        }
        "ListVersionsByFunction" => {
            let mut items = vec![Value::Object(f.config.clone())];
            items.extend(f.versions.values().cloned().map(Value::Object));
            let (items, next) = page(items, input, "MaxItems");
            let mut out = json!({ "Versions": items });
            if let Some(n) = next {
                out["NextMarker"] = json!(n);
            }
            out
        }
        "AddPermission" | "RemovePermission" | "GetPolicy" => {
            permissions(ctx, f, qualifier, operation, input)?
        }
        _ => aliases(ctx, f, operation, input)?,
    }))
}
