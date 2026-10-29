using System.Text.Json;
using Amazon.EventBridge;
using Amazon.EventBridge.Model;
using Amazon.SQS;
using Amazon.SQS.Model;

namespace Wasmo.Tests;

public class EventBridgeTest
{
    private static readonly int[] Totals = [150, 20];

    [Fact]
    public async Task PutEventsReachesAnSqsTarget()
    {
        using var mock = WasmoMock.Create();
        using var events = new AmazonEventBridgeClient(mock.Configure(new AmazonEventBridgeConfig()));
        using var sqs = new AmazonSQSClient(mock.Configure(new AmazonSQSConfig()));
        var queueUrl = (await sqs.CreateQueueAsync("orders")).QueueUrl;
        var queueArn = (await sqs.GetQueueAttributesAsync(queueUrl, ["QueueArn"])).QueueARN;
        await events.PutRuleAsync(new PutRuleRequest
        {
            Name = "big-orders",
            EventPattern = """{"source": ["shop"], "detail": {"total": [{"numeric": [">=", 100]}]}}""",
        });
        await events.PutTargetsAsync(new PutTargetsRequest { Rule = "big-orders", Targets = [new Target { Id = "queue", Arn = queueArn }] });

        var put = await events.PutEventsAsync(new PutEventsRequest
        {
            Entries = [.. Totals.Select(total => new PutEventsRequestEntry { Source = "shop", DetailType = "Order Placed", Detail = $$"""{"total": {{total}}}""" })],
        });
        Assert.Equal(0, put.FailedEntryCount);

        var received = await sqs.ReceiveMessageAsync(new ReceiveMessageRequest { QueueUrl = queueUrl, MaxNumberOfMessages = 10 });
        using var body = JsonDocument.Parse(Assert.Single(received.Messages).Body);
        var root = body.RootElement;
        Assert.Equal(put.Entries[0].EventId, root.GetProperty("id").GetString());
        Assert.Equal("shop", root.GetProperty("source").GetString());
        Assert.Equal("Order Placed", root.GetProperty("detail-type").GetString());
        Assert.Equal(150, root.GetProperty("detail").GetProperty("total").GetInt32());
    }
}
