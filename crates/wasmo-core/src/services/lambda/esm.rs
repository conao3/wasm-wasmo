use std::collections::{BTreeMap, BTreeSet};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::invoke::{Kind, Outcome, Pending, Start, send_record};
use super::{
    Lambda, MapExt, Region, SqsDelete, conflict, function_arn, invalid, no_function, not_found,
    page, split_name,
};
use crate::codec::AwsError;
use crate::codec::time::format_iso8601;
use crate::service::{Arn, Ctx, InputExt, Service};
use crate::services::pattern;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Sqs,
    Kinesis,
    Dynamo,
    Other,
}

impl Source {
    fn of(arn: &str) -> Self {
        match Arn::parse(arn) {
            Some(a) if a.service == "sqs" => Self::Sqs,
            Some(a) if a.service == "kinesis" => Self::Kinesis,
            Some(a) if a.service == "dynamodb" && a.resource.contains("/stream/") => Self::Dynamo,
            _ => Self::Other,
        }
    }

    fn stream(self) -> bool {
        matches!(self, Self::Kinesis | Self::Dynamo)
    }
}

#[derive(Debug, Clone, Default)]
struct Cursor {
    iterator: Option<String>,
    restart_at: Option<String>,
    in_flight: bool,
    finished: bool,
    retries: i64,
    blocked: u64,
}

#[derive(Debug, Clone)]
pub(super) struct Esm {
    config: Map<String, Value>,
    function: String,
    qualifier: Option<String>,
    source: Source,
    enabled: bool,
    in_flight: bool,
    shards: BTreeMap<String, Cursor>,
}

impl Esm {
    fn batch_size(&self) -> usize {
        self.config
            .get("BatchSize")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .max(1) as usize
    }

    fn reports_failures(&self) -> bool {
        self.config
            .get("FunctionResponseTypes")
            .and_then(Value::as_array)
            .is_some_and(|t| {
                t.iter()
                    .any(|x| x.as_str() == Some("ReportBatchItemFailures"))
            })
    }

    fn filters(&self) -> Vec<Value> {
        self.config
            .get("FilterCriteria")
            .and_then(|f| f.get("Filters"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|f| f.str_field("Pattern"))
            .filter_map(|p| pattern::parse(p).ok())
            .collect()
    }

    fn view(&self, state: &str) -> Value {
        let mut c = self.config.clone();
        c.insert("State".into(), json!(state));
        Value::Object(c)
    }

    fn state(&self) -> &'static str {
        if self.enabled { "Enabled" } else { "Disabled" }
    }
}

/// An event source mapping due for a poll, with the cursors of its shards.
pub struct Poll {
    pub account: String,
    pub region: String,
    uuid: String,
    source: Source,
    arn: String,
    batch: usize,
    starting: String,
    timestamp: Option<f64>,
    tick: u64,
    shards: BTreeMap<String, Cursor>,
}

pub(super) struct ShardBatch {
    shard: String,
    records: Vec<Value>,
    next: Option<String>,
}

#[derive(Default)]
pub struct Batch {
    messages: Vec<Value>,
    shards: Vec<ShardBatch>,
    known: Vec<String>,
}

pub(super) enum InFlight {
    Sqs {
        uuid: String,
        queue_arn: String,
        handles: Vec<(String, String)>,
    },
    Stream {
        uuid: String,
        shard: String,
        next: Option<String>,
        sequences: Vec<String>,
        arrivals: (Value, Value),
    },
}

fn op(svc: &mut dyn Service, ctx: &mut Ctx<'_>, name: &str, input: Value) -> Value {
    svc.call(ctx, name, input).unwrap_or(Value::Null)
}

