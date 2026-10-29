using Amazon.CloudWatch;
using Amazon.CloudWatch.Model;

namespace Wasmo.Tests;

public class CloudWatchTest
{
    private static readonly double[] Samples = [10.0, 20.0, 30.0];

    [Fact]
    public async Task PutsAndGetsMetricData()
    {
        using var mock = WasmoMock.Create();
        using var cw = new AmazonCloudWatchClient(mock.Configure(new AmazonCloudWatchConfig()));
        var now = DateTime.UtcNow;
        var start = new DateTime(now.Year, now.Month, now.Day, now.Hour, 0, 0, DateTimeKind.Utc).AddHours(-1);
        var later = start.AddMinutes(1);
        var dims = new List<Dimension> { new() { Name = "Service", Value = "web" } };
        var data = Samples.Select(v => new MetricDatum { MetricName = "Latency", Dimensions = dims, Timestamp = start, Value = v }).ToList();
        data.Add(new MetricDatum { MetricName = "Latency", Dimensions = dims, Timestamp = later, Value = 100, Unit = StandardUnit.Milliseconds });
        await cw.PutMetricDataAsync(new PutMetricDataRequest { Namespace = "App", MetricData = data });

        var output = await cw.GetMetricDataAsync(new GetMetricDataRequest
        {
            StartTime = start,
            EndTime = start.AddHours(1),
            ScanBy = ScanBy.TimestampAscending,
            MetricDataQueries =
            [
                new MetricDataQuery
                {
                    Id = "sum",
                    MetricStat = new MetricStat { Metric = new Metric { Namespace = "App", MetricName = "Latency", Dimensions = dims }, Period = 60, Stat = "Sum" },
                },
                new MetricDataQuery { Id = "doubled", Expression = "sum * 2", Label = "doubled" },
            ],
        });
        var results = output.MetricDataResults.ToDictionary(r => r.Id);
        Assert.Equal([60.0, 100.0], results["sum"].Values);
        Assert.Equal([start, later], results["sum"].Timestamps.Select(t => t.ToUniversalTime()));
        Assert.Equal([120.0, 200.0], results["doubled"].Values);
        Assert.Equal("doubled", results["doubled"].Label);
        var metrics = await cw.ListMetricsAsync(new ListMetricsRequest { Namespace = "App" });
        Assert.Equal(["Latency"], metrics.Metrics.Select(m => m.MetricName));
        await Assert.ThrowsAsync<InvalidParameterValueException>(() => cw.PutMetricDataAsync(new PutMetricDataRequest
        {
            Namespace = "AWS/EC2",
            MetricData = [new MetricDatum { MetricName = "x", Value = 1 }],
        }));
    }

    [Fact]
    public async Task GzipsALargePutMetricData()
    {
        using var mock = WasmoMock.Create();
        var config = mock.Configure(new AmazonCloudWatchConfig());
        var recorder = Recorder.Attach(mock, config);
        using var cw = new AmazonCloudWatchClient(config);
        var now = DateTime.UtcNow;
        var at = new DateTime(now.Year, now.Month, now.Day, now.Hour, 0, 0, DateTimeKind.Utc);
        await cw.PutMetricDataAsync(new PutMetricDataRequest
        {
            Namespace = "Bulk",
            MetricData = [.. Enumerable.Range(1, 500).Select(i => new MetricDatum
            {
                MetricName = "Requests",
                Dimensions = [new Dimension { Name = "Host", Value = $"host-{i % 10}" }],
                Timestamp = at,
                Value = i,
            })],
        });
        Assert.Equal(["gzip"], Assert.Single(recorder.Requests).Content!.Headers.ContentEncoding);

        var stats = await cw.GetMetricStatisticsAsync(new GetMetricStatisticsRequest
        {
            Namespace = "Bulk",
            MetricName = "Requests",
            Dimensions = [new Dimension { Name = "Host", Value = "host-0" }],
            StartTime = at,
            EndTime = at.AddHours(1),
            Period = 3600,
            Statistics = ["Sum", "SampleCount"],
        });
        var point = Assert.Single(stats.Datapoints);
        Assert.Equal(12750, point.Sum);
        Assert.Equal(50, point.SampleCount);
    }
}
