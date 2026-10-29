package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.time.Duration;
import java.time.Instant;
import java.time.temporal.ChronoUnit;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.core.interceptor.Context;
import software.amazon.awssdk.core.interceptor.ExecutionAttributes;
import software.amazon.awssdk.core.interceptor.ExecutionInterceptor;
import software.amazon.awssdk.services.cloudwatch.CloudWatchClient;
import software.amazon.awssdk.services.cloudwatch.model.Dimension;
import software.amazon.awssdk.services.cloudwatch.model.InvalidParameterValueException;
import software.amazon.awssdk.services.cloudwatch.model.Metric;
import software.amazon.awssdk.services.cloudwatch.model.MetricDataQuery;
import software.amazon.awssdk.services.cloudwatch.model.MetricDataResult;
import software.amazon.awssdk.services.cloudwatch.model.MetricDatum;
import software.amazon.awssdk.services.cloudwatch.model.ScanBy;
import software.amazon.awssdk.services.cloudwatch.model.StandardUnit;
import software.amazon.awssdk.services.cloudwatch.model.Statistic;

class CloudWatchTest {
    @Test
    void putMetricDataAndGetMetricData() {
        try (var mock = Wasmo.create()) {
            var cw = Mocks.client(mock, CloudWatchClient.builder());
            var base = Instant.now().truncatedTo(ChronoUnit.HOURS).minus(Duration.ofHours(1));
            var later = base.plusSeconds(60);
            var dims = Dimension.builder().name("Service").value("web").build();
            var data = new ArrayList<MetricDatum>();
            for (var v : List.of(10.0, 20.0, 30.0)) {
                data.add(
                        MetricDatum.builder()
                                .metricName("Latency")
                                .dimensions(dims)
                                .timestamp(base)
                                .value(v)
                                .build());
            }
            data.add(
                    MetricDatum.builder()
                            .metricName("Latency")
                            .dimensions(dims)
                            .timestamp(later)
                            .value(100.0)
                            .unit(StandardUnit.MILLISECONDS)
                            .build());
            cw.putMetricData(r -> r.namespace("App").metricData(data));

            var out =
                    cw.getMetricData(
                            r ->
                                    r.startTime(base)
                                            .endTime(base.plus(Duration.ofHours(1)))
                                            .scanBy(ScanBy.TIMESTAMP_ASCENDING)
                                            .metricDataQueries(
                                                    MetricDataQuery.builder()
                                                            .id("sum")
                                                            .metricStat(
                                                                    s ->
                                                                            s.metric(
                                                                                            Metric
                                                                                                    .builder()
                                                                                                    .namespace(
                                                                                                            "App")
                                                                                                    .metricName(
                                                                                                            "Latency")
                                                                                                    .dimensions(
                                                                                                            dims)
                                                                                                    .build())
                                                                                    .period(60)
                                                                                    .stat("Sum"))
                                                            .build(),
                                                    MetricDataQuery.builder()
                                                            .id("doubled")
                                                            .expression("sum * 2")
                                                            .label("doubled")
                                                            .build()));
            Map<String, MetricDataResult> results =
                    out.metricDataResults().stream()
                            .collect(Collectors.toMap(MetricDataResult::id, r -> r));
            assertEquals(List.of(60.0, 100.0), results.get("sum").values());
            assertEquals(List.of(base, later), results.get("sum").timestamps());
            assertEquals(List.of(120.0, 200.0), results.get("doubled").values());
            assertEquals("doubled", results.get("doubled").label());
            assertEquals(
                    List.of("Latency"),
                    cw.listMetrics(r -> r.namespace("App")).metrics().stream()
                            .map(Metric::metricName)
                            .toList());
            assertThrows(
                    InvalidParameterValueException.class,
                    () ->
                            cw.putMetricData(
                                    r ->
                                            r.namespace("AWS/EC2")
                                                    .metricData(
                                                            MetricDatum.builder()
                                                                    .metricName("x")
                                                                    .value(1.0)
                                                                    .build())));
        }
    }

    @Test
    void largePutMetricDataIsGzipped() {
        var encodings = new ArrayList<String>();
        ExecutionInterceptor record =
                new ExecutionInterceptor() {
                    @Override
                    public void beforeTransmission(
                            Context.BeforeTransmission context, ExecutionAttributes attributes) {
                        encodings.add(
                                context.httpRequest()
                                        .firstMatchingHeader("Content-Encoding")
                                        .orElse(null));
                    }
                };
        try (var mock = Wasmo.create()) {
            var cw =
                    Mocks.client(
                            mock,
                            CloudWatchClient.builder()
                                    .overrideConfiguration(o -> o.addExecutionInterceptor(record)));
            var at = Instant.now().truncatedTo(ChronoUnit.HOURS);
            var data = new ArrayList<MetricDatum>();
            for (int i = 1; i <= 500; i++) {
                data.add(
                        MetricDatum.builder()
                                .metricName("Requests")
                                .dimensions(
                                        Dimension.builder()
                                                .name("Host")
                                                .value("host-" + i % 10)
                                                .build())
                                .timestamp(at)
                                .value((double) i)
                                .build());
            }
            cw.putMetricData(r -> r.namespace("Bulk").metricData(data));
            assertEquals(List.of("gzip"), encodings);

            var stats =
                    cw.getMetricStatistics(
                                    r ->
                                            r.namespace("Bulk")
                                                    .metricName("Requests")
                                                    .dimensions(
                                                            Dimension.builder()
                                                                    .name("Host")
                                                                    .value("host-0")
                                                                    .build())
                                                    .startTime(at)
                                                    .endTime(at.plus(Duration.ofHours(1)))
                                                    .period(3600)
                                                    .statistics(
                                                            Statistic.SUM, Statistic.SAMPLE_COUNT))
                            .datapoints();
            assertEquals(
                    List.of(List.of(12750.0, 50.0)),
                    stats.stream().map(d -> List.of(d.sum(), d.sampleCount())).toList());
        }
    }
}