impl Poll {
    pub fn source_model(&self) -> &'static str {
        match self.source {
            Source::Sqs => "sqs",
            Source::Kinesis => "kinesis",
            _ => "dynamodbstreams",
        }
    }

    /// Reads the next batch from the source through its own operations.
    pub fn fetch(&self, svc: &mut dyn Service, ctx: &mut Ctx<'_>) -> Batch {
        match self.source {
            Source::Sqs => self.fetch_queue(svc, ctx),
            _ => self.fetch_stream(svc, ctx),
        }
    }

    fn fetch_queue(&self, svc: &mut dyn Service, ctx: &mut Ctx<'_>) -> Batch {
        let Some(arn) = Arn::parse(&self.arn) else {
            return Batch::default();
        };
        let url = format!("{}/{}/{}", ctx.endpoint, arn.account, arn.resource);
        let mut messages = Vec::new();
        while messages.len() < self.batch {
            let out = op(
                svc,
                ctx,
                "ReceiveMessage",
                json!({
                    "QueueUrl": url,
                    "MaxNumberOfMessages": (self.batch - messages.len()).min(10),
                    "MessageSystemAttributeNames": ["All"],
                    "MessageAttributeNames": ["All"],
                }),
            );
            let got = out.array_field("Messages");
            if got.is_empty() {
                break;
            }
            messages.extend(got.iter().cloned());
        }
        Batch {
            messages,
            ..Batch::default()
        }
    }

    fn fetch_stream(&self, svc: &mut dyn Service, ctx: &mut Ctx<'_>) -> Batch {
        let kinesis = self.source == Source::Kinesis;
        let known: Vec<String> = if kinesis {
            op(svc, ctx, "ListShards", json!({ "StreamARN": self.arn }))
                .array_field("Shards")
                .iter()
                .filter_map(|s| s.str_field("ShardId").map(str::to_string))
                .collect()
        } else {
            op(svc, ctx, "DescribeStream", json!({ "StreamArn": self.arn }))
                .get("StreamDescription")
                .map(|d| d.array_field("Shards"))
                .unwrap_or_default()
                .iter()
                .filter_map(|s| s.str_field("ShardId").map(str::to_string))
                .collect()
        };
        let arn_key = if kinesis { "StreamARN" } else { "StreamArn" };
        let first = self.shards.is_empty();
        let mut shards = Vec::new();
        for shard in &known {
            let cursor = self.shards.get(shard).cloned().unwrap_or_default();
            if cursor.in_flight || cursor.finished || cursor.blocked == self.tick {
                continue;
            }
            let iterator = match (&cursor.restart_at, &cursor.iterator) {
                (Some(seq), _) => {
                    let out = op(
                        svc,
                        ctx,
                        "GetShardIterator",
                        json!({ arn_key: self.arn, "ShardId": shard, "ShardIteratorType": "AT_SEQUENCE_NUMBER", "StartingSequenceNumber": seq }),
                    );
                    out.str_field("ShardIterator").map(str::to_string)
                }
                (None, Some(it)) => Some(it.clone()),
                (None, None) => {
                    let kind = if first {
                        self.starting.as_str()
                    } else {
                        "TRIM_HORIZON"
                    };
                    let mut input =
                        json!({ arn_key: self.arn, "ShardId": shard, "ShardIteratorType": kind });
                    if let Some(t) = self.timestamp {
                        input["Timestamp"] = json!(t);
                    }
                    op(svc, ctx, "GetShardIterator", input)
                        .str_field("ShardIterator")
                        .map(str::to_string)
                }
            };
            let Some(iterator) = iterator else {
                continue;
            };
            let mut input = json!({ "ShardIterator": iterator, "Limit": self.batch });
            if kinesis {
                input["StreamARN"] = json!(self.arn);
            }
            let out = op(svc, ctx, "GetRecords", input);
            shards.push(ShardBatch {
                shard: shard.clone(),
                records: out.array_field("Records").to_vec(),
                next: out.str_field("NextShardIterator").map(str::to_string),
            });
        }
        Batch {
            shards,
            known,
            ..Batch::default()
        }
    }
}

fn sqs_record(m: &Value, queue_arn: &str, region: &str) -> Value {
    let attributes: Map<String, Value> = m
        .obj_field("MessageAttributes")
        .into_iter()
        .flatten()
        .map(|(k, a)| {
            let mut v = json!({
                "stringListValues": [],
                "binaryListValues": [],
                "dataType": a.str_field("DataType").unwrap_or("String"),
            });
            if let Some(s) = a.get("StringValue").and_then(Value::as_str) {
                v["stringValue"] = json!(s);
            }
            if let Some(b) = a.get("BinaryValue").and_then(Value::as_str) {
                v["binaryValue"] = json!(b);
            }
            (k.clone(), v)
        })
        .collect();
    let mut r = json!({
        "messageId": m.str_field("MessageId"),
        "receiptHandle": m.str_field("ReceiptHandle"),
        "body": m.get("Body").and_then(Value::as_str).unwrap_or_default(),
        "attributes": m.get("Attributes").cloned().unwrap_or(json!({})),
        "messageAttributes": attributes,
        "md5OfBody": m.str_field("MD5OfBody"),
        "eventSource": "aws:sqs",
        "eventSourceARN": queue_arn,
        "awsRegion": region,
    });
    if let Some(md5) = m.str_field("MD5OfMessageAttributes") {
        r["md5OfMessageAttributes"] = json!(md5);
    }
    r
}

