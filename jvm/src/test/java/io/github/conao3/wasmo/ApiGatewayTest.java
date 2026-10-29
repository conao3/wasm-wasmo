package io.github.conao3.wasmo;

import static org.junit.jupiter.api.Assertions.assertEquals;

import java.io.IOException;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.util.Map;
import org.junit.jupiter.api.Test;
import software.amazon.awssdk.core.SdkBytes;
import software.amazon.awssdk.http.HttpExecuteRequest;
import software.amazon.awssdk.http.HttpExecuteResponse;
import software.amazon.awssdk.http.SdkHttpFullRequest;
import software.amazon.awssdk.http.SdkHttpMethod;
import software.amazon.awssdk.services.apigateway.ApiGatewayClient;
import software.amazon.awssdk.services.apigateway.model.IntegrationType;
import software.amazon.awssdk.services.lambda.LambdaClient;
import software.amazon.awssdk.services.lambda.model.Runtime;

class ApiGatewayTest {
    private static HttpExecuteResponse get(Wasmo mock, String url) throws IOException {
        var request =
                SdkHttpFullRequest.builder().uri(URI.create(url)).method(SdkHttpMethod.GET).build();
        return mock.prepareRequest(HttpExecuteRequest.builder().request(request).build()).call();
    }

    private static String body(HttpExecuteResponse res) throws IOException {
        return new String(res.responseBody().orElseThrow().readAllBytes(), StandardCharsets.UTF_8);
    }

    @Test
    void restApiInvokesHandlerThroughExecuteApi() throws IOException {
        try (var mock = Wasmo.create()) {
            var lambda = Mocks.client(mock, LambdaClient.builder());
            var apigw = Mocks.client(mock, ApiGatewayClient.builder());
            var fn =
                    lambda.createFunction(
                                    r ->
                                            r.functionName("pets")
                                                    .runtime(Runtime.JAVA21)
                                                    .role("arn:aws:iam::000000000000:role/lambda")
                                                    .handler("example.Handler")
                                                    .code(
                                                            c ->
                                                                    c.zipFile(
                                                                            SdkBytes.fromUtf8String(
                                                                                    "PK fake zip"))))
                            .functionArn();
            mock.registerFunction(
                    "pets",
                    (event, context) -> {
                        var e = Json.parseObject(event);
                        var id = ((Map<?, ?>) e.get("pathParameters")).get("id");
                        var env = ((Map<?, ?>) e.get("stageVariables")).get("env");
                        return "{\"statusCode\":200,\"headers\":{\"x-pet\":\""
                                + id
                                + "\"},\"body\":\"{\\\"id\\\":\\\""
                                + id
                                + "\\\",\\\"env\\\":\\\""
                                + env
                                + "\\\"}\"}";
                    });
            var api = apigw.createRestApi(r -> r.name("shop"));
            var pets =
                    apigw.createResource(
                                    r ->
                                            r.restApiId(api.id())
                                                    .parentId(api.rootResourceId())
                                                    .pathPart("pets"))
                            .id();
            var pet =
                    apigw.createResource(r -> r.restApiId(api.id()).parentId(pets).pathPart("{id}"))
                            .id();
            apigw.putMethod(
                    r ->
                            r.restApiId(api.id())
                                    .resourceId(pet)
                                    .httpMethod("GET")
                                    .authorizationType("NONE"));
            apigw.putIntegration(
                    r ->
                            r.restApiId(api.id())
                                    .resourceId(pet)
                                    .httpMethod("GET")
                                    .type(IntegrationType.AWS_PROXY)
                                    .integrationHttpMethod("POST")
                                    .uri(
                                            "arn:aws:apigateway:us-east-1:lambda:path/2015-03-31/functions/"
                                                    + fn
                                                    + "/invocations"));
            apigw.createDeployment(
                    r -> r.restApiId(api.id()).stageName("prod").variables(Map.of("env", "test")));

            var base = "https://" + api.id() + ".execute-api.us-east-1.amazonaws.com/prod";
            var res = get(mock, base + "/pets/7");
            assertEquals(200, res.httpResponse().statusCode());
            assertEquals("7", res.httpResponse().firstMatchingHeader("x-pet").orElseThrow());
            assertEquals("{\"id\":\"7\",\"env\":\"test\"}", body(res));

            var missing = get(mock, base + "/nothing");
            assertEquals(403, missing.httpResponse().statusCode());
            assertEquals("{\"message\":\"Missing Authentication Token\"}", body(missing));
        }
    }
}
