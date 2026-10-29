use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{
    Archive, Record, RecordExt, Region, bus_name, dispatch, err, not_found, now_secs, paginate,
    pattern, valid_name, validation,
};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt, OpResult};

const COPIED: &[&str] = &[
    "Description",
    "KmsKeyIdentifier",
    "DeadLetterConfig",
    "EventPattern",
    "RetentionDays",
    "AuthorizationType",
    "AuthParameters",
    "InvocationConnectivityParameters",
    "ConnectionArn",
    "InvocationEndpoint",
    "HttpMethod",
    "InvocationRateLimitPerSecond",
    "RoutingConfig",
    "ReplicationConfig",
    "EventBuses",
    "RoleArn",
    "EventSourceArn",
    "Destination",
    "EventStartTime",
    "EventEndTime",
];

fn copy_fields(record: &mut Record, input: &Value) {
    for k in COPIED {
        if let Some(v) = input.get(*k).filter(|v| !v.is_null()) {
            record.insert((*k).into(), v.clone());
        }
    }
}

fn exists(kind: &str, name: &str) -> AwsError {
    err(
        "ResourceAlreadyExistsException",
        format!("{kind} {name} already exists."),
    )
}

fn missing(kind: &str, name: &str) -> AwsError {
    not_found(format!("{kind} {name} does not exist."))
}