fn kinesis_record(r: &Value, shard: &str, arn: &str, region: &str, role: &str) -> Value {
    let seq = r.str_field("SequenceNumber").unwrap_or_default();
    json!({
        "kinesis": {
            "kinesisSchemaVersion": "1.0",
            "partitionKey": r.str_field("PartitionKey"),
            "sequenceNumber": seq,
            "data": r.get("Data").and_then(Value::as_str).unwrap_or_default(),
            "approximateArrivalTimestamp": r.get("ApproximateArrivalTimestamp"),
        },
        "eventSource": "aws:kinesis",
        "eventVersion": "1.0",
        "eventID": format!("{shard}:{seq}"),
        "eventName": "aws:kinesis:record",
        "invokeIdentityArn": role,
        "awsRegion": region,
        "eventSourceARN": arn,
    })
}

/// The document a filter pattern sees: the record with its JSON body or data
/// decoded.
fn filter_view(source: Source, record: &Value) -> Value {
    let mut v = record.clone();
    match source {
        Source::Sqs => {
            if let Some(body) = record
                .get("body")
                .and_then(Value::as_str)
                .and_then(|b| serde_json::from_str::<Value>(b).ok())
                .filter(Value::is_object)
            {
                v["body"] = body;
            }
        }
        Source::Kinesis => {
            if let Some(data) = record
                .get("kinesis")
                .and_then(|k| k.get("data"))
                .and_then(Value::as_str)
                .and_then(|d| STANDARD.decode(d).ok())
                .and_then(|d| serde_json::from_slice::<Value>(&d).ok())
                .filter(Value::is_object)
            {
                v["data"] = data;
            }
        }
        _ => {}
    }
    v
}

fn kept(filters: &[Value], source: Source, record: &Value) -> bool {
    filters.is_empty() || {
        let view = filter_view(source, record);
        filters.iter().any(|f| pattern::matches(f, &view))
    }
}

fn failures(outcome: &Outcome) -> Option<Vec<String>> {
    let items = outcome.json().get("batchItemFailures")?.as_array()?.clone();
    Some(
        items
            .iter()
            .filter_map(|i| i.str_field("itemIdentifier").map(str::to_string))
            .collect(),
    )
}

impl Lambda {
    pub fn polls(&mut self, tick: u64) -> Vec<Poll> {
        self.tick = tick;
        let mut out = Vec::new();
        for ((account, region), r) in &self.regions {
            for (uuid, e) in &r.esms {
                let live = r
                    .functions
                    .get(&e.function)
                    .is_some_and(|f| f.reserved != Some(0));
                if !e.enabled
                    || !live
                    || e.source == Source::Other
                    || (e.source == Source::Sqs && e.in_flight)
                {
                    continue;
                }
                let arn = e
                    .config
                    .str_field("EventSourceArn")
                    .unwrap_or_default()
                    .to_string();
                let (src_account, src_region) = Arn::parse(&arn)
                    .map_or((account.clone(), region.clone()), |a| {
                        (a.account.to_string(), a.region.to_string())
                    });
                out.push(Poll {
                    account: src_account,
                    region: src_region,
                    uuid: uuid.clone(),
                    source: e.source,
                    arn,
                    batch: e.batch_size(),
                    starting: e
                        .config
                        .str_field("StartingPosition")
                        .unwrap_or("LATEST")
                        .to_string(),
                    timestamp: e
                        .config
                        .get("StartingPositionTimestamp")
                        .and_then(Value::as_f64),
                    tick,
                    shards: e.shards.clone(),
                });
            }
        }
        out
    }

