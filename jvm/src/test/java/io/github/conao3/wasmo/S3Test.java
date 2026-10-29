package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertArrayEquals;
import static org.junit.jupiter.api.Assertions.assertEquals;
import static org.junit.jupiter.api.Assertions.assertThrows;

import java.util.List;
import java.util.Set;
import java.util.concurrent.ConcurrentSkipListSet;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.params.ParameterizedTest;
import org.junit.jupiter.params.provider.ValueSource;
import software.amazon.awssdk.core.interceptor.Context;
import software.amazon.awssdk.core.interceptor.ExecutionAttributes;
import software.amazon.awssdk.core.interceptor.ExecutionInterceptor;
import software.amazon.awssdk.core.sync.RequestBody;
import software.amazon.awssdk.services.s3.S3Client;
import software.amazon.awssdk.services.s3.model.NoSuchKeyException;
import software.amazon.awssdk.services.s3.model.S3Object;

class S3Test {
    static S3Client s3(Wasmo mock, boolean pathStyle, Set<String> hosts) {
        ExecutionInterceptor recordHost =
                new ExecutionInterceptor() {
                    @Override
                    public void beforeTransmission(
                            Context.BeforeTransmission context, ExecutionAttributes attributes) {
                        hosts.add(context.httpRequest().host());
                    }
                };
        return Mocks.client(
                mock,
                S3Client.builder()
                        .forcePathStyle(pathStyle)
                        .overrideConfiguration(c -> c.addExecutionInterceptor(recordHost)));
    }

    static byte[] pattern(int n) {
        var b = new byte[n];
        for (int i = 0; i < n; i++) {
            b[i] = (byte) (i * 31 + i / 7);
        }
        return b;
    }

    @ParameterizedTest
    @ValueSource(booleans = {true, false})
    void putGetList(boolean pathStyle) {
        var hosts = new ConcurrentSkipListSet<String>();
        try (var mock = Wasmo.create()) {
            var s3 = s3(mock, pathStyle, hosts);
            s3.createBucket(r -> r.bucket("alpha"));
            var data = pattern(70_000);
            var put =
                    s3.putObject(
                            r -> r.bucket("alpha").key("dir/blob.bin"),
                            RequestBody.fromBytes(data));
            var got = s3.getObjectAsBytes(r -> r.bucket("alpha").key("dir/blob.bin"));
            assertArrayEquals(data, got.asByteArray());
            assertEquals(put.eTag(), got.response().eTag());
            assertEquals(70_000L, got.response().contentLength());

            s3.putObject(
                    r -> r.bucket("alpha").key("dir/note.txt").contentType("text/plain"),
                    RequestBody.fromString("hello"));
            assertEquals(
                    "text/plain",
                    s3.headObject(r -> r.bucket("alpha").key("dir/note.txt")).contentType());
            var keys =
                    s3.listObjectsV2(r -> r.bucket("alpha").prefix("dir/")).contents().stream()
                            .map(S3Object::key)
                            .toList();
            assertEquals(List.of("dir/blob.bin", "dir/note.txt"), keys);

            assertThrows(
                    NoSuchKeyException.class,
                    () -> s3.getObjectAsBytes(r -> r.bucket("alpha").key("missing")));
        }
        assertEquals(Set.of(pathStyle ? "wasmo.local" : "alpha.wasmo.local"), hosts);
    }

    @Test
    void addressingStylesShareState() {
        var hosts = new ConcurrentSkipListSet<String>();
        try (var mock = Wasmo.create()) {
            var pathStyle = s3(mock, true, hosts);
            pathStyle.createBucket(r -> r.bucket("shared"));
            pathStyle.putObject(r -> r.bucket("shared").key("k"), RequestBody.fromString("v"));
            assertEquals(
                    "v",
                    s3(mock, false, hosts)
                            .getObjectAsBytes(r -> r.bucket("shared").key("k"))
                            .asUtf8String());
        }
        assertEquals(Set.of("wasmo.local", "shared.wasmo.local"), hosts);
    }
}
