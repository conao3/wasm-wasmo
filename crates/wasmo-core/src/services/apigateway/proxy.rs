//! Request parsing, route matching, and Lambda proxy payloads shared by REST
//! APIs (payload 1.0) and HTTP APIs (payload 1.0 and 2.0).

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Map, Value, json};

use super::vtl::url_decode;
use crate::codec::time::format_http_date;
use crate::service::Ctx;
use crate::wire::{Request, Response};

/// An `execute-api` request with the stage split off.
#[derive(Debug, Clone)]
pub struct In {
    pub method: String,
    /// Path after the stage, starting with `/`.
    pub path: String,
    /// Path as the client sent it, including the stage.
    pub raw_path: String,
    pub raw_query: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub host: String,
}

impl In {
    pub fn new(req: &Request, path: &str) -> Self {
        let query = req
            .query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (url_decode(k), url_decode(v))
            })
            .collect();
        Self {
            method: req.method.to_uppercase(),
            path: if path.is_empty() {
                "/".into()
            } else {
                path.to_string()
            },
            raw_path: req.path.clone(),
            raw_query: req.query.clone(),
            query,
            headers: req
                .headers
                .iter()
                .filter(|(k, _)| !k.starts_with("x-wasmo"))
                .cloned()
                .collect(),
            body: req.body.clone(),
            host: req.header("host").unwrap_or_default().to_string(),
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn query_param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn header_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        for (k, v) in &self.headers {
            m.insert(k.clone(), json!(v));
        }
        m
    }

    fn multi_headers(&self) -> Map<String, Value> {
        let mut m: Map<String, Value> = Map::new();
        for (k, v) in &self.headers {
            let e = m.entry(k.clone()).or_insert_with(|| json!([]));
            if let Some(a) = e.as_array_mut() {
                a.extend(v.split(',').map(|x| json!(x.trim())));
            }
        }
        m
    }

    fn query_map(&self) -> Option<Map<String, Value>> {
        (!self.query.is_empty()).then(|| {
            let mut m = Map::new();
            for (k, v) in &self.query {
                m.insert(k.clone(), json!(v));
            }
            m
        })
    }

    fn multi_query(&self) -> Option<Map<String, Value>> {
        (!self.query.is_empty()).then(|| {
            let mut m: Map<String, Value> = Map::new();
            for (k, v) in &self.query {
                if let Some(a) = m
                    .entry(k.clone())
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                {
                    a.push(json!(v));
                }
            }
            m
        })
    }

    /// The body as the proxy payload carries it: text, or base64 when the
    /// bytes are not UTF-8.
    fn payload_body(&self) -> (Value, bool) {
        if self.body.is_empty() {
            return (Value::Null, false);
        }
        match std::str::from_utf8(&self.body) {
            Ok(s) => (json!(s), false),
            Err(_) => (json!(STANDARD.encode(&self.body)), true),
        }
    }
}

/// Matches a resource path or route path (`/pets/{id}`, `/{proxy+}`) and
/// returns the path parameters with a specificity score.
/// Literal segments, parameters, and whether the match is exact (no greedy
/// `{name+}`), compared to pick the most specific route.
pub type Score = (usize, usize, bool);

pub fn match_path(pattern: &str, path: &str) -> Option<(Score, Map<String, Value>)> {
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut params = Map::new();
    let (mut literal, mut vars) = (0, 0);
    for (i, p) in pat.iter().enumerate() {
        if let Some(name) = p.strip_prefix('{').and_then(|r| r.strip_suffix("+}")) {
            if i >= segs.len() {
                return None;
            }
            let rest: Vec<String> = segs[i..].iter().map(|s| url_decode(s)).collect();
            params.insert(name.to_string(), json!(rest.join("/")));
            return Some(((literal, vars, false), params));
        }
        let seg = segs.get(i)?;
        match p.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
            Some(name) => {
                params.insert(name.to_string(), json!(url_decode(seg)));
                vars += 1;
            }
            None if p == seg => literal += 1,
            None => return None,
        }
    }
    (pat.len() == segs.len()).then_some(((literal, vars, true), params))
}

/// `${stageVariables.name}` in an integration URI or parameter.
pub fn stage_vars(s: &str, vars: &Value) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(at) = rest.find("${stageVariables.") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 17..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                out.push_str(vars.get(name).and_then(Value::as_str).unwrap_or_default());
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[at..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The function ARN of `arn:aws:apigateway:<r>:lambda:path/2015-03-31/functions/<arn>/invocations`
/// or of a bare function ARN.
pub fn lambda_target(uri: &str) -> Option<String> {
    if let Some((_, rest)) = uri.split_once(":lambda:path/") {
        let rest = rest.split_once("/functions/")?.1;
        return Some(
            rest.strip_suffix("/invocations")
                .unwrap_or(rest)
                .to_string(),
        );
    }
    uri.contains(":lambda:").then(|| uri.to_string())
}

pub fn request_time(ctx: &Ctx<'_>) -> (String, u64) {
    let ms = ctx.now_ms();
    let http = format_http_date((ms / 1000.0).floor());
    let parts: Vec<&str> = http.split(' ').collect();
    let time = match parts.as_slice() {
        [_, d, m, y, t, _] => format!("{d}/{m}/{y}:{t} +0000"),
        _ => http.clone(),
    };
    (time, ms as u64)
}

pub struct Context<'a> {
    pub api_id: &'a str,
    pub stage: &'a str,
    pub request_id: &'a str,
    pub account: &'a str,
    pub resource_id: &'a str,
    pub resource_path: &'a str,
    pub route_key: &'a str,
    pub stage_variables: &'a Value,
    pub params: &'a Map<String, Value>,
    pub authorizer: Option<&'a Value>,
}

