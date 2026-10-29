use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::pattern;
use crate::codec::AwsError;
use crate::codec::time::format_iso8601;
use crate::embedded_model;
use crate::service::{Ctx, Delivery, InputExt, OpResult, Service, ServiceModel};

mod resources;
mod transform;

const MAX_TARGETS: usize = 5;
const RULE_STATES: &[&str] = &[
    "ENABLED",
    "DISABLED",
    "ENABLED_WITH_ALL_CLOUDTRAIL_MANAGEMENT_EVENTS",
];

type Record = Map<String, Value>;

#[derive(Debug, Clone)]
struct Rule {
    record: Record,
    targets: IndexMap<String, Value>,
}

impl Rule {
    fn name(&self) -> &str {
        self.record.str_field("Name").unwrap_or_default()
    }

    fn arn(&self) -> &str {
        self.record.str_field("Arn").unwrap_or_default()
    }

    fn enabled(&self) -> bool {
        self.record.str_field("State") != Some("DISABLED")
    }
}

#[derive(Debug, Clone)]
struct Archive {
    record: Record,
    events: Vec<(f64, Value)>,
}

#[derive(Debug, Default)]
struct Region {
    buses: BTreeMap<String, Record>,
    rules: BTreeMap<(String, String), Rule>,
    archives: BTreeMap<String, Archive>,
    replays: BTreeMap<String, Record>,
    connections: BTreeMap<String, Record>,
    api_destinations: BTreeMap<String, Record>,
    endpoints: BTreeMap<String, Record>,
    partner_sources: BTreeMap<String, Record>,
    event_sources: BTreeMap<String, Record>,
    tags: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Default)]
pub struct Events {
    regions: HashMap<(String, String), Region>,
}

trait RecordExt {
    fn str_field(&self, key: &str) -> Option<&str>;
}

impl RecordExt for Record {
    fn str_field(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }
}

fn err(code: &str, message: impl Into<String>) -> AwsError {
    AwsError::new(400, code, message)
}

fn validation(message: impl Into<String>) -> AwsError {
    err("ValidationException", message)
}

fn not_found(message: impl Into<String>) -> AwsError {
    err("ResourceNotFoundException", message)
}

fn now_secs(ctx: &Ctx<'_>) -> f64 {
    ctx.now_ms() / 1000.0
}

/// Accepts an event bus name or ARN.
fn bus_name(reference: Option<&str>) -> &str {
    match reference {
        Some(r) if r.starts_with("arn:") => r.split_once(":event-bus/").map_or(r, |(_, n)| n),
        Some(r) => r,
        None => "default",
    }
}

