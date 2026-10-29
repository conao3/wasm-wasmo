import boto3


def test_get_parameters_by_path(aws):
    ssm = boto3.client("ssm")
    for name in ["/app/db/host", "/app/db/port", "/app/api/url", "/other/x"]:
        ssm.put_parameter(Name=name, Value=name.rsplit("/", 1)[1], Type="String")
    ssm.put_parameter(Name="/app/db/password", Value="hunter2", Type="SecureString")

    shallow = ssm.get_parameters_by_path(Path="/app")["Parameters"]
    assert shallow == []

    paginator = ssm.get_paginator("get_parameters_by_path")
    found = {
        p["Name"]: p["Value"]
        for page in paginator.paginate(
            Path="/app", Recursive=True, WithDecryption=True, PaginationConfig={"PageSize": 2}
        )
        for p in page["Parameters"]
    }
    assert found == {
        "/app/db/host": "host",
        "/app/db/port": "port",
        "/app/db/password": "hunter2",
        "/app/api/url": "url",
    }

    db = ssm.get_parameters_by_path(Path="/app/db/")["Parameters"]
    assert sorted(p["Name"] for p in db) == ["/app/db/host", "/app/db/password", "/app/db/port"]
    secret = next(p for p in db if p["Name"] == "/app/db/password")
    assert secret["Type"] == "SecureString"
    assert secret["Value"] != "hunter2"
