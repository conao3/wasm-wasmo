mod common;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::Mock;
use serde_json::{Value, json};
use wasmo_core::wire::Response;

fn v2(m: &mut Mock, method: &str, path: &str, body: Value) -> Value {
    let (status, out) = m.rest("apigateway", method, &format!("/v2{path}"), body);
    assert!(status < 300, "{method} {path}: {status} {out}");
    out
}

fn body(res: &Response) -> Value {
    serde_json::from_slice(&res.body).unwrap_or_else(|_| json!(String::from_utf8_lossy(&res.body)))
}

fn header<'a>(res: &'a Response, name: &str) -> Option<&'a str> {
    res.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn jwt(claims: Value) -> String {
    format!(
        "{}.{}.sig",
        URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}"),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    )
}

#[test]
fn quick_create_routes_everything_to_the_function() {
    let mut m = Mock::new();
    let f = m.function("hello");
    m.register("hello", |e| {
        Ok(json!({
            "routeKey": e["routeKey"],
            "rawPath": e["rawPath"],
            "query": e["queryStringParameters"],
            "method": e["requestContext"]["http"]["method"],
            "stage": e["requestContext"]["stage"],
            "cookies": e["cookies"],
            "body": e["body"],
        }))
    });
    let api = v2(
        &mut m,
        "POST",
        "/apis",
        json!({ "name": "quick", "protocolType": "HTTP", "target": f }),
    );
    let id = api["apiId"].as_str().unwrap();
    assert_eq!(
        api["apiEndpoint"],
        format!("https://{id}.execute-api.us-east-1.amazonaws.com")
    );
    let res = m.raw(
        "POST",
        &format!("https://{id}.execute-api.us-east-1.amazonaws.com/any/where?x=1&x=2"),
        vec![("cookie", "a=1; b=2")],
        b"hi",
    );
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    assert_eq!(
        body(&res),
        json!({
            "routeKey": "$default",
            "rawPath": "/any/where",
            "query": { "x": "1,2" },
            "method": "POST",
            "stage": "$default",
            "cookies": ["a=1", "b=2"],
            "body": "hi",
        })
    );
    assert!(header(&res, "apigw-requestid").is_some());
}

