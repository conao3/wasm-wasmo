//! REST API invocation: resource and method matching, API keys, request
//! validation, Lambda and Cognito authorizers, and MOCK, Lambda proxy,
//! Lambda, and AWS service integrations with mapping templates.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::deploy::gateway_response;
use super::proxy::{self, Context, In, invoke_input, invoke_output, json_response, message};
use super::store::{bad_request, not_found};
use super::vtl::{self, url_decode};
use super::{ApiGateway, Method, Resource, RestApi};
use crate::codec::AwsError;
use crate::jsonpath;
use crate::service::{Ctx, InputExt, OpResult};
use crate::wire::{Request, Response};

#[derive(Debug, Clone)]
struct Flow {
    api_id: String,
    stage: String,
    stage_vars: Value,
    resource_id: String,
    resource_path: String,
    method: Method,
    params: Map<String, Value>,
    req: In,
    request_id: String,
    authorizer: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Authorizer,
    Proxy,
    Lambda,
    Service,
}

pub(crate) struct Pending {
    token: String,
    test: bool,
    flow: Box<Flow>,
    phase: Phase,
}

enum Outcome {
    Done(Response),
    Wait(String, Phase),
}

fn find<'r>(
    resources: &'r IndexMap<String, Resource>,
    method: &str,
    path: &str,
) -> Option<(&'r Resource, &'r Method, Map<String, Value>)> {
    let mut best: Option<(proxy::Score, &Resource, Map<String, Value>)> = None;
    for r in resources.values() {
        let Some((score, params)) = proxy::match_path(r.path(), path) else {
            continue;
        };
        let better = best
            .as_ref()
            .is_none_or(|(s, _, _)| (score.2, score.0, score.1) > (s.2, s.0, s.1));
        if better {
            best = Some((score, r, params));
        }
    }
    let (_, r, params) = best?;
    let m = r.methods.get(method).or_else(|| r.methods.get("ANY"))?;
    Some((r, m, params))
}

fn region_api<'g>(gw: &'g ApiGateway, ctx: &Ctx<'_>, id: &str) -> Option<&'g RestApi> {
    gw.regions
        .get(&(ctx.account.clone(), ctx.region.clone()))?
        .apis
        .get(id)
}

/// A gateway response (`MISSING_AUTHENTICATION_TOKEN`, `UNAUTHORIZED`, ...)
/// rendered from the API's configuration.
fn gateway(a: Option<&RestApi>, kind: &str, status: u16, text: &str) -> Response {
    let fallback = if status < 500 {
        "DEFAULT_4XX"
    } else {
        "DEFAULT_5XX"
    };
    let r = a.map(|a| {
        if a.gateway_responses.contains_key(kind) || !a.gateway_responses.contains_key(fallback) {
            gateway_response(a, kind)
        } else {
            gateway_response(a, fallback)
        }
    });
    let status = r
        .as_ref()
        .and_then(|r| r.get("statusCode"))
        .and_then(|s| s.as_str()?.parse().ok())
        .unwrap_or(status);
    let template = r
        .as_ref()
        .and_then(|r| r.get("responseTemplates"))
        .and_then(|t| t.get("application/json"))
        .and_then(Value::as_str)
        .unwrap_or("{\"message\":$context.error.messageString}");
    let body = template
        .replace("$context.error.messageString", &json!(text).to_string())
        .replace("$context.error.message", text);
    let mut res = Response::new(status, "application/json", body.into_bytes());
    res.headers
        .push(("x-amzn-errortype".into(), error_type(kind).into()));
    for (k, v) in r
        .as_ref()
        .and_then(|r| r.get("responseParameters"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if let Some(h) = k.strip_prefix("gatewayresponse.header.") {
            res.headers.push((
                h.to_lowercase(),
                v.as_str()
                    .unwrap_or_default()
                    .trim_matches('\'')
                    .to_string(),
            ));
        }
    }
    res
}

fn error_type(kind: &str) -> &'static str {
    match kind {
        "UNAUTHORIZED" => "UnauthorizedException",
        "ACCESS_DENIED" | "MISSING_AUTHENTICATION_TOKEN" | "INVALID_API_KEY" => {
            "ForbiddenException"
        }
        "BAD_REQUEST_BODY" | "BAD_REQUEST_PARAMETERS" => "BadRequestException",
        _ => "InternalServerErrorException",
    }
}

