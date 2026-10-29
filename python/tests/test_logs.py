import time

import boto3


def test_put_and_filter_log_events(aws):
    logs = boto3.client("logs")
    logs.create_log_group(logGroupName="/app/api")
    logs.create_log_stream(logGroupName="/app/api", logStreamName="web-1")
    logs.create_log_stream(logGroupName="/app/api", logStreamName="web-2")
    now = int(time.time() * 1000)
    logs.put_log_events(
        logGroupName="/app/api",
        logStreamName="web-1",
        logEvents=[
            {"timestamp": now, "message": "GET /health 200"},
            {"timestamp": now + 1, "message": "GET /orders 500"},
        ],
    )
    logs.put_log_events(
        logGroupName="/app/api",
        logStreamName="web-2",
        logEvents=[{"timestamp": now + 2, "message": "POST /orders 500"}],
    )

    errors = logs.filter_log_events(logGroupName="/app/api", filterPattern='"500"')["events"]
    assert [(e["logStreamName"], e["message"]) for e in errors] == [
        ("web-1", "GET /orders 500"),
        ("web-2", "POST /orders 500"),
    ]
    one = logs.filter_log_events(logGroupName="/app/api", logStreamNames=["web-1"])["events"]
    assert [e["message"] for e in one] == ["GET /health 200", "GET /orders 500"]

    try:
        logs.put_log_events(
            logGroupName="/app/api",
            logStreamName="missing",
            logEvents=[{"timestamp": now, "message": "x"}],
        )
    except logs.exceptions.ResourceNotFoundException:
        pass
    else:
        raise AssertionError("missing stream accepted events")
