using System.Text.Json;
using Amazon.SimpleNotificationService;
using Amazon.SimpleNotificationService.Model;
using Amazon.SQS;
using Amazon.SQS.Model;

namespace Wasmo.Tests;

public class SnsTest
{
    [Fact]
    public async Task DeliversToASubscribedQueue()
    {
        using var mock = WasmoMock.Create();
        using var sns = new AmazonSimpleNotificationServiceClient(mock.Configure(new AmazonSimpleNotificationServiceConfig()));
        using var sqs = new AmazonSQSClient(mock.Configure(new AmazonSQSConfig()));
        var topicArn = (await sns.CreateTopicAsync("events")).TopicArn;
        var queueUrl = (await sqs.CreateQueueAsync("inbox")).QueueUrl;
        var queueArn = (await sqs.GetQueueAttributesAsync(queueUrl, ["QueueArn"])).QueueARN;
        await sns.SubscribeAsync(new SubscribeRequest { TopicArn = topicArn, Protocol = "sqs", Endpoint = queueArn });

        await sns.PublishAsync(new PublishRequest { TopicArn = topicArn, Message = "order placed", Subject = "orders" });

        var received = await sqs.ReceiveMessageAsync(new ReceiveMessageRequest { QueueUrl = queueUrl });
        using var envelope = JsonDocument.Parse(Assert.Single(received.Messages).Body);
        Assert.Equal("Notification", envelope.RootElement.GetProperty("Type").GetString());
        Assert.Equal("order placed", envelope.RootElement.GetProperty("Message").GetString());
        Assert.Equal(topicArn, envelope.RootElement.GetProperty("TopicArn").GetString());
    }
}