pub(crate) fn serve(
    gw: &mut ApiGateway,
    ctx: &mut Ctx<'_>,
    api_id: &str,
    req: &Request,
) -> Option<Response> {
    let a = region_api(gw, ctx, api_id)?;
    let trimmed = req.path.trim_start_matches('/');
    let (stage_name, rest) = trimmed
        .split_once('/')
        .map_or((trimmed, ""), |(s, r)| (s, r));
    let Some(stage) = a.stages.get(stage_name) else {
        return Some(message(403, "Forbidden"));
    };
    let resources = stage
        .get("deploymentId")
        .and_then(Value::as_str)
        .and_then(|d| a.deployments.get(d))
        .map_or(&a.resources, |d| &d.resources);
    let input = In::new(req, &format!("/{rest}"));
    let Some((r, m, params)) = find(resources, &input.method, &input.path) else {
        return Some(gateway(
            Some(a),
            "MISSING_AUTHENTICATION_TOKEN",
            403,
            "Missing Authentication Token",
        ));
    };
    let flow = Flow {
        api_id: api_id.to_string(),
        stage: stage_name.to_string(),
        stage_vars: stage.get("variables").cloned().unwrap_or(json!({})),
        resource_id: r
            .info
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        resource_path: r.path().to_string(),
        method: m.clone(),
        params,
        req: input,
        request_id: ctx.uuid(),
        authorizer: None,
    };
    let outcome = begin(gw, ctx, flow.clone());
    Some(match outcome {
        Outcome::Done(res) => finish(res, &flow),
        Outcome::Wait(call, phase) => {
            let token = ctx.uuid();
            ctx.awaiting = Some(token.clone());
            gw.pending.insert(
                call,
                Pending {
                    token,
                    test: false,
                    flow: Box::new(flow),
                    phase,
                },
            );
            Response::default()
        }
    })
}

fn finish(mut res: Response, flow: &Flow) -> Response {
    res.headers
        .push(("x-amzn-requestid".into(), flow.request_id.clone()));
    res.headers
        .push(("x-amz-apigw-id".into(), flow.request_id[..8].to_string()));
    res
}

/// `TestInvokeMethod`: runs the method of the live configuration and
/// reports the response.
pub(crate) fn test_invoke(gw: &mut ApiGateway, ctx: &mut Ctx<'_>, input: &Value) -> OpResult {
    let api_id = input.str_field("restApiId").unwrap_or_default().to_string();
    let a = region_api(gw, ctx, &api_id).ok_or_else(|| {
        not_found(format!(
            "Invalid API identifier specified {}:{api_id}",
            ctx.account
        ))
    })?;
    let r = a
        .resources
        .get(input.str_field("resourceId").unwrap_or_default())
        .ok_or_else(|| not_found("Invalid Resource identifier specified"))?;
    let verb = input
        .str_field("httpMethod")
        .unwrap_or_default()
        .to_string();
    let m = r
        .methods
        .get(&verb)
        .ok_or_else(|| not_found("Invalid Method identifier specified"))?;
    let path_query = input
        .str_field("pathWithQueryString")
        .unwrap_or(r.path())
        .to_string();
    let (path, query) = path_query.split_once('?').unwrap_or((&path_query, ""));
    let mut headers: Vec<(String, String)> = input
        .obj_field("headers")
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.to_lowercase(), vtl::text(v)))
        .collect();
    headers.push((
        "host".into(),
        format!("{api_id}.execute-api.{}.amazonaws.com", ctx.region),
    ));
    let req = Request {
        method: verb.clone(),
        path: format!("/test-invoke-stage{path}"),
        query: query.to_string(),
        headers,
        body: input
            .str_field("body")
            .unwrap_or_default()
            .as_bytes()
            .to_vec(),
    };
    let params = proxy::match_path(r.path(), path)
        .map(|(_, p)| p)
        .unwrap_or_default();
    let flow = Flow {
        api_id: api_id.clone(),
        stage: "test-invoke-stage".into(),
        stage_vars: input.get("stageVariables").cloned().unwrap_or(json!({})),
        resource_id: r
            .info
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        resource_path: r.path().to_string(),
        method: m.clone(),
        params,
        req: In::new(&req, path),
        request_id: ctx.uuid(),
        authorizer: None,
    };
    match integrate(ctx, &flow) {
        Outcome::Done(res) => Ok(test_output(&res)),
        Outcome::Wait(call, phase) => {
            let token = ctx.uuid();
            ctx.awaiting = Some(token.clone());
            gw.pending.insert(
                call,
                Pending {
                    token,
                    test: true,
                    flow: Box::new(flow),
                    phase,
                },
            );
            Ok(Value::Null)
        }
    }
}