fn identity(r: &In) -> Value {
    json!({
        "cognitoIdentityPoolId": null,
        "accountId": null,
        "cognitoIdentityId": null,
        "caller": null,
        "sourceIp": r.header("x-forwarded-for").unwrap_or("127.0.0.1"),
        "principalOrgId": null,
        "accessKey": null,
        "cognitoAuthenticationType": null,
        "cognitoAuthenticationProvider": null,
        "userArn": null,
        "userAgent": r.header("user-agent").unwrap_or_default(),
        "user": null,
    })
}

/// The Lambda proxy event in payload format 1.0.
pub fn event_v1(ctx: &Ctx<'_>, r: &In, c: &Context<'_>) -> Value {
    let (time, epoch) = request_time(ctx);
    let (body, b64) = r.payload_body();
    let domain = r.host.split(':').next().unwrap_or_default();
    let mut rc = json!({
        "resourceId": c.resource_id,
        "resourcePath": c.resource_path,
        "httpMethod": r.method,
        "extendedRequestId": c.request_id,
        "requestTime": time,
        "path": r.raw_path,
        "accountId": c.account,
        "protocol": "HTTP/1.1",
        "stage": c.stage,
        "domainPrefix": domain.split('.').next().unwrap_or_default(),
        "requestTimeEpoch": epoch,
        "requestId": c.request_id,
        "identity": identity(r),
        "domainName": domain,
        "apiId": c.api_id,
    });
    if let Some(a) = c.authorizer {
        rc["authorizer"] = a.clone();
    }
    if !c.route_key.is_empty() {
        rc["routeKey"] = json!(c.route_key);
    }
    json!({
        "version": "1.0",
        "resource": c.resource_path,
        "path": r.path,
        "httpMethod": r.method,
        "headers": r.header_map(),
        "multiValueHeaders": r.multi_headers(),
        "queryStringParameters": r.query_map(),
        "multiValueQueryStringParameters": r.multi_query(),
        "requestContext": rc,
        "pathParameters": (!c.params.is_empty()).then(|| c.params.clone()),
        "stageVariables": c.stage_variables.as_object().filter(|v| !v.is_empty()),
        "body": body,
        "isBase64Encoded": b64,
    })
}

/// The Lambda proxy event in payload format 2.0 (HTTP APIs).
pub fn event_v2(ctx: &Ctx<'_>, r: &In, c: &Context<'_>) -> Value {
    let (time, epoch) = request_time(ctx);
    let (body, b64) = r.payload_body();
    let domain = r.host.split(':').next().unwrap_or_default();
    let mut headers = Map::new();
    let mut cookies = Vec::new();
    for (k, v) in &r.headers {
        if k.eq_ignore_ascii_case("cookie") {
            cookies.extend(v.split(';').map(|c| json!(c.trim())));
            continue;
        }
        let k = k.to_lowercase();
        match headers.get_mut(&k) {
            Some(Value::String(prev)) => {
                prev.push(',');
                prev.push_str(v);
            }
            _ => {
                headers.insert(k, json!(v));
            }
        }
    }
    let mut query = Map::new();
    for (k, v) in &r.query {
        match query.get_mut(k) {
            Some(Value::String(prev)) => {
                prev.push(',');
                prev.push_str(v);
            }
            _ => {
                query.insert(k.clone(), json!(v));
            }
        }
    }
    let mut rc = json!({
        "accountId": c.account,
        "apiId": c.api_id,
        "domainName": domain,
        "domainPrefix": domain.split('.').next().unwrap_or_default(),
        "http": {
            "method": r.method,
            "path": r.raw_path,
            "protocol": "HTTP/1.1",
            "sourceIp": r.header("x-forwarded-for").unwrap_or("127.0.0.1"),
            "userAgent": r.header("user-agent").unwrap_or_default(),
        },
        "requestId": c.request_id,
        "routeKey": c.route_key,
        "stage": c.stage,
        "time": time,
        "timeEpoch": epoch,
    });
    if let Some(a) = c.authorizer {
        rc["authorizer"] = a.clone();
    }
    let mut e = json!({
        "version": "2.0",
        "routeKey": c.route_key,
        "rawPath": r.raw_path,
        "rawQueryString": r.raw_query,
        "headers": headers,
        "requestContext": rc,
        "isBase64Encoded": b64,
    });
    if !cookies.is_empty() {
        e["cookies"] = json!(cookies);
    }
    if !query.is_empty() {
        e["queryStringParameters"] = Value::Object(query);
    }
    if !c.params.is_empty() {
        e["pathParameters"] = Value::Object(c.params.clone());
    }
    if c.stage_variables.as_object().is_some_and(|v| !v.is_empty()) {
        e["stageVariables"] = c.stage_variables.clone();
    }
    if !body.is_null() {
        e["body"] = body;
    }
    e
}

