use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use md5::{Digest, Md5};
use serde_json::{Value, json};

use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Ctx, Delivery, InputExt, OpResult, Service, ServiceModel};

const MAX_RETENTION_HOURS: i64 = 8760;
const ON_DEMAND_SHARDS: usize = 4;

#[derive(Debug, Clone)]
struct Record {
    seq: u128,
    shard: String,
    data: String,
    partition_key: String,
    arrived: f64,
}

#[derive(Debug, Clone)]
struct Shard {
    id: String,
    parent: Option<String>,
    adjacent_parent: Option<String>,
    start_hash: u128,
    end_hash: u128,
    start_seq: u128,
    end_seq: Option<u128>,
}

impl Shard {
    fn open(&self) -> bool {
        self.end_seq.is_none()
    }

    fn to_value(&self) -> Value {
        let mut range = json!({ "StartingSequenceNumber": seq_string(self.start_seq) });
        if let Some(end) = self.end_seq {
            range["EndingSequenceNumber"] = json!(seq_string(end));
        }
        let mut v = json!({
            "ShardId": self.id,
            "HashKeyRange": {
                "StartingHashKey": self.start_hash.to_string(),
                "EndingHashKey": self.end_hash.to_string(),
            },
            "SequenceNumberRange": range,
        });
        if let Some(p) = &self.parent {
            v["ParentShardId"] = json!(p);
        }
        if let Some(p) = &self.adjacent_parent {
            v["AdjacentParentShardId"] = json!(p);
        }
        v
    }
}

#[derive(Debug, Clone)]
struct Consumer {
    name: String,
    arn: String,
    created: f64,
}

#[derive(Debug, Clone)]
struct Stream {
    name: String,
    arn: String,
    created: f64,
    retention: i64,
    next_seq: u128,
    next_shard: usize,
    shards: Vec<Shard>,
    records: Vec<Record>,
    tags: BTreeMap<String, String>,
    metrics: Vec<String>,
    encryption: String,
    key_id: Option<String>,
    mode: String,
    max_record_kib: i64,
    warm_mibps: Option<i64>,
    consumers: Vec<Consumer>,
}

impl Stream {
    fn split_evenly(&mut self, count: usize) {
        let per = u128::MAX / count as u128;
        for i in 0..count {
            let start = per * i as u128;
            let end = if i + 1 == count {
                u128::MAX
            } else {
                per * (i as u128 + 1) - 1
            };
            self.push_shard(start, end, None, None);
        }
    }

    fn push_shard(&mut self, start: u128, end: u128, parent: Option<String>, adj: Option<String>) {
        self.shards.push(Shard {
            id: shard_id(self.next_shard),
            parent,
            adjacent_parent: adj,
            start_hash: start,
            end_hash: end,
            start_seq: self.next_seq,
            end_seq: None,
        });
        self.next_shard += 1;
    }

    fn close(&mut self, id: &str) {
        let seq = self.next_seq;
        if let Some(s) = self.shards.iter_mut().find(|s| s.id == id) {
            s.end_seq = Some(seq);
        }
    }

    fn open_shard(&self, id: &str) -> Option<&Shard> {
        self.shards.iter().find(|s| s.id == id && s.open())
    }

    fn put(&mut self, data: String, partition_key: String, hash: u128, now: f64) -> (String, u128) {
        let shard = self
            .shards
            .iter()
            .filter(|s| s.open())
            .find(|s| s.start_hash <= hash && hash <= s.end_hash)
            .or_else(|| self.shards.iter().find(|s| s.open()))
            .map(|s| s.id.clone())
            .unwrap_or_default();
        let seq = self.next_seq;
        self.next_seq += 1;
        self.records.push(Record {
            seq,
            shard: shard.clone(),
            data,
            partition_key,
            arrived: now,
        });
        (shard, seq)
    }

    fn open_count(&self) -> usize {
        self.shards.iter().filter(|s| s.open()).count()
    }
}

#[derive(Debug, Default)]
struct Region {
    streams: BTreeMap<String, Stream>,
    policies: BTreeMap<String, String>,
    billing_commitment: Option<Value>,
}

#[derive(Default)]
pub struct Kinesis {
    regions: HashMap<(String, String), Region>,
}

fn shard_id(i: usize) -> String {
    format!("shardId-{i:012}")
}

fn seq_string(seq: u128) -> String {
    format!("{:056}", seq + 1)
}

