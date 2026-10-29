package io.github.conao3.wasmo;

import com.dylibso.chicory.runtime.ExportFunction;
import com.dylibso.chicory.runtime.HostFunction;
import com.dylibso.chicory.runtime.ImportValues;
import com.dylibso.chicory.runtime.Instance;
import com.dylibso.chicory.runtime.Memory;
import com.dylibso.chicory.wasm.types.FunctionType;
import com.dylibso.chicory.wasm.types.ValType;
import io.github.conao3.wasmo.internal.WasmoModule;
import java.io.ByteArrayInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.URI;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.security.SecureRandom;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Deque;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.ConcurrentHashMap;
import software.amazon.awssdk.auth.credentials.AwsBasicCredentials;
import software.amazon.awssdk.auth.credentials.AwsCredentialsProvider;
import software.amazon.awssdk.auth.credentials.StaticCredentialsProvider;
import software.amazon.awssdk.http.AbortableInputStream;
import software.amazon.awssdk.http.ExecutableHttpRequest;
import software.amazon.awssdk.http.HttpExecuteRequest;
import software.amazon.awssdk.http.HttpExecuteResponse;
import software.amazon.awssdk.http.SdkHttpClient;
import software.amazon.awssdk.http.SdkHttpRequest;
import software.amazon.awssdk.http.SdkHttpResponse;
import software.amazon.awssdk.regions.Region;

/**
 * An isolated in-process AWS. Each instance owns its own WebAssembly instance, so state never leaks
 * between instances. Pass it to an SDK client builder with {@code httpClient}, together with {@link
 * #endpoint()}, {@link #region()} and {@link #credentialsProvider()}.
 */
public final class Wasmo implements SdkHttpClient {
    private static final URI ENDPOINT = URI.create("http://wasmo.local");
    private static final SecureRandom RANDOM = new SecureRandom();
    private static final AwsCredentialsProvider CREDENTIALS =
            StaticCredentialsProvider.create(AwsBasicCredentials.create("test", "test"));

    private final Instance instance;
    private final Memory memory;
    private final ExportFunction alloc;
    private final ExportFunction free;
    private final ExportFunction handle;
    private final ExportFunction complete;
    private final Map<String, FunctionHandler> functions = new ConcurrentHashMap<>();
    private Deque<byte[][]> current;
    private boolean closed;

    private Wasmo() {
        instance =
                Instance.builder(WasmoModule.load())
                        .withImportValues(imports())
                        .withMachineFactory(WasmoModule::create)
                        .build();
        memory = instance.memory();
        alloc = instance.export("wasmo_alloc");
        free = instance.export("wasmo_free");
        handle = instance.export("wasmo_handle");
        complete = instance.export("wasmo_complete");
    }

    /** Starts a mock with empty state. */
    public static Wasmo create() {
        return new Wasmo();
    }

    private ImportValues imports() {
        var nowMs =
                new HostFunction(
                        "wasmo",
                        "now_ms",
                        FunctionType.of(List.of(), List.of(ValType.F64)),
                        (inst, args) ->
                                new long[] {
                                    Double.doubleToRawLongBits(System.currentTimeMillis())
                                });
        var randomFill =
                new HostFunction(
                        "wasmo",
                        "random_fill",
                        FunctionType.of(List.of(ValType.I32, ValType.I32), List.of()),
                        (inst, args) -> {
                            var buf = new byte[(int) args[1]];
                            RANDOM.nextBytes(buf);
                            inst.memory().write((int) args[0], buf);
                            return null;
                        });
        var invoke =
                new HostFunction(
                        "wasmo",
                        "invoke",
                        FunctionType.of(
                                List.of(ValType.I32, ValType.I32, ValType.I32, ValType.I32),
                                List.of()),
                        (inst, args) -> {
                            if (current == null) {
                                throw new IllegalStateException("wasmo: invoke outside a request");
                            }
                            current.add(
                                    new byte[][] {
                                        inst.memory().readBytes((int) args[0], (int) args[1]),
                                        inst.memory().readBytes((int) args[2], (int) args[3])
                                    });
                            return null;
                        });
        return ImportValues.builder().addFunction(nowMs, randomFill, invoke).build();
    }

    /**
     * Binds a handler to every invocation of the Lambda function with this name, whatever code
     * CreateFunction was given. The handler runs outside the mock's lock, so it may call AWS
     * through the same mock.
     */
    public void registerFunction(String name, FunctionHandler handler) {
        functions.put(name, handler);
    }

    public URI endpoint() {
        return ENDPOINT;
    }

    public Region region() {
        return Region.US_EAST_1;
    }

    public AwsCredentialsProvider credentialsProvider() {
        return CREDENTIALS;
    }

    @Override
    public String clientName() {
        return "wasmo";
    }

    @Override
    public synchronized void close() {
        closed = true;
    }

    @Override
    public ExecutableHttpRequest prepareRequest(HttpExecuteRequest request) {
        return new ExecutableHttpRequest() {
            @Override
            public HttpExecuteResponse call() throws IOException {
                return execute(request);
            }

            @Override
            public void abort() {}
        };
    }

