use serde_json::{Value, json};

use super::Buckets;
use crate::codec::rest::uri_encode;
use crate::codec::time::format_iso8601;
use crate::service::{Ctx, Delivery, InputExt};

const DESTINATIONS: &[(&str, &str)] = &[
    ("QueueConfigurations", "QueueArn"),
    ("TopicConfigurations", "TopicArn"),
    ("LambdaFunctionConfigurations", "LambdaFunctionArn"),
];

struct Change<'a> {
    event: &'static str,
    key: &'a str,
    version_id: Option<String>,
}

fn changes<'a>(operation: &str, input: &'a Value, output: &Value) -> Vec<Change<'a>> {
    let one = |event| {
        input
            .str_field("Key")
            .map(|key| Change {
                event,
                key,
                version_id: output.str_field("VersionId").map(str::to_string),
            })
            .into_iter()
            .collect()
    };
    match operation {
        "PutObject" => one("ObjectCreated:Put"),
        "CopyObject" => one("ObjectCreated:Copy"),
        "CompleteMultipartUpload" => one("ObjectCreated:CompleteMultipartUpload"),
        "DeleteObject" if output.get("DeleteMarker").and_then(Value::as_bool) == Some(true) => {
            one("ObjectRemoved:DeleteMarkerCreated")
        }
        "DeleteObject" => one("ObjectRemoved:Delete"),
        "PutObjectTagging" => one("ObjectTagging:Put"),
        "DeleteObjectTagging" => one("ObjectTagging:Delete"),
        "PutObjectAcl" => one("ObjectAcl:Put"),
        "RestoreObject" => one("ObjectRestore:Post"),
        "DeleteObjects" => {
            let failed: Vec<&str> = output
                .array_field("Errors")
                .iter()
                .filter_map(|e| e.str_field("Key"))
                .collect();
            input
                .get("Delete")
                .map(|d| d.array_field("Objects"))
                .unwrap_or_default()
                .iter()
                .filter_map(|o| o.str_field("Key"))
                .filter(|k| !failed.contains(k))
                .map(|key| Change {
                    event: if output.array_field("Deleted").iter().any(|d| {
                        d.str_field("Key") == Some(key)
                            && d.get("DeleteMarker").and_then(Value::as_bool) == Some(true)
                    }) {
                        "ObjectRemoved:DeleteMarkerCreated"
                    } else {
                        "ObjectRemoved:Delete"
                    },
                    key,
                    version_id: None,
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

fn wants(config: &Value, event: &str, key: &str) -> bool {
    let full = format!("s3:{event}");
    let event_ok = config
        .array_field("Events")
        .iter()
        .filter_map(Value::as_str)
        .any(|e| e == full || e.strip_suffix('*').is_some_and(|p| full.starts_with(p)));
    let rules = config
        .get("Filter")
        .and_then(|f| f.get("Key"))
        .map(|k| k.array_field("FilterRules"))
        .unwrap_or_default();
    event_ok
        && rules.iter().all(|r| {
            let value = r.str_field("Value").unwrap_or_default();
            match r.str_field("Name").map(str::to_ascii_lowercase).as_deref() {
                Some("prefix") => key.starts_with(value),
                Some("suffix") => key.ends_with(value),
                _ => true,
            }
        })
}

fn detail_type(event: &str) -> &'static str {
    match event {
        e if e.starts_with("ObjectCreated") => "Object Created",
        e if e.starts_with("ObjectRemoved") => "Object Deleted",
        "ObjectTagging:Put" => "Object Tags Added",
        "ObjectTagging:Delete" => "Object Tags Deleted",
        "ObjectAcl:Put" => "Object ACL Updated",
        _ => "Object Restore Initiated",
    }
}

/// Queues the bucket's configured notifications for a successful operation.
pub fn emit(buckets: &Buckets, ctx: &mut Ctx<'_>, operation: &str, input: &Value, output: &Value) {
    let Some(name) = input.str_field("Bucket") else {
        return;
    };
    let Some(bucket) = buckets.get(name) else {
        return;
    };
    let Some(config) = bucket.configs.get("NotificationConfiguration") else {
        return;
    };
    if operation == "PutBucketNotificationConfiguration" {
        test_event(ctx, name, config);
        return;
    }
    for change in changes(operation, input, output) {
        let object = bucket.current(change.key);
        let size = object.map_or(0, |o| o.body.len());
        let etag = object.map_or_else(String::new, |o| o.etag.trim_matches('"').to_string());
        let version_id = change
            .version_id
            .clone()
            .or_else(|| object.and_then(|o| o.version_id.clone()));
        let sequencer = format!("{:016X}", (ctx.now_ms() * 1000.0) as u64);
        let request_id = ctx.uuid().replace('-', "").to_uppercase()[..16].to_string();
        let mut s3_object = json!({
            "key": uri_encode(change.key, true).replace("%20", "+"),
            "size": size,
            "eTag": etag,
            "sequencer": sequencer,
        });
        if let Some(v) = &version_id {
            s3_object["versionId"] = json!(v);
        }
        for (list, arn_key) in DESTINATIONS {
            for c in config.array_field(list) {
                let Some(target) = c.str_field(arn_key) else {
                    continue;
                };
                if !wants(c, change.event, change.key) {
                    continue;
                }
                let record = json!({
                    "eventVersion": "2.1",
                    "eventSource": "aws:s3",
                    "awsRegion": bucket.region,
                    "eventTime": format_iso8601(ctx.now_ms().floor() / 1000.0),
                    "eventName": change.event,
                    "userIdentity": {"principalId": ctx.account},
                    "requestParameters": {"sourceIPAddress": "127.0.0.1"},
                    "responseElements": {"x-amz-request-id": request_id, "x-amz-id-2": request_id},
                    "s3": {
                        "s3SchemaVersion": "1.0",
                        "configurationId": c.str_field("Id").unwrap_or_default(),
                        "bucket": {
                            "name": name,
                            "ownerIdentity": {"principalId": ctx.account},
                            "arn": format!("arn:aws:s3:::{name}"),
                        },
                        "object": s3_object,
                    },
                });
                ctx.deliver(Delivery {
                    target: target.to_string(),
                    source: format!("arn:aws:s3:::{name}"),
                    body: json!({ "Records": [record] }).to_string(),
                    subject: Some("Amazon S3 Notification".into()),
                    ..Delivery::default()
                });
            }
        }
        if config.get("EventBridgeConfiguration").is_some() {
            let mut detail = json!({
                "version": "0",
                "bucket": {"name": name},
                "object": {"key": change.key, "size": size, "etag": etag, "sequencer": sequencer},
                "request-id": request_id,
                "requester": ctx.account,
                "source-ip-address": "127.0.0.1",
                "reason": operation,
            });
            if let Some(v) = &version_id {
                detail["object"]["version-id"] = json!(v);
            }
            if change.event.starts_with("ObjectRemoved") {
                detail["deletion-type"] = json!(if change.event.ends_with("DeleteMarkerCreated") {
                    "Delete Marker Created"
                } else {
                    "Permanently Deleted"
                });
            }
            ctx.deliver(Delivery {
                target: format!(
                    "arn:aws:events:{}:{}:event-bus/default",
                    bucket.region, ctx.account
                ),
                source: format!("arn:aws:s3:::{name}"),
                body: json!({
                    "Source": "aws.s3",
                    "DetailType": detail_type(change.event),
                    "Detail": detail,
                    "Resources": [format!("arn:aws:s3:::{name}")],
                })
                .to_string(),
                ..Delivery::default()
            });
        }
    }
}

fn test_event(ctx: &mut Ctx<'_>, name: &str, config: &Value) {
    for (list, arn_key) in &DESTINATIONS[..2] {
        for c in config.array_field(list) {
            if let Some(target) = c.str_field(arn_key) {
                let request_id = ctx.uuid().replace('-', "").to_uppercase()[..16].to_string();
                let body = json!({
                    "Service": "Amazon S3",
                    "Event": "s3:TestEvent",
                    "Time": format_iso8601(ctx.now_ms().floor() / 1000.0),
                    "Bucket": name,
                    "RequestId": request_id,
                    "HostId": request_id,
                });
                ctx.deliver(Delivery {
                    target: target.to_string(),
                    source: format!("arn:aws:s3:::{name}"),
                    body: body.to_string(),
                    subject: Some("Amazon S3 Notification".into()),
                    ..Delivery::default()
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_and_key_filters() {
        let c = json!({
            "Events": ["s3:ObjectCreated:*", "s3:ObjectRemoved:Delete"],
            "Filter": {"Key": {"FilterRules": [{"Name": "Prefix", "Value": "img/"}, {"Name": "suffix", "Value": ".png"}]}},
        });
        assert!(wants(&c, "ObjectCreated:Put", "img/a.png"));
        assert!(wants(&c, "ObjectRemoved:Delete", "img/a.png"));
        assert!(!wants(&c, "ObjectRemoved:DeleteMarkerCreated", "img/a.png"));
        assert!(!wants(&c, "ObjectCreated:Put", "doc/a.png"));
        assert!(!wants(&c, "ObjectCreated:Put", "img/a.jpg"));
        assert!(wants(
            &json!({"Events": ["s3:*"]}),
            "ObjectTagging:Put",
            "k"
        ));
    }
}
