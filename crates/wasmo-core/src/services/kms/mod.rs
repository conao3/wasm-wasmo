mod crypto;
mod grants;
mod keys;
mod ops;

use std::collections::HashMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Arn, Ctx, OpResult, Service, ServiceModel};
use crypto::Material;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KeyState {
    Enabled,
    Disabled,
    PendingDeletion,
    PendingImport,
}

impl KeyState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "Enabled",
            Self::Disabled => "Disabled",
            Self::PendingDeletion => "PendingDeletion",
            Self::PendingImport => "PendingImport",
        }
    }
}

#[derive(Clone)]
struct Rotation {
    date: f64,
    kind: &'static str,
    material_id: String,
}

#[derive(Clone)]
struct Key {
    id: String,
    arn: String,
    account: String,
    region: String,
    created: f64,
    description: String,
    usage: String,
    spec: String,
    origin: String,
    manager: &'static str,
    state: KeyState,
    deletion_date: Option<f64>,
    tags: IndexMap<String, String>,
    policy: String,
    rotation_period: Option<i64>,
    next_rotation: Option<f64>,
    rotations: Vec<Rotation>,
    material_ids: Vec<String>,
    material: Material,
    primary_region: Option<String>,
}

struct Alias {
    target: String,
    created: f64,
    updated: f64,
}

struct Grant {
    id: String,
    token: String,
    key_id: String,
    name: Option<String>,
    grantee: Option<String>,
    retiring: Option<String>,
    operations: Vec<Value>,
    constraints: Option<Value>,
    created: f64,
}

#[derive(Default)]
struct Region {
    keys: IndexMap<String, Key>,
    aliases: IndexMap<String, Alias>,
    grants: Vec<Grant>,
}

#[derive(Default)]
pub struct Kms {
    regions: HashMap<(String, String), Region>,
}

fn err(code: &str, message: impl Into<String>) -> AwsError {
    AwsError::new(400, code, message)
}

fn validation(message: impl Into<String>) -> AwsError {
    err("ValidationException", message)
}

fn secs(ctx: &Ctx<'_>) -> f64 {
    ctx.now_ms() / 1000.0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(ctx: &mut Ctx<'_>, len: usize) -> String {
    let mut b = vec![0u8; len];
    ctx.random_bytes(&mut b);
    hex(&b)
}

fn required<'a>(input: &'a Value, key: &str) -> Result<&'a str, AwsError> {
    input.get(key).and_then(Value::as_str).ok_or_else(|| {
        validation(format!(
            "1 validation error detected: Value null at '{key}' failed to satisfy constraint: Member must not be null"
        ))
    })
}

fn opt_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

fn blob(input: &Value, key: &str) -> Result<Vec<u8>, AwsError> {
    STANDARD
        .decode(required(input, key)?)
        .map_err(|_| validation(format!("{key} is not valid base64.")))
}

fn sized_blob(input: &Value, key: &str, max: usize) -> Result<Vec<u8>, AwsError> {
    let b = blob(input, key)?;
    if b.is_empty() || b.len() > max {
        return Err(validation(format!(
            "1 validation error detected: Value at '{key}' failed to satisfy constraint: Member must have length between 1 and {max}"
        )));
    }
    Ok(b)
}

fn b64(bytes: &[u8]) -> Value {
    Value::String(STANDARD.encode(bytes))
}

fn ranged(input: &Value, key: &str, min: i64, max: i64) -> Result<Option<i64>, AwsError> {
    let Some(v) = input.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    match v.as_i64() {
        Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
        _ => Err(validation(format!(
            "1 validation error detected: Value '{v}' at '{key}' failed to satisfy constraint: Member must have value between {min} and {max}"
        ))),
    }
}

fn encryption_context(input: &Value, key: &str) -> Result<Vec<u8>, AwsError> {
    let Some(map) = input.get(key).and_then(Value::as_object) else {
        return Ok(Vec::new());
    };
    let mut pairs = Vec::with_capacity(map.len());
    for (k, v) in map {
        let v = v
            .as_str()
            .ok_or_else(|| validation(format!("{key} values must be strings.")))?;
        pairs.push((k.as_str(), v));
    }
    Ok(crypto::context_bytes(pairs))
}

