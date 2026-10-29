import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CreateQueueCommand,
  DeleteMessageCommand,
  GetQueueUrlCommand,
  QueueDoesNotExist,
  ReceiveMessageCommand,
  SendMessageBatchCommand,
  SendMessageCommand,
  SQSClient,
} from "@aws-sdk/client-sqs";
import { mockAws } from "../dist/index.js";
import { md5 } from "./helpers.ts";

test("sqs send and receive with md5", async () => {
  const aws = mockAws();
  const sqs = new SQSClient(aws.clientConfig);
  const { QueueUrl } = await sqs.send(new CreateQueueCommand({ QueueName: "jobs" }));
  const body = "hello, wasmo \u{1F980}";
  const sent = await sqs.send(
    new SendMessageCommand({
      QueueUrl,
      MessageBody: body,
      MessageAttributes: { kind: { DataType: "String", StringValue: "greeting" } },
    }),
  );
  assert.equal(sent.MD5OfMessageBody, md5(body));
  await sqs.send(
    new SendMessageBatchCommand({
      QueueUrl,
      Entries: [
        { Id: "a", MessageBody: "a" },
        { Id: "b", MessageBody: "b" },
      ],
    }),
  );
  const received = await sqs.send(
    new ReceiveMessageCommand({
      QueueUrl,
      MaxNumberOfMessages: 10,
      MessageAttributeNames: ["All"],
    }),
  );
  const messages = received.Messages ?? [];
  assert.deepEqual(messages.map((m) => m.Body).sort(), ["a", "b", body].sort());
  for (const m of messages) assert.equal(m.MD5OfBody, md5(m.Body!));
  const first = messages.find((m) => m.Body === body)!;
  assert.equal(first.MessageAttributes?.kind?.StringValue, "greeting");
  await sqs.send(new DeleteMessageCommand({ QueueUrl, ReceiptHandle: first.ReceiptHandle }));
  aws.close();
});

test("sqs missing queue is QueueDoesNotExist", async () => {
  const aws = mockAws();
  const sqs = new SQSClient(aws.clientConfig);
  await assert.rejects(
    sqs.send(new GetQueueUrlCommand({ QueueName: "missing" })),
    QueueDoesNotExist,
  );
  aws.close();
});
