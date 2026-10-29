import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CreateTopicCommand,
  PublishCommand,
  SNSClient,
  SubscribeCommand,
} from "@aws-sdk/client-sns";
import { CreateQueueCommand, ReceiveMessageCommand, SQSClient } from "@aws-sdk/client-sqs";
import { mockAws } from "../dist/index.js";
import { queueArn } from "./helpers.ts";

test("sns delivers to sqs through a filter policy", async () => {
  const aws = mockAws();
  const sns = new SNSClient(aws.clientConfig);
  const sqs = new SQSClient(aws.clientConfig);
  const { TopicArn } = await sns.send(new CreateTopicCommand({ Name: "orders" }));
  const { QueueUrl: all } = await sqs.send(new CreateQueueCommand({ QueueName: "all" }));
  const { QueueUrl: paid } = await sqs.send(new CreateQueueCommand({ QueueName: "paid" }));
  await sns.send(
    new SubscribeCommand({ TopicArn, Protocol: "sqs", Endpoint: await queueArn(sqs, all!) }),
  );
  await sns.send(
    new SubscribeCommand({
      TopicArn,
      Protocol: "sqs",
      Endpoint: await queueArn(sqs, paid!),
      Attributes: { RawMessageDelivery: "true", FilterPolicy: JSON.stringify({ kind: ["paid"] }) },
    }),
  );
  for (const kind of ["paid", "refunded"]) {
    await sns.send(
      new PublishCommand({
        TopicArn,
        Message: kind,
        Subject: "order",
        MessageAttributes: { kind: { DataType: "String", StringValue: kind } },
      }),
    );
  }

  const envelopes = (
    await sqs.send(new ReceiveMessageCommand({ QueueUrl: all, MaxNumberOfMessages: 10 }))
  ).Messages!.map((m) => JSON.parse(m.Body!));
  assert.deepEqual(envelopes.map((e) => e.Message).sort(), ["paid", "refunded"]);
  for (const e of envelopes) {
    assert.equal(e.Type, "Notification");
    assert.equal(e.TopicArn, TopicArn);
    assert.equal(e.Subject, "order");
  }
  const raw = await sqs.send(
    new ReceiveMessageCommand({
      QueueUrl: paid,
      MaxNumberOfMessages: 10,
      MessageAttributeNames: ["All"],
    }),
  );
  assert.deepEqual(
    raw.Messages!.map((m) => [m.Body, m.MessageAttributes?.kind?.StringValue]),
    [["paid", "paid"]],
  );
  aws.close();
});
