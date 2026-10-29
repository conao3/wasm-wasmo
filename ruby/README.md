# wasmo

A stateful AWS mock that runs inside your test process. Write a value to DynamoDB and the next `GetItem` returns it; put an object in S3 and `ListObjectsV2` shows it. No Docker, no ports, no sidecar process: the AWS emulator is a single WebAssembly module, run by [wasmtime-rb](https://github.com/bytecodealliance/wasmtime-rb), that plugs into the AWS SDK for Ruby v3 as the send handler.

Emulated services: STS, SQS, DynamoDB, S3, SNS, EventBridge, Secrets Manager, SSM Parameter Store, KMS. The same module also backs wasmo's Go, JavaScript, and JVM adapters, so every language sees the same behaviour. Source and service coverage: <https://github.com/conao3/wasm-wasmo>.

```ruby
require "wasmo"
require "aws-sdk-dynamodb"
require "aws-sdk-sqs"

aws = Wasmo::Mock.new
ddb = aws.client(Aws::DynamoDB::Client) # or Aws::DynamoDB::Client.new(**aws.config)
ddb.put_item(table_name: "users", item: { "id" => "1" })
aws.reset # drop all state
aws.close # later requests raise Wasmo::ClosedError

Wasmo.mock_aws do |mock|
  Aws::SQS::Client.new.create_queue(queue_name: "jobs") # every client built in the block uses the mock
end
```

Requests go through the SDK's own signing, retries, checksums, and response parsing; only Net::HTTP is replaced. Each `Wasmo::Mock` owns an isolated WebAssembly instance and serializes calls into it, so one mock can be shared across threads and separate mocks never share state. Clients get `region` `us-east-1`, `endpoint` `http://wasmo.local`, and dummy credentials; S3 works with both virtual-hosted and path-style (`force_path_style: true`) addressing. `aws.request(method, url, headers, body)` sends a raw HTTP request, e.g. to fetch a presigned URL.

Lambda functions created with `create_function` run blocks registered by name, on every invocation path (Invoke, SNS, EventBridge, S3 notifications, event source mappings, async destinations): `aws.register_function("resize") { |event, context| { "ok" => true } }`. The block gets the decoded event and a `Wasmo::FunctionContext` (`function_name`, `aws_request_id`, `environment`, `log`, ...), returns a JSON-serializable value, and may call the same mock; an exception becomes a function error.

The compiled module is cached in `Gem.cache_home/wasmo` (`~/.cache/wasmo`), so only the first run after an upgrade pays for compilation.

Ruby 3.2+. wasmtime-rb ships precompiled native gems for Ruby 3.3+ on Linux and macOS; elsewhere Bundler builds it from source with a Rust toolchain.

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
