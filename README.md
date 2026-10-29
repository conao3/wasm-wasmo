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

> **Status:** early development. The Go adapter (AWS SDK for Go v2), the JavaScript adapter (AWS SDK for JavaScript v3), and the JVM adapter (AWS SDK for Java v2) run STS, SQS, DynamoDB, S3, SNS, EventBridge, Secrets Manager, SSM Parameter Store, and KMS. None is published yet.

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

Services deliver to each other inside a mock: SNS to SQS, EventBridge to SQS and SNS, and S3 notifications to SQS, SNS, and EventBridge happen synchronously during the triggering call.

The protocol codecs (awsQuery, ec2, awsJson 1.0/1.1, rest-json, rest-xml) are checked against botocore's protocol test suite; every case either passes or is listed in `crates/wasmo-core/tests/conformance-gaps.txt` as client-only behaviour.

Service behaviour is checked against the parity fixtures of [bun-bunsai](https://github.com/conao3/bun-bunsai), whose expectations come from real AWS (LocalStack snapshot recordings, moto `@aws_verified` tests, and the AWS API reference). Each fixture replays AWS SDK commands as wire requests through the emulator; every fixture of an emulated service either passes or is listed in `crates/wasmo-core/tests/parity-gaps.txt` with the reason a mock cannot reproduce it.

## How it works

Every AWS SDK lets you replace its HTTP client. wasmo uses the AWS wire protocol itself as the language boundary:

```
your language's AWS SDK
  └─ HTTP client adapter (a few hundred lines per language)
       └─ WebAssembly runtime (Go: wazero, JVM: Chicory, JS: built-in, Python/.NET/Ruby: wasmtime)
            └─ wasmo.wasm (Rust, wasm32-unknown-unknown)
```

The module needs no WASI. It imports two host functions (`wasmo.now_ms`, `wasmo.random_fill`) and exports three (`wasmo_alloc`, `wasmo_free`, `wasmo_handle`). A request is a JSON meta (method, URL, headers) plus raw body bytes; the response frame is a big-endian `u32` meta length, the meta JSON, then the body.

## JavaScript / TypeScript

`@conao3/wasmo` plugs into the AWS SDK for JavaScript v3 as a `requestHandler`. It runs on Node.js 20+ and Bun, ships ESM with type declarations, and has no runtime dependencies. The WebAssembly module is compiled once per process; each `mockAws()` gets its own instance.

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

## Layout

| Path | Contents |
| --- | --- |
| `crates/wasmo-core` | Router, protocol codecs, services, state. Plain Rust, tested natively with `cargo test` |
| `crates/wasmo-wasm` | The WebAssembly ABI around `wasmo-core` |
| `go/` | Go adapter (`github.com/conao3/wasm-wasmo/go`) built on wazero |
| `js/` | JavaScript / TypeScript adapter (`@conao3/wasmo`) for the AWS SDK for JavaScript v3 |
| `jvm/` | Java/Kotlin adapter (`io.github.conao3:wasmo`) built on Chicory, compiled to JVM bytecode at build time |
| `vendor/` | Vendored upstream test data (see `vendor/PROVENANCE.md`) |

## Development

Tools come from the Nix dev shell.

```sh
nix develop
make test   # cargo test + build wasm + go test + node --test + bun test + mvn verify
make lint   # rustfmt, clippy, gofmt, go vet, tsc, prettier, google-java-format, javac -Xlint
```

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
