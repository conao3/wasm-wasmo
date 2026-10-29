use std::collections::{BTreeMap, BTreeSet, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::pattern;
use crate::codec::AwsError;
use crate::codec::time::format_iso8601;
use crate::embedded_model;
use crate::service::{Arn, Ctx, Delivery, InputExt, OpResult, Service, ServiceModel};

const PAGE_SIZE: usize = 100;
const DEDUP_WINDOW_MS: f64 = 5.0 * 60.0 * 1000.0;
const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const PROTOCOLS: &[&str] = &[
    "http",
    "https",
    "email",
    "email-json",
    "sms",
    "sqs",
    "application",
    "lambda",
    "firehose",
];
const TOPIC_ATTRIBUTES: &[&str] = &[
    "DeliveryPolicy",
    "DisplayName",
    "Policy",
    "TracingConfig",
    "KmsMasterKeyId",
    "SignatureVersion",
    "ContentBasedDeduplication",
    "FifoTopic",
    "ArchivePolicy",
    "FifoThroughputScope",
    "DataProtectionPolicy",
];
const SUBSCRIPTION_ATTRIBUTES: &[&str] = &[
    "DeliveryPolicy",
    "FilterPolicy",
    "FilterPolicyScope",
    "RawMessageDelivery",
    "RedrivePolicy",
    "SubscriptionRoleArn",
    "ReplayPolicy",
];

#[derive(Debug, Clone)]
struct Topic {
    arn: String,
    attributes: BTreeMap<String, String>,
    tags: BTreeMap<String, String>,
    dedup: HashMap<String, (f64, Value)>,
    sequence: u64,
}

impl Topic {
    fn is_fifo(&self) -> bool {
        self.attributes
            .get("FifoTopic")
            .is_some_and(|v| v == "true")
    }

    fn content_dedup(&self) -> bool {
        self.attributes
            .get("ContentBasedDeduplication")
            .is_some_and(|v| v == "true")
    }
}

#[derive(Debug, Clone)]
struct Subscription {
    arn: String,
    topic_arn: String,
    protocol: String,
    endpoint: String,
    owner: String,
    attributes: BTreeMap<String, String>,
    token: Option<String>,
}

impl Subscription {
    fn pending(&self) -> bool {
        self.token.is_some()
    }

    fn summary(&self) -> Value {
        json!({
            "SubscriptionArn": if self.pending() { "PendingConfirmation" } else { &self.arn },
            "Owner": self.owner,
            "Protocol": self.protocol,
            "Endpoint": self.endpoint,
            "TopicArn": self.topic_arn,
        })
    }
}

#[derive(Debug, Clone)]
struct PlatformEndpoint {
    application: String,
    attributes: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct Region {
    topics: BTreeMap<String, Topic>,
    subscriptions: IndexMap<String, Subscription>,
    applications: BTreeMap<String, BTreeMap<String, String>>,
    endpoints: IndexMap<String, PlatformEndpoint>,
    sms_attributes: BTreeMap<String, String>,
    opted_out: BTreeSet<String>,
    sandbox: BTreeMap<String, String>,
}

#[derive(Default)]
pub struct Sns {
    regions: HashMap<(String, String), Region>,
}

struct Message {
    body: String,
    subject: Option<String>,
    structure_json: bool,
    attributes: Map<String, Value>,
    group_id: Option<String>,
    dedup_id: Option<String>,
}

fn invalid(message: impl Into<String>) -> AwsError {
    AwsError::new(400, "InvalidParameterException", message)
}

fn not_found(message: impl Into<String>) -> AwsError {
    AwsError::new(404, "NotFoundException", message)
}

fn topic_missing() -> AwsError {
    not_found("Topic does not exist")
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

fn tag_list(input: &Value) -> BTreeMap<String, String> {
    input
        .array_field("Tags")
        .iter()
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

fn page<T: Clone>(items: &[T], token: Option<&str>, size: usize) -> (Vec<T>, Option<String>) {
    let start = token
        .and_then(|t| STANDARD.decode(t).ok())
        .and_then(|b| String::from_utf8(b).ok())
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let end = (start + size).min(items.len());
    let next = (end < items.len()).then(|| STANDARD.encode(end.to_string()));
    (items[start..end].to_vec(), next)
}

fn name_of(arn: &str) -> &str {
    arn.rsplit(':').next().unwrap_or_default()
}

fn valid_topic_name(name: &str, fifo: bool) -> bool {
    let base = if fifo {
        match name.strip_suffix(".fifo") {
            Some(b) => b,
            None => return false,
        }
    } else {
        name
    };
    (1..=256).contains(&name.len())
        && !base.is_empty()
        && base
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn default_policy(topic_arn: &str, account: &str) -> String {
    json!({
        "Version": "2008-10-17",
        "Id": "__default_policy_ID",
        "Statement": [{
            "Sid": "__default_statement_ID",
            "Effect": "Allow",
            "Principal": {"AWS": "*"},
            "Action": [
                "SNS:GetTopicAttributes",
                "SNS:SetTopicAttributes",
                "SNS:AddPermission",
                "SNS:RemovePermission",
                "SNS:DeleteTopic",
                "SNS:Subscribe",
                "SNS:ListSubscriptionsByTopic",
                "SNS:Publish",
            ],
            "Resource": topic_arn,
            "Condition": {"StringEquals": {"AWS:SourceOwner": account}},
        }],
    })
    .to_string()
}

fn default_delivery_policy() -> String {
    json!({"http": {"defaultHealthyRetryPolicy": {
        "minDelayTarget": 20,
        "maxDelayTarget": 20,
        "numRetries": 3,
        "numMaxDelayRetries": 0,
        "numNoDelayRetries": 0,
        "numMinDelayRetries": 0,
        "backoffFunction": "linear",
    }, "disableSubscriptionOverrides": false, "defaultRequestPolicy": {"headerContentType": "text/plain; charset=UTF-8"}}})
    .to_string()
}

/// Message attributes as the JSON document a filter policy is matched against.
fn attribute_document(attributes: &Map<String, Value>) -> Value {
    let mut doc = Map::new();
    for (name, attr) in attributes {
        let data_type = attr.str_field("DataType").unwrap_or("String");
        let Some(s) = attr.get("StringValue").and_then(Value::as_str) else {
            continue;
        };
        let v = if data_type.starts_with("Number") {
            s.parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map_or_else(|| json!(s), Value::Number)
        } else if data_type == "String.Array" {
            serde_json::from_str::<Value>(s)
                .ok()
                .filter(Value::is_array)
                .unwrap_or_else(|| json!(s))
        } else {
            json!(s)
        };
        doc.insert(name.clone(), v);
    }
    Value::Object(doc)
}

fn filter_accepts(sub: &Subscription, message: &Message) -> bool {
    let Some(policy) = sub
        .attributes
        .get("FilterPolicy")
        .and_then(|p| serde_json::from_str::<Value>(p).ok())
    else {
        return true;
    };
    if sub.attributes.get("FilterPolicyScope").map(String::as_str) == Some("MessageBody") {
        serde_json::from_str::<Value>(&message.body)
            .ok()
            .filter(Value::is_object)
            .is_some_and(|body| pattern::matches(&policy, &body))
    } else {
        pattern::matches(&policy, &attribute_document(&message.attributes))
    }
}

fn envelope_attributes(attributes: &Map<String, Value>) -> Map<String, Value> {
    attributes
        .iter()
        .filter_map(|(name, attr)| {
            let value = attr
                .get("StringValue")
                .or_else(|| attr.get("BinaryValue"))?
                .as_str()?;
            Some((
                name.clone(),
                json!({"Type": attr.str_field("DataType").unwrap_or("String"), "Value": value}),
            ))
        })
        .collect()
}

fn protocol_message(message: &Message, protocol: &str) -> String {
    if !message.structure_json {
        return message.body.clone();
    }
    let doc: Value = serde_json::from_str(&message.body).unwrap_or_default();
    doc.str_field(protocol)
        .or_else(|| doc.str_field("default"))
        .unwrap_or_default()
        .to_string()
}

fn validate_attributes(attributes: &Map<String, Value>) -> Result<(), AwsError> {
    for (name, attr) in attributes {
        let data_type = attr.str_field("DataType").ok_or_else(|| {
            AwsError::new(
                400,
                "InvalidParameterValueException",
                format!(
                    "The message attribute '{name}' must contain non-empty message attribute type."
                ),
            )
        })?;
        let base = data_type.split('.').next().unwrap_or_default();
        if !matches!(base, "String" | "Number" | "Binary") {
            return Err(AwsError::new(
                400,
                "InvalidParameterValueException",
                format!(
                    "The message attribute '{name}' has an invalid message attribute type, the set of supported type prefixes is Binary, Number, and String."
                ),
            ));
        }
        let value = attr
            .get("StringValue")
            .or_else(|| attr.get("BinaryValue"))
            .and_then(Value::as_str);
        if value.is_none() {
            return Err(AwsError::new(
                400,
                "InvalidParameterValueException",
                format!(
                    "The message attribute '{name}' must contain non-empty message attribute value for message attribute type '{data_type}'."
                ),
            ));
        }
        if base == "Number" && value.and_then(|v| v.parse::<f64>().ok()).is_none() {
            return Err(AwsError::new(
                400,
                "InvalidParameterValueException",
                format!(
                    "An error occurred while trying to parse the value of the message attribute '{name}' as a number."
                ),
            ));
        }
    }
    Ok(())
}

fn validate_subscription_attribute(
    protocol: &str,
    name: &str,
    value: &str,
) -> Result<(), AwsError> {
    if !SUBSCRIPTION_ATTRIBUTES.contains(&name) {
        return Err(invalid("Invalid parameter: AttributeName"));
    }
    match name {
        "FilterPolicy" if !value.is_empty() => {
            pattern::parse(value)
                .map_err(|e| invalid(format!("Invalid parameter: FilterPolicy: {e}")))?;
        }
        "FilterPolicyScope" if !matches!(value, "MessageAttributes" | "MessageBody") => {
            return Err(invalid(format!(
                "Invalid parameter: FilterPolicyScope: Invalid value [{value}]. Please use either MessageBody or MessageAttributes"
            )));
        }
        "RawMessageDelivery" => {
            if !matches!(protocol, "sqs" | "http" | "https" | "firehose") {
                return Err(invalid(format!(
                    "Invalid parameter: Delivery protocol [{protocol}] does not support raw message delivery."
                )));
            }
            if !matches!(value.to_ascii_lowercase().as_str(), "true" | "false") {
                return Err(invalid("Invalid parameter: RawMessageDelivery"));
            }
        }
        "RedrivePolicy" if !value.is_empty() => {
            let policy: Value = serde_json::from_str(value)
                .map_err(|_| invalid("Invalid parameter: RedrivePolicy: failed to parse JSON."))?;
            let target = policy.str_field("deadLetterTargetArn").unwrap_or_default();
            if Arn::parse(target).is_none() {
                return Err(invalid(
                    "Invalid parameter: RedrivePolicy: deadLetterTargetArn is an invalid arn",
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_endpoint(protocol: &str, endpoint: &str) -> Result<(), AwsError> {
    let ok = match protocol {
        "http" => endpoint.starts_with("http://"),
        "https" => endpoint.starts_with("https://"),
        "sqs" | "lambda" | "firehose" | "application" => {
            Arn::parse(endpoint).is_some_and(|a| match protocol {
                "sqs" => a.service == "sqs",
                "lambda" => a.service == "lambda",
                "firehose" => a.service == "firehose",
                _ => a.service == "sns",
            })
        }
        "email" | "email-json" => endpoint.contains('@'),
        "sms" => valid_phone(endpoint),
        _ => true,
    };
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "Invalid parameter: {} Reason: Endpoint is invalid for protocol {protocol}",
            if protocol.starts_with("http") {
                "Endpoint"
            } else {
                "SQS endpoint ARN"
            }
        )))
    }
}

fn valid_phone(s: &str) -> bool {
    let digits = s.strip_prefix('+').unwrap_or(s);
    (8..=15).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit())
}

fn required_attribute<'a>(input: &'a Value, key: &str) -> Result<&'a str, AwsError> {
    input
        .str_field(key)
        .ok_or_else(|| invalid(format!("Invalid parameter: {key}")))
}

impl Region {
    fn topic(&self, arn: &str) -> Result<&Topic, AwsError> {
        self.topics
            .get(name_of(arn))
            .filter(|t| t.arn == arn)
            .ok_or_else(topic_missing)
    }

    fn topic_mut(&mut self, arn: &str) -> Result<&mut Topic, AwsError> {
        self.topics
            .get_mut(name_of(arn))
            .filter(|t| t.arn == arn)
            .ok_or_else(topic_missing)
    }

    fn subscription_mut(&mut self, arn: &str) -> Result<&mut Subscription, AwsError> {
        self.subscriptions
            .get_mut(arn)
            .ok_or_else(|| not_found("Subscription does not exist"))
    }
}

fn message_from(entry: &Value) -> Result<Message, AwsError> {
    let body = entry
        .get("Message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if body.is_empty() {
        return Err(invalid("Invalid parameter: Empty message"));
    }
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(invalid("Invalid parameter: Message too long"));
    }
    let structure_json = match entry.str_field("MessageStructure") {
        None => false,
        Some("json") => {
            let doc: Value = serde_json::from_str(body).map_err(|_| {
                invalid("Invalid parameter: Message Structure - JSON message body failed to parse")
            })?;
            if doc.str_field("default").is_none() {
                return Err(invalid(
                    "Invalid parameter: Message Structure - No default entry in JSON message body",
                ));
            }
            true
        }
        Some(_) => return Err(invalid("Invalid parameter: MessageStructure")),
    };
    let attributes = entry
        .obj_field("MessageAttributes")
        .cloned()
        .unwrap_or_default();
    validate_attributes(&attributes)?;
    if let Some(subject) = entry.str_field("Subject")
        && (subject.len() > 100 || subject.chars().any(|c| c.is_control()))
    {
        return Err(invalid("Invalid parameter: Subject"));
    }
    Ok(Message {
        body: body.to_string(),
        subject: entry.str_field("Subject").map(str::to_string),
        structure_json,
        attributes,
        group_id: entry.str_field("MessageGroupId").map(str::to_string),
        dedup_id: entry
            .str_field("MessageDeduplicationId")
            .map(str::to_string),
    })
}

/// Publishes to a topic and queues one delivery per matching subscription.
fn publish_to_topic(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    topic_arn: &str,
    mut message: Message,
) -> OpResult {
    let now = ctx.now_ms();
    let topic = region.topic_mut(topic_arn)?;
    let mut result = Map::new();
    if topic.is_fifo() {
        if message.group_id.is_none() {
            return Err(invalid(
                "Invalid parameter: The MessageGroupId parameter is required for FIFO topics",
            ));
        }
        if message.dedup_id.is_none() {
            if !topic.content_dedup() {
                return Err(invalid(
                    "Invalid parameter: The topic should either have ContentBasedDeduplication enabled or MessageDeduplicationId provided explicitly",
                ));
            }
            message.dedup_id = Some(
                Sha256::digest(message.body.as_bytes())
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
            );
        }
        topic
            .dedup
            .retain(|_, (at, _)| now - *at <= DEDUP_WINDOW_MS);
        if let Some((_, existing)) = message.dedup_id.as_ref().and_then(|d| topic.dedup.get(d)) {
            return Ok(existing.clone());
        }
        topic.sequence += 1;
        result.insert(
            "SequenceNumber".into(),
            json!(format!("{:020}", topic.sequence)),
        );
    } else if message.group_id.is_some() && message.dedup_id.is_some() {
        return Err(invalid(
            "Invalid parameter: MessageDeduplicationId Reason: The request includes MessageDeduplicationId parameter that is not valid for this topic type",
        ));
    }
    let message_id = ctx.uuid();
    result.insert("MessageId".into(), json!(message_id));
    let result = Value::Object(result);
    if let Some(d) = &message.dedup_id
        && topic.is_fifo()
    {
        topic.dedup.insert(d.clone(), (now, result.clone()));
    }
    let timestamp = format_iso8601(now.floor() / 1000.0);
    for sub in region.subscriptions.values() {
        if sub.topic_arn != topic_arn
            || sub.pending()
            || !matches!(sub.protocol.as_str(), "sqs" | "lambda" | "firehose")
            || !filter_accepts(sub, &message)
        {
            continue;
        }
        let text = protocol_message(&message, &sub.protocol);
        let raw = sub
            .attributes
            .get("RawMessageDelivery")
            .is_some_and(|v| v.eq_ignore_ascii_case("true"));
        let envelope = Envelope {
            topic_arn,
            subscription_arn: &sub.arn,
            message_id: &message_id,
            message: &message,
            text: &text,
            timestamp: &timestamp,
            sequence: result.get("SequenceNumber"),
        };
        let body = match sub.protocol.as_str() {
            "lambda" => json!({"Records": [{
                "EventSource": "aws:sns",
                "EventVersion": "1.0",
                "EventSubscriptionArn": sub.arn,
                "Sns": envelope.notification(ctx),
            }]})
            .to_string(),
            _ if raw => text.clone(),
            _ => envelope.notification(ctx).to_string(),
        };
        let dead_letter = sub
            .attributes
            .get("RedrivePolicy")
            .and_then(|p| serde_json::from_str::<Value>(p).ok())
            .and_then(|p| p.str_field("deadLetterTargetArn").map(str::to_string));
        ctx.deliver(Delivery {
            target: sub.endpoint.clone(),
            source: topic_arn.to_string(),
            body,
            subject: message.subject.clone(),
            attributes: if raw {
                message.attributes.clone()
            } else {
                Map::new()
            },
            group_id: message.group_id.clone(),
            dedup_id: message.dedup_id.clone(),
            dead_letter,
        });
    }
    Ok(result)
}

struct Envelope<'a> {
    topic_arn: &'a str,
    subscription_arn: &'a str,
    message_id: &'a str,
    message: &'a Message,
    text: &'a str,
    timestamp: &'a str,
    sequence: Option<&'a Value>,
}

impl Envelope<'_> {
    fn notification(&self, ctx: &Ctx<'_>) -> Value {
        let mut n = json!({
            "Type": "Notification",
            "MessageId": self.message_id,
            "TopicArn": self.topic_arn,
        });
        if let Some(s) = self.sequence {
            n["SequenceNumber"] = s.clone();
        }
        if let Some(s) = &self.message.subject {
            n["Subject"] = json!(s);
        }
        n["Message"] = json!(self.text);
        n["Timestamp"] = json!(self.timestamp);
        n["SignatureVersion"] = json!("1");
        n["Signature"] = json!(STANDARD.encode(Sha256::digest(self.text.as_bytes())));
        n["SigningCertURL"] = json!(format!(
            "{}/SimpleNotificationService-wasmo.pem",
            ctx.endpoint
        ));
        n["UnsubscribeURL"] = json!(format!(
            "{}/?Action=Unsubscribe&SubscriptionArn={}",
            ctx.endpoint, self.subscription_arn
        ));
        if !self.message.attributes.is_empty() {
            n["MessageAttributes"] = Value::Object(envelope_attributes(&self.message.attributes));
        }
        n
    }
}

impl Sns {
    fn region(&mut self, ctx: &Ctx<'_>) -> &mut Region {
        self.regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default()
    }
}

fn create_topic(ctx: &mut Ctx<'_>, region: &mut Region, input: &Value) -> OpResult {
    let name = input.require_str("Name")?;
    let mut attributes = string_map(input.obj_field("Attributes"));
    let fifo = attributes
        .get("FifoTopic")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    if !valid_topic_name(name, fifo) {
        return Err(invalid(if fifo || name.ends_with(".fifo") {
            "Invalid parameter: Fifo Topic names must end with .fifo and must be made up of only uppercase and lowercase ASCII letters, numbers, underscores, and hyphens, and must be between 1 and 256 characters long."
        } else {
            "Invalid parameter: Topic Name"
        }));
    }
    if let Some(k) = attributes
        .keys()
        .find(|k| !TOPIC_ATTRIBUTES.contains(&k.as_str()))
    {
        return Err(invalid(format!(
            "Invalid parameter: Attributes Reason: Unknown attribute {k}"
        )));
    }
    if fifo {
        attributes.insert("FifoTopic".into(), "true".into());
        attributes
            .entry("ContentBasedDeduplication".into())
            .or_insert_with(|| "false".into());
    } else {
        attributes.remove("FifoTopic");
    }
    let tags = tag_list(input);
    if let Some(existing) = region.topics.get_mut(name) {
        let differs = attributes
            .iter()
            .any(|(k, v)| existing.attributes.get(k) != Some(v));
        if differs {
            return Err(invalid(
                "Invalid parameter: Attributes Reason: Topic already exists with different attributes",
            ));
        }
        if !tags.is_empty() && tags != existing.tags {
            return Err(invalid(
                "Invalid parameter: Tags Reason: Topic already exists with different tags",
            ));
        }
        return Ok(json!({ "TopicArn": existing.arn }));
    }
    let arn = ctx.arn("sns", name);
    if !attributes.contains_key("Policy") {
        attributes.insert("Policy".into(), default_policy(&arn, &ctx.account));
    }
    region.topics.insert(
        name.to_string(),
        Topic {
            arn: arn.clone(),
            attributes,
            tags,
            dedup: HashMap::new(),
            sequence: 0,
        },
    );
    Ok(json!({ "TopicArn": arn }))
}

fn topic_attributes(ctx: &Ctx<'_>, region: &Region, arn: &str) -> OpResult {
    let topic = region.topic(arn)?;
    let (mut confirmed, mut pending) = (0, 0);
    for s in region.subscriptions.values().filter(|s| s.topic_arn == arn) {
        if s.pending() {
            pending += 1;
        } else {
            confirmed += 1;
        }
    }
    let mut attrs: BTreeMap<String, String> = BTreeMap::from([
        ("DisplayName".into(), String::new()),
        ("EffectiveDeliveryPolicy".into(), default_delivery_policy()),
        ("Owner".into(), ctx.account.clone()),
        ("TopicArn".into(), arn.to_string()),
        ("SubscriptionsConfirmed".into(), confirmed.to_string()),
        ("SubscriptionsPending".into(), pending.to_string()),
        ("SubscriptionsDeleted".into(), "0".into()),
    ]);
    attrs.extend(topic.attributes.clone());
    attrs.remove("DataProtectionPolicy");
    Ok(json!({ "Attributes": attrs }))
}

fn subscribe(ctx: &mut Ctx<'_>, region: &mut Region, input: &Value) -> OpResult {
    let topic_arn = input.require_str("TopicArn")?;
    let protocol = input.require_str("Protocol")?.to_ascii_lowercase();
    if !PROTOCOLS.contains(&protocol.as_str()) {
        return Err(invalid(
            "Invalid parameter: Amazon SNS does not support this protocol string",
        ));
    }
    let topic = region.topic(topic_arn)?;
    let endpoint = input.str_field("Endpoint").unwrap_or_default().to_string();
    validate_endpoint(&protocol, &endpoint)?;
    let fifo = topic.is_fifo();
    let attributes = string_map(input.obj_field("Attributes"));
    for (k, v) in &attributes {
        validate_subscription_attribute(&protocol, k, v)?;
    }
    if fifo && protocol == "sqs" && !endpoint.ends_with(".fifo") {
        return Err(invalid(
            "Invalid parameter: Invalid parameter: Endpoint Reason: FIFO SNS Topics currently only support FIFO SQS queues as endpoints",
        ));
    }
    if let Some(existing) = region
        .subscriptions
        .values_mut()
        .find(|s| s.topic_arn == topic_arn && s.protocol == protocol && s.endpoint == endpoint)
    {
        let conflicting = attributes
            .iter()
            .any(|(k, v)| existing.attributes.get(k).is_some_and(|e| e != v));
        if conflicting {
            return Err(invalid(
                "Invalid parameter: Attributes Reason: Subscription already exists with different attributes",
            ));
        }
        existing.attributes.extend(attributes);
        return Ok(json!({ "SubscriptionArn": existing.arn }));
    }
    let arn = format!("{topic_arn}:{}", ctx.uuid());
    let foreign_queue =
        protocol == "sqs" && Arn::parse(&endpoint).is_some_and(|a| a.account != ctx.account);
    let needs_confirmation =
        matches!(protocol.as_str(), "http" | "https" | "email" | "email-json") || foreign_queue;
    let token = needs_confirmation.then(|| {
        let mut b = [0u8; 32];
        ctx.random_bytes(&mut b);
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    });
    let mut stored = BTreeMap::from([
        ("RawMessageDelivery".to_string(), "false".to_string()),
        (
            "SubscriptionPrincipal".to_string(),
            format!("arn:aws:iam::{}:root", ctx.account),
        ),
    ]);
    if attributes.contains_key("FilterPolicy") {
        stored.insert("FilterPolicyScope".into(), "MessageAttributes".into());
    }
    stored.extend(attributes);
    if let Some(token) = &token
        && protocol == "sqs"
    {
        let body = json!({
            "Type": "SubscriptionConfirmation",
            "MessageId": ctx.uuid(),
            "Token": token,
            "TopicArn": topic_arn,
            "Message": format!("You have chosen to subscribe to the topic {topic_arn}.\nTo confirm the subscription, visit the SubscribeURL included in this message."),
            "SubscribeURL": format!("{}/?Action=ConfirmSubscription&TopicArn={topic_arn}&Token={token}", ctx.endpoint),
            "Timestamp": format_iso8601((ctx.now_ms() / 1000.0).floor()),
            "SignatureVersion": "1",
        });
        ctx.deliver(Delivery {
            target: endpoint.clone(),
            source: topic_arn.to_string(),
            body: body.to_string(),
            ..Delivery::default()
        });
    }
    region.subscriptions.insert(
        arn.clone(),
        Subscription {
            arn: arn.clone(),
            topic_arn: topic_arn.to_string(),
            protocol,
            endpoint,
            owner: ctx.account.clone(),
            attributes: stored,
            token,
        },
    );
    let return_arn = input.get("ReturnSubscriptionArn").and_then(Value::as_bool) == Some(true);
    Ok(json!({
        "SubscriptionArn": if needs_confirmation && !return_arn { "pending confirmation".to_string() } else { arn },
    }))
}

fn subscription_attributes(region: &mut Region, arn: &str) -> OpResult {
    let sub = region.subscription_mut(arn)?;
    let mut attrs = sub.attributes.clone();
    attrs.extend([
        ("SubscriptionArn".to_string(), sub.arn.clone()),
        ("TopicArn".to_string(), sub.topic_arn.clone()),
        ("Protocol".to_string(), sub.protocol.clone()),
        ("Endpoint".to_string(), sub.endpoint.clone()),
        ("Owner".to_string(), sub.owner.clone()),
        ("PendingConfirmation".to_string(), sub.pending().to_string()),
        (
            "ConfirmationWasAuthenticated".to_string(),
            (!sub.pending()).to_string(),
        ),
    ]);
    if matches!(sub.protocol.as_str(), "http" | "https") {
        attrs.insert("EffectiveDeliveryPolicy".into(), default_delivery_policy());
    }
    if let Some(token) = &sub.token {
        attrs.insert("PendingToken".into(), token.clone());
    }
    Ok(json!({ "Attributes": attrs }))
}

fn confirm(region: &mut Region, input: &Value) -> OpResult {
    let topic_arn = input.require_str("TopicArn")?;
    let token = input.require_str("Token")?;
    region.topic(topic_arn)?;
    let sub = region
        .subscriptions
        .values_mut()
        .find(|s| s.topic_arn == topic_arn && s.token.as_deref() == Some(token))
        .ok_or_else(|| invalid("Invalid token"))?;
    sub.token = None;
    Ok(json!({ "SubscriptionArn": sub.arn }))
}

fn publish(ctx: &mut Ctx<'_>, region: &mut Region, input: &Value) -> OpResult {
    let targets = ["TopicArn", "TargetArn", "PhoneNumber"]
        .iter()
        .filter(|k| input.str_field(k).is_some())
        .count();
    if targets != 1 {
        return Err(invalid(
            "Invalid parameter: TopicArn or TargetArn Reason: Only one of TopicArn, TargetArn or PhoneNumber may be specified",
        ));
    }
    let message = message_from(input)?;
    if let Some(phone) = input.str_field("PhoneNumber") {
        if !valid_phone(phone) {
            return Err(invalid(format!(
                "Invalid parameter: PhoneNumber Reason: {phone} is not valid to publish to"
            )));
        }
        if region.opted_out.contains(phone) {
            return Err(AwsError::new(
                400,
                "OptedOutException",
                "Phone number is opted out",
            ));
        }
        return Ok(json!({ "MessageId": ctx.uuid() }));
    }
    let arn = input
        .str_field("TopicArn")
        .or_else(|| input.str_field("TargetArn"))
        .unwrap_or_default()
        .to_string();
    if let Some(endpoint) = region.endpoints.get(&arn) {
        if endpoint
            .attributes
            .get("Enabled")
            .is_some_and(|v| v == "false")
        {
            return Err(AwsError::new(
                400,
                "EndpointDisabledException",
                "Endpoint is disabled",
            ));
        }
        return Ok(json!({ "MessageId": ctx.uuid() }));
    }
    if Arn::parse(&arn).is_none() {
        return Err(invalid("Invalid parameter: TopicArn"));
    }
    if input.str_field("TargetArn").is_some() && !region.topics.contains_key(name_of(&arn)) {
        return Err(not_found("Endpoint does not exist"));
    }
    publish_to_topic(ctx, region, &arn, message)
}

fn publish_batch(ctx: &mut Ctx<'_>, region: &mut Region, input: &Value) -> OpResult {
    let topic_arn = input.require_str("TopicArn")?.to_string();
    region.topic(&topic_arn)?;
    let entries = input.array_field("PublishBatchRequestEntries");
    if entries.is_empty() {
        return Err(AwsError::new(
            400,
            "EmptyBatchRequestException",
            "The batch request doesn't contain any entries",
        ));
    }
    if entries.len() > 10 {
        return Err(AwsError::new(
            400,
            "TooManyEntriesInBatchRequestException",
            "The batch request contains more entries than permissible",
        ));
    }
    let mut ids = BTreeSet::new();
    for e in entries {
        let id = e.str_field("Id").unwrap_or_default();
        if id.is_empty()
            || id.len() > 80
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(AwsError::new(
                400,
                "InvalidBatchEntryIdException",
                "The Id of a batch entry in a batch request doesn't abide by the specification.",
            ));
        }
        if !ids.insert(id) {
            return Err(AwsError::new(
                400,
                "BatchEntryIdsNotDistinctException",
                "Two or more batch entries in the request have the same Id.",
            ));
        }
    }
    let total: usize = entries
        .iter()
        .map(|e| e.str_field("Message").map_or(0, str::len))
        .sum();
    if total > MAX_MESSAGE_BYTES {
        return Err(AwsError::new(
            400,
            "BatchRequestTooLongException",
            "The length of all the messages put together is more than the limit.",
        ));
    }
    let (mut successful, mut failed) = (Vec::new(), Vec::new());
    for e in entries {
        let id = e.str_field("Id").unwrap_or_default();
        match message_from(e).and_then(|m| publish_to_topic(ctx, region, &topic_arn, m)) {
            Ok(mut r) => {
                r["Id"] = json!(id);
                successful.push(r);
            }
            Err(err) => failed.push(json!({
                "Id": id,
                "Code": err.code.trim_end_matches("Exception"),
                "Message": err.message,
                "SenderFault": err.sender_fault,
            })),
        }
    }
    Ok(json!({ "Successful": successful, "Failed": failed }))
}

