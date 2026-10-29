package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.util.List;
import java.util.Map;
import java.util.stream.Collectors;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.core.SdkBytes;
import software.amazon.awssdk.services.secretsmanager.SecretsManagerClient;
import software.amazon.awssdk.services.secretsmanager.model.ResourceNotFoundException;
import software.amazon.awssdk.services.secretsmanager.model.SecretVersionsListEntry;

class SecretsManagerTest {
    @Test
    void versionsAndStages() {
        try (var mock = Wasmo.create()) {
            var sm = Mocks.client(mock, SecretsManagerClient.builder());
            var first = sm.createSecret(r -> r.name("db").secretString("v1")).versionId();
            var second = sm.putSecretValue(r -> r.secretId("db").secretString("v2")).versionId();

            assertEquals("v2", sm.getSecretValue(r -> r.secretId("db")).secretString());
            var previous = sm.getSecretValue(r -> r.secretId("db").versionStage("AWSPREVIOUS"));
            assertEquals("v1", previous.secretString());
            assertEquals(first, previous.versionId());

            var stages =
                    sm.listSecretVersionIds(r -> r.secretId("db")).versions().stream()
                            .collect(
                                    Collectors.toMap(
                                            SecretVersionsListEntry::versionId,
                                            SecretVersionsListEntry::versionStages));
            assertEquals(
                    Map.of(first, List.of("AWSPREVIOUS"), second, List.of("AWSCURRENT")), stages);

            byte[] binary = {0, 1, (byte) 0xff};
            sm.putSecretValue(r -> r.secretId("db").secretBinary(SdkBytes.fromByteArray(binary)));
            assertArrayEquals(
                    binary, sm.getSecretValue(r -> r.secretId("db")).secretBinary().asByteArray());
            assertThrows(
                    ResourceNotFoundException.class,
                    () -> sm.getSecretValue(r -> r.secretId("missing")));
        }
    }
}
