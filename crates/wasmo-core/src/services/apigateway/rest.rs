//! REST APIs, resources, methods, integrations, and their responses.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::store::{Obj, bad_request, conflict, gen_id, not_found, page, patch, pick, secs};
use super::{Integration, Method, Region, Resource, RestApi, api};
use crate::codec::AwsError;
use crate::service::{Ctx, InputExt};

const API_KEYS: &[&str] = &[
    "name",
    "description",
    "version",
    "binaryMediaTypes",
    "minimumCompressionSize",
    "apiKeySource",
    "endpointConfiguration",
    "policy",
    "tags",
    "disableExecuteApiEndpoint",
];
const METHOD_KEYS: &[&str] = &[
    "authorizationType",
    "authorizerId",
    "apiKeyRequired",
    "operationName",
    "requestParameters",
    "requestModels",
    "requestValidatorId",
    "authorizationScopes",
];
const INTEGRATION_KEYS: &[&str] = &[
    "type",
    "uri",
    "connectionType",
    "connectionId",
    "credentials",
    "requestParameters",
    "requestTemplates",
    "passthroughBehavior",
    "cacheNamespace",
    "cacheKeyParameters",
    "contentHandling",
    "timeoutInMillis",
    "tlsConfig",
];

fn api_view(a: &RestApi) -> Value {
    Value::Object(a.info.clone())
}

pub(super) fn method_view(m: &Method) -> Value {
    let mut v = m.info.clone();
    if !m.responses.is_empty() {
        v.insert("methodResponses".into(), json!(m.responses));
    }
    if let Some(i) = &m.integration {
        v.insert("methodIntegration".into(), integration_view(i));
    }
    Value::Object(v)
}

fn integration_view(i: &Integration) -> Value {
    let mut v = i.info.clone();
    if !i.responses.is_empty() {
        v.insert("integrationResponses".into(), json!(i.responses));
    }
    Value::Object(v)
}

pub(super) fn resource_view(r: &Resource, embed: bool) -> Value {
    let mut v = r.info.clone();
    if !r.methods.is_empty() {
        let methods: Map<String, Value> = r
            .methods
            .iter()
            .map(|(k, m)| (k.clone(), if embed { method_view(m) } else { json!({}) }))
            .collect();
        v.insert("resourceMethods".into(), Value::Object(methods));
    }
    Value::Object(v)
}

fn new_api(ctx: &mut Ctx<'_>, input: &Value) -> RestApi {
    let id = gen_id(ctx, 10);
    let root = gen_id(ctx, 10);
    let mut info = pick(input, API_KEYS);
    info.insert("id".into(), json!(id));
    info.insert("createdDate".into(), secs(ctx));
    info.entry("apiKeySource").or_insert(json!("HEADER"));
    info.entry("endpointConfiguration")
        .or_insert(json!({ "types": ["EDGE"], "ipAddressType": "ipv4" }));
    info.entry("disableExecuteApiEndpoint")
        .or_insert(json!(false));
    info.insert("rootResourceId".into(), json!(root));
    let mut resources = IndexMap::new();
    resources.insert(
        root.clone(),
        Resource {
            info: Map::from_iter([
                ("id".to_string(), json!(root)),
                ("path".to_string(), json!("/")),
            ]),
            ..Resource::default()
        },
    );
    RestApi {
        info,
        resources,
        ..RestApi::default()
    }
}

/// Recomputes `path` of a resource and its descendants after a move or rename.
fn repath(a: &mut RestApi, id: &str) {
    let Some(r) = a.resources.get(id) else {
        return;
    };
    let parent_path = r
        .info
        .get("parentId")
        .and_then(Value::as_str)
        .and_then(|p| a.resources.get(p))
        .map(|p| p.path().to_string());
    let part = r
        .info
        .get("pathPart")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let path = match parent_path {
        Some(pp) if pp == "/" => format!("/{part}"),
        Some(pp) => format!("{pp}/{part}"),
        None => "/".into(),
    };
    if let Some(r) = a.resources.get_mut(id) {
        r.info.insert("path".into(), json!(path));
    }
    let children: Vec<String> = a
        .resources
        .iter()
        .filter(|(_, c)| c.info.get("parentId").and_then(Value::as_str) == Some(id))
        .map(|(k, _)| k.clone())
        .collect();
    for c in children {
        repath(a, &c);
    }
}

fn resource<'a>(a: &'a mut RestApi, input: &Value) -> Result<&'a mut Resource, AwsError> {
    let id = input.str_field("resourceId").unwrap_or_default();
    a.resources
        .get_mut(id)
        .ok_or_else(|| not_found("Invalid Resource identifier specified"))
}

