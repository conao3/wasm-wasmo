# wasmo

A stateful AWS mock that runs **inside your test process**, in every language, from a single WebAssembly module.

No Docker, no ports, no sidecar process. Write a value to DynamoDB and the next `GetItem` returns it; put an object in S3 and `ListObjects` shows it. Every `Mock` owns an isolated WebAssembly instance, so parallel tests never share state.

```go
m, _ := wasmo.New(ctx)
defer m.Close(ctx)

client := dynamodb.NewFromConfig(m.Config())
client.PutItem(ctx, &dynamodb.PutItemInput{TableName: aws.String("users"), Item: item})
out, _ := client.GetItem(ctx, &dynamodb.GetItemInput{TableName: aws.String("users"), Key: key})
// out.Item is the item written above
```

```java
try (var mock = Wasmo.create()) {
    var ddb = DynamoDbClient.builder()
            .httpClient(mock)
            .endpointOverride(mock.endpoint())
            .region(mock.region())
            .credentialsProvider(mock.credentialsProvider())
            .build();
    ddb.putItem(r -> r.tableName("users").item(item));
    var out = ddb.getItem(r -> r.tableName("users").key(key));
}
```

```csharp
using var mock = WasmoMock.Create();
using var ddb = new AmazonDynamoDBClient(mock.Configure(new AmazonDynamoDBConfig()));
await ddb.PutItemAsync("users", item);
var out = await ddb.GetItemAsync("users", key);
```

> **Status:** early development. The adapters for Go (AWS SDK for Go v2), JavaScript (AWS SDK for JavaScript v3), the JVM (AWS SDK for Java v2), .NET (AWS SDK for .NET v4), Python (boto3), and Ruby (AWS SDK for Ruby v3) are each tested through their SDK against all 16 emulated services: STS, SQS, DynamoDB, S3, SNS, EventBridge, Secrets Manager, SSM Parameter Store, KMS, IAM, CloudWatch, CloudWatch Logs, Kinesis, Data Firehose, DynamoDB Streams, and Lambda, whose function handlers are registered in the test's own language. Go covers each service in depth; the other five run at least one end-to-end scenario per service (cross-service delivery for SNS, EventBridge, and Firehose). None is published yet.

