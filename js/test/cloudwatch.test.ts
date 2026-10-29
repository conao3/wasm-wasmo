import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CloudWatchClient,
  GetMetricDataCommand,
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
