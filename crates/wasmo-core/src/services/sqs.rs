use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use md5::{Digest, Md5};
use serde_json::{Map, Value, json};
use sha2::Sha256;

use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Ctx, Delivery, InputExt, OpResult, Service, ServiceModel};

const DEDUP_WINDOW_MS: f64 = 5.0 * 60.0 * 1000.0;

#[derive(Debug, Clone)]
struct Message {
    id: String,
    receipt_handle: String,
    body: String,
    md5_of_body: String,
    attributes: Option<Map<String, Value>>,
    md5_of_attributes: Option<String>,
    invisible_until: f64,
    receive_count: u64,
    sent_at: f64,
    first_received_at: Option<f64>,
    sender_id: String,
    group_id: Option<String>,
    dedup_id: Option<String>,
    sequence_number: Option<String>,
}

#[derive(Debug, Clone)]
struct Dedup {
    at: f64,
    result: Value,
}

#[derive(Debug, Clone)]
struct Queue {
    name: String,
    url: String,
    attributes: BTreeMap<String, String>,
    tags: BTreeMap<String, String>,
    messages: Vec<Message>,
    created_at: f64,
    modified_at: f64,
    sequence: u64,
    dedup: HashMap<String, Dedup>,
}

impl Queue {
    fn is_fifo(&self) -> bool {
        self.attributes
            .get("FifoQueue")
            .is_some_and(|v| v == "true")
            || self.name.ends_with(".fifo")
    }

