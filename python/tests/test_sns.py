import json

import boto3


def test_sns_to_sqs(aws):
    sns = boto3.client("sns")
    sqs = boto3.client("sqs")
    topic = sns.create_topic(Name="events")["TopicArn"]
    url = sqs.create_queue(QueueName="inbox")["QueueUrl"]
    raw_url = sqs.create_queue(QueueName="raw")["QueueUrl"]
    arn = sqs.get_queue_attributes(QueueUrl=url, AttributeNames=["QueueArn"])["Attributes"][
        "QueueArn"
    ]
    raw_arn = sqs.get_queue_attributes(QueueUrl=raw_url, AttributeNames=["QueueArn"])["Attributes"][
        "QueueArn"
    ]
    sns.subscribe(TopicArn=topic, Protocol="sqs", Endpoint=arn)
    sns.subscribe(
        TopicArn=topic,
        Protocol="sqs",
        Endpoint=raw_arn,
        Attributes={"RawMessageDelivery": "true", "FilterPolicy": '{"kind": ["order"]}'},
    )

    sns.publish(
        TopicArn=topic,
        Message="placed",
        Subject="hi",
        MessageAttributes={"kind": {"DataType": "String", "StringValue": "order"}},
    )
    sns.publish(
        TopicArn=topic,
        Message="ignored by the filter",
        MessageAttributes={"kind": {"DataType": "String", "StringValue": "refund"}},
    )

    envelopes = [
        json.loads(m["Body"])
        for m in sqs.receive_message(QueueUrl=url, MaxNumberOfMessages=10)["Messages"]
    ]
    assert sorted(e["Message"] for e in envelopes) == ["ignored by the filter", "placed"]
    assert all(e["TopicArn"] == topic and e["Type"] == "Notification" for e in envelopes)

    [raw] = sqs.receive_message(QueueUrl=raw_url, MaxNumberOfMessages=10)["Messages"]
    assert raw["Body"] == "placed"
