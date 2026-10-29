package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.services.eventbridge.EventBridgeClient;
import software.amazon.awssdk.services.eventbridge.model.PutEventsRequestEntry;
import software.amazon.awssdk.services.eventbridge.model.Target;
import software.amazon.awssdk.services.sqs.SqsClient;

class EventBridgeTest {
    @Test
    void putEventsReachesAnSqsTarget() {
        try (var mock = Wasmo.create()) {
            var events = Mocks.client(mock, EventBridgeClient.builder());
            var sqs = Mocks.client(mock, SqsClient.builder());
            var url = sqs.createQueue(r -> r.queueName("orders")).queueUrl();
            var arn = SnsTest.queueArn(sqs, url);
            events.putRule(
                    r ->
                            r.name("big-orders")
                                    .eventPattern(
                                            "{\"source\":[\"shop\"],\"detail\":{\"total\":[{\"numeric\":[\">=\",100]}]}}"));
            events.putTargets(
                    r ->
                            r.rule("big-orders")
                                    .targets(Target.builder().id("queue").arn(arn).build()));
            var put =
                    events.putEvents(
                            r ->
                                    r.entries(
                                            List.of(150, 20).stream()
                                                    .map(
                                                            total ->
                                                                    PutEventsRequestEntry.builder()
                                                                            .source("shop")
                                                                            .detailType(
                                                                                    "Order Placed")
                                                                            .detail(
                                                                                    "{\"total\":"
                                                                                            + total
                                                                                            + "}")
                                                                            .build())
                                                    .toList()));
            assertEquals(0, put.failedEntryCount());

            var messages =
                    sqs.receiveMessage(r -> r.queueUrl(url).maxNumberOfMessages(10)).messages();
            assertEquals(1, messages.size());
            var event = Json.parseObject(messages.get(0).body());
            assertEquals(put.entries().get(0).eventId(), event.get("id"));
            assertEquals("shop", event.get("source"));
            assertEquals("Order Placed", event.get("detail-type"));
            assertEquals(Map.of("total", 150L), event.get("detail"));
        }
    }
}
