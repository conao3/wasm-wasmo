package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.concurrent.atomic.AtomicInteger;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.core.SdkBytes;
import software.amazon.awssdk.services.lambda.LambdaClient;
import software.amazon.awssdk.services.lambda.model.Runtime;
import software.amazon.awssdk.services.sfn.SfnClient;
import software.amazon.awssdk.services.sfn.model.ExecutionStatus;
import software.amazon.awssdk.services.sfn.model.StateMachineType;
import software.amazon.awssdk.services.sfn.model.SyncExecutionStatus;

class StepFunctionsTest {
    private static final String ROLE = "arn:aws:iam::000000000000:role/sfn";

    private static void createFunction(LambdaClient lambda, String name) {
        lambda.createFunction(
                r ->
                        r.functionName(name)
                                .runtime(Runtime.JAVA21)
                                .role("arn:aws:iam::000000000000:role/lambda")
                                .handler("example.Handler")
                                .code(c -> c.zipFile(SdkBytes.fromUtf8String("PK fake zip"))));
    }

    @Test
    void stateMachineRetriesAndCatchesHandlerErrors() {
        try (var mock = Wasmo.create()) {
            var lambda = Mocks.client(mock, LambdaClient.builder());
            var sfn = Mocks.client(mock, SfnClient.builder());
            createFunction(lambda, "flaky");
            createFunction(lambda, "broken");
            var calls = new AtomicInteger();
            mock.registerFunction(
                    "flaky",
                    (event, context) -> {
                        if (calls.incrementAndGet() < 2) {
                            throw new IllegalStateException("not yet");
                        }
                        return "{\"doubled\":42}";
                    });
            mock.registerFunction(
                    "broken",
                    (event, context) -> {
                        throw new IllegalArgumentException("always");
                    });
            var definition =
                    """
                    {"StartAt": "Flaky", "States": {
                      "Flaky": {"Type": "Task", "Resource": "arn:aws:states:::lambda:invoke",
                        "Parameters": {"FunctionName": "flaky", "Payload.$": "$"},
                        "ResultSelector": {"doubled.$": "$.Payload.doubled"},
                        "ResultPath": "$.result",
                        "Retry": [{"ErrorEquals": ["java.lang.IllegalStateException"], "MaxAttempts": 2}],
                        "Next": "Broken"},
                      "Broken": {"Type": "Task", "Resource": "arn:aws:states:::lambda:invoke",
                        "Parameters": {"FunctionName": "broken", "Payload": {}},
                        "Catch": [{"ErrorEquals": ["States.TaskFailed"], "ResultPath": "$.caught", "Next": "Done"}],
                        "Next": "Done"},
                      "Done": {"Type": "Succeed"}}}
                    """;
            var arn =
                    sfn.createStateMachine(
                                    r -> r.name("retry-catch").roleArn(ROLE).definition(definition))
                            .stateMachineArn();
            var execution =
                    sfn.startExecution(r -> r.stateMachineArn(arn).input("{\"n\":21}"))
                            .executionArn();
            var d = sfn.describeExecution(r -> r.executionArn(execution));
            assertEquals(ExecutionStatus.SUCCEEDED, d.status());
            assertTrue(d.output().contains("\"result\":{\"doubled\":42}"), d.output());
            assertTrue(
                    d.output().contains("\"Error\":\"java.lang.IllegalArgumentException\""),
                    d.output());
            assertEquals(2, calls.get());
        }
    }

    @Test
    void syncExecutionWaitsForTheHandler() {
        try (var mock = Wasmo.create()) {
            var lambda = Mocks.client(mock, LambdaClient.builder());
            var sfn = Mocks.client(mock, SfnClient.builder());
            createFunction(lambda, "greet");
            mock.registerFunction("greet", (event, context) -> "\"hello\"");
            var definition =
                    """
                    {"StartAt": "Greet", "States": {
                      "Greet": {"Type": "Task", "Resource": "arn:aws:states:::lambda:invoke",
                        "Parameters": {"FunctionName": "greet", "Payload.$": "$"},
                        "OutputPath": "$.Payload", "End": true}}}
                    """;
            var arn =
                    sfn.createStateMachine(
                                    r ->
                                            r.name("sync")
                                                    .roleArn(ROLE)
                                                    .type(StateMachineType.EXPRESS)
                                                    .definition(definition))
                            .stateMachineArn();
            var out = sfn.startSyncExecution(r -> r.stateMachineArn(arn).input("{}"));
            assertEquals(SyncExecutionStatus.SUCCEEDED, out.status());
            assertEquals("\"hello\"", out.output());
        }
    }
}