    fn owner(&self, uuid: &str) -> Option<(String, String)> {
        self.regions
            .iter()
            .find(|(_, r)| r.esms.contains_key(uuid))
            .map(|((a, r), _)| (a.clone(), r.clone()))
    }

    /// Starts an invocation for a fetched batch; returns the messages the
    /// mapping's filters dropped.
    pub fn dispatch(&mut self, ctx: &mut Ctx<'_>, poll: Poll, batch: Batch) -> Vec<SqsDelete> {
        let Some((account, region)) = self.owner(&poll.uuid) else {
            return Vec::new();
        };
        let mut fctx = Ctx::new(
            account.clone(),
            region.clone(),
            ctx.endpoint.clone(),
            ctx.env,
        );
        let mut deletes = Vec::new();
        let Some(e) = self
            .regions
            .get_mut(&(account.clone(), region.clone()))
            .and_then(|r| r.esms.get_mut(&poll.uuid))
            .cloned()
        else {
            return deletes;
        };
        let filters = e.filters();
        if poll.source == Source::Sqs {
            let (keep, drop): (Vec<_>, Vec<_>) = batch
                .messages
                .iter()
                .map(|m| sqs_record(m, &poll.arn, &poll.region))
                .partition(|r| kept(&filters, Source::Sqs, r));
            if !drop.is_empty() {
                deletes.push(SqsDelete {
                    queue_arn: poll.arn.clone(),
                    receipt_handles: drop
                        .iter()
                        .filter_map(|r| r.str_field("receiptHandle").map(str::to_string))
                        .collect(),
                });
            }
            if !keep.is_empty() {
                let handles = keep
                    .iter()
                    .map(|r| {
                        (
                            r.str_field("messageId").unwrap_or_default().to_string(),
                            r.str_field("receiptHandle").unwrap_or_default().to_string(),
                        )
                    })
                    .collect();
                let started = self.start(
                    &mut fctx,
                    Start {
                        function: &e.function,
                        qualifier: e.qualifier.as_deref(),
                        payload: json!({ "Records": keep }).to_string().into_bytes(),
                        kind: Kind::Esm(Box::new(InFlight::Sqs {
                            uuid: poll.uuid.clone(),
                            queue_arn: poll.arn.clone(),
                            handles,
                        })),
                        request_id: None,
                        client_context: None,
                    },
                );
                if started.is_ok() {
                    self.esm_mut(&account, &region, &poll.uuid).in_flight = true;
                }
            }
            ctx.outbox.append(&mut fctx.outbox);
            return deletes;
        }
        let role = self
            .regions
            .get(&(account.clone(), region.clone()))
            .and_then(|r| r.functions.get(&e.function))
            .and_then(|f| f.config.str_field("Role"))
            .unwrap_or_default()
            .to_string();
        {
            let esm = self.esm_mut(&account, &region, &poll.uuid);
            for shard in &batch.known {
                esm.shards.entry(shard.clone()).or_default();
            }
        }
        for sb in batch.shards {
            let records: Vec<Value> = sb
                .records
                .iter()
                .map(|r| match poll.source {
                    Source::Kinesis => kinesis_record(r, &sb.shard, &poll.arn, &poll.region, &role),
                    _ => {
                        let mut v = r.clone();
                        v["eventSourceARN"] = json!(poll.arn);
                        v
                    }
                })
                .filter(|r| kept(&filters, poll.source, r))
                .collect();
            if records.is_empty() {
                let cursor = self
                    .esm_mut(&account, &region, &poll.uuid)
                    .shards
                    .entry(sb.shard)
                    .or_default();
                cursor.restart_at = None;
                cursor.finished = sb.next.is_none();
                cursor.iterator = sb.next;
                continue;
            }
            let sequences: Vec<String> = records.iter().map(|r| sequence(poll.source, r)).collect();
            let arrival = |r: &Value| match poll.source {
                Source::Kinesis => r["kinesis"]["approximateArrivalTimestamp"].clone(),
                _ => r["dynamodb"]["ApproximateCreationDateTime"].clone(),
            };
            let arrivals = (arrival(&records[0]), arrival(&records[records.len() - 1]));
            let started = self.start(
                &mut fctx,
                Start {
                    function: &e.function,
                    qualifier: e.qualifier.as_deref(),
                    payload: json!({ "Records": records }).to_string().into_bytes(),
                    kind: Kind::Esm(Box::new(InFlight::Stream {
                        uuid: poll.uuid.clone(),
                        shard: sb.shard.clone(),
                        next: sb.next,
                        sequences,
                        arrivals,
                    })),
                    request_id: None,
                    client_context: None,
                },
            );
            if started.is_ok() {
                self.esm_mut(&account, &region, &poll.uuid)
                    .shards
                    .entry(sb.shard)
                    .or_default()
                    .in_flight = true;
            }
        }
        ctx.outbox.append(&mut fctx.outbox);
        deletes
    }