fn test_output(res: &Response) -> Value {
    let headers: Map<String, Value> = res
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    let multi: Map<String, Value> = res
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), json!([v])))
        .collect();
    json!({
        "status": res.status,
        "body": String::from_utf8_lossy(&res.body),
        "headers": headers,
        "multiValueHeaders": multi,
        "log": "",
        "latency": 1,
    })
}

pub(crate) fn answer(gw: &mut ApiGateway, ctx: &mut Ctx<'_>, id: &str, result: OpResult) {
    let Some(mut p) = gw.pending.remove(id) else {
        return;
    };
    match resume(gw, ctx, &mut p, result) {
        Outcome::Done(res) => {
            let res = finish(res, &p.flow);
            if p.test {
                ctx.settle(p.token, Ok(test_output(&res)));
            } else {
                ctx.respond(p.token, res);
            }
        }
        Outcome::Wait(call, phase) => {
            p.phase = phase;
            gw.pending.insert(call, p);
        }
    }
}

fn resume(gw: &mut ApiGateway, ctx: &mut Ctx<'_>, p: &mut Pending, result: OpResult) -> Outcome {
    let a = region_api(gw, ctx, &p.flow.api_id);
    let failed = || gateway(a, "DEFAULT_5XX", 502, "Internal server error");
    match p.phase {
        Phase::Authorizer => {
            let out = match result {
                Ok(out) => out,
                Err(_) => {
                    return Outcome::Done(gateway(
                        a,
                        "AUTHORIZER_CONFIGURATION_ERROR",
                        500,
                        "null",
                    ));
                }
            };
            let (payload, error) = invoke_output(&out);
            if error {
                if payload.get("errorMessage").and_then(Value::as_str) == Some("Unauthorized") {
                    return Outcome::Done(gateway(a, "UNAUTHORIZED", 401, "Unauthorized"));
                }
                return Outcome::Done(gateway(a, "AUTHORIZER_FAILURE", 500, "null"));
            }
            match policy(&payload) {
                Err(deny) => Outcome::Done(gateway(a, "ACCESS_DENIED", 403, deny)),
                Ok(context) => {
                    p.flow.authorizer = Some(context);
                    integrate(ctx, &p.flow)
                }
            }
        }
        Phase::Proxy => match result.map(|o| invoke_output(&o)) {
            Ok((payload, false)) => {
                Outcome::Done(proxy::proxy_response(&payload, false).unwrap_or_else(failed))
            }
            _ => Outcome::Done(failed()),
        },
        Phase::Lambda => {
            let (text, error, status) = match result.map(|o| invoke_output(&o)) {
                Ok((payload, false)) => (payload.to_string(), None, 200),
                Ok((payload, true)) => {
                    let msg = payload
                        .get("errorMessage")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    (payload.to_string(), Some(msg), 200)
                }
                Err(e) => (
                    json!({ "message": e.message }).to_string(),
                    Some(e.message),
                    e.status,
                ),
            };
            Outcome::Done(respond(&p.flow, ctx, &text, error.as_deref(), status, false).0)
        }
        Phase::Service => {
            let (text, status, ct) = match result {
                Ok(out) => service_body(&out),
                Err(e) => (
                    json!({ "__type": e.code, "message": e.message }).to_string(),
                    e.status,
                    "application/json".to_string(),
                ),
            };
            let (mut res, mapped) = respond(&p.flow, ctx, &text, None, status, false);
            if !mapped {
                res.headers.retain(|(k, _)| k != "content-type");
                res.headers.push(("content-type".into(), ct));
            }
            Outcome::Done(res)
        }
    }
}

fn service_body(out: &Value) -> (String, u16, String) {
    if let Some(body) = out.get("Body").and_then(Value::as_str) {
        let bytes = STANDARD.decode(body).unwrap_or_default();
        let ct = out
            .get("ContentType")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_string();
        return (String::from_utf8_lossy(&bytes).into_owned(), 200, ct);
    }
    (out.to_string(), 200, "application/json".to_string())
}

