import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CloudWatchClient,
  GetMetricDataCommand,
  GetMetricStatisticsCommand,
  InvalidParameterValueException,
  ListMetricsCommand,
  PutMetricDataCommand,
} from "@aws-sdk/client-cloudwatch";
import { mockAws } from "../dist/index.js";

test("cloudwatch put metric data and get metric data", async () => {
  const aws = mockAws();
  const cw = new CloudWatchClient(aws.clientConfig);
  const hour = 3_600_000;
  const base = new Date(Math.floor(Date.now() / hour) * hour - hour);
  const later = new Date(base.getTime() + 60_000);
  const Dimensions = [{ Name: "Service", Value: "web" }];
  await cw.send(
    new PutMetricDataCommand({
      Namespace: "App",
      MetricData: [
        ...[10, 20, 30].map((Value) => ({
          MetricName: "Latency",
          Dimensions,
          Timestamp: base,
          Value,
        })),
        { MetricName: "Latency", Dimensions, Timestamp: later, Value: 100, Unit: "Milliseconds" },
      ],
    }),
  );

  const out = await cw.send(
    new GetMetricDataCommand({
      StartTime: base,
      EndTime: new Date(base.getTime() + hour),
      ScanBy: "TimestampAscending",
      MetricDataQueries: [
        {
          Id: "sum",
          MetricStat: {
            Metric: { Namespace: "App", MetricName: "Latency", Dimensions },
            Period: 60,
            Stat: "Sum",
          },
        },
        { Id: "doubled", Expression: "sum * 2", Label: "doubled" },
      ],
    }),
  );
  const results = Object.fromEntries(out.MetricDataResults!.map((r) => [r.Id, r]));
  assert.deepEqual(results.sum!.Values, [60, 100]);
  assert.deepEqual(results.sum!.Timestamps, [base, later]);
  assert.deepEqual(results.doubled!.Values, [120, 200]);
  assert.equal(results.doubled!.Label, "doubled");
  const metrics = await cw.send(new ListMetricsCommand({ Namespace: "App" }));
  assert.deepEqual(
    metrics.Metrics!.map((m) => m.MetricName),
    ["Latency"],
  );
  await assert.rejects(
    cw.send(
      new PutMetricDataCommand({
        Namespace: "AWS/EC2",
        MetricData: [{ MetricName: "x", Value: 1 }],
      }),
    ),
    InvalidParameterValueException,
  );
  aws.close();
});

test("cloudwatch large put metric data is gzipped", async () => {
  const aws = mockAws();
  const handler = aws.clientConfig.requestHandler;
  const encodings: (string | undefined)[] = [];
  const cw = new CloudWatchClient({
    ...aws.clientConfig,
    requestHandler: {
      handle: (...[request, options]: Parameters<typeof handler.handle>) => {
        encodings.push(request.headers["content-encoding"]);
        return handler.handle(request, options);
      },
    },
  });
  const at = new Date(Math.floor(Date.now() / 3_600_000) * 3_600_000);
  await cw.send(
    new PutMetricDataCommand({
      Namespace: "Bulk",
      MetricData: Array.from({ length: 500 }, (_, i) => ({
        MetricName: "Requests",
        Dimensions: [{ Name: "Host", Value: `host-${(i + 1) % 10}` }],
        Timestamp: at,
        Value: i + 1,
      })),
    }),
  );
  assert.deepEqual(encodings, ["gzip"]);

  const stats = await cw.send(
    new GetMetricStatisticsCommand({
      Namespace: "Bulk",
      MetricName: "Requests",
      Dimensions: [{ Name: "Host", Value: "host-0" }],
      StartTime: at,
      EndTime: new Date(at.getTime() + 3_600_000),
      Period: 3600,
      Statistics: ["Sum", "SampleCount"],
    }),
  );
  assert.deepEqual(
    stats.Datapoints!.map((d) => [d.Sum, d.SampleCount]),
    [[12750, 50]],
  );
  aws.close();
});
