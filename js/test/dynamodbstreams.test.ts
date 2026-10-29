import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CreateTableCommand,
  DeleteItemCommand,
  DynamoDBClient,
  PutItemCommand,
  UpdateItemCommand,
} from "@aws-sdk/client-dynamodb";
import {
  DescribeStreamCommand,
  DynamoDBStreamsClient,
  GetRecordsCommand,
  GetShardIteratorCommand,
  ListStreamsCommand,
} from "@aws-sdk/client-dynamodb-streams";
import { mockAws } from "../dist/index.js";

test("dynamodb streams record writes", async () => {
  const aws = mockAws();
  const db = new DynamoDBClient(aws.clientConfig);
  const streams = new DynamoDBStreamsClient(aws.clientConfig);
  const TableName = "users";
  const { TableDescription } = await db.send(
    new CreateTableCommand({
      TableName,
      AttributeDefinitions: [{ AttributeName: "id", AttributeType: "S" }],
      KeySchema: [{ AttributeName: "id", KeyType: "HASH" }],
      BillingMode: "PAY_PER_REQUEST",
      StreamSpecification: { StreamEnabled: true, StreamViewType: "NEW_AND_OLD_IMAGES" },
    }),
  );
  const StreamArn = TableDescription!.LatestStreamArn!;
  await db.send(new PutItemCommand({ TableName, Item: { id: { S: "u1" }, name: { S: "ada" } } }));
  await db.send(
    new UpdateItemCommand({
      TableName,
      Key: { id: { S: "u1" } },
      UpdateExpression: "SET #n = :n",
      ExpressionAttributeNames: { "#n": "name" },
      ExpressionAttributeValues: { ":n": { S: "grace" } },
    }),
  );
  await db.send(new DeleteItemCommand({ TableName, Key: { id: { S: "u1" } } }));

  const listed = await streams.send(new ListStreamsCommand({ TableName }));
  assert.deepEqual(
    listed.Streams!.map((s) => s.StreamArn),
    [StreamArn],
  );
  const { StreamDescription } = await streams.send(new DescribeStreamCommand({ StreamArn }));
  const { ShardIterator } = await streams.send(
    new GetShardIteratorCommand({
      StreamArn,
      ShardId: StreamDescription!.Shards![0]!.ShardId,
      ShardIteratorType: "TRIM_HORIZON",
    }),
  );
  const { Records } = await streams.send(new GetRecordsCommand({ ShardIterator }));
  assert.deepEqual(
    Records!.map((r) => r.eventName),
    ["INSERT", "MODIFY", "REMOVE"],
  );
  assert.deepEqual(Records![0]!.dynamodb!.NewImage!.name, { S: "ada" });
  assert.deepEqual(Records![1]!.dynamodb!.OldImage!.name, { S: "ada" });
  assert.deepEqual(Records![1]!.dynamodb!.NewImage!.name, { S: "grace" });
  assert.equal(Records![2]!.dynamodb!.NewImage, undefined);
  aws.close();
});
