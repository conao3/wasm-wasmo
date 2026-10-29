package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotNull;

import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.util.HexFormat;
import java.util.Map;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.core.SdkBytes;
import software.amazon.awssdk.services.sqs.SqsClient;
import software.amazon.awssdk.services.sqs.model.MessageAttributeValue;
import software.amazon.awssdk.services.sqs.model.MessageSystemAttributeName;

class SqsTest {
    @Test
    void sendReceiveWithAttributes() throws Exception {
        try (var mock = Wasmo.create()) {
            var sqs = Mocks.client(mock, SqsClient.builder());
            var url = sqs.createQueue(r -> r.queueName("jobs")).queueUrl();
            byte[] bytes = {0, 1, 2, (byte) 0xff};
            var sent =
                    sqs.sendMessage(
                            r ->
                                    r.queueUrl(url)
                                            .messageBody("hello 日本語")
                                            .messageAttributes(
                                                    Map.of(
                                                            "kind",
                                                            MessageAttributeValue.builder()
                                                                    .dataType("String")
                                                                    .stringValue("greeting")
                                                                    .build(),
                                                            "bytes",
                                                            MessageAttributeValue.builder()
                                                                    .dataType("Binary")
                                                                    .binaryValue(
                                                                            SdkBytes.fromByteArray(
                                                                                    bytes))
                                                                    .build())));
            assertEquals(
                    HexFormat.of()
                            .formatHex(
                                    MessageDigest.getInstance("MD5")
                                            .digest("hello 日本語".getBytes(StandardCharsets.UTF_8))),
                    sent.md5OfMessageBody());

            var got =
                    sqs.receiveMessage(
                                    r ->
                                            r.queueUrl(url)
                                                    .messageAttributeNames("All")
                                                    .messageSystemAttributeNames(
                                                            MessageSystemAttributeName.ALL))
                            .messages();
            assertEquals(1, got.size());
            var m = got.get(0);
            assertEquals("hello 日本語", m.body());
            assertEquals("greeting", m.messageAttributes().get("kind").stringValue());
            assertArrayEquals(
                    bytes, m.messageAttributes().get("bytes").binaryValue().asByteArray());
            assertEquals("1", m.attributesAsStrings().get("ApproximateReceiveCount"));
            assertNotNull(m.md5OfMessageAttributes());
            assertEquals(sent.md5OfMessageAttributes(), m.md5OfMessageAttributes());
            assertEquals(sent.md5OfMessageBody(), m.md5OfBody());

            assertEquals(0, sqs.receiveMessage(r -> r.queueUrl(url)).messages().size());
            sqs.deleteMessage(r -> r.queueUrl(url).receiptHandle(m.receiptHandle()));
            var attrs =
                    sqs.getQueueAttributes(r -> r.queueUrl(url).attributeNamesWithStrings("All"))
                            .attributesAsStrings();
            assertEquals("0", attrs.get("ApproximateNumberOfMessages"));
            assertEquals("0", attrs.get("ApproximateNumberOfMessagesNotVisible"));
        }
    }
}
