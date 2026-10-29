# wasmo

A stateful AWS mock that runs **inside your test process**, in every language, from a single WebAssembly module.

No Docker, no ports, no sidecar process. Write a value to DynamoDB and the next `GetItem` returns it; put an object in S3 and `ListObjects` shows it. Every `Mock` owns an isolated WebAssembly instance, so parallel tests never share state.

```go
m, _ := wasmo.New(ctx)
defer m.Close(ctx)

client := sts.NewFromConfig(m.Config())
out, _ := client.GetCallerIdentity(ctx, &sts.GetCallerIdentityInput{})
// *out.Account == "000000000000"
```

> **Status:** early development. The pipeline (Rust core → WebAssembly → Go adapter → AWS SDK for Go v2) works end to end with STS `GetCallerIdentity`. Protocol codecs and services are being ported next.

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