/// The authorizer context of an Allow policy, or the message of a deny.
fn policy(out: &Value) -> Result<Value, &'static str> {
    let statements: Vec<&Value> = match out.get("policyDocument").and_then(|d| d.get("Statement")) {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(s @ Value::Object(_)) => vec![s],
        _ => Vec::new(),
    };
    let effect = |e: &str| {
        statements.iter().any(|s| {
            s.get("Effect")
                .and_then(Value::as_str)
                .is_some_and(|x| x.eq_ignore_ascii_case(e))
        })
    };
    if effect("Deny") {
        return Err("User is not authorized to access this resource with an explicit deny");
    }
    if !effect("Allow") {
        return Err("User is not authorized to access this resource");
    }
    let mut context = out
        .get("context")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(p) = out.get("principalId") {
        context.insert("principalId".into(), p.clone());
    }
    context.insert("integrationLatency".into(), json!(0));
    Ok(Value::Object(context))
}

fn method_arn(ctx: &Ctx<'_>, flow: &Flow) -> String {
    format!(
        "arn:aws:execute-api:{}:{}:{}/{}/{}{}",
        ctx.region, ctx.account, flow.api_id, flow.stage, flow.req.method, flow.req.path
    )
}

/// Source values like `method.request.header.Authorization`.
fn source(flow: &Flow, expr: &str, ctx: &Ctx<'_>) -> Option<String> {
    let expr = expr.trim();
    if let Some(lit) = expr.strip_prefix('\'').and_then(|e| e.strip_suffix('\'')) {
        return Some(lit.to_string());
    }
    let r = &flow.req;
    if let Some(h) = expr.strip_prefix("method.request.header.") {
        return r.header(h).map(str::to_string);
    }
    if let Some(q) = expr.strip_prefix("method.request.querystring.") {
        return r.query_param(q).map(str::to_string);
    }
    if let Some(p) = expr.strip_prefix("method.request.path.") {
        return flow.params.get(p).map(vtl::text);
    }
    if let Some(p) = expr.strip_prefix("method.request.body") {
        let data: Value = serde_json::from_slice(&r.body).ok()?;
        return jsonpath::get(&data, &format!("${p}"))
            .ok()
            .flatten()
            .map(|v| vtl::text(&v));
    }
    if let Some(s) = expr.strip_prefix("stageVariables.") {
        return flow.stage_vars.get(s).map(vtl::text);
    }
    if let Some(c) = expr.strip_prefix("context.") {
        let v = context(flow, ctx);
        return jsonpath::get(&v, &format!("$.{c}"))
            .ok()
            .flatten()
            .map(|v| vtl::text(&v));
    }
    None
}

fn begin(gw: &mut ApiGateway, ctx: &mut Ctx<'_>, mut flow: Flow) -> Outcome {
    let region = (ctx.account.clone(), ctx.region.clone());
    let Some(a) = gw
        .regions
        .get(&region)
        .and_then(|r| r.apis.get(&flow.api_id))
    else {
        return Outcome::Done(message(403, "Forbidden"));
    };
    let m = &flow.method.info;
    if m.get("apiKeyRequired").and_then(Value::as_bool) == Some(true) {
        let key = flow.req.header("x-api-key");
        let known = key.is_some_and(|k| {
            gw.regions[&region].api_keys.values().any(|x| {
                x.get("value").and_then(Value::as_str) == Some(k)
                    && x.get("enabled").and_then(Value::as_bool) == Some(true)
            })
        });
        if !known {
            return Outcome::Done(gateway(Some(a), "INVALID_API_KEY", 403, "Forbidden"));
        }
    }
    if let Some(v) = m
        .get("requestValidatorId")
        .and_then(Value::as_str)
        .and_then(|v| a.validators.get(v))
        && let Err((kind, text)) = validate(a, &flow, v)
    {
        return Outcome::Done(gateway(Some(a), kind, 400, &text));
    }
    let kind = m
        .get("authorizationType")
        .and_then(Value::as_str)
        .unwrap_or("NONE");
    let authorizer = m
        .get("authorizerId")
        .and_then(Value::as_str)
        .and_then(|id| a.authorizers.get(id))
        .cloned();
    match (kind, authorizer) {
        ("COGNITO_USER_POOLS", Some(_)) => {
            match flow.req.header("authorization").and_then(proxy::jwt_claims) {
                Some(claims) => flow.authorizer = Some(json!({ "claims": claims })),
                None => {
                    return Outcome::Done(gateway(Some(a), "UNAUTHORIZED", 401, "Unauthorized"));
                }
            }
        }
        ("CUSTOM", Some(auth)) => {
            let function = auth
                .get("authorizerUri")
                .and_then(Value::as_str)
                .and_then(proxy::lambda_target);
            let Some(function) = function.map(|f| proxy::stage_vars(&f, &flow.stage_vars)) else {
                return Outcome::Done(gateway(
                    Some(a),
                    "AUTHORIZER_CONFIGURATION_ERROR",
                    500,
                    "null",
                ));
            };
            let sources: Vec<String> = auth
                .get("identitySource")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_string())
                .collect();
            let values: Vec<Option<String>> =
                sources.iter().map(|s| source(&flow, s, ctx)).collect();
            if values.iter().any(Option::is_none) {
                return Outcome::Done(gateway(Some(a), "UNAUTHORIZED", 401, "Unauthorized"));
            }
            let event = if auth.get("type").and_then(Value::as_str) == Some("TOKEN") {
                let token = values.first().cloned().flatten().unwrap_or_default();
                if let Some(re) = auth
                    .get("identityValidationExpression")
                    .and_then(Value::as_str)
                    && regex_lite::Regex::new(&format!("^(?:{re})$"))
                        .is_ok_and(|r| !r.is_match(&token))
                {
                    return Outcome::Done(gateway(Some(a), "UNAUTHORIZED", 401, "Unauthorized"));
                }
                json!({ "type": "TOKEN", "authorizationToken": token, "methodArn": method_arn(ctx, &flow) })
            } else {
                let mut e = proxy::event_v1(ctx, &flow.req, &proxy_context(ctx, &flow));
                e["type"] = json!("REQUEST");
                e["methodArn"] = json!(method_arn(ctx, &flow));
                e
            };
            let id = ctx.uuid();
            ctx.call(
                id.clone(),
                "lambda",
                "Invoke",
                invoke_input(&function, &event),
            );
            return Outcome::Wait(id, Phase::Authorizer);
        }
        _ => {}
    }
    integrate(ctx, &flow)
}

