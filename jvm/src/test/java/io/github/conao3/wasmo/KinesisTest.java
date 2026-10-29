package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.charset.StandardCharsets;
import java.time.Duration;
import java.time.Instant;
import java.util.HashMap;
import java.util.Map;
import java.util.stream.IntStream;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.core.SdkBytes;
import software.amazon.awssdk.services.kinesis.KinesisClient;
import software.amazon.awssdk.services.kinesis.model.PutRecordsRequestEntry;
import software.amazon.awssdk.services.kinesis.model.ResourceNotFoundException;
import software.amazon.awssdk.services.kinesis.model.ShardIteratorType;

class KinesisTest {
    @Test
    void putRecordsAndGetRecords() {
        try (var mock = Wasmo.create()) {
            var kinesis = Mocks.client(mock, KinesisClient.builder());
            kinesis.createStream(r -> r.streamName("clicks").shardCount(2));
            kinesis.waiter().waitUntilStreamExists(r -> r.streamName("clicks"));
            var put =
                    kinesis.putRecords(
                            r ->
                                    r.streamName("clicks")
                                            .records(
                                                    IntStream.range(0, 10)
                                                            .mapToObj(
                                                                    i ->
                                                                            PutRecordsRequestEntry
                                                                                    .builder()
                                                                                    .partitionKey(
                                                                                            "user-"
                                                                                                    + i)
                                                                                    .data(
                                                                                            SdkBytes
                                                                                                    .fromUtf8String(
                                                                                                            "click-"
                                                                                                                    + i))
                                                                                    .build())
                                                            .toList()));
            assertEquals(0, put.failedRecordCount());
            assertEquals(10, put.records().size());

            var got = new HashMap<String, String>();
            for (var shard : kinesis.listShards(r -> r.streamName("clicks")).shards()) {
                var iterator =
                        kinesis.getShardIterator(
                                        r ->
                                                r.streamName("clicks")
                                                        .shardId(shard.shardId())
                                                        .shardIteratorType(
                                                                ShardIteratorType.TRIM_HORIZON))
                                .shardIterator();
                var out = kinesis.getRecords(r -> r.shardIterator(iterator));
                assertNotNull(out.nextShardIterator());
                for (var record : out.records()) {
                    assertTrue(
                            Duration.between(record.approximateArrivalTimestamp(), Instant.now())
                                            .abs()
                                            .toMinutes()
                                    < 1);
                    got.put(
                            record.partitionKey(),
                            new String(record.data().asByteArray(), StandardCharsets.UTF_8));
                }
            }
            var want = new HashMap<String, String>();
            IntStream.range(0, 10).forEach(i -> want.put("user-" + i, "click-" + i));
            assertEquals(Map.copyOf(want), Map.copyOf(got));
            assertThrows(
                    ResourceNotFoundException.class,
                    () -> kinesis.describeStream(r -> r.streamName("missing")));
        }
    }
}
