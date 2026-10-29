package io.github.conao3.wasmo;

import java.util.Arrays;
import java.util.Map;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.core.sync.RequestBody;
import software.amazon.awssdk.services.dynamodb.DynamoDbClient;
import software.amazon.awssdk.services.dynamodb.model.AttributeValue;
import software.amazon.awssdk.services.dynamodb.model.KeyType;
import software.amazon.awssdk.services.dynamodb.model.ScalarAttributeType;
import software.amazon.awssdk.services.s3.S3Client;

class LatencyTest {
    static final int WARMUP = 200;
    static final int RUNS = 500;

    static double medianMs(Runnable op) {
        for (int i = 0; i < WARMUP; i++) {
            op.run();
        }
        var samples = new long[RUNS];
        for (int i = 0; i < RUNS; i++) {
            long start = System.nanoTime();
            op.run();
            samples[i] = System.nanoTime() - start;
        }
        Arrays.sort(samples);
        return samples[RUNS / 2] / 1e6;
    }

    static void report(String name, double ms) {
        System.out.printf("wasmo latency %-24s median %.3f ms%n", name, ms);
    }

    @Test
    void printMedians() {
        report("Wasmo.create", medianMs(() -> Wasmo.create().close()));
        try (var mock = Wasmo.create()) {
            var ddb = Mocks.client(mock, DynamoDbClient.builder());
            ddb.createTable(
                    r ->
                            r.tableName("bench")
                                    .attributeDefinitions(
                                            DynamoDbTest.attr("id", ScalarAttributeType.S))
                                    .keySchema(DynamoDbTest.key("id", KeyType.HASH))
                                    .billingMode("PAY_PER_REQUEST"));
            var item =
                    Map.of(
                            "id",
                            AttributeValue.fromS("k"),
                            "v",
                            AttributeValue.fromS("x".repeat(100)));
            var key = Map.of("id", AttributeValue.fromS("k"));
            report(
                    "DynamoDB PutItem",
                    medianMs(() -> ddb.putItem(r -> r.tableName("bench").item(item))));
            report(
                    "DynamoDB GetItem",
                    medianMs(() -> ddb.getItem(r -> r.tableName("bench").key(key))));

            var s3 = Mocks.client(mock, S3Client.builder().forcePathStyle(true));
            s3.createBucket(r -> r.bucket("bench"));
            var data = new byte[1024];
            report(
                    "S3 PutObject 1 KiB",
                    medianMs(
                            () ->
                                    s3.putObject(
                                            r -> r.bucket("bench").key("k"),
                                            RequestBody.fromBytes(data))));
            report(
                    "S3 GetObject 1 KiB",
                    medianMs(() -> s3.getObjectAsBytes(r -> r.bucket("bench").key("k"))));
        }
    }
}
