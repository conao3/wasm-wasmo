# Wasmo

A stateful AWS mock that runs inside your test process. The mock is a single WebAssembly module, embedded in this package and run with [wasmtime](https://github.com/bytecodealliance/wasmtime-dotnet). Every `WasmoMock` owns an isolated instance, so parallel tests never share state.

```csharp
using var mock = WasmoMock.Create();
using var s3 = new AmazonS3Client(mock.Configure(new AmazonS3Config()));
using var ddb = new AmazonDynamoDBClient(mock.Configure(new AmazonDynamoDBConfig()));
```

`Configure` sets `ServiceURL`, `AuthenticationRegion` (`us-east-1`), dummy `DefaultAWSCredentials`, and an `HttpClientFactory` that answers every request from the mock. It works with any `ClientConfig` of the AWS SDK for .NET v4.

Emulated services: STS, SQS, DynamoDB, S3, SNS, EventBridge, Secrets Manager, SSM Parameter Store, and KMS. See <https://github.com/conao3/wasm-wasmo>.