fn validate(
    a: &RestApi,
    flow: &Flow,
    v: &Map<String, Value>,
) -> Result<(), (&'static str, String)> {
    let m = &flow.method.info;
    if v.get("validateRequestParameters").and_then(Value::as_bool) == Some(true) {
        let mut missing = Vec::new();
        for (k, required) in m
            .get("requestParameters")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if required != &Value::Bool(true) {
                continue;
            }
            let name = k.rsplit('.').next().unwrap_or_default();
            let present = if k.contains(".header.") {
                flow.req.header(name).is_some()
            } else if k.contains(".querystring.") {
                flow.req.query_param(name).is_some()
            } else {
                flow.params.contains_key(name)
            };
            if !present {
                missing.push(name.to_string());
            }
        }
        if !missing.is_empty() {
            return Err((
                "BAD_REQUEST_PARAMETERS",
                format!(
                    "Missing required request parameters: [{}]",
                    missing.join(", ")
                ),
            ));
        }
    }
    if v.get("validateRequestBody").and_then(Value::as_bool) == Some(true) {
        let ct = content_type(&flow.req);
        let model = m
            .get("requestModels")
            .and_then(|r| r.get(&ct).or_else(|| r.get("application/json")))
            .and_then(Value::as_str);
        if let Some(schema) = model
            .and_then(|name| a.models.get(name))
            .and_then(|m| m.get("schema"))
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
        {
            let body: Value = match serde_json::from_slice(&flow.req.body) {
                Ok(b) => b,
                Err(_) => return Err(("BAD_REQUEST_BODY", "Invalid request body".into())),
            };
            if !schema_ok(&body, &schema, &schema) {
                return Err(("BAD_REQUEST_BODY", "Invalid request body".into()));
            }
        }
    }
    Ok(())
}

