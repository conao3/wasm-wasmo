//! API Gateway v2: HTTP and WebSocket APIs, their routes, integrations,
//! stages, deployments, authorizers, and models, custom domains, VPC links,
//! and portals. Collections share one create / get / update / delete / list
//! engine keyed by the operation name.

mod invoke;

use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::apigateway::store::{Obj, bad_request, conflict, gen_id, not_found};
use crate::codec::AwsError;
use crate::codec::time::format_iso8601;
use crate::embedded_model;
use crate::service::{Ctx, InputExt, OpResult, Service, ServiceModel};
use crate::wire::{Request, Response};

#[derive(Debug, Clone, Default)]
struct Deployment {
    info: Obj,
    routes: IndexMap<String, Obj>,
    integrations: IndexMap<String, Obj>,
}

#[derive(Debug, Clone, Default)]
struct Api {
    info: Obj,
    routes: IndexMap<String, Obj>,
    integrations: IndexMap<String, Obj>,
    integration_responses: HashMap<String, IndexMap<String, Obj>>,
    route_responses: HashMap<String, IndexMap<String, Obj>>,
    stages: IndexMap<String, Obj>,
    deployments: IndexMap<String, Deployment>,
    authorizers: IndexMap<String, Obj>,
    models: IndexMap<String, Obj>,
}

#[derive(Debug, Default)]
struct Region {
    apis: IndexMap<String, Api>,
    domains: IndexMap<String, Obj>,
    mappings: HashMap<String, IndexMap<String, Obj>>,
    rules: HashMap<String, IndexMap<String, Obj>>,
    vpc_links: IndexMap<String, Obj>,
    portals: IndexMap<String, Obj>,
    products: IndexMap<String, Obj>,
    pages: HashMap<String, IndexMap<String, Obj>>,
    endpoint_pages: HashMap<String, IndexMap<String, Obj>>,
    sharing: HashMap<String, Obj>,
}

#[derive(Default)]
pub struct ApiGatewayV2 {
    regions: HashMap<(String, String), Region>,
    pending: HashMap<String, invoke::Pending>,
}

#[derive(Clone, Copy, PartialEq)]
enum Scope {
    Region,
    Api,
    Integration,
    Route,
    Domain,
    Product,
}

/// A collection: its operation noun, id member, where it lives, and whether
/// the id comes from the request.
struct Kind {
    noun: &'static str,
    id: &'static str,
    scope: Scope,
    named: bool,
}

const KINDS: &[Kind] = &[
    Kind {
        noun: "Api",
        id: "ApiId",
        scope: Scope::Region,
        named: false,
    },
    Kind {
        noun: "Route",
        id: "RouteId",
        scope: Scope::Api,
        named: false,
    },
    Kind {
        noun: "Integration",
        id: "IntegrationId",
        scope: Scope::Api,
        named: false,
    },
    Kind {
        noun: "IntegrationResponse",
        id: "IntegrationResponseId",
        scope: Scope::Integration,
        named: false,
    },
    Kind {
        noun: "RouteResponse",
        id: "RouteResponseId",
        scope: Scope::Route,
        named: false,
    },
    Kind {
        noun: "Stage",
        id: "StageName",
        scope: Scope::Api,
        named: true,
    },
    Kind {
        noun: "Deployment",
        id: "DeploymentId",
        scope: Scope::Api,
        named: false,
    },
    Kind {
        noun: "Authorizer",
        id: "AuthorizerId",
        scope: Scope::Api,
        named: false,
    },
    Kind {
        noun: "Model",
        id: "ModelId",
        scope: Scope::Api,
        named: false,
    },
    Kind {
        noun: "DomainName",
        id: "DomainName",
        scope: Scope::Region,
        named: true,
    },
    Kind {
        noun: "ApiMapping",
        id: "ApiMappingId",
        scope: Scope::Domain,
        named: false,
    },
    Kind {
        noun: "RoutingRule",
        id: "RoutingRuleId",
        scope: Scope::Domain,
        named: false,
    },
    Kind {
        noun: "VpcLink",
        id: "VpcLinkId",
        scope: Scope::Region,
        named: false,
    },
    Kind {
        noun: "Portal",
        id: "PortalId",
        scope: Scope::Region,
        named: false,
    },
    Kind {
        noun: "PortalProduct",
        id: "PortalProductId",
        scope: Scope::Region,
        named: false,
    },
    Kind {
        noun: "ProductPage",
        id: "ProductPageId",
        scope: Scope::Product,
        named: false,
    },
    Kind {
        noun: "ProductRestEndpointPage",
        id: "ProductRestEndpointPageId",
        scope: Scope::Product,
        named: false,
    },
];

