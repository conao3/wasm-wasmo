package io.github.conao3.wasmo;

/**
 * Runs a Lambda function registered with {@link Wasmo#registerFunction}. The event and the result
 * are JSON documents; an exception becomes a function error ({@code X-Amz-Function-Error:
 * Unhandled}).
 */
@FunctionalInterface
public interface FunctionHandler {
    String handle(String event, FunctionContext context) throws Exception;
}
