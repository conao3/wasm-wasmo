using System.Net;
using Amazon.S3;
using Amazon.S3.Model;
using Amazon.S3.Transfer;

namespace Wasmo.Tests;

public class S3Test
{
    private static byte[] Bytes(int length) => [.. Enumerable.Range(0, length).Select(i => (byte)(i * 31 % 256))];

    private static async Task<byte[]> ReadAll(Stream stream)
    {
        using var buffer = new MemoryStream();
        await stream.CopyToAsync(buffer);
        return buffer.ToArray();
    }

    [Theory]
    [InlineData(true)]
    [InlineData(false)]
    public async Task PutsAndGetsBinaryObjects(bool forcePathStyle)
    {
        using var mock = WasmoMock.Create();
        var config = mock.Configure(new AmazonS3Config { ForcePathStyle = forcePathStyle });
        var recorder = Recorder.Attach(mock, config);
        using var s3 = new AmazonS3Client(config);
        await s3.PutBucketAsync("photos");
        var data = Bytes(4096);
        const string key = "dir/hello world ü+%.bin";

        await s3.PutObjectAsync(new PutObjectRequest
        {
            BucketName = "photos",
            Key = key,
            InputStream = new MemoryStream(data),
            ContentType = "application/octet-stream",
        });

        using var got = await s3.GetObjectAsync(new GetObjectRequest { BucketName = "photos", Key = key, ChecksumMode = ChecksumMode.ENABLED });
        Assert.Equal("application/octet-stream", got.Headers.ContentType);
        Assert.Equal(data.Length, got.ContentLength);
        Assert.Equal(data, await ReadAll(got.ResponseStream));

        var listed = await s3.ListObjectsV2Async(new ListObjectsV2Request { BucketName = "photos", Prefix = "dir/" });
        Assert.Equal(key, Assert.Single(listed.S3Objects).Key);

        var missing = await Assert.ThrowsAsync<NoSuchKeyException>(() => s3.GetObjectAsync("photos", "absent"));
        Assert.Equal(HttpStatusCode.NotFound, missing.StatusCode);
        Assert.Equal("NoSuchKey", missing.ErrorCode);

        var putRequest = recorder.Requests.First(r => r.Method == HttpMethod.Put && r.RequestUri!.AbsolutePath.EndsWith(".bin", StringComparison.Ordinal));
        Assert.Equal(forcePathStyle ? "wasmo.local" : "photos.wasmo.local", putRequest.RequestUri!.Host);
        Assert.Equal("aws-chunked", Assert.Single(putRequest.Content!.Headers.ContentEncoding));
        Assert.Equal("x-amz-checksum-crc32", Assert.Single(putRequest.Headers.GetValues("X-Amz-Trailer")));
    }

    [Fact]
    public async Task StoresTheRequestedChecksum()
    {
        using var mock = WasmoMock.Create();
        using var s3 = new AmazonS3Client(mock.Configure(new AmazonS3Config()));
        await s3.PutBucketAsync("sums");

        var put = await s3.PutObjectAsync(new PutObjectRequest
        {
            BucketName = "sums",
            Key = "k",
            ContentBody = "checksummed",
            ChecksumAlgorithm = ChecksumAlgorithm.SHA256,
        });

        Assert.False(string.IsNullOrEmpty(put.ChecksumSHA256));
        var head = await s3.GetObjectMetadataAsync(new GetObjectMetadataRequest { BucketName = "sums", Key = "k", ChecksumMode = ChecksumMode.ENABLED });
        Assert.Equal(put.ChecksumSHA256, head.ChecksumSHA256);
    }

    [Fact]
    public async Task UploadsMultipartWithTransferUtility()
    {
        using var mock = WasmoMock.Create();
        using var s3 = new AmazonS3Client(mock.Configure(new AmazonS3Config()));
        await s3.PutBucketAsync("big");
        var data = Bytes(12 * 1024 * 1024);
        using var transfer = new TransferUtility(s3, new TransferUtilityConfig { MinSizeBeforePartUpload = 5 * 1024 * 1024 });

        await transfer.UploadAsync(new TransferUtilityUploadRequest
        {
            BucketName = "big",
            Key = "blob",
            InputStream = new MemoryStream(data),
            PartSize = 5 * 1024 * 1024,
        });

        var head = await s3.GetObjectMetadataAsync("big", "blob");
        Assert.EndsWith("-3\"", head.ETag);
        Assert.Equal(data.Length, head.ContentLength);
        using var stream = await transfer.OpenStreamAsync("big", "blob");
        Assert.Equal(data, await ReadAll(stream));
    }
}