fn paginate(input: &Value, field: &str, items: Vec<Value>, default_limit: usize) -> OpResult {
    let limit = ranged(input, "Limit", 1, 1000)?.map_or(default_limit, |n| n as usize);
    let start = match opt_str(input, "Marker") {
        Some(m) => m
            .parse::<usize>()
            .ok()
            .filter(|n| *n <= items.len())
            .ok_or_else(|| err("InvalidMarkerException", "Invalid marker."))?,
        None => 0,
    };
    let end = (start + limit).min(items.len());
    let truncated = end < items.len();
    let mut out = Map::new();
    out.insert(field.to_string(), Value::Array(items[start..end].to_vec()));
    out.insert("Truncated".to_string(), Value::Bool(truncated));
    if truncated {
        out.insert("NextMarker".to_string(), Value::String(end.to_string()));
    }
    Ok(Value::Object(out))
}

fn default_policy(account: &str) -> String {
    json!({
        "Version": "2012-10-17",
        "Id": "key-default-1",
        "Statement": [{
            "Sid": "Enable IAM User Permissions",
            "Effect": "Allow",
            "Principal": {"AWS": format!("arn:aws:iam::{account}:root")},
            "Action": "kms:*",
            "Resource": "*",
        }],
    })
    .to_string()
}

fn not_found(id: &str) -> AwsError {
    err("NotFoundException", format!("Key '{id}' does not exist"))
}

impl Key {
    fn usable(&self) -> Result<(), AwsError> {
        match self.state {
            KeyState::Enabled => Ok(()),
            KeyState::Disabled => Err(err(
                "DisabledException",
                format!("{} is disabled.", self.arn),
            )),
            KeyState::PendingDeletion => Err(err(
                "KMSInvalidStateException",
                format!("{} is pending deletion.", self.arn),
            )),
            KeyState::PendingImport => Err(err(
                "KMSInvalidStateException",
                format!("{} is pending import.", self.arn),
            )),
        }
    }

    fn mutable(&self) -> Result<(), AwsError> {
        if self.state == KeyState::PendingDeletion {
            return Err(err(
                "KMSInvalidStateException",
                format!("{} is pending deletion.", self.arn),
            ));
        }
        Ok(())
    }

    fn require_usage(&self, usage: &str) -> Result<(), AwsError> {
        if self.usage != usage {
            return Err(err(
                "InvalidKeyUsageException",
                format!(
                    "{} key usage is {} which is not valid for this operation.",
                    self.arn, self.usage
                ),
            ));
        }
        Ok(())
    }
}

impl Kms {
    fn region(&mut self, ctx: &Ctx<'_>) -> &mut Region {
        let now = secs(ctx);
        let region = self
            .regions
            .entry((ctx.account.clone(), ctx.region.clone()))
            .or_default();
        let expired: Vec<String> = region
            .keys
            .values()
            .filter(|k| k.deletion_date.is_some_and(|d| d <= now))
            .map(|k| k.id.clone())
            .collect();
        for id in expired {
            region.keys.shift_remove(&id);
            region.aliases.retain(|_, a| a.target != id);
            region.grants.retain(|g| g.key_id != id);
        }
        region
    }