pub(super) fn add_tags(region: &mut Region, arn: &str, input: &Value) {
    let tags = input.array_field("Tags");
    if tags.is_empty() {
        return;
    }
    let entry = region.tags.entry(arn.to_string()).or_default();
    for t in tags {
        if let Some(k) = t.str_field("Key") {
            entry.insert(
                k.to_string(),
                t.get("Value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            );
        }
    }
}

pub(super) fn bus_record(ctx: &Ctx<'_>, name: &str, input: &Value) -> Record {
    let now = now_secs(ctx);
    let mut r = Record::new();
    r.insert("Name".into(), json!(name));
    r.insert(
        "Arn".into(),
        json!(ctx.arn("events", &format!("event-bus/{name}"))),
    );
    r.insert("CreationTime".into(), json!(now));
    r.insert("LastModifiedTime".into(), json!(now));
    copy_fields(&mut r, input);
    r
}

fn bus_op(ctx: &mut Ctx<'_>, region: &mut Region, operation: &str, input: &Value) -> OpResult {
    Ok(match operation {
        "CreateEventBus" => {
            let name = input.require_str("Name")?;
            let partner = name.starts_with("aws.partner/");
            if name == "default"
                || (!partner && !valid_name(name, 256))
                || name.contains('/') && !partner
            {
                return Err(validation(format!("Event bus name {name} is not valid.")));
            }
            if partner && input.str_field("EventSourceName") != Some(name) {
                return Err(validation(
                    "Event source name must match the event bus name for partner event buses.",
                ));
            }
            if region.buses.contains_key(name) {
                return Err(exists("Event bus", name));
            }
            let record = bus_record(ctx, name, input);
            let arn = record.str_field("Arn").unwrap_or_default().to_string();
            region.buses.insert(name.to_string(), record.clone());
            add_tags(region, &arn, input);
            let mut out = Value::Object(record);
            out["EventBusArn"] = json!(arn);
            out
        }
        "DescribeEventBus" => Value::Object(region.bus(bus_name(input.str_field("Name")))?.clone()),
        "UpdateEventBus" => {
            let name = bus_name(input.str_field("Name")).to_string();
            let now = now_secs(ctx);
            let bus = region
                .buses
                .get_mut(&name)
                .ok_or_else(|| missing("Event bus", &name))?;
            copy_fields(bus, input);
            bus.insert("LastModifiedTime".into(), json!(now));
            Value::Object(bus.clone())
        }
        "ListEventBuses" => {
            let prefix = input.str_field("NamePrefix").unwrap_or_default();
            let buses: Vec<Value> = region
                .buses
                .iter()
                .filter(|(n, _)| n.starts_with(prefix))
                .map(|(_, b)| Value::Object(b.clone()))
                .collect();
            paginate(buses, input, "EventBuses")
        }
        "DeleteEventBus" => {
            let name = input.require_str("Name")?;
            if name == "default" {
                return Err(validation("Cannot delete event bus default."));
            }
            if let Some(bus) = region.buses.remove(name) {
                region.rules.retain(|(b, _), _| b != name);
                region.tags.remove(bus.str_field("Arn").unwrap_or_default());
            }
            json!({})
        }
        "PutPermission" => {
            let name = bus_name(input.str_field("EventBusName")).to_string();
            let arn = region
                .bus(&name)?
                .str_field("Arn")
                .unwrap_or_default()
                .to_string();
            let statement = if let Some(policy) = input.str_field("Policy") {
                let doc: Value = serde_json::from_str(policy)
                    .map_err(|_| validation("Policy is not valid JSON."))?;
                set_policy(region, &name, doc);
                return Ok(json!({}));
            } else {
                let sid = input.require_str("StatementId")?;
                let principal = input.require_str("Principal")?;
                let mut s = json!({
                    "Sid": sid,
                    "Effect": "Allow",
                    "Principal": if principal == "*" { json!("*") } else { json!({"AWS": format!("arn:aws:iam::{principal}:root")}) },
                    "Action": input.str_field("Action").unwrap_or("events:PutEvents"),
                    "Resource": arn,
                });
                if let Some(c) = input.get("Condition") {
                    s["Condition"] = json!({"StringEquals": {c.str_field("Key").unwrap_or_default(): c.str_field("Value").unwrap_or_default()}});
                }
                s
            };
            let mut doc = policy_of(region, &name);
            let statements = doc["Statement"].as_array_mut().expect("statement array");
            statements.retain(|s| s.str_field("Sid") != statement.str_field("Sid"));
            statements.push(statement);
            set_policy(region, &name, doc);
            json!({})
        }
        "RemovePermission" => {
            let name = bus_name(input.str_field("EventBusName")).to_string();
            region.bus(&name)?;
            if input.get("RemoveAllPermissions").and_then(Value::as_bool) == Some(true) {
                if let Some(bus) = region.buses.get_mut(&name) {
                    bus.remove("Policy");
                }
                return Ok(json!({}));
            }
            let sid = input.require_str("StatementId")?;
            let mut doc = policy_of(region, &name);
            let statements = doc["Statement"].as_array_mut().expect("statement array");
            let before = statements.len();
            statements.retain(|s| s.str_field("Sid") != Some(sid));
            if statements.len() == before {
                return Err(not_found("Statement with the provided id does not exist."));
            }
            set_policy(region, &name, doc);
            json!({})
        }
        _ => unreachable!("bus_op called with {operation}"),
    })
}

fn policy_of(region: &Region, bus: &str) -> Value {
    region
        .buses
        .get(bus)
        .and_then(|b| b.str_field("Policy"))
        .and_then(|p| serde_json::from_str::<Value>(p).ok())
        .filter(|p| p.get("Statement").is_some_and(Value::is_array))
        .unwrap_or_else(|| json!({"Version": "2012-10-17", "Statement": []}))
}

fn set_policy(region: &mut Region, bus: &str, doc: Value) {
    if let Some(b) = region.buses.get_mut(bus) {
        b.insert("Policy".into(), json!(doc.to_string()));
    }
}

fn archive_op(ctx: &mut Ctx<'_>, region: &mut Region, operation: &str, input: &Value) -> OpResult {
    let name = input
        .str_field("ArchiveName")
        .unwrap_or_default()
        .to_string();
    Ok(match operation {
        "CreateArchive" => {
            input.require_str("ArchiveName")?;
            let source = input.require_str("EventSourceArn")?;
            if !valid_name(&name, 48) {
                return Err(validation(format!("Archive name {name} is not valid.")));
            }
            if region.archives.contains_key(&name) {
                return Err(exists("Archive", &name));
            }
            let bus = bus_name(Some(source));
            if region.buses.get(bus).and_then(|b| b.str_field("Arn")) != Some(source) {
                return Err(missing("Event bus", bus));
            }
            if let Some(p) = input.str_field("EventPattern") {
                pattern::parse(p).map_err(|e| {
                    err(
                        "InvalidEventPatternException",
                        format!("Event pattern is not valid. Reason: {e}"),
                    )
                })?;
            }
            let mut record = Record::new();
            record.insert("ArchiveName".into(), json!(name));
            record.insert(
                "ArchiveArn".into(),
                json!(ctx.arn("events", &format!("archive/{name}"))),
            );
            record.insert("State".into(), json!("ENABLED"));
            record.insert("RetentionDays".into(), json!(0));
            record.insert("SizeBytes".into(), json!(0));
            record.insert("EventCount".into(), json!(0));
            record.insert("CreationTime".into(), json!(now_secs(ctx)));
            copy_fields(&mut record, input);
            region.archives.insert(
                name,
                Archive {
                    record: record.clone(),
                    events: Vec::new(),
                },
            );
            Value::Object(record)
        }
        "DescribeArchive" => Value::Object(
            region
                .archives
                .get(&name)
                .ok_or_else(|| missing("Archive", &name))?
                .record
                .clone(),
        ),
        "UpdateArchive" => {
            let archive = region
                .archives
                .get_mut(&name)
                .ok_or_else(|| missing("Archive", &name))?;
            if let Some(p) = input.str_field("EventPattern") {
                pattern::parse(p).map_err(|e| {
                    err(
                        "InvalidEventPatternException",
                        format!("Event pattern is not valid. Reason: {e}"),
                    )
                })?;
            }
            for k in [
                "Description",
                "EventPattern",
                "RetentionDays",
                "KmsKeyIdentifier",
            ] {
                if let Some(v) = input.get(k) {
                    archive.record.insert(k.into(), v.clone());
                }
            }
            Value::Object(archive.record.clone())
        }
        "DeleteArchive" => {
            region
                .archives
                .remove(&name)
                .ok_or_else(|| missing("Archive", &name))?;
            json!({})
        }
        "ListArchives" => {
            let prefix = input.str_field("NamePrefix").unwrap_or_default();
            let archives: Vec<Value> = region
                .archives
                .values()
                .map(|a| &a.record)
                .filter(|r| {
                    r.str_field("ArchiveName")
                        .unwrap_or_default()
                        .starts_with(prefix)
                })
                .filter(|r| {
                    ["EventSourceArn", "State"]
                        .iter()
                        .all(|k| input.str_field(k).is_none_or(|v| r.str_field(k) == Some(v)))
                })
                .map(|r| Value::Object(r.clone()))
                .collect();
            paginate(archives, input, "Archives")
        }
        _ => unreachable!("archive_op called with {operation}"),
    })
}

fn replay_op(ctx: &mut Ctx<'_>, region: &mut Region, operation: &str, input: &Value) -> OpResult {
    let name = input
        .str_field("ReplayName")
        .unwrap_or_default()
        .to_string();
    let replay_missing = || missing("Replay", &name);
    Ok(match operation {
        "StartReplay" => {
            input.require_str("ReplayName")?;
            if region.replays.contains_key(&name) {
                return Err(exists("Replay", &name));
            }
            let source = input.require_str("EventSourceArn")?;
            let start = input
                .get("EventStartTime")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let end = input
                .get("EventEndTime")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            if end <= start {
                return Err(validation(
                    "Parameter EventEndTime is not valid. Reason: EventStartTime must be before EventEndTime.",
                ));
            }
            let destination = input
                .get("Destination")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let archive_name = source.rsplit('/').next().unwrap_or_default();
            let archive = region
                .archives
                .get(archive_name)
                .filter(|a| a.record.str_field("ArchiveArn") == Some(source))
                .ok_or_else(|| missing("Archive", archive_name))?;
            let dest_bus = bus_name(destination.str_field("Arn")).to_string();
            if archive.record.str_field("EventSourceArn") != destination.str_field("Arn") {
                return Err(validation(
                    "Parameter Destination.Arn is not valid. Reason: Cross event bus replay is not permitted.",
                ));
            }
            let events: Vec<(f64, Value)> = archive
                .events
                .iter()
                .filter(|(t, _)| (start..=end).contains(t))
                .cloned()
                .collect();
            let now = now_secs(ctx);
            for (_, e) in &events {
                let mut replayed = e.clone();
                replayed["replay-name"] = json!(name);
                dispatch(ctx, region, &dest_bus, &replayed, Some(&destination));
            }
            let mut record = Record::new();
            record.insert("ReplayName".into(), json!(name));
            record.insert(
                "ReplayArn".into(),
                json!(ctx.arn("events", &format!("replay/{name}"))),
            );
            record.insert("State".into(), json!("COMPLETED"));
            record.insert("ReplayStartTime".into(), json!(now));
            record.insert("ReplayEndTime".into(), json!(now));
            if let Some((t, _)) = events.last() {
                record.insert("EventLastReplayedTime".into(), json!(t));
            }
            copy_fields(&mut record, input);
            region.replays.insert(name, record.clone());
            let mut out = Value::Object(record);
            out["State"] = json!("STARTING");
            out
        }
        "DescribeReplay" => Value::Object(
            region
                .replays
                .get(&name)
                .ok_or_else(replay_missing)?
                .clone(),
        ),
        "CancelReplay" => {
            let replay = region.replays.get(&name).ok_or_else(replay_missing)?;
            if !matches!(replay.str_field("State"), Some("STARTING" | "RUNNING")) {
                return Err(err(
                    "IllegalStatusException",
                    format!("Replay {name} is not in a valid state for this operation."),
                ));
            }
            json!({})
        }
        "ListReplays" => {
            let prefix = input.str_field("NamePrefix").unwrap_or_default();
            let replays: Vec<Value> = region
                .replays
                .values()
                .filter(|r| {
                    r.str_field("ReplayName")
                        .unwrap_or_default()
                        .starts_with(prefix)
                })
                .filter(|r| {
                    ["EventSourceArn", "State"]
                        .iter()
                        .all(|k| input.str_field(k).is_none_or(|v| r.str_field(k) == Some(v)))
                })
                .map(|r| Value::Object(r.clone()))
                .collect();
            paginate(replays, input, "Replays")
        }
        _ => unreachable!("replay_op called with {operation}"),
    })
}

/// Connections, API destinations, and global endpoints: named records that are
/// stored and described as given.
fn named_op(ctx: &mut Ctx<'_>, region: &mut Region, operation: &str, input: &Value) -> OpResult {
    let (kind, list_key, arn_key, state_key, state) =
        if operation.ends_with("Connection") || operation == "ListConnections" {
            (
                "Connection",
                "Connections",
                "ConnectionArn",
                "ConnectionState",
                "AUTHORIZED",
            )
        } else if operation.contains("ApiDestination") {
            (
                "ApiDestination",
                "ApiDestinations",
                "ApiDestinationArn",
                "ApiDestinationState",
                "ACTIVE",
            )
        } else {
            ("Endpoint", "Endpoints", "Arn", "State", "ACTIVE")
        };
    let store = match kind {
        "Connection" => &mut region.connections,
        "ApiDestination" => &mut region.api_destinations,
        _ => &mut region.endpoints,
    };
    let name = input.str_field("Name").unwrap_or_default().to_string();
    let now = now_secs(ctx);
    let verb = operation.split(kind).next().unwrap_or_default();
    Ok(match verb {
        "Create" => {
            input.require_str("Name")?;
            if store.contains_key(&name) {
                return Err(exists(kind, &name));
            }
            let resource = match kind {
                "Connection" => format!("connection/{name}/{}", ctx.uuid()),
                "ApiDestination" => format!("api-destination/{name}/{}", ctx.uuid()),
                _ => format!("endpoint/{name}"),
            };
            let mut r = Record::new();
            r.insert("Name".into(), json!(name));
            r.insert(arn_key.into(), json!(ctx.arn("events", &resource)));
            r.insert(state_key.into(), json!(state));
            r.insert("CreationTime".into(), json!(now));
            r.insert("LastModifiedTime".into(), json!(now));
            match kind {
                "Connection" => {
                    r.insert("LastAuthorizedTime".into(), json!(now));
                    let suffix = ctx.uuid();
                    r.insert(
                        "SecretArn".into(),
                        json!(ctx.arn(
                            "secretsmanager",
                            &format!("secret:events!connection/{name}/{suffix}")
                        )),
                    );
                }
                "Endpoint" => {
                    let id = ctx.uuid().replace('-', "")[..10].to_string();
                    r.insert("EndpointId".into(), json!(id));
                    r.insert(
                        "EndpointUrl".into(),
                        json!(format!("https://{id}.endpoint.events.amazonaws.com")),
                    );
                }
                _ => {}
            }
            copy_fields(&mut r, input);
            store.insert(name, r.clone());
            Value::Object(r)
        }
        "Describe" => Value::Object(
            store
                .get(&name)
                .ok_or_else(|| missing(kind, &name))?
                .clone(),
        ),
        "Update" | "Deauthorize" => {
            let r = store.get_mut(&name).ok_or_else(|| missing(kind, &name))?;
            copy_fields(r, input);
            r.insert("LastModifiedTime".into(), json!(now));
            if verb == "Deauthorize" {
                r.insert(state_key.into(), json!("DEAUTHORIZED"));
            } else if kind == "Connection" {
                r.insert(state_key.into(), json!("AUTHORIZED"));
                r.insert("LastAuthorizedTime".into(), json!(now));
            }
            Value::Object(r.clone())
        }
        "Delete" => {
            let mut r = store.remove(&name).ok_or_else(|| missing(kind, &name))?;
            if kind == "Connection" {
                r.insert(state_key.into(), json!("DELETING"));
            }
            Value::Object(r)
        }
        _ => {
            let prefix = input.str_field("NamePrefix").unwrap_or_default();
            let items: Vec<Value> = store
                .values()
                .filter(|r| r.str_field("Name").unwrap_or_default().starts_with(prefix))
                .filter(|r| {
                    input
                        .str_field("ConnectionState")
                        .is_none_or(|s| r.str_field("ConnectionState") == Some(s))
                        && input
                            .str_field("ConnectionArn")
                            .is_none_or(|s| r.str_field("ConnectionArn") == Some(s))
                })
                .map(|r| Value::Object(r.clone()))
                .collect();
            paginate(items, &limit_from(input), list_key)
        }
    })
}

fn limit_from(input: &Value) -> Value {
    let mut v = input.clone();
    if let Some(max) = input.get("MaxResults") {
        v["Limit"] = max.clone();
    }
    v
}

fn partner_op(ctx: &mut Ctx<'_>, region: &mut Region, operation: &str, input: &Value) -> OpResult {
    let name = input.str_field("Name").unwrap_or_default().to_string();
    Ok(match operation {
        "CreatePartnerEventSource" => {
            input.require_str("Name")?;
            let account = input.require_str("Account")?;
            if region.partner_sources.contains_key(&name) {
                return Err(exists("Partner event source", &name));
            }
            let arn = format!("arn:aws:events:{}::event-source/{name}", ctx.region);
            let mut r = Record::new();
            r.insert("Name".into(), json!(name));
            r.insert("Arn".into(), json!(arn));
            r.insert("Account".into(), json!(account));
            r.insert("CreationTime".into(), json!(now_secs(ctx)));
            r.insert("State".into(), json!("PENDING"));
            r.insert(
                "CreatedBy".into(),
                json!(name.split('/').nth(1).unwrap_or_default()),
            );
            region.partner_sources.insert(name.clone(), r.clone());
            region.event_sources.insert(name, r);
            json!({ "EventSourceArn": arn })
        }
        "DeletePartnerEventSource" => {
            region.partner_sources.remove(&name);
            region.event_sources.remove(&name);
            json!({})
        }
        "DescribePartnerEventSource" => Value::Object(
            region
                .partner_sources
                .get(&name)
                .ok_or_else(|| missing("Event source", &name))?
                .clone(),
        ),
        "ListPartnerEventSources" => {
            let prefix = input.require_str("NamePrefix")?;
            let items: Vec<Value> = region
                .partner_sources
                .values()
                .filter(|r| r.str_field("Name").unwrap_or_default().starts_with(prefix))
                .map(|r| json!({"Arn": r["Arn"], "Name": r["Name"]}))
                .collect();
            paginate(items, &limit_from(input), "PartnerEventSources")
        }
        "ListPartnerEventSourceAccounts" => {
            let source = input.require_str("EventSourceName")?;
            let items: Vec<Value> = region
                .partner_sources
                .get(source)
                .map(|r| vec![Value::Object(r.clone())])
                .unwrap_or_default();
            paginate(items, &limit_from(input), "PartnerEventSourceAccounts")
        }
        "ActivateEventSource" | "DeactivateEventSource" => {
            let source = region
                .event_sources
                .get_mut(&name)
                .ok_or_else(|| missing("Event source", &name))?;
            let state = if operation == "ActivateEventSource" {
                "ACTIVE"
            } else {
                "DISABLED"
            };
            source.insert("State".into(), json!(state));
            json!({})
        }
        "DescribeEventSource" => Value::Object(
            region
                .event_sources
                .get(&name)
                .ok_or_else(|| missing("Event source", &name))?
                .clone(),
        ),
        "ListEventSources" => {
            let prefix = input.str_field("NamePrefix").unwrap_or_default();
            let items: Vec<Value> = region
                .event_sources
                .values()
                .filter(|r| r.str_field("Name").unwrap_or_default().starts_with(prefix))
                .map(|r| Value::Object(r.clone()))
                .collect();
            paginate(items, &limit_from(input), "EventSources")
        }
        "PutPartnerEvents" => {
            let entries: Vec<Value> = input
                .array_field("Entries")
                .iter()
                .map(|_| json!({ "EventId": ctx.uuid() }))
                .collect();
            json!({ "FailedEntryCount": 0, "Entries": entries })
        }
        _ => unreachable!("partner_op called with {operation}"),
    })
}

fn resource_exists(region: &Region, arn: &str) -> bool {
    region
        .buses
        .values()
        .any(|b| b.str_field("Arn") == Some(arn))
        || region.rules.values().any(|r| r.arn() == arn)
}

fn tag_op(region: &mut Region, operation: &str, input: &Value) -> OpResult {
    let arn = input.require_str("ResourceARN")?.to_string();
    if !resource_exists(region, &arn) {
        return Err(not_found(format!("Resource {arn} does not exist.")));
    }
    Ok(match operation {
        "TagResource" => {
            add_tags(region, &arn, input);
            json!({})
        }
        "UntagResource" => {
            if let Some(tags) = region.tags.get_mut(&arn) {
                for k in input
                    .array_field("TagKeys")
                    .iter()
                    .filter_map(Value::as_str)
                {
                    tags.remove(k);
                }
            }
            json!({})
        }
        _ => {
            let tags: Vec<Value> = region
                .tags
                .get(&arn)
                .unwrap_or(&BTreeMap::new())
                .iter()
                .map(|(k, v)| json!({"Key": k, "Value": v}))
                .collect();
            json!({ "Tags": tags })
        }
    })
}

pub(super) fn call(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> OpResult {
    match operation {
        "CreateEventBus" | "DescribeEventBus" | "UpdateEventBus" | "ListEventBuses"
        | "DeleteEventBus" | "PutPermission" | "RemovePermission" => {
            bus_op(ctx, region, operation, input)
        }
        "CreateArchive" | "DescribeArchive" | "UpdateArchive" | "DeleteArchive"
        | "ListArchives" => archive_op(ctx, region, operation, input),
        "StartReplay" | "DescribeReplay" | "CancelReplay" | "ListReplays" => {
            replay_op(ctx, region, operation, input)
        }
        "CreateConnection"
        | "DescribeConnection"
        | "UpdateConnection"
        | "DeleteConnection"
        | "DeauthorizeConnection"
        | "ListConnections"
        | "CreateApiDestination"
        | "DescribeApiDestination"
        | "UpdateApiDestination"
        | "DeleteApiDestination"
        | "ListApiDestinations"
        | "CreateEndpoint"
        | "DescribeEndpoint"
        | "UpdateEndpoint"
        | "DeleteEndpoint"
        | "ListEndpoints" => named_op(ctx, region, operation, input),
        "CreatePartnerEventSource"
        | "DeletePartnerEventSource"
        | "DescribePartnerEventSource"
        | "ListPartnerEventSources"
        | "ListPartnerEventSourceAccounts"
        | "ActivateEventSource"
        | "DeactivateEventSource"
        | "DescribeEventSource"
        | "ListEventSources"
        | "PutPartnerEvents" => partner_op(ctx, region, operation, input),
        "TagResource" | "UntagResource" | "ListTagsForResource" => tag_op(region, operation, input),
        other => Err(err(
            "UnknownOperationException",
            format!("wasmo: {other} is not implemented"),
        )),
    }
}
