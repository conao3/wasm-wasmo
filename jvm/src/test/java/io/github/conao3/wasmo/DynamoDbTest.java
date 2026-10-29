package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.services.dynamodb.DynamoDbClient;
import software.amazon.awssdk.services.dynamodb.model.AttributeDefinition;
import software.amazon.awssdk.services.dynamodb.model.AttributeValue;
import software.amazon.awssdk.services.dynamodb.model.BillingMode;
import software.amazon.awssdk.services.dynamodb.model.ConditionalCheckFailedException;
import software.amazon.awssdk.services.dynamodb.model.GlobalSecondaryIndex;
import software.amazon.awssdk.services.dynamodb.model.KeySchemaElement;
import software.amazon.awssdk.services.dynamodb.model.KeyType;
import software.amazon.awssdk.services.dynamodb.model.ProjectionType;
import software.amazon.awssdk.services.dynamodb.model.ResourceNotFoundException;
import software.amazon.awssdk.services.dynamodb.model.ReturnValuesOnConditionCheckFailure;
import software.amazon.awssdk.services.dynamodb.model.ScalarAttributeType;

class DynamoDbTest {
    static AttributeValue s(String v) {
        return AttributeValue.fromS(v);
    }

    static AttributeValue n(String v) {
        return AttributeValue.fromN(v);
    }

    static void createOrders(DynamoDbClient ddb) {
        ddb.createTable(
                r ->
                        r.tableName("orders")
                                .attributeDefinitions(
                                        attr("customer", ScalarAttributeType.S),
                                        attr("order", ScalarAttributeType.N),
                                        attr("status", ScalarAttributeType.S),
                                        attr("placedAt", ScalarAttributeType.S))
                                .keySchema(
                                        key("customer", KeyType.HASH), key("order", KeyType.RANGE))
                                .globalSecondaryIndexes(
                                        GlobalSecondaryIndex.builder()
                                                .indexName("by-status")
                                                .keySchema(
                                                        key("status", KeyType.HASH),
                                                        key("placedAt", KeyType.RANGE))
                                                .projection(
                                                        p ->
                                                                p.projectionType(
                                                                        ProjectionType.KEYS_ONLY))
                                                .build())
                                .billingMode(BillingMode.PAY_PER_REQUEST));
        ddb.waiter().waitUntilTableExists(r -> r.tableName("orders"));
    }

    static AttributeDefinition attr(String name, ScalarAttributeType type) {
        return AttributeDefinition.builder().attributeName(name).attributeType(type).build();
    }

    static KeySchemaElement key(String name, KeyType type) {
        return KeySchemaElement.builder().attributeName(name).keyType(type).build();
    }

    @Test
    void putGetQuery() {
        try (var mock = Wasmo.create()) {
            var ddb = Mocks.client(mock, DynamoDbClient.builder());
            createOrders(ddb);
            String[][] rows = {
                {"alice", "1", "shipped", "2026-01-01"},
                {"alice", "2", "pending", "2026-01-02"},
                {"alice", "3", "shipped", "2026-01-03"},
                {"bob", "1", "shipped", "2026-01-04"},
            };
            for (var row : rows) {
                ddb.putItem(
                        r ->
                                r.tableName("orders")
                                        .item(
                                                Map.of(
                                                        "customer", s(row[0]),
                                                        "order", n(row[1]),
                                                        "status", s(row[2]),
                                                        "placedAt", s(row[3]),
                                                        "total", n("10.50"))));
            }

            var item =
                    ddb.getItem(
                                    r ->
                                            r.tableName("orders")
                                                    .key(
                                                            Map.of(
                                                                    "customer",
                                                                    s("alice"),
                                                                    "order",
                                                                    n("2")))
                                                    .consistentRead(true))
                            .item();
            assertEquals("pending", item.get("status").s());
            assertEquals("10.5", item.get("total").n());

            var mine =
                    ddb.query(
                            r ->
                                    r.tableName("orders")
                                            .keyConditionExpression("customer = :c AND #o > :o")
                                            .expressionAttributeNames(Map.of("#o", "order"))
                                            .expressionAttributeValues(
                                                    Map.of(":c", s("alice"), ":o", n("1")))
                                            .scanIndexForward(false));
            assertEquals(
                    List.of("3", "2"), mine.items().stream().map(i -> i.get("order").n()).toList());

            var shipped =
                    ddb.query(
                            r ->
                                    r.tableName("orders")
                                            .indexName("by-status")
                                            .keyConditionExpression("#s = :s AND placedAt >= :p")
                                            .expressionAttributeNames(Map.of("#s", "status"))
                                            .expressionAttributeValues(
                                                    Map.of(
                                                            ":s",
                                                            s("shipped"),
                                                            ":p",
                                                            s("2026-01-02"))));
            assertEquals(
                    List.of("2026-01-03", "2026-01-04"),
                    shipped.items().stream().map(i -> i.get("placedAt").s()).toList());
            assertFalse(shipped.items().get(0).containsKey("total"));
        }
    }

    @Test
    void conditionalCheckFailure() {
        try (var mock = Wasmo.create()) {
            var ddb = Mocks.client(mock, DynamoDbClient.builder());
            createOrders(ddb);
            Runnable put =
                    () ->
                            ddb.putItem(
                                    r ->
                                            r.tableName("orders")
                                                    .item(
                                                            Map.of(
                                                                    "customer", s("alice"),
                                                                    "order", n("1"),
                                                                    "total", n("10.50")))
                                                    .conditionExpression(
                                                            "attribute_not_exists(customer)")
                                                    .returnValuesOnConditionCheckFailure(
                                                            ReturnValuesOnConditionCheckFailure
                                                                    .ALL_OLD));
            put.run();
            var e = assertThrows(ConditionalCheckFailedException.class, put::run);
            assertEquals(400, e.statusCode());
            assertEquals("10.5", e.item().get("total").n());
            assertThrows(
                    ResourceNotFoundException.class,
                    () -> ddb.getItem(r -> r.tableName("missing").key(Map.of("id", s("x")))));
        }
    }
}
