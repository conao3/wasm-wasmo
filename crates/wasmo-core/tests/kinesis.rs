//! Kinesis is awsJson 1.1, and the AWS SDK for Java sends it a CBOR body
//! (`application/x-amz-cbor-1.1`) whose timestamps are epoch milliseconds.

use wasmo_core::Emulator;
use wasmo_core::codec::cbor::{Cbor, decode, encode};
use wasmo_core::env::FixedEnv;
use wasmo_core::wire::{Request, RequestMeta, Response};

const AUTH: &str = "AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/kinesis/aws4_request, SignedHeaders=host, Signature=x";
const NOW_MS: u64 = 1_800_000_000_000;

fn text(s: &str) -> Cbor {
    Cbor::Text(s.to_string())
}

fn call(emu: &mut Emulator<FixedEnv>, op: &str, body: Vec<(&str, Cbor)>) -> Response {
    let mut bytes = Vec::new();
    encode(
        &Cbor::Map(body.into_iter().map(|(k, v)| (text(k), v)).collect()),
        &mut bytes,
    );
    emu.handle(&Request::from_meta(
        RequestMeta {
            method: "POST".into(),
            url: "http://wasmo.local/".into(),
            headers: vec![
                ("Authorization".into(), AUTH.into()),
                ("Content-Type".into(), "application/x-amz-cbor-1.1".into()),
                ("X-Amz-Target".into(), format!("Kinesis_20131202.{op}")),
            ],
        },
        bytes,
    ))
}

fn field<'a>(c: &'a Cbor, name: &str) -> &'a Cbor {
    let Cbor::Map(entries) = c else {
        panic!("not a map: {c:?}")
    };
    entries
        .iter()
        .find(|(k, _)| *k == text(name))
        .map(|(_, v)| v)
        .unwrap_or_else(|| panic!("no {name} in {c:?}"))
}

fn ok(res: Response) -> Cbor {
    assert_eq!(res.status, 200, "{:?}", decode(&res.body));
    assert!(
        res.headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/x-amz-cbor-1.1"),
        "{:?}",
        res.headers
    );
    decode(&res.body).unwrap()
}

#[test]
fn cbor_records_carry_bytes_and_millisecond_timestamps() {
    let mut emu = Emulator::new(FixedEnv {
        now_ms: NOW_MS as f64,
        seed: 1,
    });
    ok(call(
        &mut emu,
        "CreateStream",
        vec![
            ("StreamName", text("clicks")),
            ("ShardCount", Cbor::Unsigned(1)),
        ],
    ));
    let put = ok(call(
        &mut emu,
        "PutRecord",
        vec![
            ("StreamName", text("clicks")),
            ("PartitionKey", text("k")),
            ("Data", Cbor::Bytes(vec![0, 1, 0xff])),
        ],
    ));
    let shard = field(&put, "ShardId").clone();
    let it = ok(call(
        &mut emu,
        "GetShardIterator",
        vec![
            ("StreamName", text("clicks")),
            ("ShardId", shard),
            ("ShardIteratorType", text("AT_TIMESTAMP")),
            (
                "Timestamp",
                Cbor::Tag(1, Box::new(Cbor::Unsigned(NOW_MS - 1000))),
            ),
        ],
    ));
    let out = ok(call(
        &mut emu,
        "GetRecords",
        vec![("ShardIterator", field(&it, "ShardIterator").clone())],
    ));
    let Cbor::Array(records) = field(&out, "Records") else {
        panic!("{out:?}")
    };
    assert_eq!(records.len(), 1);
    assert_eq!(field(&records[0], "Data"), &Cbor::Bytes(vec![0, 1, 0xff]));
    assert_eq!(
        field(&records[0], "ApproximateArrivalTimestamp"),
        &Cbor::Tag(1, Box::new(Cbor::Unsigned(NOW_MS)))
    );

    let missing = call(
        &mut emu,
        "DescribeStream",
        vec![("StreamName", text("missing"))],
    );
    assert_eq!(missing.status, 400);
    assert_eq!(
        field(&decode(&missing.body).unwrap(), "__type"),
        &text("ResourceNotFoundException")
    );
}
