use indexmap::IndexMap;
use serde_json::{Value, json};

use super::crypto::{self, EnvRng, Material};
use super::{
    Alias, Key, KeyState, Kms, Rotation, default_policy, err, opt_str, paginate, random_hex,
    ranged, required, secs, validation,
};
use crate::codec::AwsError;
use crate::service::{Ctx, OpResult};

const DAY: f64 = 86400.0;
const UNIMPLEMENTED_SPECS: &[&str] = &[
    "ECC_NIST_P384",
    "ECC_NIST_P521",
    "ECC_SECG_P256K1",
    "ECC_NIST_EDWARDS25519",
    "SM2",
    "ML_DSA_44",
    "ML_DSA_65",
    "ML_DSA_87",
];

fn valid_usage(spec: &str, usage: &str) -> Option<bool> {
    Some(match spec {
        "SYMMETRIC_DEFAULT" => usage == "ENCRYPT_DECRYPT",
        "RSA_2048" | "RSA_3072" | "RSA_4096" => {
            matches!(usage, "ENCRYPT_DECRYPT" | "SIGN_VERIFY")
        }
        "ECC_NIST_P256" => matches!(usage, "SIGN_VERIFY" | "KEY_AGREEMENT"),
        "HMAC_224" | "HMAC_256" | "HMAC_384" | "HMAC_512" => usage == "GENERATE_VERIFY_MAC",
        _ => return None,
    })
}

pub(super) fn new_key(
    ctx: &mut Ctx<'_>,
    spec: &str,
    usage: &str,
    origin: &str,
    multi_region: bool,
) -> Result<Key, AwsError> {
    match valid_usage(spec, usage) {
        Some(true) => {}
        Some(false) => {
            return Err(validation(format!(
                "KeyUsage {usage} is not compatible with KeySpec {spec}"
            )));
        }
        None if UNIMPLEMENTED_SPECS.contains(&spec) => {
            return Err(err(
                "UnsupportedOperationException",
                format!("wasmo does not implement KeySpec {spec}."),
            ));
        }
        None => return Err(validation(format!("Invalid KeySpec {spec}"))),
    }
    let (material, state) = match origin {
        "AWS_KMS" => (
            crypto::generate(spec, &mut EnvRng(&mut *ctx.env)).expect("spec checked above"),
            KeyState::Enabled,
        ),
        "EXTERNAL" if spec == "SYMMETRIC_DEFAULT" => (Material::Pending, KeyState::PendingImport),
        "EXTERNAL" => {
            return Err(err(
                "UnsupportedOperationException",
                "wasmo only implements EXTERNAL origin for SYMMETRIC_DEFAULT keys.",
            ));
        }
        "AWS_CLOUDHSM" | "EXTERNAL_KEY_STORE" => {
            return Err(err(
                "UnsupportedOperationException",
                format!("wasmo does not implement custom key stores (Origin {origin})."),
            ));
        }
        _ => return Err(validation(format!("Invalid Origin {origin}"))),
    };
    let id = if multi_region {
        format!("mrk-{}", random_hex(ctx, 16))
    } else {
        ctx.uuid()
    };
    let material_ids = if matches!(material, Material::Symmetric(_)) {
        vec![random_hex(ctx, 32)]
    } else {
        Vec::new()
    };
    Ok(Key {
        arn: ctx.arn("kms", &format!("key/{id}")),
        id,
        account: ctx.account.clone(),
        region: ctx.region.clone(),
        created: secs(ctx),
        description: String::new(),
        usage: usage.to_string(),
        spec: spec.to_string(),
        origin: origin.to_string(),
        manager: "CUSTOMER",
        state,
        deletion_date: None,
        tags: IndexMap::new(),
        policy: default_policy(&ctx.account),
        rotation_period: None,
        next_rotation: None,
        rotations: Vec::new(),
        material_ids,
        material,
        primary_region: multi_region.then(|| ctx.region.clone()),
    })
}

fn checked_policy(input: &Value) -> Result<Option<String>, AwsError> {
    let Some(policy) = opt_str(input, "Policy") else {
        return Ok(None);
    };
    if serde_json::from_str::<serde_json::Map<String, Value>>(policy).is_err() {
        return Err(err(
            "MalformedPolicyDocumentException",
            "The new key policy will not allow you to update the key policy in the future.",
        ));
    }
    Ok(Some(policy.to_string()))
}

