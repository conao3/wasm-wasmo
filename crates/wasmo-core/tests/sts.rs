use wasmo_core::Emulator;
use wasmo_core::env::FixedEnv;
use wasmo_core::wire::{Request, RequestMeta};

fn emulator() -> Emulator<FixedEnv> {
    Emulator::new(FixedEnv {
        now_ms: 0.0,
        seed: 1,
    })
}

fn sts_request(body: &str) -> Request {
    Request::from_meta(
        RequestMeta {
            method: "POST".into(),
            url: "http://wasmo.local/".into(),
            headers: vec![
                (
                    "Authorization".into(),
                    "AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/sts/aws4_request, SignedHeaders=host, Signature=x"
                        .into(),
                ),
                ("Content-Type".into(), "application/x-www-form-urlencoded; charset=utf-8".into()),
            ],
        },
        body.as_bytes().to_vec(),
    )
}

#[test]
fn get_caller_identity_returns_default_account() {
    let res = emulator().handle(&sts_request("Action=GetCallerIdentity&Version=2011-06-15"));
    assert_eq!(res.status, 200);
    let body = String::from_utf8(res.body).unwrap();
    assert!(body.contains("<Account>000000000000</Account>"), "{body}");
    assert!(res.headers.iter().any(|(k, _)| k == "x-amzn-requestid"));
}

#[test]
fn unknown_action_is_invalid_action() {
    let res = emulator().handle(&sts_request("Action=Nope&Version=2011-06-15"));
    assert_eq!(res.status, 400);
    assert!(
        String::from_utf8(res.body)
            .unwrap()
            .contains("<Code>InvalidAction</Code>")
    );
}
