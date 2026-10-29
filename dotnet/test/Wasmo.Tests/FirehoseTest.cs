using System.Text;
using Amazon.KinesisFirehose;
using Amazon.KinesisFirehose.Model;
using Amazon.S3;
using Amazon.S3.Model;
using Record = Amazon.KinesisFirehose.Model.Record;

namespace Wasmo.Tests;

public class FirehoseTest
{
    private static Record Data(string text) => new() { Data = new MemoryStream(Encoding.UTF8.GetBytes(text)) };

    [Fact]
    public async Task DeliversPutBatchesToS3()
    {
        using var mock = WasmoMock.Create();
        using var firehose = new AmazonKinesisFirehoseClient(mock.Configure(new AmazonKinesisFirehoseConfig()));
        using var s3 = new AmazonS3Client(mock.Configure(new AmazonS3Config { ForcePathStyle = true }));
        await s3.PutBucketAsync("lake");
        await firehose.CreateDeliveryStreamAsync(new CreateDeliveryStreamRequest
        {
            DeliveryStreamName = "events",
            ExtendedS3DestinationConfiguration = new ExtendedS3DestinationConfiguration
            {
                BucketARN = "arn:aws:s3:::lake",
                RoleARN = "arn:aws:iam::000000000000:role/firehose",
                Prefix = "raw/",
            },
        });
        await firehose.PutRecordAsync(new PutRecordRequest { DeliveryStreamName = "events", Record = Data("one\n") });
        var batch = await firehose.PutRecordBatchAsync(new PutRecordBatchRequest { DeliveryStreamName = "events", Records = [Data("two\n"), Data("three\n")] });
        Assert.Equal(0, batch.FailedPutCount);

        var keys = (await s3.ListObjectsV2Async(new ListObjectsV2Request { BucketName = "lake", Prefix = "raw/" })).S3Objects.Select(o => o.Key).ToList();
        Assert.Equal(2, keys.Count);
        Assert.All(keys, k => Assert.StartsWith("events-1-", k[(k.LastIndexOf('/') + 1)..]));
        var bodies = new List<string>();
        foreach (var key in keys)
        {
            using var obj = await s3.GetObjectAsync("lake", key);
            using var reader = new StreamReader(obj.ResponseStream);
            bodies.Add(await reader.ReadToEndAsync());
        }
        Assert.Equal(["one\n", "two\nthree\n"], bodies.Order());
    }
}