| Service | Coverage |
| --- | --- |
| STS | Caller identity, assume-role family, session and federation tokens |
| SQS | All 23 operations: standard and FIFO queues, batches, visibility, dead-letter redrive, move tasks |
| DynamoDB | Tables and indexes, items with condition/update/projection expressions, Query/Scan, batches, transactions, PartiQL, TTL, backups |
| S3 | Buckets, objects, listing, multipart, versioning, checksums, tagging, ACLs, Object Lock, bucket configuration, presigned GET, virtual-hosted and path-style addressing, event notifications |
| SNS | Topics, subscriptions with confirmation, Publish/PublishBatch, filter policies, raw delivery, FIFO topics, dead-letter redrive, platform endpoints, SMS settings |
| EventBridge | Buses, rules and targets, event patterns, input transformers, PutEvents, archives and replays, permissions, connections, API destinations |
| Secrets Manager | Secrets, versions and staging labels, delete and restore, rotation settings, random passwords, resource policies, BatchGetSecretValue |
| SSM | Parameter Store: String/StringList/SecureString, versions, labels, hierarchies, GetParametersByPath, history, tags |
| KMS | Keys and aliases, AES-GCM Encrypt/Decrypt/ReEncrypt, data keys, ECDSA and RSA Sign/Verify, HMAC, grants, key policies, rotation |
| IAM | Users, groups, roles with trust policies, managed policies with versions and inline policies, attachments, permissions boundaries, access keys, login profiles and password policy, instance profiles, OIDC and SAML providers, MFA devices, SSH keys and certificates, credential report, account summary and aliases, SimulatePrincipalPolicy and SimulateCustomPolicy |
| CloudWatch | PutMetricData, GetMetricStatistics and GetMetricData with period aggregation, percentiles, and metric math, ListMetrics, metric alarms evaluated per period, composite alarms, alarm history, dashboards, anomaly detectors, insight rules, metric streams; answers RPC v2 CBOR, awsJson, and awsQuery |
| CloudWatch Logs | Log groups and streams, PutLogEvents, GetLogEvents and FilterLogEvents with term, regex, JSON, and space-delimited patterns, retention, metric filters, subscription filters, Insights queries (fields, filter, stats, sort, limit), export tasks, policies, deliveries |
| Kinesis | Streams and shards, MD5 partition-key hashing, every shard iterator type, GetRecords, SplitShard, MergeShards, UpdateShardCount, enhanced fan-out consumers, tags, encryption, retention; answers awsJson 1.1 and the `application/x-amz-cbor-1.1` body the AWS SDK for Java v2 sends |
| Data Firehose | Delivery streams, PutRecord and PutRecordBatch written to the S3 destination (one object per batch, GZIP when configured), destination updates, tags, encryption |
| DynamoDB Streams | INSERT, MODIFY, and REMOVE records for tables with a StreamSpecification in every view type, DescribeStream, ListStreams, shard iterators, GetRecords |
| Lambda | Functions bound by name to handlers registered in the test's language, versions and aliases with weighted routing, Invoke (RequestResponse, Event, DryRun, `X-Amz-Function-Error`, LogType Tail, timeouts), InvokeAsync, retries and OnSuccess/OnFailure destinations and dead-letter queues for async invocations, reserved concurrency (0 throttles), permissions, event source mappings for SQS (batch size, filters, partial batch responses), Kinesis, and DynamoDB Streams (retries, OnFailure destinations), layers, function URLs, code signing configs, tags |

Services deliver to each other inside a mock: SNS to SQS and Lambda, EventBridge to SQS, SNS, and Lambda, S3 notifications to SQS, SNS, EventBridge, and Lambda, CloudWatch Logs subscription filters to Kinesis, Firehose, Logs destinations, and Lambda (gzipped `awslogs` payloads), SQS, Kinesis, and DynamoDB Streams event source mappings to Lambda, Lambda async destinations to SQS, SNS, EventBridge, and Lambda, Logs metric filters to CloudWatch, Firehose and Logs export tasks to S3, and CloudWatch alarm actions to SNS and EventBridge happen synchronously during the triggering call.

The protocol codecs (awsQuery, ec2, awsJson 1.0/1.1, rest-json, rest-xml, Smithy RPC v2 CBOR) are checked against botocore's protocol test suite; every case either passes or is listed in `crates/wasmo-core/tests/conformance-gaps.txt` as client-only behaviour.

