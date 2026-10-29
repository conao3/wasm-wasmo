import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CreateDeliveryStreamCommand,
  FirehoseClient,
  PutRecordBatchCommand,
  PutRecordCommand,
} from "@aws-sdk/client-firehose";
import {
  CreateBucketCommand,
  GetObjectCommand,
  ListObjectsV2Command,
  S3Client,
} from "@aws-sdk/client-s3";
import { mockAws } from "../dist/index.js";

test("firehose delivers put batches to s3", async () => {
  const aws = mockAws();
  const firehose = new FirehoseClient(aws.clientConfig);
  const s3 = new S3Client(aws.clientConfig);
  await s3.send(new CreateBucketCommand({ Bucket: "lake" }));
  const DeliveryStreamName = "events";
  await firehose.send(
    new CreateDeliveryStreamCommand({
      DeliveryStreamName,
      ExtendedS3DestinationConfiguration: {
        BucketARN: "arn:aws:s3:::lake",
        RoleARN: "arn:aws:iam::000000000000:role/firehose",
        Prefix: "raw/",
      },
    }),
  );
  const encoder = new TextEncoder();
  await firehose.send(
    new PutRecordCommand({ DeliveryStreamName, Record: { Data: encoder.encode("one\n") } }),
  );
  const batch = await firehose.send(
    new PutRecordBatchCommand({
      DeliveryStreamName,
      Records: [{ Data: encoder.encode("two\n") }, { Data: encoder.encode("three\n") }],
    }),
  );
  assert.equal(batch.FailedPutCount, 0);

  const listed = await s3.send(new ListObjectsV2Command({ Bucket: "lake", Prefix: "raw/" }));
  const keys = listed.Contents!.map((o) => o.Key!);
  assert.equal(keys.length, 2);
  assert.ok(keys.every((k) => k.split("/").pop()!.startsWith("events-1-")));
  const bodies = await Promise.all(
    keys.map(async (Key) =>
      (await s3.send(new GetObjectCommand({ Bucket: "lake", Key }))).Body!.transformToString(),
    ),
  );
  assert.deepEqual(bodies.sort(), ["one\n", "two\nthree\n"]);
  aws.close();
});