fn set_policy(topic: &mut Topic, edit: impl FnOnce(&mut Vec<Value>)) {
    let mut policy: Value = topic
        .attributes
        .get("Policy")
        .and_then(|p| serde_json::from_str(p).ok())
        .unwrap_or_else(|| json!({"Version": "2008-10-17"}));
    let mut statements = policy
        .get("Statement")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    edit(&mut statements);
    policy["Statement"] = Value::Array(statements);
    topic.attributes.insert("Policy".into(), policy.to_string());
}

fn platform_op(ctx: &mut Ctx<'_>, region: &mut Region, operation: &str, input: &Value) -> OpResult {
    let app_missing = || not_found("PlatformApplication does not exist");
    let endpoint_missing = || not_found("Endpoint does not exist");
    Ok(match operation {
        "CreatePlatformApplication" => {
            let name = input.require_str("Name")?;
            let platform = input.require_str("Platform")?;
            let arn = ctx.arn("sns", &format!("app/{platform}/{name}"));
            region
                .applications
                .insert(arn.clone(), string_map(input.obj_field("Attributes")));
            json!({ "PlatformApplicationArn": arn })
        }
        "GetPlatformApplicationAttributes" => {
            let arn = input.require_str("PlatformApplicationArn")?;
            let attrs = region.applications.get(arn).ok_or_else(app_missing)?;
            let mut out = attrs.clone();
            out.retain(|k, _| !matches!(k.as_str(), "PlatformCredential" | "PlatformPrincipal"));
            out.entry("Enabled".into()).or_insert_with(|| "true".into());
            json!({ "Attributes": out })
        }
        "SetPlatformApplicationAttributes" => {
            let arn = input.require_str("PlatformApplicationArn")?;
            let attrs = region.applications.get_mut(arn).ok_or_else(app_missing)?;
            attrs.extend(string_map(input.obj_field("Attributes")));
            json!({})
        }
        "ListPlatformApplications" => {
            let apps: Vec<Value> = region
                .applications
                .iter()
                .map(|(arn, attrs)| json!({"PlatformApplicationArn": arn, "Attributes": attrs}))
                .collect();
            let (items, next) = page(&apps, input.str_field("NextToken"), PAGE_SIZE);
            let mut out = json!({ "PlatformApplications": items });
            if let Some(n) = next {
                out["NextToken"] = json!(n);
            }
            out
        }
        "DeletePlatformApplication" => {
            let arn = input.require_str("PlatformApplicationArn")?;
            region.applications.remove(arn);
            region.endpoints.retain(|_, e| e.application != arn);
            json!({})
        }
        "CreatePlatformEndpoint" => {
            let app = input.require_str("PlatformApplicationArn")?;
            let token = input.require_str("Token")?;
            region.applications.get(app).ok_or_else(app_missing)?;
            let mut attrs = BTreeMap::from([
                ("Token".to_string(), token.to_string()),
                ("Enabled".to_string(), "true".to_string()),
            ]);
            if let Some(data) = input.str_field("CustomUserData") {
                attrs.insert("CustomUserData".into(), data.to_string());
            }
            attrs.extend(string_map(input.obj_field("Attributes")));
            if let Some((arn, existing)) = region.endpoints.iter().find(|(_, e)| {
                e.application == app && e.attributes.get("Token").map(String::as_str) == Some(token)
            }) {
                if existing.attributes != attrs {
                    return Err(invalid(format!(
                        "Invalid parameter: Token Reason: Endpoint {arn} already exists with the same Token, but different attributes."
                    )));
                }
                return Ok(json!({ "EndpointArn": arn }));
            }
            let arn = format!("{}/{}", app.replacen(":app/", ":endpoint/", 1), ctx.uuid());
            region.endpoints.insert(
                arn.clone(),
                PlatformEndpoint {
                    application: app.to_string(),
                    attributes: attrs,
                },
            );
            json!({ "EndpointArn": arn })
        }
        "GetEndpointAttributes" => {
            let arn = input.require_str("EndpointArn")?;
            let e = region.endpoints.get(arn).ok_or_else(endpoint_missing)?;
            json!({ "Attributes": e.attributes })
        }
        "SetEndpointAttributes" => {
            let arn = input.require_str("EndpointArn")?;
            let e = region.endpoints.get_mut(arn).ok_or_else(endpoint_missing)?;
            e.attributes
                .extend(string_map(input.obj_field("Attributes")));
            json!({})
        }
        "DeleteEndpoint" => {
            region
                .endpoints
                .shift_remove(input.require_str("EndpointArn")?);
            json!({})
        }
        "ListEndpointsByPlatformApplication" => {
            let app = input.require_str("PlatformApplicationArn")?;
            region.applications.get(app).ok_or_else(app_missing)?;
            let endpoints: Vec<Value> = region
                .endpoints
                .iter()
                .filter(|(_, e)| e.application == app)
                .map(|(arn, e)| json!({"EndpointArn": arn, "Attributes": e.attributes}))
                .collect();
            let (items, next) = page(&endpoints, input.str_field("NextToken"), PAGE_SIZE);
            let mut out = json!({ "Endpoints": items });
            if let Some(n) = next {
                out["NextToken"] = json!(n);
            }
            out
        }
        _ => unreachable!("platform_op called with {operation}"),
    })
}

