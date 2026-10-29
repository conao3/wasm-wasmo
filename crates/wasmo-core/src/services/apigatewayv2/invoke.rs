//! HTTP API invocation: stages (`$default` and named), route keys with
//! path parameters and greedy `{proxy+}`, CORS, JWT and Lambda authorizers,
//! Lambda proxy integrations (payload 1.0 and 2.0), AWS service integrations
//! by `IntegrationSubtype`, and parameter mapping.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use super::super::apigateway::proxy::{
    self, Context, In, invoke_input, invoke_output, json_response, message,
};
use super::super::apigateway::store::Obj;
use super::super::apigateway::vtl;
use super::ApiGatewayV2;
use crate::jsonpath;
use crate::service::{Ctx, OpResult};
use crate::wire::{Request, Response};

#[derive(Debug, Clone)]
struct Flow {
    api_id: String,
    stage: String,
    stage_vars: Value,
    route_key: String,
    route: Obj,
    integration: Obj,
    params: Map<String, Value>,
    req: In,
    request_id: String,
    authorizer: Option<Value>,
    cors: Option<Value>,
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    Authorizer { simple: bool, v2: bool },
    Lambda { v2: bool },
    Service,
}

pub(super) struct Pending {
    token: String,
    flow: Box<Flow>,
    phase: Phase,
}

enum Outcome {
    Done(Response),
    Wait(String, Phase),
}

fn forbidden() -> Response {
    message(403, "Forbidden")
}

fn unauthorized() -> Response {
    message(401, "Unauthorized")
}

fn internal() -> Response {
    message(500, "Internal Server Error")
}

/// Whether the method matched exactly, and the path specificity.
type Rank = (bool, proxy::Score);

/// The route for `method` and `path`: an exact method beats `ANY`, which
/// beats `$default`; among paths the most specific wins.
fn route<'r>(
    routes: &'r IndexMap<String, Obj>,
    method: &str,
    path: &str,
) -> Option<(&'r Obj, Map<String, Value>)> {
    let mut best: Option<(Rank, &Obj, Map<String, Value>)> = None;
    let mut fallback = None;
    for r in routes.values() {
        let key = r
            .get("RouteKey")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if key == "$default" {
            fallback = Some(r);
            continue;
        }
        let Some((verb, pattern)) = key.split_once(' ') else {
            continue;
        };
        if verb != method && verb != "ANY" {
            continue;
        }
        let Some((score, params)) = proxy::match_path(pattern, path) else {
            continue;
        };
        let rank = (verb == method, score);
        let better = best.as_ref().is_none_or(|(b, _, _)| {
            (rank.1.2, rank.1.0, rank.1.1, rank.0) > (b.1.2, b.1.0, b.1.1, b.0)
        });
        if better {
            best = Some((rank, r, params));
        }
    }
    best.map(|(_, r, p)| (r, p))
        .or_else(|| fallback.map(|r| (r, Map::new())))
}

