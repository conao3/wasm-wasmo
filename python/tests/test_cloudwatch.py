from datetime import datetime, timedelta, timezone

import boto3


def test_put_and_get_metric_data(aws):
    cw = boto3.client("cloudwatch")
    base = datetime.now(timezone.utc).replace(minute=0, second=0, microsecond=0) - timedelta(
        hours=1
    )
    dims = [{"Name": "Service", "Value": "web"}]
    cw.put_metric_data(
        Namespace="App",
        MetricData=[
            {
                "MetricName": "Latency",
                "Dimensions": dims,
                "Timestamp": base,
                "Value": v,
                "Unit": "Milliseconds",
            }
            for v in (10.0, 20.0, 30.0)
        ]
        + [
            {
                "MetricName": "Latency",
                "Dimensions": dims,
                "Timestamp": base + timedelta(minutes=1),
                "Value": 100.0,
                "Unit": "Milliseconds",
            }
        ],
    )

    out = cw.get_metric_data(
        StartTime=base,
        EndTime=base + timedelta(hours=1),
        ScanBy="TimestampAscending",
        MetricDataQueries=[
            {
                "Id": "sum",
                "MetricStat": {
                    "Metric": {"Namespace": "App", "MetricName": "Latency", "Dimensions": dims},
                    "Period": 60,
                    "Stat": "Sum",
                },
            },
            {"Id": "doubled", "Expression": "sum * 2", "Label": "doubled"},
        ],
    )
    results = {r["Id"]: r for r in out["MetricDataResults"]}
    assert results["sum"]["Values"] == [60.0, 100.0]
    assert results["sum"]["Timestamps"] == [base, base + timedelta(minutes=1)]
    assert results["doubled"]["Values"] == [120.0, 200.0]
    assert results["doubled"]["Label"] == "doubled"

    listed = cw.list_metrics(Namespace="App")["Metrics"]
    assert [m["MetricName"] for m in listed] == ["Latency"]
