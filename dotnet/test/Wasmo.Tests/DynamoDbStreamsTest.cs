using Amazon.DynamoDBStreams;
using Amazon.DynamoDBv2;
using Amazon.DynamoDBv2.Model;
using StreamsModel = Amazon.DynamoDBStreams.Model;

namespace Wasmo.Tests;

public class DynamoDbStreamsTest
{
    [Fact]
    public async Task RecordsWrites()
    {
        using var mock = WasmoMock.Create();
        using var db = new AmazonDynamoDBClient(mock.Configure(new AmazonDynamoDBConfig()));
        using var streams = new AmazonDynamoDBStreamsClient(mock.Configure(new AmazonDynamoDBStreamsConfig()));
        var table = await db.CreateTableAsync(new CreateTableRequest
        {
            TableName = "users",
            AttributeDefinitions = [new AttributeDefinition("id", ScalarAttributeType.S)],
            KeySchema = [new KeySchemaElement("id", Amazon.DynamoDBv2.KeyType.HASH)],
            BillingMode = BillingMode.PAY_PER_REQUEST,
            StreamSpecification = new StreamSpecification { StreamEnabled = true, StreamViewType = Amazon.DynamoDBv2.StreamViewType.NEW_AND_OLD_IMAGES },
        });
        var arn = table.TableDescription.LatestStreamArn;
        var key = new Dictionary<string, AttributeValue> { ["id"] = new() { S = "u1" } };
        await db.PutItemAsync("users", new Dictionary<string, AttributeValue> { ["id"] = new() { S = "u1" }, ["name"] = new() { S = "ada" } });
        await db.UpdateItemAsync(new UpdateItemRequest
        {
            TableName = "users",
            Key = key,
            UpdateExpression = "SET #n = :n",
            ExpressionAttributeNames = new() { ["#n"] = "name" },
            ExpressionAttributeValues = new() { [":n"] = new() { S = "grace" } },
        });
        await db.DeleteItemAsync("users", key);

        var listed = await streams.ListStreamsAsync(new StreamsModel.ListStreamsRequest { TableName = "users" });
        Assert.Equal([arn], listed.Streams.Select(s => s.StreamArn));
        var shard = (await streams.DescribeStreamAsync(new StreamsModel.DescribeStreamRequest { StreamArn = arn })).StreamDescription.Shards[0];
        var iterator = (await streams.GetShardIteratorAsync(new StreamsModel.GetShardIteratorRequest
        {
            StreamArn = arn,
            ShardId = shard.ShardId,
            ShardIteratorType = Amazon.DynamoDBStreams.ShardIteratorType.TRIM_HORIZON,
        })).ShardIterator;
        var records = (await streams.GetRecordsAsync(new StreamsModel.GetRecordsRequest { ShardIterator = iterator })).Records;
        Assert.Equal(["INSERT", "MODIFY", "REMOVE"], records.Select(r => r.EventName.Value));
        Assert.Equal("ada", records[0].Dynamodb.NewImage["name"].S);
        Assert.Equal("ada", records[1].Dynamodb.OldImage["name"].S);
        Assert.Equal("grace", records[1].Dynamodb.NewImage["name"].S);
        Assert.True(records[2].Dynamodb.NewImage is null || records[2].Dynamodb.NewImage.Count == 0);
    }
}