#[test]
fn routes_authorizers_and_service_integrations() {
    let mut m = Mock::new();
    let pets = m.function("pets");
    let auth = m.function("auth");
    let double = m.function("double");
    m.register("pets", |e| {
        Ok(json!({
            "statusCode": 201,
            "headers": { "x-route": e["routeKey"].as_str().unwrap_or(e["resource"].as_str().unwrap_or_default()) },
            "body": json!({
                "version": e["version"],
                "params": e["pathParameters"],
                "vars": e["stageVariables"],
                "authorizer": e["requestContext"]["authorizer"],
                "trace": e["headers"]["x-trace"],
            }).to_string(),
        }))
    });
    m.register("auth", |e| {
        Ok(json!({ "isAuthorized": e["headers"]["x-pass"] == "yes", "context": { "user": "u1" } }))
    });
    m.register("double", |e| {
        Ok(json!({ "n": e["n"].as_i64().unwrap() * 2 }))
    });
    let (queue_url, _) = m.queue("inbox");
    let machine = m.sfn(
        "CreateStateMachine",
        json!({
            "name": "doubler",
            "type": "EXPRESS",
            "roleArn": "arn:aws:iam::000000000000:role/r",
            "definition": json!({ "StartAt": "D", "States": { "D": { "Type": "Task", "Resource": double, "End": true } } }).to_string(),
        }),
    )["stateMachineArn"]
        .as_str()
        .unwrap()
        .to_string();

    let api = v2(
        &mut m,
        "POST",
        "/apis",
        json!({
            "name": "shop",
            "protocolType": "HTTP",
            "corsConfiguration": { "allowOrigins": ["https://app.example"], "allowMethods": ["GET", "POST"], "allowHeaders": ["content-type"] },
        }),
    );
    let id = api["apiId"].as_str().unwrap().to_string();
    let integration = |m: &mut Mock, body: Value| -> String {
        v2(m, "POST", &format!("/apis/{id}/integrations"), body)["integrationId"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let lambda_v2 = integration(
        &mut m,
        json!({ "integrationType": "AWS_PROXY", "integrationUri": pets, "payloadFormatVersion": "2.0", "requestParameters": { "overwrite:header.x-trace": "$request.querystring.t" } }),
    );
    let lambda_v1 = integration(
        &mut m,
        json!({ "integrationType": "AWS_PROXY", "integrationUri": pets, "payloadFormatVersion": "1.0" }),
    );
    let sqs = integration(
        &mut m,
        json!({
            "integrationType": "AWS_PROXY",
            "integrationSubtype": "SQS-SendMessage",
            "payloadFormatVersion": "1.0",
            "credentialsArn": "arn:aws:iam::000000000000:role/r",
            "requestParameters": { "QueueUrl": queue_url, "MessageBody": "$request.body.message" },
            "responseParameters": { "200": { "overwrite:statuscode": "202" } },
        }),
    );
    let sfn = integration(
        &mut m,
        json!({
            "integrationType": "AWS_PROXY",
            "integrationSubtype": "StepFunctions-StartSyncExecution",
            "payloadFormatVersion": "1.0",
            "credentialsArn": "arn:aws:iam::000000000000:role/r",
            "requestParameters": { "StateMachineArn": machine, "Input": "$request.body" },
        }),
    );
    let jwt_auth = v2(&mut m, "POST", &format!("/apis/{id}/authorizers"), json!({
        "name": "jwt", "authorizerType": "JWT", "identitySource": ["$request.header.Authorization"],
        "jwtConfiguration": { "issuer": "https://issuer.example", "audience": ["app"] },
    }))["authorizerId"].as_str().unwrap().to_string();
    let lambda_auth = v2(&mut m, "POST", &format!("/apis/{id}/authorizers"), json!({
        "name": "lam", "authorizerType": "REQUEST", "identitySource": ["$request.header.x-pass"],
        "authorizerUri": format!("arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/{auth}/invocations"),
        "authorizerPayloadFormatVersion": "2.0", "enableSimpleResponses": true,
    }))["authorizerId"].as_str().unwrap().to_string();
    for (key, target, extra) in [
        ("GET /pets/{id}", &lambda_v2, json!({})),
        ("ANY /files/{proxy+}", &lambda_v1, json!({})),
        ("POST /orders", &sqs, json!({})),
        ("POST /double", &sfn, json!({})),
        (
            "GET /me",
            &lambda_v2,
            json!({ "authorizationType": "JWT", "authorizerId": jwt_auth, "authorizationScopes": ["read"] }),
        ),
        (
            "GET /vip",
            &lambda_v2,
            json!({ "authorizationType": "CUSTOM", "authorizerId": lambda_auth }),
        ),
    ] {
        let mut b = json!({ "routeKey": key, "target": format!("integrations/{target}") });
        b.as_object_mut()
            .unwrap()
            .extend(extra.as_object().cloned().unwrap());
        v2(&mut m, "POST", &format!("/apis/{id}/routes"), b);
    }
    v2(
        &mut m,
        "POST",
        &format!("/apis/{id}/stages"),
        json!({ "stageName": "prod", "autoDeploy": true, "stageVariables": { "env": "p" } }),
    );
    let host = format!("https://{id}.execute-api.us-east-1.amazonaws.com");

    let res = m.raw(
        "GET",
        &format!("{host}/prod/pets/7?t=abc"),
        vec![("origin", "https://app.example")],
        b"",
    );
    assert_eq!(res.status, 201, "{}", String::from_utf8_lossy(&res.body));
    assert_eq!(header(&res, "x-route"), Some("GET /pets/{id}"));
    assert_eq!(
        header(&res, "access-control-allow-origin"),
        Some("https://app.example")
    );
    let b = body(&res);
    assert_eq!(
        (b["version"].as_str(), b["trace"].as_str()),
        (Some("2.0"), Some("abc"))
    );
    assert_eq!(b["params"], json!({ "id": "7" }));
    assert_eq!(b["vars"], json!({ "env": "p" }));

    let res = m.raw("DELETE", &format!("{host}/prod/files/a/b"), vec![], b"");
    let b = body(&res);
    assert_eq!(
        (b["version"].as_str(), b["params"]["proxy"].as_str()),
        (Some("1.0"), Some("a/b"))
    );

    let res = m.raw(
        "OPTIONS",
        &format!("{host}/prod/pets/7"),
        vec![
            ("origin", "https://app.example"),
            ("access-control-request-method", "GET"),
        ],
        b"",
    );
    assert_eq!(res.status, 204);
    assert_eq!(
        header(&res, "access-control-allow-methods"),
        Some("GET,POST")
    );

    let res = m.raw(
        "POST",
        &format!("{host}/prod/orders"),
        vec![("content-type", "application/json")],
        br#"{"message":"buy"}"#,
    );
    assert_eq!(res.status, 202, "{}", String::from_utf8_lossy(&res.body));
    assert!(body(&res)["MessageId"].is_string());
    assert_eq!(m.receive(&queue_url)[0]["Body"], "buy");

    let res = m.raw(
        "POST",
        &format!("{host}/prod/double"),
        vec![],
        br#"{"n":21}"#,
    );
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    let out = body(&res);
    assert_eq!(out["status"], "SUCCEEDED");
    assert_eq!(out["output"], "{\"n\":42}");

    assert_eq!(
        m.raw("GET", &format!("{host}/prod/me"), vec![], b"").status,
        401
    );
    let token = jwt(
        json!({ "iss": "https://issuer.example", "aud": "app", "sub": "s1", "scope": "read write" }),
    );
    let res = m.raw(
        "GET",
        &format!("{host}/prod/me"),
        vec![("authorization", &format!("Bearer {token}"))],
        b"",
    );
    assert_eq!(res.status, 201, "{}", String::from_utf8_lossy(&res.body));
    assert_eq!(body(&res)["authorizer"]["jwt"]["claims"]["sub"], "s1");
    let wrong = jwt(json!({ "iss": "https://issuer.example", "aud": "other" }));
    assert_eq!(
        m.raw(
            "GET",
            &format!("{host}/prod/me"),
            vec![("authorization", &wrong)],
            b""
        )
        .status,
        401
    );
    let narrow = jwt(json!({ "iss": "https://issuer.example", "aud": "app", "scope": "write" }));
    assert_eq!(
        m.raw(
            "GET",
            &format!("{host}/prod/me"),
            vec![("authorization", &narrow)],
            b""
        )
        .status,
        403
    );

    assert_eq!(
        m.raw("GET", &format!("{host}/prod/vip"), vec![], b"")
            .status,
        401
    );
    assert_eq!(
        m.raw(
            "GET",
            &format!("{host}/prod/vip"),
            vec![("x-pass", "no")],
            b""
        )
        .status,
        403
    );
    let res = m.raw(
        "GET",
        &format!("{host}/prod/vip"),
        vec![("x-pass", "yes")],
        b"",
    );
    assert_eq!(
        body(&res)["authorizer"],
        json!({ "lambda": { "user": "u1" } })
    );

    let res = m.raw("GET", &format!("{host}/prod/nothing"), vec![], b"");
    assert_eq!(
        (res.status, body(&res)),
        (404, json!({ "message": "Not Found" }))
    );
    assert_eq!(
        m.raw("GET", &format!("{host}/pets/1"), vec![], b"").status,
        404
    );

    let routes = v2(&mut m, "GET", &format!("/apis/{id}/routes"), Value::Null);
    assert_eq!(routes["items"].as_array().unwrap().len(), 6);
    let arn = format!("arn:aws:apigateway:us-east-1::/apis/{id}");
    v2(
        &mut m,
        "POST",
        &format!("/tags/{}", arn.replace('/', "%2F")),
        json!({ "tags": { "a": "b" } }),
    );
    assert_eq!(
        v2(&mut m, "GET", &format!("/apis/{id}"), Value::Null)["tags"],
        json!({ "a": "b" })
    );
}

#[test]
fn deployments_pin_stages_that_do_not_auto_deploy() {
    let mut m = Mock::new();
    let f = m.function("v");
    let calls = std::rc::Rc::new(std::cell::Cell::new(0));
    let seen = calls.clone();
    m.register("v", move |_| {
        seen.set(seen.get() + 1);
        Ok(json!("ok"))
    });
    let api = v2(
        &mut m,
        "POST",
        "/apis",
        json!({ "name": "pinned", "protocolType": "HTTP" }),
    );
    let id = api["apiId"].as_str().unwrap().to_string();
    let i = v2(
        &mut m,
        "POST",
        &format!("/apis/{id}/integrations"),
        json!({ "integrationType": "AWS_PROXY", "integrationUri": f, "payloadFormatVersion": "2.0" }),
    );
    v2(
        &mut m,
        "POST",
        &format!("/apis/{id}/routes"),
        json!({ "routeKey": "GET /a", "target": format!("integrations/{}", i["integrationId"].as_str().unwrap()) }),
    );
    v2(
        &mut m,
        "POST",
        &format!("/apis/{id}/stages"),
        json!({ "stageName": "live" }),
    );
    v2(
        &mut m,
        "POST",
        &format!("/apis/{id}/deployments"),
        json!({ "stageName": "live" }),
    );
    v2(
        &mut m,
        "POST",
        &format!("/apis/{id}/routes"),
        json!({ "routeKey": "GET /b", "target": format!("integrations/{}", i["integrationId"].as_str().unwrap()) }),
    );
    let host = format!("http://{id}.execute-api.wasmo.local");
    let res = m.raw("GET", &format!("{host}/live/a"), vec![], b"");
    assert_eq!((res.status, body(&res)), (200, json!("ok")));
    assert_eq!(
        m.raw("GET", &format!("{host}/live/b"), vec![], b"").status,
        404
    );
    assert_eq!(calls.get(), 1);
    let ws = v2(
        &mut m,
        "POST",
        "/apis",
        json!({ "name": "chat", "protocolType": "WEBSOCKET", "routeSelectionExpression": "$request.body.action" }),
    );
    assert!(ws["apiEndpoint"].as_str().unwrap().starts_with("wss://"));
    v2(
        &mut m,
        "POST",
        &format!("/apis/{}/routes", ws["apiId"].as_str().unwrap()),
        json!({ "routeKey": "$connect" }),
    );
}