fn sms_op(region: &mut Region, operation: &str, input: &Value) -> OpResult {
    Ok(match operation {
        "GetSMSAttributes" => {
            let wanted: Vec<&str> = input
                .array_field("attributes")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let attrs: BTreeMap<&String, &String> = region
                .sms_attributes
                .iter()
                .filter(|(k, _)| wanted.is_empty() || wanted.contains(&k.as_str()))
                .collect();
            json!({ "attributes": attrs })
        }
        "SetSMSAttributes" => {
            region
                .sms_attributes
                .extend(string_map(input.obj_field("attributes")));
            json!({})
        }
        "CheckIfPhoneNumberIsOptedOut" => {
            let phone = input.require_str("phoneNumber")?;
            json!({ "isOptedOut": region.opted_out.contains(phone) })
        }
        "OptInPhoneNumber" => {
            region.opted_out.remove(input.require_str("phoneNumber")?);
            json!({})
        }
        "ListPhoneNumbersOptedOut" => {
            let all: Vec<String> = region.opted_out.iter().cloned().collect();
            let (items, next) = page(&all, input.str_field("nextToken"), PAGE_SIZE);
            let mut out = json!({ "phoneNumbers": items });
            if let Some(n) = next {
                out["nextToken"] = json!(n);
            }
            out
        }
        "ListOriginationNumbers" => json!({ "PhoneNumbers": [] }),
        "GetSMSSandboxAccountStatus" => json!({ "IsInSandbox": true }),
        "CreateSMSSandboxPhoneNumber" => {
            let phone = input.require_str("PhoneNumber")?;
            if !valid_phone(phone) {
                return Err(invalid("Invalid parameter: PhoneNumber"));
            }
            region
                .sandbox
                .entry(phone.to_string())
                .or_insert_with(|| "Pending".into());
            json!({})
        }
        "VerifySMSSandboxPhoneNumber" => {
            let phone = input.require_str("PhoneNumber")?;
            input.require_str("OneTimePassword")?;
            let status = region.sandbox.get_mut(phone).ok_or_else(|| {
                AwsError::new(
                    404,
                    "ResourceNotFoundException",
                    "Phone number does not exist",
                )
            })?;
            *status = "Verified".into();
            json!({})
        }
        "DeleteSMSSandboxPhoneNumber" => {
            let phone = input.require_str("PhoneNumber")?;
            region.sandbox.remove(phone).ok_or_else(|| {
                AwsError::new(
                    404,
                    "ResourceNotFoundException",
                    "Phone number does not exist",
                )
            })?;
            json!({})
        }
        "ListSMSSandboxPhoneNumbers" => {
            let all: Vec<Value> = region
                .sandbox
                .iter()
                .map(|(p, s)| json!({"PhoneNumber": p, "Status": s}))
                .collect();
            let size = input
                .i64_field("MaxResults")
                .filter(|n| *n > 0)
                .map_or(PAGE_SIZE, |n| n as usize);
            let (items, next) = page(&all, input.str_field("NextToken"), size);
            let mut out = json!({ "PhoneNumbers": items });
            if let Some(n) = next {
                out["NextToken"] = json!(n);
            }
            out
        }
        _ => unreachable!("sms_op called with {operation}"),
    })
}

