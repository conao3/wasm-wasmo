import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CreateEventSourceMappingCommand,
  CreateFunctionCommand,
  InvokeCommand,
  LambdaClient,
} from "@aws-sdk/client-lambda";
import {
  CreateQueueCommand,
  GetQueueAttributesCommand,
  ReceiveMessageCommand,
  SendMessageCommand,
  SQSClient,
} from "@aws-sdk/client-sqs";
import { mockAws } from "../dist/index.js";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

async function createFunction(lambda: LambdaClient, name: string): Promise<void> {
  await lambda.send(
    new CreateFunctionCommand({
      FunctionName: name,
      Runtime: "nodejs20.x",
      Role: "arn:aws:iam::000000000000:role/lambda",
      Handler: "index.handler",
      Code: { ZipFile: encoder.encode("PK fake zip") },
      Environment: { Variables: { TABLE: "orders" } },
    }),
  );
}

test("invoke runs an async handler that calls the same mock", async () => {
  const aws = mockAws();
  const lambda = new LambdaClient(aws.clientConfig);
  const sqs = new SQSClient(aws.clientConfig);
  const { QueueUrl } = await sqs.send(new CreateQueueCommand({ QueueName: "out" }));
  await createFunction(lambda, "forward");
  aws.registerFunction("forward", async (event: { id: number }, context) => {
    context.log("forwarding", event.id);
    await sqs.send(new SendMessageCommand({ QueueUrl, MessageBody: JSON.stringify(event) }));
    return { table: context.environment.TABLE, version: context.functionVersion };
  });

  const out = await lambda.send(
    new InvokeCommand({
      FunctionName: "forward",
      Payload: encoder.encode('{"id":7}'),
      LogType: "Tail",
    }),
  );
  assert.equal(out.StatusCode, 200);
  assert.equal(out.FunctionError, undefined);
  assert.deepEqual(JSON.parse(decoder.decode(out.Payload)), {
    table: "orders",
    version: "$LATEST",
  });
  assert.match(Buffer.from(out.LogResult!, "base64").toString(), /forwarding 7\nEND RequestId/);
  const received = await sqs.send(new ReceiveMessageCommand({ QueueUrl }));
  assert.equal(received.Messages?.[0]?.Body, '{"id":7}');
});

test("a thrown error is a function error", async () => {
  const aws = mockAws();
  const lambda = new LambdaClient(aws.clientConfig);
  await createFunction(lambda, "boom");
  aws.registerFunction("boom", async () => {
    throw new TypeError("bad input");
  });
  const out = await lambda.send(new InvokeCommand({ FunctionName: "boom" }));
  assert.equal(out.FunctionError, "Unhandled");
  const body = JSON.parse(decoder.decode(out.Payload));
  assert.equal(body.errorType, "TypeError");
  assert.equal(body.errorMessage, "bad input");

  await createFunction(lambda, "unbound");
  const unbound = await lambda.send(new InvokeCommand({ FunctionName: "unbound" }));
  assert.equal(JSON.parse(decoder.decode(unbound.Payload)).errorType, "Runtime.HandlerNotFound");
});

test("an sqs event source mapping feeds an async handler", async () => {
  const aws = mockAws();
  const lambda = new LambdaClient(aws.clientConfig);
  const sqs = new SQSClient(aws.clientConfig);
  const { QueueUrl } = await sqs.send(new CreateQueueCommand({ QueueName: "jobs" }));
  await createFunction(lambda, "worker");
  const bodies: string[] = [];
  aws.registerFunction("worker", async (event: { Records: { body: string }[] }) => {
    await new Promise((resolve) => setTimeout(resolve, 1));
    for (const r of event.Records) bodies.push(r.body);
  });
  await lambda.send(
    new CreateEventSourceMappingCommand({
      FunctionName: "worker",
      EventSourceArn: "arn:aws:sqs:us-east-1:000000000000:jobs",
    }),
  );
  await sqs.send(new SendMessageCommand({ QueueUrl, MessageBody: "one" }));
  await sqs.send(new SendMessageCommand({ QueueUrl, MessageBody: "two" }));
  assert.deepEqual(bodies, ["one", "two"]);
  const attrs = await sqs.send(
    new GetQueueAttributesCommand({ QueueUrl, AttributeNames: ["All"] }),
  );
  assert.equal(attrs.Attributes?.ApproximateNumberOfMessages, "0");
  assert.equal(attrs.Attributes?.ApproximateNumberOfMessagesNotVisible, "0");
});