    fn int_attr(&self, key: &str, default: i64) -> i64 {
        self.attributes
            .get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    fn redrive(&self) -> Option<(String, String, u64)> {
        let policy: Value = serde_json::from_str(self.attributes.get("RedrivePolicy")?).ok()?;
        let arn = policy.str_field("deadLetterTargetArn")?.to_string();
        let max = policy.i64_field("maxReceiveCount").filter(|m| *m > 0)? as u64;
        Some((name_from_arn(&arn).to_string(), arn, max))
    }
}

#[derive(Debug, Clone)]
struct MoveTask {
    handle: String,
    status: String,
    source_arn: String,
    destination_arn: Option<String>,
    max_per_second: Option<i64>,
    moved: usize,
    to_move: usize,
    started_at: f64,
}

#[derive(Debug, Default)]
struct Region {
    queues: BTreeMap<String, Queue>,
    move_tasks: Vec<MoveTask>,
}

#[derive(Default)]
pub struct Sqs {
    regions: HashMap<(String, String), Region>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn md5_hex(bytes: &[u8]) -> String {
    hex(&Md5::digest(bytes))
}

fn md5_of_attributes(attrs: &Map<String, Value>) -> Option<String> {
    if attrs.is_empty() {
        return None;
    }
    let mut names: Vec<&String> = attrs.keys().collect();
    names.sort();
    let mut h = Md5::new();
    let prefixed = |h: &mut Md5, bytes: &[u8]| {
        h.update((bytes.len() as u32).to_be_bytes());
        h.update(bytes);
    };
    for name in names {
        let attr = &attrs[name];
        prefixed(&mut h, name.as_bytes());
        prefixed(
            &mut h,
            attr.str_field("DataType").unwrap_or("String").as_bytes(),
        );
        if let Some(s) = attr.get("StringValue").and_then(Value::as_str) {
            h.update([1u8]);
            prefixed(&mut h, s.as_bytes());
        } else if let Some(b) = attr.get("BinaryValue").and_then(Value::as_str) {
            h.update([2u8]);
            prefixed(&mut h, &STANDARD.decode(b).unwrap_or_default());
        }
    }
    Some(hex(&h.finalize()))
}

fn name_from_url(url: &str) -> &str {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
}

fn name_from_arn(arn: &str) -> &str {
    arn.rsplit(':').next().unwrap_or_default()
}

fn err(code: &str, message: impl Into<String>) -> AwsError {
    AwsError::new(400, code, message)
}

fn no_queue() -> AwsError {
    err("QueueDoesNotExist", "The specified queue does not exist.")
}

fn invalid_receipt(handle: &str) -> AwsError {
    err(
        "ReceiptHandleIsInvalid",
        format!("The input receipt handle \"{handle}\" is not a valid receipt handle."),
    )
}

fn string_map(v: Option<&Map<String, Value>>) -> BTreeMap<String, String> {
    v.into_iter()
        .flatten()
        .map(|(k, v)| {
            (
                k.clone(),
                v.as_str().map_or_else(|| v.to_string(), str::to_string),
            )
        })
        .collect()
}

fn strings(v: &[Value]) -> Vec<String> {
    v.iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn paginate(items: Vec<String>, input: &Value) -> (Vec<String>, Option<String>) {
    let start = input
        .str_field("NextToken")
        .and_then(|t| STANDARD.decode(t).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    match input.i64_field("MaxResults") {
        Some(max) => {
            let end = (start + max.max(0) as usize).min(items.len());
            let next = (end < items.len()).then(|| STANDARD.encode(end.to_string()));
            (items[start.min(end)..end].to_vec(), next)
        }
        None => (items.into_iter().skip(start).collect(), None),
    }
}

fn batch_entries(input: &Value) -> Result<&[Value], AwsError> {
    let entries = input.array_field("Entries");
    if entries.is_empty() {
        return Err(err(
            "EmptyBatchRequest",
            "There should be at least one entry in the request.",
        ));
    }
    if entries.len() > 10 {
        return Err(err(
            "TooManyEntriesInBatchRequest",
            "Maximum number of entries per request are 10.",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for (i, e) in entries.iter().enumerate() {
        if !seen.insert(
            e.str_field("Id")
                .map_or_else(|| i.to_string(), str::to_string),
        ) {
            return Err(err(
                "BatchEntryIdsNotDistinct",
                "Two or more batch entries in the request have the same Id.",
            ));
        }
    }
    Ok(entries)
}

fn entry_id(entry: &Value, i: usize) -> String {
    entry
        .str_field("Id")
        .map_or_else(|| i.to_string(), str::to_string)
}

fn batch_failure(id: String, code: &str, message: &str) -> Value {
    json!({ "Id": id, "SenderFault": true, "Code": code, "Message": message })
}

fn requested_names(input: &Value, keys: &[&str]) -> Vec<String> {
    keys.iter()
        .flat_map(|k| strings(input.array_field(k)))
        .collect()
}

impl Sqs {
    fn region(&mut self, ctx: &Ctx<'_>) -> &mut Region {
        self.regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default()
    }

    fn queue_arn(ctx: &Ctx<'_>, name: &str) -> String {
        format!("arn:aws:sqs:{}:{}:{name}", ctx.region, ctx.account)
    }
}

fn queue_mut<'a>(region: &'a mut Region, input: &Value) -> Result<&'a mut Queue, AwsError> {
    let url = input.require_str("QueueUrl")?;
    region
        .queues
        .get_mut(name_from_url(url))
        .ok_or_else(no_queue)
}

fn send_one(ctx: &mut Ctx<'_>, queue: &mut Queue, entry: &Value) -> Result<Value, AwsError> {
    let body = entry
        .str_field("MessageBody")
        .unwrap_or_default()
        .to_string();
    if body.is_empty() {
        return Err(err(
            "MissingParameter",
            "The request must contain the parameter MessageBody.",
        ));
    }
    let attributes = entry
        .obj_field("MessageAttributes")
        .filter(|m| !m.is_empty())
        .cloned();
    let now = ctx.now_ms();
    let fifo = queue.is_fifo();
    let (mut group_id, mut dedup_id) = (None, None);
    if fifo {
        group_id = Some(
            entry
                .str_field("MessageGroupId")
                .ok_or_else(|| {
                    err(
                        "MissingParameter",
                        "The request must contain the parameter MessageGroupId.",
                    )
                })?
                .to_string(),
        );
        dedup_id = Some(match entry.str_field("MessageDeduplicationId") {
            Some(id) => id.to_string(),
            None if queue
                .attributes
                .get("ContentBasedDeduplication")
                .is_some_and(|v| v == "true") =>
            {
                hex(&Sha256::digest(body.as_bytes()))
            }
            None => {
                return Err(err(
                    "InvalidParameterValue",
                    "The queue should either have ContentBasedDeduplication enabled or MessageDeduplicationId provided explicitly",
                ));
            }
        });
        queue.dedup.retain(|_, d| now - d.at <= DEDUP_WINDOW_MS);
        if let Some(existing) = dedup_id.as_ref().and_then(|id| queue.dedup.get(id)) {
            return Ok(existing.result.clone());
        }
    }
    let delay = if fifo {
        0
    } else {
        entry
            .i64_field("DelaySeconds")
            .unwrap_or_else(|| queue.int_attr("DelaySeconds", 0))
    };
    let sequence_number = fifo.then(|| {
        queue.sequence += 1;
        format!("{:020}", queue.sequence)
    });
    let message = Message {
        id: ctx.uuid(),
        receipt_handle: ctx.uuid(),
        md5_of_body: md5_hex(body.as_bytes()),
        md5_of_attributes: attributes.as_ref().and_then(md5_of_attributes),
        body,
        attributes,
        invisible_until: if delay > 0 {
            now + delay as f64 * 1000.0
        } else {
            0.0
        },
        receive_count: 0,
        sent_at: now,
        first_received_at: None,
        sender_id: ctx.account.clone(),
        group_id,
        dedup_id: dedup_id.clone(),
        sequence_number: sequence_number.clone(),
    };
    let mut result = json!({ "MessageId": message.id, "MD5OfMessageBody": message.md5_of_body });
    if let Some(m) = &message.md5_of_attributes {
        result["MD5OfMessageAttributes"] = json!(m);
    }
    if let Some(s) = &sequence_number {
        result["SequenceNumber"] = json!(s);
    }
    queue.messages.push(message);
    if let Some(id) = dedup_id {
        queue.dedup.insert(
            id,
            Dedup {
                at: now,
                result: result.clone(),
            },
        );
    }
    Ok(result)
}

fn system_attributes(m: &Message, requested: &[String]) -> Option<Map<String, Value>> {
    if requested.is_empty() {
        return None;
    }
    let all = requested.iter().any(|r| r == "All");
    let mut available = vec![
        ("ApproximateReceiveCount", m.receive_count.to_string()),
        ("SentTimestamp", format!("{}", m.sent_at as i64)),
        ("SenderId", m.sender_id.clone()),
    ];
    if let Some(t) = m.first_received_at {
        available.push(("ApproximateFirstReceiveTimestamp", format!("{}", t as i64)));
    }
    for (k, v) in [
        ("MessageGroupId", &m.group_id),
        ("SequenceNumber", &m.sequence_number),
        ("MessageDeduplicationId", &m.dedup_id),
    ] {
        if let Some(v) = v {
            available.push((k, v.clone()));
        }
    }
    let out: Map<String, Value> = available
        .into_iter()
        .filter(|(k, _)| all || requested.iter().any(|r| r == k))
        .map(|(k, v)| (k.to_string(), Value::String(v)))
        .collect();
    (!out.is_empty()).then_some(out)
}

fn filter_message_attributes(
    attrs: &Option<Map<String, Value>>,
    requested: &[String],
) -> Option<Map<String, Value>> {
    let attrs = attrs.as_ref()?;
    if requested.is_empty() {
        return None;
    }
    if requested.iter().any(|r| r == "All" || r == ".*") {
        return Some(attrs.clone());
    }
    let out: Map<String, Value> = attrs
        .iter()
        .filter(|(k, _)| {
            requested
                .iter()
                .any(|r| r == *k || r.strip_suffix(".*").is_some_and(|p| k.starts_with(p)))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!out.is_empty()).then_some(out)
}

fn receive(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    name: &str,
    input: &Value,
) -> Result<Value, AwsError> {
    let now = ctx.now_ms();
    let queue = region.queues.get(name).ok_or_else(no_queue)?;
    let limit = input
        .i64_field("MaxNumberOfMessages")
        .filter(|n| *n > 0)
        .map_or(1, |n| n.min(10)) as usize;
    let visibility = input
        .i64_field("VisibilityTimeout")
        .unwrap_or_else(|| queue.int_attr("VisibilityTimeout", 30));
    let redrive = queue.redrive();
    let fifo = queue.is_fifo();
    let locked: std::collections::HashSet<String> = if fifo {
        queue
            .messages
            .iter()
            .filter(|m| m.invisible_until > now)
            .filter_map(|m| m.group_id.clone())
            .collect()
    } else {
        Default::default()
    };
    let mut queue = region.queues.remove(name).expect("queue exists");
    let mut selected = Vec::new();
    let mut remaining = Vec::with_capacity(queue.messages.len());
    let mut dead = Vec::new();
    for mut m in std::mem::take(&mut queue.messages) {
        if m.invisible_until > now {
            remaining.push(m);
            continue;
        }
        if let Some((dlq, _, max)) = &redrive
            && m.receive_count >= *max
            && region.queues.contains_key(dlq)
        {
            dead.push((dlq.clone(), m));
            continue;
        }
        let group_locked = m.group_id.as_ref().is_some_and(|g| locked.contains(g));
        if selected.len() >= limit || (fifo && group_locked) {
            remaining.push(m);
            continue;
        }
        m.invisible_until = now + visibility as f64 * 1000.0;
        m.receive_count += 1;
        m.first_received_at.get_or_insert(now);
        m.receipt_handle = ctx.uuid();
        selected.push(m.clone());
        remaining.push(m);
    }
    queue.messages = remaining;
    region.queues.insert(name.to_string(), queue);
    for (dlq, mut m) in dead {
        m.receipt_handle = ctx.uuid();
        m.invisible_until = 0.0;
        m.receive_count = 0;
        m.first_received_at = None;
        region
            .queues
            .get_mut(&dlq)
            .expect("dlq exists")
            .messages
            .push(m);
    }
    let system = requested_names(input, &["AttributeNames", "MessageSystemAttributeNames"]);
    let wanted = strings(input.array_field("MessageAttributeNames"));
    let messages: Vec<Value> = selected
        .iter()
        .map(|m| {
            let mut out = json!({ "MessageId": m.id, "ReceiptHandle": m.receipt_handle, "MD5OfBody": m.md5_of_body, "Body": m.body });
            if let Some(attrs) = filter_message_attributes(&m.attributes, &wanted) {
                out["MessageAttributes"] = Value::Object(attrs);
                if let Some(md5) = &m.md5_of_attributes {
                    out["MD5OfMessageAttributes"] = json!(md5);
                }
            }
            if let Some(attrs) = system_attributes(m, &system) {
                out["Attributes"] = Value::Object(attrs);
            }
            out
        })
        .collect();
    Ok(json!({ "Messages": messages }))
}

fn requeue(ctx: &mut Ctx<'_>, m: &Message) -> Message {
    Message {
        receipt_handle: ctx.uuid(),
        invisible_until: 0.0,
        receive_count: 0,
        first_received_at: None,
        ..m.clone()
    }
}

impl Service for Sqs {
    fn signing_name(&self) -> &'static str {
        "sqs"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("sqs")
    }

    fn deliver(&mut self, ctx: &mut Ctx<'_>, resource: &str, delivery: &Delivery) -> bool {
        let Some(queue) = self
            .regions
            .get_mut(&(ctx.account.clone(), ctx.region.clone()))
            .and_then(|r| r.queues.get_mut(resource))
        else {
            return false;
        };
        let mut entry = json!({ "MessageBody": delivery.body });
        if !delivery.attributes.is_empty() {
            entry["MessageAttributes"] = Value::Object(delivery.attributes.clone());
        }
        if let Some(g) = &delivery.group_id {
            entry["MessageGroupId"] = json!(g);
        }
        if let Some(d) = &delivery.dedup_id {
            entry["MessageDeduplicationId"] = json!(d);
        }
        send_one(ctx, queue, &entry).is_ok()
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let now = ctx.now_ms();
        let endpoint = ctx.endpoint.clone();
        let account = ctx.account.clone();
        Ok(match operation {
            "CreateQueue" => {
                let name = input.require_str("QueueName")?.to_string();
                let attributes = string_map(input.obj_field("Attributes"));
                if name.ends_with(".fifo")
                    != attributes.get("FifoQueue").is_some_and(|v| v == "true")
                {
                    return Err(err(
                        "InvalidParameterValue",
                        "The name of a FIFO queue can only include alphanumeric characters, hyphens, or underscores, must end with .fifo suffix.",
                    ));
                }
                let region = self.region(ctx);
                if let Some(existing) = region.queues.get(&name) {
                    if existing.attributes != attributes {
                        return Err(err(
                            "QueueNameExists",
                            "A queue already exists with the same name and a different value for attribute(s).",
                        ));
                    }
                    return Ok(json!({ "QueueUrl": existing.url }));
                }
                let url = format!("{endpoint}/{account}/{name}");
                region.queues.insert(
                    name.clone(),
                    Queue {
                        name,
                        url: url.clone(),
                        attributes,
                        tags: string_map(input.obj_field("tags")),
                        messages: Vec::new(),
                        created_at: now,
                        modified_at: now,
                        sequence: 0,
                        dedup: HashMap::new(),
                    },
                );
                json!({ "QueueUrl": url })
            }
            "GetQueueUrl" => {
                let name = input.require_str("QueueName")?;
                json!({ "QueueUrl": self.region(ctx).queues.get(name).ok_or_else(no_queue)?.url })
            }
            "ListQueues" => {
                let prefix = input
                    .str_field("QueueNamePrefix")
                    .unwrap_or_default()
                    .to_string();
                let urls = self
                    .region(ctx)
                    .queues
                    .values()
                    .filter(|q| q.name.starts_with(&prefix))
                    .map(|q| q.url.clone())
                    .collect();
                let (page, next) = paginate(urls, &input);
                let mut out = json!({ "QueueUrls": page });
                if let Some(t) = next {
                    out["NextToken"] = json!(t);
                }
                out
            }
            "DeleteQueue" => {
                let name = name_from_url(input.require_str("QueueUrl")?).to_string();
                self.region(ctx).queues.remove(&name).ok_or_else(no_queue)?;
                json!({})
            }
            "SendMessage" => {
                let mut queue = queue_mut(self.region(ctx), &input)?.clone();
                let result = send_one(ctx, &mut queue, &input);
                self.region(ctx).queues.insert(queue.name.clone(), queue);
                result?
            }
            "SendMessageBatch" => {
                let entries = batch_entries(&input)?.to_vec();
                let mut queue = queue_mut(self.region(ctx), &input)?.clone();
                let (mut ok, mut failed) = (Vec::new(), Vec::new());
                for (i, e) in entries.iter().enumerate() {
                    match send_one(ctx, &mut queue, e) {
                        Ok(mut r) => {
                            r["Id"] = json!(entry_id(e, i));
                            ok.push(r);
                        }
                        Err(e2) => {
                            failed.push(batch_failure(entry_id(e, i), &e2.code, &e2.message))
                        }
                    }
                }
                self.region(ctx).queues.insert(queue.name.clone(), queue);
                json!({ "Successful": ok, "Failed": failed })
            }
            "ReceiveMessage" => {
                let name = name_from_url(input.require_str("QueueUrl")?).to_string();
                let mut region = std::mem::take(self.region(ctx));
                let result = receive(ctx, &mut region, &name, &input);
                *self.region(ctx) = region;
                result?
            }
            "DeleteMessage" => {
                let handle = input.require_str("ReceiptHandle")?;
                let queue = queue_mut(self.region(ctx), &input)?;
                let i = queue
                    .messages
                    .iter()
                    .position(|m| m.receipt_handle == handle)
                    .ok_or_else(|| invalid_receipt(handle))?;
                queue.messages.remove(i);
                json!({})
            }
            "DeleteMessageBatch" => {
                let entries = batch_entries(&input)?.to_vec();
                let queue = queue_mut(self.region(ctx), &input)?;
                let (mut ok, mut failed) = (Vec::new(), Vec::new());
                for (i, e) in entries.iter().enumerate() {
                    let handle = e.str_field("ReceiptHandle").unwrap_or_default();
                    match queue
                        .messages
                        .iter()
                        .position(|m| m.receipt_handle == handle)
                    {
                        Some(p) => {
                            queue.messages.remove(p);
                            ok.push(json!({ "Id": entry_id(e, i) }));
                        }
                        None => failed.push(batch_failure(
                            entry_id(e, i),
                            "ReceiptHandleIsInvalid",
                            "The specified receipt handle is not valid.",
                        )),
                    }
                }
                json!({ "Successful": ok, "Failed": failed })
            }
            "ChangeMessageVisibility" => {
                let handle = input.require_str("ReceiptHandle")?.to_string();
                let timeout = input.i64_field("VisibilityTimeout").unwrap_or(30);
                let queue = queue_mut(self.region(ctx), &input)?;
                let m = queue
                    .messages
                    .iter_mut()
                    .find(|m| m.receipt_handle == handle)
                    .ok_or_else(|| invalid_receipt(&handle))?;
                m.invisible_until = now + timeout as f64 * 1000.0;
                json!({})
            }
            "ChangeMessageVisibilityBatch" => {
                let entries = batch_entries(&input)?.to_vec();
                let queue = queue_mut(self.region(ctx), &input)?;
                let (mut ok, mut failed) = (Vec::new(), Vec::new());
                for (i, e) in entries.iter().enumerate() {
                    let handle = e.str_field("ReceiptHandle").unwrap_or_default();
                    match queue
                        .messages
                        .iter_mut()
                        .find(|m| m.receipt_handle == handle)
                    {
                        Some(m) => {
                            m.invisible_until = now
                                + e.i64_field("VisibilityTimeout").unwrap_or(30) as f64 * 1000.0;
                            ok.push(json!({ "Id": entry_id(e, i) }));
                        }
                        None => failed.push(batch_failure(
                            entry_id(e, i),
                            "ReceiptHandleIsInvalid",
                            "The specified receipt handle is not valid.",
                        )),
                    }
                }
                json!({ "Successful": ok, "Failed": failed })
            }
            "GetQueueAttributes" => {
                let arn = {
                    let name = name_from_url(input.require_str("QueueUrl")?);
                    Self::queue_arn(ctx, name)
                };
                let queue = queue_mut(self.region(ctx), &input)?;
                let visible = queue
                    .messages
                    .iter()
                    .filter(|m| m.invisible_until <= now)
                    .count();
                let delayed = queue
                    .messages
                    .iter()
                    .filter(|m| m.invisible_until > now && m.receive_count == 0)
                    .count();
                let mut attrs: BTreeMap<String, String> = [
                    ("DelaySeconds", "0"),
                    ("MaximumMessageSize", "262144"),
                    ("MessageRetentionPeriod", "345600"),
                    ("ReceiveMessageWaitTimeSeconds", "0"),
                    ("VisibilityTimeout", "30"),
                    (
                        "SqsManagedSseEnabled",
                        if queue.is_fifo() { "false" } else { "true" },
                    ),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
                attrs.extend(queue.attributes.clone());
                attrs.insert("QueueArn".into(), arn);
                attrs.insert("ApproximateNumberOfMessages".into(), visible.to_string());
                attrs.insert(
                    "ApproximateNumberOfMessagesNotVisible".into(),
                    (queue.messages.len() - visible - delayed).to_string(),
                );
                attrs.insert(
                    "ApproximateNumberOfMessagesDelayed".into(),
                    delayed.to_string(),
                );
                attrs.insert(
                    "CreatedTimestamp".into(),
                    ((queue.created_at / 1000.0) as i64).to_string(),
                );
                attrs.insert(
                    "LastModifiedTimestamp".into(),
                    ((queue.modified_at / 1000.0) as i64).to_string(),
                );
                let names = strings(input.array_field("AttributeNames"));
                if !names.is_empty() && !names.iter().any(|n| n == "All") {
                    attrs.retain(|k, _| names.contains(k));
                }
                json!({ "Attributes": attrs })
            }
            "SetQueueAttributes" => {
                let attributes = string_map(input.obj_field("Attributes"));
                let queue = queue_mut(self.region(ctx), &input)?;
                queue.attributes.extend(attributes);
                queue.modified_at = now;
                json!({})
            }
            "PurgeQueue" => {
                queue_mut(self.region(ctx), &input)?.messages.clear();
                json!({})
            }
            "TagQueue" => {
                let tags = string_map(input.obj_field("Tags"));
                queue_mut(self.region(ctx), &input)?.tags.extend(tags);
                json!({})
            }
            "UntagQueue" => {
                let keys = strings(input.array_field("TagKeys"));
                let queue = queue_mut(self.region(ctx), &input)?;
                queue.tags.retain(|k, _| !keys.contains(k));
                json!({})
            }
            "ListQueueTags" => json!({ "Tags": queue_mut(self.region(ctx), &input)?.tags }),
            "AddPermission" | "RemovePermission" => {
                let label = input.require_str("Label")?.to_string();
                let name = name_from_url(input.require_str("QueueUrl")?).to_string();
                let arn = Self::queue_arn(ctx, &name);
                let accounts = strings(input.array_field("AWSAccountIds"));
                let actions: Vec<String> = strings(input.array_field("Actions"))
                    .into_iter()
                    .map(|a| {
                        if a == "*" || a.contains(':') {
                            a
                        } else {
                            format!("sqs:{a}")
                        }
                    })
                    .collect();
                let queue = queue_mut(self.region(ctx), &input)?;
                let mut policy: Value = queue
                    .attributes
                    .get("Policy")
                    .and_then(|p| serde_json::from_str(p).ok())
                    .unwrap_or_else(|| json!({ "Version": "2012-10-17", "Statement": [] }));
                let statements = policy["Statement"]
                    .as_array_mut()
                    .expect("policy statements");
                statements.retain(|s| s.str_field("Sid") != Some(&label));
                if operation == "AddPermission" {
                    statements.push(json!({ "Sid": label, "Effect": "Allow", "Principal": { "AWS": accounts }, "Action": actions, "Resource": arn }));
                }
                queue.attributes.insert("Policy".into(), policy.to_string());
                json!({})
            }
            "ListDeadLetterSourceQueues" => {
                let name = name_from_url(input.require_str("QueueUrl")?).to_string();
                let arn = Self::queue_arn(ctx, &name);
                let region = self.region(ctx);
                region.queues.get(&name).ok_or_else(no_queue)?;
                let urls = region
                    .queues
                    .values()
                    .filter(|q| q.redrive().is_some_and(|(_, a, _)| a == arn))
                    .map(|q| q.url.clone())
                    .collect();
                let (page, next) = paginate(urls, &input);
                let mut out = json!({ "queueUrls": page });
                if let Some(t) = next {
                    out["NextToken"] = json!(t);
                }
                out
            }
            "StartMessageMoveTask" => {
                let source_arn = input.require_str("SourceArn")?.to_string();
                let destination_arn = input.str_field("DestinationArn").map(str::to_string);
                let source = name_from_arn(&source_arn).to_string();
                let mut region = std::mem::take(self.region(ctx));
                let outcome = (|| {
                    let to_move = region.queues.get(&source).ok_or_else(|| err("ResourceNotFoundException", "The resource that you specified for the SourceArn parameter doesn't exist."))?.messages.len();
                    let destination = destination_arn
                        .as_deref()
                        .map(|a| name_from_arn(a).to_string())
                        .or_else(|| {
                            region
                                .queues
                                .values()
                                .find(|q| q.redrive().is_some_and(|(_, a, _)| a == source_arn))
                                .map(|q| q.name.clone())
                        });
                    let mut moved = 0;
                    if let Some(dest) = destination.filter(|d| region.queues.contains_key(d)) {
                        let messages = std::mem::take(
                            &mut region.queues.get_mut(&source).expect("source").messages,
                        );
                        moved = messages.len();
                        let requeued: Vec<Message> =
                            messages.iter().map(|m| requeue(ctx, m)).collect();
                        region
                            .queues
                            .get_mut(&dest)
                            .expect("destination")
                            .messages
                            .extend(requeued);
                    }
                    let handle = ctx.uuid();
                    region.move_tasks.insert(
                        0,
                        MoveTask {
                            handle: handle.clone(),
                            status: if moved > 0 { "COMPLETED" } else { "RUNNING" }.to_string(),
                            source_arn: source_arn.clone(),
                            destination_arn: destination_arn.clone(),
                            max_per_second: input.i64_field("MaxNumberOfMessagesPerSecond"),
                            moved,
                            to_move,
                            started_at: now,
                        },
                    );
                    Ok(json!({ "TaskHandle": handle }))
                })();
                *self.region(ctx) = region;
                outcome?
            }
            "ListMessageMoveTasks" => {
                let source_arn = input.require_str("SourceArn")?.to_string();
                let region = self.region(ctx);
                region
                    .queues
                    .get(name_from_arn(&source_arn))
                    .ok_or_else(|| err("ResourceNotFoundException", "The resource that you specified for the SourceArn parameter doesn't exist."))?;
                let limit = input
                    .i64_field("MaxResults")
                    .filter(|n| *n > 0)
                    .map_or(1, |n| n.min(10)) as usize;
                let results: Vec<Value> = region
                    .move_tasks
                    .iter()
                    .filter(|t| t.source_arn == source_arn)
                    .take(limit)
                    .map(|t| {
                        let mut v = json!({
                            "Status": t.status,
                            "SourceArn": t.source_arn,
                            "ApproximateNumberOfMessagesMoved": t.moved,
                            "ApproximateNumberOfMessagesToMove": t.to_move,
                            "StartedTimestamp": t.started_at as i64,
                        });
                        if t.status == "RUNNING" {
                            v["TaskHandle"] = json!(t.handle);
                        }
                        if let Some(d) = &t.destination_arn {
                            v["DestinationArn"] = json!(d);
                        }
                        if let Some(m) = t.max_per_second {
                            v["MaxNumberOfMessagesPerSecond"] = json!(m);
                        }
                        v
                    })
                    .collect();
                json!({ "Results": results })
            }
            "CancelMessageMoveTask" => {
                let handle = input.require_str("TaskHandle")?.to_string();
                let task = self
                    .region(ctx)
                    .move_tasks
                    .iter_mut()
                    .find(|t| t.handle == handle)
                    .ok_or_else(|| err("ResourceNotFoundException", "The resource that you specified for the TaskHandle parameter doesn't exist."))?;
                task.status = "CANCELLED".into();
                json!({ "ApproximateNumberOfMessagesMoved": task.moved })
            }
            other => {
                return Err(err(
                    "InvalidAction",
                    format!("The action {other} is not valid for this endpoint."),
                ));
            }
        })
    }
}