fn parse_tags(input: &Value) -> Result<Vec<(String, String)>, AwsError> {
    input
        .get("Tags")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|t| match (opt_str(t, "TagKey"), opt_str(t, "TagValue")) {
            (Some(k), Some(v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
            _ => Err(err(
                "TagException",
                "Each tag needs a non-empty TagKey and a TagValue.",
            )),
        })
        .collect()
}

fn tag_list(tags: &IndexMap<String, String>) -> Vec<Value> {
    tags.iter()
        .map(|(k, v)| json!({"TagKey": k, "TagValue": v}))
        .collect()
}

fn alias_name(input: &Value) -> Result<&str, AwsError> {
    let name = required(input, "AliasName")?;
    let rest = name.strip_prefix("alias/").unwrap_or_default();
    if rest.is_empty()
        || !rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-'))
    {
        return Err(validation(format!(
            "1 validation error detected: Value '{name}' at 'aliasName' failed to satisfy constraint: Member must satisfy regular expression pattern: ^alias/[a-zA-Z0-9/_-]+$"
        )));
    }
    if rest.starts_with("aws/") {
        return Err(err(
            "NotAuthorizedException",
            format!("{name} is reserved for AWS managed keys."),
        ));
    }
    Ok(name)
}

fn same_kind(a: &Key, b: &Key) -> bool {
    a.usage == b.usage && (a.spec == "SYMMETRIC_DEFAULT") == (b.spec == "SYMMETRIC_DEFAULT")
}

impl Kms {
    pub(super) fn create_key(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let spec = opt_str(input, "KeySpec")
            .or_else(|| opt_str(input, "CustomerMasterKeySpec"))
            .unwrap_or("SYMMETRIC_DEFAULT");
        let usage = opt_str(input, "KeyUsage").unwrap_or("ENCRYPT_DECRYPT");
        let origin = opt_str(input, "Origin").unwrap_or("AWS_KMS");
        let multi_region = input.get("MultiRegion").and_then(Value::as_bool) == Some(true);
        let policy = checked_policy(input)?;
        let tags = parse_tags(input)?;
        let mut key = new_key(ctx, spec, usage, origin, multi_region)?;
        key.description = opt_str(input, "Description")
            .unwrap_or_default()
            .to_string();
        key.tags.extend(tags);
        if let Some(p) = policy {
            key.policy = p;
        }
        let out = json!({"KeyMetadata": self.metadata(&key)});
        self.region(ctx).keys.insert(key.id.clone(), key);
        Ok(out)
    }

    pub(super) fn describe_key(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let key = self.input_key(ctx, input)?.clone();
        Ok(json!({"KeyMetadata": self.metadata(&key)}))
    }

    pub(super) fn list_keys(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let items = self
            .region(ctx)
            .keys
            .values()
            .map(|k| json!({"KeyId": k.id, "KeyArn": k.arn}))
            .collect();
        paginate(input, "Keys", items, 100)
    }

    pub(super) fn set_enabled(
        &mut self,
        ctx: &mut Ctx<'_>,
        input: &Value,
        enabled: bool,
    ) -> OpResult {
        let key = self.input_key(ctx, input)?;
        match key.state {
            KeyState::Enabled | KeyState::Disabled => {
                key.state = if enabled {
                    KeyState::Enabled
                } else {
                    KeyState::Disabled
                };
                Ok(json!({}))
            }
            state => Err(err(
                "KMSInvalidStateException",
                format!("{} is {}.", key.arn, state.as_str()),
            )),
        }
    }

    pub(super) fn schedule_deletion(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let days = ranged(input, "PendingWindowInDays", 7, 30)?.unwrap_or(30);
        let now = secs(ctx);
        let key = self.input_key(ctx, input)?;
        key.mutable()?;
        let date = now + days as f64 * DAY;
        key.state = KeyState::PendingDeletion;
        key.deletion_date = Some(date);
        Ok(json!({
            "KeyId": key.arn,
            "DeletionDate": date,
            "KeyState": "PendingDeletion",
            "PendingWindowInDays": days,
        }))
    }

    pub(super) fn cancel_deletion(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let key = self.input_key(ctx, input)?;
        if key.state != KeyState::PendingDeletion {
            return Err(err(
                "KMSInvalidStateException",
                format!("{} is not pending deletion.", key.arn),
            ));
        }
        key.state = if matches!(key.material, Material::Pending) {
            KeyState::PendingImport
        } else {
            KeyState::Disabled
        };
        key.deletion_date = None;
        Ok(json!({"KeyId": key.arn}))
    }

    pub(super) fn update_description(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let description = required(input, "Description")?.to_string();
        let key = self.input_key(ctx, input)?;
        key.mutable()?;
        key.description = description;
        Ok(json!({}))
    }

    pub(super) fn replicate_key(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let replica_region = required(input, "ReplicaRegion")?.to_string();
        let policy = checked_policy(input)?;
        let tags = parse_tags(input)?;
        let key = self.input_key(ctx, input)?;
        key.mutable()?;
        if key.primary_region.as_deref() != Some(key.region.as_str()) {
            return Err(err(
                "UnsupportedOperationException",
                format!("{} is not a multi-Region primary key.", key.arn),
            ));
        }
        if replica_region == key.region {
            return Err(validation(
                "ReplicaRegion must differ from the primary key's Region.",
            ));
        }
        let mut replica = key.clone();
        let target = (ctx.account.clone(), replica_region.clone());
        if self
            .regions
            .get(&target)
            .is_some_and(|r| r.keys.contains_key(&replica.id))
        {
            return Err(err(
                "AlreadyExistsException",
                format!("{} already has a replica in {replica_region}.", replica.arn),
            ));
        }
        replica.arn = format!(
            "arn:aws:kms:{replica_region}:{}:key/{}",
            replica.account, replica.id
        );
        replica.region = replica_region;
        replica.created = secs(ctx);
        replica.rotation_period = None;
        replica.next_rotation = None;
        if let Some(d) = opt_str(input, "Description") {
            replica.description = d.to_string();
        }
        if let Some(p) = policy {
            replica.policy = p;
        }
        if !tags.is_empty() {
            replica.tags = tags.into_iter().collect();
        }
        let out_policy = replica.policy.clone();
        let out_tags = tag_list(&replica.tags);
        self.regions
            .entry(target)
            .or_default()
            .keys
            .insert(replica.id.clone(), replica.clone());
        Ok(json!({
            "ReplicaKeyMetadata": self.metadata(&replica),
            "ReplicaPolicy": out_policy,
            "ReplicaTags": out_tags,
        }))
    }

    pub(super) fn update_primary_region(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let primary = required(input, "PrimaryRegion")?.to_string();
        let key = self.input_key(ctx, input)?;
        if key.primary_region.as_deref() != Some(key.region.as_str()) {
            return Err(err(
                "UnsupportedOperationException",
                format!("{} is not a multi-Region primary key.", key.arn),
            ));
        }
        let id = key.id.clone();
        let account = key.account.clone();
        if !self
            .regions
            .get(&(account.clone(), primary.clone()))
            .is_some_and(|r| r.keys.contains_key(&id))
        {
            return Err(err(
                "NotFoundException",
                format!("No replica of {id} exists in {primary}."),
            ));
        }
        for ((acct, _), region) in &mut self.regions {
            if *acct == account
                && let Some(k) = region.keys.get_mut(&id)
            {
                k.primary_region = Some(primary.clone());
            }
        }
        Ok(json!({}))
    }

    pub(super) fn create_alias(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let name = alias_name(input)?.to_string();
        let target = required(input, "TargetKeyId")?;
        if target.starts_with("alias/") || target.contains(":alias/") {
            return Err(validation("TargetKeyId must be a key ID or key ARN."));
        }
        let key = self.key(ctx, target)?;
        key.mutable()?;
        let id = key.id.clone();
        let now = secs(ctx);
        let alias_arn = ctx.arn("kms", &name);
        let region = self.region(ctx);
        if region.aliases.contains_key(&name) {
            return Err(err(
                "AlreadyExistsException",
                format!("An alias with the name {alias_arn} already exists"),
            ));
        }
        region.aliases.insert(
            name,
            Alias {
                target: id,
                created: now,
                updated: now,
            },
        );
        Ok(json!({}))
    }

    pub(super) fn update_alias(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let name = alias_name(input)?.to_string();
        let target = required(input, "TargetKeyId")?;
        let new_key = self.key(ctx, target)?.clone();
        new_key.mutable()?;
        let now = secs(ctx);
        let alias_arn = ctx.arn("kms", &name);
        let region = self.region(ctx);
        let Some(current) = region.aliases.get(&name).map(|a| a.target.clone()) else {
            return Err(err(
                "NotFoundException",
                format!("Alias {alias_arn} is not found."),
            ));
        };
        if let Some(old) = region.keys.get(&current)
            && !same_kind(old, &new_key)
        {
            return Err(validation(format!(
                "{} and {} are different types of keys.",
                old.arn, new_key.arn
            )));
        }
        let alias = region.aliases.get_mut(&name).expect("alias exists");
        alias.target = new_key.id;
        alias.updated = now;
        Ok(json!({}))
    }

    pub(super) fn delete_alias(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let name = alias_name(input)?.to_string();
        let alias_arn = ctx.arn("kms", &name);
        if self.region(ctx).aliases.shift_remove(&name).is_none() {
            return Err(err(
                "NotFoundException",
                format!("Alias {alias_arn} is not found."),
            ));
        }
        Ok(json!({}))
    }

    pub(super) fn list_aliases(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let filter = match opt_str(input, "KeyId") {
            Some(k) => Some(self.resolve(ctx, k)?),
            None => None,
        };
        let items = self
            .region(ctx)
            .aliases
            .iter()
            .filter(|(_, a)| filter.as_ref().is_none_or(|f| *f == a.target))
            .map(|(name, a)| {
                json!({
                    "AliasName": name,
                    "AliasArn": ctx.arn("kms", name),
                    "TargetKeyId": a.target,
                    "CreationDate": a.created,
                    "LastUpdatedDate": a.updated,
                })
            })
            .collect();
        paginate(input, "Aliases", items, 50)
    }

    pub(super) fn tag_resource(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let tags = parse_tags(input)?;
        let key = self.input_key(ctx, input)?;
        key.mutable()?;
        key.tags.extend(tags);
        Ok(json!({}))
    }

    pub(super) fn untag_resource(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let key = self.input_key(ctx, input)?;
        key.mutable()?;
        for k in input
            .get("TagKeys")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            key.tags.shift_remove(k);
        }
        Ok(json!({}))
    }

    pub(super) fn list_tags(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let items = tag_list(&self.input_key(ctx, input)?.tags);
        paginate(input, "Tags", items, 50)
    }

    pub(super) fn get_policy(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let name = opt_str(input, "PolicyName").unwrap_or("default");
        let key = self.input_key(ctx, input)?;
        if name != "default" {
            return Err(err(
                "NotFoundException",
                format!("Policy {name} does not exist."),
            ));
        }
        Ok(json!({"Policy": key.policy, "PolicyName": "default"}))
    }

    pub(super) fn put_policy(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        required(input, "Policy")?;
        let policy = checked_policy(input)?.expect("Policy checked above");
        let name = opt_str(input, "PolicyName").unwrap_or("default");
        let key = self.input_key(ctx, input)?;
        key.mutable()?;
        if name != "default" {
            return Err(err(
                "NotFoundException",
                format!("Policy {name} does not exist."),
            ));
        }
        key.policy = policy;
        Ok(json!({}))
    }

    pub(super) fn list_policies(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        self.input_key(ctx, input)?;
        paginate(input, "PolicyNames", vec![json!("default")], 1000)
    }

    fn rotatable(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> Result<&mut Key, AwsError> {
        let key = self.input_key(ctx, input)?;
        key.usable()?;
        if key.spec != "SYMMETRIC_DEFAULT" || key.origin != "AWS_KMS" {
            return Err(err(
                "UnsupportedOperationException",
                format!("{} does not support automatic key rotation.", key.arn),
            ));
        }
        Ok(key)
    }

    pub(super) fn enable_rotation(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let days = ranged(input, "RotationPeriodInDays", 90, 2560)?.unwrap_or(365);
        let now = secs(ctx);
        let key = self.rotatable(ctx, input)?;
        key.rotation_period = Some(days);
        key.next_rotation = Some(now + days as f64 * DAY);
        Ok(json!({}))
    }

    pub(super) fn disable_rotation(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let key = self.rotatable(ctx, input)?;
        key.rotation_period = None;
        key.next_rotation = None;
        Ok(json!({}))
    }

    pub(super) fn rotation_status(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let key = self.input_key(ctx, input)?;
        let mut out = json!({
            "KeyRotationEnabled": key.rotation_period.is_some(),
            "KeyId": key.arn,
        });
        if let (Some(days), Some(next)) = (key.rotation_period, key.next_rotation) {
            out["RotationPeriodInDays"] = json!(days);
            out["NextRotationDate"] = json!(next);
        }
        if let Some(r) = key.rotations.iter().rev().find(|r| r.kind == "ON_DEMAND") {
            out["OnDemandRotationStartDate"] = json!(r.date);
        }
        Ok(out)
    }

    pub(super) fn rotate_on_demand(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let now = secs(ctx);
        let material: [u8; 32] = crypto::random_array(&mut EnvRng(&mut *ctx.env));
        let material_id = random_hex(ctx, 32);
        let key = self.rotatable(ctx, input)?;
        if let Material::Symmetric(versions) = &mut key.material {
            versions.push(material);
        }
        key.material_ids.push(material_id.clone());
        key.rotations.push(Rotation {
            date: now,
            kind: "ON_DEMAND",
            material_id,
        });
        Ok(json!({"KeyId": key.arn}))
    }

    pub(super) fn list_rotations(&mut self, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
        let key = self.input_key(ctx, input)?;
        let items = key
            .rotations
            .iter()
            .map(|r| {
                json!({
                    "KeyId": key.arn,
                    "KeyMaterialId": r.material_id,
                    "RotationDate": r.date,
                    "RotationType": r.kind,
                })
            })
            .collect();
        paginate(input, "Rotations", items, 100)
    }
}