    /// Resolves a key id, key ARN, alias name, or alias ARN to the key id.
    fn resolve(&mut self, ctx: &mut Ctx<'_>, reference: &str) -> Result<String, AwsError> {
        let own = |arn: &Arn<'_>| {
            arn.service == "kms" && arn.region == ctx.region && arn.account == ctx.account
        };
        let name = match Arn::parse(reference) {
            Some(arn) if !own(&arn) => return Err(not_found(reference)),
            Some(arn) => arn.resource.to_string(),
            None => reference.to_string(),
        };
        if let Some(alias) = name.strip_prefix("alias/") {
            if let Some(a) = self.region(ctx).aliases.get(&name) {
                return Ok(a.target.clone());
            }
            if alias.starts_with("aws/") && alias.len() > 4 {
                return Ok(self.create_managed(ctx, &name));
            }
            return Err(err(
                "NotFoundException",
                format!("Alias {} is not found.", ctx.arn("kms", &name)),
            ));
        }
        let id = name.strip_prefix("key/").unwrap_or(&name);
        if self.region(ctx).keys.contains_key(id) {
            Ok(id.to_string())
        } else {
            Err(not_found(&ctx.arn("kms", &format!("key/{id}"))))
        }
    }

    fn key(&mut self, ctx: &mut Ctx<'_>, reference: &str) -> Result<&mut Key, AwsError> {
        let id = self.resolve(ctx, reference)?;
        Ok(self
            .region(ctx)
            .keys
            .get_mut(&id)
            .expect("resolved key exists"))
    }

    fn input_key(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> Result<&mut Key, AwsError> {
        let reference = required(input, "KeyId")?;
        self.key(ctx, reference)
    }

    fn create_managed(&mut self, ctx: &mut Ctx<'_>, alias: &str) -> String {
        let service = alias.trim_start_matches("alias/aws/");
        let mut key = keys::new_key(
            ctx,
            "SYMMETRIC_DEFAULT",
            "ENCRYPT_DECRYPT",
            "AWS_KMS",
            false,
        )
        .expect("symmetric keys are supported");
        key.manager = "AWS";
        key.description = format!(
            "Default key that protects my {service} resources when no other key is defined"
        );
        let id = key.id.clone();
        let now = secs(ctx);
        let region = self.region(ctx);
        region.keys.insert(id.clone(), key);
        region.aliases.insert(
            alias.to_string(),
            Alias {
                target: id.clone(),
                created: now,
                updated: now,
            },
        );
        id
    }

    fn metadata(&self, key: &Key) -> Value {
        let mut m = json!({
            "AWSAccountId": key.account,
            "KeyId": key.id,
            "Arn": key.arn,
            "CreationDate": key.created,
            "Enabled": key.state == KeyState::Enabled,
            "Description": key.description,
            "KeyUsage": key.usage,
            "KeyState": key.state.as_str(),
            "Origin": key.origin,
            "KeyManager": key.manager,
            "CustomerMasterKeySpec": key.spec,
            "KeySpec": key.spec,
            "MultiRegion": key.primary_region.is_some(),
        });
        let o = m.as_object_mut().expect("object");
        if let Some(d) = key.deletion_date {
            o.insert("DeletionDate".into(), json!(d));
        }
        let list = |items: &[&str]| Value::from(items.to_vec());
        match key.usage.as_str() {
            "ENCRYPT_DECRYPT" => {
                o.insert(
                    "EncryptionAlgorithms".into(),
                    list(crypto::encryption_algorithms(&key.spec)),
                );
            }
            "SIGN_VERIFY" => {
                o.insert(
                    "SigningAlgorithms".into(),
                    list(crypto::signing_algorithms(&key.spec)),
                );
            }
            "GENERATE_VERIFY_MAC" => {
                o.insert(
                    "MacAlgorithms".into(),
                    list(
                        &crypto::mac_algorithm(&key.spec)
                            .into_iter()
                            .collect::<Vec<_>>(),
                    ),
                );
            }
            "KEY_AGREEMENT" => {
                o.insert("KeyAgreementAlgorithms".into(), json!(["ECDH"]));
            }
            _ => {}
        }
        if let Some(id) = key.material_ids.last() {
            o.insert("CurrentKeyMaterialId".into(), json!(id));
        }
        if let Some(primary) = &key.primary_region {
            o.insert(
                "MultiRegionConfiguration".into(),
                self.multi_region(key, primary),
            );
        }
        m
    }

    fn multi_region(&self, key: &Key, primary: &str) -> Value {
        let arn = |region: &str| format!("arn:aws:kms:{region}:{}:key/{}", key.account, key.id);
        let mut replicas: Vec<&str> = self
            .regions
            .iter()
            .filter(|((account, region), r)| {
                *account == key.account && region != primary && r.keys.contains_key(&key.id)
            })
            .map(|((_, region), _)| region.as_str())
            .collect();
        replicas.sort();
        json!({
            "MultiRegionKeyType": if key.region == primary { "PRIMARY" } else { "REPLICA" },
            "PrimaryKey": {"Arn": arn(primary), "Region": primary},
            "ReplicaKeys": replicas
                .iter()
                .map(|r| json!({"Arn": arn(r), "Region": r}))
                .collect::<Vec<_>>(),
        })
    }
}

