import hashlib
import io
import os

import boto3
import pytest
from boto3.s3.transfer import TransferConfig
from botocore.config import Config
from botocore.response import StreamingBody

MIB = 1024 * 1024


def header_text(value):
    return value.decode() if isinstance(value, bytes) else value


def record_requests(client):
    seen = []
    client.meta.events.register_first("before-send", lambda request, **_: seen.append(request))
    return seen


@pytest.mark.parametrize("style", ["virtual", "path"])
def test_addressing_styles(aws, style):
    s3 = boto3.client("s3", config=Config(s3={"addressing_style": style}))
    seen = record_requests(s3)
    s3.create_bucket(Bucket="assets")
    s3.put_object(Bucket="assets", Key="dir/a b.txt", Body=b"hello")
    keys = [o["Key"] for o in s3.list_objects_v2(Bucket="assets")["Contents"]]
    assert keys == ["dir/a b.txt"]
    expected = "http://assets.wasmo.local/" if style == "virtual" else "http://wasmo.local/assets"
    assert seen[-1].url.startswith(expected)


def test_binary_and_streaming_body(aws):
    s3 = boto3.client("s3")
    s3.create_bucket(Bucket="bin")
    data = bytes(range(256)) * 1024
    s3.put_object(Bucket="bin", Key="blob", Body=data, ContentType="application/octet-stream")
    out = s3.get_object(Bucket="bin", Key="blob")
    assert isinstance(out["Body"], StreamingBody)
    assert out["ContentLength"] == len(data)
    assert b"".join(out["Body"].iter_chunks(4096)) == data

    ranged = s3.get_object(Bucket="bin", Key="blob", Range="bytes=256-511")
    assert ranged["ResponseMetadata"]["HTTPStatusCode"] == 206
    assert ranged["Body"].read() == bytes(range(256))


def test_aws_chunked_trailer_checksum(aws):
    s3 = boto3.client("s3", endpoint_url="https://wasmo.local")
    seen = record_requests(s3)
    s3.create_bucket(Bucket="chunked")
    body = os.urandom(70_000)
    s3.put_object(Bucket="chunked", Key="k", Body=io.BytesIO(body), ChecksumAlgorithm="SHA256")
    headers = {k.lower(): header_text(v) for k, v in seen[-1].headers.items()}
    assert headers["content-encoding"] == "aws-chunked"
    assert headers["x-amz-trailer"] == "x-amz-checksum-sha256"
    out = s3.get_object(Bucket="chunked", Key="k", ChecksumMode="ENABLED")
    assert out["Body"].read() == body
    assert out["ChecksumSHA256"]


def test_transfer_multipart(aws, tmp_path):
    s3 = boto3.client("s3")
    s3.create_bucket(Bucket="big")
    data = os.urandom(12 * MIB + 123)
    config = TransferConfig(multipart_threshold=5 * MIB, multipart_chunksize=5 * MIB)
    s3.upload_fileobj(io.BytesIO(data), "big", "file.bin", Config=config)

    head = s3.head_object(Bucket="big", Key="file.bin")
    assert head["ContentLength"] == len(data)
    assert head["ETag"].endswith('-3"')

    out = io.BytesIO()
    s3.download_fileobj("big", "file.bin", out, Config=config)
    assert hashlib.sha256(out.getvalue()).digest() == hashlib.sha256(data).digest()

    path = tmp_path / "copy.bin"
    boto3.resource("s3").Bucket("big").download_file("file.bin", str(path), Config=config)
    assert path.read_bytes() == data


def test_presigned_get(aws):
    s3 = boto3.client("s3", config=Config(signature_version="s3v4"))
    s3.create_bucket(Bucket="share")
    s3.put_object(Bucket="share", Key="docs/hello world.txt", Body=b"presigned body")
    url = s3.generate_presigned_url(
        "get_object", Params={"Bucket": "share", "Key": "docs/hello world.txt"}, ExpiresIn=300
    )
    assert "X-Amz-Signature=" in url
    res = aws.fetch(url)
    assert res.status == 200
    assert res.body == b"presigned body"

    missing = aws.fetch(url.replace("hello%20world", "nothing"))
    assert missing.status == 404
    assert b"<Code>NoSuchKey</Code>" in missing.body


def test_errors(aws):
    s3 = boto3.client("s3")
    with pytest.raises(s3.exceptions.NoSuchBucket):
        s3.list_objects_v2(Bucket="nowhere")
    s3.create_bucket(Bucket="present")
    with pytest.raises(s3.exceptions.NoSuchKey):
        s3.get_object(Bucket="present", Key="absent")
    with pytest.raises(s3.exceptions.ClientError) as err:
        s3.head_object(Bucket="present", Key="absent")
    assert err.value.response["Error"]["Code"] == "404"
