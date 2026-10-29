use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::state::Bucket;
use super::{Buckets, bucket_mut, bucket_ref, err, no_such_bucket, now_secs, owner, put, with};
use crate::codec::AwsError;
use crate::embedded_model;
use crate::service::{Ctx, InputExt, OpResult};

const ALL_USERS: &str = "http://acs.amazonaws.com/groups/global/AllUsers";
const AUTHENTICATED_USERS: &str = "http://acs.amazonaws.com/groups/global/AuthenticatedUsers";

enum Missing {
    Error(&'static str, &'static str),
    Empty,
    Default(fn() -> Value),
}

type Validate = fn(&Bucket, &Value) -> Result<(), AwsError>;

struct Config {
    put: &'static str,
    get: &'static str,
    delete: Option<&'static str>,
    member: &'static str,
    missing: Missing,
    validate: Option<Validate>,
}

const fn config(
    put: &'static str,
    get: &'static str,
    delete: Option<&'static str>,
    member: &'static str,
    missing: Missing,
) -> Config {
    Config {
        put,
        get,
        delete,
        member,
        missing,
        validate: None,
    }
}

fn validate_policy(_: &Bucket, policy: &Value) -> Result<(), AwsError> {
    let doc: Option<Value> = policy.as_str().and_then(|p| serde_json::from_str(p).ok());
    if doc.as_ref().is_some_and(|d| d.get("Statement").is_some()) {
        Ok(())
    } else {
        Err(err(
            400,
            "MalformedPolicy",
            "Policies must be valid JSON and the first byte must be '{'",
        ))
    }
}

fn validate_object_lock(bucket: &Bucket, _: &Value) -> Result<(), AwsError> {
    if bucket.versioning.as_deref() == Some("Enabled") {
        Ok(())
    } else {
        Err(err(
            409,
            "InvalidBucketState",
            "Versioning must be 'Enabled' on the bucket to apply a Object Lock configuration",
        ))
    }
}

const CONFIGS: &[Config] = &[
    config(
        "PutBucketCors",
        "GetBucketCors",
        Some("DeleteBucketCors"),
        "CORSConfiguration",
        Missing::Error(
            "NoSuchCORSConfiguration",
            "The CORS configuration does not exist",
        ),
    ),
    config(
        "PutBucketLifecycleConfiguration",
        "GetBucketLifecycleConfiguration",
        Some("DeleteBucketLifecycle"),
        "LifecycleConfiguration",
        Missing::Error(
            "NoSuchLifecycleConfiguration",
            "The lifecycle configuration does not exist",
        ),
    ),
    config(
        "PutBucketEncryption",
        "GetBucketEncryption",
        Some("DeleteBucketEncryption"),
        "ServerSideEncryptionConfiguration",
        Missing::Default(|| {
            json!({ "Rules": [{
                "ApplyServerSideEncryptionByDefault": { "SSEAlgorithm": "AES256" },
                "BucketKeyEnabled": false,
            }] })
        }),
    ),
    config(
        "PutBucketWebsite",
        "GetBucketWebsite",
        Some("DeleteBucketWebsite"),
        "WebsiteConfiguration",
        Missing::Error(
            "NoSuchWebsiteConfiguration",
            "The specified bucket does not have a website configuration",
        ),
    ),
    config(
        "PutBucketNotificationConfiguration",
        "GetBucketNotificationConfiguration",
        None,
        "NotificationConfiguration",
        Missing::Empty,
    ),
    config(
        "PutBucketReplication",
        "GetBucketReplication",
        Some("DeleteBucketReplication"),
        "ReplicationConfiguration",
        Missing::Error(
            "ReplicationConfigurationNotFoundError",
            "The replication configuration was not found",
        ),
    ),
    config(
        "PutBucketOwnershipControls",
        "GetBucketOwnershipControls",
        Some("DeleteBucketOwnershipControls"),
        "OwnershipControls",
        Missing::Error(
            "OwnershipControlsNotFoundError",
            "The bucket ownership controls were not found",
        ),
    ),
    config(
        "PutPublicAccessBlock",
        "GetPublicAccessBlock",
        Some("DeletePublicAccessBlock"),
        "PublicAccessBlockConfiguration",
        Missing::Error(
            "NoSuchPublicAccessBlockConfiguration",
            "The public access block configuration was not found",
        ),
    ),
    config(
        "PutBucketLogging",
        "GetBucketLogging",
        None,
        "BucketLoggingStatus",
        Missing::Empty,
    ),
    config(
        "PutBucketAccelerateConfiguration",
        "GetBucketAccelerateConfiguration",
        None,
        "AccelerateConfiguration",
        Missing::Empty,
    ),
    config(
        "PutBucketRequestPayment",
        "GetBucketRequestPayment",
        None,
        "RequestPaymentConfiguration",
        Missing::Default(|| json!({ "Payer": "BucketOwner" })),
    ),
    Config {
        validate: Some(validate_object_lock),
        ..config(
            "PutObjectLockConfiguration",
            "GetObjectLockConfiguration",
            None,
            "ObjectLockConfiguration",
            Missing::Error(
                "ObjectLockConfigurationNotFoundError",
                "Object Lock configuration does not exist for this bucket",
            ),
        )
    },
    config(
        "PutBucketTagging",
        "GetBucketTagging",
        Some("DeleteBucketTagging"),
        "Tagging",
        Missing::Error("NoSuchTagSet", "The TagSet does not exist"),
    ),
    Config {
        validate: Some(validate_policy),
        ..config(
            "PutBucketPolicy",
            "GetBucketPolicy",
            Some("DeleteBucketPolicy"),
            "Policy",
            Missing::Error("NoSuchBucketPolicy", "The bucket policy does not exist"),
        )
    },
    config(
        "PutBucketAbac",
        "GetBucketAbac",
        None,
        "AbacStatus",
        Missing::Empty,
    ),
];

