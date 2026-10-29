use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use super::{MapExt, Region, conflict, invalid, not_found, now_iso, sha256_b64};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt};

#[derive(Debug, Clone, Default)]
pub(super) struct Layer {
    last: u64,
    versions: BTreeMap<u64, LayerVersion>,
}

#[derive(Debug, Clone)]
struct LayerVersion {
    info: Map<String, Value>,
    size: usize,
    policy: Vec<Value>,
    revision: String,
}

fn layer_name(raw: &str) -> &str {
    match raw.split(':').collect::<Vec<_>>().as_slice() {
        ["arn", _, _, _, _, "layer", name, ..] => name,
        _ => raw,
    }
}

/// Code size of a layer version in this region, addressed by its ARN.
pub(super) fn version_size(region: &Region, arn: &str) -> Option<usize> {
    let (name, version) = parse_version_arn(arn)?;
    region
        .layers
        .get(name)?
        .versions
        .get(&version)
        .map(|v| v.size)
}

fn parse_version_arn(arn: &str) -> Option<(&str, u64)> {
    match arn.split(':').collect::<Vec<_>>().as_slice() {
        ["arn", _, _, _, _, "layer", name, version] => Some((name, version.parse().ok()?)),
        _ => None,
    }
}

fn summary(v: &LayerVersion) -> Value {
    let mut out = v.info.clone();
    out.remove("Content");
    out.remove("LayerArn");
    Value::Object(out)
}

fn missing() -> AwsError {
    not_found("The resource you requested does not exist.")
}

fn version_number(input: &Value) -> Result<u64, AwsError> {
    input
        .i64_field("VersionNumber")
        .filter(|n| *n > 0)
        .map(|n| n as u64)
        .ok_or_else(|| invalid("VersionNumber is required."))
}