fn valid_name(name: &str, max: usize) -> bool {
    (1..=max).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn paginate(items: Vec<Value>, input: &Value, key: &str) -> Value {
    let start = input
        .str_field("NextToken")
        .and_then(|t| STANDARD.decode(t).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let end = input
        .i64_field("Limit")
        .filter(|l| *l > 0)
        .map_or(items.len(), |l| (start + l as usize).min(items.len()));
    let mut out = json!({ key: items[start..end] });
    if end < items.len() {
        out["NextToken"] = json!(STANDARD.encode(end.to_string()));
    }
    out
}

fn valid_schedule(expr: &str) -> bool {
    if let Some(body) = expr.strip_prefix("rate(").and_then(|s| s.strip_suffix(')')) {
        let mut parts = body.split_whitespace();
        let (Some(n), Some(unit), None) = (parts.next(), parts.next(), parts.next()) else {
            return false;
        };
        let Ok(n) = n.parse::<u64>() else {
            return false;
        };
        let units: &[&str] = if n == 1 {
            &["minute", "hour", "day"]
        } else {
            &["minutes", "hours", "days"]
        };
        return n > 0 && units.contains(&unit);
    }
    expr.strip_prefix("cron(")
        .and_then(|s| s.strip_suffix(')'))
        .is_some_and(|body| body.split_whitespace().count() == 6)
}

impl Region {
    fn new(ctx: &Ctx<'_>) -> Self {
        let mut r = Self::default();
        r.buses.insert(
            "default".into(),
            resources::bus_record(ctx, "default", &json!({})),
        );
        r
    }

    fn bus(&self, name: &str) -> Result<&Record, AwsError> {
        self.buses
            .get(name)
            .ok_or_else(|| not_found(format!("Event bus {name} does not exist.")))
    }

    fn rule_mut(&mut self, input: &Value) -> Result<&mut Rule, AwsError> {
        let bus = bus_name(input.str_field("EventBusName")).to_string();
        let name = input
            .str_field("Rule")
            .or_else(|| input.str_field("Name"))
            .ok_or_else(|| validation("Rule is a required field."))?
            .to_string();
        self.rules
            .get_mut(&(bus.clone(), name.clone()))
            .ok_or_else(|| not_found(format!("Rule {name} does not exist on EventBus {bus}.")))
    }
}

fn rule_arn(ctx: &Ctx<'_>, bus: &str, name: &str) -> String {
    if bus == "default" {
        ctx.arn("events", &format!("rule/{name}"))
    } else {
        ctx.arn("events", &format!("rule/{bus}/{name}"))
    }
}

fn put_rule(ctx: &mut Ctx<'_>, region: &mut Region, input: &Value) -> OpResult {
    let name = input.require_str("Name")?;
    if !valid_name(name, 64) {
        return Err(validation(format!(
            "1 validation error detected: Value '{name}' at 'name' failed to satisfy constraint: Member must satisfy regular expression pattern: [\\.\\-_A-Za-z0-9]+"
        )));
    }
    let bus = bus_name(input.str_field("EventBusName")).to_string();
    region.bus(&bus)?;
    let pattern = input.str_field("EventPattern");
    let schedule = input.str_field("ScheduleExpression");
    if pattern.is_none() && schedule.is_none() {
        return Err(validation(
            "Parameter(s) EventPattern or ScheduleExpression must be specified.",
        ));
    }
    if let Some(p) = pattern {
        pattern::parse(p).map_err(|e| {
            err(
                "InvalidEventPatternException",
                format!("Event pattern is not valid. Reason: {e}"),
            )
        })?;
    }
    if let Some(s) = schedule {
        if bus != "default" {
            return Err(validation(
                "ScheduleExpression is supported only on the default event bus.",
            ));
        }
        if !valid_schedule(s) {
            return Err(validation("Parameter ScheduleExpression is not valid."));
        }
    }
    let state = input.str_field("State").unwrap_or("ENABLED");
    if !RULE_STATES.contains(&state) {
        return Err(validation(format!("Parameter State is not valid: {state}")));
    }
    let arn = rule_arn(ctx, &bus, name);
    let mut record = Record::new();
    record.insert("Name".into(), json!(name));
    record.insert("Arn".into(), json!(arn));
    record.insert("State".into(), json!(state));
    record.insert("EventBusName".into(), json!(bus));
    record.insert("CreatedBy".into(), json!(ctx.account));
    for k in [
        "EventPattern",
        "ScheduleExpression",
        "Description",
        "RoleArn",
    ] {
        if let Some(v) = input.str_field(k) {
            record.insert(k.into(), json!(v));
        }
    }
    let key = (bus, name.to_string());
    let targets = region
        .rules
        .remove(&key)
        .map(|r| r.targets)
        .unwrap_or_default();
    region.rules.insert(key, Rule { record, targets });
    resources::add_tags(region, &arn, input);
    Ok(json!({ "RuleArn": arn }))
}

fn put_targets(region: &mut Region, input: &Value) -> OpResult {
    let rule = region.rule_mut(input)?;
    let targets = input.array_field("Targets");
    if targets.is_empty() {
        return Err(validation("Targets must contain at least one target."));
    }
    let new_ids = targets
        .iter()
        .filter_map(|t| t.str_field("Id"))
        .filter(|id| !rule.targets.contains_key(*id))
        .count();
    if rule.targets.len() + new_ids > MAX_TARGETS {
        return Err(err(
            "LimitExceededException",
            "The requested resource exceeds the maximum number allowed.",
        ));
    }
    let mut failed = Vec::new();
    for t in targets {
        let id = t.str_field("Id").unwrap_or_default();
        let inputs = ["Input", "InputPath", "InputTransformer"]
            .iter()
            .filter(|k| t.get(*k).is_some())
            .count();
        let problem = if !valid_name(id, 64) {
            Some("Parameter Id is not valid.")
        } else if t.str_field("Arn").is_none_or(|a| !a.starts_with("arn:")) {
            Some("Parameter Arn is not valid.")
        } else if inputs > 1 {
            Some("Only one of Input, InputPath, or InputTransformer must be provided.")
        } else if t
            .str_field("Arn")
            .is_some_and(|a| a.starts_with("arn:aws:sqs:") && a.ends_with(".fifo"))
            && t.get("SqsParameters")
                .and_then(|p| p.str_field("MessageGroupId"))
                .is_none()
        {
            Some("Parameter(s) SqsParameters must be specified for target: FIFO queues.")
        } else {
            None
        };
        if let Some(message) = problem {
            failed.push(json!({"TargetId": id, "ErrorCode": "ValidationException", "ErrorMessage": message}));
            continue;
        }
        rule.targets.insert(id.to_string(), t.clone());
    }
    Ok(json!({ "FailedEntryCount": failed.len(), "FailedEntries": failed }))
}

fn rule_op(ctx: &mut Ctx<'_>, region: &mut Region, operation: &str, input: &Value) -> OpResult {
    Ok(match operation {
        "PutRule" => return put_rule(ctx, region, input),
        "PutTargets" => return put_targets(region, input),
        "DescribeRule" => Value::Object(region.rule_mut(input)?.record.clone()),
        "EnableRule" | "DisableRule" => {
            let rule = region.rule_mut(input)?;
            let state = if operation == "EnableRule" {
                "ENABLED"
            } else {
                "DISABLED"
            };
            rule.record.insert("State".into(), json!(state));
            json!({})
        }
        "DeleteRule" => {
            let bus = bus_name(input.str_field("EventBusName")).to_string();
            let key = (bus, input.require_str("Name")?.to_string());
            if let Some(rule) = region.rules.get(&key) {
                if !rule.targets.is_empty() {
                    return Err(validation("Rule can't be deleted since it has targets."));
                }
                let arn = rule.arn().to_string();
                region.rules.remove(&key);
                region.tags.remove(&arn);
            }
            json!({})
        }
        "ListRules" => {
            let bus = bus_name(input.str_field("EventBusName"));
            region.bus(bus)?;
            let prefix = input.str_field("NamePrefix").unwrap_or_default();
            let rules: Vec<Value> = region
                .rules
                .iter()
                .filter(|((b, n), _)| b == bus && n.starts_with(prefix))
                .map(|(_, r)| Value::Object(r.record.clone()))
                .collect();
            paginate(rules, input, "Rules")
        }
        "RemoveTargets" => {
            let rule = region.rule_mut(input)?;
            for id in input.array_field("Ids").iter().filter_map(Value::as_str) {
                rule.targets.shift_remove(id);
            }
            json!({ "FailedEntryCount": 0, "FailedEntries": [] })
        }
        "ListTargetsByRule" => {
            let rule = region.rule_mut(input)?;
            paginate(rule.targets.values().cloned().collect(), input, "Targets")
        }
        "ListRuleNamesByTarget" => {
            let target = input.require_str("TargetArn")?;
            let bus = bus_name(input.str_field("EventBusName"));
            let names: Vec<Value> = region
                .rules
                .iter()
                .filter(|((b, _), r)| {
                    b == bus
                        && r.targets
                            .values()
                            .any(|t| t.str_field("Arn") == Some(target))
                })
                .map(|((_, n), _)| json!(n))
                .collect();
            paginate(names, input, "RuleNames")
        }
        "TestEventPattern" => {
            let p = pattern::parse(input.require_str("EventPattern")?).map_err(|e| {
                err(
                    "InvalidEventPatternException",
                    format!("Event pattern is not valid. Reason: {e}"),
                )
            })?;
            let event: Value = serde_json::from_str(input.require_str("Event")?)
                .ok()
                .filter(Value::is_object)
                .ok_or_else(|| validation("Parameter Event is not valid."))?;
            for field in ["id", "account", "source", "time", "region", "detail-type"] {
                if event.get(field).is_none() {
                    return Err(validation(format!(
                        "Parameter Event is not valid. Reason: Provided Event must have mandatory field {field}"
                    )));
                }
            }
            json!({ "Result": pattern::matches(&p, &event) })
        }
        _ => unreachable!("rule_op called with {operation}"),
    })
}

fn build_event(ctx: &mut Ctx<'_>, entry: &Value) -> Result<Value, (&'static str, String)> {
    let source = entry.str_field("Source").ok_or((
        "InvalidArgument",
        "Parameter Source is not valid. Reason: Source is a required argument.".to_string(),
    ))?;
    let detail_type = entry.str_field("DetailType").ok_or((
        "InvalidArgument",
        "Parameter DetailType is not valid. Reason: DetailType is a required argument.".to_string(),
    ))?;
    let detail: Value = match entry.get("Detail") {
        Some(Value::String(s)) => serde_json::from_str(s)
            .ok()
            .filter(Value::is_object)
            .ok_or(("MalformedDetail", "Detail is malformed.".to_string()))?,
        Some(v @ Value::Object(_)) => v.clone(),
        _ => {
            return Err((
                "InvalidArgument",
                "Parameter Detail is not valid. Reason: Detail is a required argument.".to_string(),
            ));
        }
    };
    let time = entry
        .get("Time")
        .and_then(Value::as_f64)
        .unwrap_or_else(|| now_secs(ctx))
        .floor();
    Ok(json!({
        "version": "0",
        "id": ctx.uuid(),
        "detail-type": detail_type,
        "source": source,
        "account": ctx.account,
        "time": format_iso8601(time),
        "region": ctx.region,
        "resources": entry.get("Resources").cloned().unwrap_or_else(|| json!([])),
        "detail": detail,
    }))
}

/// Archives `event` on `bus` and queues one delivery per matching target.
fn dispatch(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    bus: &str,
    event: &Value,
    replay: Option<&Value>,
) {
    let bus_arn = ctx.arn("events", &format!("event-bus/{bus}"));
    let now = now_secs(ctx);
    if replay.is_none() {
        for archive in region.archives.values_mut() {
            let source = archive.record.str_field("EventSourceArn");
            let wanted = archive
                .record
                .str_field("EventPattern")
                .and_then(|p| serde_json::from_str::<Value>(p).ok())
                .is_none_or(|p| pattern::matches(&p, event));
            if source == Some(bus_arn.as_str()) && wanted {
                let size = event.to_string().len() as u64;
                for (k, by) in [("EventCount", 1), ("SizeBytes", size)] {
                    let n = archive.record.get(k).and_then(Value::as_u64).unwrap_or(0);
                    archive.record.insert(k.into(), json!(n + by));
                }
                archive.events.push((now, event.clone()));
            }
        }
    }
    let filter: Option<Vec<&str>> = replay
        .and_then(|r| r.get("FilterArns"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect());
    for ((b, _), rule) in &region.rules {
        if b != bus || !rule.enabled() {
            continue;
        }
        if filter
            .as_ref()
            .is_some_and(|f| !f.is_empty() && !f.contains(&rule.arn()))
        {
            continue;
        }
        let Some(p) = rule
            .record
            .str_field("EventPattern")
            .and_then(|p| serde_json::from_str::<Value>(p).ok())
        else {
            continue;
        };
        if !pattern::matches(&p, event) {
            continue;
        }
        for target in rule.targets.values() {
            let Some(body) = transform::target_input(event, target, rule.arn(), rule.name()) else {
                continue;
            };
            ctx.deliver(Delivery {
                target: target.str_field("Arn").unwrap_or_default().to_string(),
                source: rule.arn().to_string(),
                body,
                group_id: target
                    .get("SqsParameters")
                    .and_then(|p| p.str_field("MessageGroupId"))
                    .map(str::to_string),
                dead_letter: target
                    .get("DeadLetterConfig")
                    .and_then(|d| d.str_field("Arn"))
                    .map(str::to_string),
                ..Delivery::default()
            });
        }
    }
}

fn put_events(ctx: &mut Ctx<'_>, region: &mut Region, input: &Value) -> OpResult {
    let entries = input.array_field("Entries");
    if entries.is_empty() || entries.len() > 10 {
        return Err(validation(
            "1 validation error detected: Value at 'entries' failed to satisfy constraint: Member must have length less than or equal to 10",
        ));
    }
    let mut failed = 0;
    let results: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let bus = bus_name(entry.str_field("EventBusName")).to_string();
            let outcome = if region.buses.contains_key(&bus) {
                build_event(ctx, entry)
            } else {
                Err((
                    "ResourceNotFoundException",
                    format!("Event bus {bus} does not exist."),
                ))
            };
            match outcome {
                Ok(event) => {
                    dispatch(ctx, region, &bus, &event, None);
                    json!({ "EventId": event["id"] })
                }
                Err((code, message)) => {
                    failed += 1;
                    json!({ "ErrorCode": code, "ErrorMessage": message })
                }
            }
        })
        .collect();
    Ok(json!({ "FailedEntryCount": failed, "Entries": results }))
}

impl Events {
    fn region(&mut self, ctx: &Ctx<'_>) -> &mut Region {
        self.regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_insert_with(|| Region::new(ctx))
    }
}

impl Service for Events {
    fn signing_name(&self) -> &'static str {
        "events"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("events")
    }

    fn deliver(&mut self, ctx: &mut Ctx<'_>, resource: &str, delivery: &Delivery) -> bool {
        let Some(bus) = resource.strip_prefix("event-bus/") else {
            return false;
        };
        let mut region = std::mem::take(self.region(ctx));
        let delivered = region.buses.contains_key(bus)
            && serde_json::from_str::<Value>(&delivery.body)
                .ok()
                .and_then(|body| {
                    if body.get("detail-type").is_some() {
                        Some(body)
                    } else {
                        build_event(ctx, &body).ok()
                    }
                })
                .map(|event| dispatch(ctx, &mut region, bus, &event, None))
                .is_some();
        *self.region(ctx) = region;
        delivered
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let mut region = std::mem::take(self.region(ctx));
        let result = match operation {
            "PutEvents" => put_events(ctx, &mut region, &input),
            "PutRule"
            | "PutTargets"
            | "DescribeRule"
            | "EnableRule"
            | "DisableRule"
            | "DeleteRule"
            | "ListRules"
            | "RemoveTargets"
            | "ListTargetsByRule"
            | "ListRuleNamesByTarget"
            | "TestEventPattern" => rule_op(ctx, &mut region, operation, &input),
            _ => resources::call(ctx, &mut region, operation, &input),
        };
        *self.region(ctx) = region;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_expressions() {
        assert!(valid_schedule("rate(5 minutes)"));
        assert!(valid_schedule("rate(1 hour)"));
        assert!(!valid_schedule("rate(1 hours)"));
        assert!(!valid_schedule("rate(0 minutes)"));
        assert!(valid_schedule("cron(0 12 * * ? *)"));
        assert!(!valid_schedule("cron(0 12 * *)"));
    }

    #[test]
    fn bus_references() {
        assert_eq!(bus_name(None), "default");
        assert_eq!(bus_name(Some("orders")), "orders");
        assert_eq!(
            bus_name(Some(
                "arn:aws:events:us-east-1:000000000000:event-bus/aws.partner/x.com/1/b"
            )),
            "aws.partner/x.com/1/b"
        );
    }
}
