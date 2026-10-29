use serde_json::{Value, json};
use wasmo_core::Emulator;
use wasmo_core::env::FixedEnv;
use wasmo_core::wire::{Request, RequestMeta};

struct Client {
    emu: Emulator<FixedEnv>,
    region: &'static str,
}

impl Client {
    fn new() -> Self {
        Self {
            emu: Emulator::new(FixedEnv {
                now_ms: 1_700_000_000_000.0,
                seed: 42,
            }),
            region: "us-east-1",
        }
    }

    fn send(&mut self, op: &str, body: Value) -> (u16, Value) {
        let req = Request::from_meta(
            RequestMeta {
                method: "POST".into(),
                url: "http://wasmo.local/".into(),
                headers: vec![
                    (
                        "Authorization".into(),
                        format!(
                            "AWS4-HMAC-SHA256 Credential=test/20260101/{}/dynamodb/aws4_request, SignedHeaders=host, Signature=x",
                            self.region
                        ),
                    ),
                    ("Content-Type".into(), "application/x-amz-json-1.0".into()),
                    ("X-Amz-Target".into(), format!("DynamoDB_20120810.{op}")),
                ],
            },
            serde_json::to_vec(&body).unwrap(),
        );
        let res = self.emu.handle(&req);
        (res.status, serde_json::from_slice(&res.body).unwrap())
    }

    fn ok(&mut self, op: &str, body: Value) -> Value {
        let (status, out) = self.send(op, body);
        assert_eq!(status, 200, "{op}: {out}");
        out
    }

    fn fail(&mut self, op: &str, body: Value) -> Value {
        let (status, out) = self.send(op, body);
        assert_eq!(status, 400, "{op} unexpectedly succeeded: {out}");
        out
    }
}

fn with_table() -> Client {
    let mut c = Client::new();
    c.ok(
        "CreateTable",
        json!({
            "TableName": "music",
            "AttributeDefinitions": [
                {"AttributeName": "artist", "AttributeType": "S"},
                {"AttributeName": "song", "AttributeType": "S"},
                {"AttributeName": "year", "AttributeType": "N"},
            ],
            "KeySchema": [
                {"AttributeName": "artist", "KeyType": "HASH"},
                {"AttributeName": "song", "KeyType": "RANGE"},
            ],
            "LocalSecondaryIndexes": [{
                "IndexName": "by-year",
                "KeySchema": [
                    {"AttributeName": "artist", "KeyType": "HASH"},
                    {"AttributeName": "year", "KeyType": "RANGE"},
                ],
                "Projection": {"ProjectionType": "KEYS_ONLY"},
            }],
            "BillingMode": "PAY_PER_REQUEST",
        }),
    );
    for (artist, song, year) in [
        ("acme", "a", "1999"),
        ("acme", "b", "2001"),
        ("acme", "c", "1985"),
        ("zed", "a", "2020"),
    ] {
        c.ok(
            "PutItem",
            json!({"TableName": "music", "Item": {"artist": {"S": artist}, "song": {"S": song}, "year": {"N": year}, "plays": {"N": "0"}}}),
        );
    }
    c
}

#[test]
fn table_becomes_active_on_the_next_call() {
    let mut c = Client::new();
    let created = c.ok(
        "CreateTable",
        json!({
            "TableName": "tbl",
            "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}],
            "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
            "ProvisionedThroughput": {"ReadCapacityUnits": 5, "WriteCapacityUnits": 5},
        }),
    );
    assert_eq!(created["TableDescription"]["TableStatus"], "CREATING");
    assert_eq!(
        created["TableDescription"]["CreationDateTime"],
        1_700_000_000
    );
    let d = c.ok("DescribeTable", json!({"TableName": "tbl"}));
    assert_eq!(d["Table"]["TableStatus"], "ACTIVE");
    assert_eq!(
        d["Table"]["TableArn"],
        "arn:aws:dynamodb:us-east-1:000000000000:table/tbl"
    );
    let e = c.fail("DescribeTable", json!({"TableName": "nope"}));
    assert_eq!(e["__type"], "ResourceNotFoundException");
}