pub(super) fn call(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    Ok(Some(match operation {
        "PublishLayerVersion" => {
            let name = layer_name(input.require_str("LayerName")?).to_string();
            let content = input.obj_field("Content").cloned().unwrap_or_default();
            let zip = content
                .get("ZipFile")
                .and_then(Value::as_str)
                .and_then(|z| STANDARD.decode(z).ok());
            let (size, sha) = match &zip {
                Some(z) => (z.len(), sha256_b64(z)),
                None => (
                    0,
                    sha256_b64(
                        format!(
                            "{}/{}",
                            content.str_field("S3Bucket").unwrap_or_default(),
                            content.str_field("S3Key").unwrap_or_default()
                        )
                        .as_bytes(),
                    ),
                ),
            };
            let layer = region.layers.entry(name.clone()).or_default();
            layer.last += 1;
            let n = layer.last;
            let layer_arn = ctx.arn("lambda", &format!("layer:{name}"));
            let mut info = Map::new();
            info.insert(
                "Content".into(),
                json!({
                    "Location": format!("{}/wasmo-lambda-layers/{name}/{n}", ctx.endpoint),
                    "CodeSha256": sha,
                    "CodeSize": size,
                }),
            );
            info.insert("LayerArn".into(), json!(layer_arn));
            info.insert("LayerVersionArn".into(), json!(format!("{layer_arn}:{n}")));
            info.insert(
                "Description".into(),
                json!(input.str_field("Description").unwrap_or_default()),
            );
            info.insert("CreatedDate".into(), json!(now_iso(ctx)));
            info.insert("Version".into(), json!(n));
            for k in [
                "CompatibleRuntimes",
                "CompatibleArchitectures",
                "LicenseInfo",
            ] {
                if let Some(v) = input.get(k).filter(|v| !v.is_null()) {
                    info.insert(k.into(), v.clone());
                }
            }
            let revision = ctx.uuid();
            layer.versions.insert(
                n,
                LayerVersion {
                    info: info.clone(),
                    size,
                    policy: Vec::new(),
                    revision,
                },
            );
            Value::Object(info)
        }
        "ListLayers" => {
            let runtime = input.str_field("CompatibleRuntime");
            let arch = input.str_field("CompatibleArchitecture");
            let fits = |v: &&LayerVersion, key: &str, want: Option<&str>| {
                want.is_none_or(|w| {
                    v.info
                        .get(key)
                        .and_then(Value::as_array)
                        .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(w)))
                })
            };
            let items: Vec<Value> = region
                .layers
                .iter()
                .filter_map(|(name, l)| {
                    let latest = l.versions.values().rev().find(|v| {
                        fits(v, "CompatibleRuntimes", runtime)
                            && fits(v, "CompatibleArchitectures", arch)
                    })?;
                    Some(json!({
                        "LayerName": name,
                        "LayerArn": latest.info.get("LayerArn"),
                        "LatestMatchingVersion": summary(latest),
                    }))
                })
                .collect();
            json!({ "Layers": items })
        }
        "ListLayerVersions" => {
            let name = layer_name(input.require_str("LayerName")?);
            let items: Vec<Value> = region
                .layers
                .get(name)
                .into_iter()
                .flat_map(|l| l.versions.values().rev())
                .map(summary)
                .collect();
            json!({ "LayerVersions": items })
        }
        "GetLayerVersionByArn" => {
            let arn = input.require_str("Arn")?;
            let (name, n) = parse_version_arn(arn).ok_or_else(|| invalid(format!("1 validation error detected: Value '{arn}' at 'arn' failed to satisfy constraint")))?;
            let v = region
                .layers
                .get(name)
                .and_then(|l| l.versions.get(&n))
                .ok_or_else(missing)?;
            Value::Object(v.info.clone())
        }
        "GetLayerVersion"
        | "DeleteLayerVersion"
        | "AddLayerVersionPermission"
        | "RemoveLayerVersionPermission"
        | "GetLayerVersionPolicy" => {
            let name = layer_name(input.require_str("LayerName")?).to_string();
            let n = version_number(input)?;
            if operation == "DeleteLayerVersion" {
                if let Some(l) = region.layers.get_mut(&name) {
                    l.versions.remove(&n);
                }
                return Ok(Some(json!({})));
            }
            let revision = ctx.uuid();
            let v = region
                .layers
                .get_mut(&name)
                .and_then(|l| l.versions.get_mut(&n))
                .ok_or_else(missing)?;
            match operation {
                "GetLayerVersion" => Value::Object(v.info.clone()),
                "AddLayerVersionPermission" => {
                    let sid = input.require_str("StatementId")?;
                    if v.policy
                        .iter()
                        .any(|s| s.get("Sid").and_then(Value::as_str) == Some(sid))
                    {
                        return Err(conflict(format!(
                            "The statement id ({sid}) provided already exists. Please provide a new statement id, or remove the existing statement."
                        )));
                    }
                    let principal = input.require_str("Principal")?;
                    let mut s = json!({
                        "Sid": sid,
                        "Effect": "Allow",
                        "Principal": if principal == "*" { json!("*") } else { json!({ "AWS": format!("arn:aws:iam::{principal}:root") }) },
                        "Action": input.require_str("Action")?,
                        "Resource": v.info.get("LayerVersionArn"),
                    });
                    if let Some(org) = input.str_field("OrganizationId") {
                        s["Condition"] = json!({ "StringEquals": { "aws:PrincipalOrgID": org } });
                    }
                    v.policy.push(s.clone());
                    v.revision = revision;
                    json!({ "Statement": s.to_string(), "RevisionId": v.revision })
                }
                "RemoveLayerVersionPermission" => {
                    let sid = input.require_str("StatementId")?;
                    let before = v.policy.len();
                    v.policy
                        .retain(|s| s.get("Sid").and_then(Value::as_str) != Some(sid));
                    if v.policy.len() == before {
                        return Err(missing());
                    }
                    v.revision = revision;
                    json!({})
                }
                _ => {
                    if v.policy.is_empty() {
                        return Err(missing());
                    }
                    let policy =
                        json!({ "Version": "2012-10-17", "Id": "default", "Statement": v.policy });
                    json!({ "Policy": policy.to_string(), "RevisionId": v.revision })
                }
            }
        }
        _ => return Ok(None),
    }))
}
