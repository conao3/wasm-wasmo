# @conao3/wasmo

A stateful AWS mock that runs inside your test process. Write a value to DynamoDB and the next `GetItem` returns it; put an object in S3 and `ListObjectsV2` shows it. No Docker, no ports, no sidecar process: the AWS emulator is a single WebAssembly module that plugs into the AWS SDK for JavaScript v3 as its `requestHandler`.

Emulated services: STS, SQS, DynamoDB, DynamoDB Streams, S3, SNS, EventBridge, Secrets Manager, SSM Parameter Store, KMS, IAM, CloudWatch, CloudWatch Logs, Kinesis, Data Firehose. The same module also backs wasmo's Go and JVM adapters, so every language sees the same behaviour. Source and service coverage: <https://github.com/conao3/wasm-wasmo>.

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
import {
  CreateQueueCommand,
  ReceiveMessageCommand,
  SendMessageCommand,
  SQSClient,
} from "@aws-sdk/client-sqs";
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
    doc.send(
      new PutCommand({
        TableName: "users",
        Item: { id: "1" },
        ConditionExpression: "attribute_not_exists(id)",
      }),
    );
  await put();
  await expect(put()).rejects.toBeInstanceOf(ConditionalCheckFailedException);
});
```

Jest: the package is ESM, so run Jest in ESM mode (`NODE_OPTIONS=--experimental-vm-modules npx jest`). The test body is the same as with Vitest, using Jest's globals.

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