#[test]
fn create_table_validates_key_schema_and_definitions() {
    let mut c = Client::new();
    let e = c.fail(
        "CreateTable",
        json!({
            "TableName": "tbl",
            "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}, {"AttributeName": "extra", "AttributeType": "S"}],
            "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
            "BillingMode": "PAY_PER_REQUEST",
        }),
    );
    assert_eq!(e["__type"], "ValidationException");
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("does not exactly match")
    );
    let e = c.fail(
        "CreateTable",
        json!({
            "TableName": "tbl",
            "AttributeDefinitions": [{"AttributeName": "id", "AttributeType": "S"}],
            "KeySchema": [{"AttributeName": "id", "KeyType": "HASH"}],
        }),
    );
    assert!(e["message"].as_str().unwrap().contains("ReadCapacityUnits"));
}

#[test]
fn numbers_are_normalized_and_kept_exact() {
    let mut c = with_table();
    c.ok(
        "PutItem",
        json!({"TableName": "music", "Item": {"artist": {"S": "n"}, "song": {"S": "n"}, "year": {"N": "1"},
            "big": {"N": "0012345678901234567890.123456789012345600"}, "ns": {"NS": ["1.50", "2"]}}}),
    );
    let out = c.ok(
        "UpdateItem",
        json!({"TableName": "music", "Key": {"artist": {"S": "n"}, "song": {"S": "n"}},
            "UpdateExpression": "SET big = big + :tiny", "ExpressionAttributeValues": {":tiny": {"N": "0.0000000000000004"}},
            "ReturnValues": "ALL_NEW"}),
    );
    assert_eq!(
        out["Attributes"]["big"]["N"],
        "12345678901234567890.123456789012346"
    );
    assert_eq!(out["Attributes"]["ns"]["NS"], json!(["1.5", "2"]));
}

#[test]
fn conditional_failures_carry_the_old_item() {
    let mut c = with_table();
    let e = c.fail(
        "PutItem",
        json!({"TableName": "music", "Item": {"artist": {"S": "acme"}, "song": {"S": "a"}},
            "ConditionExpression": "attribute_not_exists(artist)", "ReturnValuesOnConditionCheckFailure": "ALL_OLD"}),
    );
    assert_eq!(e["__type"], "ConditionalCheckFailedException");
    assert_eq!(e["Item"]["year"]["N"], "1999");
    let e = c.fail(
        "UpdateItem",
        json!({"TableName": "music", "Key": {"artist": {"S": "acme"}, "song": {"S": "a"}},
            "Expected": {"plays": {"Value": {"N": "5"}}}, "AttributeUpdates": {"plays": {"Action": "ADD", "Value": {"N": "1"}}}}),
    );
    assert_eq!(e["__type"], "ConditionalCheckFailedException");
    let out = c.ok(
        "UpdateItem",
        json!({"TableName": "music", "Key": {"artist": {"S": "acme"}, "song": {"S": "a"}},
            "Expected": {"plays": {"ComparisonOperator": "EQ", "AttributeValueList": [{"N": "0"}]}},
            "AttributeUpdates": {"plays": {"Action": "ADD", "Value": {"N": "1"}}, "year": {"Action": "DELETE"}},
            "ReturnValues": "UPDATED_NEW"}),
    );
    assert_eq!(out["Attributes"], json!({"plays": {"N": "1"}}));
    let e = c.fail(
        "PutItem",
        json!({"TableName": "music", "Item": {"artist": {"S": "acme"}, "song": {"S": "a"}},
            "Expected": {"artist": {"Exists": false}}, "ConditionExpression": "attribute_not_exists(artist)"}),
    );
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .starts_with("Can not use both")
    );
}

