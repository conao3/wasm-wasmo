use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::asl::validate;
use super::{
    Activity, Alias, Machine, Region, StepFunctions, Version, err, paged, secs, tag_list,
    valid_name,
};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt};

const KINDS: &[&str] = &["STANDARD", "EXPRESS"];

pub(super) fn no_machine(arn: &str) -> AwsError {
    err(
        "StateMachineDoesNotExist",
        format!("State Machine Does Not Exist: '{arn}'"),
    )
}

fn invalid_arn(arn: &str) -> AwsError {
    err("InvalidArn", format!("Invalid Arn: '{arn}'"))
}

/// Splits a state machine ARN into the unqualified ARN and the version
/// number or alias name after it.
pub(super) fn split_machine_arn(arn: &str) -> Result<(String, Option<String>), AwsError> {
    let parts: Vec<&str> = arn.split(':').collect();
    if parts.len() < 7 || parts[0] != "arn" || parts[2] != "states" || parts[5] != "stateMachine" {
        return Err(invalid_arn(arn));
    }
    let base = parts[..7].join(":");
    Ok((base, parts.get(7).map(|q| q.to_string())))
}

fn revision(ctx: &mut Ctx<'_>) -> String {
    ctx.uuid()
}

fn check_definition(definition: &str) -> Result<(), AwsError> {
    let diags = validate::check(definition);
    match diags.first() {
        None => Ok(()),
        Some(d) => Err(err(
            "InvalidDefinition",
            format!(
                "Invalid State Machine Definition: '{}: {} at {}'",
                d.code,
                d.message,
                if d.location.is_empty() {
                    "/"
                } else {
                    &d.location
                }
            ),
        )),
    }
}

fn publish(ctx: &mut Ctx<'_>, m: &mut Machine, description: Option<&str>) -> String {
    m.next_version += 1;
    let n = m.next_version;
    let arn = format!("{}:{n}", m.arn);
    m.versions.insert(
        n,
        Version {
            arn: arn.clone(),
            definition: m.definition.clone(),
            role_arn: m.role_arn.clone(),
            description: description.map(str::to_string),
            created: ctx.now_ms(),
            revision: m.revision.clone(),
        },
    );
    arn
}

fn describe(m: &Machine, version: Option<&Version>) -> Value {
    let mut out = json!({
        "stateMachineArn": version.map_or(&m.arn, |v| &v.arn),
        "name": m.name,
        "status": "ACTIVE",
        "definition": version.map_or(&m.definition, |v| &v.definition),
        "roleArn": version.map_or(&m.role_arn, |v| &v.role_arn),
        "type": m.kind,
        "creationDate": secs(version.map_or(m.created, |v| v.created)),
        "loggingConfiguration": m.logging,
        "tracingConfiguration": m.tracing,
        "revisionId": version.map_or(&m.revision, |v| &v.revision),
        "encryptionConfiguration": m.encryption,
    });
    if let Some(d) = version
        .and_then(|v| v.description.as_ref())
        .or(m.description.as_ref())
    {
        out["description"] = json!(d);
    }
    out
}

/// The machine and, for a version or alias ARN, the version it resolves to
/// (an alias picks one by its routing weights).
pub(super) fn resolve<'r>(
    ctx: &mut Ctx<'_>,
    region: &'r Region,
    arn: &str,
) -> Result<(&'r Machine, Option<&'r Version>, Option<String>), AwsError> {
    let (base, qualifier) = split_machine_arn(arn)?;
    let m = region.machines.get(&base).ok_or_else(|| no_machine(arn))?;
    let Some(q) = qualifier else {
        return Ok((m, None, None));
    };
    if let Ok(n) = q.parse::<u64>() {
        let v = m.versions.get(&n).ok_or_else(|| no_machine(arn))?;
        return Ok((m, Some(v), None));
    }
    let alias = region.aliases.get(arn).ok_or_else(|| {
        err(
            "ResourceNotFound",
            format!("State Machine Alias Does Not Exist: '{arn}'"),
        )
    })?;
    let mut b = [0u8; 4];
    ctx.random_bytes(&mut b);
    let mut draw = (u32::from_le_bytes(b) % 100) as i64;
    let mut chosen = None;
    for route in &alias.routing {
        let w = route.get("weight").and_then(Value::as_i64).unwrap_or(0);
        chosen = route.get("stateMachineVersionArn").and_then(Value::as_str);
        if draw < w {
            break;
        }
        draw -= w;
    }
    let version_arn = chosen.ok_or_else(|| no_machine(arn))?;
    let (_, n) = split_machine_arn(version_arn)?;
    let v = n
        .and_then(|n| n.parse::<u64>().ok())
        .and_then(|n| m.versions.get(&n))
        .ok_or_else(|| no_machine(version_arn))?;
    Ok((m, Some(v), Some(alias.arn.clone())))
}

