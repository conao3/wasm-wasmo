WASM := target/wasm32-unknown-unknown/release/wasmo_wasm.wasm
JS_DEPS := js/node_modules/.package-lock.json

.PHONY: all wasm test test-rust test-go test-js test-jvm test-dotnet test-python dist-python fmt lint

all: test

wasm:
	cargo build --release --target wasm32-unknown-unknown -p wasmo-wasm
	cp $(WASM) go/wasmo.wasm
	cp $(WASM) js/wasmo.wasm
	cp $(WASM) jvm/wasmo.wasm
	cp $(WASM) dotnet/wasmo.wasm
	cp $(WASM) python/src/wasmo/wasmo.wasm

$(JS_DEPS): js/package.json js/package-lock.json
	cd js && npm ci

test: test-rust test-go test-js test-jvm test-dotnet test-python

test-rust:
	cargo test --workspace

test-go: wasm
	cd go && CGO_ENABLED=0 go test ./...

test-js: wasm $(JS_DEPS)
	cd js && npm run build && npm test && npm run test:bun

test-jvm: wasm
	cd jvm && mvn -B -q verify

test-dotnet: wasm
	cd dotnet && dotnet test -c Release --logger "console;verbosity=detailed"

test-python: wasm
	cd python && python -m pytest

dist-python: wasm
	cd python && rm -rf dist && python -m build --no-isolation

fmt: $(JS_DEPS)
	cargo fmt --all
	cd go && gofmt -w .
	cd js && npm run format
	cd jvm && mvn -B -q spotless:apply
	cd dotnet && dotnet format
	cd python && ruff check --fix . && ruff format .

lint: wasm $(JS_DEPS)
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings
	cd go && test -z "$$(gofmt -l .)" && go vet ./...
	cd js && npm run build && npm run typecheck && npm run format:check
	cd jvm && mvn -B -q spotless:check test-compile
	cd dotnet && dotnet format --verify-no-changes && dotnet build -c Release --no-incremental
	cd python && ruff check . && ruff format --check .