fn parse_seq(s: &str) -> Option<u128> {
    s.parse::<u128>().ok()?.checked_sub(1)
}

/// The 128-bit hash key Kinesis derives from a partition key: the MD5 digest
/// read as a big-endian integer.
pub fn hash_key(partition_key: &str) -> u128 {
    u128::from_be_bytes(Md5::digest(partition_key.as_bytes()).into())
}

fn invalid(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "InvalidArgumentException", message)
}

fn not_found(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "ResourceNotFoundException", message)
}

fn in_use(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "ResourceInUseException", message)
}

fn required<'a>(input: &'a Value, key: &str) -> Result<&'a str, AwsError> {
    input
        .str_field(key)
        .ok_or_else(|| invalid(format!("{key} is required.")))
}

fn required_i64(input: &Value, key: &str) -> Result<i64, AwsError> {
    input
        .i64_field(key)
        .ok_or_else(|| invalid(format!("{key} is required.")))
}

fn name_of_arn(arn: &str) -> &str {
    arn.split("/consumer/")
        .next()
        .unwrap_or(arn)
        .rsplit('/')
        .next()
        .unwrap_or_default()
}

fn stream_name(input: &Value) -> Result<String, AwsError> {
    if let Some(n) = input.str_field("StreamName") {
        return Ok(n.to_string());
    }
    input
        .str_field("StreamARN")
        .map(|a| name_of_arn(a).to_string())
        .ok_or_else(|| invalid("Either StreamName or StreamARN is required."))
}

fn tags_of(input: &Value) -> BTreeMap<String, String> {
    input
        .obj_field("Tags")
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
        .collect()
}

fn tag_list(tags: &BTreeMap<String, String>) -> Vec<Value> {
    tags.iter()
        .map(|(k, v)| json!({ "Key": k, "Value": v }))
        .collect()
}

fn iterator(name: &str, shard: &str, pos: u128) -> String {
    STANDARD.encode(format!("{name}|{shard}|{pos}"))
}

fn parse_iterator(token: &str) -> Option<(String, String, u128)> {
    let raw = String::from_utf8(STANDARD.decode(token).ok()?).ok()?;
    let mut parts = raw.split('|');
    Some((
        parts.next()?.to_string(),
        parts.next()?.to_string(),
        parts.next()?.parse().ok()?,
    ))
}

fn hash_for(entry: &Value, partition_key: &str) -> Result<u128, AwsError> {
    match entry.str_field("ExplicitHashKey") {
        Some(h) => h
            .parse()
            .map_err(|_| invalid(format!("Invalid ExplicitHashKey. ExplicitHashKey must be in the range: [0, 2^128-1]. Specified value was {h}"))),
        None => Ok(hash_key(partition_key)),
    }
}

impl Region {
    fn stream(&self, ctx: &Ctx<'_>, name: &str) -> Result<&Stream, AwsError> {
        self.streams.get(name).ok_or_else(|| missing(ctx, name))
    }

    fn stream_mut(&mut self, ctx: &Ctx<'_>, name: &str) -> Result<&mut Stream, AwsError> {
        self.streams.get_mut(name).ok_or_else(|| missing(ctx, name))
    }
}

fn missing(ctx: &Ctx<'_>, name: &str) -> AwsError {
    not_found(format!(
        "Stream {name} under account {} not found.",
        ctx.account
    ))
}

fn consumer_target(input: &Value) -> Result<(String, String), AwsError> {
    if let Some(arn) = input.str_field("ConsumerARN") {
        let name = arn
            .split("/consumer/")
            .nth(1)
            .and_then(|r| r.split(':').next())
            .unwrap_or_default();
        return Ok((name_of_arn(arn).to_string(), name.to_string()));
    }
    match (
        input.str_field("StreamARN"),
        input.str_field("ConsumerName"),
    ) {
        (Some(s), Some(c)) => Ok((name_of_arn(s).to_string(), c.to_string())),
        _ => Err(invalid(
            "Either ConsumerARN or StreamARN and ConsumerName are required.",
        )),
    }
}

fn consumer_value(c: &Consumer, stream_arn: Option<&str>) -> Value {
    let mut v = json!({
        "ConsumerName": c.name,
        "ConsumerARN": c.arn,
        "ConsumerStatus": "ACTIVE",
        "ConsumerCreationTimestamp": c.created / 1000.0,
    });
    if let Some(a) = stream_arn {
        v["StreamARN"] = json!(a);
    }
    v
}

