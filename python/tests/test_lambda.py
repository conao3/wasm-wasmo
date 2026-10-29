import base64
import json

import boto3


def create_function(lam, name):
    lam.create_function(
        FunctionName=name,
        Runtime="python3.12",
        Role="arn:aws:iam::000000000000:role/lambda",
        Handler="app.handler",
        Code={"ZipFile": b"PK fake zip"},
        Environment={"Variables": {"GREETING": "hello"}},
    )


def test_invoke_runs_the_registered_handler(aws):
    lam = boto3.client("lambda")
    sqs = boto3.client("sqs")
    url = sqs.create_queue(QueueName="seen")["QueueUrl"]
    create_function(lam, "greet")

    def greet(event, context):
        context.log(f"event {event['n']}")
        sqs.send_message(QueueUrl=url, MessageBody=json.dumps(event))
        return {"message": f"{context.environment['GREETING']} from {context.function_name}"}

    aws.register_function("greet", greet)
    out = lam.invoke(FunctionName="greet", Payload=b'{"n": 1}', LogType="Tail")
    assert out["StatusCode"] == 200
    assert "FunctionError" not in out
    assert json.loads(out["Payload"].read()) == {"message": "hello from greet"}
    assert "event 1\nEND RequestId" in base64.b64decode(out["LogResult"]).decode()
    [msg] = sqs.receive_message(QueueUrl=url)["Messages"]
    assert json.loads(msg["Body"]) == {"n": 1}


def test_exceptions_are_function_errors(aws):
    lam = boto3.client("lambda")
    create_function(lam, "boom")

    def boom(event, context):
        raise ValueError("bad input")

    aws.register_function("boom", boom)
    out = lam.invoke(FunctionName="boom")
    assert out["FunctionError"] == "Unhandled"
    body = json.loads(out["Payload"].read())
    assert body["errorType"] == "ValueError"
    assert body["errorMessage"] == "bad input"

    create_function(lam, "unbound")
    out = lam.invoke(FunctionName="unbound")
    assert json.loads(out["Payload"].read())["errorType"] == "Runtime.HandlerNotFound"
