using System.Diagnostics;
using Amazon.DynamoDBv2;
using Amazon.DynamoDBv2.Model;
using Amazon.S3;
using Amazon.S3.Model;
using Xunit.Abstractions;

namespace Wasmo.Tests;

public class LatencyTest(ITestOutputHelper output)
{
    private const int Warmup = 200;
    private const int Runs = 500;

    private static async Task<double> MedianMs(Func<Task> op)
    {
        for (var i = 0; i < Warmup; i++)
        {
            await op();
        }
        var samples = new double[Runs];
        for (var i = 0; i < Runs; i++)
        {
            var start = Stopwatch.GetTimestamp();
            await op();
            samples[i] = Stopwatch.GetElapsedTime(start).TotalMilliseconds;
        }
        Array.Sort(samples);
        return samples[Runs / 2];
    }

    private void Report(string name, double ms) => output.WriteLine($"wasmo latency {name,-24} median {ms:F3} ms");

    [Fact]
    public async Task PrintsMedians()
    {
        Report("WasmoMock.Create", await MedianMs(() =>
        {
            WasmoMock.Create().Dispose();
            return Task.CompletedTask;
        }));

        using var mock = WasmoMock.Create();
        using var ddb = new AmazonDynamoDBClient(mock.Configure(new AmazonDynamoDBConfig()));
        await DynamoDbTest.CreateUsers(ddb);
        var item = new Dictionary<string, AttributeValue> { ["id"] = new() { S = "k" }, ["v"] = new() { S = new string('x', 100) } };
        var key = new Dictionary<string, AttributeValue> { ["id"] = new() { S = "k" } };
        Report("DynamoDB PutItem", await MedianMs(() => ddb.PutItemAsync("users", item)));
        Report("DynamoDB GetItem", await MedianMs(() => ddb.GetItemAsync("users", key)));

        using var s3 = new AmazonS3Client(mock.Configure(new AmazonS3Config { ForcePathStyle = true }));
        await s3.PutBucketAsync("bench");
        var data = new byte[1024];
        Report("S3 PutObject 1 KiB", await MedianMs(() =>
            s3.PutObjectAsync(new PutObjectRequest { BucketName = "bench", Key = "k", InputStream = new MemoryStream(data) })));
        Report("S3 GetObject 1 KiB", await MedianMs(async () =>
        {
            using var got = await s3.GetObjectAsync("bench", "k");
            await got.ResponseStream.CopyToAsync(Stream.Null);
        }));
    }
}