fn cors_headers(cors: &Value, origin: Option<&str>, preflight: bool) -> Vec<(String, String)> {
    let list = |k: &str| -> Vec<String> {
        cors.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let origins = list("AllowOrigins");
    let Some(origin) = origin else {
        return Vec::new();
    };
    let allowed = origins
        .iter()
        .any(|o| o == "*" || o.eq_ignore_ascii_case(origin));
    if !allowed {
        return Vec::new();
    }
    let mut h = vec![(
        "access-control-allow-origin".to_string(),
        if origins.iter().any(|o| o == "*") {
            "*".to_string()
        } else {
            origin.to_string()
        },
    )];
    if cors.get("AllowCredentials").and_then(Value::as_bool) == Some(true) {
        h.push(("access-control-allow-credentials".into(), "true".into()));
    }
    let expose = list("ExposeHeaders");
    if !expose.is_empty() {
        h.push(("access-control-expose-headers".into(), expose.join(",")));
    }
    if preflight {
        let methods = list("AllowMethods");
        if !methods.is_empty() {
            h.push(("access-control-allow-methods".into(), methods.join(",")));
        }
        let headers = list("AllowHeaders");
        if !headers.is_empty() {
            h.push(("access-control-allow-headers".into(), headers.join(",")));
        }
        if let Some(age) = cors.get("MaxAge").and_then(Value::as_i64) {
            h.push(("access-control-max-age".into(), age.to_string()));
        }
    }
    h
}

pub(super) fn serve(
    gw: &mut ApiGatewayV2,
    ctx: &mut Ctx<'_>,
    api_id: &str,
    req: &Request,
) -> Option<Response> {
    let a = gw
        .regions
        .get(&(ctx.account.clone(), ctx.region.clone()))?
        .apis
        .get(api_id)?;
    if a.info.get("ProtocolType").and_then(Value::as_str) == Some("WEBSOCKET") {
        return Some(message(
            400,
            "wasmo: WebSocket APIs take connections, not HTTP requests",
        ));
    }
    let trimmed = req.path.trim_start_matches('/');
    let (first, rest) = trimmed.split_once('/').unwrap_or((trimmed, ""));
    let (stage_name, path) = if first != "$default" && a.stages.contains_key(first) {
        (first.to_string(), format!("/{rest}"))
    } else if a.stages.contains_key("$default") {
        ("$default".to_string(), req.path.clone())
    } else {
        return Some(message(404, "Not Found"));
    };
    let stage = &a.stages[&stage_name];
    let deployed = stage
        .get("DeploymentId")
        .and_then(Value::as_str)
        .filter(|_| stage.get("AutoDeploy").and_then(Value::as_bool) != Some(true))
        .and_then(|d| a.deployments.get(d));
    let (routes, integrations) = deployed.map_or((&a.routes, &a.integrations), |d| {
        (&d.routes, &d.integrations)
    });
    let input = In::new(req, &path);
    let cors = a.info.get("CorsConfiguration").cloned();
    if let Some(c) = &cors
        && input.method == "OPTIONS"
        && input.header("access-control-request-method").is_some()
    {
        let mut res = Response::new(204, "application/json", Vec::new());
        res.headers
            .extend(cors_headers(c, input.header("origin"), true));
        return Some(res);
    }
    let Some((r, params)) = route(routes, &input.method, &input.path) else {
        return Some(message(404, "Not Found"));
    };
    let integration = r
        .get("Target")
        .and_then(Value::as_str)
        .and_then(|t| t.strip_prefix("integrations/"))
        .and_then(|t| integrations.get(t))
        .cloned()
        .unwrap_or_default();
    let flow = Flow {
        api_id: api_id.to_string(),
        stage: stage_name,
        stage_vars: stage.get("StageVariables").cloned().unwrap_or(json!({})),
        route_key: r
            .get("RouteKey")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        route: r.clone(),
        integration,
        params,
        req: input,
        request_id: ctx.uuid(),
        authorizer: None,
        cors,
    };
    let authorizer = flow
        .route
        .get("AuthorizerId")
        .and_then(Value::as_str)
        .and_then(|id| a.authorizers.get(id))
        .cloned();
    let outcome = begin(ctx, flow.clone(), authorizer);
    Some(match outcome {
        Outcome::Done(res) => finish(res, &flow),
        Outcome::Wait(call, phase) => {
            let token = ctx.uuid();
            ctx.awaiting = Some(token.clone());
            gw.pending.insert(
                call,
                Pending {
                    token,
                    flow: Box::new(flow),
                    phase,
                },
            );
            Response::default()
        }
    })
}

fn finish(mut res: Response, flow: &Flow) -> Response {
    if let Some(c) = &flow.cors {
        res.headers
            .extend(cors_headers(c, flow.req.header("origin"), false));
    }
    res.headers
        .push(("apigw-requestid".into(), flow.request_id.clone()));
    res
}

pub(super) fn answer(gw: &mut ApiGatewayV2, ctx: &mut Ctx<'_>, id: &str, result: OpResult) {
    let Some(mut p) = gw.pending.remove(id) else {
        return;
    };
    match resume(ctx, &mut p, result) {
        Outcome::Done(res) => {
            let res = finish(res, &p.flow);
            ctx.respond(p.token, res);
        }
        Outcome::Wait(call, phase) => {
            p.phase = phase;
            gw.pending.insert(call, p);
        }
    }
}

fn resume(ctx: &mut Ctx<'_>, p: &mut Pending, result: OpResult) -> Outcome {
    match p.phase {
        Phase::Authorizer { simple, v2 } => {
            let Ok(out) = result else {
                return Outcome::Done(internal());
            };
            let (payload, error) = invoke_output(&out);
            if error {
                return Outcome::Done(internal());
            }
            let context = payload.get("context").cloned().unwrap_or(json!({}));
            let allowed = if simple {
                payload.get("isAuthorized").and_then(Value::as_bool) == Some(true)
            } else {
                let statements = payload
                    .get("policyDocument")
                    .and_then(|d| d.get("Statement"))
                    .map(|s| if s.is_array() { s.clone() } else { json!([s]) })
                    .unwrap_or(json!([]));
                let effects: Vec<&str> = statements
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s.get("Effect").and_then(Value::as_str))
                    .collect();
                effects.contains(&"Allow") && !effects.contains(&"Deny")
            };
            if !allowed {
                return Outcome::Done(forbidden());
            }
            p.flow.authorizer = Some(if v2 {
                json!({ "lambda": context })
            } else {
                let mut c = context.as_object().cloned().unwrap_or_default();
                if let Some(id) = payload.get("principalId") {
                    c.insert("principalId".into(), id.clone());
                }
                Value::Object(c)
            });
            integrate(ctx, &p.flow)
        }
        Phase::Lambda { v2 } => match result.map(|o| invoke_output(&o)) {
            Ok((payload, false)) => match proxy::proxy_response(&payload, v2) {
                Some(res) => Outcome::Done(map_response(&p.flow, ctx, res)),
                None => Outcome::Done(internal()),
            },
            _ => Outcome::Done(internal()),
        },
        Phase::Service => {
            let res = match result {
                Ok(out) => json_response(200, &out),
                Err(e) => json_response(e.status, &json!({ "message": e.message })),
            };
            Outcome::Done(map_response(&p.flow, ctx, res))
        }
    }
}