/// The JSON Schema draft 4 keywords request models commonly use.
fn schema_ok(v: &Value, s: &Value, root: &Value) -> bool {
    if let Some(r) = s.get("$ref").and_then(Value::as_str) {
        let target = r
            .strip_prefix("#/definitions/")
            .and_then(|n| root.get("definitions")?.get(n))
            .cloned()
            .unwrap_or(json!({}));
        return schema_ok(v, &target, root);
    }
    if let Some(t) = s.get("type") {
        let types: Vec<&str> = match t {
            Value::String(x) => vec![x.as_str()],
            Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        let ok = types.iter().any(|t| match *t {
            "string" => v.is_string(),
            "number" => v.is_number(),
            "integer" => v.as_f64().is_some_and(|f| f.fract() == 0.0),
            "boolean" => v.is_boolean(),
            "null" => v.is_null(),
            "array" => v.is_array(),
            "object" => v.is_object(),
            _ => true,
        });
        if !ok {
            return false;
        }
    }
    if let Some(e) = s.get("enum").and_then(Value::as_array)
        && !e.contains(v)
    {
        return false;
    }
    if let Some(x) = v.as_str() {
        let n = x.chars().count() as u64;
        if s.get("minLength")
            .and_then(Value::as_u64)
            .is_some_and(|m| n < m)
            || s.get("maxLength")
                .and_then(Value::as_u64)
                .is_some_and(|m| n > m)
        {
            return false;
        }
        if let Some(p) = s.get("pattern").and_then(Value::as_str)
            && regex_lite::Regex::new(p).is_ok_and(|r| !r.is_match(x))
        {
            return false;
        }
    }
    if let Some(x) = v.as_f64()
        && (s
            .get("minimum")
            .and_then(Value::as_f64)
            .is_some_and(|m| x < m)
            || s.get("maximum")
                .and_then(Value::as_f64)
                .is_some_and(|m| x > m))
    {
        return false;
    }
    if let Some(o) = v.as_object() {
        for r in s
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if r.as_str().is_some_and(|k| !o.contains_key(k)) {
                return false;
            }
        }
        for (k, ps) in s
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if o.get(k).is_some_and(|x| !schema_ok(x, ps, root)) {
                return false;
            }
        }
    }
    if let (Some(a), Some(items)) = (v.as_array(), s.get("items"))
        && a.iter().any(|x| !schema_ok(x, items, root))
    {
        return false;
    }
    for all in s
        .get("allOf")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if !schema_ok(v, all, root) {
            return false;
        }
    }
    if let Some(any) = s.get("anyOf").and_then(Value::as_array)
        && !any.iter().any(|x| schema_ok(v, x, root))
    {
        return false;
    }
    true
}

fn content_type(r: &In) -> String {
    r.header("content-type")
        .and_then(|c| c.split(';').next())
        .map(|c| c.trim().to_lowercase())
        .unwrap_or_else(|| "application/json".into())
}

fn proxy_context<'f>(ctx: &'f Ctx<'_>, flow: &'f Flow) -> Context<'f> {
    Context {
        api_id: &flow.api_id,
        stage: &flow.stage,
        request_id: &flow.request_id,
        account: &ctx.account,
        resource_id: &flow.resource_id,
        resource_path: &flow.resource_path,
        route_key: "",
        stage_variables: &flow.stage_vars,
        params: &flow.params,
        authorizer: flow.authorizer.as_ref(),
    }
}

fn context(flow: &Flow, ctx: &Ctx<'_>) -> Value {
    let (time, epoch) = proxy::request_time(ctx);
    json!({
        "accountId": ctx.account,
        "apiId": flow.api_id,
        "httpMethod": flow.req.method,
        "path": flow.req.raw_path,
        "requestId": flow.request_id,
        "extendedRequestId": flow.request_id,
        "resourceId": flow.resource_id,
        "resourcePath": flow.resource_path,
        "stage": flow.stage,
        "requestTime": time,
        "requestTimeEpoch": epoch,
        "domainName": flow.req.host,
        "protocol": "HTTP/1.1",
        "identity": { "sourceIp": flow.req.header("x-forwarded-for").unwrap_or("127.0.0.1"), "userAgent": flow.req.header("user-agent").unwrap_or_default() },
        "authorizer": flow.authorizer.clone().unwrap_or(Value::Null),
    })
}

fn scope<'b>(flow: &Flow, ctx: &Ctx<'_>, body: &'b str) -> vtl::Scope<'b> {
    vtl::Scope {
        input: vtl::Input {
            body,
            params: vtl::params(&flow.params, &flow.req.query, &flow.req.headers),
        },
        context: context(flow, ctx),
        stage_variables: flow.stage_vars.clone(),
    }
}

/// The integration request body: the mapping template for the request's
/// content type, or the body itself as `passthroughBehavior` allows.
fn request_body(flow: &Flow, ctx: &Ctx<'_>) -> Result<String, Response> {
    let integration = flow.method.integration.as_ref().map(|i| &i.info);
    let templates = integration
        .and_then(|i| i.get("requestTemplates"))
        .and_then(Value::as_object);
    let ct = content_type(&flow.req);
    let body = flow.req.body_text();
    match templates.and_then(|t| t.get(&ct)).and_then(Value::as_str) {
        Some(t) => Ok(vtl::render(t, &scope(flow, ctx, &body))),
        None => {
            let behavior = integration
                .and_then(|i| i.get("passthroughBehavior"))
                .and_then(Value::as_str)
                .unwrap_or("WHEN_NO_MATCH");
            let any = templates.is_some_and(|t| !t.is_empty());
            if behavior == "NEVER" || (behavior == "WHEN_NO_TEMPLATES" && any) {
                return Err(message(415, "Unsupported Media Type"));
            }
            Ok(body)
        }
    }
}

