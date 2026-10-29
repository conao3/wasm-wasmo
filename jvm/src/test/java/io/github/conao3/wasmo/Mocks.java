package io.github.conao3.wasmo;

import software.amazon.awssdk.awscore.client.builder.AwsClientBuilder;
import software.amazon.awssdk.awscore.client.builder.AwsSyncClientBuilder;

final class Mocks {
    private Mocks() {}

    static <B extends AwsClientBuilder<B, C> & AwsSyncClientBuilder<B, C>, C> C client(
            Wasmo mock, B builder) {
        return builder.httpClient(mock)
                .endpointOverride(mock.endpoint())
                .region(mock.region())
                .credentialsProvider(mock.credentialsProvider())
                .build();
    }
}
