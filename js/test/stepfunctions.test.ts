import assert from "node:assert/strict";
import { test } from "node:test";
import { CreateFunctionCommand, LambdaClient } from "@aws-sdk/client-lambda";
import {
  CreateStateMachineCommand,
  DescribeExecutionCommand,
  SFNClient,
  StartExecutionCommand,
  StartSyncExecutionCommand,
} from "@aws-sdk/client-sfn";
import { mockAws } from "../dist/index.js";

const role = "arn:aws:iam::000000000000:role/sfn";

async function createFunction(lambda: LambdaClient, name: string): Promise<void> {
  await lambda.send(
    new CreateFunctionCommand({
      FunctionName: name,
      Runtime: "nodejs20.x",
      Role: "arn:aws:iam::000000000000:role/lambda",
      Handler: "index.handler",
      Code: { ZipFile: new TextEncoder().encode("PK fake zip") },
    }),
  );
}

test("a state machine retries a failing handler and catches its error", async () => {
  const aws = mockAws();
  const lambda = new LambdaClient(aws.clientConfig);
  const sfn = new SFNClient(aws.clientConfig);
  await createFunction(lambda, "flaky");
  await createFunction(lambda, "broken");
  let calls = 0;
  aws.registerFunction("flaky", async (event: { n: number }) => {
    calls++;
    if (calls < 2) throw new RangeError("not yet");
    return { doubled: event.n * 2 };
  });
  aws.registerFunction("broken", async () => {
    throw new TypeError("always");
  });
  const { stateMachineArn } = await sfn.send(
    new CreateStateMachineCommand({
      name: "retry-catch",
      roleArn: role,
      definition: JSON.stringify({
        StartAt: "Flaky",
        States: {
          Flaky: {
            Type: "Task",
            Resource: "arn:aws:states:::lambda:invoke",
            Parameters: { FunctionName: "flaky", "Payload.$": "$" },
            ResultSelector: { "doubled.$": "$.Payload.doubled" },
            ResultPath: "$.result",
            Retry: [{ ErrorEquals: ["RangeError"], IntervalSeconds: 1, MaxAttempts: 2 }],
            Next: "Broken",
          },
          Broken: {
            Type: "Task",
            Resource: "arn:aws:states:::lambda:invoke",
            Parameters: { FunctionName: "broken", Payload: {} },
            Catch: [{ ErrorEquals: ["TypeError"], ResultPath: "$.caught", Next: "Done" }],
            Next: "Done",
          },
          Done: { Type: "Succeed" },
        },
      }),
    }),
  );
  const { executionArn } = await sfn.send(
    new StartExecutionCommand({ stateMachineArn, input: '{"n":21}' }),
  );
  const d = await sfn.send(new DescribeExecutionCommand({ executionArn }));
  assert.equal(d.status, "SUCCEEDED");
  const output = JSON.parse(d.output!);
  assert.deepEqual(output.result, { doubled: 42 });
  assert.equal(output.caught.Error, "TypeError");
  assert.equal(calls, 2);
});

test("StartSyncExecution waits for the async handler", async () => {
  const aws = mockAws();
  const lambda = new LambdaClient(aws.clientConfig);
  const sfn = new SFNClient(aws.clientConfig);
  await createFunction(lambda, "greet");
  aws.registerFunction("greet", async (event: { name: string }) => {
    await new Promise((resolve) => setTimeout(resolve, 1));
    return `hello ${event.name}`;
  });
  const { stateMachineArn } = await sfn.send(
    new CreateStateMachineCommand({
      name: "sync",
      roleArn: role,
      type: "EXPRESS",
      definition: JSON.stringify({
        StartAt: "Greet",
        States: {
          Greet: {
            Type: "Task",
            Resource: "arn:aws:states:::lambda:invoke",
            Parameters: { FunctionName: "greet", "Payload.$": "$" },
            OutputPath: "$.Payload",
            End: true,
          },
        },
      }),
    }),
  );
  const out = await sfn.send(
    new StartSyncExecutionCommand({ stateMachineArn, input: '{"name":"ann"}' }),
  );
  assert.equal(out.status, "SUCCEEDED");
  assert.equal(JSON.parse(out.output!), "hello ann");
});
