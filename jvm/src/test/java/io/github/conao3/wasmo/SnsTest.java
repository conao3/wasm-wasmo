package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.util.List;
import java.util.Map;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.services.sns.SnsClient;
import software.amazon.awssdk.services.sns.model.MessageAttributeValue;
import software.amazon.awssdk.services.sqs.SqsClient;
import software.amazon.awssdk.services.sqs.model.Message;
import software.amazon.awssdk.services.sqs.model.QueueAttributeName;

class SnsTest {
    static String queueArn(SqsClient sqs, String url) {
        return sqs.getQueueAttributes(
                        r -> r.queueUrl(url).attributeNames(QueueAttributeName.QUEUE_ARN))
                .attributes()
                .get(QueueAttributeName.QUEUE_ARN);
    }

    @Test
    void deliversToSqsThroughAFilterPolicy() {
        try (var mock = Wasmo.create()) {
            var sns = Mocks.client(mock, SnsClient.builder());
            var sqs = Mocks.client(mock, SqsClient.builder());
            var topic = sns.createTopic(r -> r.name("orders")).topicArn();
            var all = sqs.createQueue(r -> r.queueName("all")).queueUrl();
            var paid = sqs.createQueue(r -> r.queueName("paid")).queueUrl();
            sns.subscribe(r -> r.topicArn(topic).protocol("sqs").endpoint(queueArn(sqs, all)));
            sns.subscribe(
                    r ->
                            r.topicArn(topic)
                                    .protocol("sqs")
                                    .endpoint(queueArn(sqs, paid))
                                    .attributes(
                                            Map.of(
                                                    "RawMessageDelivery", "true",
                                                    "FilterPolicy", "{\"kind\":[\"paid\"]}")));
            for (var kind : List.of("paid", "refunded")) {
                sns.publish(
                        r ->
                                r.topicArn(topic)
                                        .message(kind)
                                        .subject("order")
                                        .messageAttributes(
                                                Map.of(
                                                        "kind",
                                                        MessageAttributeValue.builder()
                                                                .dataType("String")
                                                                .stringValue(kind)
                                                                .build())));
            }

            var envelopes =
                    sqs.receiveMessage(r -> r.queueUrl(all).maxNumberOfMessages(10)).messages();
            assertEquals(2, envelopes.size());
            for (var m : envelopes) {
                var e = Json.parseObject(m.body());
                assertEquals("Notification", e.get("Type"));
                assertEquals(topic, e.get("TopicArn"));
            }
            var raw =
                    sqs.receiveMessage(
                                    r ->
                                            r.queueUrl(paid)
                                                    .maxNumberOfMessages(10)
                                                    .messageAttributeNames("All"))
                            .messages();
            assertEquals(List.of("paid"), raw.stream().map(Message::body).toList());
            assertEquals("paid", raw.get(0).messageAttributes().get("kind").stringValue());
        }
    }
}
