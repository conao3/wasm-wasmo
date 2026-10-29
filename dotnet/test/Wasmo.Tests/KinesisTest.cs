using System.Text;
using Amazon.Kinesis;
using Amazon.Kinesis.Model;

namespace Wasmo.Tests;

public class KinesisTest
{
    [Fact]
    public async Task PutsAndGetsRecords()
    {
        using var mock = WasmoMock.Create();
        using var kinesis = new AmazonKinesisClient(mock.Configure(new AmazonKinesisConfig()));
        await kinesis.CreateStreamAsync(new CreateStreamRequest { StreamName = "clicks", ShardCount = 2 });
        var put = await kinesis.PutRecordsAsync(new PutRecordsRequest
        {
            StreamName = "clicks",
            Records = [.. Enumerable.Range(0, 10).Select(i => new PutRecordsRequestEntry { PartitionKey = $"user-{i}", Data = new MemoryStream(Encoding.UTF8.GetBytes($"click-{i}")) })],
        });
        Assert.Equal(0, put.FailedRecordCount);
        Assert.Equal(10, put.Records.Count);

        var got = new Dictionary<string, string>();
        foreach (var shard in (await kinesis.ListShardsAsync(new ListShardsRequest { StreamName = "clicks" })).Shards)
        {
            var iterator = (await kinesis.GetShardIteratorAsync(new GetShardIteratorRequest { StreamName = "clicks", ShardId = shard.ShardId, ShardIteratorType = ShardIteratorType.TRIM_HORIZON })).ShardIterator;
            var output = await kinesis.GetRecordsAsync(new GetRecordsRequest { ShardIterator = iterator });
            Assert.NotNull(output.NextShardIterator);
            foreach (var record in output.Records ?? [])
            {
                Assert.InRange(record.ApproximateArrivalTimestamp!.Value.ToUniversalTime(), DateTime.UtcNow.AddMinutes(-1), DateTime.UtcNow.AddMinutes(1));
                got[record.PartitionKey] = Encoding.UTF8.GetString(record.Data.ToArray());
            }
        }
        Assert.Equal(Enumerable.Range(0, 10).ToDictionary(i => $"user-{i}", i => $"click-{i}"), got);
        await Assert.ThrowsAsync<ResourceNotFoundException>(() => kinesis.DescribeStreamAsync(new DescribeStreamRequest { StreamName = "missing" }));
    }
}