/// `$request.header.x`, `$request.querystring.x`, `$request.path.x`,
/// `$request.path`, `$request.body.a.b`, `$context.x`, `$stageVariables.x`,
/// literals, and `${...}` placeholders inside a string.
fn param(flow: &Flow, ctx: &Ctx<'_>, expr: &str, response: Option<&Response>) -> Option<String> {
    let selector = |s: &str| -> Option<String> {
        let r = &flow.req;
        if let Some(h) = s.strip_prefix("request.header.") {
            return r.header(h).map(str::to_string);
        }
        if let Some(q) = s.strip_prefix("request.querystring.") {
            return r.query_param(q).map(str::to_string);
        }
        if s == "request.path" {
            return Some(r.path.clone());
        }
        if let Some(p) = s.strip_prefix("request.path.") {
            return flow.params.get(p).map(vtl::text);
        }
        if let Some(b) = s.strip_prefix("request.body") {
            let data: Value = serde_json::from_slice(&r.body).ok()?;
            return jsonpath::get(&data, &format!("${b}"))
                .ok()
                .flatten()
                .map(|v| vtl::text(&v));
        }
        if let Some(h) = s.strip_prefix("response.header.") {
            let res = response?;
            return res
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(h))
                .map(|(_, v)| v.clone());
        }
        if let Some(b) = s.strip_prefix("response.body") {
            let data: Value = serde_json::from_slice(&response?.body).ok()?;
            return jsonpath::get(&data, &format!("${b}"))
                .ok()
                .flatten()
                .map(|v| vtl::text(&v));
        }
        if let Some(v) = s.strip_prefix("stageVariables.") {
            return flow.stage_vars.get(v).map(vtl::text);
        }
        if let Some(c) = s.strip_prefix("context.") {
            return jsonpath::get(&context(flow, ctx), &format!("$.{c}"))
                .ok()
                .flatten()
                .map(|v| vtl::text(&v));
        }
        None
    };
    if let Some(s) = expr.strip_prefix('$')
        && !s.starts_with('{')
        && !s.contains(char::is_whitespace)
    {
        return selector(s);
    }
    let mut out = String::new();
    let mut rest = expr;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let Some(end) = rest[at..].find('}') else {
            break;
        };
        out.push_str(&selector(&rest[at + 2..at + end]).unwrap_or_default());
        rest = &rest[at + end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

fn context(flow: &Flow, ctx: &Ctx<'_>) -> Value {
    let (time, epoch) = proxy::request_time(ctx);
    json!({
        "accountId": ctx.account,
        "apiId": flow.api_id,
        "requestId": flow.request_id,
        "routeKey": flow.route_key,
        "stage": flow.stage,
        "requestTime": time,
        "requestTimeEpoch": epoch,
        "httpMethod": flow.req.method,
        "path": flow.req.raw_path,
        "domainName": flow.req.host,
        "authorizer": flow.authorizer.clone().unwrap_or(Value::Null),
    })
}

fn route_arn(ctx: &Ctx<'_>, flow: &Flow) -> String {
    format!(
        "arn:aws:execute-api:{}:{}:{}/{}/{}{}",
        ctx.region, ctx.account, flow.api_id, flow.stage, flow.req.method, flow.req.path
    )
}

fn proxy_context<'f>(ctx: &'f Ctx<'_>, flow: &'f Flow) -> Context<'f> {
    Context {
        api_id: &flow.api_id,
        stage: &flow.stage,
        request_id: &flow.request_id,
        account: &ctx.account,
        resource_id: &flow.route_key,
        resource_path: flow
            .route_key
            .split_once(' ')
            .map_or(&flow.route_key, |(_, p)| p),
        route_key: &flow.route_key,
        stage_variables: &flow.stage_vars,
        params: &flow.params,
        authorizer: flow.authorizer.as_ref(),
    }
}

