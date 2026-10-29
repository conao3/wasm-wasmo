package io.github.conao3.wasmo;

import java.util.Map;

/** The invocation a {@link FunctionHandler} runs, as the Lambda context object describes it. */
public final class FunctionContext {
    private final Map<String, Object> meta;
    private final Map<String, String> environment;
    private final StringBuilder log = new StringBuilder();

    @SuppressWarnings("unchecked")
    FunctionContext(Map<String, Object> meta) {
        this.meta = meta;
        this.environment = Map.copyOf((Map<String, String>) meta.get("environment"));
    }

    private String text(String key) {
        return (String) meta.get(key);
    }

    public String getFunctionName() {
        return text("functionName");
    }

    public String getFunctionVersion() {
        return text("functionVersion");
    }

    public String getInvokedFunctionArn() {
        return text("invokedFunctionArn");
    }

    public String getAwsRequestId() {
        return text("awsRequestId");
    }

    public String getLogGroupName() {
        return text("logGroupName");
    }

    public String getLogStreamName() {
        return text("logStreamName");
    }

    public String getInvocationType() {
        return text("invocationType");
    }

    public String getClientContext() {
        return text("clientContext");
    }

    public int getMemoryLimitInMB() {
        return ((Number) meta.get("memoryLimitInMB")).intValue();
    }

    public int getRemainingTimeInMillis() {
        var deadline = ((Number) meta.get("deadlineMs")).longValue();
        return (int) Math.max(0, deadline - System.currentTimeMillis());
    }

    /** The function's environment variables and the ones Lambda sets ({@code AWS_REGION}, ...). */
    public Map<String, String> getEnvironment() {
        return environment;
    }

    /** Appends a line to the invocation log that Invoke returns with LogType Tail. */
    public synchronized void log(String message) {
        log.append(message);
        if (!message.endsWith("\n")) {
            log.append('\n');
        }
    }

    synchronized String logText() {
        return log.toString();
    }
}
