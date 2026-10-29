import boto3


def test_stream_records_after_writes(aws):
    db = boto3.client("dynamodb")
    streams = boto3.client("dynamodbstreams")
    arn = db.create_table(
        TableName="users",
        AttributeDefinitions=[{"AttributeName": "id", "AttributeType": "S"}],
        KeySchema=[{"AttributeName": "id", "KeyType": "HASH"}],
        BillingMode="PAY_PER_REQUEST",
        StreamSpecification={"StreamEnabled": True, "StreamViewType": "NEW_AND_OLD_IMAGES"},
    )["TableDescription"]["LatestStreamArn"]

    db.put_item(TableName="users", Item={"id": {"S": "u1"}, "name": {"S": "ada"}})
    db.update_item(
        TableName="users",
        Key={"id": {"S": "u1"}},
        UpdateExpression="SET #n = :n",
        ExpressionAttributeNames={"#n": "name"},
        ExpressionAttributeValues={":n": {"S": "grace"}},
    )
    db.delete_item(TableName="users", Key={"id": {"S": "u1"}})

    assert [s["StreamArn"] for s in streams.list_streams(TableName="users")["Streams"]] == [arn]
    shard = streams.describe_stream(StreamArn=arn)["StreamDescription"]["Shards"][0]
    iterator = streams.get_shard_iterator(
        StreamArn=arn, ShardId=shard["ShardId"], ShardIteratorType="TRIM_HORIZON"
    )["ShardIterator"]
    records = streams.get_records(ShardIterator=iterator)["Records"]
    assert [r["eventName"] for r in records] == ["INSERT", "MODIFY", "REMOVE"]
    assert records[0]["dynamodb"]["NewImage"]["name"] == {"S": "ada"}
    assert records[1]["dynamodb"]["OldImage"]["name"] == {"S": "ada"}
    assert records[1]["dynamodb"]["NewImage"]["name"] == {"S": "grace"}
    assert "NewImage" not in records[2]["dynamodb"]
    assert records[0]["dynamodb"]["SequenceNumber"] < records[1]["dynamodb"]["SequenceNumber"]
