package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.services.dynamodb.DynamoDbClient;
import software.amazon.awssdk.services.dynamodb.model.AttributeValue;
import software.amazon.awssdk.services.dynamodb.model.BillingMode;
import software.amazon.awssdk.services.dynamodb.model.KeyType;
import software.amazon.awssdk.services.dynamodb.model.OperationType;
import software.amazon.awssdk.services.dynamodb.model.Record;
import software.amazon.awssdk.services.dynamodb.model.ScalarAttributeType;
import software.amazon.awssdk.services.dynamodb.model.ShardIteratorType;
import software.amazon.awssdk.services.dynamodb.model.Stream;
import software.amazon.awssdk.services.dynamodb.model.StreamViewType;
import software.amazon.awssdk.services.dynamodb.streams.DynamoDbStreamsClient;

class DynamoDbStreamsTest {
    @Test
    void recordsWrites() {
        try (var mock = Wasmo.create()) {
            var db = Mocks.client(mock, DynamoDbClient.builder());
            var streams = Mocks.client(mock, DynamoDbStreamsClient.builder());
            var arn =
                    db.createTable(
                                    r ->
                                            r.tableName("users")
                                                    .attributeDefinitions(
                                                            DynamoDbTest.attr(
                                                                    "id", ScalarAttributeType.S))
                                                    .keySchema(DynamoDbTest.key("id", KeyType.HASH))
                                                    .billingMode(BillingMode.PAY_PER_REQUEST)
                                                    .streamSpecification(
                                                            s ->
                                                                    s.streamEnabled(true)
                                                                            .streamViewType(
                                                                                    StreamViewType
                                                                                            .NEW_AND_OLD_IMAGES)))
                            .tableDescription()
                            .latestStreamArn();
            var key = Map.of("id", AttributeValue.fromS("u1"));
            db.putItem(
                    r ->
                            r.tableName("users")
                                    .item(
                                            Map.of(
                                                    "id",
                                                    AttributeValue.fromS("u1"),
                                                    "name",
                                                    AttributeValue.fromS("ada"))));
            db.updateItem(
                    r ->
                            r.tableName("users")
                                    .key(key)
                                    .updateExpression("SET #n = :n")
                                    .expressionAttributeNames(Map.of("#n", "name"))
                                    .expressionAttributeValues(
                                            Map.of(":n", AttributeValue.fromS("grace"))));
            db.deleteItem(r -> r.tableName("users").key(key));

            assertEquals(
                    List.of(arn),
                    streams.listStreams(r -> r.tableName("users")).streams().stream()
                            .map(Stream::streamArn)
                            .toList());
            var shard =
                    streams.describeStream(r -> r.streamArn(arn))
                            .streamDescription()
                            .shards()
                            .get(0);
            var iterator =
                    streams.getShardIterator(
                                    r ->
                                            r.streamArn(arn)
                                                    .shardId(shard.shardId())
                                                    .shardIteratorType(
                                                            ShardIteratorType.TRIM_HORIZON))
                            .shardIterator();
            var records = streams.getRecords(r -> r.shardIterator(iterator)).records();
            assertEquals(
                    List.of(OperationType.INSERT, OperationType.MODIFY, OperationType.REMOVE),
                    records.stream().map(Record::eventName).toList());
            assertEquals("ada", records.get(0).dynamodb().newImage().get("name").s());
            assertEquals("ada", records.get(1).dynamodb().oldImage().get("name").s());
            assertEquals("grace", records.get(1).dynamodb().newImage().get("name").s());
            assertTrue(records.get(2).dynamodb().newImage().isEmpty());
        }
    }
}