    fn esm_mut(&mut self, account: &str, region: &str, uuid: &str) -> &mut Esm {
        self.regions
            .get_mut(&(account.to_string(), region.to_string()))
            .and_then(|r| r.esms.get_mut(uuid))
            .expect("event source mapping exists")
    }

    pub(super) fn settle_esm(
        &mut self,
        ctx: &mut Ctx<'_>,
        inflight: InFlight,
        p: &Pending,
        outcome: &Outcome,
    ) -> Vec<SqsDelete> {
        let tick = self.tick;
        let uuid = match &inflight {
            InFlight::Sqs { uuid, .. } | InFlight::Stream { uuid, .. } => uuid.clone(),
        };
        let Some(e) = self
            .regions
            .get_mut(&(p.account.clone(), p.region.clone()))
            .and_then(|r| r.esms.get_mut(&uuid))
        else {
            return Vec::new();
        };
        let reported = if e.reports_failures() && !outcome.failed() {
            failures(outcome)
        } else {
            None
        };
        let result = if outcome.failed() {
            "PROBLEM: Function call failed"
        } else {
            "OK"
        };
        e.config
            .insert("LastProcessingResult".into(), json!(result));
        match inflight {
            InFlight::Sqs {
                queue_arn, handles, ..
            } => {
                e.in_flight = false;
                if outcome.failed() {
                    return Vec::new();
                }
                let failed: BTreeSet<String> = reported.unwrap_or_default().into_iter().collect();
                vec![SqsDelete {
                    queue_arn,
                    receipt_handles: handles
                        .into_iter()
                        .filter(|(id, _)| !failed.contains(id))
                        .map(|(_, h)| h)
                        .collect(),
                }]
            }
            InFlight::Stream {
                shard,
                next,
                sequences,
                arrivals,
                ..
            } => {
                let max_retries = e
                    .config
                    .get("MaximumRetryAttempts")
                    .and_then(Value::as_i64)
                    .unwrap_or(-1);
                let destination = e
                    .config
                    .get("DestinationConfig")
                    .and_then(|d| d.get("OnFailure"))
                    .and_then(|d| d.get("Destination"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let source = e.source;
                let stream_arn = e
                    .config
                    .str_field("EventSourceArn")
                    .unwrap_or_default()
                    .to_string();
                let cursor = e.shards.entry(shard.clone()).or_default();
                cursor.in_flight = false;
                let partial =
                    reported.and_then(|f| sequences.iter().find(|s| f.contains(s)).cloned());
                if !outcome.failed() && partial.is_none() {
                    cursor.finished = next.is_none();
                    cursor.iterator = next;
                    cursor.restart_at = None;
                    cursor.retries = 0;
                    return Vec::new();
                }
                cursor.retries += 1;
                if max_retries < 0 || cursor.retries <= max_retries {
                    cursor.blocked = tick;
                    if let Some(seq) = partial {
                        cursor.restart_at = Some(seq);
                    } else {
                        cursor.restart_at = sequences.first().cloned();
                    }
                    return Vec::new();
                }
                let count = cursor.retries as u32;
                cursor.finished = next.is_none();
                cursor.iterator = next;
                cursor.restart_at = None;
                cursor.retries = 0;
                if let Some(target) = destination {
                    let key = if source == Source::Kinesis {
                        "KinesisBatchInfo"
                    } else {
                        "DDBStreamBatchInfo"
                    };
                    let mut record = json!({
                        "requestContext": {
                            "requestId": p.request_id,
                            "functionArn": p.invoked_arn,
                            "condition": "RetryAttemptsExhausted",
                            "approximateInvokeCount": count,
                        },
                        "responseContext": {
                            "statusCode": 200,
                            "executedVersion": p.version,
                            "functionError": "Unhandled",
                        },
                        "version": "1.0",
                        "timestamp": format_iso8601(ctx.now_ms() / 1000.0),
                    });
                    record[key] = json!({
                        "shardId": shard,
                        "startSequenceNumber": sequences.first(),
                        "endSequenceNumber": sequences.last(),
                        "approximateArrivalOfFirstRecord": arrivals.0,
                        "approximateArrivalOfLastRecord": arrivals.1,
                        "batchSize": sequences.len(),
                        "streamArn": stream_arn,
                    });
                    send_record(
                        ctx,
                        p,
                        &target,
                        record,
                        "Lambda Function Invocation Result - Failure",
                    );
                }
                Vec::new()
            }
        }
    }
}

fn sequence(source: Source, r: &Value) -> String {
    let v = match source {
        Source::Kinesis => &r["kinesis"]["sequenceNumber"],
        _ => &r["dynamodb"]["SequenceNumber"],
    };
    v.as_str().unwrap_or_default().to_string()
}

const MEMBERS: &[&str] = &[
    "BatchSize",
    "MaximumBatchingWindowInSeconds",
    "ParallelizationFactor",
    "FilterCriteria",
    "DestinationConfig",
    "MaximumRecordAgeInSeconds",
    "BisectBatchOnFunctionError",
    "MaximumRetryAttempts",
    "TumblingWindowInSeconds",
    "FunctionResponseTypes",
    "ScalingConfig",
    "KMSKeyArn",
    "MetricsConfig",
    "LoggingConfig",
    "ProvisionedPollerConfig",
    "Topics",
    "Queues",
    "SourceAccessConfigurations",
    "SelfManagedEventSource",
    "AmazonManagedKafkaEventSourceConfig",
    "SelfManagedKafkaEventSourceConfig",
    "DocumentDBEventSourceConfig",
];

fn bind_function(
    ctx: &Ctx<'_>,
    region: &Region,
    raw: &str,
) -> Result<(String, Option<String>, String), AwsError> {
    let (name, qualifier) = split_name(raw);
    let f = region
        .functions
        .get(&name)
        .ok_or_else(|| no_function(ctx, &name, qualifier.as_deref()))?;
    super::versions::resolve(ctx, f, qualifier.as_deref())?;
    let arn = match &qualifier {
        Some(q) => format!("{}:{q}", function_arn(ctx, &name)),
        None => function_arn(ctx, &name),
    };
    Ok((name, qualifier, arn))
}

fn create(ctx: &mut Ctx<'_>, region: &mut Region, input: &Value) -> Result<Value, AwsError> {
    let (function, qualifier, function_arn) =
        bind_function(ctx, region, input.require_str("FunctionName")?)?;
    let source_arn = input
        .str_field("EventSourceArn")
        .unwrap_or_default()
        .to_string();
    let source = Source::of(&source_arn);
    if source == Source::Sqs
        && let Some(existing) = region.esms.values().find(|e| {
            e.config.str_field("EventSourceArn") == Some(&source_arn)
                && e.config.str_field("FunctionArn") == Some(&function_arn)
        })
    {
        return Err(conflict(format!(
            "An event source mapping with SQS arn (\" {source_arn} \") and function (\" {function} \") already exists. Please update or delete the existing mapping with UUID {}",
            existing.config.str_field("UUID").unwrap_or_default()
        )));
    }
    if source.stream() && input.str_field("StartingPosition").is_none() {
        return Err(invalid(
            "1 validation error detected: Value null at 'startingPosition' failed to satisfy constraint: Member must not be null.",
        ));
    }
    let uuid = ctx.uuid();
    let mut config = Map::new();
    config.insert("UUID".into(), json!(uuid));
    config.insert("EventSourceArn".into(), json!(source_arn));
    config.insert("FunctionArn".into(), json!(function_arn));
    config.insert(
        "BatchSize".into(),
        json!(if source == Source::Sqs { 10 } else { 100 }),
    );
    config.insert("MaximumBatchingWindowInSeconds".into(), json!(0));
    config.insert(
        "LastModified".into(),
        json!((ctx.now_ms() / 1000.0).floor()),
    );
    config.insert("StateTransitionReason".into(), json!("USER_INITIATED"));
    config.insert(
        "EventSourceMappingArn".into(),
        json!(ctx.arn("lambda", &format!("event-source-mapping:{uuid}"))),
    );
    if source.stream() {
        config.insert(
            "StartingPosition".into(),
            json!(input.str_field("StartingPosition")),
        );
        if let Some(t) = input.get("StartingPositionTimestamp") {
            config.insert("StartingPositionTimestamp".into(), t.clone());
        }
        config.insert("ParallelizationFactor".into(), json!(1));
        config.insert("MaximumRecordAgeInSeconds".into(), json!(-1));
        config.insert("BisectBatchOnFunctionError".into(), json!(false));
        config.insert("MaximumRetryAttempts".into(), json!(-1));
        config.insert("TumblingWindowInSeconds".into(), json!(0));
        config.insert("LastProcessingResult".into(), json!("No records processed"));
        config.insert("DestinationConfig".into(), json!({ "OnFailure": {} }));
    }
    config.insert("FunctionResponseTypes".into(), json!([]));
    for k in MEMBERS {
        if let Some(v) = input.get(*k).filter(|v| !v.is_null()) {
            config.insert(k.to_string(), v.clone());
        }
    }
    let tags = super::string_map(input.obj_field("Tags"));
    if !tags.is_empty() {
        region.tags.insert(
            config
                .str_field("EventSourceMappingArn")
                .unwrap_or_default()
                .to_string(),
            tags,
        );
    }
    let e = Esm {
        config,
        function,
        qualifier,
        source,
        enabled: input.get("Enabled").and_then(Value::as_bool) != Some(false),
        in_flight: false,
        shards: BTreeMap::new(),
    };
    let out = e.view("Creating");
    region.esms.insert(uuid, e);
    Ok(out)
}

fn missing(uuid: &str) -> AwsError {
    not_found(format!(
        "The resource you requested does not exist. (Service: Lambda, Status Code: 404, Request ID: {uuid})"
    ))
}

pub(super) fn call(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    Ok(Some(match operation {
        "CreateEventSourceMapping" => create(ctx, region, input)?,
        "ListEventSourceMappings" => {
            let source = input.str_field("EventSourceArn");
            let function = input.str_field("FunctionName").map(|f| split_name(f).0);
            let items: Vec<Value> = region
                .esms
                .values()
                .filter(|e| source.is_none_or(|s| e.config.str_field("EventSourceArn") == Some(s)))
                .filter(|e| function.as_ref().is_none_or(|f| &e.function == f))
                .map(|e| e.view(e.state()))
                .collect();
            let (items, next) = page(items, input, "MaxItems");
            let mut out = json!({ "EventSourceMappings": items });
            if let Some(n) = next {
                out["NextMarker"] = json!(n);
            }
            out
        }
        "GetEventSourceMapping" => {
            let uuid = input.require_str("UUID")?;
            let e = region.esms.get(uuid).ok_or_else(|| missing(uuid))?;
            e.view(e.state())
        }
        "DeleteEventSourceMapping" => {
            let uuid = input.require_str("UUID")?;
            let e = region.esms.remove(uuid).ok_or_else(|| missing(uuid))?;
            e.view("Deleting")
        }
        "UpdateEventSourceMapping" => {
            let uuid = input.require_str("UUID")?.to_string();
            if !region.esms.contains_key(&uuid) {
                return Err(missing(&uuid));
            }
            let rebind = match input.str_field("FunctionName") {
                Some(raw) => Some(bind_function(ctx, region, raw)?),
                None => None,
            };
            let e = region.esms.get_mut(&uuid).expect("checked above");
            if let Some((name, qualifier, arn)) = rebind {
                e.function = name;
                e.qualifier = qualifier;
                e.config.insert("FunctionArn".into(), json!(arn));
            }
            for k in MEMBERS {
                if let Some(v) = input.get(*k).filter(|v| !v.is_null()) {
                    e.config.insert(k.to_string(), v.clone());
                }
            }
            if let Some(enabled) = input.get("Enabled").and_then(Value::as_bool) {
                e.enabled = enabled;
            }
            e.config.insert(
                "LastModified".into(),
                json!((ctx.now_ms() / 1000.0).floor()),
            );
            e.view("Updating")
        }
        _ => return Ok(None),
    }))
}