fn missing(noun: &str, id: &str) -> AwsError {
    not_found(format!("Invalid {noun} identifier specified {id}"))
}

fn now(ctx: &Ctx<'_>) -> Value {
    json!(format_iso8601((ctx.now_ms() / 1000.0).floor()))
}

fn parse_op(operation: &str) -> Option<(&'static str, &'static Kind)> {
    for verb in ["Create", "Get", "Update", "Delete", "List", "Put"] {
        let Some(rest) = operation.strip_prefix(verb) else {
            continue;
        };
        if let Some(k) = KINDS.iter().find(|k| k.noun == rest) {
            return Some((
                match verb {
                    "Create" => "create",
                    "Get" => "get",
                    "Update" => "update",
                    "Delete" => "delete",
                    _ => "put",
                },
                k,
            ));
        }
        if let Some(k) = KINDS
            .iter()
            .find(|k| rest.strip_suffix('s') == Some(k.noun))
        {
            return Some(("list", k));
        }
    }
    None
}

fn api<'r>(region: &'r mut Region, input: &Value) -> Result<&'r mut Api, AwsError> {
    let id = input.str_field("ApiId").unwrap_or_default();
    region.apis.get_mut(id).ok_or_else(|| missing("API", id))
}

/// The collection a kind lives in, for the parent the input addresses.
fn collection<'r>(
    region: &'r mut Region,
    kind: &Kind,
    input: &Value,
) -> Result<&'r mut IndexMap<String, Obj>, AwsError> {
    let parent = |key: &str| input.str_field(key).unwrap_or_default().to_string();
    Ok(match kind.scope {
        Scope::Region => match kind.noun {
            "DomainName" => &mut region.domains,
            "VpcLink" => &mut region.vpc_links,
            "Portal" => &mut region.portals,
            _ => &mut region.products,
        },
        Scope::Domain => {
            let d = parent("DomainName");
            if !region.domains.contains_key(&d) {
                return Err(missing("domain name", &d));
            }
            if kind.noun == "ApiMapping" {
                region.mappings.entry(d).or_default()
            } else {
                region.rules.entry(d).or_default()
            }
        }
        Scope::Product => {
            let p = parent("PortalProductId");
            if !region.products.contains_key(&p) {
                return Err(missing("portal product", &p));
            }
            if kind.noun == "ProductPage" {
                region.pages.entry(p).or_default()
            } else {
                region.endpoint_pages.entry(p).or_default()
            }
        }
        Scope::Api | Scope::Integration | Scope::Route => {
            let a = api(region, input)?;
            match kind.noun {
                "Route" => &mut a.routes,
                "Integration" => &mut a.integrations,
                "Stage" => &mut a.stages,
                "Authorizer" => &mut a.authorizers,
                "Model" => &mut a.models,
                "IntegrationResponse" => {
                    let i = parent("IntegrationId");
                    if !a.integrations.contains_key(&i) {
                        return Err(missing("Integration", &i));
                    }
                    a.integration_responses.entry(i).or_default()
                }
                _ => {
                    let r = parent("RouteId");
                    if !a.routes.contains_key(&r) {
                        return Err(missing("Route", &r));
                    }
                    a.route_responses.entry(r).or_default()
                }
            }
        }
    })
}

fn model() -> &'static ServiceModel {
    embedded_model!("apigatewayv2")
}

/// The members of `input` that describe the resource: everything but the
/// path and query members of `operation`.
fn fields(input: &Value, operation: &str) -> Obj {
    let m = model();
    let located: Vec<&str> = m
        .operations
        .get(operation)
        .and_then(|op| op.input.as_ref())
        .map(|i| {
            m.registry
                .shape(&i.shape)
                .members
                .iter()
                .filter(|(_, r)| r.location.is_some())
                .map(|(n, _)| n.as_str())
                .collect()
        })
        .unwrap_or_default();
    input
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, v)| !located.contains(&k.as_str()) && !v.is_null())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

fn set(v: &mut Obj, k: &str, x: Value) {
    v.entry(k).or_insert(x);
}

