mod filter;
mod insights;

use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::gzip;
use crate::codec::AwsError;
use crate::codec::time::format_iso8601;
use crate::embedded_model;
use crate::service::{Ctx, Delivery, InputExt, OpResult, Service, ServiceModel};
use insights::Row;

#[derive(Debug, Clone)]
struct Event {
    timestamp: i64,
    message: String,
    ingestion: i64,
    id: String,
}

#[derive(Debug, Clone)]
struct LogStream {
    created: i64,
    events: Vec<Event>,
}

#[derive(Debug, Clone)]
struct Transformation {
    name: String,
    namespace: String,
    value: String,
    default: Option<f64>,
    dimensions: BTreeMap<String, String>,
    unit: Option<String>,
}

#[derive(Debug, Clone)]
struct MetricFilter {
    pattern: String,
    created: i64,
    transformations: Vec<Transformation>,
}

#[derive(Debug, Clone)]
struct SubscriptionFilter {
    pattern: String,
    destination: String,
    role: Option<String>,
    distribution: Option<String>,
    created: i64,
}

#[derive(Debug, Clone, Default)]
struct Group {
    name: String,
    created: i64,
    streams: BTreeMap<String, LogStream>,
    retention: Option<i64>,
    tags: BTreeMap<String, String>,
    kms_key: Option<String>,
    class: String,
    deletion_protection: bool,
    data_protection: Option<(String, i64)>,
    index_policy: Option<(String, i64)>,
    transformer: Option<(Value, i64, i64)>,
    metric_filters: BTreeMap<String, MetricFilter>,
    subscriptions: BTreeMap<String, SubscriptionFilter>,
}

#[derive(Debug, Clone)]
struct Query {
    id: String,
    query: String,
    groups: Vec<String>,
    start: Option<i64>,
    end: Option<i64>,
    limit: Option<usize>,
    created: i64,
    status: String,
    results: Option<(Vec<Row>, usize)>,
}

#[derive(Debug, Default)]
struct Region {
    groups: BTreeMap<String, Group>,
    destinations: BTreeMap<String, Value>,
    queries: Vec<Query>,
    query_definitions: BTreeMap<String, Value>,
    resource_policies: BTreeMap<String, Value>,
    account_policies: BTreeMap<(String, String), Value>,
    export_tasks: Vec<Value>,
    delivery_sources: BTreeMap<String, Value>,
    delivery_destinations: BTreeMap<String, Value>,
    delivery_policies: BTreeMap<String, String>,
    deliveries: BTreeMap<String, Value>,
    anomaly_detectors: BTreeMap<String, Value>,
    resource_tags: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Default)]
pub struct Logs {
    regions: HashMap<(String, String), Region>,
}

fn invalid(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "InvalidParameterException", message)
}

fn not_found(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "ResourceNotFoundException", message)
}

fn exists(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "ResourceAlreadyExistsException", message)
}

fn required<'a>(input: &'a Value, key: &str) -> Result<&'a str, AwsError> {
    input.str_field(key).ok_or_else(|| {
        invalid(format!(
            "1 validation error detected: Value null at '{key}' failed to satisfy constraint: Member must not be null"
        ))
    })
}

fn group_name(identifier: &str) -> &str {
    identifier
        .split_once(":log-group:")
        .map_or(identifier, |(_, rest)| {
            rest.split(':').next().unwrap_or(rest)
        })
}

fn group_arn(ctx: &Ctx<'_>, name: &str) -> String {
    ctx.arn("logs", &format!("log-group:{name}"))
}

fn tag_map(v: Option<&Map<String, Value>>) -> BTreeMap<String, String> {
    v.into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect()
}

