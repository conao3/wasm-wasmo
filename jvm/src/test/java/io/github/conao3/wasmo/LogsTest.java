package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.util.List;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.services.cloudwatchlogs.CloudWatchLogsClient;
import software.amazon.awssdk.services.cloudwatchlogs.model.FilteredLogEvent;
import software.amazon.awssdk.services.cloudwatchlogs.model.InputLogEvent;
import software.amazon.awssdk.services.cloudwatchlogs.model.ResourceNotFoundException;

class LogsTest {
    static InputLogEvent event(long timestamp, String message) {
        return InputLogEvent.builder().timestamp(timestamp).message(message).build();
    }

    @Test
    void putLogEventsAndFilterLogEvents() {
        try (var mock = Wasmo.create()) {
            var logs = Mocks.client(mock, CloudWatchLogsClient.builder());
            var group = "/app/api";
            logs.createLogGroup(r -> r.logGroupName(group));
            for (var stream : List.of("web-1", "web-2")) {
                logs.createLogStream(r -> r.logGroupName(group).logStreamName(stream));
            }
            long now = System.currentTimeMillis();
            logs.putLogEvents(
                    r ->
                            r.logGroupName(group)
                                    .logStreamName("web-1")
                                    .logEvents(
                                            event(now, "GET /health 200"),
                                            event(now + 1, "GET /orders 500")));
            logs.putLogEvents(
                    r ->
                            r.logGroupName(group)
                                    .logStreamName("web-2")
                                    .logEvents(event(now + 2, "POST /orders 500")));

            var errors =
                    logs.filterLogEvents(r -> r.logGroupName(group).filterPattern("\"500\""))
                            .events();
            assertEquals(
                    List.of(
                            List.of("web-1", "GET /orders 500"),
                            List.of("web-2", "POST /orders 500")),
                    errors.stream().map(e -> List.of(e.logStreamName(), e.message())).toList());
            var one =
                    logs.filterLogEvents(r -> r.logGroupName(group).logStreamNames("web-1"))
                            .events();
            assertEquals(
                    List.of("GET /health 200", "GET /orders 500"),
                    one.stream().map(FilteredLogEvent::message).toList());
            assertThrows(
                    ResourceNotFoundException.class,
                    () ->
                            logs.putLogEvents(
                                    r ->
                                            r.logGroupName(group)
                                                    .logStreamName("missing")
                                                    .logEvents(event(now, "x"))));
        }
    }
}