/// Defaults a new object of `kind` gets, as API Gateway fills them in.
fn defaults(ctx: &Ctx<'_>, kind: &Kind, v: &mut Obj, protocol: &str) {
    match kind.noun {
        "Route" => {
            set(v, "ApiKeyRequired", json!(false));
            set(v, "AuthorizationType", json!("NONE"));
        }
        "Integration" => {
            set(v, "ConnectionType", json!("INTERNET"));
            set(
                v,
                "TimeoutInMillis",
                json!(if protocol == "HTTP" { 30000 } else { 29000 }),
            );
            if protocol == "HTTP" {
                set(v, "PayloadFormatVersion", json!("2.0"));
            } else {
                set(v, "PassthroughBehavior", json!("WHEN_NO_MATCH"));
                set(v, "PayloadFormatVersion", json!("1.0"));
            }
        }
        "Stage" => {
            set(v, "CreatedDate", now(ctx));
            set(v, "LastUpdatedDate", now(ctx));
            set(
                v,
                "DefaultRouteSettings",
                json!({ "DetailedMetricsEnabled": false }),
            );
            set(v, "RouteSettings", json!({}));
            set(v, "StageVariables", json!({}));
            set(v, "Tags", json!({}));
        }
        "Authorizer" => {
            if v.get("AuthorizerType").and_then(Value::as_str) == Some("REQUEST") {
                set(v, "AuthorizerResultTtlInSeconds", json!(300));
            }
        }
        "DomainName" => {
            let name = v
                .get("DomainName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            set(
                v,
                "DomainNameArn",
                json!(format!(
                    "arn:aws:apigateway:{}::/domainnames/{name}",
                    ctx.region
                )),
            );
            set(
                v,
                "ApiMappingSelectionExpression",
                json!("$request.basepath"),
            );
            set(v, "RoutingMode", json!("API_MAPPING_ONLY"));
            if let Some(Value::Array(configs)) = v.get_mut("DomainNameConfigurations") {
                for c in configs.iter_mut().filter_map(Value::as_object_mut) {
                    c.entry("ApiGatewayDomainName").or_insert(json!(format!(
                        "d-{}.execute-api.{}.amazonaws.com",
                        name.len(),
                        ctx.region
                    )));
                    c.entry("HostedZoneId").or_insert(json!("Z1UJRXOUMOOFQ8"));
                    c.entry("DomainNameStatus").or_insert(json!("AVAILABLE"));
                    c.entry("EndpointType").or_insert(json!("REGIONAL"));
                    c.entry("SecurityPolicy").or_insert(json!("TLS_1_2"));
                }
            }
        }
        "VpcLink" => {
            set(v, "CreatedDate", now(ctx));
            set(v, "VpcLinkStatus", json!("AVAILABLE"));
            set(v, "VpcLinkVersion", json!("V2"));
            set(v, "SecurityGroupIds", json!([]));
        }
        "Portal" | "PortalProduct" | "ProductPage" | "ProductRestEndpointPage" => {
            set(v, "LastModified", now(ctx));
        }
        _ => {}
    }
}

fn page(items: Vec<Value>, input: &Value) -> Value {
    let start = input
        .str_field("NextToken")
        .and_then(|t| t.parse::<usize>().ok())
        .unwrap_or(0)
        .min(items.len());
    let limit = match input.i64_field("MaxResults") {
        Some(n) if n > 0 => n as usize,
        _ => 100,
    };
    let end = (start + limit).min(items.len());
    let mut out = json!({ "Items": items[start..end] });
    if end < items.len() {
        out["NextToken"] = json!(end.to_string());
    }
    out
}

