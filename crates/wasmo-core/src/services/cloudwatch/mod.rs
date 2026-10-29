mod metrics;

use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use crate::codec::AwsError;
use crate::codec::time::format_iso8601;
use crate::embedded_model;
use crate::service::{Ctx, Delivery, InputExt, OpResult, Service, ServiceModel};
use metrics::{Bucket, Dims, Point, Val, aggregate};

type SeriesKey = (String, String, Dims);

#[derive(Debug, Clone)]
struct Alarm {
    config: Map<String, Value>,
    composite: bool,
    state: String,
    reason: String,
    reason_data: Option<String>,
    updated: f64,
    state_updated: f64,
    evaluated: Option<i64>,
}

#[derive(Debug, Default)]
struct Region {
    series: BTreeMap<SeriesKey, Vec<Point>>,
    alarms: BTreeMap<String, Alarm>,
    history: Vec<Value>,
    dashboards: BTreeMap<String, (String, f64)>,
    anomaly_detectors: Vec<Value>,
    insight_rules: BTreeMap<String, Value>,
    managed_rules: Vec<Value>,
    streams: BTreeMap<String, Value>,
    tags: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Default)]
pub struct CloudWatch {
    regions: HashMap<(String, String), Region>,
}

fn invalid(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "InvalidParameterValue", message)
}

fn missing(key: &str) -> AwsError {
    AwsError::new(
        400,
        "MissingParameter",
        format!("The parameter {key} is required."),
    )
}

fn not_found(message: impl Into<String>) -> AwsError {
    AwsError::new(404, "ResourceNotFound", message)
}

fn required<'a>(input: &'a Value, key: &str) -> Result<&'a str, AwsError> {
    input.str_field(key).ok_or_else(|| missing(key))
}

fn required_f64(input: &Value, key: &str) -> Result<f64, AwsError> {
    input
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| missing(key))
}

