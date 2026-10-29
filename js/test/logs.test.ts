import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CloudWatchLogsClient,
  CreateLogGroupCommand,
  CreateLogStreamCommand,
  FilterLogEventsCommand,
  PutLogEventsCommand,
  ResourceNotFoundException,
} from "@aws-sdk/client-cloudwatch-logs";
import { mockAws } from "../dist/index.js";

test("cloudwatch logs put log events and filter log events", async () => {
  const aws = mockAws();
  const logs = new CloudWatchLogsClient(aws.clientConfig);
  const logGroupName = "/app/api";
  await logs.send(new CreateLogGroupCommand({ logGroupName }));
  for (const logStreamName of ["web-1", "web-2"]) {
    await logs.send(new CreateLogStreamCommand({ logGroupName, logStreamName }));
  }
  const now = Date.now();
  await logs.send(
    new PutLogEventsCommand({
      logGroupName,
      logStreamName: "web-1",
      logEvents: [
        { timestamp: now, message: "GET /health 200" },
        { timestamp: now + 1, message: "GET /orders 500" },
      ],
    }),
  );
  await logs.send(
    new PutLogEventsCommand({
      logGroupName,
      logStreamName: "web-2",
      logEvents: [{ timestamp: now + 2, message: "POST /orders 500" }],
    }),
  );

  const errors = await logs.send(
    new FilterLogEventsCommand({ logGroupName, filterPattern: '"500"' }),
  );
  assert.deepEqual(
    errors.events!.map((e) => [e.logStreamName, e.message]),
    [
      ["web-1", "GET /orders 500"],
      ["web-2", "POST /orders 500"],
    ],
  );
  const one = await logs.send(
    new FilterLogEventsCommand({ logGroupName, logStreamNames: ["web-1"] }),
  );
  assert.deepEqual(
    one.events!.map((e) => e.message),
    ["GET /health 200", "GET /orders 500"],
  );
  await assert.rejects(
    logs.send(
      new PutLogEventsCommand({
        logGroupName,
        logStreamName: "missing",
        logEvents: [{ timestamp: now, message: "x" }],
      }),
    ),
    ResourceNotFoundException,
  );
  aws.close();
});
