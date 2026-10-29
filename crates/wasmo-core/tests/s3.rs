use wasmo_core::Emulator;
use wasmo_core::env::FixedEnv;
use wasmo_core::wire::{Request, RequestMeta, Response};

const AUTH: &str = "AWS4-HMAC-SHA256 Credential=test/20260101/us-east-1/s3/aws4_request, SignedHeaders=host, Signature=x";

fn emulator() -> Emulator<FixedEnv> {
    Emulator::new(FixedEnv {
        now_ms: 1_700_000_000_000.0,
        seed: 7,
    })
}

fn send(
    emu: &mut Emulator<FixedEnv>,
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Response {
    let host = url
        .split("://")
        .nth(1)
        .and_then(|r| r.split('/').next())
        .unwrap_or_default();
    let mut all = vec![
        ("host".to_string(), host.to_string()),
        ("authorization".to_string(), AUTH.to_string()),
    ];
    all.extend(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    emu.handle(&Request::from_meta(
        RequestMeta {
            method: method.into(),
            url: url.into(),
            headers: all,
        },
        body.to_vec(),
    ))
}

fn header<'a>(res: &'a Response, name: &str) -> Option<&'a str> {
    res.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn text(res: &Response) -> String {
    String::from_utf8_lossy(&res.body).into_owned()
}

#[test]
fn aws_chunked_put_with_trailing_checksum() {
    let mut emu = emulator();
    assert_eq!(
        send(&mut emu, "PUT", "http://wasmo.local/chunks", &[], b"").status,
        200
    );
    let chunked = [
        ("content-encoding", "aws-chunked"),
        ("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER"),
        ("x-amz-decoded-content-length", "11"),
        ("x-amz-trailer", "x-amz-checksum-crc32"),
        ("x-amz-sdk-checksum-algorithm", "CRC32"),
    ];
    let body = b"6\r\nhello \r\n5\r\nworld\r\n0\r\nx-amz-checksum-crc32:DUoRhQ==\r\n\r\n";
    let put = send(
        &mut emu,
        "PUT",
        "http://chunks.wasmo.local/greeting.txt",
        &chunked,
        body,
    );
    assert_eq!(put.status, 200, "{}", text(&put));
    assert_eq!(header(&put, "x-amz-checksum-crc32"), Some("DUoRhQ=="));

    let get = send(
        &mut emu,
        "GET",
        "http://wasmo.local/chunks/greeting.txt",
        &[("x-amz-checksum-mode", "ENABLED")],
        b"",
    );
    assert_eq!(get.status, 200);
    assert_eq!(get.body, b"hello world");
    assert_eq!(header(&get, "x-amz-checksum-crc32"), Some("DUoRhQ=="));
    assert_eq!(header(&get, "content-encoding"), None);
    assert_eq!(header(&get, "content-length"), Some("11"));

    let bad = b"b\r\nhello world\r\n0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n";
    let rejected = send(
        &mut emu,
        "PUT",
        "http://wasmo.local/chunks/bad.txt",
        &chunked,
        bad,
    );
    assert_eq!(rejected.status, 400);
    assert!(text(&rejected).contains("<Code>BadDigest</Code>"));

    let truncated = send(
        &mut emu,
        "PUT",
        "http://wasmo.local/chunks/t.txt",
        &chunked,
        b"ff\r\nshort",
    );
    assert_eq!(truncated.status, 400);
    assert!(text(&truncated).contains("<Code>IncompleteBody</Code>"));
}

#[test]
fn errors_use_the_unwrapped_s3_document() {
    let mut emu = emulator();
    let res = send(&mut emu, "GET", "http://wasmo.local/missing/key", &[], b"");
    assert_eq!(res.status, 404);
    let body = text(&res);
    assert!(
        body.starts_with("<Error><Code>NoSuchBucket</Code>"),
        "{body}"
    );
    assert!(body.contains("<BucketName>missing</BucketName>"), "{body}");
    assert!(body.contains("<RequestId>"), "{body}");
}

#[test]
fn head_and_not_modified_have_no_body() {
    let mut emu = emulator();
    send(&mut emu, "PUT", "http://wasmo.local/heads", &[], b"");
    let put = send(
        &mut emu,
        "PUT",
        "http://wasmo.local/heads/k",
        &[],
        b"abcdef",
    );
    let etag = header(&put, "etag").unwrap().to_string();
    let head = send(&mut emu, "HEAD", "http://wasmo.local/heads/k", &[], b"");
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    let lengths: Vec<_> = head
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .collect();
    assert_eq!(lengths.len(), 1);
    assert_eq!(lengths[0].1, "6");
    let missing = send(&mut emu, "HEAD", "http://wasmo.local/heads/none", &[], b"");
    assert_eq!(missing.status, 404);
    assert!(missing.body.is_empty());
    let cached = send(
        &mut emu,
        "GET",
        "http://wasmo.local/heads/k",
        &[("if-none-match", &etag)],
        b"",
    );
    assert_eq!(cached.status, 304);
    assert!(cached.body.is_empty());
    let partial = send(
        &mut emu,
        "GET",
        "http://wasmo.local/heads/k",
        &[("range", "bytes=1-2")],
        b"",
    );
    assert_eq!(partial.status, 206);
    assert_eq!(partial.body, b"bc");
    assert_eq!(header(&partial, "content-range"), Some("bytes 1-2/6"));
}

#[test]
fn bucket_location_is_a_bare_document() {
    let mut emu = emulator();
    send(&mut emu, "PUT", "http://wasmo.local/loc", &[], b"");
    let res = send(
        &mut emu,
        "GET",
        "http://loc.wasmo.local/?location",
        &[],
        b"",
    );
    assert_eq!(res.status, 200);
    assert_eq!(
        text(&res),
        "<LocationConstraint xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"></LocationConstraint>"
    );
}
