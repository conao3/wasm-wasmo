import boto3


def test_caller_identity(aws):
    identity = boto3.client("sts").get_caller_identity()
    assert identity["Account"] == "000000000000"
    assert identity["Arn"].startswith("arn:aws:iam::000000000000:")


def test_assume_role(aws):
    sts = boto3.client("sts")
    out = sts.assume_role(RoleArn="arn:aws:iam::000000000000:role/deployer", RoleSessionName="ci")
    creds = out["Credentials"]
    assert creds["AccessKeyId"]
    assert out["AssumedRoleUser"]["Arn"].endswith(":assumed-role/deployer/ci")
