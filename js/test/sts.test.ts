import assert from "node:assert/strict";
import { test } from "node:test";
import { GetCallerIdentityCommand, STSClient } from "@aws-sdk/client-sts";
import { mockAws } from "../dist/index.js";

test("sts get caller identity", async () => {
  const aws = mockAws();
  const out = await new STSClient(aws.clientConfig).send(new GetCallerIdentityCommand({}));
  assert.equal(out.Account, "000000000000");
  aws.close();
});