fn create(sfn: &mut StepFunctions, ctx: &mut Ctx<'_>, input: &Value) -> Result<Value, AwsError> {
    let name = input.require_str("name")?.to_string();
    valid_name(&name)?;
    let definition = input.require_str("definition")?.to_string();
    let role_arn = input.require_str("roleArn")?.to_string();
    let kind = input.str_field("type").unwrap_or("STANDARD").to_string();
    if !KINDS.contains(&kind.as_str()) {
        return Err(err(
            "ValidationException",
            format!("Invalid type: '{kind}'"),
        ));
    }
    check_definition(&definition)?;
    let arn = ctx.arn("states", &format!("stateMachine:{name}"));
    let logging = input
        .get("loggingConfiguration")
        .cloned()
        .unwrap_or(json!({ "level": "OFF", "includeExecutionData": false }));
    let tracing = input
        .get("tracingConfiguration")
        .cloned()
        .unwrap_or(json!({ "enabled": false }));
    let encryption = input
        .get("encryptionConfiguration")
        .cloned()
        .unwrap_or(json!({ "type": "AWS_OWNED_KEY" }));
    let now = ctx.now_ms();
    let rev = revision(ctx);
    let publish_it = input.get("publish").and_then(Value::as_bool) == Some(true);
    let version_description = input.str_field("versionDescription").map(str::to_string);
    let tags = tag_list(input.get("tags").unwrap_or(&Value::Null));
    let region = sfn.region(ctx);
    if let Some(m) = region.machines.get(&arn) {
        if m.definition == definition && m.role_arn == role_arn && m.kind == kind {
            let mut out = json!({ "stateMachineArn": arn, "creationDate": secs(m.created) });
            if let Some((_, v)) = m.versions.last_key_value() {
                out["stateMachineVersionArn"] = json!(v.arn);
            }
            return Ok(out);
        }
        return Err(err(
            "StateMachineAlreadyExists",
            format!("State Machine Already Exists: '{arn}'"),
        ));
    }
    let mut m = Machine {
        arn: arn.clone(),
        name,
        kind,
        definition,
        role_arn,
        created: now,
        updated: None,
        revision: rev,
        description: None,
        logging,
        tracing,
        encryption,
        versions: BTreeMap::new(),
        next_version: 0,
    };
    let mut out = json!({ "stateMachineArn": arn, "creationDate": secs(now) });
    if publish_it {
        let v = publish(ctx, &mut m, version_description.as_deref());
        out["stateMachineVersionArn"] = json!(v);
    }
    let region = sfn.region(ctx);
    region.machines.insert(arn.clone(), m);
    if !tags.is_empty() {
        region.tags.insert(arn, tags);
    }
    Ok(out)
}

fn update(sfn: &mut StepFunctions, ctx: &mut Ctx<'_>, input: &Value) -> Result<Value, AwsError> {
    let arn = input.require_str("stateMachineArn")?.to_string();
    if let Some(d) = input.str_field("definition") {
        check_definition(d)?;
    }
    let now = ctx.now_ms();
    let rev = revision(ctx);
    let publish_it = input.get("publish").and_then(Value::as_bool) == Some(true);
    let description = input.str_field("versionDescription").map(str::to_string);
    let mut m = sfn
        .region(ctx)
        .machines
        .get(&arn)
        .cloned()
        .ok_or_else(|| no_machine(&arn))?;
    let mut changed = false;
    if let Some(d) = input.str_field("definition") {
        changed |= m.definition != d;
        m.definition = d.to_string();
    }
    if let Some(r) = input.str_field("roleArn") {
        changed |= m.role_arn != r;
        m.role_arn = r.to_string();
    }
    for (key, field) in [
        ("loggingConfiguration", &mut m.logging),
        ("tracingConfiguration", &mut m.tracing),
        ("encryptionConfiguration", &mut m.encryption),
    ] {
        if let Some(v) = input.get(key) {
            *field = v.clone();
        }
    }
    if changed {
        m.revision = rev;
    }
    m.updated = Some(now);
    let mut out = json!({ "updateDate": secs(now), "revisionId": m.revision });
    if publish_it {
        out["stateMachineVersionArn"] = json!(publish(ctx, &mut m, description.as_deref()));
    }
    sfn.region(ctx).machines.insert(arn, m);
    Ok(out)
}

