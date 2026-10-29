import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CreateStreamCommand,
  GetRecordsCommand,
  GetShardIteratorCommand,
  KinesisClient,
  ListShardsCommand,
  PutRecordsCommand,
  waitUntilStreamExists,
} from "@aws-sdk/client-kinesis";
import { mockAws } from "../dist/index.js";

test("kinesis put records and get records", async () => {
  const aws = mockAws();
  const kinesis = new KinesisClient(aws.clientConfig);
  const StreamName = "clicks";
  await kinesis.send(new CreateStreamCommand({ StreamName, ShardCount: 2 }));
  await waitUntilStreamExists({ client: kinesis, maxWaitTime: 5, minDelay: 1 }, { StreamName });
  const encoder = new TextEncoder();
  const put = await kinesis.send(
    new PutRecordsCommand({
      StreamName,
      Records: Array.from({ length: 10 }, (_, i) => ({
        PartitionKey: `user-${i}`,
        Data: encoder.encode(`click-${i}`),
      })),
    }),
  );
  assert.equal(put.FailedRecordCount, 0);
  assert.equal(put.Records?.length, 10);

  const got: Record<string, string> = {};
  const { Shards } = await kinesis.send(new ListShardsCommand({ StreamName }));
  for (const shard of Shards!) {
    const { ShardIterator } = await kinesis.send(
      new GetShardIteratorCommand({
        StreamName,
        ShardId: shard.ShardId,
        ShardIteratorType: "TRIM_HORIZON",
      }),
    );
    const out = await kinesis.send(new GetRecordsCommand({ ShardIterator }));
    assert.ok(out.NextShardIterator);
    for (const r of out.Records!) {
      assert.ok(r.ApproximateArrivalTimestamp instanceof Date);
      got[r.PartitionKey!] = new TextDecoder().decode(r.Data);
    }
  }
  assert.deepEqual(
    got,
    Object.fromEntries(Array.from({ length: 10 }, (_, i) => [`user-${i}`, `click-${i}`])),
  );
  aws.close();
});