pub fn json_response(status: u16, body: &Value) -> Response {
    Response::new(status, "application/json", body.to_string().into_bytes())
}

pub fn message(status: u16, text: &str) -> Response {
    json_response(status, &json!({ "message": text }))
}

fn decode_body(v: &Value, b64: bool) -> Vec<u8> {
    let text = match v {
        Value::Null => return Vec::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    if b64 {
        STANDARD.decode(text.as_bytes()).unwrap_or_default()
    } else {
        text.into_bytes()
    }
}

fn add_headers(res: &mut Response, out: &Value) {
    for (k, v) in out
        .get("headers")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let v = super::vtl::text(v);
        res.headers.retain(|(h, _)| !h.eq_ignore_ascii_case(k));
        res.headers.push((k.to_lowercase(), v));
    }
    for (k, vs) in out
        .get("multiValueHeaders")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        res.headers.retain(|(h, _)| !h.eq_ignore_ascii_case(k));
        for v in vs.as_array().into_iter().flatten() {
            res.headers.push((k.to_lowercase(), super::vtl::text(v)));
        }
    }
}

/// The HTTP response a proxy Lambda result describes. Payload 1.0 requires
/// `statusCode`; payload 2.0 also takes any other JSON as a 200 body.
pub fn proxy_response(out: &Value, v2: bool) -> Option<Response> {
    let structured = out.get("statusCode").is_some();
    if !structured {
        if !v2 {
            return None;
        }
        let body = match out {
            Value::String(s) => s.clone().into_bytes(),
            other => other.to_string().into_bytes(),
        };
        return Some(Response::new(200, "application/json", body));
    }
    let status = out
        .get("statusCode")
        .and_then(|s| s.as_u64().or_else(|| s.as_str()?.parse().ok()))? as u16;
    let b64 = out.get("isBase64Encoded").and_then(Value::as_bool) == Some(true);
    let body = decode_body(out.get("body").unwrap_or(&Value::Null), b64);
    let mut res = Response::new(status, "application/json", body);
    add_headers(&mut res, out);
    for c in out
        .get("cookies")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        res.headers.push(("set-cookie".into(), super::vtl::text(c)));
    }
    Some(res)
}

/// The claims of a JWT, without checking its signature.
pub fn jwt_claims(token: &str) -> Option<Value> {
    let token = token.strip_prefix("Bearer ").unwrap_or(token).trim();
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The Lambda `Invoke` input for a JSON event.
pub fn invoke_input(function: &str, event: &Value) -> Value {
    json!({ "FunctionName": function, "Payload": STANDARD.encode(event.to_string()) })
}

/// The decoded payload of an `Invoke` output, and whether it is a function
/// error.
pub fn invoke_output(out: &Value) -> (Value, bool) {
    let payload = out
        .get("Payload")
        .and_then(Value::as_str)
        .and_then(|p| STANDARD.decode(p).ok())
        .unwrap_or_default();
    (
        serde_json::from_slice(&payload).unwrap_or(Value::Null),
        out.get("FunctionError").is_some(),
    )
}

/// Model name of the service an `arn:aws:apigateway:<r>:<service>:...` URI
/// names by its endpoint prefix.
pub fn service_model(prefix: &str) -> &str {
    match prefix {
        "states" => "stepfunctions",
        "monitoring" => "cloudwatch",
        "execute-api" => "apigateway",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_paths() {
        let (score, p) = match_path("/pets/{id}", "/pets/7").unwrap();
        assert_eq!((score, p["id"].as_str()), ((1, 1, true), Some("7")));
        let (_, p) = match_path("/files/{proxy+}", "/files/a/b%20c").unwrap();
        assert_eq!(p["proxy"], "a/b c");
        assert!(match_path("/files/{proxy+}", "/files").is_none());
        assert!(match_path("/pets", "/pets/7").is_none());
        assert_eq!(match_path("/", "/").unwrap().0, (0, 0, true));
        assert_eq!(
            stage_vars("fn-${stageVariables.env}", &json!({"env": "dev"})),
            "fn-dev"
        );
        assert_eq!(
            lambda_target("arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/arn:aws:lambda:us-east-1:000000000000:function:f/invocations").as_deref(),
            Some("arn:aws:lambda:us-east-1:000000000000:function:f")
        );
    }
}
