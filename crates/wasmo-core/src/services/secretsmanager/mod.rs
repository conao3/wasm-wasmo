use std::collections::{BTreeMap, HashMap};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Ctx, InputExt, OpResult, Service, ServiceModel};

mod ops;
mod password;
#[cfg(test)]
mod tests;

use password::{arn_suffix, random_password};

const CURRENT: &str = "AWSCURRENT";
const PREVIOUS: &str = "AWSPREVIOUS";
const PENDING: &str = "AWSPENDING";
const DAY: f64 = 86400.0;
const DELETED_STATE: &str =
    "You can't perform this operation on the secret because it was marked for deletion.";

#[derive(Debug, Clone, PartialEq)]
enum Payload {
    String(String),
    Binary(String),
}

#[derive(Debug, Clone)]
struct Version {
    id: String,
    payload: Payload,
    stages: Vec<String>,
    created: f64,
    accessed: Option<f64>,
}

#[derive(Debug, Clone)]
struct Replica {
    region: String,
    kms_key_id: Option<String>,
}

#[derive(Debug, Clone)]
struct Secret {
    arn: String,
    name: String,
    primary_region: String,
    description: Option<String>,
    kms_key_id: Option<String>,
    secret_type: Option<String>,
    tags: Vec<(String, String)>,
    created: f64,
    changed: f64,
    accessed: Option<f64>,
    deleted: Option<f64>,
    versions: Vec<Version>,
    rotation_enabled: bool,
    rotation_lambda: Option<String>,
    rotation_rules: Option<Value>,
    last_rotated: Option<f64>,
    policy: Option<String>,
    replicas: Vec<Replica>,
    seq: u64,
}

impl Secret {
    fn position(&self, id: &str) -> Option<usize> {
        self.versions.iter().position(|v| v.id == id)
    }

    fn holder(&self, stage: &str) -> Option<usize> {
        self.versions
            .iter()
            .position(|v| v.stages.iter().any(|s| s == stage))
    }

    fn detach(&mut self, stage: &str) {
        for v in &mut self.versions {
            v.stages.retain(|s| s != stage);
        }
    }

    fn attach(&mut self, stage: &str, to: usize) {
        let from = self.holder(stage);
        self.detach(stage);
        self.versions[to].stages.push(stage.to_string());
        if stage == CURRENT
            && let Some(from) = from.filter(|f| *f != to)
        {
            self.detach(PREVIOUS);
            self.versions[from].stages.push(PREVIOUS.to_string());
        }
    }

    fn add_version(
        &mut self,
        id: String,
        payload: Payload,
        stages: &[String],
        now: f64,
    ) -> Result<Vec<String>, AwsError> {
        if let Some(v) = self.versions.iter().find(|v| v.id == id) {
            if v.payload == payload {
                return Ok(v.stages.clone());
            }
            return Err(exists(
                "A resource with the ID you requested already exists.",
            ));
        }
        self.versions.push(Version {
            id,
            payload,
            stages: Vec::new(),
            created: now,
            accessed: None,
        });
        let idx = self.versions.len() - 1;
        for stage in stages {
            self.attach(stage, idx);
        }
        self.changed = now;
        Ok(self.versions[idx].stages.clone())
    }

    fn ensure_active(&self) -> Result<(), AwsError> {
        match self.deleted {
            Some(_) => Err(invalid_request(DELETED_STATE)),
            None => Ok(()),
        }
    }

    fn next_rotation(&self) -> Option<f64> {
        let days = self
            .rotation_rules
            .as_ref()?
            .i64_field("AutomaticallyAfterDays")?;
        self.rotation_enabled
            .then(|| self.last_rotated.unwrap_or(self.created) + days as f64 * DAY)
    }

    fn tags_json(&self) -> Value {
        Value::Array(
            self.tags
                .iter()
                .map(|(k, v)| json!({ "Key": k, "Value": v }))
                .collect(),
        )
    }

    fn replication_json(&self) -> Value {
        Value::Array(
            self.replicas
                .iter()
                .map(|r| {
                    json!({
                        "Region": r.region,
                        "KmsKeyId": r.kms_key_id,
                        "Status": "InSync",
                        "StatusMessage": "Replication succeeded",
                    })
                })
                .collect(),
        )
    }