fn jwt(flow: &Flow, ctx: &Ctx<'_>, auth: &Obj) -> Result<Value, Response> {
    let token = identity(flow, ctx, auth)
        .and_then(|v| v.into_iter().next())
        .ok_or_else(unauthorized)?;
    let claims = proxy::jwt_claims(&token).ok_or_else(unauthorized)?;
    let config = auth.get("JwtConfiguration").cloned().unwrap_or(json!({}));
    if let Some(issuer) = config.get("Issuer").and_then(Value::as_str)
        && claims.get("iss").and_then(Value::as_str) != Some(issuer)
    {
        return Err(unauthorized());
    }
    let audiences: Vec<&str> = config
        .get("Audience")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !audiences.is_empty() {
        let aud: Vec<String> = match claims.get("aud") {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => claims
                .get("client_id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .into_iter()
                .collect(),
        };
        if !aud.iter().any(|a| audiences.contains(&a.as_str())) {
            return Err(unauthorized());
        }
    }
    if claims
        .get("exp")
        .and_then(Value::as_f64)
        .is_some_and(|e| e * 1000.0 < ctx.now_ms())
    {
        return Err(unauthorized());
    }
    let scopes: Vec<String> = match (claims.get("scope"), claims.get("scp")) {
        (Some(Value::String(s)), _) => s.split_whitespace().map(str::to_string).collect(),
        (_, Some(Value::Array(a))) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    let wanted: Vec<&str> = flow
        .route
        .get("AuthorizationScopes")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !wanted.is_empty() && !wanted.iter().any(|w| scopes.iter().any(|s| s == w)) {
        return Err(forbidden());
    }
    let claims: Map<String, Value> = claims
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), json!(vtl::text(v))))
        .collect();
    Ok(json!({ "jwt": { "claims": claims, "scopes": (!scopes.is_empty()).then_some(scopes) } }))
}

/// The values of an authorizer's `IdentitySource`, or None when one is
/// missing.
fn identity(flow: &Flow, ctx: &Ctx<'_>, auth: &Obj) -> Option<Vec<String>> {
    let sources: Vec<&str> = auth
        .get("IdentitySource")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    sources
        .iter()
        .map(|s| param(flow, ctx, s, None).filter(|v| !v.is_empty()))
        .collect()
}

