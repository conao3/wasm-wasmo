import boto3


def test_send_and_receive(aws):
    sqs = boto3.client("sqs")
    url = sqs.create_queue(QueueName="jobs", Attributes={"VisibilityTimeout": "45"})["QueueUrl"]
    attrs = sqs.get_queue_attributes(QueueUrl=url, AttributeNames=["All"])["Attributes"]
    assert attrs["VisibilityTimeout"] == "45"
    assert attrs["QueueArn"] == "arn:aws:sqs:us-east-1:000000000000:jobs"

    sqs.send_message(
        QueueUrl=url,
        MessageBody="héllo",
        MessageAttributes={
            "kind": {"DataType": "String", "StringValue": "greeting"},
            "blob": {"DataType": "Binary", "BinaryValue": b"\x00\xff"},
        },
    )
    out = sqs.receive_message(QueueUrl=url, MessageAttributeNames=["All"], WaitTimeSeconds=0)
    [msg] = out["Messages"]
    assert msg["Body"] == "héllo"
    assert msg["MessageAttributes"]["kind"]["StringValue"] == "greeting"
    assert msg["MessageAttributes"]["blob"]["BinaryValue"] == b"\x00\xff"
    sqs.delete_message(QueueUrl=url, ReceiptHandle=msg["ReceiptHandle"])
    assert sqs.receive_message(QueueUrl=url, WaitTimeSeconds=0).get("Messages", []) == []


def test_batch_and_resource(aws):
    queue = boto3.resource("sqs").create_queue(QueueName="batch")
    res = queue.send_messages(Entries=[{"Id": str(i), "MessageBody": f"m{i}"} for i in range(5)])
    assert len(res["Successful"]) == 5
    bodies = set()
    while msgs := queue.receive_messages(MaxNumberOfMessages=10):
        for m in msgs:
            bodies.add(m.body)
            m.delete()
    assert bodies == {f"m{i}" for i in range(5)}


def test_fifo(aws):
    sqs = boto3.client("sqs")
    url = sqs.create_queue(
        QueueName="orders.fifo",
        Attributes={"FifoQueue": "true", "ContentBasedDeduplication": "true"},
    )["QueueUrl"]
    for body in ["a", "a", "b"]:
        sqs.send_message(QueueUrl=url, MessageBody=body, MessageGroupId="g")
    bodies = []
    while msgs := sqs.receive_message(QueueUrl=url, MaxNumberOfMessages=10).get("Messages"):
        for m in msgs:
            bodies.append(m["Body"])
            sqs.delete_message(QueueUrl=url, ReceiptHandle=m["ReceiptHandle"])
    assert bodies == ["a", "b"]