#[test]
fn query_orders_by_sort_key_and_pages() {
    let mut c = with_table();
    let q = json!({"TableName": "music", "KeyConditionExpression": "artist = :a",
        "ExpressionAttributeValues": {":a": {"S": "acme"}}, "Limit": 2});
    let first = c.ok("Query", q.clone());
    assert_eq!(first["Count"], 2);
    assert_eq!(first["Items"][1]["song"]["S"], "b");
    let mut next = q.clone();
    next["ExclusiveStartKey"] = first["LastEvaluatedKey"].clone();
    let second = c.ok("Query", next);
    assert_eq!(second["Items"][0]["song"]["S"], "c");
    assert!(second.get("LastEvaluatedKey").is_none());

    let lsi = c.ok(
        "Query",
        json!({"TableName": "music", "IndexName": "by-year", "KeyConditionExpression": "artist = :a AND #y < :y",
            "ExpressionAttributeNames": {"#y": "year"}, "ExpressionAttributeValues": {":a": {"S": "acme"}, ":y": {"N": "2000"}},
            "ScanIndexForward": false}),
    );
    assert_eq!(
        lsi["Items"],
        json!([
            {"artist": {"S": "acme"}, "song": {"S": "a"}, "year": {"N": "1999"}},
            {"artist": {"S": "acme"}, "song": {"S": "c"}, "year": {"N": "1985"}},
        ])
    );

    let legacy = c.ok(
        "Query",
        json!({"TableName": "music", "KeyConditions": {
            "artist": {"ComparisonOperator": "EQ", "AttributeValueList": [{"S": "acme"}]},
            "song": {"ComparisonOperator": "BEGINS_WITH", "AttributeValueList": [{"S": "b"}]},
        }}),
    );
    assert_eq!(legacy["Count"], 1);

    let e = c.fail(
        "Query",
        json!({"TableName": "music", "KeyConditionExpression": "artist = :a",
            "ExpressionAttributeValues": {":a": {"N": "1"}}}),
    );
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("Condition parameter type does not match schema type")
    );
}

#[test]
fn transaction_cancellation_reasons_are_serialized() {
    let mut c = with_table();
    let e = c.fail(
        "TransactWriteItems",
        json!({"TransactItems": [
            {"ConditionCheck": {"TableName": "music", "Key": {"artist": {"S": "acme"}, "song": {"S": "a"}},
                "ConditionExpression": "plays > :z", "ExpressionAttributeValues": {":z": {"N": "0"}}}},
            {"Delete": {"TableName": "music", "Key": {"artist": {"S": "zed"}, "song": {"S": "a"}}}},
        ]}),
    );
    assert_eq!(e["__type"], "TransactionCanceledException");
    assert_eq!(
        e["CancellationReasons"],
        json!([{"Code": "ConditionalCheckFailed", "Message": "The conditional request failed"}, {"Code": "None"}])
    );
    assert_eq!(
        c.ok("Scan", json!({"TableName": "music", "Select": "COUNT"}))["Count"],
        4
    );
}

#[test]
fn state_is_per_region() {
    let mut c = with_table();
    c.region = "eu-west-1";
    c.fail("DescribeTable", json!({"TableName": "music"}));
    assert_eq!(c.ok("ListTables", json!({}))["TableNames"], json!([]));
    c.region = "us-east-1";
    assert_eq!(
        c.ok("ListTables", json!({}))["TableNames"],
        json!(["music"])
    );
}

#[test]
fn ttl_expires_items_lazily() {
    let mut c = with_table();
    c.ok(
        "UpdateTimeToLive",
        json!({"TableName": "music", "TimeToLiveSpecification": {"Enabled": true, "AttributeName": "exp"}}),
    );
    c.ok(
        "PutItem",
        json!({"TableName": "music", "Item": {"artist": {"S": "t"}, "song": {"S": "old"}, "exp": {"N": "1699999999"}}}),
    );
    c.ok(
        "PutItem",
        json!({"TableName": "music", "Item": {"artist": {"S": "t"}, "song": {"S": "new"}, "exp": {"N": "1800000000"}}}),
    );
    let q = c.ok(
        "Query",
        json!({"TableName": "music", "KeyConditionExpression": "artist = :t", "ExpressionAttributeValues": {":t": {"S": "t"}}}),
    );
    assert_eq!(q["Count"], 1);
    let ttl = c.ok("DescribeTimeToLive", json!({"TableName": "music"}));
    assert_eq!(ttl["TimeToLiveDescription"]["TimeToLiveStatus"], "ENABLED");
}