    fn describe(&self, list: bool) -> Value {
        let stages: Map<String, Value> = self
            .versions
            .iter()
            .filter(|v| !v.stages.is_empty())
            .map(|v| (v.id.clone(), json!(v.stages)))
            .collect();
        let mut out = json!({
            "ARN": self.arn,
            "Name": self.name,
            "Description": self.description,
            "KmsKeyId": self.kms_key_id,
            "Type": self.secret_type,
            "RotationEnabled": self.rotation_enabled,
            "RotationLambdaARN": self.rotation_lambda,
            "RotationRules": self.rotation_rules,
            "LastRotatedDate": self.last_rotated,
            "NextRotationDate": self.next_rotation(),
            "LastChangedDate": self.changed,
            "LastAccessedDate": self.accessed,
            "DeletedDate": self.deleted,
            "CreatedDate": self.created,
            "Tags": self.tags_json(),
        });
        let o = out.as_object_mut().expect("object");
        let key = if list {
            "SecretVersionsToStages"
        } else {
            "VersionIdsToStages"
        };
        o.insert(key.to_string(), Value::Object(stages));
        if !self.replicas.is_empty() {
            o.insert("PrimaryRegion".to_string(), json!(self.primary_region));
            if !list {
                o.insert("ReplicationStatus".to_string(), self.replication_json());
            }
        }
        out
    }

    fn value_json(&self, idx: usize) -> Value {
        let v = &self.versions[idx];
        let mut out = json!({
            "ARN": self.arn,
            "Name": self.name,
            "VersionId": v.id,
            "VersionStages": v.stages,
            "CreatedDate": v.created,
        });
        let (key, value) = match &v.payload {
            Payload::String(s) => ("SecretString", s),
            Payload::Binary(b) => ("SecretBinary", b),
        };
        out.as_object_mut()
            .expect("object")
            .insert(key.to_string(), json!(value));
        out
    }

    fn touch(&mut self, idx: usize, now: f64) {
        let day = (now / DAY).floor() * DAY;
        self.accessed = Some(day);
        self.versions[idx].accessed = Some(day);
    }

