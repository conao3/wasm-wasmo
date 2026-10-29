//! Account-level API Gateway resources: the account, API keys, usage plans,
//! custom domains, client certificates, VPC links, and tags.

use std::collections::BTreeMap;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::store::{Obj, bad_request, conflict, gen_id, not_found, page, patch, pick, secs};
use super::{Region, UsagePlan};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt};

const SDK_TYPES: &[(&str, &str)] = &[
    ("android", "Android SDK"),
    ("javascript", "JavaScript SDK"),
    ("ios-objective-c", "iOS Objective-C SDK"),
    ("ios-swift", "iOS Swift SDK"),
    ("java", "Java SDK"),
    ("ruby", "Ruby SDK"),
];

fn entry<'a>(
    map: &'a mut IndexMap<String, Obj>,
    id: &str,
    what: &str,
) -> Result<&'a mut Obj, AwsError> {
    map.get_mut(id)
        .ok_or_else(|| not_found(format!("Invalid {what} identifier specified")))
}

/// Get, update, or delete one entry of a collection.
fn crud(
    map: &mut IndexMap<String, Obj>,
    id: &str,
    what: &str,
    operation: &str,
    input: &Value,
    now: Value,
) -> Result<Value, AwsError> {
    let v = entry(map, id, what)?;
    if operation.starts_with("Update") {
        patch(v, input)?;
        if v.contains_key("lastUpdatedDate") {
            v.insert("lastUpdatedDate".into(), now);
        }
        return Ok(Value::Object(v.clone()));
    }
    if operation.starts_with("Delete") {
        map.shift_remove(id);
        return Ok(json!({}));
    }
    Ok(Value::Object(v.clone()))
}

fn list(map: &IndexMap<String, Obj>, input: &Value) -> Value {
    page(
        map.values().cloned().map(Value::Object).collect(),
        input,
        "items",
        "position",
    )
}

fn api_key(ctx: &mut Ctx<'_>, input: &Value) -> Obj {
    let id = gen_id(ctx, 10);
    let mut v = pick(
        input,
        &[
            "name",
            "description",
            "enabled",
            "customerId",
            "tags",
            "stageKeys",
        ],
    );
    v.insert("id".into(), json!(id));
    let value = match input.str_field("value") {
        Some(x) => x.to_string(),
        None => gen_id(ctx, 40),
    };
    v.insert("value".into(), json!(value));
    v.entry("enabled").or_insert(json!(false));
    v.insert("createdDate".into(), secs(ctx));
    v.insert("lastUpdatedDate".into(), secs(ctx));
    v
}

fn without_value(mut v: Obj, include: bool) -> Value {
    if !include {
        v.remove("value");
    }
    Value::Object(v)
}