fn wraps_output(get: &str, member: &str) -> bool {
    let model = embedded_model!("s3");
    model.operations[get]
        .output
        .as_ref()
        .is_some_and(|o| model.registry.shape(&o.shape).members.contains_key(member))
}

fn stored_output(get: &str, member: &str, value: Value) -> Value {
    if wraps_output(get, member) {
        json!({ member: value })
    } else {
        value
    }
}

/// Bucket sub-resources that are stored and returned as given.
pub fn config_op(buckets: &mut Buckets, operation: &str, input: &Value) -> Option<OpResult> {
    if let Some(c) = CONFIGS
        .iter()
        .find(|c| [c.put, c.get].contains(&operation) || c.delete == Some(operation))
    {
        return Some(simple_config(c, buckets, operation, input));
    }
    super::KEYED_CONFIGS
        .iter()
        .find(|k| operation.contains(&format!("Bucket{k}Configuration")))
        .map(|k| keyed_config(k, buckets, operation, input))
}

fn simple_config(c: &Config, buckets: &mut Buckets, operation: &str, input: &Value) -> OpResult {
    let bucket = bucket_mut(buckets, input)?;
    if operation == c.put {
        let value = input.get(c.member).cloned().unwrap_or_else(|| json!({}));
        if let Some(validate) = c.validate {
            validate(bucket, &value)?;
        }
        bucket.configs.insert(c.member, value);
        return Ok(json!({}));
    }
    if Some(operation) == c.delete {
        bucket.configs.remove(c.member);
        return Ok(json!({}));
    }
    match (bucket.configs.get(c.member), &c.missing) {
        (Some(v), _) => Ok(stored_output(c.get, c.member, v.clone())),
        (None, Missing::Error(code, message)) => Err(with(
            err(404, code, *message),
            "BucketName",
            input.str_field("Bucket").unwrap_or_default(),
        )),
        (None, Missing::Empty) => Ok(json!({})),
        (None, Missing::Default(f)) => Ok(stored_output(c.get, c.member, f())),
    }
}

