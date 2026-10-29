import assert from "node:assert/strict";
import { test } from "node:test";
import {
  EventBridgeClient,
  PutEventsCommand,
  PutRuleCommand,
  PutTargetsCommand,
} from "@aws-sdk/client-eventbridge";
import { CreateQueueCommand, ReceiveMessageCommand, SQSClient } from "@aws-sdk/client-sqs";
import { mockAws } from "../dist/index.js";
import { queueArn } from "./helpers.ts";

test("eventbridge put events reaches an sqs target", async () => {
  const aws = mockAws();
  const events = new EventBridgeClient(aws.clientConfig);
  const sqs = new SQSClient(aws.clientConfig);
  const { QueueUrl } = await sqs.send(new CreateQueueCommand({ QueueName: "orders" }));
  await events.send(
    new PutRuleCommand({
      Name: "big-orders",
      EventPattern: JSON.stringify({
        source: ["shop"],
        detail: { total: [{ numeric: [">=", 100] }] },
      }),
    }),
  );
  await events.send(
    new PutTargetsCommand({
      Rule: "big-orders",
      Targets: [{ Id: "queue", Arn: await queueArn(sqs, QueueUrl!) }],
    }),
  );
  const put = await events.send(
    new PutEventsCommand({
      Entries: [150, 20].map((total) => ({
        Source: "shop",
        DetailType: "Order Placed",
        Detail: JSON.stringify({ total }),
      })),
    }),
  );
  assert.equal(put.FailedEntryCount, 0);

  const { Messages } = await sqs.send(
    new ReceiveMessageCommand({ QueueUrl, MaxNumberOfMessages: 10 }),
  );
  assert.equal(Messages?.length, 1);
  const event = JSON.parse(Messages[0]!.Body!);
  assert.equal(event.id, put.Entries![0]!.EventId);
  assert.equal(event.source, "shop");
  assert.equal(event["detail-type"], "Order Placed");
  assert.deepEqual(event.detail, { total: 150 });
  aws.close();
});
