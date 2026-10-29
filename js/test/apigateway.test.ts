import assert from "node:assert/strict";
import { test } from "node:test";
import {
  APIGatewayClient,
  CreateDeploymentCommand,
  CreateResourceCommand,
  CreateRestApiCommand,
  PutIntegrationCommand,
  PutMethodCommand,
} from "@aws-sdk/client-api-gateway";
import { CreateFunctionCommand, LambdaClient } from "@aws-sdk/client-lambda";
import type { HttpRequest } from "@smithy/types";
import { mockAws, type WasmoMock } from "../dist/index.js";

async function send(
  aws: WasmoMock,
  method: string,
  hostname: string,
  path: string,
  query: Record<string, string> = {},
): Promise<{ status: number; headers: Record<string, string>; body: string }> {
  const request: HttpRequest = {
    protocol: "https:",
    hostname,
    path,
    method,
    query,
    headers: { host: hostname },
  };
  const { response } = await aws.clientConfig.requestHandler.handle(request);
  const chunks: Buffer[] = [];
  for await (const chunk of response.body as AsyncIterable<Buffer>) chunks.push(Buffer.from(chunk));
  return {
    status: response.statusCode,
    headers: response.headers,
    body: Buffer.concat(chunks).toString(),
  };
}

test("a rest api invokes a registered handler through execute-api", async () => {
  const aws = mockAws();
  const lambda = new LambdaClient(aws.clientConfig);
  const gw = new APIGatewayClient(aws.clientConfig);
  const { FunctionArn } = await lambda.send(
    new CreateFunctionCommand({
      FunctionName: "pets",
      Runtime: "nodejs20.x",
      Role: "arn:aws:iam::000000000000:role/lambda",
      Handler: "index.handler",
      Code: { ZipFile: new TextEncoder().encode("PK fake zip") },
    }),
  );
  aws.registerFunction(
    "pets",
    async (event: {
      pathParameters: Record<string, string>;
      queryStringParameters: Record<string, string>;
      requestContext: { stage: string };
    }) => ({
      statusCode: 200,
      headers: { "X-Pet": event.pathParameters.id },
      body: JSON.stringify({
        id: event.pathParameters.id,
        q: event.queryStringParameters.q,
        stage: event.requestContext.stage,
      }),
    }),
  );
  const api = await gw.send(new CreateRestApiCommand({ name: "pets" }));
  const pets = await gw.send(
    new CreateResourceCommand({
      restApiId: api.id,
      parentId: api.rootResourceId,
      pathPart: "pets",
    }),
  );
  const pet = await gw.send(
    new CreateResourceCommand({ restApiId: api.id, parentId: pets.id, pathPart: "{id}" }),
  );
  await gw.send(
    new PutMethodCommand({
      restApiId: api.id,
      resourceId: pet.id,
      httpMethod: "GET",
      authorizationType: "NONE",
    }),
  );
  await gw.send(
    new PutIntegrationCommand({
      restApiId: api.id,
      resourceId: pet.id,
      httpMethod: "GET",
      type: "AWS_PROXY",
      integrationHttpMethod: "POST",
      uri: `arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/${FunctionArn}/invocations`,
    }),
  );
  await gw.send(new CreateDeploymentCommand({ restApiId: api.id, stageName: "prod" }));
  const host = `${api.id}.execute-api.us-east-1.amazonaws.com`;

  const res = await send(aws, "GET", host, "/prod/pets/7", { q: "x" });
  assert.equal(res.status, 200);
  assert.equal(res.headers["x-pet"], "7");
  assert.deepEqual(JSON.parse(res.body), { id: "7", q: "x", stage: "prod" });

  const missing = await send(aws, "GET", host, "/prod/nothing");
  assert.equal(missing.status, 403);
  assert.deepEqual(JSON.parse(missing.body), { message: "Missing Authentication Token" });
  aws.close();
});
