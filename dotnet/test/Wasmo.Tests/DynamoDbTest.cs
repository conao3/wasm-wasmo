using Amazon.DynamoDBv2;
using Amazon.DynamoDBv2.DataModel;
using Amazon.DynamoDBv2.Model;

namespace Wasmo.Tests;

public class DynamoDbTest
{
    [DynamoDBTable("users")]
    public class User
    {
        [DynamoDBHashKey("id")]
        public string Id { get; set; } = "";

        [DynamoDBGlobalSecondaryIndexHashKey("by-email", AttributeName = "email")]
        public string Email { get; set; } = "";

        [DynamoDBProperty("age")]
        public int Age { get; set; }
    }

    internal static async Task CreateUsers(IAmazonDynamoDB ddb)
    {
        await ddb.CreateTableAsync(new CreateTableRequest
        {
            TableName = "users",
            BillingMode = BillingMode.PAY_PER_REQUEST,
            AttributeDefinitions =
            [
                new AttributeDefinition("id", ScalarAttributeType.S),
                new AttributeDefinition("email", ScalarAttributeType.S),
            ],
            KeySchema = [new KeySchemaElement("id", KeyType.HASH)],
            GlobalSecondaryIndexes =
            [
                new GlobalSecondaryIndex
                {
                    IndexName = "by-email",
                    KeySchema = [new KeySchemaElement("email", KeyType.HASH)],
                    Projection = new Projection { ProjectionType = ProjectionType.ALL },
                },
            ],
        });
    }

    [Fact]
    public async Task RejectsAConditionalPut()
    {
        using var mock = WasmoMock.Create();
        using var ddb = new AmazonDynamoDBClient(mock.Configure(new AmazonDynamoDBConfig()));
        await CreateUsers(ddb);
        var put = new PutItemRequest
        {
            TableName = "users",
            Item = new Dictionary<string, AttributeValue> { ["id"] = new() { S = "1" } },
            ConditionExpression = "attribute_not_exists(id)",
        };

        await ddb.PutItemAsync(put);
        await Assert.ThrowsAsync<ConditionalCheckFailedException>(() => ddb.PutItemAsync(put));
    }

    [Fact]
    public async Task QueriesAGlobalSecondaryIndex()
    {
        using var mock = WasmoMock.Create();
        using var ddb = new AmazonDynamoDBClient(mock.Configure(new AmazonDynamoDBConfig()));
        await CreateUsers(ddb);
        foreach (var (id, email) in new[] { ("1", "a@example.com"), ("2", "b@example.com"), ("3", "a@example.com") })
        {
            await ddb.PutItemAsync("users", new Dictionary<string, AttributeValue>
            {
                ["id"] = new() { S = id },
                ["email"] = new() { S = email },
            });
        }

        var result = await ddb.QueryAsync(new QueryRequest
        {
            TableName = "users",
            IndexName = "by-email",
            KeyConditionExpression = "email = :e",
            ExpressionAttributeValues = new Dictionary<string, AttributeValue> { [":e"] = new() { S = "a@example.com" } },
        });

        Assert.Equal(["1", "3"], result.Items.Select(i => i["id"].S).Order());
    }

    [Fact]
    public async Task RoundTripsThroughTheObjectPersistenceModel()
    {
        using var mock = WasmoMock.Create();
        using var ddb = new AmazonDynamoDBClient(mock.Configure(new AmazonDynamoDBConfig()));
        await CreateUsers(ddb);
        var context = new DynamoDBContextBuilder().WithDynamoDBClient(() => ddb).Build();

        await context.SaveAsync(new User { Id = "1", Email = "a@example.com", Age = 30 });
        await context.SaveAsync(new User { Id = "2", Email = "b@example.com", Age = 40 });

        var loaded = await context.LoadAsync<User>("1");
        Assert.Equal(30, loaded.Age);
        var byEmail = await context.QueryAsync<User>("b@example.com", new QueryConfig { IndexName = "by-email" }).GetRemainingAsync();
        Assert.Equal("2", Assert.Single(byEmail).Id);
    }
}