fn dims(v: Option<&Value>) -> Dims {
    let mut d: Dims = v
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|d| {
            Some((
                d.str_field("Name")?.to_string(),
                d.get("Value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ))
        })
        .collect();
    d.sort();
    d
}

fn dims_value(d: &Dims) -> Value {
    Value::Array(
        d.iter()
            .map(|(n, v)| json!({ "Name": n, "Value": v }))
            .collect(),
    )
}

fn strings(v: &[Value]) -> Vec<String> {
    v.iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn page(
    items: Vec<Value>,
    input: &Value,
    key: &str,
    default: usize,
) -> (Vec<Value>, Option<String>) {
    let start = input
        .str_field("NextToken")
        .and_then(|t| STANDARD.decode(t).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let limit = input.i64_field(key).map_or(default, |l| l.max(1) as usize);
    let end = (start + limit).min(items.len());
    let next = (end < items.len()).then(|| STANDARD.encode(end.to_string()));
    (items[start..end].to_vec(), next)
}

fn with_next(mut v: Value, next: Option<String>) -> Value {
    if let Some(n) = next {
        v["NextToken"] = json!(n);
    }
    v
}

fn tag_pairs(list: &[Value]) -> BTreeMap<String, String> {
    list.iter()
        .filter_map(|t| {
            Some((
                t.str_field("Key")?.to_string(),
                t.get("Value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ))
        })
        .collect()
}

fn put_metric_data(region: &mut Region, now: f64, input: &Value) -> Result<(), AwsError> {
    let ns = required(input, "Namespace")?;
    if ns.starts_with("AWS/") {
        return Err(invalid(
            "The value AWS/ for parameter Namespace is invalid.",
        ));
    }
    for d in input.array_field("MetricData") {
        let name = required(d, "MetricName")?.to_string();
        let unit = d.str_field("Unit").unwrap_or("None").to_string();
        let ts = d.get("Timestamp").and_then(Value::as_f64).unwrap_or(now);
        let point = if let Some(v) = d.get("Value").and_then(Value::as_f64) {
            Point::single(ts, v, &unit)
        } else if let Some(s) = d.obj_field("StatisticValues") {
            let f = |k: &str| s.get(k).and_then(Value::as_f64).unwrap_or_default();
            Point {
                ts,
                count: f("SampleCount"),
                sum: f("Sum"),
                min: f("Minimum"),
                max: f("Maximum"),
                values: Vec::new(),
                unit: unit.clone(),
            }
        } else if !d.array_field("Values").is_empty() {
            let values: Vec<f64> = d
                .array_field("Values")
                .iter()
                .filter_map(Value::as_f64)
                .collect();
            let counts: Vec<f64> = d
                .array_field("Counts")
                .iter()
                .filter_map(Value::as_f64)
                .collect();
            if !counts.is_empty() && counts.len() != values.len() {
                return Err(AwsError::new(
                    400,
                    "InvalidParameterCombination",
                    "The parameters MetricData.member.Values and MetricData.member.Counts must be of the same size.",
                ));
            }
            let mut p = Point {
                ts,
                min: f64::INFINITY,
                max: f64::NEG_INFINITY,
                unit: unit.clone(),
                ..Default::default()
            };
            for (i, v) in values.iter().enumerate() {
                let c = counts.get(i).copied().unwrap_or(1.0);
                p.count += c;
                p.sum += v * c;
                p.min = p.min.min(*v);
                p.max = p.max.max(*v);
                p.values
                    .extend(std::iter::repeat_n(*v, c.max(0.0) as usize));
            }
            p
        } else {
            return Err(AwsError::new(
                400,
                "InvalidParameterCombination",
                "One of the parameters Value, Values, or StatisticValues must be specified.",
            ));
        };
        region
            .series
            .entry((ns.to_string(), name, dims(d.get("Dimensions"))))
            .or_default()
            .push(point);
    }
    Ok(())
}

/// Buckets of the series a `Metric` structure names.
fn metric_buckets(
    region: &Region,
    metric: &Value,
    start: f64,
    end: f64,
    period: f64,
    unit: Option<&str>,
) -> BTreeMap<i64, Bucket> {
    let key = (
        metric
            .str_field("Namespace")
            .unwrap_or_default()
            .to_string(),
        metric
            .str_field("MetricName")
            .unwrap_or_default()
            .to_string(),
        dims(metric.get("Dimensions")),
    );
    region.series.get(&key).map_or_else(BTreeMap::new, |pts| {
        aggregate(pts.iter(), start, end, period, unit)
    })
}

/// Resolves `MetricDataQueries` into one series per query id.
fn resolve_queries(
    region: &Region,
    queries: &[Value],
    start: f64,
    end: f64,
) -> Result<BTreeMap<String, Val>, AwsError> {
    let mut vars = BTreeMap::new();
    for q in queries.iter().filter(|q| q.get("MetricStat").is_some()) {
        let id = required(q, "Id")?.to_string();
        let ms = &q["MetricStat"];
        let period = ms
            .get("Period")
            .and_then(Value::as_f64)
            .unwrap_or(60.0)
            .max(1.0);
        let stat = ms.str_field("Stat").unwrap_or("Average");
        let buckets = metric_buckets(
            region,
            &ms["Metric"],
            start,
            end,
            period,
            ms.str_field("Unit"),
        );
        vars.insert(
            id,
            Val::Series(
                buckets
                    .iter()
                    .filter_map(|(t, b)| Some((*t, b.stat(stat)?)))
                    .collect(),
            ),
        );
    }
    for q in queries.iter().filter(|q| q.get("Expression").is_some()) {
        let id = required(q, "Id")?.to_string();
        let v = metrics::eval(required(q, "Expression")?, &vars)
            .map_err(|e| AwsError::new(400, "ValidationError", e))?;
        vars.insert(id, v);
    }
    Ok(vars)
}

fn get_metric_data(region: &Region, input: &Value) -> OpResult {
    let queries = input.array_field("MetricDataQueries");
    if queries.is_empty() {
        return Err(missing("MetricDataQueries"));
    }
    let start = required_f64(input, "StartTime")?;
    let end = required_f64(input, "EndTime")?;
    if end <= start {
        return Err(invalid(
            "The parameter StartTime must be less than the parameter EndTime.",
        ));
    }
    let vars = resolve_queries(region, queries, start, end)?;
    let ascending = input.str_field("ScanBy") == Some("TimestampAscending");
    let results: Vec<Value> = queries
        .iter()
        .filter(|q| q.get("ReturnData").and_then(Value::as_bool) != Some(false))
        .filter_map(|q| {
            let id = q.str_field("Id")?;
            let label = q.str_field("Label").map_or_else(
                || {
                    q.pointer("/MetricStat/Metric/MetricName")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_string()
                },
                str::to_string,
            );
            let mut points: Vec<(i64, f64)> = match vars.get(id)? {
                Val::Series(s) => s.iter().map(|(t, v)| (*t, *v)).collect(),
                Val::Scalar(x) => vec![(start as i64, *x)],
            };
            if !ascending {
                points.reverse();
            }
            Some(json!({
                "Id": id,
                "Label": label,
                "Timestamps": points.iter().map(|(t, _)| *t as f64).collect::<Vec<_>>(),
                "Values": points.iter().map(|(_, v)| *v).collect::<Vec<_>>(),
                "StatusCode": "Complete",
            }))
        })
        .collect();
    Ok(json!({ "MetricDataResults": results, "Messages": [] }))
}

fn get_metric_statistics(region: &Region, input: &Value) -> OpResult {
    let ns = required(input, "Namespace")?;
    let name = required(input, "MetricName")?;
    let start = required_f64(input, "StartTime")?;
    let end = required_f64(input, "EndTime")?;
    let period = required_f64(input, "Period")?;
    if period < 1.0 {
        return Err(invalid("The parameter Period must be greater than 0."));
    }
    let stats = strings(input.array_field("Statistics"));
    let extended = strings(input.array_field("ExtendedStatistics"));
    if stats.is_empty() && extended.is_empty() {
        return Err(missing("Statistics"));
    }
    let metric =
        json!({ "Namespace": ns, "MetricName": name, "Dimensions": input.get("Dimensions") });
    let unit = input.str_field("Unit");
    let buckets = metric_buckets(region, &metric, start, end, period, unit);
    let series_unit = region
        .series
        .get(&(
            ns.to_string(),
            name.to_string(),
            dims(input.get("Dimensions")),
        ))
        .and_then(|p| p.last())
        .map_or("None".to_string(), |p| p.unit.clone());
    let points: Vec<Value> = buckets
        .iter()
        .map(|(t, b)| {
            let mut d = json!({ "Timestamp": *t as f64, "Unit": unit.unwrap_or(&series_unit) });
            for s in &stats {
                if let Some(v) = b.stat(s) {
                    d[s] = json!(v);
                }
            }
            if !extended.is_empty() {
                d["ExtendedStatistics"] = Value::Object(
                    extended
                        .iter()
                        .filter_map(|p| Some((p.clone(), json!(b.stat(p)?))))
                        .collect(),
                );
            }
            d
        })
        .collect();
    Ok(json!({ "Label": name, "Datapoints": points }))
}

fn breaches(op: &str, v: f64, threshold: f64) -> bool {
    match op {
        "GreaterThanThreshold" => v > threshold,
        "GreaterThanOrEqualToThreshold" => v >= threshold,
        "LessThanThreshold" => v < threshold,
        "LessThanOrEqualToThreshold" => v <= threshold,
        _ => false,
    }
}

/// Evaluates a metric alarm over its last `EvaluationPeriods` periods
/// (`DatapointsToAlarm` of them must breach) and returns the new state and
/// reason when it can be decided.
fn evaluate(region: &Region, a: &Alarm, now: f64) -> Option<(String, String)> {
    let c = &a.config;
    let period = c
        .get("Period")
        .and_then(Value::as_f64)
        .unwrap_or(60.0)
        .max(1.0);
    let n = c
        .get("EvaluationPeriods")
        .and_then(Value::as_i64)
        .unwrap_or(1)
        .max(1);
    let m = c
        .get("DatapointsToAlarm")
        .and_then(Value::as_i64)
        .unwrap_or(n);
    let threshold = c
        .get("Threshold")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let op = c
        .get("ComparisonOperator")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let current = (now / period).floor() * period;
    let start = current - (n - 1) as f64 * period;
    let end = current + period;
    let values: BTreeMap<i64, f64> = if let Some(queries) =
        c.get("Metrics").and_then(Value::as_array)
    {
        let vars = resolve_queries(region, queries, start, end).ok()?;
        let id = queries
            .iter()
            .find(|q| q.get("ReturnData").and_then(Value::as_bool) != Some(false))?
            .str_field("Id")?;
        match vars.get(id)? {
            Val::Series(s) => s.clone(),
            Val::Scalar(x) => BTreeMap::from([(current as i64, *x)]),
        }
    } else {
        let stat = c
            .get("Statistic")
            .or_else(|| c.get("ExtendedStatistic"))
            .and_then(Value::as_str)?;
        let metric = json!({ "Namespace": c.get("Namespace"), "MetricName": c.get("MetricName"), "Dimensions": c.get("Dimensions") });
        metric_buckets(
            region,
            &metric,
            start,
            end,
            period,
            c.get("Unit").and_then(Value::as_str),
        )
        .iter()
        .filter_map(|(t, b)| Some((*t, b.stat(stat)?)))
        .collect()
    };
    let treat = c
        .get("TreatMissingData")
        .and_then(Value::as_str)
        .unwrap_or("missing");
    if values.is_empty() {
        return match treat {
            "breaching" => Some((
                "ALARM".into(),
                "Threshold Crossed: no datapoints were received (treated as breaching).".into(),
            )),
            "notBreaching" => Some((
                "OK".into(),
                "Threshold Crossed: no datapoints were received (treated as not breaching).".into(),
            )),
            "ignore" => None,
            _ => Some((
                "INSUFFICIENT_DATA".into(),
                format!("Insufficient Data: {n} datapoints were unknown."),
            )),
        };
    }
    let missing = n - values.len() as i64;
    let breaching = values
        .values()
        .filter(|v| breaches(op, **v, threshold))
        .count() as i64
        + if treat == "breaching" { missing } else { 0 };
    let shown: Vec<String> = values
        .iter()
        .rev()
        .map(|(t, v)| format!("{v:?} ({})", format_iso8601(*t as f64)))
        .collect();
    let word = match op {
        "GreaterThanThreshold" => "greater than",
        "GreaterThanOrEqualToThreshold" => "greater than or equal to",
        "LessThanThreshold" => "less than",
        _ => "less than or equal to",
    };
    if breaching >= m {
        Some((
            "ALARM".into(),
            format!(
                "Threshold Crossed: {} datapoint{} [{}] {} {word} the threshold ({threshold:?}).",
                values.len(),
                if values.len() == 1 { "" } else { "s" },
                shown.join(", "),
                if values.len() == 1 { "was" } else { "were" },
            ),
        ))
    } else {
        Some((
            "OK".into(),
            format!(
                "Threshold Crossed: {} datapoint{} [{}] {} not {word} the threshold ({threshold:?}).",
                values.len(),
                if values.len() == 1 { "" } else { "s" },
                shown.join(", "),
                if values.len() == 1 { "was" } else { "were" },
            ),
        ))
    }
}

fn rule_state(region: &Region, rule: &str) -> String {
    let toks: Vec<String> = {
        let mut out = Vec::new();
        let chars: Vec<char> = rule.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c.is_whitespace() {
                i += 1;
            } else if c == '(' || c == ')' {
                out.push(c.to_string());
                i += 1;
            } else if c == '"' {
                let start = i + 1;
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    i += 1;
                }
                out.push(format!(
                    "\"{}",
                    chars[start..i.min(chars.len())].iter().collect::<String>()
                ));
                i += 1;
            } else {
                let start = i;
                while i < chars.len()
                    && !chars[i].is_whitespace()
                    && chars[i] != '('
                    && chars[i] != ')'
                {
                    i += 1;
                }
                out.push(chars[start..i].iter().collect());
            }
        }
        out
    };
    struct P<'a> {
        t: &'a [String],
        i: usize,
        r: &'a Region,
    }
    impl P<'_> {
        fn peek(&self) -> Option<&str> {
            self.t.get(self.i).map(String::as_str)
        }
        fn or(&mut self) -> &'static str {
            let mut l = self.and();
            while self.peek() == Some("OR") {
                self.i += 1;
                let r = self.and();
                l = if l == "ALARM" || r == "ALARM" {
                    "ALARM"
                } else if l == "OK" && r == "OK" {
                    "OK"
                } else {
                    "INSUFFICIENT_DATA"
                };
            }
            l
        }
        fn and(&mut self) -> &'static str {
            let mut l = self.not();
            while self.peek() == Some("AND") {
                self.i += 1;
                let r = self.not();
                l = if l == "OK" || r == "OK" {
                    "OK"
                } else if l == "ALARM" && r == "ALARM" {
                    "ALARM"
                } else {
                    "INSUFFICIENT_DATA"
                };
            }
            l
        }
        fn not(&mut self) -> &'static str {
            if self.peek() == Some("NOT") {
                self.i += 1;
                return match self.not() {
                    "ALARM" => "OK",
                    "OK" => "ALARM",
                    _ => "INSUFFICIENT_DATA",
                };
            }
            self.atom()
        }
        fn atom(&mut self) -> &'static str {
            let Some(t) = self.peek().map(str::to_string) else {
                return "INSUFFICIENT_DATA";
            };
            self.i += 1;
            match t.as_str() {
                "(" => {
                    let v = self.or();
                    self.i += 1;
                    v
                }
                "TRUE" => "ALARM",
                "FALSE" => "OK",
                s @ ("ALARM" | "OK" | "INSUFFICIENT_DATA") => {
                    self.i += 1;
                    let name = self
                        .peek()
                        .unwrap_or_default()
                        .trim_start_matches('"')
                        .to_string();
                    self.i += 2;
                    let name = name.rsplit(':').next().unwrap_or_default().to_string();
                    let state = self
                        .r
                        .alarms
                        .get(&name)
                        .map_or("INSUFFICIENT_DATA", |a| a.state.as_str());
                    if state == s {
                        "ALARM"
                    } else if s != "INSUFFICIENT_DATA" && state == "INSUFFICIENT_DATA" {
                        "INSUFFICIENT_DATA"
                    } else {
                        "OK"
                    }
                }
                _ => "INSUFFICIENT_DATA",
            }
        }
    }
    P {
        t: &toks,
        i: 0,
        r: region,
    }
    .or()
    .to_string()
}

