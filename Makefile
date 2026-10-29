WASM := target/wasm32-unknown-unknown/release/wasmo_wasm.wasm

.PHONY: all wasm test test-rust test-go fmt lint

all: test

wasm:
	cargo build --release --target wasm32-unknown-unknown -p wasmo-wasm
	cp $(WASM) go/wasmo.wasm

test: test-rust test-go

test-rust:
	cargo test --workspace

test-go: wasm
	cd go && CGO_ENABLED=0 go test ./...

fmt:
	cargo fmt --all
	cd go && gofmt -w .

lint: wasm
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cd go && test -z "$$(gofmt -l .)" && go vet ./...
