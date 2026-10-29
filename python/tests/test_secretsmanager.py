import boto3


def test_versions_and_stages(aws):
    sm = boto3.client("secretsmanager")
    first = sm.create_secret(Name="db", SecretString="v1")
    second = sm.put_secret_value(SecretId="db", SecretString="v2")

    assert sm.get_secret_value(SecretId="db")["SecretString"] == "v2"
    previous = sm.get_secret_value(SecretId="db", VersionStage="AWSPREVIOUS")
    assert previous["SecretString"] == "v1"
    assert previous["VersionId"] == first["VersionId"]

    versions = sm.list_secret_version_ids(SecretId="db")["Versions"]
    stages = {v["VersionId"]: v["VersionStages"] for v in versions}
    assert stages == {first["VersionId"]: ["AWSPREVIOUS"], second["VersionId"]: ["AWSCURRENT"]}

    sm.put_secret_value(SecretId="db", SecretBinary=b"\x00\x01v3")
    assert sm.get_secret_value(SecretId="db")["SecretBinary"] == b"\x00\x01v3"
    try:
        sm.get_secret_value(SecretId="missing")
    except sm.exceptions.ResourceNotFoundException:
        pass
    else:
        raise AssertionError("missing secret was found")