fn alarm_view(ctx: &Ctx<'_>, a: &Alarm) -> Value {
    let mut v = Value::Object(a.config.clone());
    let name = a
        .config
        .get("AlarmName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    v["AlarmArn"] = json!(ctx.arn("cloudwatch", &format!("alarm:{name}")));
    v["StateValue"] = json!(a.state);
    v["StateReason"] = json!(a.reason);
    if let Some(d) = &a.reason_data {
        v["StateReasonData"] = json!(d);
    }
    v["StateUpdatedTimestamp"] = json!(a.state_updated);
    v["StateTransitionedTimestamp"] = json!(a.state_updated);
    v["AlarmConfigurationUpdatedTimestamp"] = json!(a.updated);
    for k in ["AlarmActions", "OKActions", "InsufficientDataActions"] {
        v.as_object_mut()
            .expect("alarm view is an object")
            .entry(k)
            .or_insert_with(|| json!([]));
    }
    v.as_object_mut()
        .expect("alarm view is an object")
        .entry("AlarmDescription")
        .or_insert_with(|| json!(""));
    if !a.composite {
        let d = dims(a.config.get("Dimensions"));
        v["Dimensions"] = dims_value(&d);
    }
    v
}

/// Moves an alarm to `state`, records history, and sends its actions.
fn transition(
    region: &mut Region,
    ctx: &mut Ctx<'_>,
    name: &str,
    state: &str,
    reason: String,
    data: Option<String>,
) {
    let now = ctx.now_ms() / 1000.0;
    let Some(a) = region.alarms.get_mut(name) else {
        return;
    };
    if a.state == state {
        return;
    }
    let old = std::mem::replace(&mut a.state, state.to_string());
    a.reason = reason.clone();
    a.reason_data = data;
    a.state_updated = now;
    let a = a.clone();
    let arn = ctx.arn("cloudwatch", &format!("alarm:{name}"));
    let kind = if a.composite {
        "CompositeAlarm"
    } else {
        "MetricAlarm"
    };
    region.history.push(json!({
        "AlarmName": name,
        "AlarmType": kind,
        "Timestamp": now,
        "HistoryItemType": "StateUpdate",
        "HistorySummary": format!("Alarm updated from {old} to {state}"),
        "HistoryData": json!({
            "version": "1.0",
            "oldState": { "stateValue": old },
            "newState": { "stateValue": state, "stateReason": reason },
        }).to_string(),
    }));
    let when = format_iso8601(now).replace('Z', "+0000");
    let event_id = ctx.uuid();
    ctx.deliver(Delivery {
        target: ctx.arn("events", "event-bus/default"),
        source: arn.clone(),
        body: json!({
            "version": "0",
            "id": event_id,
            "detail-type": "CloudWatch Alarm State Change",
            "source": "aws.cloudwatch",
            "account": ctx.account,
            "time": format_iso8601(now.floor()),
            "region": ctx.region,
            "resources": [arn],
            "detail": {
                "alarmName": name,
                "state": { "value": state, "reason": reason, "timestamp": when },
                "previousState": { "value": old },
            },
        })
        .to_string(),
        ..Default::default()
    });
    let enabled = a.config.get("ActionsEnabled").and_then(Value::as_bool) != Some(false);
    let key = match state {
        "ALARM" => "AlarmActions",
        "OK" => "OKActions",
        _ => "InsufficientDataActions",
    };
    let actions = strings(
        a.config
            .get(key)
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice),
    );
    if !enabled || actions.is_empty() {
        return;
    }
    let c = &a.config;
    let mut message = json!({
        "AlarmName": name,
        "AlarmDescription": c.get("AlarmDescription"),
        "AWSAccountId": ctx.account,
        "AlarmConfigurationUpdatedTimestamp": format_iso8601(a.updated).replace('Z', "+0000"),
        "NewStateValue": state,
        "NewStateReason": reason,
        "StateChangeTime": when,
        "Region": ctx.region,
        "AlarmArn": arn,
        "OldStateValue": old,
        "OKActions": c.get("OKActions").cloned().unwrap_or_else(|| json!([])),
        "AlarmActions": c.get("AlarmActions").cloned().unwrap_or_else(|| json!([])),
        "InsufficientDataActions": c.get("InsufficientDataActions").cloned().unwrap_or_else(|| json!([])),
    });
    if a.composite {
        message["AlarmRule"] = c.get("AlarmRule").cloned().unwrap_or_default();
    } else {
        message["Trigger"] = json!({
            "MetricName": c.get("MetricName"),
            "Namespace": c.get("Namespace"),
            "StatisticType": if c.contains_key("ExtendedStatistic") { "ExtendedStatistic" } else { "Statistic" },
            "Statistic": c.get("Statistic").and_then(Value::as_str).map(str::to_uppercase),
            "Unit": c.get("Unit"),
            "Dimensions": dims(c.get("Dimensions")).iter().map(|(n, v)| json!({ "value": v, "name": n })).collect::<Vec<_>>(),
            "Period": c.get("Period"),
            "EvaluationPeriods": c.get("EvaluationPeriods"),
            "ComparisonOperator": c.get("ComparisonOperator"),
            "Threshold": c.get("Threshold"),
            "TreatMissingData": c.get("TreatMissingData").and_then(Value::as_str).unwrap_or("missing"),
        });
    }
    let subject = format!("{state}: \"{name}\" in {}", ctx.region);
    let mut summary = Vec::new();
    for action in actions {
        if action.starts_with("arn:aws:sns:") {
            ctx.deliver(Delivery {
                target: action.clone(),
                source: arn.clone(),
                body: message.to_string(),
                subject: Some(subject.chars().take(100).collect()),
                ..Default::default()
            });
            summary.push(action);
        }
    }
    if !summary.is_empty() {
        region.history.push(json!({
            "AlarmName": name,
            "AlarmType": kind,
            "Timestamp": now,
            "HistoryItemType": "Action",
            "HistorySummary": format!("Successfully executed action {}", summary.join(", ")),
        }));
    }
}