fn keyed_config(
    kind: &'static str,
    buckets: &mut Buckets,
    operation: &str,
    input: &Value,
) -> OpResult {
    let bucket = bucket_mut(buckets, input)?;
    let member = format!("{kind}Configuration");
    let id = input.str_field("Id").unwrap_or_default().to_string();
    let missing = || {
        err(
            404,
            "NoSuchConfiguration",
            "The specified configuration does not exist.",
        )
    };
    match operation.split("Bucket").next().unwrap_or_default() {
        "Put" => {
            let value = input.get(&member).cloned().unwrap_or_else(|| json!({}));
            bucket.keyed_configs.insert((kind, id), value);
            Ok(json!({}))
        }
        "Get" => bucket
            .keyed_configs
            .get(&(kind, id))
            .map(|v| json!({ member: v }))
            .ok_or_else(missing),
        "Delete" => bucket
            .keyed_configs
            .remove(&(kind, id))
            .map(|_| json!({}))
            .ok_or_else(missing),
        _ => {
            let list: Vec<Value> = bucket
                .keyed_configs
                .iter()
                .filter(|((k, _), _)| *k == kind)
                .map(|(_, v)| v.clone())
                .collect();
            Ok(json!({ format!("{member}List"): list, "IsTruncated": false }))
        }
    }
}

pub fn grants(ctx: &Ctx<'_>, canned: &str) -> Result<Value, AwsError> {
    let mut owner_grantee = owner(ctx);
    owner_grantee["Type"] = json!("CanonicalUser");
    let group = |uri: &str, permission: &str| json!({ "Grantee": { "URI": uri, "Type": "Group" }, "Permission": permission });
    let mut out = vec![json!({ "Grantee": owner_grantee, "Permission": "FULL_CONTROL" })];
    match canned {
        "private"
        | "bucket-owner-full-control"
        | "bucket-owner-read"
        | "aws-exec-read"
        | "log-delivery-write" => {}
        "public-read" => out.push(group(ALL_USERS, "READ")),
        "public-read-write" => {
            out.push(group(ALL_USERS, "READ"));
            out.push(group(ALL_USERS, "WRITE"));
        }
        "authenticated-read" => out.push(group(AUTHENTICATED_USERS, "READ")),
        other => {
            return Err(super::invalid_argument(
                format!("Invalid canned ACL {other}"),
                "x-amz-acl",
                other,
            ));
        }
    }
    Ok(Value::Array(out))
}

/// Grants from an `x-amz-acl` header or an `AccessControlPolicy` body.
pub fn acl_from(ctx: &Ctx<'_>, input: &Value) -> Result<Value, AwsError> {
    if let Some(g) = input
        .obj_field("AccessControlPolicy")
        .and_then(|p| p.get("Grants"))
    {
        return Ok(g.clone());
    }
    grants(ctx, input.str_field("ACL").unwrap_or("private"))
}

fn valid_bucket_name(name: &str) -> bool {
    let b = name.as_bytes();
    (3..=63).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'.' || *c == b'-')
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && !name.contains("..")
        && !name.split('.').all(|p| p.parse::<u8>().is_ok())
}

