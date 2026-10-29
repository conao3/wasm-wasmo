# Changelog

All packages share one version, set in `VERSION`. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow [Semantic Versioning](https://semver.org/).

## [0.1.0]

First release. One WebAssembly module (`wasmo.wasm`, built from Rust for `wasm32-unknown-unknown`) is embedded in an adapter for each language's AWS SDK and answers its requests inside the test process.

### Packages

- Go: `github.com/conao3/wasm-wasmo/go` for the AWS SDK for Go v2, on wazero (pure Go, `CGO_ENABLED=0`).
- JavaScript / TypeScript: `@conao3/wasmo` on npm for the AWS SDK for JavaScript v3, ESM with type declarations, Node.js 22+ and Bun.
- JVM: `io.github.conao3:wasmo` on Maven Central for the AWS SDK for Java v2, Java 17+. The module is compiled to JVM bytecode at build time with Chicory.
- .NET: `Wasmo` on NuGet for the AWS SDK for .NET v4, `net8.0`, on wasmtime-dotnet. Ships a symbol package and Source Link.
- Python: `wasmo` on PyPI for boto3 / botocore, Python 3.10+, on wasmtime.
- Ruby: `wasmo` on RubyGems for the AWS SDK for Ruby v3, Ruby 3.3+, on wasmtime-rb, with an on-disk cache of the compiled module.

### Services

- STS, SQS, DynamoDB, S3, SNS, EventBridge, Secrets Manager, SSM Parameter Store, KMS, IAM, CloudWatch, CloudWatch Logs, Kinesis, Data Firehose, and DynamoDB Streams.
- Cross-service delivery inside one mock: SNS to SQS, EventBridge to SQS and SNS, S3 notifications to SQS, SNS, and EventBridge, CloudWatch Logs subscription filters to Kinesis, Firehose, and Logs destinations, Logs metric filters to CloudWatch, Firehose and Logs export tasks to S3, and CloudWatch alarm actions to SNS and EventBridge.
- Protocols: awsQuery, ec2, awsJson 1.0 / 1.1, rest-json, rest-xml, and Smithy RPC v2 CBOR, checked against the botocore protocol test suite.
- Behaviour checked against the bun-bunsai parity fixtures recorded from real AWS.

[0.1.0]: https://github.com/conao3/wasm-wasmo/releases/tag/v0.1.0