fn integrate(ctx: &mut Ctx<'_>, flow: &Flow) -> Outcome {
    let Some(i) = flow.method.integration.as_ref() else {
        return Outcome::Done(message(500, "Internal server error"));
    };
    let kind = i.info.get("type").and_then(Value::as_str).unwrap_or("MOCK");
    let uri = proxy::stage_vars(
        i.info
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        &flow.stage_vars,
    );
    match kind {
        "MOCK" => {
            let body = match request_body(flow, ctx) {
                Ok(b) => b,
                Err(res) => return Outcome::Done(res),
            };
            let status = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v.get("statusCode")?.as_u64())
                .unwrap_or(200) as u16;
            Outcome::Done(respond(flow, ctx, "", None, status, true).0)
        }
        "AWS_PROXY" => {
            let Some(function) = proxy::lambda_target(&uri) else {
                return Outcome::Done(message(500, "Internal server error"));
            };
            let event = proxy::event_v1(ctx, &flow.req, &proxy_context(ctx, flow));
            let id = ctx.uuid();
            ctx.call(
                id.clone(),
                "lambda",
                "Invoke",
                invoke_input(&function, &event),
            );
            Outcome::Wait(id, Phase::Proxy)
        }
        "AWS" => {
            let body = match request_body(flow, ctx) {
                Ok(b) => b,
                Err(res) => return Outcome::Done(res),
            };
            if let Some(function) = uri
                .contains(":lambda:path/")
                .then(|| proxy::lambda_target(&uri))
                .flatten()
            {
                let payload = if body.trim().is_empty() {
                    "{}".to_string()
                } else {
                    body
                };
                let id = ctx.uuid();
                ctx.call(
                    id.clone(),
                    "lambda",
                    "Invoke",
                    json!({ "FunctionName": function, "Payload": STANDARD.encode(payload) }),
                );
                return Outcome::Wait(id, Phase::Lambda);
            }
            match service_call(flow, ctx, &uri, &body) {
                Ok((service, op, input)) => {
                    let id = ctx.uuid();
                    ctx.call(id.clone(), service, op, input);
                    Outcome::Wait(id, Phase::Service)
                }
                Err(e) => Outcome::Done(json_response(
                    e.status,
                    &json!({ "__type": e.code, "message": e.message }),
                )),
            }
        }
        _ => Outcome::Done(message(
            502,
            &format!("wasmo: {kind} integrations do not reach the network"),
        )),
    }
}

/// Form (`Action=SendMessage&MessageBody=...`) or JSON request parameters.
fn params_of(body: &str) -> Map<String, Value> {
    let t = body.trim();
    if t.starts_with('{') {
        return serde_json::from_str::<Value>(t)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
    }
    t.split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((url_decode(k), json!(url_decode(v))))
        })
        .collect()
}

