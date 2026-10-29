import json

import boto3

TRUST = {
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Principal": {"Service": "lambda.amazonaws.com"},
            "Action": "sts:AssumeRole",
        }
    ],
}
READ_ORDERS = {
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Action": "dynamodb:GetItem",
            "Resource": "arn:aws:dynamodb:*:*:table/orders",
        }
    ],
}


def test_role_policy_and_simulation(aws):
    iam = boto3.client("iam")
    role = iam.create_role(RoleName="app", AssumeRolePolicyDocument=json.dumps(TRUST))["Role"]
    assert role["Arn"] == "arn:aws:iam::000000000000:role/app"
    policy = iam.create_policy(PolicyName="read-orders", PolicyDocument=json.dumps(READ_ORDERS))
    iam.attach_role_policy(RoleName="app", PolicyArn=policy["Policy"]["Arn"])

    attached = iam.list_attached_role_policies(RoleName="app")["AttachedPolicies"]
    assert [p["PolicyName"] for p in attached] == ["read-orders"]

    results = iam.simulate_principal_policy(
        PolicySourceArn=role["Arn"],
        ActionNames=["dynamodb:GetItem", "dynamodb:PutItem"],
        ResourceArns=["arn:aws:dynamodb:us-east-1:000000000000:table/orders"],
    )["EvaluationResults"]
    decisions = {r["EvalActionName"]: r["EvalDecision"] for r in results}
    assert decisions == {"dynamodb:GetItem": "allowed", "dynamodb:PutItem": "implicitDeny"}

    try:
        iam.get_role(RoleName="missing")
    except iam.exceptions.NoSuchEntityException:
        pass
    else:
        raise AssertionError("missing role was found")