fn tag_op(region: &mut Region, operation: &str, input: &Value) -> OpResult {
    let arn = input.require_str("ResourceArn")?;
    let topic = region
        .topics
        .get_mut(name_of(arn))
        .filter(|t| t.arn == arn)
        .ok_or_else(|| {
            AwsError::new(404, "ResourceNotFoundException", "Resource does not exist")
        })?;
    Ok(match operation {
        "TagResource" => {
            let tags = tag_list(input);
            let mut merged = topic.tags.clone();
            merged.extend(tags);
            if merged.len() > 50 {
                return Err(AwsError::new(
                    400,
                    "TagLimitExceededException",
                    "Could not complete request: tag quota of per resource exceeded",
                ));
            }
            topic.tags = merged;
            json!({})
        }
        "UntagResource" => {
            for k in input
                .array_field("TagKeys")
                .iter()
                .filter_map(Value::as_str)
            {
                topic.tags.remove(k);
            }
            json!({})
        }
        _ => {
            let tags: Vec<Value> = topic
                .tags
                .iter()
                .map(|(k, v)| json!({"Key": k, "Value": v}))
                .collect();
            json!({ "Tags": tags })
        }
    })
}

fn topic_op(ctx: &mut Ctx<'_>, region: &mut Region, operation: &str, input: &Value) -> OpResult {
    Ok(match operation {
        "CreateTopic" => return create_topic(ctx, region, input),
        "DeleteTopic" => {
            let arn = input.require_str("TopicArn")?;
            if region.topic(arn).is_ok() {
                region.topics.remove(name_of(arn));
                region.subscriptions.retain(|_, s| s.topic_arn != arn);
            }
            json!({})
        }
        "ListTopics" => {
            let all: Vec<Value> = region
                .topics
                .values()
                .map(|t| json!({ "TopicArn": t.arn }))
                .collect();
            let (items, next) = page(&all, input.str_field("NextToken"), PAGE_SIZE);
            let mut out = json!({ "Topics": items });
            if let Some(n) = next {
                out["NextToken"] = json!(n);
            }
            out
        }
        "GetTopicAttributes" => {
            return topic_attributes(ctx, region, input.require_str("TopicArn")?);
        }
        "SetTopicAttributes" => {
            let name = required_attribute(input, "AttributeName")?;
            let value = input
                .get("AttributeValue")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let topic = region.topic_mut(input.require_str("TopicArn")?)?;
            if !TOPIC_ATTRIBUTES.contains(&name) {
                return Err(invalid("Invalid parameter: AttributeName"));
            }
            if name == "FifoTopic" {
                return Err(invalid(
                    "Invalid parameter: AttributeName Reason: FifoTopic cannot be changed",
                ));
            }
            if name == "ContentBasedDeduplication" && !topic.is_fifo() {
                return Err(invalid(
                    "Invalid parameter: ContentBasedDeduplication Reason: Content-based deduplication can only be set for FIFO topics",
                ));
            }
            if name == "Policy" && serde_json::from_str::<Value>(value).is_err() {
                return Err(invalid("Invalid parameter: Policy Error: null"));
            }
            topic.attributes.insert(name.to_string(), value.to_string());
            json!({})
        }
        "AddPermission" => {
            let arn = input.require_str("TopicArn")?.to_string();
            let label = input.require_str("Label")?.to_string();
            let accounts: Vec<Value> = input.array_field("AWSAccountId").to_vec();
            let actions: Vec<Value> = input
                .array_field("ActionName")
                .iter()
                .filter_map(Value::as_str)
                .map(|a| json!(format!("SNS:{a}")))
                .collect();
            if accounts.is_empty() || actions.is_empty() {
                return Err(invalid(
                    "Invalid parameter: AWSAccountId and ActionName are required",
                ));
            }
            let topic = region.topic_mut(&arn)?;
            let mut duplicate = false;
            set_policy(topic, |statements| {
                duplicate = statements
                    .iter()
                    .any(|s| s.str_field("Sid") == Some(&label));
                if !duplicate {
                    statements.push(json!({
                        "Sid": label,
                        "Effect": "Allow",
                        "Principal": {"AWS": accounts.iter().filter_map(Value::as_str).map(|a| format!("arn:aws:iam::{a}:root")).collect::<Vec<_>>()},
                        "Action": actions,
                        "Resource": arn,
                    }));
                }
            });
            if duplicate {
                return Err(invalid("Invalid parameter: Statement already exists"));
            }
            json!({})
        }
        "RemovePermission" => {
            let label = input.require_str("Label")?.to_string();
            let topic = region.topic_mut(input.require_str("TopicArn")?)?;
            set_policy(topic, |statements| {
                statements.retain(|s| s.str_field("Sid") != Some(&label));
            });
            json!({})
        }
        "GetDataProtectionPolicy" => {
            let topic = region.topic(input.require_str("ResourceArn")?)?;
            json!({ "DataProtectionPolicy": topic.attributes.get("DataProtectionPolicy").cloned().unwrap_or_default() })
        }
        "PutDataProtectionPolicy" => {
            let policy = input.require_str("DataProtectionPolicy")?.to_string();
            serde_json::from_str::<Value>(&policy)
                .map_err(|_| invalid("Invalid parameter: DataProtectionPolicy"))?;
            let topic = region.topic_mut(input.require_str("ResourceArn")?)?;
            topic
                .attributes
                .insert("DataProtectionPolicy".into(), policy);
            json!({})
        }
        _ => unreachable!("topic_op called with {operation}"),
    })
}