    fn matches(&self, filters: &[Value]) -> Result<bool, AwsError> {
        for filter in filters {
            let key = filter.str_field("Key").unwrap_or("all");
            let fields: Vec<&str> = match key {
                "name" => vec![self.name.as_str()],
                "description" => self.description.iter().map(String::as_str).collect(),
                "tag-key" => self.tags.iter().map(|(k, _)| k.as_str()).collect(),
                "tag-value" => self.tags.iter().map(|(_, v)| v.as_str()).collect(),
                "primary-region" => vec![self.primary_region.as_str()],
                "owning-service" => Vec::new(),
                "all" => std::iter::once(self.name.as_str())
                    .chain(self.description.iter().map(String::as_str))
                    .chain(self.tags.iter().flat_map(|(k, v)| [k.as_str(), v.as_str()]))
                    .collect(),
                other => {
                    return Err(validation(format!(
                        "Value '{other}' at 'filters.member.key' failed to satisfy constraint: Member must satisfy enum value set: [description, name, tag-key, tag-value, primary-region, owning-service, all]"
                    )));
                }
            };
            let hit = filter.array_field("Values").iter().any(|value| {
                let value = value.as_str().unwrap_or_default();
                let (negated, needle) = match value.strip_prefix('!') {
                    Some(rest) => (true, rest),
                    None => (false, value),
                };
                field_match(key, &fields, needle) != negated
            });
            if !hit {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn field_match(key: &str, fields: &[&str], needle: &str) -> bool {
    match key {
        "description" => {
            let needle = needle.to_lowercase();
            fields.iter().any(|f| f.to_lowercase().starts_with(&needle))
        }
        "all" => needle.split_whitespace().all(|word| {
            let word = word.to_lowercase();
            fields.iter().any(|f| {
                let f = f.to_lowercase();
                f.starts_with(&word)
                    || f.split(|c: char| !c.is_alphanumeric())
                        .any(|part| part.starts_with(&word))
            })
        }),
        _ => fields.iter().any(|f| f.starts_with(needle)),
    }
}

#[derive(Debug, Default)]
struct Region {
    secrets: BTreeMap<String, Secret>,
    seq: u64,
}

impl Region {
    fn purge(&mut self, now: f64) {
        self.secrets
            .retain(|_, s| s.deleted.is_none_or(|d| d > now));
    }

    fn key(&self, id: &str) -> Option<String> {
        if self.secrets.contains_key(id) {
            return Some(id.to_string());
        }
        self.secrets
            .values()
            .find(|s| {
                s.arn == id
                    || (id.starts_with("arn:")
                        && s.arn.len() == id.len() + 7
                        && s.arn.starts_with(id)
                        && s.arn.as_bytes()[id.len()] == b'-')
            })
            .map(|s| s.name.clone())
    }

    fn get(&self, id: &str) -> Result<&Secret, AwsError> {
        self.key(id)
            .and_then(|k| self.secrets.get(&k))
            .ok_or_else(not_found)
    }

    fn get_mut(&mut self, id: &str) -> Result<&mut Secret, AwsError> {
        let key = self.key(id).ok_or_else(not_found)?;
        self.secrets.get_mut(&key).ok_or_else(not_found)
    }

    fn sorted(&self, sort_by: &str, desc: bool) -> Vec<&Secret> {
        let mut list: Vec<&Secret> = self.secrets.values().collect();
        list.sort_by(|a, b| {
            let ord = match sort_by {
                "name" => a.name.cmp(&b.name),
                "last-changed-date" => a.changed.total_cmp(&b.changed),
                "last-accessed-date" => a
                    .accessed
                    .unwrap_or_default()
                    .total_cmp(&b.accessed.unwrap_or_default()),
                _ => a.created.total_cmp(&b.created),
            };
            ord.then(a.seq.cmp(&b.seq))
        });
        if desc {
            list.reverse();
        }
        list
    }
}

#[derive(Default)]
pub struct SecretsManager {
    regions: HashMap<(String, String), Region>,
}

fn err(code: &str, message: impl Into<String>) -> AwsError {
    AwsError::new(400, code, message)
}

fn not_found() -> AwsError {
    err(
        "ResourceNotFoundException",
        "Secrets Manager can't find the specified secret.",
    )
}

fn exists(message: &str) -> AwsError {
    err("ResourceExistsException", message)
}

fn invalid_param(message: impl Into<String>) -> AwsError {
    err("InvalidParameterException", message)
}

fn invalid_request(message: impl Into<String>) -> AwsError {
    err("InvalidRequestException", message)
}

fn validation(message: impl Into<String>) -> AwsError {
    err(
        "ValidationException",
        format!("1 validation error detected: {}", message.into()),
    )
}

fn secs(ctx: &Ctx<'_>) -> f64 {
    ctx.now_ms() / 1000.0
}

fn secret_id(input: &Value) -> Result<&str, AwsError> {
    input
        .str_field("SecretId")
        .ok_or_else(|| invalid_param("You must specify a SecretId."))
}

fn flag(input: &Value, key: &str) -> Option<bool> {
    input.get(key).and_then(Value::as_bool)
}

fn opt_string(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_string)
}

fn strings(v: &[Value]) -> Vec<String> {
    v.iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

fn payload(input: &Value) -> Result<Option<Payload>, AwsError> {
    match (
        input.get("SecretString").and_then(Value::as_str),
        input.str_field("SecretBinary"),
    ) {
        (Some(_), Some(_)) => Err(invalid_param(
            "You can't specify both a binary secret value and a string secret value in the same secret.",
        )),
        (Some(s), None) => Ok(Some(Payload::String(s.to_string()))),
        (None, Some(b)) => Ok(Some(Payload::Binary(b.to_string()))),
        (None, None) => Ok(None),
    }
}

fn token(ctx: &mut Ctx<'_>, input: &Value) -> String {
    input
        .str_field("ClientRequestToken")
        .map_or_else(|| ctx.uuid(), str::to_string)
}

fn max_results(input: &Value, max: i64) -> Result<Option<usize>, AwsError> {
    match input.i64_field("MaxResults") {
        None => Ok(None),
        Some(n) if (1..=max).contains(&n) => Ok(Some(n as usize)),
        Some(n) => Err(validation(format!(
            "Value '{n}' at 'maxResults' failed to satisfy constraint: Member must have value between 1 and {max}"
        ))),
    }
}

fn paginate<T>(
    items: Vec<T>,
    input: &Value,
    limit: Option<usize>,
) -> Result<(Vec<T>, Option<String>), AwsError> {
    let start = match input.str_field("NextToken") {
        None => 0,
        Some(t) => STANDARD
            .decode(t)
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|s| *s <= items.len())
            .ok_or_else(|| {
                err(
                    "InvalidNextTokenException",
                    "The NextToken value is invalid.",
                )
            })?,
    };
    let end = limit.map_or(items.len(), |l| (start + l).min(items.len()));
    let next = (end < items.len()).then(|| STANDARD.encode(end.to_string()));
    Ok((
        items.into_iter().skip(start).take(end - start).collect(),
        next,
    ))
}

fn policy_document(input: &Value) -> Result<Value, AwsError> {
    let text = input
        .str_field("ResourcePolicy")
        .ok_or_else(|| invalid_param("You must specify a ResourcePolicy."))?;
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(|p| p.get("Statement").is_some())
        .ok_or_else(|| {
            err(
                "MalformedPolicyDocumentException",
                "This resource policy contains a syntax error.",
            )
        })
}

fn is_public(policy: &Value) -> bool {
    let statements = match policy.get("Statement") {
        Some(Value::Array(a)) => a.clone(),
        Some(s) => vec![s.clone()],
        None => Vec::new(),
    };
    let star = |v: &Value| match v {
        Value::String(s) => s == "*",
        Value::Array(a) => a.iter().any(|x| x.as_str() == Some("*")),
        _ => false,
    };
    statements.iter().any(|s| {
        s.str_field("Effect") == Some("Allow")
            && s.get("Condition").is_none()
            && s.get("Principal")
                .is_some_and(|p| star(p) || p.get("AWS").is_some_and(star))
    })
}

fn add_replicas(secret: &mut Secret, regions: &[Value]) -> Result<(), AwsError> {
    for replica in regions {
        let region = replica
            .str_field("Region")
            .ok_or_else(|| invalid_param("You must specify a replica Region."))?;
        if region == secret.primary_region {
            return Err(invalid_param(
                "Invalid replica region: the replica can't be in the primary Region.",
            ));
        }
        let kms_key_id = opt_string(replica, "KmsKeyId");
        match secret.replicas.iter_mut().find(|r| r.region == region) {
            Some(r) => r.kms_key_id = kms_key_id,
            None => secret.replicas.push(Replica {
                region: region.to_string(),
                kms_key_id,
            }),
        }
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    (1..=512).contains(&name.len())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/_+=.@-".contains(c))
}

fn merge_tags(secret: &mut Secret, tags: &[Value]) {
    for tag in tags {
        let Some(key) = tag.get("Key").and_then(Value::as_str) else {
            continue;
        };
        let value = tag.str_field("Value").unwrap_or_default().to_string();
        match secret.tags.iter_mut().find(|(k, _)| k == key) {
            Some(t) => t.1 = value,
            None => secret.tags.push((key.to_string(), value)),
        }
    }
}

fn select_version(
    secret: &Secret,
    version_id: Option<&str>,
    stage: Option<&str>,
) -> Result<usize, AwsError> {
    match version_id {
        Some(id) => {
            let idx = secret.position(id).ok_or_else(|| {
                err(
                    "ResourceNotFoundException",
                    format!(
                        "Secrets Manager can't find the specified secret value for VersionId: {id}"
                    ),
                )
            })?;
            if let Some(stage) =
                stage.filter(|s| !secret.versions[idx].stages.iter().any(|x| x == s))
            {
                return Err(invalid_request(format!(
                    "You provided a VersionStage {stage} that is not associated to the provided VersionId {id}."
                )));
            }
            Ok(idx)
        }
        None => {
            let stage = stage.unwrap_or(CURRENT);
            secret.holder(stage).ok_or_else(|| {
                err(
                    "ResourceNotFoundException",
                    format!("Secrets Manager can't find the specified secret value for staging label: {stage}"),
                )
            })
        }
    }
}

fn sort_options(input: &Value) -> Result<(&str, bool), AwsError> {
    let sort_by = input.str_field("SortBy").unwrap_or("created-date");
    if ![
        "created-date",
        "last-accessed-date",
        "last-changed-date",
        "name",
    ]
    .contains(&sort_by)
    {
        return Err(validation(format!(
            "Value '{sort_by}' at 'sortBy' failed to satisfy constraint: Member must satisfy enum value set: [created-date, last-accessed-date, last-changed-date, name]"
        )));
    }
    match input.str_field("SortOrder").unwrap_or("asc") {
        "asc" => Ok((sort_by, false)),
        "desc" => Ok((sort_by, true)),
        other => Err(validation(format!(
            "Value '{other}' at 'sortOrder' failed to satisfy constraint: Member must satisfy enum value set: [asc, desc]"
        ))),
    }
}

impl SecretsManager {
    fn region(&mut self, ctx: &Ctx<'_>) -> &mut Region {
        let region = self
            .regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default();
        region.purge(secs(ctx));
        region
    }
}

impl Service for SecretsManager {
    fn signing_name(&self) -> &'static str {
        "secretsmanager"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("secretsmanager")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        if operation == "GetRandomPassword" {
            return Ok(json!({ "RandomPassword": random_password(ctx, &input)? }));
        }
        let region = self.region(ctx);
        match operation {
            "CreateSecret" => region.create_secret(ctx, &input),
            "GetSecretValue" => region.get_secret_value(ctx, &input),
            "PutSecretValue" => region.put_secret_value(ctx, &input),
            "UpdateSecret" => region.update_secret(ctx, &input),
            "DeleteSecret" => region.delete_secret(ctx, &input),
            "RestoreSecret" => region.restore_secret(ctx, &input),
            "DescribeSecret" => Ok(region.get(secret_id(&input)?)?.describe(false)),
            "ListSecrets" => region.list_secrets(&input),
            "BatchGetSecretValue" => region.batch_get_secret_value(ctx, &input),
            "ListSecretVersionIds" => region.list_secret_version_ids(&input),
            "UpdateSecretVersionStage" => region.update_secret_version_stage(ctx, &input),
            "RotateSecret" => region.rotate_secret(ctx, &input),
            "CancelRotateSecret" => region.cancel_rotate_secret(ctx, &input),
            "TagResource" => region.tag_resource(&input),
            "UntagResource" => region.untag_resource(&input),
            "PutResourcePolicy" => region.put_resource_policy(&input),
            "GetResourcePolicy" => {
                let s = region.get(secret_id(&input)?)?;
                Ok(json!({ "ARN": s.arn, "Name": s.name, "ResourcePolicy": s.policy }))
            }
            "DeleteResourcePolicy" => {
                let s = region.get_mut(secret_id(&input)?)?;
                s.policy = None;
                Ok(json!({ "ARN": s.arn, "Name": s.name }))
            }
            "ValidateResourcePolicy" => region.validate_resource_policy(&input),
            "ReplicateSecretToRegions" => {
                let s = region.get_mut(secret_id(&input)?)?;
                add_replicas(s, input.array_field("AddReplicaRegions"))?;
                Ok(json!({ "ARN": s.arn, "ReplicationStatus": s.replication_json() }))
            }
            "RemoveRegionsFromReplication" => {
                let s = region.get_mut(secret_id(&input)?)?;
                let remove = strings(input.array_field("RemoveReplicaRegions"));
                s.replicas.retain(|r| !remove.contains(&r.region));
                Ok(json!({ "ARN": s.arn, "ReplicationStatus": s.replication_json() }))
            }
            "StopReplicationToReplica" => {
                let s = region.get_mut(secret_id(&input)?)?;
                s.replicas.clear();
                Ok(json!({ "ARN": s.arn }))
            }
            _ => Err(err(
                "InvalidRequestException",
                format!("wasmo: {operation} is not implemented"),
            )),
        }
    }
}