impl Service for Kms {
    fn signing_name(&self) -> &'static str {
        "kms"
    }

    fn model(&self) -> &'static ServiceModel {
        embedded_model!("kms")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        if input.get("DryRun").and_then(Value::as_bool) == Some(true) {
            return Err(err(
                "DryRunOperationException",
                "The request would have succeeded, but the DryRun option is set.",
            ));
        }
        match operation {
            "CreateKey" => self.create_key(ctx, &input),
            "DescribeKey" => self.describe_key(ctx, &input),
            "ListKeys" => self.list_keys(ctx, &input),
            "EnableKey" => self.set_enabled(ctx, &input, true),
            "DisableKey" => self.set_enabled(ctx, &input, false),
            "ScheduleKeyDeletion" => self.schedule_deletion(ctx, &input),
            "CancelKeyDeletion" => self.cancel_deletion(ctx, &input),
            "UpdateKeyDescription" => self.update_description(ctx, &input),
            "ReplicateKey" => self.replicate_key(ctx, &input),
            "UpdatePrimaryRegion" => self.update_primary_region(ctx, &input),
            "CreateAlias" => self.create_alias(ctx, &input),
            "UpdateAlias" => self.update_alias(ctx, &input),
            "DeleteAlias" => self.delete_alias(ctx, &input),
            "ListAliases" => self.list_aliases(ctx, &input),
            "TagResource" => self.tag_resource(ctx, &input),
            "UntagResource" => self.untag_resource(ctx, &input),
            "ListResourceTags" => self.list_tags(ctx, &input),
            "GetKeyPolicy" => self.get_policy(ctx, &input),
            "PutKeyPolicy" => self.put_policy(ctx, &input),
            "ListKeyPolicies" => self.list_policies(ctx, &input),
            "EnableKeyRotation" => self.enable_rotation(ctx, &input),
            "DisableKeyRotation" => self.disable_rotation(ctx, &input),
            "GetKeyRotationStatus" => self.rotation_status(ctx, &input),
            "RotateKeyOnDemand" => self.rotate_on_demand(ctx, &input),
            "ListKeyRotations" => self.list_rotations(ctx, &input),
            "Encrypt" => self.encrypt(ctx, &input),
            "Decrypt" => self.decrypt(ctx, &input),
            "ReEncrypt" => self.re_encrypt(ctx, &input),
            "GenerateDataKey" => self.generate_data_key(ctx, &input, true),
            "GenerateDataKeyWithoutPlaintext" => self.generate_data_key(ctx, &input, false),
            "GenerateDataKeyPair" => self.generate_data_key_pair(ctx, &input, true),
            "GenerateDataKeyPairWithoutPlaintext" => {
                self.generate_data_key_pair(ctx, &input, false)
            }
            "Sign" => self.sign(ctx, &input),
            "Verify" => self.verify(ctx, &input),
            "GetPublicKey" => self.get_public_key(ctx, &input),
            "GenerateMac" => self.generate_mac(ctx, &input),
            "VerifyMac" => self.verify_mac(ctx, &input),
            "DeriveSharedSecret" => self.derive_shared_secret(ctx, &input),
            "GenerateRandom" => self.generate_random(ctx, &input),
            "CreateGrant" => self.create_grant(ctx, &input),
            "ListGrants" => self.list_grants(ctx, &input),
            "ListRetirableGrants" => self.list_retirable_grants(ctx, &input),
            "RetireGrant" => self.retire_grant(ctx, &input),
            "RevokeGrant" => self.revoke_grant(ctx, &input),
            other => Err(err(
                "UnsupportedOperationException",
                format!("wasmo does not implement the KMS operation {other}."),
            )),
        }
    }
}
