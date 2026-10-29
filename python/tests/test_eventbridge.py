import json

import boto3


def test_put_events_to_sqs_target(aws):
    events = boto3.client("events")
    sqs = boto3.client("sqs")
    url = sqs.create_queue(QueueName="orders")["QueueUrl"]
    arn = sqs.get_queue_attributes(QueueUrl=url, AttributeNames=["QueueArn"])["Attributes"][
        "QueueArn"
    ]
    events.put_rule(
        Name="big-orders",
        EventPattern=json.dumps(
            {"source": ["shop"], "detail": {"total": [{"numeric": [">=", 100]}]}}
        ),
    )
    events.put_targets(Rule="big-orders", Targets=[{"Id": "queue", "Arn": arn}])

    put = events.put_events(
        Entries=[
            {"Source": "shop", "DetailType": "Order Placed", "Detail": json.dumps({"total": 150})},
            {"Source": "shop", "DetailType": "Order Placed", "Detail": json.dumps({"total": 20})},
        ]
    )
    assert put["FailedEntryCount"] == 0

    [message] = sqs.receive_message(QueueUrl=url, MaxNumberOfMessages=10)["Messages"]
    event = json.loads(message["Body"])
    assert event["id"] == put["Entries"][0]["EventId"]
    assert event["source"] == "shop"
    assert event["detail-type"] == "Order Placed"
    assert event["detail"] == {"total": 150}
