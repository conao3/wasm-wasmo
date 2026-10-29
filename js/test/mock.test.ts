import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CreateBucketCommand,
  GetObjectCommand,
  ListBucketsCommand,
  PutObjectCommand,
  S3Client,
} from "@aws-sdk/client-s3";
import { GetCallerIdentityCommand, STSClient } from "@aws-sdk/client-sts";
import { mockAws } from "../dist/index.js";

test("parallel mocks keep separate state", async () => {
  const results = await Promise.all(
    Array.from({ length: 8 }, async (_, i) => {
      const aws = mockAws();
      const s3 = new S3Client(aws.clientConfig);
      await s3.send(new CreateBucketCommand({ Bucket: `bucket-${i}` }));
      await s3.send(new PutObjectCommand({ Bucket: `bucket-${i}`, Key: "k", Body: `v${i}` }));
      const got = await s3.send(new GetObjectCommand({ Bucket: `bucket-${i}`, Key: "k" }));
      const buckets = await s3.send(new ListBucketsCommand({}));
      aws.close();
      return [await got.Body!.transformToString(), buckets.Buckets?.map((b) => b.Name)];
    }),
  );
  results.forEach(([body, names], i) => {
    assert.equal(body, `v${i}`);
    assert.deepEqual(names, [`bucket-${i}`]);
  });
});

test("closed mock rejects requests", async () => {
  const aws = mockAws();
  aws.close();
  await assert.rejects(
    new STSClient(aws.clientConfig).send(new GetCallerIdentityCommand({})),
    /mock is closed/,
  );
});

test("aborted request rejects", async () => {
  const aws = mockAws();
  const controller = new AbortController();
  controller.abort();
  await assert.rejects(
    new STSClient(aws.clientConfig).send(new GetCallerIdentityCommand({}), {
      abortSignal: controller.signal,
    }),
  );
  aws.close();
});
