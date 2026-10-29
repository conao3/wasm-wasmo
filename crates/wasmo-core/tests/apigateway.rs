mod common;

use common::Mock;
use serde_json::{Value, json};

fn gw(m: &mut Mock, method: &str, path: &str, body: Value) -> Value {
    let (status, out) = m.rest("apigateway", method, path, body);
    assert!(status < 300, "{method} {path}: {status} {out}");
    out
}

fn resource(m: &mut Mock, api: &str, parent: &str, part: &str) -> String {
    gw(
        m,
        "POST",
        &format!("/restapis/{api}/resources/{parent}"),
        json!({ "pathPart": part }),
    )["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn method(m: &mut Mock, api: &str, res: &str, verb: &str, extra: Value) {
    let mut body = json!({ "authorizationType": "NONE" });
    body.as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    gw(
        m,
        "PUT",
        &format!("/restapis/{api}/resources/{res}/methods/{verb}"),
        body,
    );
}

fn integration(m: &mut Mock, api: &str, res: &str, verb: &str, body: Value) {
    gw(
        m,
        "PUT",
        &format!("/restapis/{api}/resources/{res}/methods/{verb}/integration"),
        body,
    );
}

fn lambda_uri(arn: &str) -> String {
    format!("arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/{arn}/invocations")
}

fn body(res: &wasmo_core::wire::Response) -> Value {
    serde_json::from_slice(&res.body).unwrap_or_else(|_| json!(String::from_utf8_lossy(&res.body)))
}

#[test]
fn rest_api_routes_to_lambda_mock_and_services() {
    let mut m = Mock::new();
    let echo = m.function("echo");
    let auth = m.function("auth");
    m.register("echo", |e| {
        Ok(json!({
            "statusCode": 200,
            "headers": { "x-echo": "1" },
            "body": json!({
                "resource": e["resource"],
                "path": e["path"],
                "params": e["pathParameters"],
                "query": e["queryStringParameters"],
                "stage": e["requestContext"]["stage"],
                "vars": e["stageVariables"],
                "principal": e["requestContext"]["authorizer"]["principalId"],
                "body": e["body"],
            }).to_string(),
        }))
    });
    m.register("auth", |e| {
        let allow = e["authorizationToken"] == "allow";
        Ok(json!({
            "principalId": "user-1",
            "policyDocument": { "Version": "2012-10-17", "Statement": [{ "Action": "execute-api:Invoke", "Effect": if allow { "Allow" } else { "Deny" }, "Resource": e["methodArn"] }] },
            "context": { "tier": "gold" },
        }))
    });
    let (queue_url, _) = m.queue("orders");

    let api = gw(&mut m, "POST", "/restapis", json!({ "name": "shop" }));
    let id = api["id"].as_str().unwrap().to_string();
    let root = api["rootResourceId"].as_str().unwrap().to_string();
    let pets = resource(&mut m, &id, &root, "pets");
    let pet = resource(&mut m, &id, &pets, "{id}");
    let files = resource(&mut m, &id, &root, "files");
    let proxy = resource(&mut m, &id, &files, "{proxy+}");
    let orders = resource(&mut m, &id, &root, "orders");
    let secure = resource(&mut m, &id, &root, "secure");

    method(&mut m, &id, &pet, "GET", json!({}));
    integration(
        &mut m,
        &id,
        &pet,
        "GET",
        json!({ "type": "AWS_PROXY", "httpMethod": "POST", "uri": lambda_uri(&echo) }),
    );
    method(&mut m, &id, &proxy, "ANY", json!({}));
    integration(
        &mut m,
        &id,
        &proxy,
        "ANY",
        json!({ "type": "AWS_PROXY", "httpMethod": "POST", "uri": lambda_uri(&echo) }),
    );

    method(
        &mut m,
        &id,
        &pets,
        "POST",
        json!({ "apiKeyRequired": true }),
    );
    integration(
        &mut m,
        &id,
        &pets,
        "POST",
        json!({ "type": "MOCK", "requestTemplates": { "application/json": "{\"statusCode\": 201}" } }),
    );
    gw(
        &mut m,
        "PUT",
        &format!("/restapis/{id}/resources/{pets}/methods/POST/integration/responses/201"),
        json!({
            "selectionPattern": "201",
            "responseTemplates": { "application/json": "{\"created\": \"$context.stage\", \"env\": \"$stageVariables.env\"}" },
            "responseParameters": { "method.response.header.Location": "'/pets/1'" },
        }),
    );

    method(&mut m, &id, &orders, "POST", json!({}));
    integration(
        &mut m,
        &id,
        &orders,
        "POST",
        json!({
            "type": "AWS",
            "httpMethod": "POST",
            "uri": "arn:aws:apigateway:us-east-1:sqs:path/000000000000/orders",
            "requestTemplates": { "application/json": "Action=SendMessage&MessageBody=$util.urlEncode($input.json('$.order'))" },
        }),
    );
    gw(
        &mut m,
        "PUT",
        &format!("/restapis/{id}/resources/{orders}/methods/POST/integration/responses/200"),
        json!({
            "responseTemplates": { "application/json": "{\"queued\": \"$input.path('$.MessageId')\"}" },
        }),
    );

    let authorizer = gw(&mut m, "POST", &format!("/restapis/{id}/authorizers"), json!({
        "name": "tok", "type": "TOKEN", "authorizerUri": lambda_uri(&auth), "identitySource": "method.request.header.Authorization",
    }))["id"]
        .as_str()
        .unwrap()
        .to_string();
    method(
        &mut m,
        &id,
        &secure,
        "GET",
        json!({ "authorizationType": "CUSTOM", "authorizerId": authorizer }),
    );
    integration(
        &mut m,
        &id,
        &secure,
        "GET",
        json!({ "type": "AWS_PROXY", "httpMethod": "POST", "uri": lambda_uri(&echo) }),
    );

    gw(
        &mut m,
        "POST",
        &format!("/restapis/{id}/deployments"),
        json!({ "stageName": "prod", "variables": { "env": "production" } }),
    );
    let key = gw(
        &mut m,
        "POST",
        "/apikeys",
        json!({ "name": "k", "enabled": true, "value": "test-key" }),
    );
    assert_eq!(key["value"], "test-key");

    let host = format!("https://{id}.execute-api.us-east-1.amazonaws.com");
    let res = m.raw("GET", &format!("{host}/prod/pets/7?color=red"), vec![], b"");
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    assert!(res.headers.iter().any(|(k, v)| k == "x-echo" && v == "1"));
    let b = body(&res);
    assert_eq!(b["resource"], "/pets/{id}");
    assert_eq!(b["path"], "/pets/7");
    assert_eq!(b["params"], json!({ "id": "7" }));
    assert_eq!(b["query"], json!({ "color": "red" }));
    assert_eq!(b["stage"], "prod");
    assert_eq!(b["vars"], json!({ "env": "production" }));

    let res = m.raw(
        "PUT",
        &format!("http://wasmo.local/restapis/{id}/prod/_user_request_/files/a/b.txt"),
        vec![],
        b"data",
    );
    let b = body(&res);
    assert_eq!(b["params"], json!({ "proxy": "a/b.txt" }));
    assert_eq!(b["body"], "data");

    let res = m.raw(
        "POST",
        &format!("{host}/prod/pets"),
        vec![("content-type", "application/json")],
        b"{}",
    );
    assert_eq!(res.status, 403);
    let res = m.raw(
        "POST",
        &format!("{host}/prod/pets"),
        vec![
            ("content-type", "application/json"),
            ("x-api-key", "test-key"),
        ],
        b"{}",
    );
    assert_eq!(res.status, 201, "{}", String::from_utf8_lossy(&res.body));
    assert_eq!(
        body(&res),
        json!({ "created": "prod", "env": "production" })
    );
    assert!(
        res.headers
            .iter()
            .any(|(k, v)| k == "location" && v == "/pets/1")
    );

    let res = m.raw(
        "POST",
        &format!("{host}/prod/orders"),
        vec![("content-type", "application/json")],
        br#"{"order":{"sku":"a1","qty":2}}"#,
    );
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    assert!(body(&res)["queued"].as_str().is_some_and(|s| !s.is_empty()));
    let msgs = m.receive(&queue_url);
    assert_eq!(msgs[0]["Body"], "{\"sku\":\"a1\",\"qty\":2}");

    let res = m.raw("GET", &format!("{host}/prod/secure"), vec![], b"");
    assert_eq!(res.status, 401);
    let res = m.raw(
        "GET",
        &format!("{host}/prod/secure"),
        vec![("authorization", "deny")],
        b"",
    );
    assert_eq!(res.status, 403);
    let res = m.raw(
        "GET",
        &format!("{host}/prod/secure"),
        vec![("authorization", "allow")],
        b"",
    );
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    assert_eq!(body(&res)["principal"], "user-1");

    let res = m.raw("GET", &format!("{host}/prod/nothing"), vec![], b"");
    assert_eq!(res.status, 403);
    assert_eq!(
        body(&res),
        json!({ "message": "Missing Authentication Token" })
    );
    assert_eq!(
        m.raw("GET", &format!("{host}/dev/pets/1"), vec![], b"")
            .status,
        403
    );

    let t = gw(
        &mut m,
        "POST",
        &format!("/restapis/{id}/resources/{pet}/methods/GET"),
        json!({ "pathWithQueryString": "/pets/9", "stageVariables": { "env": "t" } }),
    );
    assert_eq!(t["status"], 200);
    let tb: Value = serde_json::from_str(t["body"].as_str().unwrap()).unwrap();
    assert_eq!(tb["params"], json!({ "id": "9" }));
}

#[test]
fn deployments_snapshot_and_control_plane() {
    let mut m = Mock::new();
    let api = gw(&mut m, "POST", "/restapis", json!({ "name": "snap" }));
    let id = api["id"].as_str().unwrap().to_string();
    let root = api["rootResourceId"].as_str().unwrap().to_string();
    method(&mut m, &id, &root, "GET", json!({}));
    integration(
        &mut m,
        &id,
        &root,
        "GET",
        json!({ "type": "MOCK", "requestTemplates": { "application/json": "{\"statusCode\": 200}" } }),
    );
    gw(
        &mut m,
        "PUT",
        &format!("/restapis/{id}/resources/{root}/methods/GET/integration/responses/200"),
        json!({
            "responseTemplates": { "application/json": "{\"v\": 1}" },
        }),
    );
    gw(
        &mut m,
        "POST",
        &format!("/restapis/{id}/deployments"),
        json!({ "stageName": "v1" }),
    );
    gw(
        &mut m,
        "PATCH",
        &format!("/restapis/{id}/resources/{root}/methods/GET/integration/responses/200"),
        json!({
            "patchOperations": [{ "op": "replace", "path": "/responseTemplates/application~1json", "value": "{\"v\": 2}" }],
        }),
    );
    let host = format!("http://{id}.execute-api.wasmo.local");
    assert_eq!(
        body(&m.raw("GET", &format!("{host}/v1"), vec![], b"")),
        json!({ "v": 1 })
    );
    let d = gw(
        &mut m,
        "POST",
        &format!("/restapis/{id}/deployments"),
        json!({}),
    );
    gw(
        &mut m,
        "PATCH",
        &format!("/restapis/{id}/stages/v1"),
        json!({
            "patchOperations": [
                { "op": "replace", "path": "/deploymentId", "value": d["id"] },
                { "op": "replace", "path": "/variables/color", "value": "blue" },
                { "op": "replace", "path": "/*/*/metrics/enabled", "value": "true" },
            ],
        }),
    );
    assert_eq!(
        body(&m.raw("GET", &format!("{host}/v1/"), vec![], b"")),
        json!({ "v": 2 })
    );
    let stage = gw(
        &mut m,
        "GET",
        &format!("/restapis/{id}/stages/v1"),
        Value::Null,
    );
    assert_eq!(stage["variables"], json!({ "color": "blue" }));
    assert_eq!(stage["methodSettings"]["*/*"]["metricsEnabled"], true);
    assert_eq!(
        gw(
            &mut m,
            "GET",
            &format!("/restapis/{id}/deployments"),
            Value::Null
        )["item"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    let (status, err) = m.rest("apigateway", "GET", "/restapis/nope", Value::Null);
    assert_eq!((status, err["message"].as_str().is_some()), (404, true));
    let arn = format!("arn:aws:apigateway:us-east-1::/restapis/{id}");
    gw(
        &mut m,
        "PUT",
        &format!("/tags/{}", arn.replace('/', "%2F")),
        json!({ "tags": { "team": "a" } }),
    );
    assert_eq!(
        gw(
            &mut m,
            "GET",
            &format!("/tags/{}", arn.replace('/', "%2F")),
            Value::Null
        )["tags"],
        json!({ "team": "a" })
    );
    let plan = gw(
        &mut m,
        "POST",
        "/usageplans",
        json!({ "name": "p", "apiStages": [{ "apiId": id, "stage": "v1" }] }),
    );
    let key = gw(
        &mut m,
        "POST",
        "/apikeys",
        json!({ "name": "k", "enabled": true }),
    );
    gw(
        &mut m,
        "POST",
        &format!("/usageplans/{}/keys", plan["id"].as_str().unwrap()),
        json!({ "keyId": key["id"], "keyType": "API_KEY" }),
    );
    let keys = gw(
        &mut m,
        "GET",
        &format!("/usageplans/{}/keys", plan["id"].as_str().unwrap()),
        Value::Null,
    );
    assert_eq!(keys["item"][0]["id"], key["id"]);
}