pub(super) fn call(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    let now = secs(ctx);
    let include = input.get("includeValue").and_then(Value::as_bool) == Some(true);
    Ok(Some(match operation {
        "GetAccount" | "UpdateAccount" => {
            let a = &mut region.account;
            if a.is_empty() {
                a.insert(
                    "throttleSettings".into(),
                    json!({ "burstLimit": 5000, "rateLimit": 10000.0 }),
                );
                a.insert("features".into(), json!(["UsagePlans"]));
                a.insert("apiKeyVersion".into(), json!("4"));
            }
            if operation == "UpdateAccount" {
                patch(a, input)?;
            }
            Value::Object(a.clone())
        }
        "CreateApiKey" => {
            let v = api_key(ctx, input);
            region
                .api_keys
                .insert(v["id"].as_str().unwrap_or_default().to_string(), v.clone());
            Value::Object(v)
        }
        "ImportApiKeys" => {
            let csv = input
                .str_field("body")
                .and_then(|b| {
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b).ok()
                })
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let mut lines = csv.lines().filter(|l| !l.trim().is_empty());
            let header: Vec<String> = lines
                .next()
                .unwrap_or_default()
                .split(',')
                .map(|h| h.trim().to_lowercase())
                .collect();
            let mut ids = Vec::new();
            for line in lines {
                let cols: Vec<&str> = line.split(',').map(str::trim).collect();
                let field = |name: &str| {
                    header
                        .iter()
                        .position(|h| h == name)
                        .and_then(|i| cols.get(i))
                        .map(|s| s.to_string())
                };
                let key_input = json!({
                    "name": field("name"),
                    "value": field("key"),
                    "description": field("description"),
                    "enabled": field("enabled").is_some_and(|e| e.eq_ignore_ascii_case("true")),
                });
                let v = api_key(ctx, &key_input);
                let id = v["id"].as_str().unwrap_or_default().to_string();
                region.api_keys.insert(id.clone(), v);
                ids.push(id);
            }
            json!({ "ids": ids, "warnings": [] })
        }
        "GetApiKeys" => {
            let name = input.str_field("nameQuery");
            let items = region
                .api_keys
                .values()
                .filter(|k| {
                    name.is_none_or(|n| {
                        k.get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|x| x.starts_with(n))
                    })
                })
                .map(|k| without_value(k.clone(), include))
                .collect();
            page(items, input, "items", "position")
        }
        "GetApiKey" => without_value(
            entry(
                &mut region.api_keys,
                input.str_field("apiKey").unwrap_or_default(),
                "API Key",
            )?
            .clone(),
            include,
        ),
        "UpdateApiKey" | "DeleteApiKey" => crud(
            &mut region.api_keys,
            input.str_field("apiKey").unwrap_or_default(),
            "API Key",
            operation,
            input,
            now,
        )?,
        "CreateUsagePlan" => {
            let id = gen_id(ctx, 6);
            let mut v = pick(
                input,
                &[
                    "name",
                    "description",
                    "apiStages",
                    "throttle",
                    "quota",
                    "tags",
                ],
            );
            v.insert("id".into(), json!(id));
            v.entry("apiStages").or_insert(json!([]));
            region.usage_plans.insert(
                id,
                UsagePlan {
                    info: v.clone(),
                    keys: IndexMap::new(),
                },
            );
            Value::Object(v)
        }
        "GetUsagePlans" => {
            let key = input.str_field("keyId");
            let items = region
                .usage_plans
                .values()
                .filter(|p| key.is_none_or(|k| p.keys.contains_key(k)))
                .map(|p| Value::Object(p.info.clone()))
                .collect();
            page(items, input, "items", "position")
        }
        "GetUsagePlan" | "UpdateUsagePlan" | "DeleteUsagePlan" => {
            let id = input
                .str_field("usagePlanId")
                .unwrap_or_default()
                .to_string();
            let p = region
                .usage_plans
                .get_mut(&id)
                .ok_or_else(|| not_found("Invalid Usage Plan ID specified"))?;
            match operation {
                "UpdateUsagePlan" => {
                    patch(&mut p.info, input)?;
                    Value::Object(p.info.clone())
                }
                "DeleteUsagePlan" => {
                    region.usage_plans.shift_remove(&id);
                    json!({})
                }
                _ => Value::Object(p.info.clone()),
            }
        }
        "CreateUsagePlanKey" | "GetUsagePlanKeys" | "GetUsagePlanKey" | "DeleteUsagePlanKey"
        | "GetUsage" | "UpdateUsage" => {
            let id = input
                .str_field("usagePlanId")
                .unwrap_or_default()
                .to_string();
            let p = region
                .usage_plans
                .get_mut(&id)
                .ok_or_else(|| not_found("Invalid Usage Plan ID specified"))?;
            let key_id = input.str_field("keyId").unwrap_or_default().to_string();
            match operation {
                "CreateUsagePlanKey" => {
                    let k = region
                        .api_keys
                        .get(&key_id)
                        .ok_or_else(|| not_found("Invalid API Key identifier specified"))?;
                    if p.keys.contains_key(&key_id) {
                        return Err(conflict("Usage Plan already contains specified API key"));
                    }
                    let v = Map::from_iter([
                        ("id".to_string(), json!(key_id)),
                        (
                            "type".to_string(),
                            json!(input.str_field("keyType").unwrap_or("API_KEY")),
                        ),
                        (
                            "value".to_string(),
                            k.get("value").cloned().unwrap_or(Value::Null),
                        ),
                        (
                            "name".to_string(),
                            k.get("name").cloned().unwrap_or(Value::Null),
                        ),
                    ]);
                    p.keys.insert(key_id, v.clone());
                    Value::Object(v)
                }
                "GetUsagePlanKeys" => page(
                    p.keys.values().cloned().map(Value::Object).collect(),
                    input,
                    "items",
                    "position",
                ),
                "GetUsagePlanKey" => Value::Object(
                    p.keys
                        .get(&key_id)
                        .cloned()
                        .ok_or_else(|| not_found("Invalid Usage Plan Key identifier specified"))?,
                ),
                "DeleteUsagePlanKey" => {
                    p.keys
                        .shift_remove(&key_id)
                        .ok_or_else(|| not_found("Invalid Usage Plan Key identifier specified"))?;
                    json!({})
                }
                _ => {
                    let items: Map<String, Value> = p
                        .keys
                        .keys()
                        .map(|k| (k.clone(), json!([[0, 0]])))
                        .collect();
                    json!({
                        "usagePlanId": id,
                        "startDate": input.str_field("startDate"),
                        "endDate": input.str_field("endDate"),
                        "items": items,
                    })
                }
            }
        }
        "GetSdkTypes" => json!({
            "items": SDK_TYPES.iter().map(|(id, name)| json!({ "id": id, "friendlyName": name, "description": name, "configurationProperties": [] })).collect::<Vec<_>>(),
        }),
        "GetSdkType" => {
            let id = input.str_field("id").unwrap_or_default();
            let (id, name) = SDK_TYPES
                .iter()
                .find(|(x, _)| *x == id)
                .ok_or_else(|| not_found("Invalid SDK type specified"))?;
            json!({ "id": id, "friendlyName": name, "description": name, "configurationProperties": [] })
        }
        "GetTags" | "TagResource" | "UntagResource" => tags(region, operation, input)?,
        _ => return Ok(None),
    }))
}

