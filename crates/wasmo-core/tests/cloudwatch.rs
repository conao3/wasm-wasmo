//! CloudWatch lists RPC v2 CBOR, awsJson 1.0, and awsQuery; each request is
//! answered in the protocol it was sent with.

use wasmo_core::Emulator;
use wasmo_core::env::FixedEnv;
use wasmo_core::wire::{Request, RequestMeta, Response};

const AUTH: &str = "AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/monitoring/aws4_request, SignedHeaders=host, Signature=x";

fn emulator() -> Emulator<FixedEnv> {
    Emulator::new(FixedEnv {
        now_ms: 1_800_000_000_000.0,
        seed: 1,
    })
}

fn request(path: &str, headers: &[(&str, &str)], body: &[u8]) -> Request {
    let mut h: Vec<(String, String)> = vec![("Authorization".into(), AUTH.into())];
    h.extend(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    Request::from_meta(
        RequestMeta {
            method: "POST".into(),
            url: format!("http://wasmo.local{path}"),
            headers: h,
        },
        body.to_vec(),
    )
}

fn query(body: &str) -> Response {
    emulator_with_data().handle(&request(
        "/",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        body.as_bytes(),
    ))
}

fn emulator_with_data() -> Emulator<FixedEnv> {
    let mut emu = emulator();
    let res = emu.handle(&request(
        "/",
        &[("Content-Type", "application/x-www-form-urlencoded")],
        b"Action=PutMetricData&Version=2010-08-01&Namespace=App&MetricData.member.1.MetricName=Hits&MetricData.member.1.Value=3&MetricData.member.1.Timestamp=2027-01-15T08:00:10Z&MetricData.member.2.MetricName=Hits&MetricData.member.2.Value=5&MetricData.member.2.Timestamp=2027-01-15T08:00:20Z",
    ));
    assert_eq!(res.status, 200, "{}", String::from_utf8_lossy(&res.body));
    emu
}

fn header<'a>(res: &'a Response, name: &str) -> Option<&'a str> {
    res.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[test]
fn query_requests_get_xml() {
    let res = query(
        "Action=GetMetricStatistics&Version=2010-08-01&Namespace=App&MetricName=Hits&StartTime=2027-01-15T08:00:00Z&EndTime=2027-01-15T09:00:00Z&Period=60&Statistics.member.1=Sum",
    );
    let body = String::from_utf8(res.body).unwrap();
    assert_eq!(res.status, 200, "{body}");
    assert!(body.contains("<GetMetricStatisticsResult>"), "{body}");
    assert!(body.contains("<Sum>8.0</Sum>"), "{body}");
}

#[test]
fn query_errors_use_the_query_code() {
    let res =
        query("Action=SetAlarmState&Version=2010-08-01&AlarmName=nope&StateValue=OK&StateReason=x");
    assert_eq!(res.status, 404);
    assert!(
        String::from_utf8(res.body)
            .unwrap()
            .contains("<Code>ResourceNotFound</Code>")
    );
}

#[test]
fn json_requests_get_json() {
    let mut emu = emulator_with_data();
    let res = emu.handle(&request(
        "/",
        &[
            ("Content-Type", "application/x-amz-json-1.0"),
            ("X-Amz-Target", "GraniteServiceVersion20100801.ListMetrics"),
        ],
        br#"{"Namespace":"App"}"#,
    ));
    assert_eq!(
        header(&res, "content-type"),
        Some("application/x-amz-json-1.0")
    );
    let body: serde_json::Value = serde_json::from_slice(&res.body).unwrap();
    assert_eq!(body["Metrics"][0]["MetricName"], "Hits");
}

#[test]
fn cbor_requests_get_cbor() {
    let mut emu = emulator_with_data();
    let res = emu.handle(&request(
        "/service/GraniteServiceVersion20100801/operation/ListMetrics",
        &[
            ("Content-Type", "application/cbor"),
            ("smithy-protocol", "rpc-v2-cbor"),
        ],
        &[
            0xa1, 0x69, b'N', b'a', b'm', b'e', b's', b'p', b'a', b'c', b'e', 0x63, b'A', b'p',
            b'p',
        ],
    ));
    assert_eq!(res.status, 200);
    assert_eq!(header(&res, "smithy-protocol"), Some("rpc-v2-cbor"));
    let doc = wasmo_core::codec::cbor::decode(&res.body).unwrap();
    assert!(format!("{doc:?}").contains("Hits"), "{doc:?}");
}