pub(super) fn call(
    sfn: &mut StepFunctions,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    Ok(Some(match operation {
        "CreateStateMachine" => create(sfn, ctx, input)?,
        "UpdateStateMachine" => update(sfn, ctx, input)?,
        "DescribeStateMachine" => {
            let arn = input.require_str("stateMachineArn")?.to_string();
            let (base, qualifier) = split_machine_arn(&arn)?;
            let region = sfn.region(ctx);
            let m = region.machines.get(&base).ok_or_else(|| no_machine(&arn))?;
            match qualifier.and_then(|q| q.parse::<u64>().ok()) {
                Some(n) => describe(m, Some(m.versions.get(&n).ok_or_else(|| no_machine(&arn))?)),
                None => describe(m, None),
            }
        }
        "DeleteStateMachine" => {
            let arn = input.require_str("stateMachineArn")?.to_string();
            let (base, _) = split_machine_arn(&arn)?;
            let region = sfn.region(ctx);
            region.machines.remove(&base);
            region.aliases.retain(|_, a| a.machine_arn != base);
            region.tags.remove(&base);
            json!({})
        }
        "ListStateMachines" => {
            let items = sfn
                .region(ctx)
                .machines
                .values()
                .map(|m| {
                    json!({
                        "stateMachineArn": m.arn,
                        "name": m.name,
                        "type": m.kind,
                        "creationDate": secs(m.created),
                    })
                })
                .collect();
            paged("stateMachines", items, input)
        }
        "PublishStateMachineVersion" => {
            let arn = input.require_str("stateMachineArn")?.to_string();
            let description = input.str_field("description").map(str::to_string);
            let mut m = sfn
                .region(ctx)
                .machines
                .get(&arn)
                .cloned()
                .ok_or_else(|| no_machine(&arn))?;
            if let Some(rev) = input.str_field("revisionId")
                && rev != m.revision
            {
                return Err(err(
                    "ConflictException",
                    format!(
                        "Failed to publish the State Machine version for revision {rev}. The current State Machine revision is {}.",
                        m.revision
                    ),
                ));
            }
            let reuse = m
                .versions
                .values()
                .last()
                .filter(|v| v.revision == m.revision && v.description == description)
                .map(|v| (v.arn.clone(), v.created));
            let (version, created) = match reuse {
                Some(x) => x,
                None => {
                    let v = publish(ctx, &mut m, description.as_deref());
                    (v, ctx.now_ms())
                }
            };
            sfn.region(ctx).machines.insert(arn, m);
            json!({ "stateMachineVersionArn": version, "creationDate": secs(created) })
        }
        "ListStateMachineVersions" => {
            let arn = input.require_str("stateMachineArn")?.to_string();
            let m = sfn
                .region(ctx)
                .machines
                .get(&arn)
                .ok_or_else(|| no_machine(&arn))?;
            let items = m
                .versions
                .values()
                .rev()
                .map(
                    |v| json!({ "stateMachineVersionArn": v.arn, "creationDate": secs(v.created) }),
                )
                .collect();
            paged("stateMachineVersions", items, input)
        }
        "DeleteStateMachineVersion" => {
            let arn = input.require_str("stateMachineVersionArn")?.to_string();
            let (base, n) = split_machine_arn(&arn)?;
            let n = n
                .and_then(|n| n.parse::<u64>().ok())
                .ok_or_else(|| invalid_arn(&arn))?;
            let region = sfn.region(ctx);
            if region.aliases.values().any(|a| {
                a.routing
                    .iter()
                    .any(|r| r.get("stateMachineVersionArn").and_then(Value::as_str) == Some(&arn))
            }) {
                return Err(err(
                    "ConflictException",
                    format!(
                        "Version to be deleted must not be referenced by an alias. Current list of aliases referencing this version: {arn}"
                    ),
                ));
            }
            if let Some(m) = region.machines.get_mut(&base) {
                m.versions.remove(&n);
            }
            json!({})
        }
        "ValidateStateMachineDefinition" => {
            let definition = input.require_str("definition")?;
            let diagnostics: Vec<Value> = validate::check(definition)
                .into_iter()
                .map(|d| {
                    json!({
                        "severity": "ERROR",
                        "code": d.code,
                        "message": d.message,
                        "location": d.location,
                    })
                })
                .collect();
            json!({
                "result": if diagnostics.is_empty() { "OK" } else { "FAIL" },
                "diagnostics": diagnostics,
                "truncated": false,
            })
        }
        _ => return activities(sfn, ctx, operation, input),
    }))
}

