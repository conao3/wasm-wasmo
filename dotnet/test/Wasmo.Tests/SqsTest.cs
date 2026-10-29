using Amazon.SQS;
using Amazon.SQS.Model;

namespace Wasmo.Tests;

public class SqsTest
{
    [Fact]
    public async Task SendsAndReceivesWithAttributes()
    {
        using var mock = WasmoMock.Create();
        using var sqs = new AmazonSQSClient(mock.Configure(new AmazonSQSConfig()));
        var queueUrl = (await sqs.CreateQueueAsync("jobs")).QueueUrl;

        await sqs.SendMessageAsync(new SendMessageRequest
        {
            QueueUrl = queueUrl,
            MessageBody = "héllo wasmo",
            MessageAttributes = new Dictionary<string, MessageAttributeValue>
            {
                ["kind"] = new() { DataType = "String", StringValue = "greeting" },
                ["count"] = new() { DataType = "Number", StringValue = "42" },
                ["blob"] = new() { DataType = "Binary", BinaryValue = new MemoryStream([0, 1, 2, 255]) },
            },
        });

        var received = await sqs.ReceiveMessageAsync(new ReceiveMessageRequest
        {
            QueueUrl = queueUrl,
            MessageAttributeNames = ["All"],
            MessageSystemAttributeNames = ["All"],
        });

        var message = Assert.Single(received.Messages);
        Assert.Equal("héllo wasmo", message.Body);
        Assert.Equal("greeting", message.MessageAttributes["kind"].StringValue);
        Assert.Equal("42", message.MessageAttributes["count"].StringValue);
        Assert.Equal([0, 1, 2, 255], message.MessageAttributes["blob"].BinaryValue.ToArray());
        Assert.Equal("1", message.Attributes["ApproximateReceiveCount"]);

        await sqs.DeleteMessageAsync(queueUrl, message.ReceiptHandle);
        var empty = await sqs.ReceiveMessageAsync(new ReceiveMessageRequest { QueueUrl = queueUrl });
        Assert.True(empty.Messages is null || empty.Messages.Count == 0);
    }
}