#[test]
fn backups_restore_items_into_a_new_table() {
    let mut c = with_table();
    let b = c.ok(
        "CreateBackup",
        json!({"TableName": "music", "BackupName": "nightly"}),
    );
    let arn = b["BackupDetails"]["BackupArn"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(arn.starts_with("arn:aws:dynamodb:us-east-1:000000000000:table/music/backup/"));
    c.ok(
        "DeleteItem",
        json!({"TableName": "music", "Key": {"artist": {"S": "zed"}, "song": {"S": "a"}}}),
    );
    c.ok(
        "RestoreTableFromBackup",
        json!({"TargetTableName": "music-restored", "BackupArn": arn}),
    );
    let d = c.ok("DescribeTable", json!({"TableName": "music-restored"}));
    assert_eq!(d["Table"]["ItemCount"], 4);
    assert_eq!(
        d["Table"]["RestoreSummary"]["SourceBackupArn"],
        arn.as_str()
    );
    let e = c.fail(
        "RestoreTableToPointInTime",
        json!({"SourceTableName": "music", "TargetTableName": "music-pitr"}),
    );
    assert_eq!(e["__type"], "PointInTimeRecoveryUnavailableException");
    let listed = c.ok("ListBackups", json!({"TableName": "music"}));
    assert_eq!(listed["BackupSummaries"][0]["BackupName"], "nightly");
}

#[test]
fn update_table_creates_and_deletes_global_indexes() {
    let mut c = with_table();
    let out = c.ok(
        "UpdateTable",
        json!({"TableName": "music",
            "AttributeDefinitions": [{"AttributeName": "genre", "AttributeType": "S"}],
            "GlobalSecondaryIndexUpdates": [{"Create": {"IndexName": "by-genre",
                "KeySchema": [{"AttributeName": "genre", "KeyType": "HASH"}],
                "Projection": {"ProjectionType": "INCLUDE", "NonKeyAttributes": ["plays"]}}}]}),
    );
    assert_eq!(out["TableDescription"]["TableStatus"], "UPDATING");
    c.ok(
        "PutItem",
        json!({"TableName": "music", "Item": {"artist": {"S": "g"}, "song": {"S": "x"}, "genre": {"S": "jazz"},
            "plays": {"N": "3"}, "year": {"N": "1"}, "secret": {"S": "s"}}}),
    );
    let e = c.fail(
        "PutItem",
        json!({"TableName": "music", "Item": {"artist": {"S": "g"}, "song": {"S": "y"}, "genre": {"N": "1"}}}),
    );
    assert!(
        e["message"]
            .as_str()
            .unwrap()
            .contains("Type mismatch for Index Key genre")
    );
    let q = c.ok(
        "Query",
        json!({"TableName": "music", "IndexName": "by-genre", "KeyConditionExpression": "genre = :g",
            "ExpressionAttributeValues": {":g": {"S": "jazz"}}}),
    );
    assert_eq!(
        q["Items"],
        json!([{"artist": {"S": "g"}, "song": {"S": "x"}, "genre": {"S": "jazz"}, "plays": {"N": "3"}}])
    );
    let e = c.fail(
        "Query",
        json!({"TableName": "music", "IndexName": "by-genre", "KeyConditionExpression": "genre = :g",
            "ExpressionAttributeValues": {":g": {"S": "jazz"}}, "ConsistentRead": true}),
    );
    assert!(e["message"].as_str().unwrap().contains("Consistent reads"));
    c.ok(
        "UpdateTable",
        json!({"TableName": "music", "GlobalSecondaryIndexUpdates": [{"Delete": {"IndexName": "by-genre"}}]}),
    );
    let d = c.ok("DescribeTable", json!({"TableName": "music"}));
    assert!(d["Table"].get("GlobalSecondaryIndexes").is_none());
    assert_eq!(
        d["Table"]["AttributeDefinitions"].as_array().unwrap().len(),
        3
    );
}

#[test]
fn batch_operations_validate_before_writing() {
    let mut c = with_table();
    let e = c.fail(
        "BatchWriteItem",
        json!({"RequestItems": {"music": [
            {"PutRequest": {"Item": {"artist": {"S": "new"}, "song": {"S": "1"}}}},
            {"PutRequest": {"Item": {"artist": {"S": "new"}}}},
        ]}}),
    );
    assert_eq!(e["__type"], "ValidationException");
    let got = c.ok(
        "BatchGetItem",
        json!({"RequestItems": {"music": {"AttributesToGet": ["year"], "Keys": [
            {"artist": {"S": "new"}, "song": {"S": "1"}},
            {"artist": {"S": "acme"}, "song": {"S": "b"}},
        ]}}}),
    );
    assert_eq!(got["Responses"]["music"], json!([{"year": {"N": "2001"}}]));
}
