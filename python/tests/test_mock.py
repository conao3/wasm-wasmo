import os
import threading
from concurrent.futures import ThreadPoolExecutor

import boto3
import pytest

import wasmo


def queues(client):
    return client.list_queues().get("QueueUrls", [])


def test_mocks_are_isolated():
    with wasmo.mock_aws() as a, wasmo.mock_aws() as b:
        sqs_b = boto3.client("sqs")
        sqs_a = a.client("sqs")
        sqs_a.create_queue(QueueName="only-in-a")
        assert len(queues(sqs_a)) == 1
        assert queues(sqs_b) == []
        assert queues(b.client("sqs")) == []


def test_clients_bind_at_creation():
    with wasmo.mock_aws():
        first = boto3.client("sqs")
        first.create_queue(QueueName="first")
        with wasmo.mock_aws():
            assert queues(boto3.client("sqs")) == []
            assert len(queues(first)) == 1


def test_explicit_mock_without_patching():
    aws = wasmo.Mock()
    try:
        ddb = aws.resource("dynamodb", region_name="eu-west-1")
        ddb.create_table(
            TableName="tbl",
            AttributeDefinitions=[{"AttributeName": "k", "AttributeType": "S"}],
            KeySchema=[{"AttributeName": "k", "KeyType": "HASH"}],
            BillingMode="PAY_PER_REQUEST",
        )
        assert ddb.meta.client.meta.region_name == "eu-west-1"
        eu = aws.client("dynamodb", region_name="eu-west-1")
        assert eu.list_tables()["TableNames"] == ["tbl"]
        assert aws.client("dynamodb").list_tables()["TableNames"] == []
        assert aws.session().region_name == "us-east-1"
    finally:
        aws.close()


def test_reset_and_close():
    with wasmo.mock_aws() as aws:
        sqs = boto3.client("sqs")
        sqs.create_queue(QueueName="q")
        aws.reset()
        assert queues(sqs) == []
    with pytest.raises(RuntimeError, match="closed"):
        queues(sqs)


@wasmo.mock_aws
def test_decorator_gets_fresh_state():
    sqs = boto3.client("sqs")
    assert queues(sqs) == []
    sqs.create_queue(QueueName="decorated")


def test_decorator_repeats():
    test_decorator_gets_fresh_state()
    test_decorator_gets_fresh_state()


def test_environment_is_forced_and_restored(monkeypatch):
    monkeypatch.setenv("AWS_ACCESS_KEY_ID", "AKIAREAL")
    monkeypatch.setenv("AWS_SESSION_TOKEN", "real")
    monkeypatch.setenv("AWS_DEFAULT_REGION", "ap-northeast-1")
    session = boto3.Session()
    with wasmo.mock_aws():
        client = session.client("sts")
        assert client.meta.region_name == "us-east-1"
        assert client.meta.endpoint_url == wasmo.ENDPOINT
        assert os.environ["AWS_ACCESS_KEY_ID"] == "test"
        assert "AWS_SESSION_TOKEN" not in os.environ
        explicit = session.client("sqs", region_name="eu-west-1")
        assert explicit.get_queue_url  # binds without error
        assert explicit.create_queue(QueueName="q")["QueueUrl"]
        arn = explicit.get_queue_attributes(
            QueueUrl=explicit.list_queues()["QueueUrls"][0], AttributeNames=["QueueArn"]
        )["Attributes"]["QueueArn"]
        assert arn == "arn:aws:sqs:eu-west-1:000000000000:q"
    assert os.environ["AWS_ACCESS_KEY_ID"] == "AKIAREAL"
    assert os.environ["AWS_SESSION_TOKEN"] == "real"
    assert os.environ["AWS_DEFAULT_REGION"] == "ap-northeast-1"


def test_threads_share_one_mock(aws):
    ddb = boto3.client("dynamodb")
    ddb.create_table(
        TableName="counter",
        AttributeDefinitions=[{"AttributeName": "k", "AttributeType": "S"}],
        KeySchema=[{"AttributeName": "k", "KeyType": "HASH"}],
        BillingMode="PAY_PER_REQUEST",
    )

    def work(n):
        client = boto3.client("dynamodb") if n % 2 else ddb
        for i in range(25):
            client.update_item(
                TableName="counter",
                Key={"k": {"S": "hits"}},
                UpdateExpression="ADD n :one",
                ExpressionAttributeValues={":one": {"N": "1"}},
            )
            client.put_item(TableName="counter", Item={"k": {"S": f"{n}-{i}"}})

    with ThreadPoolExecutor(8) as pool:
        list(pool.map(work, range(8)))
    item = ddb.get_item(TableName="counter", Key={"k": {"S": "hits"}})["Item"]
    assert item["n"] == {"N": "200"}
    assert ddb.scan(TableName="counter", Select="COUNT")["Count"] == 201


def test_threads_with_their_own_mocks():
    errors = []
    barrier = threading.Barrier(4)

    def work(n):
        try:
            aws = wasmo.Mock()
            sqs = aws.client("sqs")
            barrier.wait()
            for i in range(n + 1):
                sqs.create_queue(QueueName=f"q{i}")
            assert len(queues(sqs)) == n + 1
            aws.close()
        except BaseException as e:
            errors.append(e)

    threads = [threading.Thread(target=work, args=(n,)) for n in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert errors == []