/// The stored object an `arn:aws:apigateway:<region>::/<path>` tag ARN names.
fn tag_target<'r>(region: &'r mut Region, arn: &str) -> Option<&'r mut Obj> {
    let path = arn.split_once("::")?.1;
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match parts.as_slice() {
        ["restapis", id] => region.apis.get_mut(*id).map(|a| &mut a.info),
        ["restapis", id, "stages", name] => region.apis.get_mut(*id)?.stages.get_mut(*name),
        ["apikeys", id] => region.api_keys.get_mut(*id),
        ["usageplans", id] => region.usage_plans.get_mut(*id).map(|p| &mut p.info),
        ["domainnames", name] => region.domains.get_mut(*name),
        ["clientcertificates", id] => region.certificates.get_mut(*id),
        ["vpclinks", id] => region.vpc_links.get_mut(*id),
        _ => None,
    }
}

fn tags(region: &mut Region, operation: &str, input: &Value) -> Result<Value, AwsError> {
    let arn = input
        .str_field("resourceArn")
        .unwrap_or_default()
        .to_string();
    let target = tag_target(region, &arn)
        .ok_or_else(|| not_found(format!("Invalid resource ARN specified: {arn}")))?;
    let tags = target.entry("tags").or_insert_with(|| json!({}));
    let mut map: BTreeMap<String, Value> = tags
        .as_object()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .collect();
    match operation {
        "TagResource" => {
            for (k, v) in input.obj_field("tags").into_iter().flatten() {
                map.insert(k.clone(), v.clone());
            }
        }
        "UntagResource" => {
            for k in input
                .array_field("tagKeys")
                .iter()
                .filter_map(Value::as_str)
            {
                map.remove(k);
            }
        }
        _ => return Ok(json!({ "tags": map })),
    }
    *tags = json!(map);
    Ok(json!({}))
}

