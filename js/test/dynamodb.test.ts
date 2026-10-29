import assert from "node:assert/strict";
import { test } from "node:test";
import {
  ConditionalCheckFailedException,
  CreateTableCommand,
  DynamoDBClient,
  ListTablesCommand,
  PutItemCommand,
} from "@aws-sdk/client-dynamodb";
import {
  DynamoDBDocumentClient,
  GetCommand,
  PutCommand,
  QueryCommand,
  UpdateCommand,
} from "@aws-sdk/lib-dynamodb";
import { mockAws, type WasmoMock } from "../dist/index.js";

async function createTable(aws: WasmoMock, name: string): Promise<DynamoDBClient> {
  const ddb = new DynamoDBClient(aws.clientConfig);
  await ddb.send(
    new CreateTableCommand({
      TableName: name,
      AttributeDefinitions: [
        { AttributeName: "pk", AttributeType: "S" },
        { AttributeName: "sk", AttributeType: "N" },
      ],
      KeySchema: [
        { AttributeName: "pk", KeyType: "HASH" },
        { AttributeName: "sk", KeyType: "RANGE" },
      ],
      BillingMode: "PAY_PER_REQUEST",
    }),
  );
  return ddb;
}

test("dynamodb document client round trip", async () => {
  const aws = mockAws();
  const doc = DynamoDBDocumentClient.from(await createTable(aws, "users"));
  const item = {
    pk: "user#1",
    sk: 1,
    name: "Ada",
    tags: new Set(["a", "b"]),
    nested: { score: 1.5, list: [1, "two", null, true] },
  };
  await doc.send(new PutCommand({ TableName: "users", Item: item }));
  await doc.send(new PutCommand({ TableName: "users", Item: { pk: "user#1", sk: 2 } }));
  const got = await doc.send(new GetCommand({ TableName: "users", Key: { pk: "user#1", sk: 1 } }));
  assert.deepEqual(got.Item, item);
  const updated = await doc.send(
    new UpdateCommand({
      TableName: "users",
      Key: { pk: "user#1", sk: 1 },
      UpdateExpression: "SET visits = if_not_exists(visits, :zero) + :one",
      ExpressionAttributeValues: { ":zero": 0, ":one": 1 },
      ReturnValues: "UPDATED_NEW",
    }),
  );
  assert.deepEqual(updated.Attributes, { visits: 1 });
  const q = await doc.send(
    new QueryCommand({
      TableName: "users",
      KeyConditionExpression: "pk = :pk AND sk > :min",
      ExpressionAttributeValues: { ":pk": "user#1", ":min": 0 },
      ScanIndexForward: false,
    }),
  );
  assert.deepEqual(
    q.Items?.map((i) => i.sk),
    [2, 1],
  );
  aws.close();
});

test("dynamodb conditional failure is ConditionalCheckFailedException", async () => {
  const aws = mockAws();
  const ddb = await createTable(aws, "users");
  const put = () =>
    ddb.send(
      new PutItemCommand({
        TableName: "users",
        Item: { pk: { S: "a" }, sk: { N: "1" } },
        ConditionExpression: "attribute_not_exists(pk)",
      }),
    );
  await put();
  await assert.rejects(put(), (err: unknown) => {
    assert.ok(err instanceof ConditionalCheckFailedException);
    assert.equal(err.$metadata.httpStatusCode, 400);
    return true;
  });
  aws.close();
});

test("dynamodb state is isolated per mock and cleared by reset", async () => {
  const a = mockAws();
  const b = mockAws();
  await createTable(a, "only-in-a");
  const list = async (m: WasmoMock) =>
    (await new DynamoDBClient(m.clientConfig).send(new ListTablesCommand({}))).TableNames;
  assert.deepEqual(await list(a), ["only-in-a"]);
  assert.deepEqual(await list(b), []);
  a.reset();
  assert.deepEqual(await list(a), []);
  a.close();
  b.close();
});
