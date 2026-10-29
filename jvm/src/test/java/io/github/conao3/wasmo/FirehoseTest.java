package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.core.SdkBytes;
import software.amazon.awssdk.services.firehose.FirehoseClient;
import software.amazon.awssdk.services.firehose.model.Record;
import software.amazon.awssdk.services.s3.S3Client;
import software.amazon.awssdk.services.s3.model.S3Object;

class FirehoseTest {
    static Record record(String data) {
        return Record.builder().data(SdkBytes.fromUtf8String(data)).build();
    }

    @Test
    void deliversPutBatchesToS3() {
        try (var mock = Wasmo.create()) {
            var firehose = Mocks.client(mock, FirehoseClient.builder());
            var s3 = Mocks.client(mock, S3Client.builder().forcePathStyle(true));
            s3.createBucket(r -> r.bucket("lake"));
            firehose.createDeliveryStream(
                    r ->
                            r.deliveryStreamName("events")
                                    .extendedS3DestinationConfiguration(
                                            d ->
                                                    d.bucketARN("arn:aws:s3:::lake")
                                                            .roleARN(
                                                                    "arn:aws:iam::000000000000:role/firehose")
                                                            .prefix("raw/")));
            firehose.putRecord(r -> r.deliveryStreamName("events").record(record("one\n")));
            var batch =
                    firehose.putRecordBatch(
                            r ->
                                    r.deliveryStreamName("events")
                                            .records(record("two\n"), record("three\n")));
            assertEquals(0, batch.failedPutCount());

            var keys =
                    s3.listObjectsV2(r -> r.bucket("lake").prefix("raw/")).contents().stream()
                            .map(S3Object::key)
                            .toList();
            assertEquals(2, keys.size());
            assertTrue(
                    keys.stream()
                            .allMatch(
                                    k ->
                                            k.substring(k.lastIndexOf('/') + 1)
                                                    .startsWith("events-1-")));
            assertEquals(
                    List.of("one\n", "two\nthree\n"),
                    keys.stream()
                            .map(
                                    k ->
                                            s3.getObjectAsBytes(r -> r.bucket("lake").key(k))
                                                    .asUtf8String())
                            .sorted()
                            .toList());
        }
    }
}