/// Create, get, update, delete, put, or list one kind of collection.
fn generic(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    verb: &str,
    kind: &Kind,
    input: &Value,
    operation: &str,
) -> OpResult {
    let protocol = region
        .apis
        .get(input.str_field("ApiId").unwrap_or_default())
        .and_then(|a| a.info.get("ProtocolType"))
        .and_then(Value::as_str)
        .unwrap_or("HTTP")
        .to_string();
    let id = input.str_field(kind.id).map(str::to_string);
    let fresh = if kind.named {
        None
    } else {
        Some(gen_id(ctx, 7))
    };
    let map = collection(region, kind, input)?;
    Ok(match verb {
        "create" | "put" => {
            let id = match (kind.named || verb == "put", id) {
                (true, Some(id)) => id,
                (true, None) => return Err(bad_request(format!("{} is required", kind.id))),
                (false, _) => fresh.unwrap_or_default(),
            };
            if verb == "create" && map.contains_key(&id) {
                return Err(conflict(format!("{} {id} already exists", kind.noun)));
            }
            let mut v = fields(input, operation);
            v.insert(kind.id.into(), json!(id));
            defaults(ctx, kind, &mut v, &protocol);
            map.insert(id, v.clone());
            Value::Object(v)
        }
        "list" => page(map.values().cloned().map(Value::Object).collect(), input),
        _ => {
            let id = id.unwrap_or_default();
            let v = map.get_mut(&id).ok_or_else(|| missing(kind.noun, &id))?;
            match verb {
                "update" => {
                    v.extend(fields(input, operation));
                    if v.contains_key("LastUpdatedDate") {
                        v.insert("LastUpdatedDate".into(), now(ctx));
                    }
                    Value::Object(v.clone())
                }
                "delete" => {
                    map.shift_remove(&id);
                    json!({})
                }
                _ => Value::Object(v.clone()),
            }
        }
    })
}

fn create_api(ctx: &mut Ctx<'_>, region: &mut Region, input: &Value) -> OpResult {
    let name = input
        .require_str("Name")
        .map_err(|e| bad_request(e.message))?
        .to_string();
    let protocol = input
        .require_str("ProtocolType")
        .map_err(|e| bad_request(e.message))?
        .to_string();
    if protocol == "WEBSOCKET" && input.str_field("RouteSelectionExpression").is_none() {
        return Err(bad_request(
            "RouteSelectionExpression is required for WEBSOCKET APIs",
        ));
    }
    let id = gen_id(ctx, 10);
    let mut info = fields(input, "CreateApi");
    for k in ["Target", "RouteKey", "CredentialsArn"] {
        info.remove(k);
    }
    info.insert("ApiId".into(), json!(id));
    info.insert("Name".into(), json!(name));
    let scheme = if protocol == "WEBSOCKET" {
        "wss"
    } else {
        "https"
    };
    info.insert(
        "ApiEndpoint".into(),
        json!(format!(
            "{scheme}://{id}.execute-api.{}.amazonaws.com",
            ctx.region
        )),
    );
    info.insert("CreatedDate".into(), now(ctx));
    info.entry("ApiKeySelectionExpression")
        .or_insert(json!("$request.header.x-api-key"));
    info.entry("RouteSelectionExpression")
        .or_insert(json!("$request.method $request.path"));
    info.entry("DisableExecuteApiEndpoint")
        .or_insert(json!(false));
    info.entry("IpAddressType").or_insert(json!("ipv4"));
    info.entry("Tags").or_insert(json!({}));
    let mut a = Api {
        info,
        ..Api::default()
    };
    if let Some(target) = input.str_field("Target") {
        let integration = gen_id(ctx, 7);
        let lambda = target.contains(":lambda:");
        let mut i = Map::from_iter([
            ("IntegrationId".to_string(), json!(integration)),
            (
                "IntegrationType".to_string(),
                json!(if lambda { "AWS_PROXY" } else { "HTTP_PROXY" }),
            ),
            ("IntegrationUri".to_string(), json!(target)),
            (
                "IntegrationMethod".to_string(),
                json!(if lambda { "POST" } else { "ANY" }),
            ),
            ("ApiGatewayManaged".to_string(), json!(true)),
        ]);
        if let Some(c) = input.get("CredentialsArn") {
            i.insert("CredentialsArn".into(), c.clone());
        }
        defaults(ctx, &KINDS[2], &mut i, &protocol);
        a.integrations.insert(integration.clone(), i);
        let route = gen_id(ctx, 7);
        let mut r = Map::from_iter([
            ("RouteId".to_string(), json!(route)),
            (
                "RouteKey".to_string(),
                json!(input.str_field("RouteKey").unwrap_or("$default")),
            ),
            (
                "Target".to_string(),
                json!(format!("integrations/{integration}")),
            ),
            ("ApiGatewayManaged".to_string(), json!(true)),
        ]);
        defaults(ctx, &KINDS[1], &mut r, &protocol);
        a.routes.insert(route, r);
        let mut s = Map::from_iter([
            ("StageName".to_string(), json!("$default")),
            ("AutoDeploy".to_string(), json!(true)),
            ("ApiGatewayManaged".to_string(), json!(true)),
        ]);
        defaults(ctx, &KINDS[5], &mut s, &protocol);
        a.stages.insert("$default".into(), s);
    }
    let v = Value::Object(a.info.clone());
    region.apis.insert(id, a);
    Ok(v)
}

