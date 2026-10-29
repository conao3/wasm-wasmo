# wasmo

A stateful AWS mock that runs **inside your test process**, in every language, from a single WebAssembly module.

No Docker, no ports, no sidecar process. Write a value to DynamoDB and the next `GetItem` returns it; put an object in S3 and `ListObjects` shows it. Every `Mock` owns an isolated WebAssembly instance, so parallel tests never share state.

```go
m, _ := wasmo.New(ctx)
defer m.Close(ctx)

client := dynamodb.NewFromConfig(m.Config())
client.PutItem(ctx, &dynamodb.PutItemInput{TableName: aws.String("users"), Item: item})
out, _ := client.GetItem(ctx, &dynamodb.GetItemInput{TableName: aws.String("users"), Key: key})
// out.Item is the item written above
```

> **Status:** early development. The Go adapter runs STS, SQS, DynamoDB, and S3 through the AWS SDK for Go v2. Adapters for other languages are not published yet.

| Service | Coverage |
| --- | --- |
| STS | Caller identity, assume-role family, session and federation tokens |
| SQS | All 23 operations: standard and FIFO queues, batches, visibility, dead-letter redrive, move tasks |
| DynamoDB | Tables and indexes, items with condition/update/projection expressions, Query/Scan, batches, transactions, PartiQL, TTL, backups |
| S3 | Buckets, objects, listing, multipart, versioning, checksums, tagging, ACLs, Object Lock, bucket configuration, presigned GET, virtual-hosted and path-style addressing |

The protocol codecs (awsQuery, ec2, awsJson 1.0/1.1, rest-json, rest-xml) are checked against botocore's protocol test suite; every case either passes or is listed in `crates/wasmo-core/tests/conformance-gaps.txt` as client-only behaviour.

## How it works

Every AWS SDK lets you replace its HTTP client. wasmo uses the AWS wire protocol itself as the language boundary:

```
your language's AWS SDK
  └─ HTTP client adapter (a few hundred lines per language)
       └─ WebAssembly runtime (Go: wazero, JVM: Chicory, JS: built-in, Python/.NET/Ruby: wasmtime)
            └─ wasmo.wasm (Rust, wasm32-unknown-unknown)
```

The module needs no WASI. It imports two host functions (`wasmo.now_ms`, `wasmo.random_fill`) and exports three (`wasmo_alloc`, `wasmo_free`, `wasmo_handle`). A request is a JSON meta (method, URL, headers) plus raw body bytes; the response frame is a big-endian `u32` meta length, the meta JSON, then the body.

## Layout

| Path | Contents |
| --- | --- |
| `crates/wasmo-core` | Router, protocol codecs, services, state. Plain Rust, tested natively with `cargo test` |
| `crates/wasmo-wasm` | The WebAssembly ABI around `wasmo-core` |
| `go/` | Go adapter (`github.com/conao3/wasm-wasmo/go`) built on wazero |
| `vendor/` | Vendored upstream test data (see `vendor/PROVENANCE.md`) |

## Development

Tools come from the Nix dev shell.

```sh
nix develop
make test   # cargo test + build wasm + go test
make lint   # rustfmt, clippy, gofmt, go vet
```

## License

Apache-2.0. See `LICENSE` and `NOTICE`.