fn activities(
    sfn: &mut StepFunctions,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    let no_activity = |arn: &str| {
        err(
            "ActivityDoesNotExist",
            format!("Activity Does Not Exist: '{arn}'"),
        )
    };
    Ok(Some(match operation {
        "CreateActivity" => {
            let name = input.require_str("name")?.to_string();
            valid_name(&name)?;
            let arn = ctx.arn("states", &format!("activity:{name}"));
            let now = ctx.now_ms();
            let tags = tag_list(input.get("tags").unwrap_or(&Value::Null));
            let encryption = input
                .get("encryptionConfiguration")
                .cloned()
                .unwrap_or(json!({ "type": "AWS_OWNED_KEY" }));
            let region = sfn.region(ctx);
            let created = region
                .activities
                .entry(arn.clone())
                .or_insert_with(|| Activity {
                    arn: arn.clone(),
                    name,
                    created: now,
                    encryption,
                })
                .created;
            if !tags.is_empty() {
                region.tags.entry(arn.clone()).or_default().extend(tags);
            }
            json!({ "activityArn": arn, "creationDate": secs(created) })
        }
        "DescribeActivity" => {
            let arn = input.require_str("activityArn")?;
            let a = sfn
                .region(ctx)
                .activities
                .get(arn)
                .ok_or_else(|| no_activity(arn))?;
            json!({
                "activityArn": a.arn,
                "name": a.name,
                "creationDate": secs(a.created),
                "encryptionConfiguration": a.encryption,
            })
        }
        "ListActivities" => {
            let items = sfn
                .region(ctx)
                .activities
                .values()
                .map(|a| json!({ "activityArn": a.arn, "name": a.name, "creationDate": secs(a.created) }))
                .collect();
            paged("activities", items, input)
        }
        "DeleteActivity" => {
            let arn = input.require_str("activityArn")?;
            let region = sfn.region(ctx);
            region.activities.remove(arn);
            region.tags.remove(arn);
            json!({})
        }
        "TagResource" | "UntagResource" | "ListTagsForResource" => {
            tags(sfn, ctx, operation, input)?
        }
        _ => return Ok(None),
    }))
}

