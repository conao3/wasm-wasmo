use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::gzip;
use crate::codec::AwsError;
use crate::codec::time::civil_from_days;
use crate::embedded_model;
use crate::service::{Ctx, Delivery, InputExt, OpResult, Service, ServiceModel};

const DESTINATION_ID: &str = "destinationId-000000000001";

#[derive(Debug, Clone)]
struct DeliveryStream {
    name: String,
    arn: String,
    kind: String,
    version: u64,
    created: f64,
    updated: f64,
    destinations: Vec<Map<String, Value>>,
    tags: Vec<(String, Option<String>)>,
    encryption: Option<Value>,
    source: Option<Value>,
}

impl DeliveryStream {
    fn describe(&self) -> Value {
        let mut d = json!({
            "DeliveryStreamName": self.name,
            "DeliveryStreamARN": self.arn,
            "DeliveryStreamStatus": "ACTIVE",
            "DeliveryStreamType": self.kind,
            "VersionId": self.version.to_string(),
            "CreateTimestamp": self.created / 1000.0,
            "LastUpdateTimestamp": self.updated / 1000.0,
            "Destinations": self.destinations,
            "HasMoreDestinations": false,
        });
        if let Some(e) = &self.encryption {
            d["DeliveryStreamEncryptionConfiguration"] = e.clone();
        }
        if let Some(s) = &self.source {
            d["Source"] = s.clone();
        }
        d
    }

    fn s3_destination(&self) -> Option<&Map<String, Value>> {
        self.destinations.iter().find_map(|d| {
            d.get("ExtendedS3DestinationDescription")
                .or_else(|| d.get("S3DestinationDescription"))
                .and_then(Value::as_object)
        })
    }
}

#[derive(Default)]
pub struct Firehose {
    regions: HashMap<(String, String), BTreeMap<String, DeliveryStream>>,
}

fn invalid(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "InvalidArgumentException", message)
}

fn required<'a>(input: &'a Value, key: &str) -> Result<&'a str, AwsError> {
    input
        .str_field(key)
        .ok_or_else(|| invalid(format!("{key} is required.")))
}

fn stream_mut<'a>(
    streams: &'a mut BTreeMap<String, DeliveryStream>,
    ctx: &Ctx<'_>,
    name: &str,
) -> Result<&'a mut DeliveryStream, AwsError> {
    streams.get_mut(name).ok_or_else(|| {
        AwsError::new(
            400,
            "ResourceNotFoundException",
            format!("Firehose {name} under account {} not found.", ctx.account),
        )
    })
}

fn with_defaults(key: &str, mut config: Map<String, Value>) -> Map<String, Value> {
    if key.contains("S3") {
        config
            .entry("BufferingHints")
            .or_insert_with(|| json!({ "SizeInMBs": 5, "IntervalInSeconds": 300 }));
        config
            .entry("CompressionFormat")
            .or_insert_with(|| json!("UNCOMPRESSED"));
        config
            .entry("EncryptionConfiguration")
            .or_insert_with(|| json!({ "NoEncryptionConfig": "NoEncryption" }));
    }
    config
}

fn description_key(config_key: &str, suffix: &str) -> Option<String> {
    config_key
        .strip_suffix(suffix)
        .map(|base| format!("{base}Description"))
}

fn destinations(input: &Value) -> Vec<Map<String, Value>> {
    let Some(obj) = input.as_object() else {
        return Vec::new();
    };
    let mut d = Map::new();
    for (k, v) in obj {
        if let (Some(desc), Some(cfg)) = (description_key(k, "Configuration"), v.as_object())
            && k.ends_with("DestinationConfiguration")
        {
            let cfg = with_defaults(k, cfg.clone());
            if k == "ExtendedS3DestinationConfiguration" {
                let mut plain = cfg.clone();
                plain.retain(|f, _| {
                    [
                        "RoleARN",
                        "BucketARN",
                        "Prefix",
                        "ErrorOutputPrefix",
                        "BufferingHints",
                        "CompressionFormat",
                        "EncryptionConfiguration",
                        "CloudWatchLoggingOptions",
                    ]
                    .contains(&f.as_str())
                });
                d.insert("S3DestinationDescription".to_string(), Value::Object(plain));
            }
            d.insert(desc, Value::Object(cfg));
        }
    }
    if d.is_empty() {
        return Vec::new();
    }
    d.insert("DestinationId".to_string(), json!(DESTINATION_ID));
    vec![d]
}