fn page<T: Clone>(items: &[T], input: &Value, key: &str) -> (Vec<T>, Option<String>) {
    let start = input
        .str_field("NextToken")
        .and_then(|t| STANDARD.decode(t).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let limit = input
        .i64_field(key)
        .map_or(items.len(), |l| l.max(1) as usize);
    let end = (start + limit).min(items.len());
    let next = (end < items.len()).then(|| STANDARD.encode(end.to_string()));
    (items[start..end].to_vec(), next)
}

fn metrics_change(stream: &mut Stream, input: &Value, enable: bool) -> Value {
    let current = stream.metrics.clone();
    let requested: Vec<String> = input
        .array_field("ShardLevelMetrics")
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    let all = requested.iter().any(|m| m == "ALL");
    stream.metrics = match (enable, all) {
        (true, true) => vec!["ALL".to_string()],
        (true, false) => {
            let mut m = current.clone();
            m.extend(requested.into_iter().filter(|r| !current.contains(r)));
            m
        }
        (false, true) => Vec::new(),
        (false, false) => current
            .iter()
            .filter(|m| !requested.contains(m))
            .cloned()
            .collect(),
    };
    json!({
        "StreamName": stream.name,
        "StreamARN": stream.arn,
        "CurrentShardLevelMetrics": current,
        "DesiredShardLevelMetrics": stream.metrics,
    })
}

fn call(region: &mut Region, ctx: &mut Ctx<'_>, operation: &str, input: &Value) -> OpResult {
    let now = ctx.now_ms();
    Ok(match operation {
        "CreateStream" => {
            let name = required(input, "StreamName")?.to_string();
            if region.streams.contains_key(&name) {
                return Err(in_use(format!(
                    "Stream {name} under account {} already exists.",
                    ctx.account
                )));
            }
            let mode = input
                .obj_field("StreamModeDetails")
                .and_then(|m| m.get("StreamMode"))
                .and_then(Value::as_str)
                .unwrap_or("PROVISIONED")
                .to_string();
            let count = match input.i64_field("ShardCount") {
                Some(n) if n < 1 => {
                    return Err(invalid(format!(
                        "1 validation error detected: Value '{n}' at 'shardCount' failed to satisfy constraint: Member must have value greater than or equal to 1"
                    )));
                }
                Some(n) => n as usize,
                None if mode == "ON_DEMAND" => ON_DEMAND_SHARDS,
                None => 1,
            };
            let mut stream = Stream {
                arn: ctx.arn("kinesis", &format!("stream/{name}")),
                name: name.clone(),
                created: now,
                retention: 24,
                next_seq: 0,
                next_shard: 0,
                shards: Vec::new(),
                records: Vec::new(),
                tags: tags_of(input),
                metrics: Vec::new(),
                encryption: "NONE".to_string(),
                key_id: None,
                mode,
                max_record_kib: 1024,
                warm_mibps: None,
                consumers: Vec::new(),
            };
            stream.split_evenly(count);
            region.streams.insert(name, stream);
            json!({})
        }
        "DeleteStream" => {
            let name = stream_name(input)?;
            let stream = region.stream(ctx, &name)?;
            if !stream.consumers.is_empty()
                && input
                    .get("EnforceConsumerDeletion")
                    .and_then(Value::as_bool)
                    != Some(true)
            {
                return Err(in_use(format!(
                    "Stream {name} has registered consumers. Set EnforceConsumerDeletion to true to delete."
                )));
            }
            region.streams.remove(&name);
            json!({})
        }
        "ListStreams" => {
            let after = input.str_field("ExclusiveStartStreamName");
            let names: Vec<&Stream> = region
                .streams
                .values()
                .filter(|s| after.is_none_or(|a| s.name.as_str() > a))
                .collect();
            let (items, next) = page(&names, input, "Limit");
            let mut out = json!({
                "StreamNames": items.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
                "HasMoreStreams": next.is_some(),
                "StreamSummaries": items.iter().map(|s| json!({
                    "StreamName": s.name,
                    "StreamARN": s.arn,
                    "StreamStatus": "ACTIVE",
                    "StreamModeDetails": { "StreamMode": s.mode },
                    "StreamCreationTimestamp": s.created / 1000.0,
                })).collect::<Vec<_>>(),
            });
            if let Some(n) = next {
                out["NextToken"] = json!(n);
            }
            out
        }
        "DescribeStream" => {
            let s = region.stream(ctx, &stream_name(input)?)?;
            let after = input.str_field("ExclusiveStartShardId");
            let shards: Vec<Value> = s
                .shards
                .iter()
                .filter(|sh| after.is_none_or(|a| sh.id.as_str() > a))
                .map(Shard::to_value)
                .collect();
            let limit = input.i64_field("Limit").map_or(100, |l| l.max(1) as usize);
            let more = shards.len() > limit;
            let mut d = json!({
                "StreamName": s.name,
                "StreamARN": s.arn,
                "StreamStatus": "ACTIVE",
                "StreamModeDetails": { "StreamMode": s.mode },
                "Shards": shards.into_iter().take(limit).collect::<Vec<_>>(),
                "HasMoreShards": more,
                "RetentionPeriodHours": s.retention,
                "StreamCreationTimestamp": s.created / 1000.0,
                "EnhancedMonitoring": [{ "ShardLevelMetrics": s.metrics }],
                "EncryptionType": s.encryption,
            });
            if let Some(k) = &s.key_id {
                d["KeyId"] = json!(k);
            }
            json!({ "StreamDescription": d })
        }
        "DescribeStreamSummary" => {
            let s = region.stream(ctx, &stream_name(input)?)?;
            let mut d = json!({
                "StreamName": s.name,
                "StreamARN": s.arn,
                "StreamStatus": "ACTIVE",
                "StreamModeDetails": { "StreamMode": s.mode },
                "RetentionPeriodHours": s.retention,
                "StreamCreationTimestamp": s.created / 1000.0,
                "EnhancedMonitoring": [{ "ShardLevelMetrics": s.metrics }],
                "EncryptionType": s.encryption,
                "OpenShardCount": s.open_count(),
                "ConsumerCount": s.consumers.len(),
                "MaxRecordSizeInKiB": s.max_record_kib,
            });
            if let Some(k) = &s.key_id {
                d["KeyId"] = json!(k);
            }
            if let Some(w) = s.warm_mibps {
                d["WarmThroughput"] = json!({ "TargetMiBps": w, "CurrentMiBps": w });
            }
            json!({ "StreamDescriptionSummary": d })
        }
        "PutRecord" => {
            let s = region.stream_mut(ctx, &stream_name(input)?)?;
            let pk = required(input, "PartitionKey")?.to_string();
            let hash = hash_for(input, &pk)?;
            let data = input.str_field("Data").unwrap_or_default().to_string();
            let (shard, seq) = s.put(data, pk, hash, now);
            json!({
                "ShardId": shard,
                "SequenceNumber": seq_string(seq),
                "EncryptionType": s.encryption,
            })
        }
        "PutRecords" => {
            let s = region.stream_mut(ctx, &stream_name(input)?)?;
            let entries = input.array_field("Records");
            if entries.is_empty() || entries.len() > 500 {
                return Err(invalid(format!(
                    "1 validation error detected: Value at 'records' failed to satisfy constraint: Member must have length between 1 and 500, got {}",
                    entries.len()
                )));
            }
            let mut results = Vec::new();
            for e in entries {
                let pk = required(e, "PartitionKey")?.to_string();
                let hash = hash_for(e, &pk)?;
                let data = e.str_field("Data").unwrap_or_default().to_string();
                let (shard, seq) = s.put(data, pk, hash, now);
                results.push(json!({ "ShardId": shard, "SequenceNumber": seq_string(seq) }));
            }
            json!({ "FailedRecordCount": 0, "Records": results, "EncryptionType": s.encryption })
        }
        "GetShardIterator" => {
            let s = region.stream(ctx, &stream_name(input)?)?;
            let shard_id = required(input, "ShardId")?;
            let shard = s.shards.iter().find(|x| x.id == shard_id).ok_or_else(|| {
                not_found(format!(
                    "Shard {shard_id} in stream {} under account {} does not exist",
                    s.name, ctx.account
                ))
            })?;
            let pos = match required(input, "ShardIteratorType")? {
                "TRIM_HORIZON" => shard.start_seq,
                "LATEST" => s.next_seq,
                t @ ("AT_SEQUENCE_NUMBER" | "AFTER_SEQUENCE_NUMBER") => {
                    let n = input
                        .str_field("StartingSequenceNumber")
                        .and_then(parse_seq)
                        .ok_or_else(|| invalid("StartingSequenceNumber is invalid."))?;
                    if t == "AFTER_SEQUENCE_NUMBER" {
                        n + 1
                    } else {
                        n
                    }
                }
                "AT_TIMESTAMP" => {
                    let ts = input
                        .get("Timestamp")
                        .and_then(Value::as_f64)
                        .ok_or_else(|| invalid("Timestamp is required for AT_TIMESTAMP."))?
                        * 1000.0;
                    s.records
                        .iter()
                        .find(|r| r.shard == shard_id && r.arrived >= ts)
                        .map_or(s.next_seq, |r| r.seq)
                }
                other => {
                    return Err(invalid(format!(
                        "1 validation error detected: Value '{other}' at 'shardIteratorType' failed to satisfy constraint: Member must satisfy enum value set: [AFTER_SEQUENCE_NUMBER, AT_SEQUENCE_NUMBER, AT_TIMESTAMP, TRIM_HORIZON, LATEST]"
                    )));
                }
            };
            json!({ "ShardIterator": iterator(&s.name, shard_id, pos) })
        }
        "GetRecords" => {
            let token = required(input, "ShardIterator")?;
            let (name, shard_id, pos) =
                parse_iterator(token).ok_or_else(|| invalid("Invalid ShardIterator."))?;
            let s = region.stream(ctx, &name)?;
            let limit = input.i64_field("Limit").unwrap_or(10000).clamp(1, 10000) as usize;
            let selected: Vec<&Record> = s
                .records
                .iter()
                .filter(|r| r.shard == shard_id && r.seq >= pos)
                .take(limit)
                .collect();
            let next = selected.last().map_or(pos, |r| r.seq + 1);
            let shard = s.shards.iter().find(|x| x.id == shard_id);
            let exhausted = shard.is_some_and(|x| !x.open())
                && !s
                    .records
                    .iter()
                    .any(|r| r.shard == shard_id && r.seq >= next);
            let mut out = json!({
                "Records": selected.iter().map(|r| json!({
                    "SequenceNumber": seq_string(r.seq),
                    "ApproximateArrivalTimestamp": r.arrived / 1000.0,
                    "Data": r.data,
                    "PartitionKey": r.partition_key,
                    "EncryptionType": s.encryption,
                })).collect::<Vec<_>>(),
                "MillisBehindLatest": 0,
            });
            if exhausted {
                out["ChildShards"] = Value::Array(
                    s.shards
                        .iter()
                        .filter(|c| {
                            c.parent.as_deref() == Some(shard_id.as_str())
                                || c.adjacent_parent.as_deref() == Some(shard_id.as_str())
                        })
                        .map(|c| {
                            let parents: Vec<&String> =
                                c.parent.iter().chain(&c.adjacent_parent).collect();
                            json!({
                                "ShardId": c.id,
                                "ParentShards": parents,
                                "HashKeyRange": {
                                    "StartingHashKey": c.start_hash.to_string(),
                                    "EndingHashKey": c.end_hash.to_string(),
                                },
                            })
                        })
                        .collect(),
                );
            } else {
                out["NextShardIterator"] = json!(iterator(&name, &shard_id, next));
            }
            out
        }
        "IncreaseStreamRetentionPeriod" | "DecreaseStreamRetentionPeriod" => {
            let name = stream_name(input)?;
            let s = region.stream_mut(ctx, &name)?;
            let hours = required_i64(input, "RetentionPeriodHours")?;
            let increase = operation.starts_with("Increase");
            if !(24..=MAX_RETENTION_HOURS).contains(&hours) {
                return Err(invalid(format!(
                    "Minimum allowed retention period is 24 hours. Maximum allowed retention period is {MAX_RETENTION_HOURS} hours. Requested retention period ({hours} hours) is too {}.",
                    if hours < 24 { "short" } else { "long" }
                )));
            }
            if increase && hours < s.retention {
                return Err(invalid(format!(
                    "Requested retention period ({hours} hours) for stream {name} can not be shorter than existing retention period ({} hours). Use DecreaseRetentionPeriod API.",
                    s.retention
                )));
            }
            if !increase && hours > s.retention {
                return Err(invalid(format!(
                    "Requested retention period ({hours} hours) for stream {name} can not be longer than existing retention period ({} hours). Use IncreaseRetentionPeriod API.",
                    s.retention
                )));
            }
            s.retention = hours;
            json!({})
        }
        "AddTagsToStream" | "TagResource" => {
            let name = match input.str_field("ResourceARN") {
                Some(a) => name_of_arn(a).to_string(),
                None => stream_name(input)?,
            };
            let s = region.stream_mut(ctx, &name)?;
            s.tags.extend(tags_of(input));
            json!({})
        }
        "RemoveTagsFromStream" | "UntagResource" => {
            let name = match input.str_field("ResourceARN") {
                Some(a) => name_of_arn(a).to_string(),
                None => stream_name(input)?,
            };
            let s = region.stream_mut(ctx, &name)?;
            for k in input
                .array_field("TagKeys")
                .iter()
                .filter_map(Value::as_str)
            {
                s.tags.remove(k);
            }
            json!({})
        }
        "ListTagsForStream" => {
            let s = region.stream(ctx, &stream_name(input)?)?;
            let after = input.str_field("ExclusiveStartTagKey");
            let tags: BTreeMap<String, String> = s
                .tags
                .iter()
                .filter(|(k, _)| after.is_none_or(|a| k.as_str() > a))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            let limit = input
                .i64_field("Limit")
                .map_or(tags.len(), |l| l.max(1) as usize);
            let more = tags.len() > limit;
            let shown: BTreeMap<String, String> = tags.into_iter().take(limit).collect();
            json!({ "Tags": tag_list(&shown), "HasMoreTags": more })
        }
        "ListTagsForResource" => {
            let arn = required(input, "ResourceARN")?;
            let s = region.stream(ctx, name_of_arn(arn))?;
            json!({ "Tags": tag_list(&s.tags) })
        }
        "EnableEnhancedMonitoring" | "DisableEnhancedMonitoring" => {
            let s = region.stream_mut(ctx, &stream_name(input)?)?;
            metrics_change(s, input, operation.starts_with("Enable"))
        }
        "ListShards" => {
            let s = region.stream(ctx, &stream_name(input)?)?;
            let filter = input.obj_field("ShardFilter");
            let kind = filter.and_then(|f| f.get("Type")).and_then(Value::as_str);
            let after = input.str_field("ExclusiveStartShardId").or_else(|| {
                filter
                    .and_then(|f| f.get("ShardId"))
                    .and_then(Value::as_str)
                    .filter(|_| kind == Some("AFTER_SHARD_ID"))
            });
            let shards: Vec<Value> = s
                .shards
                .iter()
                .filter(|sh| after.is_none_or(|a| sh.id.as_str() > a))
                .filter(|sh| kind != Some("AT_LATEST") || sh.open())
                .map(Shard::to_value)
                .collect();
            let (items, next) = page(&shards, input, "MaxResults");
            let mut out = json!({ "Shards": items });
            if let Some(n) = next {
                out["NextToken"] = json!(n);
            }
            out
        }
        "MergeShards" => {
            let name = stream_name(input)?;
            let s = region.stream_mut(ctx, &name)?;
            let a = required(input, "ShardToMerge")?.to_string();
            let b = required(input, "AdjacentShardToMerge")?.to_string();
            let (Some(x), Some(y)) = (s.open_shard(&a).cloned(), s.open_shard(&b).cloned()) else {
                return Err(not_found(format!("Shard not found in stream {name}.")));
            };
            if x.end_hash.checked_add(1) != Some(y.start_hash)
                && y.end_hash.checked_add(1) != Some(x.start_hash)
            {
                return Err(invalid(format!(
                    "Shards {a} and {b} in stream {name} under account {} are not an adjacent pair of shards eligible for merging",
                    ctx.account
                )));
            }
            s.close(&a);
            s.close(&b);
            s.push_shard(
                x.start_hash.min(y.start_hash),
                x.end_hash.max(y.end_hash),
                Some(a),
                Some(b),
            );
            json!({})
        }
        "SplitShard" => {
            let name = stream_name(input)?;
            let s = region.stream_mut(ctx, &name)?;
            let id = required(input, "ShardToSplit")?.to_string();
            let at: u128 = required(input, "NewStartingHashKey")?
                .parse()
                .map_err(|_| invalid("NewStartingHashKey is invalid."))?;
            let shard = s
                .open_shard(&id)
                .cloned()
                .ok_or_else(|| not_found(format!("Shard {id} not found in stream {name}.")))?;
            if at <= shard.start_hash || at > shard.end_hash {
                return Err(invalid(format!(
                    "NewStartingHashKey {at} used in SplitShard() on shard {id} in stream {name} under account {} is not both greater than one plus the shard's StartingHashKey {} and less than the shard's EndingHashKey {}.",
                    ctx.account, shard.start_hash, shard.end_hash
                )));
            }
            s.close(&id);
            s.push_shard(shard.start_hash, at - 1, Some(id.clone()), None);
            s.push_shard(at, shard.end_hash, Some(id), None);
            json!({})
        }
        "UpdateShardCount" => {
            let s = region.stream_mut(ctx, &stream_name(input)?)?;
            let target = required_i64(input, "TargetShardCount")?;
            if target < 1 {
                return Err(invalid("TargetShardCount must be at least 1."));
            }
            let current = s.open_count();
            let open: Vec<String> = s
                .shards
                .iter()
                .filter(|x| x.open())
                .map(|x| x.id.clone())
                .collect();
            open.iter().for_each(|id| s.close(id));
            s.split_evenly(target as usize);
            json!({
                "StreamName": s.name,
                "StreamARN": s.arn,
                "CurrentShardCount": current,
                "TargetShardCount": target,
            })
        }
        "RegisterStreamConsumer" => {
            let arn = required(input, "StreamARN")?;
            let consumer = required(input, "ConsumerName")?.to_string();
            let s = region.stream_mut(ctx, name_of_arn(arn))?;
            if s.consumers.iter().any(|c| c.name == consumer) {
                return Err(in_use(format!(
                    "Consumer {consumer} already exists for stream {arn}."
                )));
            }
            let c = Consumer {
                arn: format!("{}/consumer/{consumer}:{}", s.arn, (now / 1000.0) as u64),
                name: consumer,
                created: now,
            };
            let out = json!({ "Consumer": consumer_value(&c, None) });
            s.consumers.push(c);
            out
        }
        "DeregisterStreamConsumer" | "DescribeStreamConsumer" => {
            let (stream, consumer) = consumer_target(input)?;
            let s = region.stream_mut(ctx, &stream)?;
            let i = s
                .consumers
                .iter()
                .position(|c| c.name == consumer)
                .ok_or_else(|| not_found(format!("Consumer {consumer} not found.")))?;
            if operation.starts_with("Deregister") {
                s.consumers.remove(i);
                json!({})
            } else {
                json!({ "ConsumerDescription": consumer_value(&s.consumers[i], Some(&s.arn)) })
            }
        }
        "ListStreamConsumers" => {
            let arn = required(input, "StreamARN")?;
            let s = region.stream(ctx, name_of_arn(arn))?;
            let all: Vec<Value> = s
                .consumers
                .iter()
                .map(|c| consumer_value(c, None))
                .collect();
            let (items, next) = page(&all, input, "MaxResults");
            let mut out = json!({ "Consumers": items });
            if let Some(n) = next {
                out["NextToken"] = json!(n);
            }
            out
        }
        "StartStreamEncryption" => {
            let s = region.stream_mut(ctx, &stream_name(input)?)?;
            s.encryption = required(input, "EncryptionType")?.to_string();
            s.key_id = Some(required(input, "KeyId")?.to_string());
            json!({})
        }
        "StopStreamEncryption" => {
            let s = region.stream_mut(ctx, &stream_name(input)?)?;
            s.encryption = "NONE".to_string();
            s.key_id = None;
            json!({})
        }
        "UpdateStreamMode" => {
            let s = region.stream_mut(ctx, &stream_name(input)?)?;
            if let Some(m) = input
                .obj_field("StreamModeDetails")
                .and_then(|m| m.get("StreamMode"))
                .and_then(Value::as_str)
            {
                s.mode = m.to_string();
            }
            json!({})
        }
        "UpdateMaxRecordSize" => {
            let s = region.stream_mut(ctx, &stream_name(input)?)?;
            s.max_record_kib = required_i64(input, "MaxRecordSizeInKiB")?;
            json!({})
        }
        "UpdateStreamWarmThroughput" => {
            let s = region.stream_mut(ctx, &stream_name(input)?)?;
            let w = required_i64(input, "WarmThroughputMiBps")?;
            s.warm_mibps = Some(w);
            json!({
                "StreamARN": s.arn,
                "StreamName": s.name,
                "WarmThroughput": { "TargetMiBps": w, "CurrentMiBps": w },
            })
        }
        "DescribeLimits" => json!({
            "ShardLimit": 10000,
            "OpenShardCount": region.streams.values().map(Stream::open_count).sum::<usize>(),
            "OnDemandStreamCount": region.streams.values().filter(|s| s.mode == "ON_DEMAND").count(),
            "OnDemandStreamCountLimit": 50,
        }),
        "DescribeAccountSettings" => json!({
            "MinimumThroughputBillingCommitment": region
                .billing_commitment
                .clone()
                .unwrap_or_else(|| json!({ "Status": "DISABLED" })),
        }),
        "UpdateAccountSettings" => {
            let status = input
                .obj_field("MinimumThroughputBillingCommitment")
                .and_then(|c| c.get("Status"))
                .and_then(Value::as_str)
                .unwrap_or("DISABLED");
            let v = json!({ "Status": status });
            region.billing_commitment = Some(v.clone());
            json!({ "MinimumThroughputBillingCommitment": v })
        }
        "PutResourcePolicy" => {
            let arn = required(input, "ResourceARN")?.to_string();
            region.stream(ctx, name_of_arn(&arn))?;
            region
                .policies
                .insert(arn, required(input, "Policy")?.to_string());
            json!({})
        }
        "GetResourcePolicy" => {
            let arn = required(input, "ResourceARN")?;
            json!({ "Policy": region.policies.get(arn).cloned().unwrap_or_else(|| "{}".to_string()) })
        }
        "DeleteResourcePolicy" => {
            let arn = required(input, "ResourceARN")?;
            region.policies.remove(arn).ok_or_else(|| {
                not_found(format!("No resource policy found for resource ARN {arn}."))
            })?;
            json!({})
        }
        other => {
            return Err(AwsError::new(
                400,
                "UnknownOperationException",
                format!("wasmo: {other} is not implemented"),
            ));
        }
    })
}

impl Service for Kinesis {
    fn signing_name(&self) -> &'static str {
        "kinesis"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("kinesis")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let region = self
            .regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default();
        call(region, ctx, operation, &input)
    }

    fn deliver(&mut self, ctx: &mut Ctx<'_>, resource: &str, delivery: &Delivery) -> bool {
        let Some(stream) = resource.strip_prefix("stream/").and_then(|name| {
            self.regions
                .get_mut(&(ctx.account.clone(), ctx.region.clone()))?
                .streams
                .get_mut(name)
        }) else {
            return false;
        };
        let data = match &delivery.bytes {
            Some(b) => STANDARD.encode(b),
            None => STANDARD.encode(delivery.body.as_bytes()),
        };
        let pk = delivery
            .attributes
            .get("PartitionKey")
            .and_then(Value::as_str)
            .map_or_else(|| ctx.uuid(), str::to_string);
        let hash = hash_key(&pk);
        stream.put(data, pk, hash, ctx.now_ms());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_keys_hash_with_md5() {
        assert_eq!(hash_key(""), 0xd41d8cd98f00b204e9800998ecf8427e);
        assert_eq!(hash_key("abc"), 0x900150983cd24fb0d6963f7d28e17f72);
    }

    #[test]
    fn records_land_in_the_shard_owning_their_hash() {
        let mut s = Stream {
            name: "s".into(),
            arn: String::new(),
            created: 0.0,
            retention: 24,
            next_seq: 0,
            next_shard: 0,
            shards: Vec::new(),
            records: Vec::new(),
            tags: BTreeMap::new(),
            metrics: Vec::new(),
            encryption: "NONE".into(),
            key_id: None,
            mode: "PROVISIONED".into(),
            max_record_kib: 1024,
            warm_mibps: None,
            consumers: Vec::new(),
        };
        s.split_evenly(4);
        assert_eq!(s.shards[3].end_hash, u128::MAX);
        assert_eq!(s.shards[0].end_hash + 1, s.shards[1].start_hash);
        for (pk, shard) in [("a", 0), ("b", 2), ("c", 1)] {
            let h = hash_key(pk);
            let (id, _) = s.put(String::new(), pk.into(), h, 0.0);
            assert_eq!(id, shard_id(shard), "{pk} hashes to {h:032x}");
        }
        let (id, _) = s.put(String::new(), "x".into(), u128::MAX, 0.0);
        assert_eq!(id, shard_id(3));
    }
}