Service behaviour is checked against the parity fixtures of [bun-bunsai](https://github.com/conao3/bun-bunsai), whose expectations come from real AWS (LocalStack snapshot recordings, moto `@aws_verified` tests, and the AWS API reference). Each fixture replays AWS SDK commands as wire requests through the emulator; every fixture of an emulated service either passes or is listed in `crates/wasmo-core/tests/parity-gaps.txt` with the reason a mock cannot reproduce it.

## How it works

Every AWS SDK lets you replace its HTTP client. wasmo uses the AWS wire protocol itself as the language boundary:

```
your language's AWS SDK
  └─ HTTP client adapter (a few hundred lines per language)
       └─ WebAssembly runtime (Go: wazero, JVM: Chicory, JS: built-in, Ruby: wasmtime-rb, Python/.NET: wasmtime)
            └─ wasmo.wasm (Rust, wasm32-unknown-unknown)
```

The module needs no WASI. It imports three host functions and exports four:

| | Signature | Contract |
| --- | --- | --- |
| import | `wasmo.now_ms() -> f64` | Wall-clock time in milliseconds |
| import | `wasmo.random_fill(ptr, len)` | Fill the range with random bytes |
| import | `wasmo.invoke(meta_ptr, meta_len, payload_ptr, payload_len)` | A Lambda invocation starts. The host copies both buffers (they stay owned by the module) and queues the invocation; it does not call back into the module here |
| export | `wasmo_alloc(len) -> ptr`, `wasmo_free(ptr, len)` | Buffers passed in either direction |
| export | `wasmo_handle(meta_ptr, meta_len, body_ptr, body_len) -> u64` | Takes ownership of both buffers and returns `(ptr << 32) \| len` of a response frame, which the host frees |
| export | `wasmo_complete(meta_ptr, meta_len, payload_ptr, payload_len) -> u64` | Answers one queued invocation. Takes ownership of both buffers and returns 0, or the response frame of the request that waited for that invocation |

A request is a JSON meta (`{method, url, headers}`) plus raw body bytes. A response frame is a big-endian `u32` meta length, the meta JSON, then the body. The meta is `{status, headers}`, or `{"await": "<invocation id>"}` when the response depends on a synchronous Invoke that is still queued.

The invocation meta is a JSON object with `id`, `functionName`, `functionVersion`, `invokedFunctionArn`, `memoryLimitInMB`, `timeoutSeconds`, `deadlineMs`, `awsRequestId`, `logGroupName`, `logStreamName`, `invocationType`, `environment` (the function's variables plus `AWS_REGION`, `AWS_LAMBDA_FUNCTION_NAME`, and the other variables Lambda sets), `account`, `region`, and `clientContext` when the caller sent one; the payload is the event JSON. The completion meta is `{id, registered, error, log}`: `registered: false` when no handler is bound to the function name (the invocation fails with `Runtime.HandlerNotFound`), `error: {type, message, stackTrace}` when the handler failed (the payload is then ignored and the function error is `{"errorMessage", "errorType", "stackTrace"}`), and `log` for the lines the handler wrote; the completion payload is the handler's JSON response.

After `wasmo_handle` returns, an adapter runs the queued invocations in order, outside its per-mock lock, and passes each result to `wasmo_complete`, which may queue more invocations (destinations, event source batches, retries); it repeats until the queue is empty and the frame no longer awaits. Handlers can therefore send requests to the same mock, and async handlers (JavaScript, .NET) are awaited between the two calls.


## Lambda

`CreateFunction` accepts any code (`ZipFile`, S3, or an image), but wasmo never runs it. Every invocation of a function, whether from `Invoke`, an alias or version, an SNS subscription, an EventBridge target, an S3 notification, a Logs subscription filter, an event source mapping, or an async destination, calls the handler registered under the function's name in the test's own language. The handler gets the event JSON and a context (function name and version, invoked ARN, request id, memory, deadline, log group and stream, and the environment variables of the invoked version) and returns the response JSON; an error becomes a function error (`X-Amz-Function-Error: Unhandled` with `errorType`/`errorMessage`), and an invocation of a function with no registered handler fails with `Runtime.HandlerNotFound`. Lines written with the context's log method appear in `LogType: Tail` results. Handlers run outside the mock's lock, so they can call AWS through the same mock.

```go
m.RegisterFunction("resize", func(ctx context.Context, event json.RawMessage) (json.RawMessage, error) {
	fc, _ := wasmo.FunctionContextFrom(ctx)
	fc.Logf("bucket %s", fc.Environment["BUCKET"])
	return json.RawMessage(`{"ok":true}`), nil
})
```

```ts
aws.registerFunction("resize", async (event, context) => {
  context.log("bucket", context.environment.BUCKET);
  return { ok: true };
});
```

```java
mock.registerFunction("resize", (event, context) -> {
    context.log("bucket " + context.getEnvironment().get("BUCKET"));
    return "{\"ok\":true}";
});
```

```csharp
mock.RegisterFunction("resize", async (eventJson, context) =>
{
    context.Log($"bucket {context.Environment["BUCKET"]}");
    return "{\"ok\":true}";
});
```

```python
def resize(event, context):
    context.log(f"bucket {context.environment['BUCKET']}")
    return {"ok": True}


aws.register_function("resize", resize)
```

```ruby
aws.register_function("resize") do |event, context|
  context.log("bucket #{context.environment["BUCKET"]}")
  { "ok" => true }
end
```

Event invocations retry a failed handler up to `MaximumRetryAttempts` (2 by default) and then deliver the invocation record to the OnFailure destination and the function's dead-letter queue; successes go to the OnSuccess destination. SQS event source mappings consume the queue after every request, in batches of `BatchSize`, delete the messages a batch handled (all of them, or all but the `batchItemFailures` with `ReportBatchItemFailures`), and drop messages that no `FilterCriteria` pattern matches. Kinesis and DynamoDB Streams mappings read every shard from `StartingPosition`, and retry a failed batch on the next request up to `MaximumRetryAttempts` before sending it to the OnFailure destination.

## JavaScript / TypeScript

`@conao3/wasmo` plugs into the AWS SDK for JavaScript v3 as a `requestHandler`. It runs on Node.js 22+ and Bun, ships ESM with type declarations, and has no runtime dependencies. The WebAssembly module is compiled once per process; each `mockAws()` gets its own instance.

```ts
import { mockAws } from "@conao3/wasmo";

const aws = mockAws();
const s3 = new S3Client(aws.clientConfig);
const ddb = DynamoDBDocumentClient.from(new DynamoDBClient(aws.clientConfig));

aws.reset(); // drop all state
aws.close(); // later requests reject
```

`clientConfig` holds `endpoint`, `region` (`us-east-1`), dummy `credentials`, and `requestHandler`. Spread it to add client options, e.g. `new S3Client({ ...aws.clientConfig, forcePathStyle: true })`. S3 `GetObject` returns a `Readable` with `transformToByteArray()` and `transformToString()`, as with the real service.

node:test (also runs under `bun test`):

```ts
import assert from "node:assert/strict";
import { beforeEach, test } from "node:test";
import { CreateQueueCommand, ReceiveMessageCommand, SendMessageCommand, SQSClient } from "@aws-sdk/client-sqs";
import { mockAws } from "@conao3/wasmo";

const aws = mockAws();
const sqs = new SQSClient(aws.clientConfig);
beforeEach(() => aws.reset());

test("enqueue", async () => {
  const { QueueUrl } = await sqs.send(new CreateQueueCommand({ QueueName: "jobs" }));
  await sqs.send(new SendMessageCommand({ QueueUrl, MessageBody: "hi" }));
  const out = await sqs.send(new ReceiveMessageCommand({ QueueUrl }));
  assert.equal(out.Messages?.[0]?.Body, "hi");
});
```

Vitest:

```ts
import { afterAll, beforeEach, expect, test } from "vitest";
import { ConditionalCheckFailedException, DynamoDBClient } from "@aws-sdk/client-dynamodb";
import { DynamoDBDocumentClient, PutCommand } from "@aws-sdk/lib-dynamodb";
import { mockAws } from "@conao3/wasmo";

const aws = mockAws();
const doc = DynamoDBDocumentClient.from(new DynamoDBClient(aws.clientConfig));
beforeEach(() => aws.reset());
afterAll(() => aws.close());

test("rejects a duplicate", async () => {
  // create the table, then:
  const put = () =>
    doc.send(new PutCommand({ TableName: "users", Item: { id: "1" }, ConditionExpression: "attribute_not_exists(id)" }));
  await put();
  await expect(put()).rejects.toBeInstanceOf(ConditionalCheckFailedException);
});
```

Jest: the package is ESM, so run Jest in ESM mode (`NODE_OPTIONS=--experimental-vm-modules npx jest`). The test body is the same as with Vitest, using Jest's globals.

## .NET

`Wasmo` (NuGet, `net8.0` and later) plugs into the AWS SDK for .NET v4 through `ClientConfig.HttpClientFactory`. The WebAssembly module is embedded in the assembly and runs on [wasmtime-dotnet](https://github.com/bytecodealliance/wasmtime-dotnet) (the `Wasmtime` package, which ships the native `libwasmtime` for Linux, macOS, and Windows). The module is compiled once per process; each `WasmoMock` gets its own store and instance, and calls into one instance are serialized.

```csharp
using Amazon.S3;
using Amazon.SQS;
using Wasmo;

using var mock = WasmoMock.Create();
using var sqs = new AmazonSQSClient(mock.Configure(new AmazonSQSConfig()));
using var s3 = new AmazonS3Client(mock.Configure(new AmazonS3Config { ForcePathStyle = true }));
```

`Configure` sets `ServiceURL` (`http://wasmo.local`), `AuthenticationRegion` (`us-east-1`), dummy `DefaultAWSCredentials`, and an `HttpClientFactory` whose handler answers every request from the mock, then returns the config. `mock.Handler` is the underlying `HttpMessageHandler`. S3 works with path-style and virtual-hosted addressing (`<bucket>.wasmo.local`), the SDK's `aws-chunked` bodies with checksum trailers, and `TransferUtility` multipart uploads. Disposing the mock releases the instance; later requests throw `ObjectDisposedException`.

xUnit:

```csharp
public class OrdersTest
{
    [Fact]
    public async Task RejectsADuplicate()
    {
        using var mock = WasmoMock.Create();
        using var ddb = new AmazonDynamoDBClient(mock.Configure(new AmazonDynamoDBConfig()));
        // create the table, then:
        var put = new PutItemRequest { TableName = "orders", Item = item, ConditionExpression = "attribute_not_exists(id)" };
        await ddb.PutItemAsync(put);
        await Assert.ThrowsAsync<ConditionalCheckFailedException>(() => ddb.PutItemAsync(put));
    }
}
```

On NixOS, use the `dotnet-sdk_8` (or newer) package from nixpkgs, as the dev shell does. Its host process already loads `libgcc_s.so.1`, the one library `libwasmtime.so` needs beyond glibc, so the native library from the NuGet package loads without `patchelf`, `LD_LIBRARY_PATH`, or nix-ld.

## Python

`wasmo` hooks into botocore at the HTTP layer (the `before-send` event), so boto3 still signs, retries, validates checksums, and parses responses. It runs on Python 3.10+ with [wasmtime](https://pypi.org/project/wasmtime/); the WebAssembly module ships inside the package, is compiled once per process, and each mock gets its own instance.

```python
import boto3
import wasmo

with wasmo.mock_aws() as aws:
    ddb = boto3.resource("dynamodb")
    s3 = boto3.client("s3")
    # every client and resource created here talks to this mock
    aws.reset()  # drop all state

@wasmo.mock_aws  # each call runs in a fresh mock
def test_enqueue():
    sqs = boto3.client("sqs")
    ...

aws = wasmo.Mock()  # explicit clients, no patching
sqs = aws.client("sqs")
aws.close()
```

Inside `mock_aws()`, clients default to the endpoint `http://wasmo.local`, region `us-east-1`, and dummy credentials. `aws.fetch(url)` sends a raw request such as a presigned URL to the mock. See `python/README.md` for details.

## Ruby

The `wasmo` gem plugs into the AWS SDK for Ruby v3 as a Seahorse plugin whose send handler replaces Net::HTTP, so signing, retries, checksums, and response parsing stay in the SDK. It needs Ruby 3.3+ and runs the module with [wasmtime-rb](https://github.com/bytecodealliance/wasmtime-rb); the WebAssembly module ships inside the gem. The module is loaded once per process, and its compiled machine code is cached in `Gem.cache_home/wasmo` (`~/.cache/wasmo`), so only the first run after an upgrade compiles it. Each `Wasmo::Mock` gets its own instance, and calls into one instance are serialized, so a mock can be shared across threads.

```ruby
require "wasmo"
require "aws-sdk-s3"

aws = Wasmo::Mock.new
s3 = aws.client(Aws::S3::Client) # or Aws::S3::Client.new(**aws.config)
s3.create_bucket(bucket: "assets")
s3.put_object(bucket: "assets", key: "a.txt", body: "hi")
s3.get_object(bucket: "assets", key: "a.txt").body.read # => "hi"

aws.client(Aws::S3::Client, force_path_style: true) # extra client options pass through
aws.reset # drop all state
aws.close # later requests raise Wasmo::ClosedError
```

`aws.config` holds the plugin, `region` (`us-east-1`), `endpoint` (`http://wasmo.local`), and dummy `credentials`. `Wasmo.mock_aws` merges the same options into `Aws.config` for the duration of the block, so clients built inside it with no arguments talk to the mock; `Aws.config` is restored and the mock closed when the block returns. A client that passes its own `plugins:` inside the block skips the global plugin list; build it with `aws.client` instead.

```ruby
Wasmo.mock_aws do |aws|
  sqs = Aws::SQS::Client.new
  url = sqs.create_queue(queue_name: "jobs").queue_url
  sqs.send_message(queue_url: url, message_body: "hi")
end
```

Minitest:

```ruby
class UsersTest < Minitest::Test
  def setup
    @aws = Wasmo::Mock.new
    @ddb = @aws.client(Aws::DynamoDB::Client)
  end

  def teardown
    @aws.close
  end

  def test_rejects_a_duplicate
    # create the table, then:
    put = -> { @ddb.put_item(table_name: "users", item: { "id" => "1" }, condition_expression: "attribute_not_exists(id)") }
    put.call
    assert_raises(Aws::DynamoDB::Errors::ConditionalCheckFailedException) { put.call }
  end
end
```

`aws.request(method, url, headers, body)` sends a raw HTTP request to the mock, e.g. to fetch a presigned URL from `Aws::S3::Presigner`.

wasmtime-rb publishes precompiled native gems for Ruby 3.3+ on x86_64/aarch64 Linux (glibc and musl) and macOS; on other platforms Bundler builds it from source, which needs a Rust toolchain. The precompiled `x86_64-linux` gem also loads on NixOS under the nixpkgs `ruby`: its only dependencies are glibc and `libgcc_s`, which the nixpkgs Ruby has already loaded.

## Layout

| Path | Contents |
| --- | --- |
| `crates/wasmo-core` | Router, protocol codecs, services, state. Plain Rust, tested natively with `cargo test` |
| `crates/wasmo-wasm` | The WebAssembly ABI around `wasmo-core` |
| `go/` | Go adapter (`github.com/conao3/wasm-wasmo/go`) built on wazero |
| `js/` | JavaScript / TypeScript adapter (`@conao3/wasmo`) for the AWS SDK for JavaScript v3 |
| `python/` | Python adapter (`wasmo` on PyPI) for boto3, built on wasmtime |
| `jvm/` | Java/Kotlin adapter (`io.github.conao3:wasmo`) built on Chicory, compiled to JVM bytecode at build time |
| `dotnet/` | .NET adapter (NuGet `Wasmo`) for the AWS SDK for .NET v4, built on wasmtime-dotnet |
| `ruby/` | Ruby adapter (gem `wasmo`) for the AWS SDK for Ruby v3, built on wasmtime-rb |
| `vendor/` | Vendored upstream test data (see `vendor/PROVENANCE.md`) |

## Development

Tools come from the Nix dev shell.

```sh
nix develop
make test   # cargo test + build wasm + go test + node --test + bun test + mvn verify + dotnet test + pytest + rake test
make lint   # rustfmt, clippy, gofmt, go vet, tsc, prettier, google-java-format, javac -Xlint, dotnet format, dotnet build (warnings as errors), ruff, rubocop
```

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