pub fn account_op(
    accounts: &mut HashMap<String, Buckets>,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> OpResult {
    if operation == "ListBuckets" {
        return list_buckets(accounts.entry(ctx.account.clone()).or_default(), ctx, input);
    }
    let name = input.require_str("Bucket")?.to_string();
    if !valid_bucket_name(&name) {
        return Err(with(
            err(
                400,
                "InvalidBucketName",
                "The specified bucket is not valid.",
            ),
            "BucketName",
            name,
        ));
    }
    if let Some(existing) = accounts.get(&ctx.account).and_then(|b| b.get(&name)) {
        if existing.region == "us-east-1" && ctx.region == "us-east-1" {
            return Ok(json!({ "Location": format!("/{name}") }));
        }
        return Err(with(
            err(
                409,
                "BucketAlreadyOwnedByYou",
                "Your previous request to create the named bucket succeeded and you already own it.",
            ),
            "BucketName",
            name,
        ));
    }
    if accounts.values().any(|b| b.contains_key(&name)) {
        return Err(with(
            err(
                409,
                "BucketAlreadyExists",
                "The requested bucket name is not available. The bucket namespace is shared by all users of the system. Please select a different name and try again.",
            ),
            "BucketName",
            name,
        ));
    }
    let constraint = input
        .obj_field("CreateBucketConfiguration")
        .and_then(|c| c.get("LocationConstraint"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let region = match (ctx.region.as_str(), constraint) {
        ("us-east-1", Some("us-east-1")) => {
            return Err(with(
                err(
                    400,
                    "InvalidLocationConstraint",
                    "The specified location-constraint is not valid",
                ),
                "LocationConstraint",
                "us-east-1",
            ));
        }
        ("us-east-1", Some(c)) => c.to_string(),
        ("us-east-1", None) => "us-east-1".to_string(),
        (r, Some(c)) if c == r => c.to_string(),
        (_, c) => {
            return Err(with(
                err(
                    400,
                    "IllegalLocationConstraintException",
                    format!(
                        "The {} location constraint is incompatible for the region specific endpoint this request was sent to.",
                        c.unwrap_or("unspecified")
                    ),
                ),
                "LocationConstraint",
                c.unwrap_or_default(),
            ));
        }
    };
    let mut bucket = Bucket::new(region.clone(), now_secs(ctx));
    bucket.acl = grants(ctx, input.str_field("ACL").unwrap_or("private"))?;
    if input
        .get("ObjectLockEnabledForBucket")
        .and_then(Value::as_bool)
        == Some(true)
    {
        bucket.versioning = Some("Enabled".into());
        bucket.configs.insert(
            "ObjectLockConfiguration",
            json!({ "ObjectLockEnabled": "Enabled" }),
        );
    }
    if let Some(o) = input.str_field("ObjectOwnership") {
        bucket.configs.insert(
            "OwnershipControls",
            json!({ "Rules": [{ "ObjectOwnership": o }] }),
        );
    }
    let location = if region == "us-east-1" {
        format!("/{name}")
    } else {
        format!("http://{name}.s3.{region}.amazonaws.com/")
    };
    accounts
        .entry(ctx.account.clone())
        .or_default()
        .insert(name.clone(), bucket);
    Ok(json!({ "Location": location, "BucketArn": format!("arn:aws:s3:::{name}") }))
}

fn list_buckets(buckets: &Buckets, ctx: &Ctx<'_>, input: &Value) -> OpResult {
    let prefix = input.str_field("Prefix").unwrap_or_default();
    let after = input.str_field("ContinuationToken").unwrap_or_default();
    let max = input
        .i64_field("MaxBuckets")
        .map_or(10000, |m| m.clamp(1, 10000) as usize);
    let mut matching = buckets
        .iter()
        .filter(|(n, b)| {
            n.starts_with(prefix)
                && n.as_str() > after
                && input
                    .str_field("BucketRegion")
                    .is_none_or(|r| r == b.region)
        })
        .peekable();
    let mut out_buckets = Vec::new();
    while out_buckets.len() < max {
        let Some((name, b)) = matching.next() else {
            break;
        };
        out_buckets.push(json!({
            "Name": name,
            "CreationDate": b.created,
            "BucketRegion": b.region,
            "BucketArn": format!("arn:aws:s3:::{name}"),
        }));
    }
    let mut out = Map::new();
    if matching.peek().is_some() {
        put(
            &mut out,
            "ContinuationToken",
            out_buckets
                .last()
                .and_then(|b| b["Name"].as_str())
                .map(str::to_string),
        );
    }
    out.insert("Buckets".into(), Value::Array(out_buckets));
    out.insert("Owner".into(), owner(ctx));
    put(&mut out, "Prefix", input.str_field("Prefix"));
    Ok(Value::Object(out))
}

fn policy_is_public(policy: &str) -> bool {
    let Ok(doc) = serde_json::from_str::<Value>(policy) else {
        return false;
    };
    let statements = match &doc["Statement"] {
        Value::Array(a) => a.clone(),
        s => vec![s.clone()],
    };
    statements.iter().any(|s| {
        s["Effect"] == "Allow"
            && (s["Principal"] == "*" || s["Principal"]["AWS"] == "*")
            && s.get("Condition").is_none()
    })
}

pub fn bucket_op(
    buckets: &mut Buckets,
    ctx: &mut Ctx<'_>,
    operation: &str,
    input: &Value,
) -> OpResult {
    let name = input.require_str("Bucket")?.to_string();
    match operation {
        "DeleteBucket" => {
            let bucket = bucket_ref(buckets, input)?;
            if !bucket.objects.is_empty() {
                return Err(with(
                    err(
                        409,
                        "BucketNotEmpty",
                        "The bucket you tried to delete is not empty",
                    ),
                    "BucketName",
                    name,
                ));
            }
            buckets.remove(&name);
            Ok(json!({}))
        }
        "HeadBucket" => {
            let bucket = bucket_ref(buckets, input)?;
            Ok(json!({
                "BucketRegion": bucket.region,
                "AccessPointAlias": false,
                "BucketArn": format!("arn:aws:s3:::{name}"),
            }))
        }
        "GetBucketLocation" => {
            let bucket = bucket_ref(buckets, input)?;
            Ok(if bucket.region == "us-east-1" {
                json!({})
            } else {
                json!({ "LocationConstraint": bucket.region })
            })
        }
        "PutBucketVersioning" => {
            let bucket = bucket_mut(buckets, input)?;
            let config = input.obj_field("VersioningConfiguration");
            let status = config.and_then(|c| c.get("Status")).and_then(Value::as_str);
            match status {
                Some("Enabled") => {}
                Some("Suspended") if bucket.configs.contains_key("ObjectLockConfiguration") => {
                    return Err(err(
                        409,
                        "InvalidBucketState",
                        "An Object Lock configuration is present on this bucket, so the versioning state cannot be changed.",
                    ));
                }
                Some("Suspended") => {}
                _ => {
                    return Err(err(
                        400,
                        "MalformedXML",
                        "The XML you provided was not well-formed or did not validate against our published schema",
                    ));
                }
            }
            bucket.versioning = status.map(str::to_string);
            if let Some(m) = config
                .and_then(|c| c.get("MFADelete"))
                .and_then(Value::as_str)
            {
                bucket.mfa_delete = Some(m.to_string());
            }
            Ok(json!({}))
        }
        "GetBucketVersioning" => {
            let bucket = bucket_ref(buckets, input)?;
            let mut out = Map::new();
            put(&mut out, "Status", bucket.versioning.clone());
            put(&mut out, "MFADelete", bucket.mfa_delete.clone());
            Ok(Value::Object(out))
        }
        "GetBucketAcl" => {
            let bucket = bucket_ref(buckets, input)?;
            Ok(json!({ "Owner": owner(ctx), "Grants": bucket.acl }))
        }
        "PutBucketAcl" => {
            let acl = acl_from(ctx, input)?;
            bucket_mut(buckets, input)?.acl = acl;
            Ok(json!({}))
        }
        "GetBucketPolicyStatus" => {
            let bucket = bucket_ref(buckets, input)?;
            let policy = bucket
                .configs
                .get("Policy")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    err(
                        404,
                        "NoSuchBucketPolicy",
                        "The bucket policy does not exist",
                    )
                })?;
            Ok(json!({ "PolicyStatus": { "IsPublic": policy_is_public(policy) } }))
        }
        _ => Err(no_such_bucket(&name)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_names() {
        assert!(valid_bucket_name("my-bucket.logs"));
        assert!(!valid_bucket_name("ab"));
        assert!(!valid_bucket_name("Upper"));
        assert!(!valid_bucket_name("-dash"));
        assert!(!valid_bucket_name("a..b"));
        assert!(!valid_bucket_name("192.168.0.1"));
    }

    #[test]
    fn public_policy() {
        assert!(policy_is_public(
            r#"{"Statement":[{"Effect":"Allow","Principal":"*","Action":"s3:GetObject"}]}"#
        ));
        assert!(!policy_is_public(
            r#"{"Statement":[{"Effect":"Allow","Principal":{"AWS":"arn:aws:iam::1:root"}}]}"#
        ));
    }
}
