import assert from "node:assert/strict";
import { test } from "node:test";
import {
  GetParametersByPathCommand,
  paginateGetParametersByPath,
  PutParameterCommand,
  SSMClient,
} from "@aws-sdk/client-ssm";
import { mockAws } from "../dist/index.js";

test("ssm get parameters by path", async () => {
  const aws = mockAws();
  const ssm = new SSMClient(aws.clientConfig);
  for (const Name of ["/app/db/host", "/app/db/port", "/app/api/url", "/other/x"]) {
    await ssm.send(new PutParameterCommand({ Name, Value: Name.split("/").pop(), Type: "String" }));
  }
  await ssm.send(
    new PutParameterCommand({ Name: "/app/db/password", Value: "hunter2", Type: "SecureString" }),
  );

  const shallow = await ssm.send(new GetParametersByPathCommand({ Path: "/app" }));
  assert.deepEqual(shallow.Parameters, []);

  const found: Record<string, string> = {};
  for await (const page of paginateGetParametersByPath(
    { client: ssm, pageSize: 2 },
    { Path: "/app", Recursive: true, WithDecryption: true },
  )) {
    for (const p of page.Parameters ?? []) found[p.Name!] = p.Value!;
  }
  assert.deepEqual(found, {
    "/app/db/host": "host",
    "/app/db/port": "port",
    "/app/db/password": "hunter2",
    "/app/api/url": "url",
  });

  const db = (await ssm.send(new GetParametersByPathCommand({ Path: "/app/db/" }))).Parameters!;
  assert.deepEqual(db.map((p) => p.Name).sort(), [
    "/app/db/host",
    "/app/db/password",
    "/app/db/port",
  ]);
  const secret = db.find((p) => p.Name === "/app/db/password")!;
  assert.equal(secret.Type, "SecureString");
  assert.notEqual(secret.Value, "hunter2");
  aws.close();
});