fn strings(v: &[Value]) -> Vec<String> {
    v.iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn page<T: Clone>(items: Vec<T>, input: &Value, default: usize) -> (Vec<T>, Option<String>) {
    let start = input
        .str_field("nextToken")
        .and_then(|t| STANDARD.decode(t).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let limit = input
        .i64_field("limit")
        .map_or(default, |l| l.max(1) as usize);
    let end = (start + limit).min(items.len());
    let next = (end < items.len()).then(|| STANDARD.encode(end.to_string()));
    (items[start..end].to_vec(), next)
}

fn with_next(mut out: Value, next: Option<String>) -> Value {
    if let Some(n) = next {
        out["nextToken"] = json!(n);
    }
    out
}

fn event_id(ctx: &mut Ctx<'_>) -> String {
    let mut b = [0u8; 28];
    ctx.random_bytes(&mut b);
    b.iter().map(|x| format!("{:02}", x % 100)).collect()
}

fn pattern(p: &str) -> Result<filter::Pattern, AwsError> {
    filter::parse(p).map_err(|e| invalid(format!("Invalid filter pattern: {e}")))
}

impl Region {
    fn group(&self, name: &str) -> Result<&Group, AwsError> {
        self.groups
            .get(group_name(name))
            .ok_or_else(|| not_found("The specified log group does not exist."))
    }

    fn group_mut(&mut self, name: &str) -> Result<&mut Group, AwsError> {
        self.groups
            .get_mut(group_name(name))
            .ok_or_else(|| not_found("The specified log group does not exist."))
    }

    fn group_ref(&self, input: &Value) -> Result<&Group, AwsError> {
        match (
            input.str_field("logGroupName"),
            input.str_field("logGroupIdentifier"),
        ) {
            (Some(_), Some(_)) => Err(invalid(
                "LogGroup name and LogGroup ARN are mutually exclusive parameters.",
            )),
            (Some(n), None) | (None, Some(n)) => self.group(n),
            (None, None) => Err(invalid(
                "Should provide either a LogGroup name or LogGroup ARN.",
            )),
        }
    }
}

fn group_view(ctx: &Ctx<'_>, g: &Group) -> Value {
    let arn = group_arn(ctx, &g.name);
    let stored: usize = g
        .streams
        .values()
        .flat_map(|s| &s.events)
        .map(|e| e.message.len())
        .sum();
    let mut v = json!({
        "logGroupName": g.name,
        "creationTime": g.created,
        "metricFilterCount": g.metric_filters.len(),
        "arn": format!("{arn}:*"),
        "logGroupArn": arn,
        "storedBytes": stored,
        "logGroupClass": g.class,
        "deletionProtectionEnabled": g.deletion_protection,
    });
    if let Some(r) = g.retention {
        v["retentionInDays"] = json!(r);
    }
    if let Some(k) = &g.kms_key {
        v["kmsKeyId"] = json!(k);
    }
    if g.data_protection.is_some() {
        v["dataProtectionStatus"] = json!("ACTIVATED");
    }
    v
}

fn stream_view(ctx: &Ctx<'_>, group: &str, name: &str, s: &LogStream) -> Value {
    let mut v = json!({
        "logStreamName": name,
        "creationTime": s.created,
        "arn": ctx.arn("logs", &format!("log-group:{group}:log-stream:{name}")),
        "storedBytes": 0,
        "uploadSequenceToken": format!("{:056}", s.events.len()),
    });
    if let (Some(first), Some(last)) = (
        s.events.iter().map(|e| e.timestamp).min(),
        s.events.iter().map(|e| e.timestamp).max(),
    ) {
        v["firstEventTimestamp"] = json!(first);
        v["lastEventTimestamp"] = json!(last);
        v["lastIngestionTime"] = json!(s.events.iter().map(|e| e.ingestion).max());
    }
    v
}

fn transformations(input: &Value) -> Result<Vec<Transformation>, AwsError> {
    let list = input.array_field("metricTransformations");
    if list.is_empty() {
        return Err(invalid("metricTransformations is required."));
    }
    list.iter()
        .map(|t| {
            Ok(Transformation {
                name: required(t, "metricName")?.to_string(),
                namespace: required(t, "metricNamespace")?.to_string(),
                value: required(t, "metricValue")?.to_string(),
                default: t.get("defaultValue").and_then(Value::as_f64),
                dimensions: tag_map(t.obj_field("dimensions")),
                unit: t.str_field("unit").map(str::to_string),
            })
        })
        .collect()
}

fn metric_filter_view(group: &str, name: &str, f: &MetricFilter) -> Value {
    json!({
        "filterName": name,
        "filterPattern": f.pattern,
        "logGroupName": group,
        "creationTime": f.created,
        "metricTransformations": f.transformations.iter().map(|t| {
            let mut v = json!({
                "metricName": t.name,
                "metricNamespace": t.namespace,
                "metricValue": t.value,
            });
            if let Some(d) = t.default {
                v["defaultValue"] = json!(d);
            }
            if !t.dimensions.is_empty() {
                v["dimensions"] = json!(t.dimensions);
            }
            if let Some(u) = &t.unit {
                v["unit"] = json!(u);
            }
            v
        }).collect::<Vec<_>>(),
    })
}

/// Turns matched events into CloudWatch datapoints, delivered as a
/// PutMetricData request body.
fn emit_metrics(ctx: &mut Ctx<'_>, group: &Group, events: &[Event]) {
    for f in group.metric_filters.values() {
        let Ok(p) = filter::parse(&f.pattern) else {
            continue;
        };
        for t in &f.transformations {
            let mut data = Vec::new();
            for e in events {
                let Some(caps) = p.matches(&e.message) else {
                    continue;
                };
                let value = if t.value.starts_with('$') {
                    caps.select(&t.value).and_then(|v| v.parse::<f64>().ok())
                } else {
                    t.value.parse().ok()
                };
                let Some(value) = value else {
                    continue;
                };
                let dims: Option<Vec<Value>> = t
                    .dimensions
                    .iter()
                    .map(|(k, sel)| Some(json!({ "Name": k, "Value": caps.select(sel)? })))
                    .collect();
                let Some(dims) = dims else {
                    continue;
                };
                let mut d = json!({
                    "MetricName": t.name,
                    "Value": value,
                    "Timestamp": e.timestamp as f64 / 1000.0,
                    "Unit": t.unit.as_deref().unwrap_or("None"),
                });
                if !dims.is_empty() {
                    d["Dimensions"] = Value::Array(dims);
                }
                data.push(d);
            }
            if data.is_empty()
                && let Some(default) = t.default
                && !events.is_empty()
            {
                data.push(json!({
                    "MetricName": t.name,
                    "Value": default,
                    "Timestamp": events[0].timestamp as f64 / 1000.0,
                    "Unit": t.unit.as_deref().unwrap_or("None"),
                }));
            }
            if data.is_empty() {
                continue;
            }
            let target = ctx.arn("cloudwatch", "metric-data");
            ctx.deliver(Delivery {
                target,
                source: group_arn(ctx, &group.name),
                body: json!({ "Namespace": t.namespace, "MetricData": data }).to_string(),
                ..Default::default()
            });
        }
    }
}

/// The `awslogs` payload subscription filters deliver: gzip-compressed JSON.
fn subscription_payload(
    ctx: &Ctx<'_>,
    group: &str,
    stream: &str,
    filter: &str,
    events: &[&Event],
) -> Vec<u8> {
    let doc = json!({
        "messageType": "DATA_MESSAGE",
        "owner": ctx.account,
        "logGroup": group,
        "logStream": stream,
        "subscriptionFilters": [filter],
        "logEvents": events.iter().map(|e| json!({
            "id": e.id,
            "timestamp": e.timestamp,
            "message": e.message,
        })).collect::<Vec<_>>(),
    });
    gzip(doc.to_string().as_bytes())
}

fn deliver_subscriptions(ctx: &mut Ctx<'_>, group: &Group, stream: &str, events: &[Event]) {
    for (name, f) in &group.subscriptions {
        let Ok(p) = filter::parse(&f.pattern) else {
            continue;
        };
        let matched: Vec<&Event> = events
            .iter()
            .filter(|e| p.matches(&e.message).is_some())
            .collect();
        if matched.is_empty() {
            continue;
        }
        let data = subscription_payload(ctx, &group.name, stream, name, &matched);
        let mut d = Delivery {
            target: f.destination.clone(),
            source: group_arn(ctx, &group.name),
            ..Default::default()
        };
        if f.destination.starts_with("arn:aws:lambda:") {
            d.body = json!({ "awslogs": { "data": STANDARD.encode(&data) } }).to_string();
        } else {
            d.attributes
                .insert("PartitionKey".to_string(), json!(stream));
            d.bytes = Some(data);
        }
        ctx.deliver(d);
    }
}

fn put_events(
    region: &mut Region,
    ctx: &mut Ctx<'_>,
    group_name_: &str,
    stream_name: &str,
    raw: &[Value],
) -> Result<Value, AwsError> {
    let now = ctx.now_ms() as i64;
    let mut fresh = Vec::new();
    let mut last = i64::MIN;
    for e in raw {
        let timestamp = e
            .i64_field("timestamp")
            .ok_or_else(|| invalid("Each log event requires a timestamp."))?;
        let message = e
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("Each log event requires a message."))?;
        if timestamp < last {
            return Err(invalid(
                "Log events in a single PutLogEvents request must be in chronological order.",
            ));
        }
        last = timestamp;
        fresh.push(Event {
            timestamp,
            message: message.to_string(),
            ingestion: now,
            id: event_id(ctx),
        });
    }
    let group = region.group_mut(group_name_)?;
    let stream = group
        .streams
        .get_mut(stream_name)
        .ok_or_else(|| not_found("The specified log stream does not exist."))?;
    stream.events.extend(fresh.iter().cloned());
    stream.events.sort_by_key(|e| e.timestamp);
    let token = format!("{:056}", stream.events.len());
    let group = region.group(group_name_)?.clone();
    emit_metrics(ctx, &group, &fresh);
    deliver_subscriptions(ctx, &group, stream_name, &fresh);
    Ok(json!({ "nextSequenceToken": token }))
}

fn get_events(region: &Region, input: &Value) -> OpResult {
    let group = region.group_ref(input)?;
    let name = required(input, "logStreamName")?;
    let stream = group
        .streams
        .get(name)
        .ok_or_else(|| not_found("The specified log stream does not exist."))?;
    let (start, end) = (input.i64_field("startTime"), input.i64_field("endTime"));
    let events: Vec<&Event> = stream
        .events
        .iter()
        .filter(|e| start.is_none_or(|s| e.timestamp >= s) && end.is_none_or(|t| e.timestamp < t))
        .collect();
    let limit = input
        .i64_field("limit")
        .map_or(10000, |l| l.max(1) as usize);
    let head = input.get("startFromHead").and_then(Value::as_bool) == Some(true);
    let token = input.str_field("nextToken");
    let parse_tok = |prefix: &str| {
        token
            .and_then(|t| t.strip_prefix(prefix))
            .and_then(|n| n.parse::<usize>().ok())
            .map(|n| n.min(events.len()))
    };
    let (from, to) = if let Some(i) = parse_tok("f/") {
        (i, (i + limit).min(events.len()))
    } else if let Some(i) = parse_tok("b/") {
        (i.saturating_sub(limit), i)
    } else if head {
        (0, limit.min(events.len()))
    } else {
        (events.len().saturating_sub(limit), events.len())
    };
    Ok(json!({
        "events": events[from..to].iter().map(|e| json!({
            "timestamp": e.timestamp,
            "message": e.message,
            "ingestionTime": e.ingestion,
        })).collect::<Vec<_>>(),
        "nextForwardToken": format!("f/{to:056}"),
        "nextBackwardToken": format!("b/{from:056}"),
    }))
}

fn filter_events(region: &Region, input: &Value) -> OpResult {
    let group = region.group_ref(input)?;
    let names = input
        .get("logStreamNames")
        .map(|_| strings(input.array_field("logStreamNames")));
    let prefix = input.str_field("logStreamNamePrefix");
    if names.is_some() && prefix.is_some() {
        return Err(invalid(
            "Cannot specify both logStreamNames and logStreamNamePrefix.",
        ));
    }
    let p = pattern(input.str_field("filterPattern").unwrap_or_default())?;
    let (start, end) = (input.i64_field("startTime"), input.i64_field("endTime"));
    let mut found: Vec<Value> = Vec::new();
    let mut searched = Vec::new();
    for (name, s) in &group.streams {
        if names.as_ref().is_some_and(|n| !n.contains(name))
            || prefix.is_some_and(|p| !name.starts_with(p))
        {
            continue;
        }
        searched.push(json!({ "logStreamName": name, "searchedCompletely": true }));
        for e in &s.events {
            if start.is_some_and(|s| e.timestamp < s)
                || end.is_some_and(|t| e.timestamp >= t)
                || p.matches(&e.message).is_none()
            {
                continue;
            }
            found.push(json!({
                "logStreamName": name,
                "timestamp": e.timestamp,
                "message": e.message,
                "ingestionTime": e.ingestion,
                "eventId": e.id,
            }));
        }
    }
    found.sort_by_key(|e| e["timestamp"].as_i64());
    let (events, next) = page(found, input, 10000);
    Ok(with_next(
        json!({ "events": events, "searchedLogStreams": searched }),
        next,
    ))
}

fn insights_rows(ctx: &Ctx<'_>, region: &Region, q: &Query) -> (Vec<Row>, usize) {
    let mut rows = Vec::new();
    let mut scanned = 0;
    for gname in &q.groups {
        let Some(g) = region.groups.get(gname) else {
            continue;
        };
        for (sname, s) in &g.streams {
            for e in &s.events {
                scanned += 1;
                if q.start.is_some_and(|t| e.timestamp < t * 1000)
                    || q.end.is_some_and(|t| e.timestamp > t * 1000)
                {
                    continue;
                }
                let ts = format_iso8601(e.timestamp as f64 / 1000.0);
                let mut row = Row::new();
                row.insert(
                    "@timestamp".to_string(),
                    format!("{} {}", &ts[..10], ts[11..].trim_end_matches('Z')),
                );
                row.insert("@message".to_string(), e.message.clone());
                row.insert("@logStream".to_string(), sname.clone());
                row.insert("@log".to_string(), format!("{}:{gname}", ctx.account));
                row.insert("@ingestionTime".to_string(), e.ingestion.to_string());
                if let Ok(doc @ Value::Object(_)) = serde_json::from_str::<Value>(&e.message) {
                    insights::discover("", &doc, &mut row);
                }
                rows.push(row);
            }
        }
    }
    rows.sort_by(|a, b| b["@timestamp"].cmp(&a["@timestamp"]));
    (rows, scanned)
}

fn stored(region: &mut BTreeMap<String, Value>, key: &str, what: &str) -> Result<Value, AwsError> {
    region
        .get(key)
        .cloned()
        .ok_or_else(|| not_found(format!("The specified {what} does not exist.")))
}

fn remove(map: &mut BTreeMap<String, Value>, key: &str, what: &str) -> OpResult {
    map.remove(key)
        .map(|_| json!({}))
        .ok_or_else(|| not_found(format!("The specified {what} does not exist.")))
}

fn export(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let gname = required(input, "logGroupName")?;
    let bucket = required(input, "destination")?.to_string();
    let from = input.i64_field("from").unwrap_or(0);
    let to = input.i64_field("to").unwrap_or(i64::MAX);
    let prefix = input
        .str_field("destinationPrefix")
        .unwrap_or("exportedlogs")
        .trim_end_matches('/')
        .to_string();
    let group = region.group(gname)?;
    let name_prefix = input.str_field("logStreamNamePrefix");
    let id = ctx.uuid();
    let mut objects = Vec::new();
    for (sname, s) in &group.streams {
        if name_prefix.is_some_and(|p| !sname.starts_with(p)) {
            continue;
        }
        let lines: String = s
            .events
            .iter()
            .filter(|e| e.timestamp >= from && e.timestamp < to)
            .map(|e| {
                format!(
                    "{} {}\n",
                    format_iso8601(e.timestamp as f64 / 1000.0),
                    e.message
                )
            })
            .collect();
        if !lines.is_empty() {
            objects.push((
                format!("{prefix}/{id}/{sname}/000000.gz"),
                gzip(lines.as_bytes()),
            ));
        }
    }
    let source = group_arn(ctx, group.name.as_str());
    for (key, data) in objects {
        let mut attributes = Map::new();
        attributes.insert("Key".to_string(), json!(key));
        ctx.deliver(Delivery {
            target: format!("arn:aws:s3:::{bucket}"),
            source: source.clone(),
            bytes: Some(data),
            attributes,
            ..Default::default()
        });
    }
    let now = ctx.now_ms() as i64;
    let mut task = json!({
        "taskId": id,
        "logGroupName": gname,
        "from": from,
        "to": input.i64_field("to"),
        "destination": bucket,
        "destinationPrefix": prefix,
        "status": { "code": "COMPLETED", "message": "Completed successfully" },
        "executionInfo": { "creationTime": now, "completionTime": now },
    });
    if let Some(n) = input.str_field("taskName") {
        task["taskName"] = json!(n);
    }
    region.export_tasks.push(task);
    Ok(json!({ "taskId": id }))
}

type Handled = Result<Option<Value>, AwsError>;

fn call(region: &mut Region, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> OpResult {
    for part in [groups, filters, queries, admin] {
        if let Some(out) = part(region, ctx, operation, input)? {
            return Ok(out);
        }
    }
    Err(AwsError::new(
        400,
        "UnknownOperationException",
        format!("wasmo: {operation} is not implemented"),
    ))
}

fn groups(region: &mut Region, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> Handled {
    let now = ctx.now_ms() as i64;
    Ok(Some(match operation {
        "CreateLogGroup" => {
            let name = required(input, "logGroupName")?.to_string();
            if region.groups.contains_key(&name) {
                return Err(exists("The specified log group already exists"));
            }
            region.groups.insert(
                name.clone(),
                Group {
                    name,
                    created: now,
                    tags: tag_map(input.obj_field("tags")),
                    kms_key: input.str_field("kmsKeyId").map(str::to_string),
                    class: input
                        .str_field("logGroupClass")
                        .unwrap_or("STANDARD")
                        .to_string(),
                    deletion_protection: input
                        .get("deletionProtectionEnabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    ..Default::default()
                },
            );
            json!({})
        }
        "DeleteLogGroup" => {
            let name = required(input, "logGroupName")?;
            if region.group(name)?.deletion_protection {
                return Err(AwsError::new(
                    400,
                    "ValidationException",
                    "The log group has deletion protection enabled.",
                ));
            }
            region.groups.remove(name);
            json!({})
        }
        "DescribeLogGroups" | "ListLogGroups" => {
            let prefix = input.str_field("logGroupNamePrefix");
            let pat = input.str_field("logGroupNamePattern");
            let ids = strings(input.array_field("logGroupIdentifiers"));
            let class = input.str_field("logGroupClass");
            let groups: Vec<Value> = region
                .groups
                .values()
                .filter(|g| prefix.is_none_or(|p| g.name.starts_with(p)))
                .filter(|g| pat.is_none_or(|p| g.name.to_lowercase().contains(&p.to_lowercase())))
                .filter(|g| ids.is_empty() || ids.iter().any(|i| group_name(i) == g.name))
                .filter(|g| class.is_none_or(|c| g.class == c))
                .map(|g| group_view(ctx, g))
                .collect();
            let (groups, next) = page(groups, input, 50);
            with_next(json!({ "logGroups": groups }), next)
        }
        "CreateLogStream" => {
            let stream = required(input, "logStreamName")?.to_string();
            let group = region.group_mut(required(input, "logGroupName")?)?;
            if group.streams.contains_key(&stream) {
                return Err(exists("The specified log stream already exists"));
            }
            group.streams.insert(
                stream,
                LogStream {
                    created: now,
                    events: Vec::new(),
                },
            );
            json!({})
        }
        "DeleteLogStream" => {
            let stream = required(input, "logStreamName")?;
            let group = region.group_mut(required(input, "logGroupName")?)?;
            group
                .streams
                .remove(stream)
                .ok_or_else(|| not_found("The specified log stream does not exist."))?;
            json!({})
        }
        "DescribeLogStreams" => {
            let group = region.group_ref(input)?;
            let prefix = input.str_field("logStreamNamePrefix");
            let by_time = input.str_field("orderBy") == Some("LastEventTime");
            if by_time && prefix.is_some() {
                return Err(invalid(
                    "Cannot order by LastEventTime with a logStreamNamePrefix.",
                ));
            }
            let mut streams: Vec<(&String, &LogStream)> = group
                .streams
                .iter()
                .filter(|(n, _)| prefix.is_none_or(|p| n.starts_with(p)))
                .collect();
            if by_time {
                streams.sort_by_key(|(_, s)| s.events.iter().map(|e| e.timestamp).max());
            }
            if input.get("descending").and_then(Value::as_bool) == Some(true) {
                streams.reverse();
            }
            let views: Vec<Value> = streams
                .iter()
                .map(|(n, s)| stream_view(ctx, &group.name, n, s))
                .collect();
            let (streams, next) = page(views, input, 50);
            with_next(json!({ "logStreams": streams }), next)
        }
        "PutLogEvents" => {
            let gname = required(input, "logGroupName")?.to_string();
            let sname = required(input, "logStreamName")?.to_string();
            let events = input.array_field("logEvents");
            if events.is_empty() {
                return Err(invalid("logEvents must contain at least one event."));
            }
            put_events(region, ctx, &gname, &sname, events)?
        }
        "GetLogEvents" => get_events(region, input)?,
        "FilterLogEvents" => filter_events(region, input)?,
        "PutRetentionPolicy" => {
            let days = input
                .i64_field("retentionInDays")
                .ok_or_else(|| invalid("retentionInDays is required."))?;
            const DAYS: &[i64] = &[
                1, 3, 5, 7, 14, 30, 60, 90, 120, 150, 180, 365, 400, 545, 731, 1096, 1827, 2192,
                2557, 2922, 3288, 3653,
            ];
            if !DAYS.contains(&days) {
                return Err(invalid(format!(
                    "1 validation error detected: Value '{days}' at 'retentionInDays' failed to satisfy constraint: Member must satisfy enum value set: {DAYS:?}"
                )));
            }
            region
                .group_mut(required(input, "logGroupName")?)?
                .retention = Some(days);
            json!({})
        }
        "DeleteRetentionPolicy" => {
            region
                .group_mut(required(input, "logGroupName")?)?
                .retention = None;
            json!({})
        }
        _ => return Ok(None),
    }))
}

fn filters(region: &mut Region, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> Handled {
    let now = ctx.now_ms() as i64;
    Ok(Some(match operation {
        "PutMetricFilter" => {
            let name = required(input, "filterName")?.to_string();
            let pat = input
                .str_field("filterPattern")
                .unwrap_or_default()
                .to_string();
            pattern(&pat)?;
            let ts = transformations(input)?;
            let group = region.group_mut(required(input, "logGroupName")?)?;
            let created = group.metric_filters.get(&name).map_or(now, |f| f.created);
            group.metric_filters.insert(
                name,
                MetricFilter {
                    pattern: pat,
                    created,
                    transformations: ts,
                },
            );
            json!({})
        }
        "DescribeMetricFilters" => {
            let gname = input.str_field("logGroupName");
            if let Some(g) = gname {
                region.group(g)?;
            }
            let prefix = input.str_field("filterNamePrefix");
            let metric = input.str_field("metricName");
            let ns = input.str_field("metricNamespace");
            let filters: Vec<Value> = region
                .groups
                .values()
                .filter(|g| gname.is_none_or(|n| g.name == n))
                .flat_map(|g| g.metric_filters.iter().map(move |(n, f)| (g, n, f)))
                .filter(|(_, n, _)| prefix.is_none_or(|p| n.starts_with(p)))
                .filter(|(_, _, f)| {
                    f.transformations.iter().any(|t| {
                        metric.is_none_or(|m| t.name == m) && ns.is_none_or(|x| t.namespace == x)
                    })
                })
                .map(|(g, n, f)| metric_filter_view(&g.name, n, f))
                .collect();
            let (filters, next) = page(filters, input, 50);
            with_next(json!({ "metricFilters": filters }), next)
        }
        "DeleteMetricFilter" => {
            let name = required(input, "filterName")?;
            region
                .group_mut(required(input, "logGroupName")?)?
                .metric_filters
                .remove(name)
                .ok_or_else(|| not_found("The specified metric filter does not exist."))?;
            json!({})
        }
        "TestMetricFilter" => {
            let p = pattern(required(input, "filterPattern")?)?;
            let matches: Vec<Value> = input
                .array_field("logEventMessages")
                .iter()
                .filter_map(Value::as_str)
                .enumerate()
                .filter_map(|(i, m)| {
                    let caps = p.matches(m)?;
                    let extracted: Map<String, Value> = caps
                        .fields
                        .iter()
                        .map(|(k, v)| (format!("${k}"), json!(v)))
                        .collect();
                    Some(json!({ "eventNumber": i + 1, "eventMessage": m, "extractedValues": extracted }))
                })
                .collect();
            json!({ "matches": matches })
        }
        "PutSubscriptionFilter" => {
            let name = required(input, "filterName")?.to_string();
            let destination = required(input, "destinationArn")?.to_string();
            let pat = input
                .str_field("filterPattern")
                .unwrap_or_default()
                .to_string();
            pattern(&pat)?;
            let group = region.group_mut(required(input, "logGroupName")?)?;
            if !group.subscriptions.contains_key(&name) && group.subscriptions.len() >= 2 {
                return Err(AwsError::new(
                    400,
                    "LimitExceededException",
                    "Resource limit exceeded.",
                ));
            }
            let created = group.subscriptions.get(&name).map_or(now, |f| f.created);
            group.subscriptions.insert(
                name,
                SubscriptionFilter {
                    pattern: pat,
                    destination,
                    role: input.str_field("roleArn").map(str::to_string),
                    distribution: input.str_field("distribution").map(str::to_string),
                    created,
                },
            );
            json!({})
        }
        "DescribeSubscriptionFilters" => {
            let group = region.group(required(input, "logGroupName")?)?;
            let prefix = input.str_field("filterNamePrefix");
            let filters: Vec<Value> = group
                .subscriptions
                .iter()
                .filter(|(n, _)| prefix.is_none_or(|p| n.starts_with(p)))
                .map(|(n, f)| {
                    let mut v = json!({
                        "filterName": n,
                        "filterPattern": f.pattern,
                        "logGroupName": group.name,
                        "destinationArn": f.destination,
                        "distribution": f.distribution.as_deref().unwrap_or("ByLogStream"),
                        "creationTime": f.created,
                    });
                    if let Some(r) = &f.role {
                        v["roleArn"] = json!(r);
                    }
                    v
                })
                .collect();
            let (filters, next) = page(filters, input, 50);
            with_next(json!({ "subscriptionFilters": filters }), next)
        }
        "DeleteSubscriptionFilter" => {
            let name = required(input, "filterName")?;
            region
                .group_mut(required(input, "logGroupName")?)?
                .subscriptions
                .remove(name)
                .ok_or_else(|| not_found("The specified subscription filter does not exist."))?;
            json!({})
        }
        "TagLogGroup" => {
            let tags = tag_map(input.obj_field("tags"));
            region
                .group_mut(required(input, "logGroupName")?)?
                .tags
                .extend(tags);
            json!({})
        }
        "UntagLogGroup" => {
            let keys = strings(input.array_field("tags"));
            let g = region.group_mut(required(input, "logGroupName")?)?;
            keys.iter().for_each(|k| {
                g.tags.remove(k);
            });
            json!({})
        }
        "ListTagsLogGroup" => {
            json!({ "tags": region.group(required(input, "logGroupName")?)?.tags })
        }
        "TagResource" | "UntagResource" | "ListTagsForResource" => {
            let arn = required(input, "resourceArn")?.to_string();
            let tags = if arn.contains(":log-group:") {
                &mut region.group_mut(&arn)?.tags
            } else {
                let known = region
                    .destinations
                    .values()
                    .any(|d| d["arn"] == arn.as_str())
                    || region.deliveries.values().any(|d| d["arn"] == arn.as_str())
                    || region
                        .delivery_sources
                        .values()
                        .any(|d| d["arn"] == arn.as_str())
                    || region
                        .delivery_destinations
                        .values()
                        .any(|d| d["arn"] == arn.as_str())
                    || region.anomaly_detectors.contains_key(&arn);
                if !known {
                    return Err(not_found(format!(
                        "The specified resource {arn} does not exist."
                    )));
                }
                region.resource_tags.entry(arn).or_default()
            };
            match operation {
                "TagResource" => {
                    tags.extend(tag_map(input.obj_field("tags")));
                    json!({})
                }
                "UntagResource" => {
                    for k in strings(input.array_field("tagKeys")) {
                        tags.remove(&k);
                    }
                    json!({})
                }
                _ => json!({ "tags": tags }),
            }
        }
        "AssociateKmsKey" => {
            let key = required(input, "kmsKeyId")?.to_string();
            region.group_mut(required(input, "logGroupName")?)?.kms_key = Some(key);
            json!({})
        }
        "DisassociateKmsKey" => {
            region.group_mut(required(input, "logGroupName")?)?.kms_key = None;
            json!({})
        }
        "PutLogGroupDeletionProtection" => {
            let on = input
                .get("deletionProtectionEnabled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            region
                .group_mut(required(input, "logGroupIdentifier")?)?
                .deletion_protection = on;
            json!({})
        }
        _ => return Ok(None),
    }))
}

fn queries(region: &mut Region, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> Handled {
    let now = ctx.now_ms() as i64;
    Ok(Some(match operation {
        "PutDestination" => {
            let name = required(input, "destinationName")?.to_string();
            let mut d = json!({
                "destinationName": name,
                "targetArn": required(input, "targetArn")?,
                "roleArn": required(input, "roleArn")?,
                "arn": ctx.arn("logs", &format!("destination:{name}")),
                "creationTime": now,
            });
            if let Some(old) = region.destinations.get(&name) {
                d["creationTime"] = old["creationTime"].clone();
                if let Some(p) = old.get("accessPolicy") {
                    d["accessPolicy"] = p.clone();
                }
            }
            region.destinations.insert(name, d.clone());
            json!({ "destination": d })
        }
        "PutDestinationPolicy" => {
            let name = required(input, "destinationName")?;
            let policy = required(input, "accessPolicy")?;
            let d = region
                .destinations
                .get_mut(name)
                .ok_or_else(|| not_found("The specified destination does not exist."))?;
            d["accessPolicy"] = json!(policy);
            json!({})
        }
        "DescribeDestinations" => {
            let prefix = input.str_field("DestinationNamePrefix");
            let list: Vec<Value> = region
                .destinations
                .iter()
                .filter(|(n, _)| prefix.is_none_or(|p| n.starts_with(p)))
                .map(|(_, d)| d.clone())
                .collect();
            let (list, next) = page(list, input, 50);
            with_next(json!({ "destinations": list }), next)
        }
        "DeleteDestination" => remove(
            &mut region.destinations,
            required(input, "destinationName")?,
            "destination",
        )?,
        "StartQuery" => {
            let query = required(input, "queryString")?.to_string();
            let mut groups: Vec<String> = input
                .str_field("logGroupName")
                .map(|n| vec![n.to_string()])
                .unwrap_or_default();
            groups.extend(strings(input.array_field("logGroupNames")));
            groups.extend(
                strings(input.array_field("logGroupIdentifiers"))
                    .iter()
                    .map(|i| group_name(i).to_string()),
            );
            if groups.is_empty() {
                return Err(invalid(
                    "Query must specify either logGroupName, logGroupNames, or logGroupIdentifiers.",
                ));
            }
            for g in &groups {
                region.group(g)?;
            }
            insights::run(&query, Vec::new())
                .map_err(|e| AwsError::new(400, "MalformedQueryException", e))?;
            let id = ctx.uuid();
            region.queries.push(Query {
                id: id.clone(),
                query,
                groups,
                start: input.i64_field("startTime"),
                end: input.i64_field("endTime"),
                limit: input.i64_field("limit").map(|l| l.max(1) as usize),
                created: now,
                status: "Scheduled".to_string(),
                results: None,
            });
            json!({ "queryId": id })
        }
        "GetQueryResults" => {
            let id = required(input, "queryId")?;
            let i = region
                .queries
                .iter()
                .position(|q| q.id == id)
                .ok_or_else(|| not_found("The specified query does not exist."))?;
            if region.queries[i].results.is_none() && region.queries[i].status != "Cancelled" {
                let (rows, scanned) = insights_rows(ctx, region, &region.queries[i]);
                let q = &mut region.queries[i];
                let mut out = insights::run(&q.query, rows)
                    .map_err(|e| AwsError::new(400, "MalformedQueryException", e))?;
                out.truncate(q.limit.unwrap_or(10000));
                q.results = Some((out, scanned));
                q.status = "Complete".to_string();
            }
            let q = &region.queries[i];
            let (rows, scanned) = q.results.clone().unwrap_or_default();
            let bytes: usize = rows.iter().flat_map(|r| r.values()).map(String::len).sum();
            json!({
                "status": q.status,
                "results": rows.iter().map(|r| r.iter().map(|(k, v)| json!({ "field": k, "value": v })).collect::<Vec<_>>()).collect::<Vec<_>>(),
                "statistics": {
                    "recordsMatched": rows.len() as f64,
                    "recordsScanned": scanned as f64,
                    "bytesScanned": bytes as f64,
                },
            })
        }
        "StopQuery" => {
            let id = required(input, "queryId")?;
            let q = region
                .queries
                .iter_mut()
                .find(|q| q.id == id)
                .ok_or_else(|| not_found("The specified query does not exist."))?;
            if q.results.is_some() {
                return Err(AwsError::new(
                    400,
                    "InvalidParameterException",
                    "Query is not running.",
                ));
            }
            q.status = "Cancelled".to_string();
            json!({ "success": true })
        }
        "DescribeQueries" => {
            let gname = input.str_field("logGroupName");
            let status = input.str_field("status");
            let list: Vec<Value> = region
                .queries
                .iter()
                .filter(|q| gname.is_none_or(|g| q.groups.iter().any(|x| x == g)))
                .filter(|q| status.is_none_or(|s| q.status == s))
                .map(|q| {
                    json!({
                        "queryId": q.id,
                        "queryString": q.query,
                        "status": q.status,
                        "createTime": q.created,
                        "logGroupName": q.groups.first(),
                        "queryLanguage": "CWLI",
                    })
                })
                .collect();
            let (list, next) = page(list, input, 1000);
            with_next(json!({ "queries": list }), next)
        }
        "ListLogGroupsForQuery" => {
            let id = required(input, "queryId")?;
            let q = region
                .queries
                .iter()
                .find(|q| q.id == id)
                .ok_or_else(|| not_found("The specified query does not exist."))?;
            json!({ "logGroupIdentifiers": q.groups.iter().map(|g| group_arn(ctx, g)).collect::<Vec<_>>() })
        }
        "PutQueryDefinition" => {
            let id = input
                .str_field("queryDefinitionId")
                .map_or_else(|| ctx.uuid(), str::to_string);
            if input.str_field("queryDefinitionId").is_some()
                && !region.query_definitions.contains_key(&id)
            {
                return Err(not_found("The specified query definition does not exist."));
            }
            region.query_definitions.insert(
                id.clone(),
                json!({
                    "queryDefinitionId": id,
                    "name": required(input, "name")?,
                    "queryString": required(input, "queryString")?,
                    "logGroupNames": strings(input.array_field("logGroupNames")),
                    "lastModified": now,
                    "queryLanguage": input.str_field("queryLanguage").unwrap_or("CWLI"),
                }),
            );
            json!({ "queryDefinitionId": id })
        }
        "DescribeQueryDefinitions" => {
            let prefix = input.str_field("queryDefinitionNamePrefix");
            let list: Vec<Value> = region
                .query_definitions
                .values()
                .filter(|d| {
                    prefix.is_none_or(|p| d["name"].as_str().is_some_and(|n| n.starts_with(p)))
                })
                .cloned()
                .collect();
            let (list, next) = page(list, input, 1000);
            with_next(json!({ "queryDefinitions": list }), next)
        }
        "DeleteQueryDefinition" => {
            let id = required(input, "queryDefinitionId")?;
            json!({ "success": region.query_definitions.remove(id).is_some() })
        }
        "GetLogGroupFields" => {
            let group = region.group_ref(input)?;
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            let mut total = 0;
            for e in group.streams.values().flat_map(|s| &s.events) {
                total += 1;
                for f in [
                    "@timestamp",
                    "@message",
                    "@logStream",
                    "@log",
                    "@ingestionTime",
                ] {
                    *counts.entry(f.to_string()).or_default() += 1;
                }
                if let Ok(doc @ Value::Object(_)) = serde_json::from_str::<Value>(&e.message) {
                    let mut row = Row::new();
                    insights::discover("", &doc, &mut row);
                    for k in row.keys() {
                        *counts.entry(k.clone()).or_default() += 1;
                    }
                }
            }
            json!({
                "logGroupFields": counts.iter().map(|(k, n)| json!({
                    "name": k,
                    "percent": (n * 100).checked_div(total).unwrap_or(0),
                })).collect::<Vec<_>>(),
            })
        }
        "GetLogRecord" => {
            json!({ "logRecord": {} })
        }
        _ => return Ok(None),
    }))
}

fn admin(region: &mut Region, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> Handled {
    let now = ctx.now_ms() as i64;
    Ok(Some(match operation {
        "PutResourcePolicy" => {
            let name = input
                .str_field("policyName")
                .unwrap_or("default")
                .to_string();
            let doc = required(input, "policyDocument")?;
            if !region.resource_policies.contains_key(&name) && region.resource_policies.len() >= 10
            {
                return Err(AwsError::new(
                    400,
                    "LimitExceededException",
                    "Resource limit exceeded.",
                ));
            }
            let mut p = json!({
                "policyName": name,
                "policyDocument": doc,
                "lastUpdatedTime": now,
                "policyScope": if input.str_field("resourceArn").is_some() { "RESOURCE" } else { "ACCOUNT" },
                "revisionId": ctx.uuid(),
            });
            if let Some(a) = input.str_field("resourceArn") {
                p["resourceArn"] = json!(a);
            }
            region.resource_policies.insert(name, p.clone());
            json!({ "resourcePolicy": p, "revisionId": p["revisionId"] })
        }
        "DescribeResourcePolicies" => {
            let list: Vec<Value> = region.resource_policies.values().cloned().collect();
            let (list, next) = page(list, input, 50);
            with_next(json!({ "resourcePolicies": list }), next)
        }
        "DeleteResourcePolicy" => {
            let name = input.str_field("policyName").unwrap_or("default");
            remove(&mut region.resource_policies, name, "resource policy")?
        }
        "PutAccountPolicy" => {
            let name = required(input, "policyName")?.to_string();
            let kind = required(input, "policyType")?.to_string();
            let mut p = json!({
                "policyName": name,
                "policyDocument": required(input, "policyDocument")?,
                "policyType": kind,
                "scope": input.str_field("scope").unwrap_or("ALL"),
                "lastUpdatedTime": now,
                "accountId": ctx.account,
            });
            if let Some(s) = input.str_field("selectionCriteria") {
                p["selectionCriteria"] = json!(s);
            }
            region.account_policies.insert((kind, name), p.clone());
            json!({ "accountPolicy": p })
        }
        "DescribeAccountPolicies" => {
            let kind = required(input, "policyType")?;
            let name = input.str_field("policyName");
            let list: Vec<&Value> = region
                .account_policies
                .iter()
                .filter(|((k, n), _)| k == kind && name.is_none_or(|x| x == n))
                .map(|(_, p)| p)
                .collect();
            json!({ "accountPolicies": list })
        }
        "DeleteAccountPolicy" => {
            let key = (
                required(input, "policyType")?.to_string(),
                required(input, "policyName")?.to_string(),
            );
            region
                .account_policies
                .remove(&key)
                .ok_or_else(|| not_found("Policy with the specified name does not exist."))?;
            json!({})
        }
        "PutDataProtectionPolicy" => {
            let doc = required(input, "policyDocument")?.to_string();
            let g = region.group_mut(required(input, "logGroupIdentifier")?)?;
            g.data_protection = Some((doc.clone(), now));
            json!({ "logGroupIdentifier": g.name, "policyDocument": doc, "lastUpdatedTime": now })
        }
        "GetDataProtectionPolicy" => {
            let g = region.group(required(input, "logGroupIdentifier")?)?;
            match &g.data_protection {
                Some((doc, at)) => {
                    json!({ "logGroupIdentifier": g.name, "policyDocument": doc, "lastUpdatedTime": at })
                }
                None => json!({ "logGroupIdentifier": g.name }),
            }
        }
        "DeleteDataProtectionPolicy" => {
            let g = region.group_mut(required(input, "logGroupIdentifier")?)?;
            g.data_protection
                .take()
                .ok_or_else(|| not_found("The specified data protection policy does not exist."))?;
            json!({})
        }
        "PutIndexPolicy" => {
            let doc = required(input, "policyDocument")?.to_string();
            let g = region.group_mut(required(input, "logGroupIdentifier")?)?;
            g.index_policy = Some((doc.clone(), now));
            json!({ "indexPolicy": {
                "logGroupIdentifier": g.name,
                "policyDocument": doc,
                "lastUpdateTime": now,
                "policyName": "default",
                "source": "LOG_GROUP",
            }})
        }
        "DescribeIndexPolicies" => {
            let ids = strings(input.array_field("logGroupIdentifiers"));
            let mut list = Vec::new();
            for id in &ids {
                let g = region.group(id)?;
                if let Some((doc, at)) = &g.index_policy {
                    list.push(json!({
                        "logGroupIdentifier": g.name,
                        "policyDocument": doc,
                        "lastUpdateTime": at,
                        "policyName": "default",
                        "source": "LOG_GROUP",
                    }));
                }
            }
            json!({ "indexPolicies": list })
        }
        "DeleteIndexPolicy" => {
            let g = region.group_mut(required(input, "logGroupIdentifier")?)?;
            g.index_policy
                .take()
                .ok_or_else(|| not_found("The specified index policy does not exist."))?;
            json!({})
        }
        "PutTransformer" => {
            let config = input
                .get("transformerConfig")
                .cloned()
                .ok_or_else(|| invalid("transformerConfig is required."))?;
            let g = region.group_mut(required(input, "logGroupIdentifier")?)?;
            let created = g.transformer.as_ref().map_or(now, |t| t.1);
            g.transformer = Some((config, created, now));
            json!({})
        }
        "GetTransformer" => {
            let g = region.group(required(input, "logGroupIdentifier")?)?;
            let (config, created, updated) = g
                .transformer
                .clone()
                .ok_or_else(|| not_found("The specified transformer does not exist."))?;
            json!({
                "logGroupIdentifier": g.name,
                "creationTime": created,
                "lastModifiedTime": updated,
                "transformerConfig": config,
            })
        }
        "DeleteTransformer" => {
            let g = region.group_mut(required(input, "logGroupIdentifier")?)?;
            g.transformer
                .take()
                .ok_or_else(|| not_found("The specified transformer does not exist."))?;
            json!({})
        }
        "CreateExportTask" => export(region, ctx, input)?,
        "DescribeExportTasks" => {
            let id = input.str_field("taskId");
            let code = input.str_field("statusCode");
            let list: Vec<Value> = region
                .export_tasks
                .iter()
                .filter(|t| id.is_none_or(|i| t["taskId"] == i))
                .filter(|t| code.is_none_or(|c| t["status"]["code"] == c))
                .cloned()
                .collect();
            let (list, next) = page(list, input, 50);
            with_next(json!({ "exportTasks": list }), next)
        }
        "CancelExportTask" => {
            let id = required(input, "taskId")?;
            let t = region
                .export_tasks
                .iter()
                .find(|t| t["taskId"] == id)
                .ok_or_else(|| not_found("The specified export task does not exist."))?;
            return Err(AwsError::new(
                400,
                "InvalidOperationException",
                format!(
                    "The specified export task has already finished ({}).",
                    t["status"]["code"].as_str().unwrap_or_default()
                ),
            ));
        }
        "PutDeliverySource" => {
            let name = required(input, "name")?.to_string();
            let resource = required(input, "resourceArn")?;
            let v = json!({
                "name": name,
                "arn": ctx.arn("logs", &format!("delivery-source:{name}")),
                "resourceArns": [resource],
                "service": resource.split(':').nth(2).unwrap_or_default(),
                "logType": required(input, "logType")?,
            });
            region.delivery_sources.insert(name, v.clone());
            json!({ "deliverySource": v })
        }
        "GetDeliverySource" => {
            json!({ "deliverySource": stored(&mut region.delivery_sources, required(input, "name")?, "delivery source")? })
        }
        "DescribeDeliverySources" => {
            let (list, next) = page(
                region.delivery_sources.values().cloned().collect(),
                input,
                50,
            );
            with_next(json!({ "deliverySources": list }), next)
        }
        "DeleteDeliverySource" => remove(
            &mut region.delivery_sources,
            required(input, "name")?,
            "delivery source",
        )?,
        "PutDeliveryDestination" => {
            let name = required(input, "name")?.to_string();
            let config = input
                .get("deliveryDestinationConfiguration")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let kind = input.str_field("deliveryDestinationType").map_or_else(
                || {
                    let arn = config
                        .str_field("destinationResourceArn")
                        .unwrap_or_default();
                    if arn.contains(":s3:") {
                        "S3"
                    } else if arn.contains(":firehose:") {
                        "FH"
                    } else {
                        "CWL"
                    }
                    .to_string()
                },
                str::to_string,
            );
            let mut v = json!({
                "name": name,
                "arn": ctx.arn("logs", &format!("delivery-destination:{name}")),
                "deliveryDestinationType": kind,
                "deliveryDestinationConfiguration": config,
            });
            if let Some(f) = input.str_field("outputFormat") {
                v["outputFormat"] = json!(f);
            }
            region.delivery_destinations.insert(name, v.clone());
            json!({ "deliveryDestination": v })
        }
        "GetDeliveryDestination" => {
            json!({ "deliveryDestination": stored(&mut region.delivery_destinations, required(input, "name")?, "delivery destination")? })
        }
        "DescribeDeliveryDestinations" => {
            let (list, next) = page(
                region.delivery_destinations.values().cloned().collect(),
                input,
                50,
            );
            with_next(json!({ "deliveryDestinations": list }), next)
        }
        "DeleteDeliveryDestination" => remove(
            &mut region.delivery_destinations,
            required(input, "name")?,
            "delivery destination",
        )?,
        "PutDeliveryDestinationPolicy" => {
            let name = required(input, "deliveryDestinationName")?.to_string();
            let policy = required(input, "deliveryDestinationPolicy")?.to_string();
            stored(
                &mut region.delivery_destinations,
                &name,
                "delivery destination",
            )?;
            region
                .delivery_policies
                .insert(name.clone(), policy.clone());
            json!({ "policy": { "deliveryDestinationPolicy": policy } })
        }
        "GetDeliveryDestinationPolicy" => {
            let name = required(input, "deliveryDestinationName")?;
            let policy = region.delivery_policies.get(name).ok_or_else(|| {
                not_found("The specified delivery destination policy does not exist.")
            })?;
            json!({ "policy": { "deliveryDestinationPolicy": policy } })
        }
        "DeleteDeliveryDestinationPolicy" => {
            let name = required(input, "deliveryDestinationName")?;
            region.delivery_policies.remove(name).ok_or_else(|| {
                not_found("The specified delivery destination policy does not exist.")
            })?;
            json!({})
        }
        "CreateDelivery" => {
            let source = required(input, "deliverySourceName")?.to_string();
            let dest = required(input, "deliveryDestinationArn")?.to_string();
            stored(&mut region.delivery_sources, &source, "delivery source")?;
            let kind = region
                .delivery_destinations
                .values()
                .find(|d| d["arn"] == dest.as_str())
                .map(|d| d["deliveryDestinationType"].clone())
                .ok_or_else(|| not_found("The specified delivery destination does not exist."))?;
            let id = ctx.uuid().replace('-', "")[..16].to_string();
            let mut v = json!({
                "id": id,
                "arn": ctx.arn("logs", &format!("delivery:{id}")),
                "deliverySourceName": source,
                "deliveryDestinationArn": dest,
                "deliveryDestinationType": kind,
            });
            for k in ["recordFields", "fieldDelimiter", "s3DeliveryConfiguration"] {
                if let Some(x) = input.get(k) {
                    v[k] = x.clone();
                }
            }
            region.deliveries.insert(id, v.clone());
            json!({ "delivery": v })
        }
        "GetDelivery" => {
            json!({ "delivery": stored(&mut region.deliveries, required(input, "id")?, "delivery")? })
        }
        "DescribeDeliveries" => {
            let (list, next) = page(region.deliveries.values().cloned().collect(), input, 50);
            with_next(json!({ "deliveries": list }), next)
        }
        "UpdateDeliveryConfiguration" => {
            let id = required(input, "id")?;
            let d = region
                .deliveries
                .get_mut(id)
                .ok_or_else(|| not_found("The specified delivery does not exist."))?;
            for k in ["recordFields", "fieldDelimiter", "s3DeliveryConfiguration"] {
                if let Some(x) = input.get(k) {
                    d[k] = x.clone();
                }
            }
            json!({})
        }
        "DeleteDelivery" => remove(&mut region.deliveries, required(input, "id")?, "delivery")?,
        "CreateLogAnomalyDetector" => {
            let groups = strings(input.array_field("logGroupArnList"));
            for g in &groups {
                region.group(g)?;
            }
            let id = ctx.uuid();
            let arn = ctx.arn("logs", &format!("anomaly-detector:{id}"));
            let mut v = json!({
                "anomalyDetectorArn": arn,
                "logGroupArnList": groups,
                "evaluationFrequency": input.str_field("evaluationFrequency").unwrap_or("FIVE_MIN"),
                "anomalyDetectorStatus": "INITIALIZING",
                "creationTimeStamp": now,
                "lastModifiedTimeStamp": now,
                "anomalyVisibilityTime": input.i64_field("anomalyVisibilityTime").unwrap_or(21),
            });
            for k in ["detectorName", "filterPattern", "kmsKeyId"] {
                if let Some(x) = input.get(k) {
                    v[k] = x.clone();
                }
            }
            region.anomaly_detectors.insert(arn.clone(), v);
            json!({ "anomalyDetectorArn": arn })
        }
        "GetLogAnomalyDetector" => stored(
            &mut region.anomaly_detectors,
            required(input, "anomalyDetectorArn")?,
            "anomaly detector",
        )?,
        "ListLogAnomalyDetectors" => {
            let group = input.str_field("filterLogGroupArn");
            let list: Vec<Value> = region
                .anomaly_detectors
                .values()
                .filter(|d| {
                    group.is_none_or(|g| {
                        d["logGroupArnList"]
                            .as_array()
                            .is_some_and(|l| l.iter().any(|x| x == g))
                    })
                })
                .cloned()
                .collect();
            let (list, next) = page(list, input, 50);
            with_next(json!({ "anomalyDetectors": list }), next)
        }
        "UpdateLogAnomalyDetector" => {
            let arn = required(input, "anomalyDetectorArn")?;
            let d = region
                .anomaly_detectors
                .get_mut(arn)
                .ok_or_else(|| not_found("The specified anomaly detector does not exist."))?;
            if let Some(enabled) = input.get("enabled").and_then(Value::as_bool) {
                d["anomalyDetectorStatus"] = json!(if enabled { "ANALYZING" } else { "PAUSED" });
            }
            for k in [
                "evaluationFrequency",
                "filterPattern",
                "anomalyVisibilityTime",
            ] {
                if let Some(x) = input.get(k) {
                    d[k] = x.clone();
                }
            }
            d["lastModifiedTimeStamp"] = json!(now);
            json!({})
        }
        "DeleteLogAnomalyDetector" => remove(
            &mut region.anomaly_detectors,
            required(input, "anomalyDetectorArn")?,
            "anomaly detector",
        )?,
        "ListAnomalies" => json!({ "anomalies": [] }),
        _ => return Ok(None),
    }))
}

impl Service for Logs {
    fn signing_name(&self) -> &'static str {
        "logs"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("logs")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let region = self
            .regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default();
        call(region, ctx, operation, &input)
    }

    fn deliver(&mut self, ctx: &mut Ctx<'_>, resource: &str, delivery: &Delivery) -> bool {
        let Some(region) = self.regions.get(&(ctx.account.clone(), ctx.region.clone())) else {
            return false;
        };
        let Some(target) = resource
            .strip_prefix("destination:")
            .and_then(|name| region.destinations.get(name))
            .and_then(|d| d.str_field("targetArn"))
        else {
            return false;
        };
        ctx.deliver(Delivery {
            target: target.to_string(),
            ..delivery.clone()
        });
        true
    }
}
