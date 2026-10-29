# wasmo

A stateful AWS mock that runs inside your test process. Write a value to DynamoDB and the next `get_item` returns it; put an object in S3 and `list_objects_v2` shows it. No Docker, no ports, no sidecar process: the AWS emulator is a single WebAssembly module that wasmo hooks into botocore's HTTP layer, so request signing, retries, checksums, and response parsing are boto3's own.

Emulated services: STS, SQS, DynamoDB, S3, SNS, EventBridge, Secrets Manager, SSM Parameter Store, KMS. The same module also backs wasmo's Go, JavaScript, and JVM adapters, so every language sees the same behaviour. Source and service coverage: <https://github.com/conao3/wasm-wasmo>.

Requires Python 3.10+. The WebAssembly module ships inside the package and runs on [wasmtime](https://pypi.org/project/wasmtime/). It is compiled once per process; each mock gets its own instance.

```python
import boto3
import wasmo

with wasmo.mock_aws() as aws:
    s3 = boto3.client("s3")
    s3.create_bucket(Bucket="assets")
    s3.put_object(Bucket="assets", Key="hello.txt", Body=b"hi")
    assert s3.get_object(Bucket="assets", Key="hello.txt")["Body"].read() == b"hi"

    aws.reset()  # drop all state
```

Inside `mock_aws()`, every botocore client and boto3 resource created from any session talks to that mock. A client stays bound to the mock it was created in. Unless you pass them explicitly, wasmo sets the endpoint to `http://wasmo.local` and the credentials to `test` / `test`, and the environment gets `AWS_DEFAULT_REGION=us-east-1` and the same dummy credentials for the duration of the block. Leaving the block closes the mock; its clients then raise.

As a decorator, each call runs in a fresh mock:

```python
@wasmo.mock_aws
def test_enqueue():
    sqs = boto3.client("sqs")
    url = sqs.create_queue(QueueName="jobs")["QueueUrl"]
    sqs.send_message(QueueUrl=url, MessageBody="hi")
    assert sqs.receive_message(QueueUrl=url)["Messages"][0]["Body"] == "hi"
```

A pytest fixture:

```python
import pytest
import wasmo


@pytest.fixture
def aws():
    with wasmo.mock_aws() as aws:
        yield aws


def test_rejects_a_duplicate(aws):
    table = boto3.resource("dynamodb").Table("users")  # create the table first
    put = dict(Item={"id": "1"}, ConditionExpression="attribute_not_exists(id)")
    table.put_item(**put)
    with pytest.raises(table.meta.client.exceptions.ConditionalCheckFailedException):
        table.put_item(**put)
```

Without patching botocore, `wasmo.Mock()` hands out bound clients directly:

```python
aws = wasmo.Mock()
ddb = aws.resource("dynamodb")
sqs = aws.client("sqs", region_name="eu-west-1")
session = aws.session()  # a boto3.Session bound to this mock
aws.close()
```

`aws.fetch(url, method="GET", headers=None, body=b"")` sends a raw HTTP request to the mock and returns a `Response(status, headers, body)`, for example to fetch a presigned URL.

S3 works with virtual-hosted (`<bucket>.wasmo.local`, the default) and path-style addressing, `GetObject` returns a botocore `StreamingBody`, and `boto3.s3.transfer` multipart uploads and ranged downloads work as against AWS. Clients created with an `https://` endpoint send `aws-chunked` bodies with trailing checksums, which wasmo decodes.

A `Mock` is safe to use from several threads; calls into one instance are serialized.
