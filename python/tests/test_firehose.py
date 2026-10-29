import boto3


def test_delivers_to_s3(aws):
    s3 = boto3.client("s3")
    firehose = boto3.client("firehose")
    s3.create_bucket(Bucket="lake")
    firehose.create_delivery_stream(
        DeliveryStreamName="events",
        ExtendedS3DestinationConfiguration={
            "BucketARN": "arn:aws:s3:::lake",
            "RoleARN": "arn:aws:iam::000000000000:role/firehose",
            "Prefix": "raw/",
        },
    )
    firehose.put_record(DeliveryStreamName="events", Record={"Data": b"one\n"})
    batch = firehose.put_record_batch(
        DeliveryStreamName="events", Records=[{"Data": b"two\n"}, {"Data": b"three\n"}]
    )
    assert batch["FailedPutCount"] == 0

    keys = [o["Key"] for o in s3.list_objects_v2(Bucket="lake", Prefix="raw/")["Contents"]]
    assert len(keys) == 2
    bodies = sorted(s3.get_object(Bucket="lake", Key=k)["Body"].read() for k in keys)
    assert bodies == [b"one\n", b"two\nthree\n"]
    assert all(k.split("/")[-1].startswith("events-1-") for k in keys)
