import boto3


def test_put_records_and_get_records(aws):
    kinesis = boto3.client("kinesis")
    kinesis.create_stream(StreamName="clicks", ShardCount=2)
    kinesis.get_waiter("stream_exists").wait(
        StreamName="clicks", WaiterConfig={"Delay": 0, "MaxAttempts": 5}
    )
    put = kinesis.put_records(
        StreamName="clicks",
        Records=[{"PartitionKey": f"user-{i}", "Data": f"click-{i}".encode()} for i in range(10)],
    )
    assert put["FailedRecordCount"] == 0
    assert len(put["Records"]) == 10

    got = {}
    for shard in kinesis.list_shards(StreamName="clicks")["Shards"]:
        iterator = kinesis.get_shard_iterator(
            StreamName="clicks", ShardId=shard["ShardId"], ShardIteratorType="TRIM_HORIZON"
        )["ShardIterator"]
        out = kinesis.get_records(ShardIterator=iterator)
        assert out["NextShardIterator"]
        for record in out["Records"]:
            assert record["ApproximateArrivalTimestamp"]
            got[record["PartitionKey"]] = record["Data"]
    assert got == {f"user-{i}": f"click-{i}".encode() for i in range(10)}