fn source(input: &Value, now: f64) -> Option<Value> {
    if let Some(k) = input.obj_field("KinesisStreamSourceConfiguration") {
        return Some(json!({ "KinesisStreamSourceDescription": {
            "KinesisStreamARN": k.get("KinesisStreamARN"),
            "RoleARN": k.get("RoleARN"),
            "DeliveryStartTimestamp": now / 1000.0,
        }}));
    }
    if let Some(m) = input.obj_field("MSKSourceConfiguration") {
        let mut d = m.clone();
        d.insert("DeliveryStartTimestamp".to_string(), json!(now / 1000.0));
        return Some(json!({ "MSKSourceDescription": d }));
    }
    if let Some(db) = input.obj_field("DatabaseSourceConfiguration") {
        return Some(json!({ "DatabaseSourceDescription": db }));
    }
    input
        .obj_field("DirectPutSourceConfiguration")
        .map(|d| json!({ "DirectPutSourceDescription": d }))
}

fn tags(list: &[Value]) -> Result<Vec<(String, Option<String>)>, AwsError> {
    list.iter()
        .map(|t| {
            let key = t
                .str_field("Key")
                .ok_or_else(|| invalid("Tag Key is required."))?;
            if key.len() > 128 || key.starts_with("aws:") {
                return Err(invalid(format!(
                    "Tag Key \"{key}\" contains invalid characters or uses reserved prefix."
                )));
            }
            let value = t.get("Value").and_then(Value::as_str).map(str::to_string);
            if value.as_ref().is_some_and(|v| v.len() > 256) {
                return Err(invalid(format!(
                    "Tag Value for key \"{key}\" exceeds maximum length of 256."
                )));
            }
            Ok((key.to_string(), value))
        })
        .collect()
}

fn tag_value((k, v): &(String, Option<String>)) -> Value {
    let mut t = json!({ "Key": k });
    if let Some(v) = v {
        t["Value"] = json!(v);
    }
    t
}

/// S3 key Firehose writes a batch under: the prefix, then
/// `YYYY/MM/DD/HH/<stream>-<version>-YYYY-MM-DD-HH-MM-SS-<uuid>` in UTC.
fn object_key(prefix: &str, stream: &DeliveryStream, now_ms: f64, id: &str) -> String {
    let secs = (now_ms / 1000.0).floor() as i64;
    let (y, mo, d) = civil_from_days(secs.div_euclid(86400));
    let rem = secs.rem_euclid(86400);
    let (h, mi, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    format!(
        "{prefix}{y:04}/{mo:02}/{d:02}/{h:02}/{}-{}-{y:04}-{mo:02}-{d:02}-{h:02}-{mi:02}-{s:02}-{id}",
        stream.name, stream.version
    )
}

/// Writes one batch of records as a single S3 object when the stream has an
/// S3 destination.
fn flush(ctx: &mut Ctx<'_>, stream: &DeliveryStream, records: &[Vec<u8>]) {
    let Some(dest) = stream.s3_destination() else {
        return;
    };
    let Some(bucket_arn) = dest.get("BucketARN").and_then(Value::as_str) else {
        return;
    };
    let mut data: Vec<u8> = records.concat();
    let prefix = dest
        .get("Prefix")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let id = ctx.uuid();
    let gzipped = dest.get("CompressionFormat").and_then(Value::as_str) == Some("GZIP");
    if gzipped {
        data = gzip(&data);
    }
    let mut key = object_key(prefix, stream, ctx.now_ms(), &id);
    if gzipped {
        key.push_str(".gz");
    }
    let mut attributes = Map::new();
    attributes.insert("Key".to_string(), json!(key));
    ctx.deliver(Delivery {
        target: bucket_arn.to_string(),
        source: stream.arn.clone(),
        bytes: Some(data),
        attributes,
        ..Default::default()
    });
}

fn decode_record(record: &Value) -> Result<Vec<u8>, AwsError> {
    let data = record
        .get("Data")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("Record.Data is required."))?;
    STANDARD
        .decode(data)
        .map_err(|_| invalid("Record.Data is not valid base64."))
}

fn direct_put(stream: &DeliveryStream) -> Result<(), AwsError> {
    if stream.kind == "DirectPut" {
        Ok(())
    } else {
        Err(invalid(format!(
            "This operation is not permitted on KinesisStreamAsSource delivery stream type. Delivery stream {} is of type {}.",
            stream.name, stream.kind
        )))
    }
}