    private HttpExecuteResponse execute(HttpExecuteRequest request) throws IOException {
        var http = request.httpRequest();
        byte[] body;
        if (request.contentStreamProvider().isPresent()) {
            try (InputStream in = request.contentStreamProvider().get().newStream()) {
                body = in.readAllBytes();
            }
        } else {
            body = new byte[0];
        }
        var meta = encodeMeta(http).getBytes(StandardCharsets.UTF_8);
        var pending = new ArrayDeque<byte[][]>();
        var frame = roundtrip(handle, pending, meta, body);
        while (!pending.isEmpty()) {
            var result = run(pending.poll());
            var answer = roundtrip(complete, pending, result[0], result[1]);
            if (answer != null) {
                frame = answer;
            }
        }

        var buf = ByteBuffer.wrap(frame);
        int metaLen = buf.getInt();
        var res = Json.parseObject(new String(frame, 4, metaLen, StandardCharsets.UTF_8));
        if (res.containsKey("await")) {
            throw new IllegalStateException(
                    "wasmo: invocation " + res.get("await") + " was never answered");
        }
        var response =
                SdkHttpResponse.builder().statusCode(((Number) res.get("status")).intValue());
        for (var kv : (List<?>) res.get("headers")) {
            var pair = (List<?>) kv;
            response.appendHeader((String) pair.get(0), (String) pair.get(1));
        }
        int offset = 4 + metaLen;
        return HttpExecuteResponse.builder()
                .response(response.build())
                .responseBody(
                        AbortableInputStream.create(
                                new ByteArrayInputStream(frame, offset, frame.length - offset)))
                .build();
    }

    private static String encodeMeta(SdkHttpRequest http) {
        var headers = new ArrayList<String[]>();
        boolean hasHost = false;
        for (Map.Entry<String, List<String>> e : http.headers().entrySet()) {
            var name = e.getKey().toLowerCase(Locale.ROOT);
            hasHost |= name.equals("host");
            headers.add(new String[] {name, String.join(",", e.getValue())});
        }
        if (!hasHost) {
            headers.add(0, new String[] {"host", http.getUri().getRawAuthority()});
        }
        var sb = new StringBuilder("{\"method\":");
        Json.quote(sb, http.method().name());
        sb.append(",\"url\":");
        Json.quote(sb, http.getUri().toString());
        sb.append(",\"headers\":[");
        for (int i = 0; i < headers.size(); i++) {
            if (i > 0) {
                sb.append(',');
            }
            sb.append('[');
            Json.quote(sb, headers.get(i)[0]);
            sb.append(',');
            Json.quote(sb, headers.get(i)[1]);
            sb.append(']');
        }
        return sb.append("]}").toString();
    }

    private byte[][] run(byte[][] invocation) {
        var meta = Json.parseObject(new String(invocation[0], StandardCharsets.UTF_8));
        var out = new StringBuilder("{\"id\":");
        Json.quote(out, (String) meta.get("id"));
        var handler = functions.get((String) meta.get("functionName"));
        if (handler == null) {
            return new byte[][] {
                out.append(",\"registered\":false}").toString().getBytes(StandardCharsets.UTF_8),
                new byte[0]
            };
        }
        var context = new FunctionContext(meta);
        var payload = new byte[0];
        try {
            var result = handler.handle(new String(invocation[1], StandardCharsets.UTF_8), context);
            payload = (result == null ? "null" : result).getBytes(StandardCharsets.UTF_8);
        } catch (Exception e) {
            out.append(",\"error\":{\"type\":");
            Json.quote(out, e.getClass().getName());
            out.append(",\"message\":");
            Json.quote(out, String.valueOf(e.getMessage()));
            out.append(",\"stackTrace\":[");
            var trace = e.getStackTrace();
            for (int i = 0; i < trace.length; i++) {
                if (i > 0) {
                    out.append(',');
                }
                Json.quote(out, trace[i].toString());
            }
            out.append("]}");
        }
        out.append(",\"log\":");
        Json.quote(out, context.logText());
        return new byte[][] {out.append('}').toString().getBytes(StandardCharsets.UTF_8), payload};
    }

    private synchronized byte[] roundtrip(
            ExportFunction fn, Deque<byte[][]> pending, byte[] meta, byte[] body) {
        if (closed) {
            throw new IllegalStateException("wasmo: instance is closed");
        }
        int metaPtr = put(meta);
        int bodyPtr = put(body);
        current = pending;
        long res;
        try {
            res = fn.apply(metaPtr, meta.length, bodyPtr, body.length)[0];
        } finally {
            current = null;
        }
        if (res == 0) {
            return null;
        }
        int ptr = (int) (res >>> 32);
        int len = (int) res;
        var frame = memory.readBytes(ptr, len);
        free.apply(ptr, len);
        return frame;
    }

    private int put(byte[] data) {
        int ptr = (int) alloc.apply(data.length)[0];
        if (data.length > 0) {
            memory.write(ptr, data);
        }
        return ptr;
    }
}
