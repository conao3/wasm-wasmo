package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertFalse;
import static org.junit.jupiter.api.Assertions.assertThrows;
import static org.junit.jupiter.api.Assertions.assertTrue;

import java.util.Arrays;
import java.util.Map;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.core.SdkBytes;
import software.amazon.awssdk.services.kms.KmsClient;
import software.amazon.awssdk.services.kms.model.DataKeySpec;
import software.amazon.awssdk.services.kms.model.InvalidCiphertextException;

class KmsTest {
    @Test
    void encryptsAndDecryptsWithAContext() {
        try (var mock = Wasmo.create()) {
            var kms = Mocks.client(mock, KmsClient.builder());
            var key = kms.createKey(r -> r.description("app")).keyMetadata().keyId();
            kms.createAlias(r -> r.aliasName("alias/app").targetKeyId(key));
            var secret = new byte[300];
            for (int i = 0; i < secret.length; i++) {
                secret[i] = (byte) (i * 31 + 7);
            }
            var blob =
                    kms.encrypt(
                                    r ->
                                            r.keyId("alias/app")
                                                    .plaintext(SdkBytes.fromByteArray(secret))
                                                    .encryptionContext(Map.of("tenant", "a")))
                            .ciphertextBlob();
            assertFalse(Arrays.equals(secret, blob.asByteArray()));
            var out =
                    kms.decrypt(
                            r -> r.ciphertextBlob(blob).encryptionContext(Map.of("tenant", "a")));
            assertArrayEquals(secret, out.plaintext().asByteArray());
            assertTrue(out.keyId().endsWith(key));
            assertThrows(
                    InvalidCiphertextException.class,
                    () ->
                            kms.decrypt(
                                    r ->
                                            r.ciphertextBlob(blob)
                                                    .encryptionContext(Map.of("tenant", "b"))));

            var dataKey =
                    kms.generateDataKey(r -> r.keyId("alias/app").keySpec(DataKeySpec.AES_256));
            assertEquals(32, dataKey.plaintext().asByteArray().length);
            assertArrayEquals(
                    dataKey.plaintext().asByteArray(),
                    kms.decrypt(r -> r.ciphertextBlob(dataKey.ciphertextBlob()))
                            .plaintext()
                            .asByteArray());
        }
    }
}