/// Adds a resource under `parent`, or returns the id of the existing one.
fn add_resource(ctx: &mut Ctx<'_>, a: &mut RestApi, parent: &str, part: &str) -> String {
    if let Some((id, _)) = a.resources.iter().find(|(_, r)| {
        r.info.get("parentId").and_then(Value::as_str) == Some(parent)
            && r.info.get("pathPart").and_then(Value::as_str) == Some(part)
    }) {
        return id.clone();
    }
    let id = gen_id(ctx, 6);
    a.resources.insert(
        id.clone(),
        Resource {
            info: Map::from_iter([
                ("id".to_string(), json!(id)),
                ("parentId".to_string(), json!(parent)),
                ("pathPart".to_string(), json!(part)),
            ]),
            ..Resource::default()
        },
    );
    repath(a, &id);
    id
}

/// Replaces the resources of `a` with the paths of an OpenAPI or Swagger
/// document, including `x-amazon-apigateway-integration`.
fn import(ctx: &mut Ctx<'_>, a: &mut RestApi, body: &str) -> Result<(), AwsError> {
    let doc: Value = serde_json::from_str(body)
        .map_err(|e| bad_request(format!("Invalid OpenAPI input: {e}")))?;
    if let Some(title) = doc.get("info").and_then(|i| i.get("title")) {
        a.info.entry("name").or_insert(title.clone());
    }
    let root = a
        .info
        .get("rootResourceId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    a.resources.retain(|k, _| *k == root);
    for (path, ops) in doc
        .get("paths")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let mut id = root.clone();
        for part in path.split('/').filter(|p| !p.is_empty()) {
            id = add_resource(ctx, a, &id, part);
        }
        for (verb, op) in ops.as_object().into_iter().flatten() {
            let method = if verb == "x-amazon-apigateway-any-method" {
                "ANY".to_string()
            } else {
                verb.to_uppercase()
            };
            let mut m = Method {
                info: Map::from_iter([
                    ("httpMethod".to_string(), json!(method)),
                    ("authorizationType".to_string(), json!("NONE")),
                    ("apiKeyRequired".to_string(), json!(false)),
                ]),
                ..Method::default()
            };
            if let Some(x) = op.get("x-amazon-apigateway-integration") {
                let mut info = pick(x, &INTEGRATION_KEYS[1..]);
                info.insert(
                    "type".into(),
                    json!(x.str_field("type").unwrap_or("MOCK").to_uppercase()),
                );
                if let Some(h) = x.get("httpMethod") {
                    info.insert("httpMethod".into(), h.clone());
                }
                let mut responses = std::collections::BTreeMap::new();
                for (pattern, r) in x
                    .get("responses")
                    .and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                {
                    let status = r.str_field("statusCode").unwrap_or("200").to_string();
                    let mut ir = pick(
                        r,
                        &["responseParameters", "responseTemplates", "contentHandling"],
                    );
                    ir.insert("statusCode".into(), json!(status));
                    if pattern != "default" {
                        ir.insert("selectionPattern".into(), json!(pattern));
                    }
                    responses.insert(status, ir);
                }
                m.integration = Some(Integration { info, responses });
            }
            if let Some(r) = a.resources.get_mut(&id) {
                r.methods.insert(method, m);
            }
        }
    }
    Ok(())
}