fn subscription_op(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> OpResult {
    Ok(match operation {
        "Subscribe" => return subscribe(ctx, region, input),
        "ConfirmSubscription" => return confirm(region, input),
        "Unsubscribe" => {
            let arn = input.require_str("SubscriptionArn")?;
            region.subscriptions.shift_remove(arn);
            json!({})
        }
        "ListSubscriptions" | "ListSubscriptionsByTopic" => {
            let topic = if operation == "ListSubscriptionsByTopic" {
                let arn = input.require_str("TopicArn")?;
                region.topic(arn)?;
                Some(arn)
            } else {
                None
            };
            let all: Vec<Value> = region
                .subscriptions
                .values()
                .filter(|s| topic.is_none_or(|t| s.topic_arn == t))
                .map(Subscription::summary)
                .collect();
            let (items, next) = page(&all, input.str_field("NextToken"), PAGE_SIZE);
            let mut out = json!({ "Subscriptions": items });
            if let Some(n) = next {
                out["NextToken"] = json!(n);
            }
            out
        }
        "GetSubscriptionAttributes" => {
            return subscription_attributes(region, input.require_str("SubscriptionArn")?);
        }
        "SetSubscriptionAttributes" => {
            let name = required_attribute(input, "AttributeName")?;
            let value = input
                .get("AttributeValue")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let sub = region.subscription_mut(input.require_str("SubscriptionArn")?)?;
            validate_subscription_attribute(&sub.protocol, name, value)?;
            if value.is_empty() && matches!(name, "FilterPolicy" | "RedrivePolicy") {
                sub.attributes.remove(name);
                if name == "FilterPolicy" {
                    sub.attributes.remove("FilterPolicyScope");
                }
            } else {
                if name == "FilterPolicy" {
                    sub.attributes
                        .entry("FilterPolicyScope".into())
                        .or_insert_with(|| "MessageAttributes".into());
                }
                sub.attributes.insert(name.to_string(), value.to_string());
            }
            json!({})
        }
        _ => unreachable!("subscription_op called with {operation}"),
    })
}

impl Service for Sns {
    fn signing_name(&self) -> &'static str {
        "sns"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("sns")
    }

    fn deliver(&mut self, ctx: &mut Ctx<'_>, resource: &str, delivery: &Delivery) -> bool {
        let arn = ctx.arn("sns", resource);
        let region = self.region(ctx);
        let message = Message {
            body: delivery.body.clone(),
            subject: delivery.subject.clone(),
            structure_json: false,
            attributes: delivery.attributes.clone(),
            group_id: delivery.group_id.clone(),
            dedup_id: delivery.dedup_id.clone(),
        };
        publish_to_topic(ctx, region, &arn, message).is_ok()
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let mut region = std::mem::take(self.region(ctx));
        let result = match operation {
            "Publish" => publish(ctx, &mut region, &input),
            "PublishBatch" => publish_batch(ctx, &mut region, &input),
            "Subscribe"
            | "ConfirmSubscription"
            | "Unsubscribe"
            | "ListSubscriptions"
            | "ListSubscriptionsByTopic"
            | "GetSubscriptionAttributes"
            | "SetSubscriptionAttributes" => subscription_op(ctx, &mut region, operation, &input),
            "TagResource" | "UntagResource" | "ListTagsForResource" => {
                tag_op(&mut region, operation, &input)
            }
            "CreatePlatformApplication"
            | "GetPlatformApplicationAttributes"
            | "SetPlatformApplicationAttributes"
            | "ListPlatformApplications"
            | "DeletePlatformApplication"
            | "CreatePlatformEndpoint"
            | "GetEndpointAttributes"
            | "SetEndpointAttributes"
            | "DeleteEndpoint"
            | "ListEndpointsByPlatformApplication" => {
                platform_op(ctx, &mut region, operation, &input)
            }
            "GetSMSAttributes"
            | "SetSMSAttributes"
            | "CheckIfPhoneNumberIsOptedOut"
            | "OptInPhoneNumber"
            | "ListPhoneNumbersOptedOut"
            | "ListOriginationNumbers"
            | "GetSMSSandboxAccountStatus"
            | "CreateSMSSandboxPhoneNumber"
            | "VerifySMSSandboxPhoneNumber"
            | "DeleteSMSSandboxPhoneNumber"
            | "ListSMSSandboxPhoneNumbers" => sms_op(&mut region, operation, &input),
            "CreateTopic"
            | "DeleteTopic"
            | "ListTopics"
            | "GetTopicAttributes"
            | "SetTopicAttributes"
            | "AddPermission"
            | "RemovePermission"
            | "GetDataProtectionPolicy"
            | "PutDataProtectionPolicy" => topic_op(ctx, &mut region, operation, &input),
            other => Err(AwsError::new(
                400,
                "InvalidAction",
                format!("The action {other} is not valid for this endpoint."),
            )),
        };
        *self.region(ctx) = region;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(policy: &str, scope: Option<&str>) -> Subscription {
        let mut attributes = BTreeMap::from([("FilterPolicy".to_string(), policy.to_string())]);
        if let Some(s) = scope {
            attributes.insert("FilterPolicyScope".into(), s.into());
        }
        Subscription {
            arn: "arn:aws:sns:us-east-1:000000000000:t:1".into(),
            topic_arn: "arn:aws:sns:us-east-1:000000000000:t".into(),
            protocol: "sqs".into(),
            endpoint: "arn:aws:sqs:us-east-1:000000000000:q".into(),
            owner: "000000000000".into(),
            attributes,
            token: None,
        }
    }

    fn msg(body: &str, attrs: Value) -> Message {
        Message {
            body: body.into(),
            subject: None,
            structure_json: false,
            attributes: attrs.as_object().cloned().unwrap_or_default(),
            group_id: None,
            dedup_id: None,
        }
    }

    #[test]
    fn filter_policy_on_attributes() {
        let s = sub(
            r#"{"color": ["red"], "price": [{"numeric": [">=", 10]}]}"#,
            None,
        );
        let attrs = |color: &str, price: &str| {
            json!({
                "color": {"DataType": "String", "StringValue": color},
                "price": {"DataType": "Number", "StringValue": price},
            })
        };
        assert!(filter_accepts(&s, &msg("x", attrs("red", "10"))));
        assert!(!filter_accepts(&s, &msg("x", attrs("red", "9.5"))));
        assert!(!filter_accepts(&s, &msg("x", attrs("blue", "20"))));
        assert!(!filter_accepts(&s, &msg("x", json!({}))));
        let arr = sub(r#"{"tags": ["b"]}"#, None);
        assert!(filter_accepts(
            &arr,
            &msg(
                "x",
                json!({"tags": {"DataType": "String.Array", "StringValue": "[\"a\",\"b\"]"}})
            )
        ));
    }

    #[test]
    fn filter_policy_on_body() {
        let s = sub(
            r#"{"order": {"kind": [{"prefix": "big"}]}}"#,
            Some("MessageBody"),
        );
        assert!(filter_accepts(
            &s,
            &msg(r#"{"order": {"kind": "bigone"}}"#, json!({}))
        ));
        assert!(!filter_accepts(
            &s,
            &msg(r#"{"order": {"kind": "small"}}"#, json!({}))
        ));
        assert!(!filter_accepts(&s, &msg("not json", json!({}))));
    }

    #[test]
    fn json_structure_selects_protocol() {
        let mut m = msg(r#"{"default": "d", "sqs": "q"}"#, json!({}));
        m.structure_json = true;
        assert_eq!(protocol_message(&m, "sqs"), "q");
        assert_eq!(protocol_message(&m, "lambda"), "d");
    }
}
