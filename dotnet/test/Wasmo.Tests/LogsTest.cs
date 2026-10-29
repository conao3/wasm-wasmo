using Amazon.CloudWatchLogs;
using Amazon.CloudWatchLogs.Model;

namespace Wasmo.Tests;

public class LogsTest
{
    [Fact]
    public async Task PutsAndFiltersLogEvents()
    {
        using var mock = WasmoMock.Create();
        using var logs = new AmazonCloudWatchLogsClient(mock.Configure(new AmazonCloudWatchLogsConfig()));
        const string group = "/app/api";
        await logs.CreateLogGroupAsync(new CreateLogGroupRequest { LogGroupName = group });
        foreach (var stream in new[] { "web-1", "web-2" })
        {
            await logs.CreateLogStreamAsync(new CreateLogStreamRequest { LogGroupName = group, LogStreamName = stream });
        }
        var now = DateTime.UtcNow;
        await logs.PutLogEventsAsync(new PutLogEventsRequest
        {
            LogGroupName = group,
            LogStreamName = "web-1",
            LogEvents = [new InputLogEvent { Timestamp = now, Message = "GET /health 200" }, new InputLogEvent { Timestamp = now.AddMilliseconds(1), Message = "GET /orders 500" }],
        });
        await logs.PutLogEventsAsync(new PutLogEventsRequest
        {
            LogGroupName = group,
            LogStreamName = "web-2",
            LogEvents = [new InputLogEvent { Timestamp = now.AddMilliseconds(2), Message = "POST /orders 500" }],
        });

        var errors = await logs.FilterLogEventsAsync(new FilterLogEventsRequest { LogGroupName = group, FilterPattern = "\"500\"" });
        Assert.Equal(["web-1 GET /orders 500", "web-2 POST /orders 500"], errors.Events.Select(e => $"{e.LogStreamName} {e.Message}"));
        var one = await logs.FilterLogEventsAsync(new FilterLogEventsRequest { LogGroupName = group, LogStreamNames = ["web-1"] });
        Assert.Equal(["GET /health 200", "GET /orders 500"], one.Events.Select(e => e.Message));
        await Assert.ThrowsAsync<ResourceNotFoundException>(() => logs.PutLogEventsAsync(new PutLogEventsRequest
        {
            LogGroupName = group,
            LogStreamName = "missing",
            LogEvents = [new InputLogEvent { Timestamp = now, Message = "x" }],
        }));
    }
}
