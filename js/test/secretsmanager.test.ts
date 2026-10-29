import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CreateSecretCommand,
  GetSecretValueCommand,
  ListSecretVersionIdsCommand,
  PutSecretValueCommand,
  ResourceNotFoundException,
  SecretsManagerClient,
} from "@aws-sdk/client-secrets-manager";
import { mockAws } from "../dist/index.js";

test("secrets manager versions and stages", async () => {
  const aws = mockAws();
  const sm = new SecretsManagerClient(aws.clientConfig);
  const first = await sm.send(new CreateSecretCommand({ Name: "db", SecretString: "v1" }));
  const second = await sm.send(new PutSecretValueCommand({ SecretId: "db", SecretString: "v2" }));

  assert.equal((await sm.send(new GetSecretValueCommand({ SecretId: "db" }))).SecretString, "v2");
  const previous = await sm.send(
    new GetSecretValueCommand({ SecretId: "db", VersionStage: "AWSPREVIOUS" }),
  );
  assert.equal(previous.SecretString, "v1");
  assert.equal(previous.VersionId, first.VersionId);

  const { Versions } = await sm.send(new ListSecretVersionIdsCommand({ SecretId: "db" }));
  assert.deepEqual(Object.fromEntries(Versions!.map((v) => [v.VersionId, v.VersionStages])), {
    [first.VersionId!]: ["AWSPREVIOUS"],
    [second.VersionId!]: ["AWSCURRENT"],
  });

  const binary = new Uint8Array([0, 1, 0xff]);
  await sm.send(new PutSecretValueCommand({ SecretId: "db", SecretBinary: binary }));
  assert.deepEqual(
    (await sm.send(new GetSecretValueCommand({ SecretId: "db" }))).SecretBinary,
    binary,
  );
  await assert.rejects(
    sm.send(new GetSecretValueCommand({ SecretId: "missing" })),
    ResourceNotFoundException,
  );
  aws.close();
});
