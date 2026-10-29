package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertNotEquals;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.services.ssm.SsmClient;
import software.amazon.awssdk.services.ssm.model.Parameter;
import software.amazon.awssdk.services.ssm.model.ParameterType;

class SsmTest {
    @Test
    void getParametersByPath() {
        try (var mock = Wasmo.create()) {
            var ssm = Mocks.client(mock, SsmClient.builder());
            for (var name : List.of("/app/db/host", "/app/db/port", "/app/api/url", "/other/x")) {
                ssm.putParameter(
                        r ->
                                r.name(name)
                                        .value(name.substring(name.lastIndexOf('/') + 1))
                                        .type(ParameterType.STRING));
            }
            ssm.putParameter(
                    r ->
                            r.name("/app/db/password")
                                    .value("hunter2")
                                    .type(ParameterType.SECURE_STRING));

            assertTrue(ssm.getParametersByPath(r -> r.path("/app")).parameters().isEmpty());

            var found =
                    ssm
                            .getParametersByPathPaginator(
                                    r ->
                                            r.path("/app")
                                                    .recursive(true)
                                                    .withDecryption(true)
                                                    .maxResults(2))
                            .stream()
                            .flatMap(p -> p.parameters().stream())
                            .collect(Collectors.toMap(Parameter::name, Parameter::value));
            assertEquals(
                    Map.of(
                            "/app/db/host", "host",
                            "/app/db/port", "port",
                            "/app/db/password", "hunter2",
                            "/app/api/url", "url"),
                    found);

            var db = ssm.getParametersByPath(r -> r.path("/app/db/")).parameters();
            assertEquals(
                    List.of("/app/db/host", "/app/db/password", "/app/db/port"),
                    db.stream().map(Parameter::name).sorted().toList());
            var secret =
                    db.stream().filter(p -> p.name().equals("/app/db/password")).findFirst().get();
            assertEquals(ParameterType.SECURE_STRING, secret.type());
            assertNotEquals("hunter2", secret.value());
        }
    }
}