fn body_text(input: &Value) -> String {
    input
        .str_field("body")
        .and_then(|b| STANDARD.decode(b).ok())
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

pub(super) fn call(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    Ok(Some(match operation {
        "CreateRestApi" => {
            input
                .require_str("name")
                .map_err(|e| bad_request(e.message))?;
            let mut a = new_api(ctx, input);
            if let Some(src) = input.str_field("cloneFrom") {
                let from = region.apis.get(src).ok_or_else(|| {
                    not_found(format!(
                        "Invalid API identifier specified {}:{src}",
                        ctx.account
                    ))
                })?;
                a.resources = from.resources.clone();
                a.info
                    .insert("rootResourceId".into(), from.info["rootResourceId"].clone());
                a.models = from.models.clone();
            }
            let v = api_view(&a);
            region
                .apis
                .insert(a.info["id"].as_str().unwrap_or_default().to_string(), a);
            v
        }
        "ImportRestApi" => {
            let mut a = new_api(ctx, &json!({}));
            import(ctx, &mut a, &body_text(input))?;
            a.info.entry("name").or_insert(json!("imported"));
            let v = api_view(&a);
            region
                .apis
                .insert(a.info["id"].as_str().unwrap_or_default().to_string(), a);
            v
        }
        "PutRestApi" => {
            let body = body_text(input);
            let a = api(region, ctx, input)?;
            import(ctx, a, &body)?;
            api_view(a)
        }
        "GetRestApis" => page(
            region.apis.values().map(api_view).collect(),
            input,
            "items",
            "position",
        ),
        "GetRestApi" => api_view(api(region, ctx, input)?),
        "UpdateRestApi" => {
            let a = api(region, ctx, input)?;
            patch(&mut a.info, input)?;
            api_view(a)
        }
        "DeleteRestApi" => {
            api(region, ctx, input)?;
            region
                .apis
                .shift_remove(input.str_field("restApiId").unwrap_or_default());
            json!({})
        }
        "GetResources" => {
            let embed = input.array_field("embed").iter().any(|e| e == "methods");
            let a = api(region, ctx, input)?;
            page(
                a.resources
                    .values()
                    .map(|r| resource_view(r, embed))
                    .collect(),
                input,
                "items",
                "position",
            )
        }
        "GetResource" => {
            let embed = input.array_field("embed").iter().any(|e| e == "methods");
            resource_view(resource(api(region, ctx, input)?, input)?, embed)
        }
        "CreateResource" => {
            let a = api(region, ctx, input)?;
            let parent = input.str_field("parentId").unwrap_or_default().to_string();
            if !a.resources.contains_key(&parent) {
                return Err(not_found("Invalid Resource identifier specified"));
            }
            let part = input
                .require_str("pathPart")
                .map_err(|e| bad_request(e.message))?
                .to_string();
            let exists = a.resources.values().any(|r| {
                r.info.get("parentId").and_then(Value::as_str) == Some(&parent)
                    && r.info.get("pathPart").and_then(Value::as_str) == Some(&part)
            });
            if exists {
                return Err(conflict(format!(
                    "Another resource with the same parent already has this name: {part}"
                )));
            }
            let id = add_resource(ctx, a, &parent, &part);
            resource_view(&a.resources[&id], false)
        }
        "UpdateResource" => {
            let a = api(region, ctx, input)?;
            let r = resource(a, input)?;
            patch(&mut r.info, input)?;
            let id = input
                .str_field("resourceId")
                .unwrap_or_default()
                .to_string();
            repath(a, &id);
            resource_view(&a.resources[&id], false)
        }
        "DeleteResource" => {
            let a = api(region, ctx, input)?;
            let id = input
                .str_field("resourceId")
                .unwrap_or_default()
                .to_string();
            if !a.resources.contains_key(&id) {
                return Err(not_found("Invalid Resource identifier specified"));
            }
            if a.info.get("rootResourceId").and_then(Value::as_str) == Some(&id) {
                return Err(bad_request("Cannot delete the root resource"));
            }
            let mut gone = vec![id];
            while let Some(x) = gone.pop() {
                a.resources.shift_remove(&x);
                gone.extend(
                    a.resources
                        .iter()
                        .filter(|(_, r)| r.info.get("parentId").and_then(Value::as_str) == Some(&x))
                        .map(|(k, _)| k.clone()),
                );
            }
            json!({})
        }
        _ => return Ok(None),
    }))
}

fn method<'a>(r: &'a mut Resource, input: &Value) -> Result<&'a mut Method, AwsError> {
    let m = input.str_field("httpMethod").unwrap_or_default();
    r.methods
        .get_mut(m)
        .ok_or_else(|| not_found("Invalid Method identifier specified"))
}

fn integration(m: &mut Method) -> Result<&mut Integration, AwsError> {
    m.integration
        .as_mut()
        .ok_or_else(|| not_found("No integration defined for method"))
}

fn status_of(input: &Value) -> String {
    input
        .str_field("statusCode")
        .unwrap_or_default()
        .to_string()
}

fn put_integration(input: &Value) -> Result<Obj, AwsError> {
    let kind = input
        .require_str("type")
        .map_err(|e| bad_request(e.message))?
        .to_string();
    let mut info = pick(input, INTEGRATION_KEYS);
    if let Some(h) = input.get("integrationHttpMethod") {
        info.insert("httpMethod".into(), h.clone());
    }
    if matches!(kind.as_str(), "AWS" | "AWS_PROXY" | "HTTP" | "HTTP_PROXY")
        && !info.contains_key("uri")
    {
        return Err(bad_request(
            "Enumeration value for Integration Type must be accompanied by a URI",
        ));
    }
    if kind != "MOCK" && kind != "HTTP_PROXY" && kind != "HTTP" && !info.contains_key("httpMethod")
    {
        return Err(bad_request(
            "Enumeration value for HttpMethod must be non-empty",
        ));
    }
    info.entry("passthroughBehavior")
        .or_insert(json!("WHEN_NO_MATCH"));
    info.entry("timeoutInMillis").or_insert(json!(29000));
    info.entry("cacheNamespace")
        .or_insert(json!(input.str_field("resourceId").unwrap_or_default()));
    info.entry("cacheKeyParameters").or_insert(json!([]));
    Ok(info)
}

