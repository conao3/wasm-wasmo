import json

import boto3


def test_rest_api_invokes_the_registered_handler(aws):
    lam = boto3.client("lambda")
    gw = boto3.client("apigateway")
    arn = lam.create_function(
        FunctionName="pets",
        Runtime="python3.12",
        Role="arn:aws:iam::000000000000:role/lambda",
        Handler="app.handler",
        Code={"ZipFile": b"PK fake zip"},
    )["FunctionArn"]

    def pets(event, context):
        body = {
            "id": event["pathParameters"]["id"],
            "q": event["queryStringParameters"]["q"],
            "stage": event["requestContext"]["stage"],
        }
        return {"statusCode": 200, "headers": {"X-Pet": body["id"]}, "body": json.dumps(body)}

    aws.register_function("pets", pets)
    api = gw.create_rest_api(name="pets")
    root = api["rootResourceId"]
    parent = gw.create_resource(restApiId=api["id"], parentId=root, pathPart="pets")
    pet = gw.create_resource(restApiId=api["id"], parentId=parent["id"], pathPart="{id}")
    gw.put_method(
        restApiId=api["id"], resourceId=pet["id"], httpMethod="GET", authorizationType="NONE"
    )
    gw.put_integration(
        restApiId=api["id"],
        resourceId=pet["id"],
        httpMethod="GET",
        type="AWS_PROXY",
        integrationHttpMethod="POST",
        uri=f"arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/{arn}/invocations",
    )
    gw.create_deployment(restApiId=api["id"], stageName="prod")
    base = f"https://{api['id']}.execute-api.us-east-1.amazonaws.com/prod"

    res = aws.fetch(f"{base}/pets/7?q=x")
    assert res.status == 200
    assert res.headers["x-pet"] == "7"
    assert json.loads(res.body) == {"id": "7", "q": "x", "stage": "prod"}

    missing = aws.fetch(f"{base}/nothing", method="GET")
    assert missing.status == 403
    assert json.loads(missing.body) == {"message": "Missing Authentication Token"}
