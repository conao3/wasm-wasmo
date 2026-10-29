using Amazon.DynamoDBv2;
using Amazon.DynamoDBv2.Model;
using Amazon.SQS;

namespace Wasmo.Tests;

public class MockTest
{
    [Fact]
    public async Task KeepsMocksIsolated()
    {
        using var a = WasmoMock.Create();
        using var b = WasmoMock.Create();
        using var sqsA = new AmazonSQSClient(a.Configure(new AmazonSQSConfig()));
        using var sqsB = new AmazonSQSClient(b.Configure(new AmazonSQSConfig()));

        await sqsA.CreateQueueAsync("only-in-a");

        Assert.Single((await sqsA.ListQueuesAsync("")).QueueUrls);
        var inB = (await sqsB.ListQueuesAsync("")).QueueUrls;
        Assert.True(inB is null || inB.Count == 0);
    }

    [Fact]
    public async Task ServesConcurrentRequests()
    {
        using var mock = WasmoMock.Create();
        using var ddb = new AmazonDynamoDBClient(mock.Configure(new AmazonDynamoDBConfig()));
        await DynamoDbTest.CreateUsers(ddb);

        await Parallel.ForEachAsync(Enumerable.Range(0, 200), new ParallelOptions { MaxDegreeOfParallelism = 16 }, async (i, ct) =>
            await ddb.PutItemAsync("users", new Dictionary<string, AttributeValue> { ["id"] = new() { S = $"{i}" } }, ct));

        var scan = await ddb.ScanAsync(new ScanRequest { TableName = "users", Select = Select.COUNT });
        Assert.Equal(200, scan.Count);
    }

    [Fact]
    public async Task RunsManyMocksInParallel()
    {
        var counts = await Task.WhenAll(Enumerable.Range(0, 8).Select(i => Task.Run(async () =>
        {
            using var mock = WasmoMock.Create();
            using var sqs = new AmazonSQSClient(mock.Configure(new AmazonSQSConfig()));
            for (var q = 0; q <= i; q++)
            {
                await sqs.CreateQueueAsync($"q{q}");
            }
            return (await sqs.ListQueuesAsync("")).QueueUrls.Count;
        })));

        Assert.Equal(Enumerable.Range(1, 8), counts);
    }

    [Fact]
    public async Task RejectsRequestsAfterDispose()
    {
        var mock = WasmoMock.Create();
        using var sqs = new AmazonSQSClient(mock.Configure(new AmazonSQSConfig { MaxErrorRetry = 0 }));
        mock.Dispose();

        var error = await Record.ExceptionAsync(() => sqs.ListQueuesAsync(""));
        while (error is not null and not ObjectDisposedException)
        {
            error = error.InnerException;
        }
        Assert.IsType<ObjectDisposedException>(error);
    }
}