pub(super) fn methods(
    ctx: &mut Ctx<'_>,
    region: &mut Region,
    operation: &str,
    input: &Value,
) -> Result<Option<Value>, AwsError> {
    if !matches!(
        operation,
        "PutMethod"
            | "GetMethod"
            | "UpdateMethod"
            | "DeleteMethod"
            | "PutIntegration"
            | "GetIntegration"
            | "UpdateIntegration"
            | "DeleteIntegration"
            | "PutMethodResponse"
            | "GetMethodResponse"
            | "UpdateMethodResponse"
            | "DeleteMethodResponse"
            | "PutIntegrationResponse"
            | "GetIntegrationResponse"
            | "UpdateIntegrationResponse"
            | "DeleteIntegrationResponse"
    ) {
        return Ok(None);
    }
    let a = api(region, ctx, input)?;
    let r = resource(a, input)?;
    if operation == "PutMethod" {
        let verb = input
            .str_field("httpMethod")
            .unwrap_or_default()
            .to_string();
        if r.methods.contains_key(&verb) {
            return Err(conflict("Method already exists for this resource"));
        }
        input
            .require_str("authorizationType")
            .map_err(|e| bad_request(e.message))?;
        let mut info = pick(input, METHOD_KEYS);
        info.insert("httpMethod".into(), json!(verb));
        info.entry("apiKeyRequired").or_insert(json!(false));
        r.methods.insert(
            verb.clone(),
            Method {
                info,
                ..Method::default()
            },
        );
        return Ok(Some(method_view(&r.methods[&verb])));
    }
    if operation == "DeleteMethod" {
        method(r, input)?;
        r.methods
            .remove(input.str_field("httpMethod").unwrap_or_default());
        return Ok(Some(json!({})));
    }
    let m = method(r, input)?;
    let status = status_of(input);
    Ok(Some(match operation {
        "GetMethod" => method_view(m),
        "UpdateMethod" => {
            patch(&mut m.info, input)?;
            method_view(m)
        }
        "PutIntegration" => {
            m.integration = Some(Integration {
                info: put_integration(input)?,
                responses: Default::default(),
            });
            integration_view(m.integration.as_ref().expect("set above"))
        }
        "GetIntegration" => integration_view(integration(m)?),
        "UpdateIntegration" => {
            let i = integration(m)?;
            patch(&mut i.info, input)?;
            integration_view(i)
        }
        "DeleteIntegration" => {
            integration(m)?;
            m.integration = None;
            json!({})
        }
        "PutMethodResponse" => {
            let mut v = pick(input, &["responseParameters", "responseModels"]);
            v.insert("statusCode".into(), json!(status));
            m.responses.insert(status.clone(), v.clone());
            Value::Object(v)
        }
        "GetMethodResponse" | "UpdateMethodResponse" | "DeleteMethodResponse" => {
            let v = m
                .responses
                .get_mut(&status)
                .ok_or_else(|| not_found("Invalid Response status code specified"))?;
            match operation {
                "UpdateMethodResponse" => {
                    patch(v, input)?;
                    Value::Object(v.clone())
                }
                "DeleteMethodResponse" => {
                    m.responses.remove(&status);
                    json!({})
                }
                _ => Value::Object(v.clone()),
            }
        }
        "PutIntegrationResponse" => {
            let i = integration(m)?;
            let mut v = pick(
                input,
                &[
                    "selectionPattern",
                    "responseParameters",
                    "responseTemplates",
                    "contentHandling",
                ],
            );
            v.insert("statusCode".into(), json!(status));
            i.responses.insert(status, v.clone());
            Value::Object(v)
        }
        _ => {
            let i = integration(m)?;
            let v = i
                .responses
                .get_mut(&status)
                .ok_or_else(|| not_found("Invalid Response status code specified"))?;
            match operation {
                "UpdateIntegrationResponse" => {
                    patch(v, input)?;
                    Value::Object(v.clone())
                }
                "DeleteIntegrationResponse" => {
                    i.responses.remove(&status);
                    json!({})
                }
                _ => Value::Object(v.clone()),
            }
        }
    }))
}