/// Replaces routes and integrations with the paths of an OpenAPI document.
fn import(ctx: &mut Ctx<'_>, a: &mut Api, body: &str) -> Result<(), AwsError> {
    let doc: Value = serde_json::from_str(body)
        .map_err(|e| bad_request(format!("Invalid OpenAPI document: {e}")))?;
    if let Some(t) = doc.get("info").and_then(|i| i.get("title")) {
        a.info.insert("Name".into(), t.clone());
    }
    a.routes.clear();
    a.integrations.clear();
    for (path, ops) in doc
        .get("paths")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        for (verb, op) in ops.as_object().into_iter().flatten() {
            let method = if verb == "x-amazon-apigateway-any-method" {
                "ANY".to_string()
            } else {
                verb.to_uppercase()
            };
            let mut route = Map::from_iter([
                ("RouteId".to_string(), json!(gen_id(ctx, 7))),
                ("RouteKey".to_string(), json!(format!("{method} {path}"))),
            ]);
            if let Some(x) = op.get("x-amazon-apigateway-integration") {
                let id = gen_id(ctx, 7);
                let mut i = Map::from_iter([
                    ("IntegrationId".to_string(), json!(id)),
                    (
                        "IntegrationType".to_string(),
                        json!(x.str_field("type").unwrap_or("AWS_PROXY").to_uppercase()),
                    ),
                ]);
                for (from, to) in [
                    ("uri", "IntegrationUri"),
                    ("httpMethod", "IntegrationMethod"),
                    ("payloadFormatVersion", "PayloadFormatVersion"),
                    ("connectionType", "ConnectionType"),
                ] {
                    if let Some(v) = x.get(from) {
                        i.insert(to.into(), v.clone());
                    }
                }
                defaults(ctx, &KINDS[2], &mut i, "HTTP");
                a.integrations.insert(id.clone(), i);
                route.insert("Target".into(), json!(format!("integrations/{id}")));
            }
            defaults(ctx, &KINDS[1], &mut route, "HTTP");
            a.routes.insert(
                route["RouteId"].as_str().unwrap_or_default().to_string(),
                route,
            );
        }
    }
    Ok(())
}

fn export(a: &Api) -> Value {
    let mut paths = Map::new();
    for r in a.routes.values() {
        let key = r
            .get("RouteKey")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some((method, path)) = key.split_once(' ') else {
            continue;
        };
        let mut op = json!({ "responses": { "default": { "description": "Default response" } } });
        if let Some(i) = r
            .get("Target")
            .and_then(Value::as_str)
            .and_then(|t| t.strip_prefix("integrations/"))
            .and_then(|t| a.integrations.get(t))
        {
            op["x-amazon-apigateway-integration"] = json!({
                "type": i.get("IntegrationType").and_then(Value::as_str).map(str::to_lowercase),
                "uri": i.get("IntegrationUri"),
                "httpMethod": i.get("IntegrationMethod"),
                "payloadFormatVersion": i.get("PayloadFormatVersion"),
            });
        }
        let verb = if method == "ANY" {
            "x-amazon-apigateway-any-method".to_string()
        } else {
            method.to_lowercase()
        };
        paths
            .entry(path.to_string())
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .expect("an object")
            .insert(verb, op);
    }
    json!({
        "openapi": "3.0.1",
        "info": { "title": a.info.get("Name"), "version": a.info.get("Version").cloned().unwrap_or(json!("1.0")) },
        "paths": paths,
    })
}

fn tag_target<'r>(region: &'r mut Region, arn: &str) -> Option<&'r mut Obj> {
    let path = arn.split_once("::")?.1;
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match parts.as_slice() {
        ["apis", id] => region.apis.get_mut(*id).map(|a| &mut a.info),
        ["apis", id, "stages", name] => region.apis.get_mut(*id)?.stages.get_mut(*name),
        ["domainnames", name] => region.domains.get_mut(*name),
        ["vpclinks", id] => region.vpc_links.get_mut(*id),
        ["portals", id] => region.portals.get_mut(*id),
        ["portalproducts", id] => region.products.get_mut(*id),
        _ => None,
    }
}

