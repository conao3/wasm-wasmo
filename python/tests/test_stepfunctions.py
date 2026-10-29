import json

import boto3

ROLE = "arn:aws:iam::000000000000:role/sfn"


def create_function(lam, name):
    lam.create_function(
        FunctionName=name,
        Runtime="python3.12",
        Role="arn:aws:iam::000000000000:role/lambda",
        Handler="app.handler",
        Code={"ZipFile": b"PK fake zip"},
    )


def test_state_machine_retries_and_catches_handler_errors(aws):
    lam = boto3.client("lambda")
    sfn = boto3.client("stepfunctions")
    create_function(lam, "flaky")
    create_function(lam, "broken")
    calls = []

    def flaky(event, context):
        calls.append(event)
        if len(calls) < 2:
            raise TimeoutError("not yet")
        return {"doubled": event["n"] * 2}

    def broken(event, context):
        raise ValueError("always")

    aws.register_function("flaky", flaky)
    aws.register_function("broken", broken)
    definition = {
        "StartAt": "Flaky",
        "States": {
            "Flaky": {
                "Type": "Task",
                "Resource": "arn:aws:states:::lambda:invoke",
                "Parameters": {"FunctionName": "flaky", "Payload.$": "$"},
                "ResultSelector": {"doubled.$": "$.Payload.doubled"},
                "ResultPath": "$.result",
                "Retry": [{"ErrorEquals": ["TimeoutError"], "MaxAttempts": 2}],
                "Next": "Broken",
            },
            "Broken": {
                "Type": "Task",
                "Resource": "arn:aws:states:::lambda:invoke",
                "Parameters": {"FunctionName": "broken", "Payload": {}},
                "Catch": [
                    {"ErrorEquals": ["ValueError"], "ResultPath": "$.caught", "Next": "Done"}
                ],
                "Next": "Done",
            },
            "Done": {"Type": "Succeed"},
        },
    }
    arn = sfn.create_state_machine(
        name="retry-catch", roleArn=ROLE, definition=json.dumps(definition)
    )["stateMachineArn"]
    execution = sfn.start_execution(stateMachineArn=arn, input='{"n": 21}')["executionArn"]
    d = sfn.describe_execution(executionArn=execution)
    assert d["status"] == "SUCCEEDED"
    output = json.loads(d["output"])
    assert output["result"] == {"doubled": 42}
    assert output["caught"]["Error"] == "ValueError"
    assert len(calls) == 2


def test_sync_execution_waits_for_the_handler(aws):
    lam = boto3.client("lambda")
    sfn = boto3.client("stepfunctions")
    create_function(lam, "greet")
    aws.register_function("greet", lambda event, context: f"hello {event['name']}")
    definition = {
        "StartAt": "Greet",
        "States": {
            "Greet": {
                "Type": "Task",
                "Resource": "arn:aws:states:::lambda:invoke",
                "Parameters": {"FunctionName": "greet", "Payload.$": "$"},
                "OutputPath": "$.Payload",
                "End": True,
            }
        },
    }
    arn = sfn.create_state_machine(
        name="sync", roleArn=ROLE, type="EXPRESS", definition=json.dumps(definition)
    )["stateMachineArn"]
    out = sfn.start_sync_execution(stateMachineArn=arn, input='{"name": "ann"}')
    assert out["status"] == "SUCCEEDED"
    assert json.loads(out["output"]) == "hello ann"
