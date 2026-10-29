import assert from "node:assert/strict";
import { test } from "node:test";
import {
  AttachRolePolicyCommand,
  CreatePolicyCommand,
  CreateRoleCommand,
  GetRoleCommand,
  IAMClient,
  ListAttachedRolePoliciesCommand,
  NoSuchEntityException,
  SimulatePrincipalPolicyCommand,
} from "@aws-sdk/client-iam";
import { mockAws } from "../dist/index.js";

const trust = {
  Version: "2012-10-17",
  Statement: [
    { Effect: "Allow", Principal: { Service: "lambda.amazonaws.com" }, Action: "sts:AssumeRole" },
  ],
};
const readOrders = {
  Version: "2012-10-17",
  Statement: [
    { Effect: "Allow", Action: "dynamodb:GetItem", Resource: "arn:aws:dynamodb:*:*:table/orders" },
  ],
};

test("iam role with an attached policy and a simulation", async () => {
  const aws = mockAws();
  const iam = new IAMClient(aws.clientConfig);
  const { Role } = await iam.send(
    new CreateRoleCommand({ RoleName: "app", AssumeRolePolicyDocument: JSON.stringify(trust) }),
  );
  assert.equal(Role!.Arn, "arn:aws:iam::000000000000:role/app");
  const { Policy } = await iam.send(
    new CreatePolicyCommand({
      PolicyName: "read-orders",
      PolicyDocument: JSON.stringify(readOrders),
    }),
  );
  await iam.send(new AttachRolePolicyCommand({ RoleName: "app", PolicyArn: Policy!.Arn }));
  const attached = await iam.send(new ListAttachedRolePoliciesCommand({ RoleName: "app" }));
  assert.deepEqual(
    attached.AttachedPolicies!.map((p) => p.PolicyName),
    ["read-orders"],
  );

  const { EvaluationResults } = await iam.send(
    new SimulatePrincipalPolicyCommand({
      PolicySourceArn: Role!.Arn,
      ActionNames: ["dynamodb:GetItem", "dynamodb:PutItem"],
      ResourceArns: ["arn:aws:dynamodb:us-east-1:000000000000:table/orders"],
    }),
  );
  assert.deepEqual(
    Object.fromEntries(EvaluationResults!.map((r) => [r.EvalActionName, r.EvalDecision])),
    {
      "dynamodb:GetItem": "allowed",
      "dynamodb:PutItem": "implicitDeny",
    },
  );
  await assert.rejects(
    iam.send(new GetRoleCommand({ RoleName: "missing" })),
    NoSuchEntityException,
  );
  aws.close();
});