fn special(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    Ok(Some(match operation {
        "CreateApi" => create_api(ctx, region, input)?,
        "ImportApi" | "ReimportApi" => {
            let body = input.str_field("Body").unwrap_or_default().to_string();
            if operation == "ImportApi" {
                let mut info = json!({ "Name": "imported", "ProtocolType": "HTTP" });
                if let Some(n) = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|d| d.get("info")?.get("title").cloned())
                {
                    info["Name"] = n;
                }
                let created = create_api(ctx, region, &info)?;
                let a = region
                    .apis
                    .get_mut(created["ApiId"].as_str().unwrap_or_default())
                    .expect("just created");
                import(ctx, a, &body)?;
                Value::Object(a.info.clone())
            } else {
                let a = api(region, input)?;
                import(ctx, a, &body)?;
                Value::Object(a.info.clone())
            }
        }
        "ExportApi" => {
            let a = api(region, input)?;
            let doc = export(a);
            let text = if input.str_field("OutputType") == Some("YAML") {
                serde_json::to_string_pretty(&doc).unwrap_or_default()
            } else {
                doc.to_string()
            };
            json!({ "body": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, text) })
        }
        "GetApi" => Value::Object(api(region, input)?.info.clone()),
        "GetApis" => page(
            region
                .apis
                .values()
                .map(|a| Value::Object(a.info.clone()))
                .collect(),
            input,
        ),
        "UpdateApi" => {
            let a = api(region, input)?;
            let mut f = fields(input, "UpdateApi");
            for k in ["Target", "RouteKey", "CredentialsArn"] {
                f.remove(k);
            }
            a.info.extend(f);
            Value::Object(a.info.clone())
        }
        "DeleteApi" => {
            api(region, input)?;
            region
                .apis
                .shift_remove(input.str_field("ApiId").unwrap_or_default());
            json!({})
        }
        "CreateDeployment" => {
            let id = gen_id(ctx, 6);
            let created = now(ctx);
            let a = api(region, input)?;
            let mut info = Map::from_iter([
                ("DeploymentId".to_string(), json!(id)),
                ("DeploymentStatus".to_string(), json!("DEPLOYED")),
                ("CreatedDate".to_string(), created),
                ("AutoDeployed".to_string(), json!(false)),
            ]);
            if let Some(d) = input.get("Description") {
                info.insert("Description".into(), d.clone());
            }
            if let Some(stage) = input.str_field("StageName") {
                let s = a
                    .stages
                    .get_mut(stage)
                    .ok_or_else(|| missing("Stage", stage))?;
                s.insert("DeploymentId".into(), json!(id));
            }
            let d = Deployment {
                info: info.clone(),
                routes: a.routes.clone(),
                integrations: a.integrations.clone(),
            };
            a.deployments.insert(id, d);
            Value::Object(info)
        }
        "GetDeployments" => {
            let a = api(region, input)?;
            page(
                a.deployments
                    .values()
                    .map(|d| Value::Object(d.info.clone()))
                    .collect(),
                input,
            )
        }
        "GetDeployment" | "UpdateDeployment" | "DeleteDeployment" => {
            let a = api(region, input)?;
            let id = input
                .str_field("DeploymentId")
                .unwrap_or_default()
                .to_string();
            let d = a
                .deployments
                .get_mut(&id)
                .ok_or_else(|| missing("Deployment", &id))?;
            match operation {
                "UpdateDeployment" => {
                    if let Some(x) = input.get("Description") {
                        d.info.insert("Description".into(), x.clone());
                    }
                    Value::Object(d.info.clone())
                }
                "DeleteDeployment" => {
                    a.deployments.shift_remove(&id);
                    json!({})
                }
                _ => Value::Object(d.info.clone()),
            }
        }
        "DeleteCorsConfiguration" => {
            api(region, input)?.info.remove("CorsConfiguration");
            json!({})
        }
        "DeleteRouteRequestParameter" => {
            let a = api(region, input)?;
            let id = input.str_field("RouteId").unwrap_or_default();
            let r = a.routes.get_mut(id).ok_or_else(|| missing("Route", id))?;
            if let Some(p) = r
                .get_mut("RequestParameters")
                .and_then(Value::as_object_mut)
            {
                p.remove(input.str_field("RequestParameterKey").unwrap_or_default());
            }
            json!({})
        }
        "DeleteRouteSettings" | "DeleteAccessLogSettings" | "ResetAuthorizersCache" => {
            let a = api(region, input)?;
            let name = input.str_field("StageName").unwrap_or_default();
            let s = a
                .stages
                .get_mut(name)
                .ok_or_else(|| missing("Stage", name))?;
            match operation {
                "DeleteRouteSettings" => {
                    if let Some(rs) = s.get_mut("RouteSettings").and_then(Value::as_object_mut) {
                        rs.remove(input.str_field("RouteKey").unwrap_or_default());
                    }
                }
                "DeleteAccessLogSettings" => {
                    s.remove("AccessLogSettings");
                }
                _ => {}
            }
            json!({})
        }
        "GetModelTemplate" => {
            let a = api(region, input)?;
            let id = input.str_field("ModelId").unwrap_or_default();
            a.models.get(id).ok_or_else(|| missing("Model", id))?;
            json!({ "Value": "#set($inputRoot = $input.path('$'))\n{}" })
        }
        "GetTags" | "TagResource" | "UntagResource" => {
            let arn = input
                .str_field("ResourceArn")
                .unwrap_or_default()
                .to_string();
            let target = tag_target(region, &arn)
                .ok_or_else(|| not_found(format!("Invalid resource ARN specified: {arn}")))?;
            let tags = target.entry("Tags").or_insert_with(|| json!({}));
            match operation {
                "TagResource" => {
                    if let (Some(t), Some(new)) = (tags.as_object_mut(), input.obj_field("Tags")) {
                        t.extend(new.clone());
                    }
                    json!({})
                }
                "UntagResource" => {
                    if let Some(t) = tags.as_object_mut() {
                        for k in input
                            .array_field("TagKeys")
                            .iter()
                            .filter_map(Value::as_str)
                        {
                            t.remove(k);
                        }
                    }
                    json!({})
                }
                _ => json!({ "Tags": tags }),
            }
        }
        "GetPortalProductSharingPolicy"
        | "PutPortalProductSharingPolicy"
        | "DeletePortalProductSharingPolicy" => {
            let id = input
                .str_field("PortalProductId")
                .unwrap_or_default()
                .to_string();
            if !region.products.contains_key(&id) {
                return Err(missing("portal product", &id));
            }
            match operation {
                "PutPortalProductSharingPolicy" => {
                    let mut v = fields(input, operation);
                    v.insert("PortalProductId".into(), json!(id));
                    region.sharing.insert(id, v);
                    json!({})
                }
                "DeletePortalProductSharingPolicy" => {
                    region.sharing.remove(&id);
                    json!({})
                }
                _ => Value::Object(
                    region
                        .sharing
                        .get(&id)
                        .cloned()
                        .ok_or_else(|| missing("sharing policy", &id))?,
                ),
            }
        }
        "PublishPortal" | "PreviewPortal" | "DisablePortal" => {
            let id = input.str_field("PortalId").unwrap_or_default();
            let p = region
                .portals
                .get_mut(id)
                .ok_or_else(|| missing("portal", id))?;
            let status = match operation {
                "PublishPortal" => "PUBLISHED",
                "DisablePortal" => "DISABLED",
                _ => "PREVIEW",
            };
            p.insert("PublishStatus".into(), json!(status));
            json!({})
        }
        _ => return Ok(None),
    }))
}