/// The call an `arn:aws:apigateway:<region>:<service>:action/<Action>` or
/// `:path/<path>` integration makes.
fn service_call(
    flow: &Flow,
    ctx: &Ctx<'_>,
    uri: &str,
    body: &str,
) -> Result<(String, String, Value), AwsError> {
    let rest = uri
        .strip_prefix("arn:aws:apigateway:")
        .ok_or_else(|| bad_request(format!("Invalid integration URI: {uri}")))?;
    let mut parts = rest.splitn(3, ':');
    let _region = parts.next();
    let service = parts.next().unwrap_or_default();
    let service = service.rsplit('.').next().unwrap_or(service);
    let target = parts.next().unwrap_or_default();
    let model = proxy::service_model(service).to_string();
    let mut params = params_of(body);
    let mut mapped = Map::new();
    for (k, v) in flow
        .method
        .integration
        .as_ref()
        .and_then(|i| i.info.get("requestParameters"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if let (Some(name), Some(v)) = (
            k.strip_prefix("integration.request.path."),
            v.as_str().and_then(|v| source(flow, v, ctx)),
        ) {
            mapped.insert(name.to_string(), json!(v));
        }
    }
    if let Some(action) = target.strip_prefix("action/") {
        let (action, query) = action.split_once(['&', '?']).unwrap_or((action, ""));
        params.extend(params_of(query));
        params.remove("Action");
        params.remove("Version");
        return Ok((model, action.to_string(), Value::Object(params)));
    }
    let path = target
        .strip_prefix("path/")
        .ok_or_else(|| bad_request(format!("Invalid integration URI: {uri}")))?;
    let path: String = path
        .split('/')
        .map(
            |seg| match seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                Some(name) => mapped.get(name).map(vtl::text).unwrap_or_default(),
                None => seg.to_string(),
            },
        )
        .collect::<Vec<_>>()
        .join("/");
    let http_method = flow
        .method
        .integration
        .as_ref()
        .and_then(|i| i.info.get("httpMethod"))
        .and_then(Value::as_str)
        .unwrap_or("POST")
        .to_uppercase();
    match model.as_str() {
        "sqs" => {
            let action = params
                .remove("Action")
                .and_then(|a| a.as_str().map(str::to_string))
                .unwrap_or_else(|| "SendMessage".into());
            params.remove("Version");
            params.insert(
                "QueueUrl".into(),
                json!(format!("{}/{}", ctx.endpoint, path.trim_matches('/'))),
            );
            Ok((model, action, Value::Object(params)))
        }
        "s3" => {
            let (bucket, key) = path
                .trim_start_matches('/')
                .split_once('/')
                .unwrap_or((path.trim_start_matches('/'), ""));
            let mut input = json!({ "Bucket": bucket });
            if !key.is_empty() {
                input["Key"] = json!(key);
            }
            let op = match (http_method.as_str(), key.is_empty()) {
                ("GET", true) => "ListObjectsV2",
                ("GET", false) => "GetObject",
                ("PUT", true) => "CreateBucket",
                ("PUT", false) => {
                    input["Body"] = json!(STANDARD.encode(body));
                    "PutObject"
                }
                ("DELETE", false) => "DeleteObject",
                ("HEAD", false) => "HeadObject",
                _ => {
                    return Err(bad_request(format!(
                        "wasmo: S3 {http_method} {path} is not supported"
                    )));
                }
            };
            Ok((model, op.to_string(), input))
        }
        _ => Err(bad_request(format!(
            "wasmo: path integrations for {service} are not supported"
        ))),
    }
}

/// Picks the integration response (by `selectionPattern` against the
/// Lambda error message or the status code) and applies its mapping.
/// Without integration responses the integration output passes through;
/// the flag says whether a response mapped it.
fn respond(
    flow: &Flow,
    ctx: &Ctx<'_>,
    body: &str,
    error: Option<&str>,
    status: u16,
    mock: bool,
) -> (Response, bool) {
    let responses = flow.method.integration.as_ref().map(|i| &i.responses);
    let subject = error.map_or_else(|| status.to_string(), str::to_string);
    let selected = |r: &&super::store::Obj| {
        r.get("selectionPattern")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
            .is_some_and(|p| {
                regex_lite::Regex::new(&format!("^(?:{p})$")).is_ok_and(|re| re.is_match(&subject))
            })
    };
    let default = |r: &&super::store::Obj| {
        r.get("selectionPattern")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    };
    let matched = responses.and_then(|rs| {
        rs.values()
            .find(selected)
            .or_else(|| rs.values().find(default))
    });
    let Some(ir) = matched else {
        let body = if mock {
            Vec::new()
        } else {
            body.as_bytes().to_vec()
        };
        return (Response::new(status, "application/json", body), false);
    };
    let code = ir
        .get("statusCode")
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let template = ir
        .get("responseTemplates")
        .and_then(|t| {
            t.get("application/json")
                .or_else(|| t.as_object()?.values().next())
        })
        .and_then(Value::as_str);
    let out = match template {
        Some(t) => vtl::render(t, &scope(flow, ctx, body)),
        None if mock => String::new(),
        None => body.to_string(),
    };
    let mut res = Response::new(code, "application/json", out.into_bytes());
    let data: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    for (k, v) in ir
        .get("responseParameters")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let Some(h) = k.strip_prefix("method.response.header.") else {
            continue;
        };
        let v = v.as_str().unwrap_or_default();
        let value = if let Some(lit) = v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
            Some(lit.to_string())
        } else if let Some(p) = v.strip_prefix("integration.response.body") {
            jsonpath::get(&data, &format!("${p}"))
                .ok()
                .flatten()
                .map(|x| vtl::text(&x))
        } else {
            None
        };
        if let Some(value) = value {
            res.headers.retain(|(x, _)| !x.eq_ignore_ascii_case(h));
            res.headers.push((h.to_lowercase(), value));
        }
    }
    (res, true)
}
