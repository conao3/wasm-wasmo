package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNull;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.nio.charset.StandardCharsets;
import java.util.Base64;
import java.util.Map;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.core.SdkBytes;
import software.amazon.awssdk.services.lambda.LambdaClient;
import software.amazon.awssdk.services.lambda.model.LogType;
import software.amazon.awssdk.services.lambda.model.Runtime;
import software.amazon.awssdk.services.sqs.SqsClient;

class LambdaTest {
    private static void createFunction(LambdaClient lambda, String name) {
        lambda.createFunction(
                r ->
                        r.functionName(name)
                                .runtime(Runtime.JAVA21)
                                .role("arn:aws:iam::000000000000:role/lambda")
                                .handler("example.Handler")
                                .code(c -> c.zipFile(SdkBytes.fromUtf8String("PK fake zip")))
                                .environment(e -> e.variables(Map.of("GREETING", "hello"))));
    }

    @Test
    void invokeRunsTheRegisteredHandler() {
        try (var mock = Wasmo.create()) {
            var lambda = Mocks.client(mock, LambdaClient.builder());
            var sqs = Mocks.client(mock, SqsClient.builder());
            var url = sqs.createQueue(r -> r.queueName("seen")).queueUrl();
            createFunction(lambda, "greet");
            mock.registerFunction(
                    "greet",
                    (event, context) -> {
                        context.log("event " + event);
                        sqs.sendMessage(r -> r.queueUrl(url).messageBody(event));
                        return "\""
                                + context.getEnvironment().get("GREETING")
                                + " from "
                                + context.getFunctionName()
                                + "\"";
                    });

            var out =
                    lambda.invoke(
                            r ->
                                    r.functionName("greet")
                                            .payload(SdkBytes.fromUtf8String("{\"n\":1}"))
                                            .logType(LogType.TAIL));
            assertEquals(200, out.statusCode());
            assertNull(out.functionError());
            assertEquals("\"hello from greet\"", out.payload().asUtf8String());
            var log =
                    new String(Base64.getDecoder().decode(out.logResult()), StandardCharsets.UTF_8);
            assertTrue(log.contains("event {\"n\":1}\nEND RequestId"), log);
            assertEquals(
                    "{\"n\":1}", sqs.receiveMessage(r -> r.queueUrl(url)).messages().get(0).body());
        }
    }

    @Test
    void exceptionsAreFunctionErrors() {
        try (var mock = Wasmo.create()) {
            var lambda = Mocks.client(mock, LambdaClient.builder());
            createFunction(lambda, "boom");
            mock.registerFunction(
                    "boom",
                    (event, context) -> {
                        throw new IllegalArgumentException("bad input");
                    });
            var out = lambda.invoke(r -> r.functionName("boom"));
            assertEquals("Unhandled", out.functionError());
            var body = out.payload().asUtf8String();
            assertTrue(
                    body.startsWith(
                            "{\"errorMessage\":\"bad input\",\"errorType\":\"java.lang.IllegalArgumentException\",\"stackTrace\":["),
                    body);
        }
    }
}
