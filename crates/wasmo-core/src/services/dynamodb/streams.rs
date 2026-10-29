use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::attr::{Item, item_size, item_to_value};
use super::{KeySchema, Pk, Region, Regions, table_arn};
use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Ctx, InputExt, OpResult, Service, ServiceModel};

const WRITES: &[&str] = &[
    "PutItem",
    "UpdateItem",
    "DeleteItem",
    "BatchWriteItem",
    "TransactWriteItems",
    "ExecuteStatement",
    "BatchExecuteStatement",
    "ExecuteTransaction",
];

#[derive(Debug, Clone)]
pub struct Stream {
    pub arn: String,
    pub table: String,
    pub label: String,
    pub view: String,
    pub key: KeySchema,
    pub created: f64,
    pub enabled: bool,
    pub shard_id: String,
    pub records: Vec<(u64, Value)>,
}

pub type Snapshot = Vec<(String, BTreeMap<Pk, Item>)>;

/// Copies the items of the tables whose stream would capture `operation`.
pub fn snapshot(region: &Region, operation: &str, ttl: bool) -> Snapshot {
    if !ttl && !WRITES.contains(&operation) {
        return Vec::new();
    }
    region
        .tables
        .values()
        .filter(|t| t.stream.as_ref().is_some_and(|(enabled, _, _)| *enabled))
        .filter(|t| !ttl || t.ttl.is_some())
        .map(|t| (t.name.clone(), t.items.clone()))
        .collect()
}

/// Creates the stream of each table with an enabled specification and
/// disables the streams whose table no longer feeds them.
pub fn sync(region: &mut Region, ctx: &Ctx<'_>) {
    let mut live = Vec::new();
    for t in region.tables.values() {
        let Some((true, view, label)) = &t.stream else {
            continue;
        };
        let arn = format!("{}/stream/{label}", table_arn(ctx, &t.name));
        live.push(arn.clone());
        region.streams.entry(arn.clone()).or_insert_with(|| {
            let created = ctx.now_ms();
            Stream {
                arn,
                table: t.name.clone(),
                label: label.clone(),
                view: view.clone(),
                key: t.key.clone(),
                created,
                enabled: true,
                shard_id: format!("shardId-{:020}-{}", created as u64, &t.id[..8]),
                records: Vec::new(),
            }
        });
    }
    for s in region.streams.values_mut() {
        if !live.contains(&s.arn) {
            s.enabled = false;
        }
    }
}

/// Appends INSERT / MODIFY / REMOVE records for the item changes since `before`.
pub fn record(region: &mut Region, ctx: &mut Ctx<'_>, before: Snapshot, by_ttl: bool) {
    for (name, old_items) in before {
        let Some(t) = region.tables.get(&name) else {
            continue;
        };
        let Some((true, _, label)) = &t.stream else {
            continue;
        };
        let arn = format!("{}/stream/{label}", table_arn(ctx, &name));
        let mut keys: Vec<&Pk> = old_items.keys().chain(t.items.keys()).collect();
        keys.sort();
        keys.dedup();
        let mut changes = Vec::new();
        for k in keys {
            let (old, new) = (old_items.get(k), t.items.get(k));
            let event = match (old, new) {
                (None, Some(_)) => "INSERT",
                (Some(_), None) => "REMOVE",
                (Some(a), Some(b)) if a != b => "MODIFY",
                _ => continue,
            };
            changes.push((event, old.cloned(), new.cloned()));
        }
        let key = t.key.clone();
        let Some(stream) = region.streams.get_mut(&arn) else {
            continue;
        };
        for (event, old, new) in changes {
            region.stream_seq += 1;
            let seq = region.stream_seq;
            let any = new.as_ref().or(old.as_ref()).expect("changed item");
            let keys: Item = key
                .names()
                .filter_map(|n| Some((n.clone(), any.get(n)?.clone())))
                .collect();
            let mut size = item_size(&keys);
            let mut body = json!({
                "ApproximateCreationDateTime": (ctx.now_ms() / 1000.0).floor(),
                "Keys": item_to_value(&keys),
                "SequenceNumber": format!("{seq:021}"),
                "StreamViewType": stream.view,
            });
            let view = stream.view.as_str();
            if let Some(n) = new.as_ref().filter(|_| view.contains("NEW")) {
                size += item_size(n);
                body["NewImage"] = item_to_value(n);
            }
            if let Some(o) = old.as_ref().filter(|_| view.contains("OLD")) {
                size += item_size(o);
                body["OldImage"] = item_to_value(o);
            }
            body["SizeBytes"] = json!(size);
            let mut rec = json!({
                "eventID": ctx.uuid().replace('-', ""),
                "eventName": event,
                "eventVersion": "1.1",
                "eventSource": "aws:dynamodb",
                "awsRegion": ctx.region,
                "dynamodb": body,
            });
            if by_ttl {
                rec["userIdentity"] =
                    json!({ "Type": "Service", "PrincipalId": "dynamodb.amazonaws.com" });
            }
            stream.records.push((seq, rec));
        }
    }
}

fn not_found(what: &str) -> AwsError {
    AwsError::new(
        400,
        "ResourceNotFoundException",
        format!("Requested resource not found: {what} not found"),
    )
}

fn validation(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "ValidationException", message)
}