fn call(
    streams: &mut BTreeMap<String, DeliveryStream>,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> OpResult {
    let now = ctx.now_ms();
    Ok(match operation {
        "CreateDeliveryStream" => {
            let name = required(input, "DeliveryStreamName")?.to_string();
            if streams.contains_key(&name) {
                return Err(AwsError::new(
                    400,
                    "ResourceInUseException",
                    format!(
                        "Firehose {name} under accountId {} already exists",
                        ctx.account
                    ),
                ));
            }
            let source = source(input, now);
            let kind = input.str_field("DeliveryStreamType").map_or_else(
                || {
                    if input.get("KinesisStreamSourceConfiguration").is_some() {
                        "KinesisStreamAsSource"
                    } else {
                        "DirectPut"
                    }
                    .to_string()
                },
                str::to_string,
            );
            let encryption = input
                .obj_field("DeliveryStreamEncryptionConfigurationInput")
                .map(|c| {
                    let mut e = c.clone();
                    e.insert("Status".to_string(), json!("ENABLED"));
                    Value::Object(e)
                });
            let stream = DeliveryStream {
                arn: ctx.arn("firehose", &format!("deliverystream/{name}")),
                name: name.clone(),
                kind,
                version: 1,
                created: now,
                updated: now,
                destinations: destinations(input),
                tags: tags(input.array_field("Tags"))?,
                encryption,
                source,
            };
            let arn = stream.arn.clone();
            streams.insert(name, stream);
            json!({ "DeliveryStreamARN": arn })
        }
        "DescribeDeliveryStream" => {
            let s = stream_mut(streams, ctx, required(input, "DeliveryStreamName")?)?;
            json!({ "DeliveryStreamDescription": s.describe() })
        }
        "ListDeliveryStreams" => {
            let kind = input.str_field("DeliveryStreamType");
            let after = input.str_field("ExclusiveStartDeliveryStreamName");
            let limit = input.i64_field("Limit").unwrap_or(10);
            if !(1..=10000).contains(&limit) {
                return Err(invalid("Limit must be between 1 and 10000."));
            }
            let names: Vec<&String> = streams
                .values()
                .filter(|s| kind.is_none_or(|k| s.kind == k))
                .filter(|s| after.is_none_or(|a| s.name.as_str() > a))
                .map(|s| &s.name)
                .collect();
            json!({
                "DeliveryStreamNames": names.iter().take(limit as usize).collect::<Vec<_>>(),
                "HasMoreDeliveryStreams": names.len() > limit as usize,
            })
        }
        "DeleteDeliveryStream" => {
            let name = required(input, "DeliveryStreamName")?;
            stream_mut(streams, ctx, name)?;
            streams.remove(name);
            json!({})
        }
        "PutRecord" => {
            let s = stream_mut(streams, ctx, required(input, "DeliveryStreamName")?)?;
            direct_put(s)?;
            let record = input
                .get("Record")
                .ok_or_else(|| invalid("Record is required."))?;
            let data = decode_record(record)?;
            flush(ctx, s, &[data]);
            json!({ "RecordId": record_id(ctx), "Encrypted": s.encryption.is_some() })
        }
        "PutRecordBatch" => {
            let s = stream_mut(streams, ctx, required(input, "DeliveryStreamName")?)?;
            direct_put(s)?;
            let records = input.array_field("Records");
            if records.is_empty() || records.len() > 500 {
                return Err(invalid("Records must contain between 1 and 500 items."));
            }
            let data = records
                .iter()
                .map(decode_record)
                .collect::<Result<Vec<_>, _>>()?;
            flush(ctx, s, &data);
            let responses: Vec<Value> = data
                .iter()
                .map(|_| json!({ "RecordId": record_id(ctx) }))
                .collect();
            json!({
                "FailedPutCount": 0,
                "Encrypted": s.encryption.is_some(),
                "RequestResponses": responses,
            })
        }
        "TagDeliveryStream" => {
            let s = stream_mut(streams, ctx, required(input, "DeliveryStreamName")?)?;
            let list = input.array_field("Tags");
            if list.is_empty() || list.len() > 50 {
                return Err(invalid("Tags must contain between 1 and 50 items."));
            }
            for (k, v) in tags(list)? {
                match s.tags.iter_mut().find(|(x, _)| *x == k) {
                    Some(t) => t.1 = v,
                    None => s.tags.push((k, v)),
                }
            }
            s.updated = now;
            json!({})
        }
        "UntagDeliveryStream" => {
            let s = stream_mut(streams, ctx, required(input, "DeliveryStreamName")?)?;
            let keys: Vec<&str> = input
                .array_field("TagKeys")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            if keys.is_empty() || keys.len() > 50 {
                return Err(invalid("TagKeys must contain between 1 and 50 items."));
            }
            s.tags.retain(|(k, _)| !keys.contains(&k.as_str()));
            s.updated = now;
            json!({})
        }
        "ListTagsForDeliveryStream" => {
            let s = stream_mut(streams, ctx, required(input, "DeliveryStreamName")?)?;
            let after = input.str_field("ExclusiveStartTagKey");
            let limit = input.i64_field("Limit").unwrap_or(50);
            if !(1..=50).contains(&limit) {
                return Err(invalid("Limit must be between 1 and 50."));
            }
            let mut sorted: Vec<&(String, Option<String>)> = s
                .tags
                .iter()
                .filter(|(k, _)| after.is_none_or(|a| k.as_str() > a))
                .collect();
            sorted.sort();
            json!({
                "Tags": sorted.iter().take(limit as usize).map(|t| tag_value(t)).collect::<Vec<_>>(),
                "HasMoreTags": sorted.len() > limit as usize,
            })
        }
        "StartDeliveryStreamEncryption" => {
            let s = stream_mut(streams, ctx, required(input, "DeliveryStreamName")?)?;
            let config = input
                .obj_field("DeliveryStreamEncryptionConfigurationInput")
                .ok_or_else(|| {
                    invalid("DeliveryStreamEncryptionConfigurationInput is required.")
                })?;
            let key_type = config
                .get("KeyType")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    invalid("DeliveryStreamEncryptionConfigurationInput.KeyType is required.")
                })?;
            let mut e = json!({ "KeyType": key_type, "Status": "ENABLED" });
            if let Some(k) = config.get("KeyARN") {
                e["KeyARN"] = k.clone();
            }
            s.encryption = Some(e);
            s.updated = now;
            json!({})
        }
        "StopDeliveryStreamEncryption" => {
            let s = stream_mut(streams, ctx, required(input, "DeliveryStreamName")?)?;
            if s.encryption.is_none() {
                return Err(AwsError::new(
                    400,
                    "ResourceInUseException",
                    format!(
                        "Delivery stream {} has no active encryption to stop.",
                        s.name
                    ),
                ));
            }
            s.encryption = None;
            s.updated = now;
            json!({})
        }
        "UpdateDestination" => {
            let s = stream_mut(streams, ctx, required(input, "DeliveryStreamName")?)?;
            let current = required(input, "CurrentDeliveryStreamVersionId")?;
            if current != s.version.to_string() {
                return Err(AwsError::new(
                    400,
                    "ConcurrentModificationException",
                    format!(
                        "Cannot update firehose: {} since the current version id: {} and specified version id: {current} do not match",
                        s.name, s.version
                    ),
                ));
            }
            let id = required(input, "DestinationId")?.to_string();
            let updates: Vec<(String, Map<String, Value>)> = input
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(k, _)| k.ends_with("DestinationUpdate"))
                .filter_map(|(k, v)| Some((description_key(k, "Update")?, v.as_object()?.clone())))
                .collect();
            let i = match s
                .destinations
                .iter()
                .position(|d| d["DestinationId"] == id.as_str())
            {
                Some(i) => i,
                None => {
                    s.destinations
                        .push(Map::from_iter([("DestinationId".to_string(), json!(id))]));
                    s.destinations.len() - 1
                }
            };
            let dest = &mut s.destinations[i];
            for (desc, update) in updates {
                let entry = dest
                    .entry(desc.clone())
                    .or_insert_with(|| Value::Object(with_defaults(&desc, Map::new())));
                if let Some(e) = entry.as_object_mut() {
                    e.extend(update);
                }
            }
            s.version += 1;
            s.updated = now;
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

fn record_id(ctx: &mut Ctx<'_>) -> String {
    let mut b = [0u8; 56];
    ctx.random_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Service for Firehose {
    fn signing_name(&self) -> &'static str {
        "firehose"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("firehose")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let streams = self
            .regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default();
        call(streams, ctx, operation, &input)
    }

    fn deliver(&mut self, ctx: &mut Ctx<'_>, resource: &str, delivery: &Delivery) -> bool {
        let Some(stream) = resource.strip_prefix("deliverystream/").and_then(|name| {
            self.regions
                .get(&(ctx.account.clone(), ctx.region.clone()))?
                .get(name)
        }) else {
            return false;
        };
        let data = delivery
            .bytes
            .clone()
            .unwrap_or_else(|| delivery.body.clone().into_bytes());
        flush(ctx, stream, &[data]);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_keys_follow_the_firehose_layout() {
        let s = DeliveryStream {
            name: "logs".into(),
            arn: String::new(),
            kind: "DirectPut".into(),
            version: 3,
            created: 0.0,
            updated: 0.0,
            destinations: Vec::new(),
            tags: Vec::new(),
            encryption: None,
            source: None,
        };
        assert_eq!(
            object_key("raw/", &s, 1_767_323_045_000.0, "id"),
            "raw/2026/01/02/03/logs-3-2026-01-02-03-04-05-id"
        );
    }
}
