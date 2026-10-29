from decimal import Decimal

import boto3
import pytest
from boto3.dynamodb.conditions import Key


def create_orders(ddb):
    ddb.create_table(
        TableName="orders",
        AttributeDefinitions=[
            {"AttributeName": "id", "AttributeType": "S"},
            {"AttributeName": "customer", "AttributeType": "S"},
            {"AttributeName": "placed", "AttributeType": "N"},
        ],
        KeySchema=[{"AttributeName": "id", "KeyType": "HASH"}],
        GlobalSecondaryIndexes=[
            {
                "IndexName": "by-customer",
                "KeySchema": [
                    {"AttributeName": "customer", "KeyType": "HASH"},
                    {"AttributeName": "placed", "KeyType": "RANGE"},
                ],
                "Projection": {"ProjectionType": "ALL"},
            }
        ],
        BillingMode="PAY_PER_REQUEST",
    )


def test_client_put_get(aws):
    ddb = boto3.client("dynamodb")
    create_orders(ddb)
    ddb.put_item(
        TableName="orders",
        Item={
            "id": {"S": "o1"},
            "customer": {"S": "c1"},
            "placed": {"N": "1"},
            "tags": {"SS": ["a", "b"]},
            "raw": {"B": b"\x00\x01"},
        },
    )
    item = ddb.get_item(TableName="orders", Key={"id": {"S": "o1"}})["Item"]
    assert item["customer"] == {"S": "c1"}
    assert sorted(item["tags"]["SS"]) == ["a", "b"]
    assert item["raw"] == {"B": b"\x00\x01"}
    assert "Item" not in ddb.get_item(TableName="orders", Key={"id": {"S": "missing"}})


def test_resource_table(aws):
    create_orders(boto3.client("dynamodb"))
    table = boto3.resource("dynamodb").Table("orders")
    table.put_item(Item={"id": "o1", "customer": "c1", "placed": 3, "total": Decimal("9.5")})
    table.update_item(
        Key={"id": "o1"},
        UpdateExpression="SET #t = #t + :d",
        ExpressionAttributeNames={"#t": "total"},
        ExpressionAttributeValues={":d": Decimal("0.5")},
    )
    item = table.get_item(Key={"id": "o1"})["Item"]
    assert item == {"id": "o1", "customer": "c1", "placed": 3, "total": Decimal("10")}


def test_conditional_failure(aws):
    ddb = boto3.client("dynamodb")
    create_orders(ddb)
    table = boto3.resource("dynamodb").Table("orders")
    put = dict(
        Item={"id": "o1", "customer": "c1", "placed": 1},
        ConditionExpression="attribute_not_exists(id)",
    )
    table.put_item(**put)
    with pytest.raises(table.meta.client.exceptions.ConditionalCheckFailedException):
        table.put_item(**put)
    with pytest.raises(ddb.exceptions.ConditionalCheckFailedException) as err:
        ddb.put_item(
            TableName="orders",
            Item={"id": {"S": "o1"}},
            ConditionExpression="attribute_not_exists(id)",
            ReturnValuesOnConditionCheckFailure="ALL_OLD",
        )
    assert err.value.response["Error"]["Code"] == "ConditionalCheckFailedException"
    assert err.value.response["Item"]["customer"] == {"S": "c1"}


def test_query_gsi_with_paginator(aws):
    ddb = boto3.client("dynamodb")
    create_orders(ddb)
    table = boto3.resource("dynamodb").Table("orders")
    with table.batch_writer() as batch:
        for i in range(25):
            batch.put_item(Item={"id": f"o{i}", "customer": f"c{i % 2}", "placed": i})

    pages = list(
        ddb.get_paginator("query").paginate(
            TableName="orders",
            IndexName="by-customer",
            KeyConditionExpression="customer = :c AND placed >= :p",
            ExpressionAttributeValues={":c": {"S": "c0"}, ":p": {"N": "4"}},
            PaginationConfig={"PageSize": 3},
        )
    )
    placed = [int(item["placed"]["N"]) for page in pages for item in page["Items"]]
    assert placed == list(range(4, 25, 2))
    assert len(pages) >= 4

    out = table.query(
        IndexName="by-customer",
        KeyConditionExpression=Key("customer").eq("c1"),
        ScanIndexForward=False,
        Limit=2,
    )
    assert [item["placed"] for item in out["Items"]] == [23, 21]
    assert "LastEvaluatedKey" in out


def test_transaction_cancel(aws):
    ddb = boto3.client("dynamodb")
    create_orders(ddb)
    ddb.put_item(TableName="orders", Item={"id": {"S": "o1"}})
    with pytest.raises(ddb.exceptions.TransactionCanceledException) as err:
        ddb.transact_write_items(
            TransactItems=[
                {"Put": {"TableName": "orders", "Item": {"id": {"S": "o2"}}}},
                {
                    "Put": {
                        "TableName": "orders",
                        "Item": {"id": {"S": "o1"}},
                        "ConditionExpression": "attribute_not_exists(id)",
                    }
                },
            ]
        )
    reasons = [r["Code"] for r in err.value.response["CancellationReasons"]]
    assert reasons == ["None", "ConditionalCheckFailed"]
    assert "Item" not in ddb.get_item(TableName="orders", Key={"id": {"S": "o2"}})
