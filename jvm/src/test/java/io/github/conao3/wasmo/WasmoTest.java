package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.util.ArrayList;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.services.sqs.SqsClient;
import software.amazon.awssdk.services.sqs.model.QueueDoesNotExistException;
import software.amazon.awssdk.services.sts.StsClient;

class WasmoTest {
    @Test
    void getCallerIdentity() {
        try (var mock = Wasmo.create()) {
            var sts = Mocks.client(mock, StsClient.builder());
            assertEquals("000000000000", sts.getCallerIdentity().account());
        }
    }

    @Test
    void instancesAreIsolated() {
        try (var a = Wasmo.create();
                var b = Wasmo.create()) {
            var sqsA = Mocks.client(a, SqsClient.builder());
            var sqsB = Mocks.client(b, SqsClient.builder());
            sqsA.createQueue(r -> r.queueName("only-in-a"));
            assertThrows(
                    QueueDoesNotExistException.class,
                    () -> sqsB.getQueueUrl(r -> r.queueName("only-in-a")));
            assertEquals(1, sqsA.listQueues().queueUrls().size());
            assertEquals(0, sqsB.listQueues().queueUrls().size());
        }
    }

    @Test
    void sharedInstanceAcrossThreads() throws Exception {
        ExecutorService pool = Executors.newFixedThreadPool(8);
        try (var mock = Wasmo.create()) {
            var sqs = Mocks.client(mock, SqsClient.builder());
            var url = sqs.createQueue(r -> r.queueName("shared")).queueUrl();
            var futures = new ArrayList<Future<?>>();
            for (int t = 0; t < 8; t++) {
                futures.add(
                        pool.submit(
                                () -> {
                                    for (int i = 0; i < 25; i++) {
                                        sqs.sendMessage(r -> r.queueUrl(url).messageBody("m"));
                                    }
                                }));
            }
            for (var f : futures) {
                f.get();
            }
            var attrs =
                    sqs.getQueueAttributes(r -> r.queueUrl(url).attributeNamesWithStrings("All"))
                            .attributesAsStrings();
            assertEquals("200", attrs.get("ApproximateNumberOfMessages"));
        } finally {
            pool.shutdown();
        }
    }

    @Test
    void parallelInstances() throws Exception {
        ExecutorService pool = Executors.newFixedThreadPool(8);
        try {
            var futures = new ArrayList<Future<?>>();
            for (int t = 0; t < 8; t++) {
                futures.add(
                        pool.submit(
                                () -> {
                                    try (var mock = Wasmo.create()) {
                                        var sts = Mocks.client(mock, StsClient.builder());
                                        for (int i = 0; i < 20; i++) {
                                            sts.getCallerIdentity();
                                        }
                                    }
                                }));
            }
            for (var f : futures) {
                f.get();
            }
        } finally {
            pool.shutdown();
        }
    }

    @Test
    void closedInstanceRejectsRequests() {
        var mock = Wasmo.create();
        var sts = Mocks.client(mock, StsClient.builder());
        mock.close();
        assertThrows(Exception.class, sts::getCallerIdentity);
    }
}