fn tags(
    sfn: &mut StepFunctions,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> Result<Value, AwsError> {
    let arn = input.require_str("resourceArn")?.to_string();
    let region = sfn.region(ctx);
    let known = region.machines.contains_key(&arn)
        || region.activities.contains_key(&arn)
        || region.aliases.contains_key(&arn);
    if !known {
        return Err(err(
            "ResourceNotFound",
            format!("Resource not found: '{arn}'"),
        ));
    }
    let tags = region.tags.entry(arn).or_default();
    Ok(match operation {
        "TagResource" => {
            tags.extend(tag_list(input.get("tags").unwrap_or(&Value::Null)));
            json!({})
        }
        "UntagResource" => {
            for k in input
                .array_field("tagKeys")
                .iter()
                .filter_map(Value::as_str)
            {
                tags.remove(k);
            }
            json!({})
        }
        _ => json!({
            "tags": tags.iter().map(|(k, v)| json!({ "key": k, "value": v })).collect::<Vec<_>>(),
        }),
    })
}

pub(super) fn aliases(
    sfn: &mut StepFunctions,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    let no_alias = |arn: &str| {
        err(
            "ResourceNotFound",
            format!("State Machine Alias Does Not Exist: '{arn}'"),
        )
    };
    let routing = |input: &Value| -> Result<Vec<Value>, AwsError> {
        let items: Vec<Value> = input
            .array_field("routingConfiguration")
            .iter()
            .map(|r| {
                json!({
                    "stateMachineVersionArn": r.get("stateMachineVersionArn").and_then(Value::as_str).unwrap_or_default(),
                    "weight": r.get("weight").and_then(Value::as_i64).unwrap_or(100),
                })
            })
            .collect();
        let total: i64 = items.iter().filter_map(|r| r["weight"].as_i64()).sum();
        if items.is_empty() || items.len() > 2 || total != 100 {
            return Err(err(
                "ValidationException",
                "routingConfiguration must have one or two versions whose weights add up to 100",
            ));
        }
        Ok(items)
    };
    let now = ctx.now_ms();
    Ok(Some(match operation {
        "CreateStateMachineAlias" => {
            let name = input.require_str("name")?.to_string();
            valid_name(&name)?;
            let routing = routing(input)?;
            let first = routing[0]["stateMachineVersionArn"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let (base, _) = split_machine_arn(&first)?;
            let description = input.str_field("description").map(str::to_string);
            let region = sfn.region(ctx);
            let m = region
                .machines
                .get(&base)
                .ok_or_else(|| no_machine(&base))?;
            for r in &routing {
                let v = r["stateMachineVersionArn"].as_str().unwrap_or_default();
                let (b, n) = split_machine_arn(v)?;
                if b != base
                    || n.and_then(|n| n.parse::<u64>().ok())
                        .is_none_or(|n| !m.versions.contains_key(&n))
                {
                    return Err(err(
                        "ResourceNotFound",
                        format!("State Machine Version Does Not Exist: '{v}'"),
                    ));
                }
            }
            let arn = format!("{base}:{name}");
            if let Some(a) = region.aliases.get(&arn) {
                if a.description == description && a.routing == routing {
                    return Ok(Some(
                        json!({ "stateMachineAliasArn": arn, "creationDate": secs(a.created) }),
                    ));
                }
                return Err(err(
                    "ConflictException",
                    format!("Alias Already Exists: '{arn}'"),
                ));
            }
            region.aliases.insert(
                arn.clone(),
                Alias {
                    arn: arn.clone(),
                    name,
                    machine_arn: base,
                    description,
                    routing,
                    created: now,
                    updated: now,
                },
            );
            json!({ "stateMachineAliasArn": arn, "creationDate": secs(now) })
        }
        "DescribeStateMachineAlias" => {
            let arn = input.require_str("stateMachineAliasArn")?;
            let a = sfn
                .region(ctx)
                .aliases
                .get(arn)
                .ok_or_else(|| no_alias(arn))?;
            let mut out = json!({
                "stateMachineAliasArn": a.arn,
                "name": a.name,
                "routingConfiguration": a.routing,
                "creationDate": secs(a.created),
                "updateDate": secs(a.updated),
            });
            if let Some(d) = &a.description {
                out["description"] = json!(d);
            }
            out
        }
        "UpdateStateMachineAlias" => {
            let arn = input.require_str("stateMachineAliasArn")?.to_string();
            let new_routing = if input.get("routingConfiguration").is_some() {
                Some(routing(input)?)
            } else {
                None
            };
            let a = sfn
                .region(ctx)
                .aliases
                .get_mut(&arn)
                .ok_or_else(|| no_alias(&arn))?;
            if let Some(d) = input.get("description").and_then(Value::as_str) {
                a.description = Some(d.to_string());
            }
            if let Some(r) = new_routing {
                a.routing = r;
            }
            a.updated = now;
            json!({ "updateDate": secs(now) })
        }
        "DeleteStateMachineAlias" => {
            let arn = input.require_str("stateMachineAliasArn")?;
            sfn.region(ctx).aliases.remove(arn);
            json!({})
        }
        "ListStateMachineAliases" => {
            let arn = input.require_str("stateMachineArn")?.to_string();
            let (base, _) = split_machine_arn(&arn)?;
            let region = sfn.region(ctx);
            if !region.machines.contains_key(&base) {
                return Err(no_machine(&arn));
            }
            let items = region
                .aliases
                .values()
                .filter(|a| a.machine_arn == base)
                .map(|a| json!({ "stateMachineAliasArn": a.arn, "creationDate": secs(a.created) }))
                .collect();
            paged("stateMachineAliases", items, input)
        }
        _ => return Ok(None),
    }))
}