pub(super) fn domains(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    let now = secs(ctx);
    Ok(Some(match operation {
        "CreateDomainName" => {
            let name = input
                .require_str("domainName")
                .map_err(|e| bad_request(e.message))?
                .to_string();
            if region.domains.contains_key(&name) {
                return Err(conflict("The domain name you provided already exists."));
            }
            let mut v = pick(
                input,
                &[
                    "domainName",
                    "certificateName",
                    "certificateArn",
                    "regionalCertificateName",
                    "regionalCertificateArn",
                    "endpointConfiguration",
                    "tags",
                    "securityPolicy",
                    "mutualTlsAuthentication",
                    "policy",
                    "routingMode",
                ],
            );
            let edge = input.str_field("regionalCertificateArn").is_none();
            let distribution = format!("{}.cloudfront.net", gen_id(ctx, 14));
            if edge {
                v.insert("distributionDomainName".into(), json!(distribution));
                v.insert("distributionHostedZoneId".into(), json!("Z2FDTNDATAQYW2"));
            } else {
                v.insert(
                    "regionalDomainName".into(),
                    json!(format!(
                        "d-{}.execute-api.{}.amazonaws.com",
                        gen_id(ctx, 10),
                        ctx.region
                    )),
                );
                v.insert("regionalHostedZoneId".into(), json!("Z1UJRXOUMOOFQ8"));
            }
            v.entry("endpointConfiguration")
                .or_insert(json!({ "types": [if edge { "EDGE" } else { "REGIONAL" }] }));
            v.insert("domainNameStatus".into(), json!("AVAILABLE"));
            v.insert("certificateUploadDate".into(), now);
            v.insert(
                "domainNameArn".into(),
                json!(format!(
                    "arn:aws:apigateway:{}::/domainnames/{name}",
                    ctx.region
                )),
            );
            region.domains.insert(name, v.clone());
            Value::Object(v)
        }
        "GetDomainNames" => list(&region.domains, input),
        "GetDomainName" | "UpdateDomainName" | "DeleteDomainName" => {
            let name = input
                .str_field("domainName")
                .unwrap_or_default()
                .to_string();
            if operation == "DeleteDomainName" {
                region.mappings.remove(&name);
            }
            crud(
                &mut region.domains,
                &name,
                "domain name",
                operation,
                input,
                now,
            )?
        }
        "CreateBasePathMapping"
        | "GetBasePathMappings"
        | "GetBasePathMapping"
        | "UpdateBasePathMapping"
        | "DeleteBasePathMapping" => {
            let domain = input
                .str_field("domainName")
                .unwrap_or_default()
                .to_string();
            if !region.domains.contains_key(&domain) {
                return Err(not_found("Invalid domain name identifier specified"));
            }
            let mappings = region.mappings.entry(domain).or_default();
            let base = input.str_field("basePath").unwrap_or("(none)").to_string();
            match operation {
                "CreateBasePathMapping" => {
                    let api = input.str_field("restApiId").unwrap_or_default();
                    if !region.apis.contains_key(api) {
                        return Err(bad_request("Invalid REST API identifier specified"));
                    }
                    if mappings.contains_key(&base) {
                        return Err(conflict("Base path already exists for this domain name"));
                    }
                    let mut v = pick(input, &["restApiId", "stage"]);
                    v.insert("basePath".into(), json!(base));
                    mappings.insert(base, v.clone());
                    Value::Object(v)
                }
                "GetBasePathMappings" => list(mappings, input),
                _ => crud(mappings, &base, "base path mapping", operation, input, now)?,
            }
        }
        "GenerateClientCertificate" => {
            let id = gen_id(ctx, 6);
            let mut v = pick(input, &["description", "tags"]);
            v.insert("clientCertificateId".into(), json!(id));
            v.insert(
                "pemEncodedCertificate".into(),
                json!("-----BEGIN CERTIFICATE-----\nd2FzbW8gY2xpZW50IGNlcnRpZmljYXRl\n-----END CERTIFICATE-----\n"),
            );
            v.insert("createdDate".into(), now.clone());
            v.insert(
                "expirationDate".into(),
                json!(now.as_f64().unwrap_or_default() + 365.0 * 86400.0),
            );
            region.certificates.insert(id, v.clone());
            Value::Object(v)
        }
        "GetClientCertificates" => list(&region.certificates, input),
        "GetClientCertificate" | "UpdateClientCertificate" | "DeleteClientCertificate" => crud(
            &mut region.certificates,
            input.str_field("clientCertificateId").unwrap_or_default(),
            "client certificate",
            operation,
            input,
            now,
        )?,
        "CreateVpcLink" => {
            let id = gen_id(ctx, 6);
            let mut v = pick(input, &["name", "description", "targetArns", "tags"]);
            v.insert("id".into(), json!(id));
            v.insert("status".into(), json!("AVAILABLE"));
            region.vpc_links.insert(id, v.clone());
            Value::Object(v)
        }
        "GetVpcLinks" => list(&region.vpc_links, input),
        "GetVpcLink" | "UpdateVpcLink" | "DeleteVpcLink" => crud(
            &mut region.vpc_links,
            input.str_field("vpcLinkId").unwrap_or_default(),
            "VPC link",
            operation,
            input,
            now,
        )?,
        "CreateDomainNameAccessAssociation" => {
            let domain = input
                .require_str("domainNameArn")
                .map_err(|e| bad_request(e.message))?;
            let arn = format!("{domain}+{}", gen_id(ctx, 10));
            let mut v = pick(
                input,
                &[
                    "domainNameArn",
                    "accessAssociationSourceType",
                    "accessAssociationSource",
                    "tags",
                ],
            );
            v.insert("domainNameAccessAssociationArn".into(), json!(arn));
            region.associations.insert(arn, v.clone());
            Value::Object(v)
        }
        "GetDomainNameAccessAssociations" => list(&region.associations, input),
        "DeleteDomainNameAccessAssociation" => crud(
            &mut region.associations,
            input
                .str_field("domainNameAccessAssociationArn")
                .unwrap_or_default(),
            "domain name access association",
            operation,
            input,
            now,
        )?,
        "RejectDomainNameAccessAssociation" => {
            let arn = input
                .str_field("domainNameAccessAssociationArn")
                .unwrap_or_default();
            region.associations.shift_remove(arn);
            json!({})
        }
        _ => return Ok(None),
    }))
}