fn begin(ctx: &mut Ctx<'_>, mut flow: Flow, authorizer: Option<Obj>) -> Outcome {
    let kind = flow
        .route
        .get("AuthorizationType")
        .and_then(Value::as_str)
        .unwrap_or("NONE");
    match (kind, authorizer) {
        ("AWS_IAM", _) => {
            if !flow
                .req
                .header("authorization")
                .is_some_and(|a| a.starts_with("AWS4-HMAC-SHA256"))
            {
                return Outcome::Done(forbidden());
            }
        }
        ("JWT", Some(auth)) => match jwt(&flow, ctx, &auth) {
            Ok(a) => flow.authorizer = Some(a),
            Err(res) => return Outcome::Done(res),
        },
        ("CUSTOM", Some(auth)) => {
            let configured = auth
                .get("IdentitySource")
                .and_then(Value::as_array)
                .is_some_and(|a| !a.is_empty());
            let values = identity(&flow, ctx, &auth);
            if configured && values.is_none() {
                return Outcome::Done(unauthorized());
            }
            let Some(function) = auth
                .get("AuthorizerUri")
                .and_then(Value::as_str)
                .and_then(proxy::lambda_target)
                .map(|f| proxy::stage_vars(&f, &flow.stage_vars))
            else {
                return Outcome::Done(internal());
            };
            let v2 = auth
                .get("AuthorizerPayloadFormatVersion")
                .and_then(Value::as_str)
                != Some("1.0");
            let simple =
                v2 && auth.get("EnableSimpleResponses").and_then(Value::as_bool) == Some(true);
            let event = if v2 {
                let mut e = proxy::event_v2(ctx, &flow.req, &proxy_context(ctx, &flow));
                e["type"] = json!("REQUEST");
                e["routeArn"] = json!(route_arn(ctx, &flow));
                e["identitySource"] = json!(values.unwrap_or_default());
                e
            } else {
                let mut e = proxy::event_v1(ctx, &flow.req, &proxy_context(ctx, &flow));
                e["type"] = json!("REQUEST");
                e["methodArn"] = json!(route_arn(ctx, &flow));
                e
            };
            let id = ctx.uuid();
            ctx.call(
                id.clone(),
                "lambda",
                "Invoke",
                invoke_input(&function, &event),
            );
            return Outcome::Wait(id, Phase::Authorizer { simple, v2 });
        }
        _ => {}
    }
    integrate(ctx, &flow)
}

/// Applies `append:` / `overwrite:` / `remove:` request parameter mappings
/// to the request a proxy integration forwards.
fn map_request(flow: &Flow, ctx: &Ctx<'_>) -> In {
    let mut r = flow.req.clone();
    for (k, v) in flow
        .integration
        .get("RequestParameters")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let Some((op, target)) = k.split_once(':') else {
            continue;
        };
        let value = v
            .as_str()
            .and_then(|v| param(flow, ctx, v, None))
            .unwrap_or_default();
        if let Some(h) = target.strip_prefix("header.") {
            let h = h.to_lowercase();
            match op {
                "remove" => r.headers.retain(|(x, _)| *x != h),
                "overwrite" => {
                    r.headers.retain(|(x, _)| *x != h);
                    r.headers.push((h, value));
                }
                _ => match r.headers.iter_mut().find(|(x, _)| *x == h) {
                    Some((_, old)) => *old = format!("{old},{value}"),
                    None => r.headers.push((h, value)),
                },
            }
        } else if let Some(q) = target.strip_prefix("querystring.") {
            if op != "append" {
                r.query.retain(|(x, _)| x != q);
            }
            if op != "remove" {
                r.query.push((q.to_string(), value));
            }
            r.raw_query = r
                .query
                .iter()
                .map(|(k, v)| format!("{}={}", vtl::url_encode(k), vtl::url_encode(v)))
                .collect::<Vec<_>>()
                .join("&");
        } else if target == "path" && op == "overwrite" {
            r.path = value;
        }
    }
    r
}