fn description(s: &Stream) -> Value {
    let mut range = json!({
        "StartingSequenceNumber": format!("{:021}", s.records.first().map_or(1, |(n, _)| *n)),
    });
    if !s.enabled {
        range["EndingSequenceNumber"] =
            json!(format!("{:021}", s.records.last().map_or(1, |(n, _)| *n)));
    }
    json!({
        "StreamArn": s.arn,
        "StreamLabel": s.label,
        "StreamStatus": if s.enabled { "ENABLED" } else { "DISABLED" },
        "StreamViewType": s.view,
        "CreationRequestDateTime": (s.created / 1000.0).floor(),
        "TableName": s.table,
        "KeySchema": s.key.to_value(),
        "Shards": [{ "ShardId": s.shard_id, "SequenceNumberRange": range }],
    })
}

fn iterator(arn: &str, shard: &str, pos: usize) -> String {
    STANDARD.encode(format!("{arn}|{shard}|{pos}"))
}

fn parse_iterator(token: &str) -> Option<(String, String, usize)> {
    let raw = String::from_utf8(STANDARD.decode(token).ok()?).ok()?;
    let mut parts = raw.rsplitn(3, '|');
    let pos = parts.next()?.parse().ok()?;
    let shard = parts.next()?.to_string();
    Some((parts.next()?.to_string(), shard, pos))
}

fn sequence(input: &Value) -> Result<u64, AwsError> {
    input
        .str_field("SequenceNumber")
        .ok_or_else(|| validation("SequenceNumber is required for this iterator type"))?
        .parse()
        .map_err(|_| validation("Invalid SequenceNumber"))
}

fn call(region: &mut Region, operation: &str, input: &Value) -> OpResult {
    match operation {
        "ListStreams" => {
            let table = input.str_field("TableName");
            let start = input.str_field("ExclusiveStartStreamArn");
            let limit = input.i64_field("Limit").unwrap_or(100).max(1) as usize;
            let mut matching: Vec<&Stream> = region
                .streams
                .values()
                .filter(|s| table.is_none_or(|t| s.table == t))
                .filter(|s| start.is_none_or(|a| s.arn.as_str() > a))
                .collect();
            let more = matching.len() > limit;
            matching.truncate(limit);
            let mut out = json!({
                "Streams": matching
                    .iter()
                    .map(|s| json!({ "StreamArn": s.arn, "TableName": s.table, "StreamLabel": s.label }))
                    .collect::<Vec<_>>(),
            });
            if more && let Some(last) = matching.last() {
                out["LastEvaluatedStreamArn"] = json!(last.arn);
            }
            Ok(out)
        }
        "DescribeStream" => {
            let arn = input.require_str("StreamArn")?;
            let s = region
                .streams
                .get(arn)
                .ok_or_else(|| not_found(&format!("Stream: {arn}")))?;
            Ok(json!({ "StreamDescription": description(s) }))
        }
        "GetShardIterator" => {
            let arn = input.require_str("StreamArn")?;
            let shard = input.require_str("ShardId")?;
            let s = region
                .streams
                .get(arn)
                .ok_or_else(|| not_found(&format!("Stream: {arn}")))?;
            if s.shard_id != shard {
                return Err(not_found(&format!("Shard: {shard} in Stream: {arn}")));
            }
            let pos = match input.require_str("ShardIteratorType")? {
                "TRIM_HORIZON" => 0,
                "LATEST" => s.records.len(),
                "AT_SEQUENCE_NUMBER" => {
                    let n = sequence(input)?;
                    s.records.partition_point(|(seq, _)| *seq < n)
                }
                "AFTER_SEQUENCE_NUMBER" => {
                    let n = sequence(input)?;
                    s.records.partition_point(|(seq, _)| *seq <= n)
                }
                other => {
                    return Err(validation(format!(
                        "1 validation error detected: Value '{other}' at 'shardIteratorType' failed to satisfy constraint: Member must satisfy enum value set: [TRIM_HORIZON, LATEST, AT_SEQUENCE_NUMBER, AFTER_SEQUENCE_NUMBER]"
                    )));
                }
            };
            Ok(json!({ "ShardIterator": iterator(arn, shard, pos) }))
        }
        "GetRecords" => {
            let token = input.require_str("ShardIterator")?;
            let (arn, shard, pos) =
                parse_iterator(token).ok_or_else(|| validation("Invalid ShardIterator"))?;
            let s = region
                .streams
                .get(&arn)
                .ok_or_else(|| not_found(&format!("Stream: {arn}")))?;
            let limit = input.i64_field("Limit").unwrap_or(1000).clamp(1, 1000) as usize;
            let pos = pos.min(s.records.len());
            let end = (pos + limit).min(s.records.len());
            let records: Vec<Value> = s.records[pos..end].iter().map(|(_, r)| r.clone()).collect();
            let mut out = Map::new();
            out.insert("Records".to_string(), Value::Array(records));
            if s.enabled || end < s.records.len() {
                out.insert(
                    "NextShardIterator".to_string(),
                    json!(iterator(&arn, &shard, end)),
                );
            }
            Ok(Value::Object(out))
        }
        other => Err(AwsError::new(
            400,
            "UnknownOperationException",
            format!("wasmo: {other} is not implemented"),
        )),
    }
}

/// DynamoDB Streams: reads the change records DynamoDB keeps for each table
/// with an enabled stream.
pub struct DynamoDbStreams {
    regions: Regions,
}

impl DynamoDbStreams {
    pub fn new(regions: Regions) -> Self {
        Self { regions }
    }
}

impl Service for DynamoDbStreams {
    fn signing_name(&self) -> &'static str {
        "dynamodb"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("dynamodbstreams")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let mut regions = self.regions.borrow_mut();
        let region = regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default();
        call(region, operation, &input)
    }
}