/// Re-evaluates metric alarms whose data or period changed, then composite
/// alarms, sending actions for every transition.
fn tick(region: &mut Region, ctx: &mut Ctx<'_>, touched: &[SeriesKey]) {
    let now = ctx.now_ms() / 1000.0;
    let names: Vec<String> = region.alarms.keys().cloned().collect();
    for name in &names {
        let a = &region.alarms[name];
        if a.composite {
            continue;
        }
        let period = a
            .config
            .get("Period")
            .and_then(Value::as_f64)
            .unwrap_or(60.0)
            .max(1.0);
        let slot = (now / period).floor() as i64;
        let key = (
            a.config
                .get("Namespace")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            a.config
                .get("MetricName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            dims(a.config.get("Dimensions")),
        );
        let dirty = a.evaluated != Some(slot)
            || touched.contains(&key)
            || (a.config.contains_key("Metrics") && !touched.is_empty());
        if !dirty {
            continue;
        }
        let result = evaluate(region, a, now);
        if let Some(a) = region.alarms.get_mut(name) {
            a.evaluated = Some(slot);
        }
        if let Some((state, reason)) = result {
            transition(region, ctx, name, &state, reason, None);
        }
    }
    for _ in 0..4 {
        let mut changed = false;
        for name in &names {
            let Some(a) = region.alarms.get(name).filter(|a| a.composite) else {
                continue;
            };
            let rule = a
                .config
                .get("AlarmRule")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let state = rule_state(region, rule);
            if state != region.alarms[name].state {
                transition(
                    region,
                    ctx,
                    name,
                    &state,
                    format!("Composite alarm rule evaluated to {state}."),
                    None,
                );
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

fn new_alarm(region: &mut Region, ctx: &mut Ctx<'_>, input: &Value, composite: bool) -> OpResult {
    let name = required(input, "AlarmName")?.to_string();
    let now = ctx.now_ms() / 1000.0;
    let mut config = input.as_object().cloned().unwrap_or_default();
    config.remove("Tags");
    config.entry("ActionsEnabled").or_insert(json!(true));
    if composite {
        required(input, "AlarmRule")?;
    } else {
        for k in ["EvaluationPeriods", "ComparisonOperator"] {
            if input.get(k).is_none() {
                return Err(missing(k));
            }
        }
        if input.get("Metrics").is_none() && input.get("MetricName").is_none() {
            return Err(AwsError::new(
                400,
                "ValidationError",
                "Either MetricName or Metrics must be specified.",
            ));
        }
    }
    let arn = ctx.arn("cloudwatch", &format!("alarm:{name}"));
    let tags = tag_pairs(input.array_field("Tags"));
    if !tags.is_empty() {
        region.tags.entry(arn).or_default().extend(tags);
    }
    let previous = region.alarms.remove(&name);
    let alarm = match previous {
        Some(p) => Alarm {
            config,
            composite,
            updated: now,
            evaluated: None,
            ..p
        },
        None => {
            region.history.push(json!({
                "AlarmName": name,
                "AlarmType": if composite { "CompositeAlarm" } else { "MetricAlarm" },
                "Timestamp": now,
                "HistoryItemType": "ConfigurationUpdate",
                "HistorySummary": format!("Alarm \"{name}\" created"),
            }));
            Alarm {
                config,
                composite,
                state: "INSUFFICIENT_DATA".to_string(),
                reason: "Unchecked: Initial alarm creation".to_string(),
                reason_data: None,
                updated: now,
                state_updated: now,
                evaluated: None,
            }
        }
    };
    region.alarms.insert(name, alarm);
    Ok(json!({}))
}

fn alarms_op(
    region: &mut Region,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    let now = ctx.now_ms() / 1000.0;
    Ok(Some(match operation {
        "PutMetricAlarm" => new_alarm(region, ctx, input, false)?,
        "PutCompositeAlarm" => new_alarm(region, ctx, input, true)?,
        "DescribeAlarms" => {
            let names = strings(input.array_field("AlarmNames"));
            let prefix = input.str_field("AlarmNamePrefix");
            let types = strings(input.array_field("AlarmTypes"));
            let state = input.str_field("StateValue");
            let action = input.str_field("ActionPrefix");
            let children: Option<String> = input.str_field("ChildrenOfAlarmName").map(|p| {
                region
                    .alarms
                    .get(p)
                    .and_then(|a| a.config.get("AlarmRule"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            });
            let parents = input.str_field("ParentsOfAlarmName");
            let selected: Vec<&Alarm> = region
                .alarms
                .values()
                .filter(|a| {
                    let name = a
                        .config
                        .get("AlarmName")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    (names.is_empty() || names.iter().any(|n| n == name))
                        && prefix.is_none_or(|p| name.starts_with(p))
                        && state.is_none_or(|s| a.state == s)
                        && children.as_ref().is_none_or(|rule| {
                            rule.contains(&format!("\"{name}\""))
                                || rule.contains(&format!(":alarm:{name}\""))
                        })
                        && parents.is_none_or(|p| {
                            a.config
                                .get("AlarmRule")
                                .and_then(Value::as_str)
                                .is_some_and(|r| r.contains(p))
                        })
                        && action.is_none_or(|p| {
                            ["AlarmActions", "OKActions", "InsufficientDataActions"]
                                .iter()
                                .any(|k| {
                                    a.config.get(*k).and_then(Value::as_array).is_some_and(|l| {
                                        l.iter()
                                            .any(|x| x.as_str().is_some_and(|x| x.starts_with(p)))
                                    })
                                })
                        })
                })
                .collect();
            let (items, next) = page(
                selected
                    .iter()
                    .filter(|a| match (&children, parents) {
                        (Some(_), _) => true,
                        (_, Some(_)) => a.composite,
                        _ if a.composite => types.iter().any(|t| t == "CompositeAlarm"),
                        _ => types.is_empty() || types.iter().any(|t| t == "MetricAlarm"),
                    })
                    .map(|a| json!({ "composite": a.composite, "view": alarm_view(ctx, a) }))
                    .collect(),
                input,
                "MaxRecords",
                50,
            );
            let pick = |c: bool| -> Vec<Value> {
                items
                    .iter()
                    .filter(|x| x["composite"] == c)
                    .map(|x| x["view"].clone())
                    .collect()
            };
            with_next(
                json!({ "MetricAlarms": pick(false), "CompositeAlarms": pick(true) }),
                next,
            )
        }
        "DescribeAlarmsForMetric" => {
            let ns = required(input, "Namespace")?;
            let name = required(input, "MetricName")?;
            let d = dims(input.get("Dimensions"));
            let stat = input.str_field("Statistic");
            let period = input.get("Period").and_then(Value::as_f64);
            json!({ "MetricAlarms": region.alarms.values().filter(|a| {
                let c = &a.config;
                !a.composite
                    && c.get("Namespace").and_then(Value::as_str) == Some(ns)
                    && c.get("MetricName").and_then(Value::as_str) == Some(name)
                    && (d.is_empty() || dims(c.get("Dimensions")) == d)
                    && stat.is_none_or(|s| c.get("Statistic").and_then(Value::as_str) == Some(s))
                    && period.is_none_or(|p| c.get("Period").and_then(Value::as_f64) == Some(p))
            }).map(|a| alarm_view(ctx, a)).collect::<Vec<_>>() })
        }
        "DeleteAlarms" => {
            let names = strings(input.array_field("AlarmNames"));
            for n in &names {
                if region.alarms.get(n).is_some_and(|a| !a.composite) {
                    let parent = region.alarms.values().find(|p| {
                        p.composite
                            && !names.iter().any(|x| {
                                p.config.get("AlarmName").and_then(Value::as_str) == Some(x)
                            })
                            && p.config
                                .get("AlarmRule")
                                .and_then(Value::as_str)
                                .is_some_and(|r| {
                                    r.contains(&format!("\"{n}\""))
                                        || r.contains(&format!(":alarm:{n}\""))
                                })
                    });
                    if let Some(p) = parent {
                        return Err(invalid(format!(
                            "Alarm {n} is referenced by composite alarm {}.",
                            p.config
                                .get("AlarmName")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                        )));
                    }
                }
            }
            for n in names {
                region.alarms.remove(&n);
                region
                    .tags
                    .remove(&ctx.arn("cloudwatch", &format!("alarm:{n}")));
            }
            json!({})
        }
        "SetAlarmState" => {
            let name = required(input, "AlarmName")?.to_string();
            let state = required(input, "StateValue")?.to_string();
            let reason = required(input, "StateReason")?.to_string();
            let data = input.str_field("StateReasonData").map(str::to_string);
            if let Some(d) = &data
                && serde_json::from_str::<Value>(d).is_err()
            {
                return Err(AwsError::new(
                    400,
                    "InvalidFormat",
                    "StateReasonData is not valid JSON.",
                ));
            }
            let a = region
                .alarms
                .get_mut(&name)
                .ok_or_else(|| not_found(format!("Alarm {name} not found")))?;
            let period = a
                .config
                .get("Period")
                .and_then(Value::as_f64)
                .unwrap_or(60.0)
                .max(1.0);
            a.evaluated = Some((now / period).floor() as i64);
            transition(region, ctx, &name, &state, reason, data);
            json!({})
        }
        "EnableAlarmActions" | "DisableAlarmActions" => {
            let on = operation == "EnableAlarmActions";
            for n in strings(input.array_field("AlarmNames")) {
                if let Some(a) = region.alarms.get_mut(&n) {
                    a.config.insert("ActionsEnabled".to_string(), json!(on));
                }
            }
            json!({})
        }
        "DescribeAlarmHistory" => {
            let name = input.str_field("AlarmName");
            let kind = input.str_field("HistoryItemType");
            let types = strings(input.array_field("AlarmTypes"));
            let start = input.get("StartDate").and_then(Value::as_f64);
            let end = input.get("EndDate").and_then(Value::as_f64);
            let mut items: Vec<Value> = region
                .history
                .iter()
                .filter(|h| name.is_none_or(|n| h["AlarmName"] == n))
                .filter(|h| kind.is_none_or(|k| h["HistoryItemType"] == k))
                .filter(|h| types.is_empty() || types.iter().any(|t| h["AlarmType"] == t.as_str()))
                .filter(|h| {
                    let t = h["Timestamp"].as_f64().unwrap_or_default();
                    start.is_none_or(|s| t >= s) && end.is_none_or(|e| t <= e)
                })
                .cloned()
                .collect();
            if input.str_field("ScanBy") != Some("TimestampAscending") {
                items.reverse();
            }
            let (items, next) = page(items, input, "MaxRecords", 100);
            with_next(json!({ "AlarmHistoryItems": items }), next)
        }
        "DescribeAlarmContributors" => {
            let name = required(input, "AlarmName")?;
            region
                .alarms
                .get(name)
                .ok_or_else(|| not_found(format!("Alarm {name} not found")))?;
            json!({ "AlarmContributors": [] })
        }
        _ => return Ok(None),
    }))
}

fn resources_op(
    region: &mut Region,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    let now = ctx.now_ms() / 1000.0;
    Ok(Some(match operation {
        "PutDashboard" => {
            let name = required(input, "DashboardName")?.to_string();
            let body = required(input, "DashboardBody")?.to_string();
            let doc: Value = serde_json::from_str(&body).map_err(|_| {
                AwsError::new(
                    400,
                    "InvalidParameterInput",
                    "The field DashboardBody must be a valid JSON object",
                )
            })?;
            if !doc.is_object() {
                return Err(AwsError::new(
                    400,
                    "InvalidParameterInput",
                    "The field DashboardBody must be a valid JSON object",
                ));
            }
            region.dashboards.insert(name, (body, now));
            json!({ "DashboardValidationMessages": [] })
        }
        "GetDashboard" => {
            let name = required(input, "DashboardName")?;
            let (body, _) = region
                .dashboards
                .get(name)
                .ok_or_else(|| not_found(format!("Dashboard {name} does not exist")))?;
            json!({
                "DashboardArn": format!("arn:aws:cloudwatch::{}:dashboard/{name}", ctx.account),
                "DashboardName": name,
                "DashboardBody": body,
            })
        }
        "ListDashboards" => {
            let prefix = input.str_field("DashboardNamePrefix");
            let list: Vec<Value> = region
                .dashboards
                .iter()
                .filter(|(n, _)| prefix.is_none_or(|p| n.starts_with(p)))
                .map(|(n, (body, at))| json!({
                    "DashboardName": n,
                    "DashboardArn": format!("arn:aws:cloudwatch::{}:dashboard/{n}", ctx.account),
                    "LastModified": at,
                    "Size": body.len(),
                }))
                .collect();
            let (list, next) = page(list, input, "MaxResults", 1000);
            with_next(json!({ "DashboardEntries": list }), next)
        }
        "DeleteDashboards" => {
            let names = strings(input.array_field("DashboardNames"));
            if let Some(n) = names.iter().find(|n| !region.dashboards.contains_key(*n)) {
                return Err(not_found(format!("Dashboard {n} does not exist")));
            }
            names.iter().for_each(|n| {
                region.dashboards.remove(n);
            });
            json!({})
        }
        "PutAnomalyDetector" => {
            let mut d = input.as_object().cloned().unwrap_or_default();
            if let Some(single) = d.remove("SingleMetricAnomalyDetector") {
                for (k, v) in single.as_object().into_iter().flatten() {
                    d.insert(k.clone(), v.clone());
                }
                d.insert("SingleMetricAnomalyDetector".to_string(), single);
            }
            d.entry("StateValue").or_insert(json!("TRAINED"));
            let key = |v: &Map<String, Value>| {
                (
                    v.get("Namespace").cloned(),
                    v.get("MetricName").cloned(),
                    dims(v.get("Dimensions")),
                    v.get("Stat").cloned(),
                    v.get("MetricMathAnomalyDetector").cloned(),
                )
            };
            let k = key(&d);
            region
                .anomaly_detectors
                .retain(|x| x.as_object().is_none_or(|x| key(x) != k));
            region.anomaly_detectors.push(Value::Object(d));
            json!({})
        }
        "DescribeAnomalyDetectors" => {
            let ns = input.str_field("Namespace");
            let name = input.str_field("MetricName");
            let d = dims(input.get("Dimensions"));
            let list: Vec<Value> = region
                .anomaly_detectors
                .iter()
                .filter(|x| ns.is_none_or(|n| x.str_field("Namespace") == Some(n)))
                .filter(|x| name.is_none_or(|n| x.str_field("MetricName") == Some(n)))
                .filter(|x| d.is_empty() || dims(x.get("Dimensions")) == d)
                .cloned()
                .collect();
            let (list, next) = page(list, input, "MaxResults", 100);
            with_next(json!({ "AnomalyDetectors": list }), next)
        }
        "DeleteAnomalyDetector" => {
            let single = input
                .get("SingleMetricAnomalyDetector")
                .cloned()
                .unwrap_or_else(|| input.clone());
            let before = region.anomaly_detectors.len();
            region.anomaly_detectors.retain(|x| {
                !(x.get("Namespace") == single.get("Namespace")
                    && x.get("MetricName") == single.get("MetricName")
                    && x.get("Stat") == single.get("Stat")
                    && dims(x.get("Dimensions")) == dims(single.get("Dimensions")))
            });
            if before == region.anomaly_detectors.len() {
                return Err(AwsError::new(
                    404,
                    "ResourceNotFoundException",
                    "No anomaly detector found for the given parameters.",
                ));
            }
            json!({})
        }
        "PutInsightRule" => {
            let name = required(input, "RuleName")?.to_string();
            let definition = required(input, "RuleDefinition")?;
            let schema = serde_json::from_str::<Value>(definition)
                .ok()
                .and_then(|d| {
                    d.pointer("/Schema/Name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "CloudWatchLogRule/1".to_string());
            let arn = ctx.arn("cloudwatch", &format!("insight-rule/{name}"));
            let tags = tag_pairs(input.array_field("Tags"));
            if !tags.is_empty() {
                region.tags.entry(arn).or_default().extend(tags);
            }
            region.insight_rules.insert(
                name.clone(),
                json!({
                    "Name": name,
                    "State": input.str_field("RuleState").unwrap_or("ENABLED"),
                    "Schema": schema,
                    "Definition": definition,
                    "ManagedRule": false,
                }),
            );
            json!({})
        }
        "DescribeInsightRules" => {
            let (list, next) = page(
                region.insight_rules.values().cloned().collect(),
                input,
                "MaxResults",
                500,
            );
            with_next(json!({ "InsightRules": list }), next)
        }
        "DeleteInsightRules" | "EnableInsightRules" | "DisableInsightRules" => {
            let mut failures = Vec::new();
            for n in strings(input.array_field("RuleNames")) {
                match region.insight_rules.get_mut(&n) {
                    None => failures.push(json!({ "FailureResource": n, "ExceptionType": "ResourceNotFound", "FailureCode": "404", "FailureDescription": "Rule does not exist" })),
                    Some(r) if operation == "EnableInsightRules" => r["State"] = json!("ENABLED"),
                    Some(r) if operation == "DisableInsightRules" => r["State"] = json!("DISABLED"),
                    Some(_) => {
                        region.insight_rules.remove(&n);
                    }
                }
            }
            json!({ "Failures": failures })
        }
        "GetInsightRuleReport" => {
            let name = required(input, "RuleName")?;
            region
                .insight_rules
                .get(name)
                .ok_or_else(|| not_found(format!("Rule {name} does not exist")))?;
            json!({ "KeyLabels": [], "AggregationStatistic": "Sum", "AggregateValue": 0.0, "ApproximateUniqueCount": 0, "Contributors": [], "MetricDatapoints": [] })
        }
        "PutManagedInsightRules" => {
            for r in input.array_field("ManagedRules") {
                region.managed_rules.retain(|x| {
                    !(x.get("TemplateName") == r.get("TemplateName")
                        && x.get("ResourceARN") == r.get("ResourceARN"))
                });
                region.managed_rules.push(r.clone());
            }
            json!({ "Failures": [] })
        }
        "ListManagedInsightRules" => {
            let resource = required(input, "ResourceARN")?;
            let list: Vec<Value> = region
                .managed_rules
                .iter()
                .filter(|r| r.str_field("ResourceARN") == Some(resource))
                .map(|r| json!({ "TemplateName": r.get("TemplateName"), "ResourceARN": resource, "RuleState": { "RuleName": format!("{}-{}", r.str_field("TemplateName").unwrap_or_default(), ctx.uuid()), "State": "ENABLED" } }))
                .collect();
            let (list, next) = page(list, input, "MaxResults", 100);
            with_next(json!({ "ManagedRules": list }), next)
        }
        "PutMetricStream" => {
            let name = required(input, "Name")?.to_string();
            let arn = ctx.arn("cloudwatch", &format!("metric-stream/{name}"));
            let created = region
                .streams
                .get(&name)
                .and_then(|s| s.get("CreationDate").cloned())
                .unwrap_or(json!(now));
            let mut s = input.as_object().cloned().unwrap_or_default();
            s.remove("Tags");
            s.insert("Arn".to_string(), json!(arn));
            s.insert("CreationDate".to_string(), created);
            s.insert("LastUpdateDate".to_string(), json!(now));
            s.insert("State".to_string(), json!("running"));
            let tags = tag_pairs(input.array_field("Tags"));
            if !tags.is_empty() {
                region.tags.entry(arn.clone()).or_default().extend(tags);
            }
            region.streams.insert(name, Value::Object(s));
            json!({ "Arn": arn })
        }
        "GetMetricStream" => {
            let name = required(input, "Name")?;
            region.streams.get(name).cloned().ok_or_else(|| {
                AwsError::new(
                    404,
                    "ResourceNotFoundException",
                    format!("Metric stream {name} does not exist"),
                )
            })?
        }
        "ListMetricStreams" => {
            let (list, next) = page(
                region.streams.values().cloned().collect(),
                input,
                "MaxResults",
                500,
            );
            with_next(json!({ "Entries": list }), next)
        }
        "DeleteMetricStream" => {
            region.streams.remove(required(input, "Name")?);
            json!({})
        }
        "StartMetricStreams" | "StopMetricStreams" => {
            let state = if operation == "StartMetricStreams" {
                "running"
            } else {
                "stopped"
            };
            for n in strings(input.array_field("Names")) {
                if let Some(s) = region.streams.get_mut(&n) {
                    s["State"] = json!(state);
                    s["LastUpdateDate"] = json!(now);
                }
            }
            json!({})
        }
        "TagResource" | "UntagResource" | "ListTagsForResource" => {
            let arn = required(input, "ResourceARN")?.to_string();
            let name = arn.rsplit([':', '/']).next().unwrap_or_default();
            let known = (arn.contains(":alarm:") && region.alarms.contains_key(name))
                || (arn.contains(":insight-rule/") && region.insight_rules.contains_key(name))
                || (arn.contains(":metric-stream/") && region.streams.contains_key(name))
                || (arn.contains(":dashboard/") && region.dashboards.contains_key(name));
            if !known {
                return Err(AwsError::new(
                    404,
                    "ResourceNotFoundException",
                    format!("Resource {arn} not found"),
                ));
            }
            let tags = region.tags.entry(arn).or_default();
            match operation {
                "TagResource" => {
                    tags.extend(tag_pairs(input.array_field("Tags")));
                    json!({})
                }
                "UntagResource" => {
                    for k in strings(input.array_field("TagKeys")) {
                        tags.remove(&k);
                    }
                    json!({})
                }
                _ => {
                    json!({ "Tags": tags.iter().map(|(k, v)| json!({ "Key": k, "Value": v })).collect::<Vec<_>>() })
                }
            }
        }
        _ => return Ok(None),
    }))
}

fn call(region: &mut Region, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> OpResult {
    let now = ctx.now_ms() / 1000.0;
    match operation {
        "PutMetricData" => {
            let ns = required(input, "Namespace")?.to_string();
            put_metric_data(region, now, input)?;
            let touched: Vec<SeriesKey> = input
                .array_field("MetricData")
                .iter()
                .filter_map(|d| {
                    Some((
                        ns.clone(),
                        d.str_field("MetricName")?.to_string(),
                        dims(d.get("Dimensions")),
                    ))
                })
                .collect();
            tick(region, ctx, &touched);
            return Ok(json!({}));
        }
        "GetMetricData" => return get_metric_data(region, input),
        "GetMetricStatistics" => return get_metric_statistics(region, input),
        "ListMetrics" => {
            let ns = input.str_field("Namespace");
            let name = input.str_field("MetricName");
            let filters: Vec<(String, Option<String>)> = input
                .array_field("Dimensions")
                .iter()
                .filter_map(|d| {
                    Some((
                        d.str_field("Name")?.to_string(),
                        d.str_field("Value").map(str::to_string),
                    ))
                })
                .collect();
            let recent = input.str_field("RecentlyActive") == Some("PT3H");
            let list: Vec<Value> = region
                .series
                .iter()
                .filter(|((n, m, _), _)| ns.is_none_or(|x| x == n) && name.is_none_or(|x| x == m))
                .filter(|((_, _, d), _)| filters.iter().all(|(k, v)| d.iter().any(|(dk, dv)| dk == k && v.as_ref().is_none_or(|v| v == dv))))
                .filter(|(_, pts)| !recent || pts.iter().any(|p| p.ts >= now - 3.0 * 3600.0))
                .map(|((n, m, d), _)| json!({ "Namespace": n, "MetricName": m, "Dimensions": dims_value(d) }))
                .collect();
            let (list, next) = page(list, input, "MaxResults", 500);
            return Ok(with_next(json!({ "Metrics": list }), next));
        }
        _ => {}
    }
    tick(region, ctx, &[]);
    for part in [alarms_op, resources_op] {
        if let Some(out) = part(region, ctx, operation, input)? {
            return Ok(out);
        }
    }
    Err(AwsError::new(
        400,
        "InvalidAction",
        format!("wasmo: {operation} is not implemented"),
    ))
}

impl Service for CloudWatch {
    fn signing_name(&self) -> &'static str {
        "monitoring"
    }

    fn arn_service(&self) -> &'static str {
        "cloudwatch"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("cloudwatch")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let region = self
            .regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default();
        call(region, ctx, operation, &input)
    }

    fn deliver(&mut self, ctx: &mut Ctx<'_>, resource: &str, delivery: &Delivery) -> bool {
        if resource != "metric-data" {
            return false;
        }
        let Ok(input) = serde_json::from_str::<Value>(&delivery.body) else {
            return false;
        };
        let region = self
            .regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default();
        call(region, ctx, "PutMetricData", &input).is_ok()
    }
}