/// The AWS call an `IntegrationSubtype` makes with the mapped parameters.
fn subtype_call(
    flow: &Flow,
    ctx: &Ctx<'_>,
    subtype: &str,
) -> Option<(&'static str, &'static str, Value)> {
    let mut p = Map::new();
    for (k, v) in flow
        .integration
        .get("RequestParameters")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        if let Some(value) = v.as_str().and_then(|v| param(flow, ctx, v, None)) {
            let parsed = match k.as_str() {
                "MessageAttributes" | "Resources" | "AttributeNames" | "MessageAttributeNames" => {
                    serde_json::from_str(&value).unwrap_or(json!(value))
                }
                _ => json!(value),
            };
            p.insert(k.clone(), parsed);
        }
    }
    let camel = |p: &Map<String, Value>, keys: &[(&str, &str)]| -> Value {
        let mut o = Map::new();
        for (from, to) in keys {
            if let Some(v) = p.get(*from) {
                o.insert(to.to_string(), v.clone());
            }
        }
        Value::Object(o)
    };
    Some(match subtype {
        "SQS-SendMessage" => ("sqs", "SendMessage", Value::Object(p)),
        "SQS-ReceiveMessage" => ("sqs", "ReceiveMessage", Value::Object(p)),
        "SQS-DeleteMessage" => ("sqs", "DeleteMessage", Value::Object(p)),
        "SQS-PurgeQueue" => ("sqs", "PurgeQueue", Value::Object(p)),
        "EventBridge-PutEvents" => {
            p.remove("Region");
            ("events", "PutEvents", json!({ "Entries": [p] }))
        }
        "Kinesis-PutRecord" => {
            if let Some(d) = p.get("Data").map(vtl::text) {
                p.insert("Data".into(), json!(STANDARD.encode(d)));
            }
            p.remove("Region");
            ("kinesis", "PutRecord", Value::Object(p))
        }
        s @ ("StepFunctions-StartExecution" | "StepFunctions-StartSyncExecution") => (
            "stepfunctions",
            if s.ends_with("SyncExecution") {
                "StartSyncExecution"
            } else {
                "StartExecution"
            },
            camel(
                &p,
                &[
                    ("StateMachineArn", "stateMachineArn"),
                    ("Name", "name"),
                    ("Input", "input"),
                    ("TraceHeader", "traceHeader"),
                ],
            ),
        ),
        "StepFunctions-StopExecution" => (
            "stepfunctions",
            "StopExecution",
            camel(
                &p,
                &[
                    ("ExecutionArn", "executionArn"),
                    ("Cause", "cause"),
                    ("Error", "error"),
                ],
            ),
        ),
        _ => return None,
    })
}

fn integrate(ctx: &mut Ctx<'_>, flow: &Flow) -> Outcome {
    let i = &flow.integration;
    let kind = i
        .get("IntegrationType")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if kind != "AWS_PROXY" {
        return Outcome::Done(if kind.is_empty() {
            internal()
        } else {
            message(
                502,
                &format!("wasmo: {kind} integrations do not reach the network"),
            )
        });
    }
    if let Some(subtype) = i.get("IntegrationSubtype").and_then(Value::as_str) {
        let Some((service, op, input)) = subtype_call(flow, ctx, subtype) else {
            return Outcome::Done(message(
                500,
                &format!("wasmo: the integration subtype {subtype} is not supported"),
            ));
        };
        let id = ctx.uuid();
        ctx.call(id.clone(), service, op, input);
        return Outcome::Wait(id, Phase::Service);
    }
    let uri = proxy::stage_vars(
        i.get("IntegrationUri")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        &flow.stage_vars,
    );
    let Some(function) = proxy::lambda_target(&uri) else {
        return Outcome::Done(internal());
    };
    let v2 = i.get("PayloadFormatVersion").and_then(Value::as_str) != Some("1.0");
    let req = map_request(flow, ctx);
    let c = proxy_context(ctx, flow);
    let event = if v2 {
        proxy::event_v2(ctx, &req, &c)
    } else {
        proxy::event_v1(ctx, &req, &c)
    };
    let id = ctx.uuid();
    ctx.call(
        id.clone(),
        "lambda",
        "Invoke",
        invoke_input(&function, &event),
    );
    Outcome::Wait(id, Phase::Lambda { v2 })
}

/// Applies the integration's `ResponseParameters` for the response status.
fn map_response(flow: &Flow, ctx: &Ctx<'_>, mut res: Response) -> Response {
    let Some(map) = flow
        .integration
        .get("ResponseParameters")
        .and_then(|r| r.get(res.status.to_string()))
        .and_then(Value::as_object)
        .cloned()
    else {
        return res;
    };
    let original = res.clone();
    for (k, v) in map {
        let Some((op, target)) = k.split_once(':') else {
            continue;
        };
        let value = v
            .as_str()
            .and_then(|v| param(flow, ctx, v, Some(&original)))
            .unwrap_or_default();
        if target == "statuscode" {
            if let Ok(s) = value.parse() {
                res.status = s;
            }
        } else if let Some(h) = target.strip_prefix("header.") {
            let h = h.to_lowercase();
            if op != "append" {
                res.headers.retain(|(x, _)| *x != h);
            }
            if op != "remove" {
                res.headers.push((h, value));
            }
        }
    }
    res
}