impl Service for ApiGatewayV2 {
    fn signing_name(&self) -> &'static str {
        "apigateway"
    }

    fn model(&self) -> &'static ServiceModel {
        model()
    }

    fn accepts(&self, req: &Request) -> bool {
        req.path.starts_with("/v2/")
    }

    fn call(&mut self, ctx: &mut Ctx<'_>, operation: &str, input: Value) -> OpResult {
        let key = (ctx.account.clone(), ctx.region.clone());
        let mut region = std::mem::take(self.regions.entry(key.clone()).or_default());
        let result = (|| {
            if let Some(out) = special(ctx, &mut region, operation, &input)? {
                return Ok(out);
            }
            let (verb, kind) = parse_op(operation)
                .ok_or_else(|| bad_request(format!("wasmo: {operation} is not implemented")))?;
            generic(ctx, &mut region, verb, kind, &input, operation)
        })();
        self.regions.insert(key, region);
        result
    }

    fn serve(&mut self, ctx: &mut Ctx<'_>, api_id: &str, req: &Request) -> Option<Response> {
        invoke::serve(self, ctx, api_id, req)
    }

    fn answer(&mut self, ctx: &mut Ctx<'_>, id: &str, result: OpResult) {
        invoke::answer(self, ctx, id, result);
    }
}
