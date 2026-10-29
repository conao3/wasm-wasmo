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
}
