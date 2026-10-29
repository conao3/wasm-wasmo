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

    try:
        cw.put_metric_data(Namespace="AWS/EC2", MetricData=[{"MetricName": "x", "Value": 1.0}])
    except cw.exceptions.InvalidParameterValueException:
        pass
    else:
        raise AssertionError("reserved namespace accepted")


def test_large_put_metric_data_is_gzipped(aws):
    cw = boto3.client("cloudwatch")
    encodings = []
    cw.meta.events.register_first(
        "before-send.cloudwatch.PutMetricData",
        lambda request, **_: encodings.append(request.headers.get("Content-Encoding")),
    )
    at = datetime.now(timezone.utc).replace(minute=0, second=0, microsecond=0)
    cw.put_metric_data(
        Namespace="Bulk",
        MetricData=[
            {
                "MetricName": "Requests",
                "Dimensions": [{"Name": "Host", "Value": f"host-{i % 10}"}],
                "Timestamp": at,
                "Value": float(i),
            }
            for i in range(1, 501)
        ],
    )
    assert encodings == [b"gzip"]

    stats = cw.get_metric_statistics(
        Namespace="Bulk",
        MetricName="Requests",
        Dimensions=[{"Name": "Host", "Value": "host-0"}],
        StartTime=at,
        EndTime=at + timedelta(hours=1),
        Period=3600,
        Statistics=["Sum", "SampleCount"],
    )["Datapoints"]
    assert [(d["Sum"], d["SampleCount"]) for d in stats] == [(12750.0, 50.0)]
