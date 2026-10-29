import { test } from "node:test";
import {
  CreateTableCommand,
  DynamoDBClient,
  GetItemCommand,
  ListTablesCommand,
  PutItemCommand,
} from "@aws-sdk/client-dynamodb";
import {
  CreateBucketCommand,
  GetObjectCommand,
  PutObjectCommand,
  S3Client,
} from "@aws-sdk/client-s3";
import { mockAws } from "../dist/index.js";
import { median, micros, pattern } from "./helpers.ts";

const N = 300;

test("latency", async (t) => {
  const aws = mockAws();
  const ddb = new DynamoDBClient(aws.clientConfig);
  await ddb.send(
    new CreateTableCommand({
      TableName: "bench",
      AttributeDefinitions: [{ AttributeName: "pk", AttributeType: "S" }],
      KeySchema: [{ AttributeName: "pk", KeyType: "HASH" }],
      BillingMode: "PAY_PER_REQUEST",
    }),
  );
  let i = 0;
  const put = await median(N, () =>
    ddb.send(
      new PutItemCommand({
        TableName: "bench",
        Item: { pk: { S: `k${i++ % 100}` }, n: { N: String(i) }, s: { S: "value" } },
      }),
    ),
  );
  const get = await median(N, () =>
    ddb.send(new GetItemCommand({ TableName: "bench", Key: { pk: { S: `k${i++ % 100}` } } })),
  );

  const s3 = new S3Client(aws.clientConfig);
  await s3.send(new CreateBucketCommand({ Bucket: "bench" }));
  const kib = pattern(1024);
  const s3put = await median(N, () =>
    s3.send(new PutObjectCommand({ Bucket: "bench", Key: `k${i++ % 100}`, Body: kib })),
  );
  const s3get = await median(N, async () => {
    const out = await s3.send(new GetObjectCommand({ Bucket: "bench", Key: `k${i++ % 100}` }));
    await out.Body!.transformToByteArray();
  });
  const list = await median(N, () => ddb.send(new ListTablesCommand({})));
  aws.close();

  const create = await median(50, async () => {
    const m = mockAws();
    await new DynamoDBClient(m.clientConfig).send(new ListTablesCommand({}));
    m.close();
  });

  t.diagnostic(`dynamodb PutItem median ${micros(put)}`);
  t.diagnostic(`dynamodb GetItem median ${micros(get)}`);
  t.diagnostic(`s3 PutObject 1KiB median ${micros(s3put)}`);
  t.diagnostic(`s3 GetObject 1KiB median ${micros(s3get)}`);
  t.diagnostic(`dynamodb ListTables median ${micros(list)}`);
  t.diagnostic(`mockAws() + first ListTables median ${micros(create)}`);
});
